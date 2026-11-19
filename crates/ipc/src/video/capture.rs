// SPDX-License-Identifier: EUPL-1.2

use super::{
    Error, HEADER_BYTES, Header, Kind, Settings, Source, invalid,
    supervisor::{DEADLINE, SOURCES},
};
use crate::host::preview_lifecycle::Retirement;
use nuschl_sandbox::Role;
use std::{
    os::fd::OwnedFd,
    path::PathBuf,
    process::Stdio,
    time::{Duration, Instant},
};
use tokio::{
    io::AsyncReadExt as _, process::ChildStdout, sync::mpsc, task::JoinHandle, time::timeout,
};

pub struct VideoFrame {
    pub width: u16,
    pub height: u16,
    /// Microseconds since this capture began, on the worker's monotonic clock.
    pub capture_time_us: u64,
    pub av1: Vec<u8>,
    pub hardware: bool,
    received: Instant,
}

pub struct Capture {
    frames: mpsc::Receiver<Result<VideoFrame, Error>>,
    task: JoinHandle<()>,
}

impl Capture {
    /// The remote is the connected PipeWire FD granted by the Linux portal.
    pub fn start(
        executable: PathBuf,
        remote: OwnedFd,
        source: Source,
        node_id: Option<u32>,
    ) -> Result<Self, Error> {
        Self::start_inner(executable, remote, source, node_id, false)
    }

    /// Prefer the isolated VA-API path when its fixed render device exists.
    pub fn start_vaapi(
        executable: PathBuf,
        remote: OwnedFd,
        source: Source,
        node_id: Option<u32>,
    ) -> Result<Self, Error> {
        Self::start_inner(executable, remote, source, node_id, true)
    }

    fn start_inner(
        executable: PathBuf,
        remote: OwnedFd,
        source: Source,
        node_id: Option<u32>,
        vaapi: bool,
    ) -> Result<Self, Error> {
        if (source == Source::Screen) != node_id.is_some() || matches!(node_id, Some(0 | u32::MAX))
        {
            return Err(invalid().into());
        }
        let permit = SOURCES.try_acquire().map_err(|_| Error::Busy)?;
        let (sender, frames) = mpsc::channel(1);
        let runtime = tokio::runtime::Handle::try_current().map_err(|_| Error::Runtime)?;
        let task = runtime.spawn(async move {
            let _permit = permit;
            if let Err(error) = capture(executable, remote, source, node_id, vaapi, &sender).await {
                let _ = sender.try_send(Err(error));
            }
        });
        Ok(Self { frames, task })
    }

    pub async fn next(&mut self) -> Result<VideoFrame, Error> {
        loop {
            let frame = self.frames.recv().await.ok_or(Error::Stopped)??;
            if frame.received.elapsed() <= Duration::from_millis(250) {
                return Ok(frame);
            }
        }
    }

    pub async fn stop(self) -> Result<(), Error> {
        drop(self.frames);
        self.task.await.map_err(|_| Error::Stopped)
    }
}

async fn capture(
    executable: PathBuf,
    remote: OwnedFd,
    source: Source,
    node: Option<u32>,
    vaapi: bool,
    sender: &mpsc::Sender<Result<VideoFrame, Error>>,
) -> Result<(), Error> {
    let mut child = nuschl_sandbox::command(&executable, Role::VideoCapture { node, vaapi })?
        .stdin(Stdio::from(remote))
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()?;
    let containment = nuschl_sandbox::contain(&child)?;
    let mut retirement = Retirement::new(&mut child);
    let result = async {
        retirement.ready().await.map_err(|_| Error::Confinement)?;
        let mut output = child.stdout.take().ok_or_else(invalid)?;
        let mut previous = 0;
        loop {
            let frame = tokio::select! {
                _ = sender.closed() => return Ok(()),
                result = async {
                    if previous == 0 {
                        timeout(Duration::from_secs(15), read_frame(&mut output, source, 0))
                            .await.unwrap_or(Err(Error::Timeout))
                    } else {
                        read_frame(&mut output, source, previous).await
                    }
                } => result?,
            };
            previous = frame.capture_time_us;
            // Each frame is independent, so a slow consumer can skip frames.
            let _ = sender.try_send(Ok(frame));
        }
    }
    .await;
    if let Err(error) = result {
        let _ = sender.try_send(Err(error));
    }
    retirement.retire(&mut child, &containment).await;
    Ok(())
}

async fn read_frame(
    input: &mut ChildStdout,
    source: Source,
    previous: u64,
) -> Result<VideoFrame, Error> {
    let mut bytes = [0; HEADER_BYTES];
    input.read_exact(&mut bytes[..1]).await?;
    timeout(DEADLINE, async {
        input.read_exact(&mut bytes[1..]).await?;
        let header = Header::from_bytes(bytes)?;
        if !matches!(header.kind, Kind::CaptureFrame | Kind::HardwareFrame)
            || !header.key
            || header.id <= previous
        {
            return Err(invalid().into());
        }
        let mut settings = [0; Settings::BYTES];
        input.read_exact(&mut settings).await?;
        let settings = Settings::from_bytes(settings)?;
        let size = header.bytes as usize;
        if settings.source != source || size == 0 || size > settings.packet_limit(true) {
            return Err(invalid().into());
        }
        let mut av1 = vec![0; size];
        input.read_exact(&mut av1).await?;
        Ok(VideoFrame {
            width: settings.width,
            height: settings.height,
            capture_time_us: header.id,
            av1,
            hardware: header.kind == Kind::HardwareFrame,
            received: Instant::now(),
        })
    })
    .await
    .unwrap_or(Err(Error::Timeout))
}
