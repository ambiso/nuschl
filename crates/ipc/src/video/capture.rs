// SPDX-License-Identifier: EUPL-1.2

use super::{
    Error, HEADER_BYTES, Header, Kind, Settings, Source, invalid,
    supervisor::{DEADLINE, SOURCES},
};
use crate::host::preview_lifecycle::Retirement;
use nuschl_sandbox::Role;
use std::{
    os::{fd::OwnedFd, unix::net::UnixStream as Socket},
    path::PathBuf,
    process::Stdio,
    time::{Duration, Instant},
};
use tokio::{
    io::{AsyncRead, AsyncReadExt as _, AsyncWrite, AsyncWriteExt as _},
    net::UnixStream,
    sync::{mpsc, watch},
    task::JoinHandle,
    time::timeout,
};

pub struct VideoFrame {
    pub width: u16,
    pub height: u16,
    /// Microseconds since this capture began, on the worker's monotonic clock.
    pub capture_time_us: u64,
    pub av1: Vec<u8>,
    pub keyframe: bool,
    pub hardware: bool,
    pub bitrate_bps: u32,
    received: Instant,
}

pub struct Capture {
    frames: mpsc::Receiver<Result<VideoFrame, Error>>,
    task: JoinHandle<()>,
    control: watch::Sender<Control>,
    waiting_for_key: bool,
    #[cfg(feature = "video-portal")]
    grant: Option<ashpd::zbus::Connection>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
struct Control {
    bitrate: u32,
    key_request: u64,
}

const MAX_AGE: Duration = Duration::from_millis(250);

impl Capture {
    /// The remote is the connected PipeWire FD granted by the Linux portal.
    pub fn start(
        executable: PathBuf,
        remote: OwnedFd,
        source: Source,
        node_id: Option<u32>,
    ) -> Result<Self, Error> {
        Self::start_inner(executable, remote, source, node_id, false, 2_000_000)
    }

    /// Prefer the isolated VA-API path when its fixed render device exists.
    pub fn start_vaapi(
        executable: PathBuf,
        remote: OwnedFd,
        source: Source,
        node_id: Option<u32>,
    ) -> Result<Self, Error> {
        Self::start_inner(executable, remote, source, node_id, true, 20_000_000)
    }

    pub(super) fn start_inner(
        executable: PathBuf,
        remote: OwnedFd,
        source: Source,
        node_id: Option<u32>,
        vaapi: bool,
        bitrate: u32,
    ) -> Result<Self, Error> {
        if (source == Source::Screen) != node_id.is_some()
            || matches!(node_id, Some(0 | u32::MAX))
            || !Settings::valid_bitrate(bitrate)
        {
            return Err(invalid().into());
        }
        let permit = SOURCES.try_acquire().map_err(|_| Error::Busy)?;
        let (sender, frames) = mpsc::channel(1);
        let (control, updates) = watch::channel(Control {
            bitrate,
            key_request: 0,
        });
        let requests = control.clone();
        let runtime = tokio::runtime::Handle::try_current().map_err(|_| Error::Runtime)?;
        let task = runtime.spawn(async move {
            let _permit = permit;
            if let Err(error) = capture(
                executable,
                remote,
                source,
                node_id,
                vaapi,
                &sender,
                (updates, requests),
            )
            .await
            {
                let _ = sender.try_send(Err(error));
            }
        });
        Ok(Self {
            frames,
            task,
            control,
            waiting_for_key: true,
            #[cfg(feature = "video-portal")]
            grant: None,
        })
    }

    #[cfg(feature = "video-portal")]
    pub(super) fn retain_grant(&mut self, connection: ashpd::zbus::Connection) {
        self.grant = Some(connection);
    }

    /// Update the target bitrate. The latest request replaces pending requests.
    pub fn set_bitrate(&self, bitrate_bps: u32) -> Result<(), Error> {
        if !Settings::valid_bitrate(bitrate_bps) {
            return Err(invalid().into());
        }
        if self.control.is_closed() {
            return Err(Error::Stopped);
        }
        self.control.send_if_modified(|control| {
            std::mem::replace(&mut control.bitrate, bitrate_bps) != bitrate_bps
        });
        Ok(())
    }

    /// Request a new independently decodable frame after a receiver loss report.
    pub fn request_keyframe(&self) {
        request_keyframe(&self.control);
    }

    pub async fn next(&mut self) -> Result<VideoFrame, Error> {
        loop {
            let frame = self.frames.recv().await.ok_or(Error::Stopped)??;
            if frame.received.elapsed() > MAX_AGE {
                self.waiting_for_key = true;
                self.request_keyframe();
            } else if frame.keyframe || !self.waiting_for_key {
                self.waiting_for_key = false;
                return Ok(frame);
            }
        }
    }

