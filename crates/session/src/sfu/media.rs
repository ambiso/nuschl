// SPDX-License-Identifier: EUPL-1.2
//! Encoded media for a two-person native call. The WSS connection owns
//! signaling, and the isolated RTC worker owns ICE, DTLS, SRTP and SFrame.

use super::{Action, ClientMessage, Codec, Connection, Error, Outcome, ServerMessage, write};
use nuschl_ipc::rtc::{self, Command, Description, Event, LegRole, Reply, Track, Worker};
use std::{
    future::pending,
    net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr},
    os::unix::net::UnixStream,
    path::PathBuf,
    time::Duration,
};
use tokio::{
    net::UdpSocket,
    sync::mpsc,
    task::JoinHandle,
    time::{Instant, sleep_until},
};

pub use rtc::Purpose;

const PUBLISH: u32 = 1;
const SUBSCRIBE: u32 = 2;
const UDP_BURST: usize = 64;

pub struct MediaOptions {
    /// Path of the installed `nuschl-rtc-worker`.
    pub executable: PathBuf,
    /// UDP port on the same authenticated SFU address as WSS (default 3479).
    pub sfu_port: u16,
    /// The source to publish, or `None` to receive video.
    pub publish: Option<Purpose>,
}

pub enum MediaEvent {
    SendReady,
    BandwidthEstimate {
        bitrate: u64,
    },
    EncoderLimits {
        height: u16,
        fps: u8,
        bitrate: u64,
    },
    KeyframeRequired,
    Frame {
        publisher: u64,
        purpose: Purpose,
        timestamp: u64,
        contiguous: bool,
        key_frame: bool,
        data: Vec<u8>,
    },
}

struct Video {
    timestamp: u64,
    key_frame: bool,
    data: Vec<u8>,
}

pub struct Media {
    input: mpsc::Sender<Video>,
    events: mpsc::Receiver<Result<MediaEvent, Error>>,
    task: JoinHandle<()>,
    source: Option<Purpose>,
}

impl Media {
    /// Submit an encoded access unit. A full queue drops this frame. RTP
    /// timestamps use the 90 kHz video clock and increase for the source.
    pub fn send_video(
        &self,
        timestamp: u64,
        key_frame: bool,
        data: Vec<u8>,
    ) -> Result<bool, Error> {
        let source = self
            .source
            .ok_or(Error::Protocol("call has no publisher"))?;
        if data.is_empty() || data.len() > source.frame_limit() {
            return Err(Error::Protocol("invalid encoded frame size"));
        }
        match self.input.try_send(Video {
            timestamp,
            key_frame,
            data,
        }) {
            Ok(()) => Ok(true),
            Err(mpsc::error::TrySendError::Full(_)) => Ok(false),
            Err(mpsc::error::TrySendError::Closed(_)) => Err(Error::Protocol("media ended")),
        }
    }

    pub async fn next(&mut self) -> Option<Result<MediaEvent, Error>> {
        self.events.recv().await
    }

    pub async fn stop(self) -> Result<(), Error> {
        drop(self.input);
        drop(self.events);
        self.task
            .await
            .map_err(|_| Error::Protocol("media task stopped"))
    }
}

