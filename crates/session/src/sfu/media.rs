// SPDX-License-Identifier: EUPL-1.2
//! Encoded media for a two-person native call. The WSS connection owns
//! signaling, and the isolated RTC worker owns ICE, DTLS, SRTP and SFrame.

use super::turn::NetworkLeg;
use super::{
    Action, ClientMessage, Codec, Connection, Error, HANDSHAKE, Outcome, Quality, ServerMessage,
    VideoKind, write,
};
use nuschl_core::network::Destination;
use nuschl_ipc::rtc::{self, Command, Description, Event, LegRole, Reply, Track, Worker};
use nuschl_stream_quality::{QualityRequest, Throttle};
use std::{
    future::pending,
    net::SocketAddr,
    os::unix::net::UnixStream,
    path::PathBuf,
    sync::atomic::{AtomicU64, Ordering},
    time::Duration,
};
use tokio::{
    sync::{mpsc, watch},
    task::JoinHandle,
    time::{Instant, sleep, sleep_until, timeout},
};

mod direct;
use direct::{Candidates, Direct, Signal};

pub use rtc::Purpose;

const PUBLISH: u32 = 1;
const SUBSCRIBE: u32 = 2;
const UDP_BURST: usize = 64;
const MAX_PENDING_FRAME_AGE: Duration = Duration::from_millis(150);
const KEYFRAME_INTERVAL: Duration = Duration::from_secs(1);
// A subscription accepts 48 SDP sections, including its control channel and first source.
const MAX_SOURCE_CHANGES: u8 = 46;

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
    /// The active media path changed. The SFU stays available for fallback.
    Direct {
        active: bool,
    },
    /// A negotiated video source was removed while the publisher stayed in the room.
    SourceEnded {
        purpose: Purpose,
    },
    Frame {
        publisher: u64,
        purpose: Purpose,
        timestamp: u64,
        contiguous: bool,
        key_frame: bool,
        data: Vec<u8>,
    },
}

enum QueuedEvent {
    Call(MediaEvent),
    SendReady(u8),
}

#[derive(Clone, Copy)]
struct EncoderCaps {
    revision: u8,
    height: u16,
    fps: u8,
    bitrate: u64,
}

enum Input {
    Frame(Video),
    RequestKeyframe(Purpose),
    #[cfg(test)]
    LoseDirect,
}

struct Video {
    sequence: u64,
    revision: u8,
    queued: Instant,
    timestamp: u64,
    key_frame: bool,
    data: Vec<u8>,
}

