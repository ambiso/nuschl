// SPDX-License-Identifier: EUPL-1.2

use super::{Command, Packet, Reply, read_reply, write_request};
use crate::host::preview_lifecycle::Retirement;
use nuschl_sandbox::{Containment, Role};
#[cfg(target_os = "linux")]
use std::os::{fd::OwnedFd, unix::net::UnixStream as Socket};
use std::{io, path::PathBuf, process::Stdio, time::Duration};
use tokio::{
    io::{AsyncRead, AsyncWrite},
    process::Child,
    sync::{Semaphore, mpsc, oneshot},
    task::JoinHandle,
    time::timeout,
};

#[cfg(target_os = "linux")]
use tokio::net::{
    UnixStream,
    unix::{OwnedReadHalf as Reader, OwnedWriteHalf as Writer},
};
#[cfg(target_os = "linux")]
type Epochs = OwnedFd;
#[cfg(not(target_os = "linux"))]
use tokio::process::{ChildStdin as Writer, ChildStdout as Reader};
#[cfg(not(target_os = "linux"))]
type Epochs = io::PipeReader;

struct Channel {
    child: Child,
    reader: Reader,
    writer: Writer,
    containment: Containment,
}

const DEADLINE: Duration = Duration::from_secs(5);
static WORKERS: Semaphore = Semaphore::const_new(2);

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error(transparent)]
    Io(#[from] io::Error),
    #[error(transparent)]
    Wire(#[from] crate::Error),
    #[error(transparent)]
    Sandbox(#[from] nuschl_sandbox::Error),
    #[error("no RTC worker slot is available")]
    Busy,
    #[error("a Tokio runtime is required for RTC")]
    Runtime,
    #[error("RTC worker confinement could not be verified")]
    Confinement,
    #[error("RTC worker exchange timed out")]
    Timeout,
    #[error("invalid RTC worker reply")]
    Protocol,
    #[error("RTC worker has stopped")]
    Stopped,
}

struct Request {
    kind: Command,
    data: Vec<u8>,
    reply: oneshot::Sender<Result<Packet<Reply>, Error>>,
}

pub struct Worker {
    commands: mpsc::Sender<Request>,
    task: JoinHandle<()>,
}

impl Worker {
    /// The RTC worker reads keys from `epochs`, connected directly to the crypto
    /// worker. The control channel never carries those keys.
    pub async fn start(
        executable: PathBuf,
        generation: u64,
        epochs: Epochs,
    ) -> Result<Self, Error> {
        let runtime = tokio::runtime::Handle::try_current().map_err(|_| Error::Runtime)?;
        let permit = WORKERS.try_acquire().map_err(|_| Error::Busy)?;
        let (commands, queue) = mpsc::channel(1);
        let (ready, started) = oneshot::channel();
        let task = runtime.spawn(async move {
            let _permit = permit;
            supervise(executable, generation, epochs, queue, ready).await;
        });
        started.await.map_err(|_| Error::Stopped)??;
        Ok(Self { commands, task })
    }

    pub async fn exchange(&mut self, kind: Command, data: Vec<u8>) -> Result<Packet<Reply>, Error> {
        let (reply, result) = oneshot::channel();
        self.commands
            .send(Request { kind, data, reply })
            .await
            .map_err(|_| Error::Stopped)?;
        result.await.map_err(|_| Error::Stopped)?
    }

    pub async fn stop(self) -> Result<(), Error> {
        drop(self.commands);
        self.task.await.map_err(|_| Error::Stopped)
    }
}

#[cfg(target_os = "linux")]
async fn spawn(executable: PathBuf, epochs: Epochs) -> Result<Channel, Error> {
    let (supervisor, worker) = Socket::pair()?;
    supervisor.set_nonblocking(true)?;
    let (reader, writer) = UnixStream::from_std(supervisor)?.into_split();
    let mut child = nuschl_sandbox::command(&executable, Role::Rtc)?
        .stdin(Stdio::from(epochs))
        .stdout(Stdio::from(OwnedFd::from(worker)))
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()?;
    let containment = match nuschl_sandbox::contain(&child) {
        Ok(containment) => containment,
        Err(error) => {
            nuschl_sandbox::reap(&mut child).await;
            return Err(error.into());
        }
    };
    Ok(Channel {
        child,
        reader,
        writer,
        containment,
    })
}

#[cfg(not(target_os = "linux"))]
async fn spawn(executable: PathBuf, epochs: Epochs) -> Result<Channel, Error> {
    let nuschl_sandbox::Worker {
        child,
        input: writer,
        output: reader,
        containment,
    } = nuschl_sandbox::spawn_call_worker(&executable, Role::Rtc, Stdio::from(epochs)).await?;
    Ok(Channel {
        child,
        reader,
        writer,
        containment,
    })
}

async fn supervise(
    executable: PathBuf,
    generation: u64,
    epochs: Epochs,
    mut queue: mpsc::Receiver<Request>,
    ready: oneshot::Sender<Result<(), Error>>,
) {
    let Channel {
        mut child,
        mut reader,
        mut writer,
        containment,
    } = match spawn(executable, epochs).await {
        Ok(parts) => parts,
        Err(error) => {
            let _ = ready.send(Err(error));
            return;
        }
    };
    let mut retirement = Retirement::new(&mut child);
    let start = async {
        retirement.ready().await.map_err(|_| Error::Confinement)?;
        let response = exchange(
            &mut reader,
            &mut writer,
            1,
            Command::Start { generation },
            Vec::new(),
        )
        .await?;
        if !matches!(response.kind, Reply::Done) {
            return Err(Error::Protocol);
        }
        Ok(())
    }
    .await;
    let healthy = start.is_ok();
    if ready.send(start).is_ok() && healthy {
        let mut id = 1_u64;
        while let Some(Request { kind, data, reply }) = queue.recv().await {
            if reply.is_closed() {
                continue;
            }
            let Some(next) = id.checked_add(1) else {
                break;
            };
            id = next;
            let response = exchange(&mut reader, &mut writer, id, kind, data).await;
            let failed = response.is_err();
            let _ = reply.send(response);
            if failed {
                break;
            }
        }
    }
    drop(writer);
    drop(reader);
    retirement.retire(&mut child, &containment).await;
}

async fn exchange(
    reader: &mut (impl AsyncRead + Unpin),
    writer: &mut (impl AsyncWrite + Unpin),
    id: u64,
    kind: Command,
    data: Vec<u8>,
) -> Result<Packet<Reply>, Error> {
    timeout(DEADLINE, async {
        let request = Packet { id, kind, data };
        write_request(writer, &request).await?;
        Ok(read_reply(reader, &request).await?)
    })
    .await
    .map_err(|_| Error::Timeout)?
}
