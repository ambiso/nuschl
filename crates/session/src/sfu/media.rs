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
    sync::atomic::{AtomicU64, Ordering},
    time::Duration,
};
use tokio::{
    net::UdpSocket,
    sync::mpsc,
    task::JoinHandle,
    time::{Instant, sleep, sleep_until, timeout},
};

pub use rtc::Purpose;

const PUBLISH: u32 = 1;
const SUBSCRIBE: u32 = 2;
const UDP_BURST: usize = 64;
const MAX_PENDING_FRAME_AGE: Duration = Duration::from_millis(150);
const KEYFRAME_INTERVAL: Duration = Duration::from_secs(1);

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
    sequence: u64,
    queued: Instant,
    timestamp: u64,
    key_frame: bool,
    data: Vec<u8>,
}

pub struct Media {
    input: mpsc::Sender<Video>,
    sequence: AtomicU64,
    events: mpsc::Receiver<Result<MediaEvent, Error>>,
    keyframes: mpsc::Receiver<()>,
    task: JoinHandle<()>,
    source: Option<Purpose>,
}

impl Media {
    /// Submit an independent encoded frame after [`MediaEvent::SendReady`].
    pub fn send_keyframe(&self, timestamp: u64, data: Vec<u8>) -> Result<bool, Error> {
        self.send_frame(timestamp, true, data)
    }

    /// Submit an encoded video frame. Timestamps use the 90 kHz RTP clock. A
    /// full queue drops the frame and dependent frames wait for a keyframe.
    pub fn send_frame(
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
        let sequence = self
            .sequence
            .fetch_add(1, Ordering::Relaxed)
            .wrapping_add(1);
        let frame = Video {
            sequence,
            queued: Instant::now(),
            timestamp,
            key_frame,
            data,
        };
        match self.input.try_send(frame) {
            Ok(()) => Ok(true),
            Err(mpsc::error::TrySendError::Full(_)) => Ok(false),
            Err(mpsc::error::TrySendError::Closed(_)) => Err(Error::Protocol("media ended")),
        }
    }

    pub async fn next(&mut self) -> Option<Result<MediaEvent, Error>> {
        tokio::select! {
            biased;
            requested = self.keyframes.recv() => match requested {
                Some(()) => Some(Ok(MediaEvent::KeyframeRequired)),
                None => self.events.recv().await,
            },
            event = self.events.recv() => event,
        }
    }

    pub async fn stop(self) -> Result<(), Error> {
        drop(self.input);
        drop(self.events);
        drop(self.keyframes);
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
        let mut destination = self.socket.get_ref().get_ref().0.peer_addr()?;
        destination.set_port(options.sfu_port);
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
        let (requests, keyframes) = mpsc::channel(1);
        let task = tokio::spawn(async move {
            let mut connection = self;
            let result = run(
                &mut connection,
                &mut worker,
                destination,
                codec,
                source,
                frames,
                Output {
                    events: &output,
                    keyframes: &requests,
                },
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
            sequence: AtomicU64::new(0),
            events,
            keyframes,
            task,
            source,
        })
    }
}

struct Leg {
    id: u32,
    socket: UdpSocket,
    wake: Option<Instant>,
    camera: FrameGate,
    screen: FrameGate,
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
            camera: FrameGate::new(),
            screen: FrameGate::new(),
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

struct Output<'a> {
    events: &'a mpsc::Sender<Result<MediaEvent, Error>>,
    keyframes: &'a mpsc::Sender<()>,
}

struct FrameGate {
    awaiting_key: bool,
    requested: Option<Instant>,
}

impl FrameGate {
    fn new() -> Self {
        Self {
            awaiting_key: true,
            requested: None,
        }
    }

    fn request(&mut self) -> bool {
        let now = Instant::now();
        if self
            .requested
            .is_some_and(|at| now - at < KEYFRAME_INTERVAL)
        {
            return false;
        }
        self.requested = Some(now);
        true
    }