impl Connection {
    /// Start native encoded video over the configured SFU. Calls use a supplied
    /// publisher or viewer role and keep their signaling connection until stop.
    pub async fn into_media(self, options: MediaOptions) -> Result<Media, Error> {
        if self.policy.relay_only {
            return Err(Error::Protocol(
                "native media requires TURN in relay-only mode",
            ));
        }
        if options.sfu_port == 0
            || options
                .publish
                .is_some_and(|kind| matches!(kind, Purpose::Microphone | Purpose::SystemAudio))
        {
            return Err(Error::Protocol("invalid video source or SFU port"));
        }
        if self.welcome.peers.len() > 1 {
            return Err(Error::Protocol("native media requires a two-person call"));
        }
        let destination = SocketAddr::new(
            self.socket.get_ref().get_ref().0.peer_addr()?.ip(),
            options.sfu_port,
        );
        let (epochs, key_receiver) = UnixStream::pair()?;
        let mut worker = Worker::start(
            options.executable,
            self.scope.generation,
            key_receiver.into(),
        )
        .await?;
        if let Err(error) = self
            .scope
            .crypto
            .attach_media(self.scope.generation, epochs.into())
            .await
        {
            let _ = worker.stop().await;
            return Err(error.into());
        }
        let codec = match self.welcome.codec {
            Codec::Av1 => rtc::Codec::Av1,
            Codec::Vp9 => rtc::Codec::Vp9,
        };
        let source = options.publish;
        let (input, frames) = mpsc::channel(1);
        let (output, events) = mpsc::channel(4);
        let task = tokio::spawn(async move {
            let mut connection = self;
            let result = run(
                &mut connection,
                &mut worker,
                destination,
                codec,
                source,
                frames,
                &output,
            )
            .await;
            let _ = connection.close().await;
            let _ = worker.stop().await;
            if let Err(error) = result {
                let _ = output.send(Err(error)).await;
            }
        });
        Ok(Media {
            input,
            events,
            task,
            source,
        })
    }
}

struct Leg {
    id: u32,
    socket: UdpSocket,
    wake: Option<Instant>,
}

impl Leg {
    async fn open(
        worker: &mut Worker,
        id: u32,
        destination: SocketAddr,
        codec: rtc::Codec,
        role: LegRole,
    ) -> Result<Self, Error> {
        let address = match destination {
            SocketAddr::V4(_) => IpAddr::V4(Ipv4Addr::UNSPECIFIED),
            SocketAddr::V6(_) => IpAddr::V6(Ipv6Addr::UNSPECIFIED),
        };
        let socket = UdpSocket::bind((address, 0)).await?;
        socket.connect(destination).await?;
        let local = socket.local_addr()?;
        done(
            worker,
            Command::OpenLeg {
                leg: id,
                local,
                codec,
                role,
            },
        )
        .await?;
        Ok(Self {
            id,
            socket,
            wake: None,
        })
    }

    async fn datagram(&mut self, worker: &mut Worker, data: &[u8]) -> Result<(), Error> {
        if data.is_empty() || data.len() > rtc::MAX_DATAGRAM {
            return Ok(());
        }
        let source = self.socket.peer_addr()?;
        worker
            .exchange(
                Command::Datagram {
                    leg: self.id,
                    source,
                },
                data.to_vec(),
            )
            .await?;
        self.wake = Some(Instant::now());
        Ok(())
    }
}

async fn receive(leg: Option<&Leg>, bytes: &mut [u8]) -> std::io::Result<usize> {
    match leg {
        Some(leg) => leg.socket.recv(bytes).await,
        None => pending().await,
    }
}

async fn wake(leg: Option<&Leg>) {
    match leg.and_then(|leg| leg.wake) {
        Some(until) => sleep_until(until).await,
        None => pending().await,
    }
}

async fn done(worker: &mut Worker, command: Command) -> Result<(), Error> {
    match worker.exchange(command, Vec::new()).await?.kind {
        Reply::Done => Ok(()),
        _ => Err(Error::Protocol("RTC worker rejected media setup")),
    }
}

async fn describe(worker: &mut Worker, command: Command) -> Result<Description, Error> {
    match worker.exchange(command, Vec::new()).await?.kind {
        Reply::Description { description, .. } => Ok(description),
        _ => Err(Error::Protocol("RTC worker rejected media description")),
    }
}

struct Pair {
    peer: Option<u64>,
    ready: bool,
    publishing: bool,
}

impl Pair {
    async fn connect(&mut self, connection: &mut Connection, peer: u64) -> Result<(), Error> {
        if peer == 0 || peer == connection.welcome.peer || self.peer.is_some() {
            return Err(Error::Protocol("invalid native call peer"));
        }
        connection
            .signal(Action::Pair {
                peer,
                publisher: self.publishing,
            })
            .await?;
        self.peer = Some(peer);
        Ok(())
    }