    pub async fn stop(self) -> Result<(), Error> {
        drop(self.frames);
        #[cfg(feature = "video-portal")]
        drop(self.grant);
        self.task.await.map_err(|_| Error::Stopped)
    }
}

fn request_keyframe(control: &watch::Sender<Control>) {
    control.send_modify(|value| value.key_request = value.key_request.wrapping_add(1));
}

async fn capture(
    executable: PathBuf,
    remote: OwnedFd,
    source: Source,
    node: Option<u32>,
    vaapi: bool,
    sender: &mpsc::Sender<Result<VideoFrame, Error>>,
    (mut updates, requests): (watch::Receiver<Control>, watch::Sender<Control>),
) -> Result<(), Error> {
    let (supervisor, worker) = Socket::pair()?;
    supervisor.set_nonblocking(true)?;
    let (mut output, mut control) = UnixStream::from_std(supervisor)?.into_split();
    let mut child = nuschl_sandbox::command(&executable, Role::VideoCapture { node, vaapi })?
        .stdin(Stdio::from(remote))
        .stdout(Stdio::from(OwnedFd::from(worker)))
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()?;
    let containment = nuschl_sandbox::contain(&child)?;
    let mut retirement = Retirement::new(&mut child);
    let result = async {
        retirement.ready().await.map_err(|_| Error::Confinement)?;
        let initial = *updates.borrow_and_update();
        send_control(
            &mut control,
            Kind::CaptureStart,
            &initial.bitrate.to_be_bytes(),
        )
        .await?;
        tokio::select! {
            _ = sender.closed() => Ok(()),
            result = receive_frames(&mut output, source, sender, &requests) => result,
            result = send_updates(&mut control, &mut updates, initial) => result,
        }
    }
    .await;
    if let Err(error) = result {
        let _ = sender.try_send(Err(error));
    }
    retirement.retire(&mut child, &containment).await;
    Ok(())
}

async fn send_updates(
    socket: &mut (impl AsyncWrite + Unpin),
    updates: &mut watch::Receiver<Control>,
    mut previous: Control,
) -> Result<(), Error> {
    while updates.changed().await.is_ok() {
        let target = *updates.borrow_and_update();
        if target.bitrate != previous.bitrate {
            send_control(socket, Kind::SetBitrate, &target.bitrate.to_be_bytes()).await?;
        }
        if target.key_request != previous.key_request {
            send_control(socket, Kind::CaptureKeyframe, &[]).await?;
        }
        previous = target;
    }
    Ok(())
}

async fn send_control(
    socket: &mut (impl AsyncWrite + Unpin),
    kind: Kind,
    body: &[u8],
) -> Result<(), Error> {
    let header = Header {
        bytes: body.len() as u32,
        ..Header::empty(kind, 0)
    };
    timeout(DEADLINE, async {
        socket.write_all(&header.to_bytes()).await?;
        socket.write_all(body).await
    })
    .await
    .map_err(|_| Error::Timeout)??;
    Ok(())
}

async fn receive_frames(
    output: &mut (impl AsyncRead + Unpin),
    source: Source,
    sender: &mpsc::Sender<Result<VideoFrame, Error>>,
    control: &watch::Sender<Control>,
) -> Result<(), Error> {
    let mut frame = timeout(Duration::from_secs(15), read_frame(output, source, 0))
        .await
        .map_err(|_| Error::Timeout)??;
    let mut waiting_for_key = true;
    loop {
        let previous = frame.capture_time_us;
        if frame.keyframe || !waiting_for_key {
            waiting_for_key = sender.try_send(Ok(frame)).is_err();
            if waiting_for_key {
                request_keyframe(control);
            }
        }
        frame = read_frame(output, source, previous).await?;
    }
}

async fn read_frame(
    input: &mut (impl AsyncRead + Unpin),
    source: Source,
    previous: u64,
) -> Result<VideoFrame, Error> {
    let mut bytes = [0; HEADER_BYTES];
    input.read_exact(&mut bytes[..1]).await?;
    timeout(DEADLINE, async {
        input.read_exact(&mut bytes[1..]).await?;
        let header = Header::from_bytes(bytes)?;
        if !matches!(header.kind, Kind::CaptureFrame | Kind::HardwareFrame) || header.id <= previous
        {
            return Err(invalid().into());
        }
        let mut settings = [0; Settings::BYTES];
        input.read_exact(&mut settings).await?;
        let settings = Settings::from_bytes(settings)?;
        let size = header.bytes as usize;
        if settings.source != source || size == 0 || size > settings.packet_limit(header.key) {
            return Err(invalid().into());
        }
        let mut av1 = vec![0; size];
        input.read_exact(&mut av1).await?;
        Ok(VideoFrame {
            width: settings.width,
            height: settings.height,
            capture_time_us: header.id,
            av1,
            keyframe: header.key,
            hardware: header.kind == Kind::HardwareFrame,
            bitrate_bps: settings.bitrate,
            received: Instant::now(),
        })
    })
    .await
    .unwrap_or(Err(Error::Timeout))
}