    fn arrived(&mut self, key_frame: bool) {
        if key_frame {
            self.awaiting_key = false;
        }
    }
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
    output: Output<'_>,
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
    let mut publish_mid = None;
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
        let [track] = description.tracks.as_slice() else {
            return Err(Error::Protocol("invalid publish track"));
        };
        if track.purpose != purpose || track.publisher.is_some() {
            return Err(Error::Protocol("invalid publish track"));
        }
        publish_mid = Some(track.mid.clone());
        let screens = match purpose {
            Purpose::ScreenVideo => vec![track.mid.clone()],
            _ => Vec::new(),
        };
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
    let mut send_gate = FrameGate::new();
    let mut sequence = 0_u64;
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
                ServerMessage::Quality { source, max_height, max_fps, max_bitrate, .. }
                    if publish_mid.as_ref() == Some(&source) && (16..=2160).contains(&max_height) && (1..=120).contains(&max_fps) && (64_000..=50_000_000).contains(&max_bitrate) => {
                    let _ = output.events.try_send(Ok(MediaEvent::EncoderLimits { height: max_height, fps: max_fps, bitrate: max_bitrate }));
                }
                ServerMessage::Codec { codec: changed } if changed != connection.welcome.codec => return Err(Error::Protocol("native call codec changed")),
                ServerMessage::Error { reason } => return Err(Error::Refused(reason.into_owned())),
                _ => {}
            },
            frame = frames.recv() => {
                let Some(frame) = frame else { return Ok(()); };
                if let (Some(purpose), Some(publish), true) = (source, &mut publish, sending) {
                    if frame.sequence != sequence.wrapping_add(1) {
                        send_gate.awaiting_key = true;
                    }
                    sequence = frame.sequence;
                    if frame.queued.elapsed() <= MAX_PENDING_FRAME_AGE && (!send_gate.awaiting_key || frame.key_frame) {
                        let command = Command::Write { purpose, timestamp: frame.timestamp, key_frame: frame.key_frame };
                        match worker.exchange(command, frame.data).await?.kind {
                            Reply::Done => {
                                send_gate.arrived(frame.key_frame);
                                publish.wake = Some(Instant::now());
                                continue;
                            }
                            Reply::Rejected => {}
                            _ => return Err(Error::Protocol("invalid RTC write reply")),
                        }
                    }
                    send_gate.awaiting_key = true;
                    if send_gate.request() {
                        let _ = output.keyframes.try_send(());
                    }
                }
            }
            received = receive(publish.as_ref(), &mut pub_buffer) => {
                if let Some(leg) = &mut publish { leg.datagram(worker, &pub_buffer[..received?]).await?; }
            }
            received = subscribe.socket.recv(&mut sub_buffer) => subscribe.datagram(worker, &sub_buffer[..received?]).await?,
            () = wake(publish.as_ref()) => {
                if let Some(leg) = &mut publish { connected |= drain(worker, leg, other, &output).await?; }
            }
            () = wake(Some(&subscribe)) => { drain(worker, &mut subscribe, other, &output).await?; }
        }
        if !sending && connected && pair.ready && publishing {
            // The crypto worker sent the epoch on a separate socket. Wait for
            // the RTC process to install it before admitting frames.
            timeout(Duration::from_secs(1), async {
                loop {
                    if matches!(
                        worker
                            .exchange(Command::SetSendLeg { leg: PUBLISH }, Vec::new())
                            .await?
                            .kind,
                        Reply::Done
                    ) {
                        return Ok::<(), Error>(());
                    }
                    sleep(Duration::from_millis(5)).await;
                }
            })
            .await
            .map_err(|_| Error::Timeout)??;
            sending = true;
            if output.events.send(Ok(MediaEvent::SendReady)).await.is_err() {
                return Ok(());
            }
        }
    }
}

async fn drain(
    worker: &mut Worker,
    leg: &mut Leg,
    peer: Option<u64>,
    output: &Output<'_>,
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
                    Event::Keyframe { .. } if leg.id == PUBLISH => {
                        let _ = output.keyframes.try_send(());
                        continue;
                    }
                    Event::Frame {
                        publisher,
                        purpose,
                        timestamp,
                        contiguous,
                        key_frame,
                    } if leg.id == SUBSCRIBE && peer == Some(publisher) => {
                        let gate = match purpose {
                            Purpose::Camera => &mut leg.camera,
                            Purpose::ScreenVideo => &mut leg.screen,
                            _ => continue,
                        };
                        if (key_frame || (contiguous && !gate.awaiting_key))
                            && output
                                .events
                                .try_send(Ok(MediaEvent::Frame {
                                    publisher,
                                    purpose,
                                    timestamp,
                                    contiguous,
                                    key_frame,
                                    data: packet.data,
                                }))
                                .is_ok()
                        {
                            gate.arrived(key_frame);
                        } else {
                            gate.awaiting_key = true;
                            if gate.request() {
                                worker
                                    .exchange(
                                        Command::RequestKeyframe {
                                            leg: leg.id,
                                            purpose,
                                        },
                                        Vec::new(),
                                    )
                                    .await?;
                            }
                        }
                        continue;
                    }
                    _ => continue,
                };
                let _ = output.events.try_send(Ok(event));
            }
            _ => return Err(Error::Protocol("invalid RTC worker output")),
        }
    }
    leg.wake = Some(Instant::now());
    Ok(connected)
}