    async fn key(&mut self, connection: &mut Connection, peer: u64, data: &str) {
        if self.peer != Some(peer) {
            return;
        }
        let Ok(data) = hex::decode(data) else {
            return;
        };
        let result = connection.signal(Action::Key { peer, data }).await;
        self.ready |= matches!(
            result,
            Ok(Outcome::Ready { .. } | Outcome::Send { ready: true, .. })
        );
    }
}

fn signaling_id(connection: &Connection, leg: u32) -> [u8; 16] {
    let mut id = [0; 16];
    id[..8].copy_from_slice(&connection.scope.generation.to_be_bytes());
    id[8..12].copy_from_slice(&leg.to_be_bytes());
    id
}

async fn run(
    connection: &mut Connection,
    worker: &mut Worker,
    destination: SocketAddr,
    codec: rtc::Codec,
    source: Option<Purpose>,
    mut frames: mpsc::Receiver<Video>,
    events: &mpsc::Sender<Result<MediaEvent, Error>>,
) -> Result<(), Error> {
    let publishing = source.is_some();
    let mut pair = Pair {
        peer: None,
        ready: false,
        publishing,
    };
    let mut other = connection.welcome.peers.first().map(|peer| peer.id);
    if publishing && let Some(peer) = other {
        pair.connect(connection, peer).await?;
    }

    let mut offered = None;
    let mut publish = if let Some(purpose) = source {
        let leg = Leg::open(worker, PUBLISH, destination, codec, LegRole::SfuPublish).await?;
        let mut description = describe(
            worker,
            Command::Offer {
                leg: PUBLISH,
                id: signaling_id(connection, PUBLISH),
                purposes: vec![purpose],
            },
        )
        .await?;
        let sdp = std::mem::take(&mut description.sdp);
        let screens = description
            .tracks
            .iter()
            .filter(|track| track.purpose == Purpose::ScreenVideo)
            .map(|track| track.mid.clone())
            .collect();
        write(
            &mut connection.socket,
            &ClientMessage::Offer { sdp, screens },
        )
        .await?;
        offered = Some(description);
        Some(leg)
    } else {
        None
    };
    let mut subscribe =
        Leg::open(worker, SUBSCRIBE, destination, codec, LegRole::SfuSubscribe).await?;
    let mut pub_buffer = [0; rtc::MAX_DATAGRAM + 1];
    let mut sub_buffer = [0; rtc::MAX_DATAGRAM + 1];
    let mut connected = false;
    let mut sending = false;
    loop {
        tokio::select! {
            message = connection.next() => match message? {
                ServerMessage::Joined { peer, .. } => {
                    if other.replace(peer).is_some() { return Err(Error::Protocol("native media requires a two-person call")); }
                    if publishing { pair.connect(connection, peer).await?; }
                }
                ServerMessage::Left { peer } if other == Some(peer) => return Ok(()),
                ServerMessage::Offer { sdp, tracks } => {
                    if sdp.len() > rtc::MAX_SDP || tracks.len() > 16 { return Err(Error::Protocol("invalid native SFU offer")); }
                    let tracks = tracks.into_iter().map(|track| {
                        let purpose = match track.kind.as_ref() {
                            "camera" => Purpose::Camera,
                            "screen" => Purpose::ScreenVideo,
                            _ => return Err(Error::Protocol("invalid media source")),
                        };
                        if publishing || other != Some(track.peer) { return Err(Error::Protocol("invalid media publisher")); }
                        Ok(Track { mid: track.mid, purpose, publisher: Some(track.peer) })
                    }).collect::<Result<Vec<_>, _>>()?;
                    if pair.peer.is_none() && !tracks.is_empty() && let Some(peer) = other { pair.connect(connection, peer).await?; }
                    let description = Description { id: signaling_id(connection, SUBSCRIBE), sdp, tracks };
                    let answer = describe(worker, Command::AcceptOffer { leg: SUBSCRIBE, description }).await?;
                    write(&mut connection.socket, &ClientMessage::Answer { sdp: answer.sdp }).await?;
                    subscribe.wake = Some(Instant::now());
                }
                ServerMessage::Answer { sdp } => {
                    let mut description = offered.take().ok_or(Error::Protocol("unexpected publish answer"))?;
                    description.sdp = sdp;
                    done(worker, Command::AcceptAnswer { leg: PUBLISH, description }).await?;
                    if let Some(leg) = &mut publish { leg.wake = Some(Instant::now()); }
                }
                ServerMessage::Key { from, data } => pair.key(connection, from, &data).await,
                ServerMessage::Quality { max_height, max_fps, max_bitrate, .. }
                    if (16..=2160).contains(&max_height) && (1..=120).contains(&max_fps) && (64_000..=50_000_000).contains(&max_bitrate) => {
                    let _ = events.try_send(Ok(MediaEvent::EncoderLimits { height: max_height, fps: max_fps, bitrate: max_bitrate }));
                }
                ServerMessage::Codec { codec: changed } if changed != connection.welcome.codec => return Err(Error::Protocol("native call codec changed")),
                ServerMessage::Error { reason } => return Err(Error::Refused(reason.into_owned())),
                _ => {}
            },
            frame = frames.recv() => {
                let Some(Video { timestamp, key_frame, data }) = frame else { return Ok(()); };
                if let (Some(purpose), Some(publish), true) = (source, &mut publish, sending) {
                    worker.exchange(Command::Write { purpose, timestamp, key_frame }, data).await?;
                    publish.wake = Some(Instant::now());
                }
            }
            received = receive(publish.as_ref(), &mut pub_buffer) => {
                if let Some(leg) = &mut publish { leg.datagram(worker, &pub_buffer[..received?]).await?; }
            }
            received = subscribe.socket.recv(&mut sub_buffer) => subscribe.datagram(worker, &sub_buffer[..received?]).await?,
            () = wake(publish.as_ref()) => {
                if let Some(leg) = &mut publish { connected |= drain(worker, leg, other, events).await?; }
            }
            () = wake(Some(&subscribe)) => { drain(worker, &mut subscribe, other, events).await?; }
        }
        if !sending && connected && pair.ready && publishing {
            done(worker, Command::SetSendLeg { leg: PUBLISH }).await?;
            sending = true;
            if events.send(Ok(MediaEvent::SendReady)).await.is_err() {
                return Ok(());
            }
        }
    }
}