pub struct Media {
    input: mpsc::Sender<Input>,
    sequence: AtomicU64,
    preferences: watch::Sender<Preferences>,
    events: mpsc::Receiver<Result<QueuedEvent, Error>>,
    keyframes: mpsc::Receiver<()>,
    limits: watch::Receiver<Option<EncoderCaps>>,
    bandwidth: watch::Receiver<Option<u64>>,
    task: JoinHandle<()>,
    source: Option<Purpose>,
    source_revision: u8,
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
            revision: self.source_revision,
            queued: Instant::now(),
            timestamp,
            key_frame,
            data,
        };
        match self.input.try_send(Input::Frame(frame)) {
            Ok(()) => Ok(true),
            Err(mpsc::error::TrySendError::Full(_)) => Ok(false),
            Err(mpsc::error::TrySendError::Closed(_)) => Err(Error::Protocol("media ended")),
        }
    }

    /// Replace this publisher's camera or screen while keeping the call open.
    /// The latest request can wait for an outstanding offer or a viewer.
    /// Submit the first independent frame after the next [`MediaEvent::SendReady`].
    pub fn switch_source(&mut self, purpose: Purpose) -> Result<(), Error> {
        if self.source.is_none() || self.source == Some(purpose) || video_kind(purpose).is_none() {
            return Err(Error::Protocol("invalid replacement video source"));
        }
        if self.source_revision == MAX_SOURCE_CHANGES {
            return Err(Error::Protocol("video source limit reached for this call"));
        }
        let revision = self.source_revision + 1;
        self.preferences
            .send(Preferences::Source { purpose, revision })
            .map_err(|_| Error::Protocol("media ended"))?;
        self.source = Some(purpose);
        self.source_revision = revision;
        Ok(())
    }

    /// Set the ceiling for a subscribed video source. Auto fits the tile's
    /// device-pixel height. The latest choice for each source takes effect when
    /// the SFU accepts its next request.
    pub fn set_quality(
        &self,
        publisher: u64,
        purpose: Purpose,
        quality: Quality,
        tile_height: u32,
    ) -> Result<(), Error> {
        let kind = video_kind(purpose).ok_or(Error::Protocol("invalid video source"))?;
        if self.source.is_some() || publisher == 0 {
            return Err(Error::Protocol("invalid video subscription"));
        }
        if self.preferences.is_closed() {
            return Err(Error::Protocol("media ended"));
        }
        if quality == Quality::Auto && tile_height == 0 {
            return Ok(());
        }
        let request = QualityRequest {
            publisher,
            kind,
            tier: kind.request(quality, tile_height) as u8,
        };
        self.preferences.send_if_modified(|latest| {
            let Preferences::Quality(latest) = latest else {
                return false;
            };
            let slot = &mut latest[quality_index(kind)];
            if *slot == Some(request) {
                return false;
            }
            *slot = Some(request);
            true
        });
        Ok(())
    }

    /// Recover decoding for the active subscription with a fresh keyframe.
    /// Subsequent requests share the transport's keyframe cooldown.
    pub fn request_keyframe(&self, purpose: Purpose) -> Result<(), Error> {
        if self.source.is_some() || video_kind(purpose).is_none() {
            return Err(Error::Protocol("invalid video subscription"));
        }
        match self.input.try_send(Input::RequestKeyframe(purpose)) {
            Ok(()) | Err(mpsc::error::TrySendError::Full(_)) => Ok(()),
            Err(mpsc::error::TrySendError::Closed(_)) => Err(Error::Protocol("media ended")),
        }
    }

    #[cfg(test)]
    pub(super) async fn lose_direct(&self) {
        assert!(self.input.send(Input::LoseDirect).await.is_ok());
    }

    pub async fn next(&mut self) -> Option<Result<MediaEvent, Error>> {
        loop {
            let event = tokio::select! {
                biased;
                requested = self.keyframes.recv() => match requested {
                    Some(()) => return Some(Ok(MediaEvent::KeyframeRequired)),
                    None => self.events.recv().await,
                },
                event = self.events.recv() => event,
                changed = self.limits.changed() => {
                    if changed.is_err() {
                        self.events.recv().await
                    } else {
                        let caps = *self.limits.borrow_and_update();
                        if let Some(EncoderCaps { revision, height, fps, bitrate }) = caps
                            && revision == self.source_revision
                        {
                            return Some(Ok(MediaEvent::EncoderLimits { height, fps, bitrate }));
                        }
                        continue;
                    }
                }
                changed = self.bandwidth.changed() => {
                    if changed.is_err() {
                        self.events.recv().await
                    } else {
                        if let Some(bitrate) = *self.bandwidth.borrow_and_update() {
                            return Some(Ok(MediaEvent::BandwidthEstimate { bitrate }));
                        }
                        continue;
                    }
                }
            };
            match event? {
                Ok(QueuedEvent::Call(event)) => return Some(Ok(event)),
                Ok(QueuedEvent::SendReady(revision)) if revision == self.source_revision => {
                    return Some(Ok(MediaEvent::SendReady));
                }
                Err(error) => return Some(Err(error)),
                _ => {}
            }
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
        if self.policy.relay_only && self.welcome.turn.is_none() {
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
        let (preferences, changes) = watch::channel(Preferences::Quality([None; 2]));
        let (output, events) = mpsc::channel(4);
        let (requests, keyframes) = mpsc::channel(1);
        let (encoder_caps, limits) = watch::channel(None);
        let (upload_estimate, bandwidth) = watch::channel(None);
        let task = tokio::spawn(async move {
            let mut connection = self;
            let result = run(
                &mut connection,
                &mut worker,
                destination,
                codec,
                source,
                Inputs { frames, changes },
                Output {
                    events: &output,
                    keyframes: &requests,
                    limits: &encoder_caps,
                    bandwidth: &upload_estimate,
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
            preferences,
            events,
            keyframes,
            limits,
            bandwidth,
            task,
            source,
            source_revision: 0,
        })
    }
}

enum Network {
    Sfu(Box<NetworkLeg>),
    Direct {
        socket: tokio::net::UdpSocket,
        remote: Option<Candidates>,
    },
}

impl Network {
    fn address(&self) -> std::io::Result<SocketAddr> {
        match self {
            Self::Sfu(leg) => Ok(leg.address()),
            Self::Direct { socket, .. } => socket.local_addr(),
        }
    }

    fn permits(&self, address: SocketAddr) -> std::io::Result<bool> {
        match self {
            Self::Sfu(leg) => Ok(leg.peer()? == address),
            Self::Direct { remote, .. } => {
                Ok(remote.is_some_and(|remote| remote.contains(address)))
            }
        }
    }

    async fn receive(&mut self, data: &mut [u8]) -> std::io::Result<(usize, SocketAddr)> {
        match self {
            Self::Sfu(leg) => Ok((leg.recv(data).await?, leg.peer()?)),
            Self::Direct { socket, .. } => socket.recv_from(data).await,
        }
    }

    async fn send(&mut self, data: &[u8], destination: SocketAddr) -> std::io::Result<()> {
        if self.permits(destination)? {
            match self {
                Self::Sfu(leg) => leg.send(data).await?,
                Self::Direct { socket, .. } => {
                    socket.send_to(data, destination).await?;
                }
            }
        }
        Ok(())
    }
}

struct Leg {
    id: u32,
    network: Network,
    wake: Option<Instant>,
    connected: bool,
    selected: bool,
    bandwidth: Option<(Instant, u64)>,
    last_packet: Option<Instant>,
    camera: FrameGate,
    screen: FrameGate,
}

impl Leg {
    fn video_gate(&mut self, purpose: Purpose) -> Option<&mut FrameGate> {
        match purpose {
            Purpose::Camera => Some(&mut self.camera),
            Purpose::ScreenVideo => Some(&mut self.screen),
            _ => None,
        }
    }

    async fn request_keyframe(
        &mut self,
        worker: &mut Worker,
        purpose: Purpose,
    ) -> Result<(), Error> {
        if let Some(gate) = self.video_gate(purpose) {
            gate.awaiting_key = true;
            if gate.request() {
                worker
                    .exchange(
                        Command::RequestKeyframe {
                            leg: self.id,
                            purpose,
                        },
                        Vec::new(),
                    )
                    .await?;
                self.wake = Some(Instant::now());
            }
        }
        Ok(())
    }

    async fn start(
        worker: &mut Worker,
        id: u32,
        network: Network,
        codec: rtc::Codec,
        role: LegRole,
    ) -> Result<Self, Error> {
        done(
            worker,
            Command::OpenLeg {
                leg: id,
                local: network.address()?,
                codec,
                role,
                relayed: matches!(&network, Network::Sfu(leg) if leg.is_relayed()),
            },
        )
        .await?;
        Ok(Self {
            id,
            network,
            wake: None,
            connected: false,
            selected: false,
            bandwidth: None,
            last_packet: None,
            camera: FrameGate::new(),
            screen: FrameGate::new(),
        })
    }

    async fn datagram(
        &mut self,
        worker: &mut Worker,
        source: SocketAddr,
        data: &[u8],
    ) -> Result<(), Error> {
        if data.is_empty() || data.len() > rtc::MAX_DATAGRAM || !self.network.permits(source)? {
            return Ok(());
        }
        let reply = worker
            .exchange(
                Command::Datagram {
                    leg: self.id,
                    source,
                },
                data.to_vec(),
            )
            .await?;
        let now = Instant::now();
        if matches!(reply.kind, Reply::Done) && matches!(self.network, Network::Direct { .. }) {
            self.last_packet = Some(now);
        }
        self.wake = Some(now);
        Ok(())
    }
}

async fn receive(leg: Option<&mut Leg>, bytes: &mut [u8]) -> std::io::Result<(usize, SocketAddr)> {
    match leg {
        Some(leg) => leg.network.receive(bytes).await,
        None => pending().await,
    }
}

async fn wake(until: Option<Instant>) {
    match until {
        Some(until) if until > Instant::now() => sleep_until(until).await,
        Some(_) => {}
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
    events: &'a mpsc::Sender<Result<QueuedEvent, Error>>,
    keyframes: &'a mpsc::Sender<()>,
    // Feedback updates carry the latest value without holding up media I/O.
    limits: &'a watch::Sender<Option<EncoderCaps>>,
    bandwidth: &'a watch::Sender<Option<u64>>,
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

    async fn key(&mut self, connection: &mut Connection, peer: u64, data: &str) -> Option<Signal> {
        if self.peer != Some(peer) {
            return None;
        }
        let data = hex::decode(data).ok()?;
        match connection.signal(Action::Key { peer, data }).await.ok()? {
            Outcome::Ready { .. } | Outcome::Send { ready: true, .. } => self.ready = true,
            Outcome::Signal { data, .. } => return serde_json::from_slice(&data).ok(),
            _ => {}
        }
        None
    }
}

fn signaling_id(connection: &Connection, leg: u32) -> [u8; 16] {
    let mut id = [0; 16];
    id[..8].copy_from_slice(&connection.scope.generation.to_be_bytes());
    id[8..12].copy_from_slice(&leg.to_be_bytes());
    id
}

type QualityRequests = [Option<QualityRequest>; 2];

#[derive(Clone, Copy)]
enum Preferences {
    Quality(QualityRequests),
    Source { purpose: Purpose, revision: u8 },
}

struct Inputs {
    frames: mpsc::Receiver<Input>,
    changes: watch::Receiver<Preferences>,
}

#[derive(Default)]
struct ReceiveQuality {
    wanted: QualityRequests,
    sent: QualityRequests,
    next: [Option<Instant>; 2],
}

impl ReceiveQuality {
    fn next_request(&self, tracks: &[Track]) -> Option<(Instant, QualityRequest)> {
        self.wanted
            .iter()
            .enumerate()
            .filter_map(|(i, request)| {
                let request = (*request)?;
                if Some(request) == self.sent[i]
                    || !tracks.iter().any(|track| {
                        track.publisher == Some(request.publisher)
                            && video_kind(track.purpose) == Some(request.kind)
                    })
                {
                    return None;
                }
                Some((self.next[i].unwrap_or_else(Instant::now), request))
            })
            .min_by_key(|(at, _)| *at)
    }

    fn sent(&mut self, request: QualityRequest) {
        let i = quality_index(request.kind);
        self.sent[i] = Some(request);
        // Give signaling transit jitter the same 100 ms margin as the call page.
        self.next[i] = Some(Instant::now() + Throttle::INTERVAL + Duration::from_millis(100));
    }

    fn source_ended(&mut self, purpose: Purpose) {
        if let Some(kind) = video_kind(purpose) {
            self.sent[quality_index(kind)] = None;
        }
    }
}

fn video_kind(purpose: Purpose) -> Option<VideoKind> {
    match purpose {
        Purpose::Camera => Some(VideoKind::Camera),
        Purpose::ScreenVideo => Some(VideoKind::Screen),
        _ => None,
    }
}

fn quality_index(kind: VideoKind) -> usize {
    match kind {
        VideoKind::Camera => 0,
        VideoKind::Screen => 1,
    }
}

async fn run(
    connection: &mut Connection,
    worker: &mut Worker,
    destination: SocketAddr,
    codec: rtc::Codec,
    mut source: Option<Purpose>,
    mut inputs: Inputs,
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
    let mut source_change_deadline = None;
    let mut pending_source = None;
    let turn = connection
        .welcome
        .turn
        .as_ref()
        .filter(|_| connection.policy.relay_only);
    let (publish_path, subscribe_path) = tokio::try_join!(
        async {
            if publishing {
                NetworkLeg::open(destination, turn).await.map(Some)
            } else {
                Ok(None)
            }
        },
        NetworkLeg::open(destination, turn)
    )?;
    let mut publish = if let (Some(purpose), Some(path)) = (source, publish_path) {
        let leg = Leg::start(
            worker,
            PUBLISH,
            Network::Sfu(Box::new(path)),
            codec,
            LegRole::SfuPublish,
        )
        .await?;
        let (mid, description) = offer_publish(connection, worker, purpose, 0).await?;
        publish_mid = Some(mid);
        offered = Some(description);
        Some(leg)
    } else {
        None
    };
    let mut subscribe = Leg::start(
        worker,
        SUBSCRIBE,
        Network::Sfu(Box::new(subscribe_path)),
        codec,
        LegRole::SfuSubscribe,
    )
    .await?;
    let mut direct = if connection.policy.permits(Destination::DirectPeer) {
        Some(Direct::new(
            connection.socket.get_ref().get_ref().0.local_addr()?.ip(),
            connection
                .welcome
                .turn
                .as_ref()
                .map_or(&[], |turn| turn.urls.as_slice()),
        ))
    } else {
        None
    };
    let mut discovery = None;
    let mut desired = None;
    let mut direct_buffer = [0; rtc::MAX_DATAGRAM + 1];
    let mut subscribed = Vec::<Track>::new();
    let mut quality = ReceiveQuality::default();
    let mut pub_buffer = [0; rtc::MAX_DATAGRAM + 1];
    let mut sub_buffer = [0; rtc::MAX_DATAGRAM + 1];
    let mut sending = false;
    let mut send_gate = FrameGate::new();
    let mut sequence = 0_u64;
    let mut source_revision = 0;
    loop {
        let attempt = direct.as_ref().and_then(|direct| direct.leg.as_ref());
        let selected = attempt.is_some_and(|attempt| attempt.transport.selected);
        let attempting_direct = attempt.is_some() || discovery.is_some();
        let direct_wake = attempt
            .filter(|attempt| attempt.answered)
            .and_then(|attempt| attempt.transport.wake);
        let direct_deadline = direct.as_ref().and_then(Direct::deadline);
        let direct_connected = direct
            .as_mut()
            .and_then(|direct| direct.leg.as_mut())
            .filter(|attempt| attempt.answered)
            .map(|attempt| &mut attempt.transport);
        let publish_wake = publish.as_ref().and_then(|leg| leg.wake);
        let subscribe_wake = subscribe.wake;
        let next_quality = quality.next_request(&subscribed);
        let mut retreat = false;
        let mut notify = true;
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
                    let description = Description { id: signaling_id(connection, SUBSCRIBE), sdp, tracks: tracks.clone() };
                    let answer = describe(worker, Command::AcceptOffer { leg: SUBSCRIBE, description }).await?;
                    write(&mut connection.socket, &ClientMessage::Answer { sdp: answer.sdp }).await?;
                    // End old sources after queued frames and before any media from the new offer.
                    for track in subscribed.iter().filter(|track| !tracks.contains(track)) {
                        retreat |= attempting_direct;
                        quality.source_ended(track.purpose);
                        if let Some(gate) = subscribe.video_gate(track.purpose) {
                            *gate = FrameGate::new();
                        }
                        if output.events.send(Ok(QueuedEvent::Call(MediaEvent::SourceEnded { purpose: track.purpose }))).await.is_err() {
                            return Ok(());
                        }
                    }
                    subscribed = tracks;
                    subscribe.wake = Some(Instant::now());
                }
                ServerMessage::Answer { sdp } => {
                    let mut description = offered.take().ok_or(Error::Protocol("unexpected publish answer"))?;
                    description.sdp = sdp;
                    done(worker, Command::AcceptAnswer { leg: PUBLISH, description }).await?;
                    source_change_deadline = None;
                    if let Some(leg) = &mut publish { leg.wake = Some(Instant::now()); }
                }
                ServerMessage::Key { from, data } => {
                    if let Some(signal) = pair.key(connection, from, &data).await
                        && let Some(direct) = &mut direct
                    {
                        let result = match signal {
                            Signal::Ready if publishing => {
                                direct.ready = true;
                                Ok(())
                            }
                            Signal::Offer { candidates, description }
                                if !publishing && direct.can_accept() && discovery.is_none() =>
                            {
                                discovery = direct.accept(from, candidates, description, &subscribed);
                                Ok(())
                            }
                            Signal::Answer { candidates, description } if publishing => direct.answer(worker, from, candidates, description).await,
                            Signal::Connected { id } if publishing => {
                                if let Some(attempt) = &mut direct.leg && attempt.id == id {
                                    attempt.confirmed = true;
                                }
                                Ok(())
                            }
                            Signal::Close { id } => {
                                retreat = direct.leg.as_ref().is_some_and(|attempt| attempt.id == id);
                                notify = false;
                                Ok(())
                            }
                            _ => Ok(()),
                        };
                        retreat |= result.is_err();
                    }
                }
                ServerMessage::Quality { source, max_height, max_fps, max_bitrate, .. }
                    if publish_mid.as_ref() == Some(&source) && (16..=2160).contains(&max_height) && (1..=120).contains(&max_fps) && (64_000..=50_000_000).contains(&max_bitrate) => {
                    desired = Some(max_bitrate);
                    output.limits.send_replace(Some(EncoderCaps {
                        revision: source_revision, height: max_height, fps: max_fps, bitrate: max_bitrate,
                    }));
                }
                ServerMessage::Codec { codec: changed } if changed != connection.welcome.codec => return Err(Error::Protocol("native call codec changed")),
                ServerMessage::Error { reason } => return Err(Error::Refused(reason.into_owned())),
                _ => {}
            },
            changed = inputs.changes.changed() => {
                if changed.is_err() {
                    return Ok(());
                }
                let update = *inputs.changes.borrow_and_update();
                match update {
                    Preferences::Quality(wanted) => quality.wanted = wanted,
                    Preferences::Source { purpose, revision } => pending_source = Some((purpose, revision)),
                }
            }
            () = wake(next_quality.map(|(at, _)| at)) => {
                // A new choice can arrive as the timer expires.
                if let Preferences::Quality(wanted) = *inputs.changes.borrow_and_update() {
                    quality.wanted = wanted;
                }
                if let Some((at, request)) = quality.next_request(&subscribed)
                    && at <= Instant::now()
                {
                    write(&mut connection.socket, &ClientMessage::Quality {
                        request: hex::encode(request.to_bytes()),
                    }).await?;
                    quality.sent(request);
                }
            }
            input = inputs.frames.recv() => {
                let frame = match input {
                    Some(Input::Frame(frame)) => frame,
                    Some(Input::RequestKeyframe(purpose)) => {
                        if subscribed.iter().any(|track| track.purpose == purpose) {
                            let active = direct.as_mut()
                                .and_then(|direct| direct.leg.as_mut())
                                .filter(|attempt| attempt.transport.selected)
                                .map_or(&mut subscribe, |attempt| &mut attempt.transport);
                            active.request_keyframe(worker, purpose).await?;
                        }
                        continue;
                    }
                    #[cfg(test)]
                    Some(Input::LoseDirect) => {
                        if let Some(attempt) = direct.as_mut().and_then(|direct| direct.leg.as_mut())
                            && let Network::Direct { remote, .. } = &mut attempt.transport.network
                        {
                            *remote = None;
                        }
                        continue;
                    }
                    None => return Ok(()),
                };
                let active = if selected {
                    direct.as_mut().and_then(|direct| direct.leg.as_mut()).map(|attempt| &mut attempt.transport)
                } else {
                    publish.as_mut()
                };
                if frame.revision == source_revision
                    && let (Some(purpose), Some(active), true) = (source, active, sending)
                {
                    if frame.sequence != sequence.wrapping_add(1) {
                        send_gate.awaiting_key = true;
                    }
                    sequence = frame.sequence;
                    if frame.queued.elapsed() <= MAX_PENDING_FRAME_AGE && (!send_gate.awaiting_key || frame.key_frame) {
                        let command = Command::Write { purpose, timestamp: frame.timestamp, key_frame: frame.key_frame };
                        match worker.exchange(command, frame.data).await?.kind {
                            Reply::Done => {
                                send_gate.arrived(frame.key_frame);
                                active.wake = Some(Instant::now());
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
            received = receive(publish.as_mut(), &mut pub_buffer) => {
                if let Some(leg) = &mut publish {
                    let (size, source) = received?;
                    leg.datagram(worker, source, &pub_buffer[..size]).await?;
                }
            }
            received = subscribe.network.receive(&mut sub_buffer) => {
                let (size, source) = received?;
                subscribe.datagram(worker, source, &sub_buffer[..size]).await?;
            }
            received = receive(direct_connected, &mut direct_buffer) => {
                if let Some(attempt) = direct.as_mut().and_then(|direct| direct.leg.as_mut()) {
                    retreat = match received {
                        Ok((size, source)) => attempt.transport.datagram(worker, source, &direct_buffer[..size]).await.is_err(),
                        Err(_) => true,
                    };
                }
            }
            result = direct::next_prepared(&mut discovery) => {
                discovery = None;
                if let (Some(direct), Some(peer)) = (&mut direct, pair.peer) {
                    retreat = match result {
                        Ok(prepared) => match direct.complete(worker, codec, prepared).await {
                            Ok(signal) => direct::send(connection, peer, &signal).await.is_err(),
                            Err(_) => true,
                        },
                        Err(_) => true,
                    };
                }
            }
            () = wake(publish_wake) => {
                if let Some(leg) = &mut publish { drain(worker, leg, None, !selected, &output).await?; }
            }
            () = wake(subscribe_wake) => { drain(worker, &mut subscribe, other.filter(|_| !selected), false, &output).await?; }
            () = wake(direct_wake) => {
                if let Some(attempt) = direct.as_mut().and_then(|direct| direct.leg.as_mut()) {
                    retreat = drain(worker, &mut attempt.transport, other.filter(|_| !publishing), selected && publishing, &output).await.is_err();
                }
            }
            () = wake(direct_deadline) => retreat = true,
            () = wake(source_change_deadline) => return Err(Error::Timeout),
        }
        if offered.is_none()
            && let Some((purpose, revision)) = pending_source.take()
        {
            let leg = publish
                .as_mut()
                .ok_or(Error::Protocol("call has no publisher"))?;
            discovery = None;
            if let (Some(direct), Some(peer)) = (&mut direct, pair.peer)
                && direct.leg.is_some()
                && let Some(id) = direct.close(worker, Some(leg)).await?
            {
                direct::send(connection, peer, &Signal::Close { id }).await?;
                if selected
                    && output
                        .events
                        .send(Ok(QueuedEvent::Call(MediaEvent::Direct { active: false })))
                        .await
                        .is_err()
                {
                    return Ok(());
                }
            }
            let (mid, description) = offer_publish(connection, worker, purpose, revision).await?;
            publish_mid = Some(mid);
            offered = Some(description);
            source_change_deadline = Some(Instant::now() + HANDSHAKE);
            leg.wake = Some(Instant::now());
            source = Some(purpose);
            source_revision = revision;
            desired = None;
            sending = false;
            send_gate = FrameGate::new();
        }
        if !sending
            && offered.is_none()
            && publish.as_ref().is_some_and(|leg| leg.connected)
            && pair.ready
            && publishing
        {
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
            if output
                .events
                .send(Ok(QueuedEvent::SendReady(source_revision)))
                .await
                .is_err()
            {
                return Ok(());
            }
        }

        let (Some(direct), Some(peer)) = (&mut direct, pair.peer) else {
            continue;
        };
        if pair.ready && !publishing && !direct.asked {
            direct.asked = true;
            let _ = direct::send(connection, peer, &Signal::Ready).await;
        }
        if let Some(desired) = desired
            && let Some(attempt) = &direct.leg
            && attempt.transport.selected
        {
            retreat |= attempt
                .transport
                .bandwidth
                .is_some_and(|(_, rate)| rate.saturating_mul(4) / 5 < desired);
        }
        if retreat {
            discovery = None;
            let was_selected = direct
                .leg
                .as_ref()
                .is_some_and(|attempt| attempt.transport.selected);
            if let Some(id) = direct.close(worker, publish.as_mut()).await? {
                if notify {
                    let _ = direct::send(connection, peer, &Signal::Close { id }).await;
                }
                if was_selected {
                    send_gate = FrameGate::new();
                    if publishing {
                        output.bandwidth.send_replace(None);
                        let _ = output.keyframes.try_send(());
                    } else {
                        for purpose in [Purpose::Camera, Purpose::ScreenVideo] {
                            if subscribed.iter().any(|track| track.purpose == purpose)
                                && let Some(gate) = subscribe.video_gate(purpose)
                            {
                                *gate = FrameGate::new();
                                subscribe.request_keyframe(worker, purpose).await?;
                            }
                        }
                    }
                    if output
                        .events
                        .send(Ok(QueuedEvent::Call(MediaEvent::Direct { active: false })))
                        .await
                        .is_err()
                    {
                        return Ok(());
                    }
                }
            }
            continue;
        }
        match (source, publish.as_ref(), desired) {
            (Some(purpose), Some(publish), Some(desired))
                if sending && direct::enough(publish.bandwidth, desired) =>
            {
                if direct.can_offer() && discovery.is_none() {
                    discovery = Some(direct.discover(direct::Setup::Offer(purpose)));
                } else if let Some(attempt) = &mut direct.leg
                    && attempt.transport.connected
                    && attempt.confirmed
                    && !attempt.transport.selected
                {
                    if !matches!(
                        worker
                            .exchange(
                                Command::SetSendLeg {
                                    leg: attempt.transport.id
                                },
                                Vec::new()
                            )
                            .await?
                            .kind,
                        Reply::Done,
                    ) {
                        if let Some(id) = direct.close(worker, None).await? {
                            let _ = direct::send(connection, peer, &Signal::Close { id }).await;
                        }
                        continue;
                    }
                    output.bandwidth.send_replace(None);
                    attempt.transport.selected = true;
                    attempt.transport.wake = Some(Instant::now());
                    send_gate = FrameGate::new();
                    let _ = output.keyframes.try_send(());
                    if output
                        .events
                        .send(Ok(QueuedEvent::Call(MediaEvent::Direct { active: true })))
                        .await
                        .is_err()
                    {
                        return Ok(());
                    }
                }
            }
            (None, _, _) => {
                if let Some(attempt) = &mut direct.leg
                    && attempt.transport.connected
                    && !attempt.confirmed
                {
                    attempt.confirmed = true;
                    if direct::send(connection, peer, &Signal::Connected { id: attempt.id })
                        .await
                        .is_err()
                    {
                        direct.close(worker, None).await?;
                    }
                }
            }
            _ => {}
        }
    }
}

async fn offer_publish(
    connection: &mut Connection,
    worker: &mut Worker,
    purpose: Purpose,
    revision: u8,
) -> Result<(String, Description), Error> {
    let mut id = signaling_id(connection, PUBLISH);
    id[15] = revision;
    let mut description = describe(
        worker,
        Command::Offer {
            leg: PUBLISH,
            id,
            purposes: vec![purpose],
        },
    )
    .await?;
    let [track] = description.tracks.as_slice() else {
        return Err(Error::Protocol("invalid publish track"));
    };
    if track.purpose != purpose || track.publisher.is_some() {
        return Err(Error::Protocol("invalid publish track"));
    }
    let mid = track.mid.clone();
    let screens = if purpose == Purpose::ScreenVideo {
        vec![mid.clone()]
    } else {
        Vec::new()
    };
    let sdp = std::mem::take(&mut description.sdp);
    write(
        &mut connection.socket,
        &ClientMessage::Offer { sdp, screens },
    )
    .await?;
    Ok((mid, description))
}

async fn drain(
    worker: &mut Worker,
    leg: &mut Leg,
    peer: Option<u64>,
    sending: bool,
    output: &Output<'_>,
) -> Result<(), Error> {
    for _ in 0..UDP_BURST {
        let packet = worker
            .exchange(Command::Poll { leg: leg.id }, Vec::new())
            .await?;
        match packet.kind {
            Reply::Wake { after_millis, .. } => {
                leg.wake = Some(Instant::now() + Duration::from_millis(after_millis));
                return Ok(());
            }
            Reply::Datagram { destination, .. } => {
                leg.network.send(&packet.data, destination).await?;
            }
            Reply::Event { event, .. } => match event {
                Event::Connected => leg.connected = true,
                Event::Disconnected => {
                    return Err(Error::Protocol("native media transport disconnected"));
                }
                Event::BandwidthEstimate { bitrate } => {
                    leg.bandwidth = Some((Instant::now(), bitrate));
                    if sending {
                        output.bandwidth.send_replace(Some(bitrate));
                    }
                }
                Event::Keyframe { .. } if sending => {
                    let _ = output.keyframes.try_send(());
                }
                Event::Frame {
                    publisher,
                    purpose,
                    timestamp,
                    contiguous,
                    key_frame,
                } if peer == Some(publisher) => {
                    let upgrading = leg.id > SUBSCRIBE && !leg.selected;
                    let Some(gate) = leg.video_gate(purpose) else {
                        continue;
                    };
                    if (key_frame || (contiguous && !gate.awaiting_key))
                        && output
                            .events
                            .try_send(Ok(QueuedEvent::Call(MediaEvent::Frame {
                                publisher,
                                purpose,
                                timestamp,
                                contiguous: contiguous && !gate.awaiting_key,
                                key_frame,
                                data: packet.data,
                            })))
                            .is_ok()
                    {
                        gate.arrived(key_frame);
                        if upgrading {
                            leg.selected = true;
                            if output
                                .events
                                .send(Ok(QueuedEvent::Call(MediaEvent::Direct { active: true })))
                                .await
                                .is_err()
                            {
                                return Ok(());
                            }
                        }
                    } else {
                        leg.request_keyframe(worker, purpose).await?;
                    }
                }
                _ => {}
            },
            _ => return Err(Error::Protocol("invalid RTC worker output")),
        }
    }
    leg.wake = Some(Instant::now());
    Ok(())
}