async fn drain(
    worker: &mut Worker,
    leg: &mut Leg,
    peer: Option<u64>,
    events: &mpsc::Sender<Result<MediaEvent, Error>>,
) -> Result<bool, Error> {
    let mut connected = false;
    for _ in 0..UDP_BURST {
        let packet = worker
            .exchange(Command::Poll { leg: leg.id }, Vec::new())
            .await?;
        match packet.kind {
            Reply::Wake { after_millis, .. } => {
                leg.wake = Some(Instant::now() + Duration::from_millis(after_millis));
                return Ok(connected);
            }
            Reply::Datagram { destination, .. } if destination == leg.socket.peer_addr()? => {
                leg.socket.send(&packet.data).await?;
            }
            Reply::Datagram { .. } => {}
            Reply::Event { event, .. } => {
                let event = match event {
                    Event::Connected => {
                        connected = true;
                        continue;
                    }
                    Event::Disconnected => {
                        return Err(Error::Protocol("native media transport disconnected"));
                    }
                    Event::BandwidthEstimate { bitrate } if leg.id == PUBLISH => {
                        MediaEvent::BandwidthEstimate { bitrate }
                    }
                    Event::Keyframe { .. } if leg.id == PUBLISH => MediaEvent::KeyframeRequired,
                    Event::Frame {
                        publisher,
                        purpose,
                        timestamp,
                        contiguous,
                        key_frame,
                    } if leg.id == SUBSCRIBE && peer == Some(publisher) => MediaEvent::Frame {
                        publisher,
                        purpose,
                        timestamp,
                        contiguous,
                        key_frame,
                        data: packet.data,
                    },
                    _ => continue,
                };
                let _ = events.try_send(Ok(event));
            }
            _ => return Err(Error::Protocol("invalid RTC worker output")),
        }
    }
    leg.wake = Some(Instant::now());
    Ok(connected)
}
