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
    sync::{
        Arc,
        atomic::{AtomicU8, AtomicU64, Ordering},
    },
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

const SUBSCRIBE: u32 = 1;
const PUBLISH: u32 = 2;
const UDP_BURST: usize = 64;
const MAX_PENDING_FRAME_AGE: Duration = Duration::from_millis(150);
const KEYFRAME_INTERVAL: Duration = Duration::from_secs(1);
// A subscription accepts 48 SDP sections, including its control channel and first source.
const MAX_SOURCE_CHANGES: u8 = 46;
const VIDEO: [Purpose; 2] = [Purpose::Camera, Purpose::ScreenVideo];
const IDLE: u8 = 0;
const SENDING: u8 = 1;
const RECEIVING: u8 = 2;

pub struct MediaOptions {
    /// Path of the installed `nuschl-rtc-worker`.
    pub executable: PathBuf,
    /// UDP port on the same authenticated SFU address as WSS (default 3479).
    pub sfu_port: u16,
    /// The initial source to publish, or `None` to start with capture off.
    pub publish: Option<Purpose>,
}

pub enum MediaEvent {
    SourceReady {
        purpose: Purpose,
    },
    BandwidthEstimate {
        bitrate: u64,
    },
    SourceLimits {
        purpose: Purpose,
        height: u16,
        fps: u8,
        bitrate: u64,
    },
    SourceKeyframeRequired {
        purpose: Purpose,
    },
    /// A remote track is available after signaling has accepted its SDP.
    SourceAvailable {
        publisher: u64,
        purpose: Purpose,
    },
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
    SourceReady { index: usize, revision: u8 },
}

#[derive(Clone, Copy, PartialEq, Eq)]
struct EncoderCaps {
    revision: u8,
    height: u16,
    fps: u8,
    bitrate: u64,
}

type SourceCaps = [Option<EncoderCaps>; 2];

#[derive(Clone, Copy, Default)]
struct Sources {
    tracks: [Option<u8>; 2],
    revision: u8,
}

impl Sources {
    fn active(self) -> Vec<Purpose> {
        self.tracks
            .iter()
            .zip(VIDEO)
            .filter_map(|(revision, purpose)| revision.map(|_| purpose))
            .collect()
    }

    fn update(&mut self, tracks: [bool; 2]) -> Result<(), Error> {
        if self.tracks.map(|slot| slot.is_some()) == tracks {
            return Ok(());
        }
        if self.revision == MAX_SOURCE_CHANGES {
            return Err(Error::Protocol("video source limit reached for this call"));
        }
        self.revision += 1;
        for (slot, active) in self.tracks.iter_mut().zip(tracks) {
            *slot = active.then(|| slot.unwrap_or(self.revision));
        }
        Ok(())
    }
}

enum Input {
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
    frames: [mpsc::Sender<Video>; 2],
    sequence: [AtomicU64; 2],
    quality: watch::Sender<QualityRequests>,
    sources: watch::Sender<Sources>,
    role: Arc<AtomicU8>,
    events: mpsc::Receiver<Result<QueuedEvent, Error>>,
    keyframes: [mpsc::Receiver<()>; 2],
    limits: watch::Receiver<SourceCaps>,
    reported_limits: SourceCaps,
    bandwidth: watch::Receiver<Option<u64>>,
    task: JoinHandle<()>,
}

impl Media {
    /// Submit an independent encoded frame on a call with one active source.
    pub fn send_keyframe(&self, timestamp: u64, data: Vec<u8>) -> Result<bool, Error> {
        self.send_frame(timestamp, true, data)
    }

    /// Submit a frame on a call with one active source.
    pub fn send_frame(
        &self,
        timestamp: u64,
        key_frame: bool,
        data: Vec<u8>,
    ) -> Result<bool, Error> {
        let sources = self.sources.borrow().active();
        let [purpose] = sources.as_slice() else {
            return Err(Error::Protocol("select an active video source"));
        };
        self.send_source_frame(*purpose, timestamp, key_frame, data)
    }

    /// Submit a frame on a camera or screen track after its `SourceReady` event.
    /// Each source has a separate one-frame queue and a 90 kHz timestamp clock.
    pub fn send_source_frame(
        &self,
        purpose: Purpose,
        timestamp: u64,
        key_frame: bool,
        data: Vec<u8>,
    ) -> Result<bool, Error> {
        let index = video_index(purpose)?;
        let revision =
            self.sources.borrow().tracks[index].ok_or(Error::Protocol("video source is off"))?;
        if data.is_empty() || data.len() > purpose.frame_limit() {
            return Err(Error::Protocol("invalid encoded frame size"));
        }
        let sequence = self.sequence[index]
            .fetch_add(1, Ordering::Relaxed)
            .wrapping_add(1);
        let frame = Video {
            sequence,
            revision,
            queued: Instant::now(),
            timestamp,
            key_frame,
            data,
        };
        match self.frames[index].try_send(frame) {
            Ok(()) => Ok(true),
            Err(mpsc::error::TrySendError::Full(_)) => Ok(false),
            Err(mpsc::error::TrySendError::Closed(_)) => Err(Error::Protocol("media ended")),
        }
    }

    /// Native calls keep the first publisher direction for the paired meeting.
    pub fn can_publish(&self) -> bool {
        self.role.load(Ordering::Relaxed) != RECEIVING
    }

    /// Change one granted source while keeping the other source and signaling live.
    pub fn set_source(&mut self, purpose: Purpose, active: bool) -> Result<(), Error> {
        let index = video_index(purpose)?;
        let mut tracks = self.sources.borrow().tracks.map(|slot| slot.is_some());
        tracks[index] = active;
        self.set_sources(tracks)
    }

    /// Replace the current source set with one source.
    pub fn switch_source(&mut self, purpose: Purpose) -> Result<(), Error> {
        let index = video_index(purpose)?;
        self.set_sources(std::array::from_fn(|i| i == index))
    }

    fn set_sources(&self, tracks: [bool; 2]) -> Result<(), Error> {
        let mut sources = *self.sources.borrow();
        sources.update(tracks)?;
        if tracks.into_iter().any(|active| active) {
            self.role
                .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |role| {
                    (role != RECEIVING).then_some(SENDING)
                })
                .map_err(|_| Error::Protocol("another participant is presenting"))?;
        }
        self.sources
            .send(sources)
            .map_err(|_| Error::Protocol("media ended"))
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
        if publisher == 0 {
            return Err(Error::Protocol("invalid video subscription"));
        }
        if self.quality.is_closed() {
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
        self.quality.send_if_modified(|latest| {
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
        if video_kind(purpose).is_none() {
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
            let limits = *self.limits.borrow_and_update();
            for (index, caps) in limits.into_iter().enumerate() {
                if self.reported_limits[index] != caps {
                    self.reported_limits[index] = caps;
                    if let Some(EncoderCaps {
                        revision,
                        height,
                        fps,
                        bitrate,
                    }) = caps
                        && Some(revision) == self.sources.borrow().tracks[index]
                    {
                        return Some(Ok(MediaEvent::SourceLimits {
                            purpose: VIDEO[index],
                            height,
                            fps,
                            bitrate,
                        }));
                    }
                }
            }
            let event = tokio::select! {
                biased;
                event = self.events.recv() => event,
                (index, requested) = next_source(&mut self.keyframes) => {
                    if requested.is_some() {
                        return Some(Ok(MediaEvent::SourceKeyframeRequired { purpose: VIDEO[index] }));
                    }
                    self.events.recv().await
                },
                changed = self.limits.changed() => {
                    if changed.is_err() {
                        self.events.recv().await
                    } else {
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
                Ok(QueuedEvent::SourceReady { index, revision })
                    if Some(revision) == self.sources.borrow().tracks[index] =>
                {
                    return Some(Ok(MediaEvent::SourceReady {
                        purpose: VIDEO[index],
                    }));
                }
                Err(error) => return Some(Err(error)),
                _ => {}
            }
        }
    }

    pub async fn stop(self) -> Result<(), Error> {
        drop(self.input);
        drop(self.frames);
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
        let role = Arc::new(AtomicU8::new(if options.publish.is_some() {
            SENDING
        } else {
            IDLE
        }));
        let actor_role = Arc::clone(&role);
        let initial = Sources {
            tracks: VIDEO.map(|purpose| (options.publish == Some(purpose)).then_some(0)),
            revision: 0,
        };
        let (input, control) = mpsc::channel(1);
        let ([camera, screen], frames) = frame_channels();
        let (quality, wanted) = watch::channel([None; 2]);
        let (sources, changes) = watch::channel(initial);
        let (output, events) = mpsc::channel(4);
        let (requests, keyframes) = frame_channels();
        let (encoder_caps, limits) = watch::channel([None; 2]);
        let (upload_estimate, bandwidth) = watch::channel(None);
        let task = tokio::spawn(async move {
            let mut connection = self;
            let result = run(
                &mut connection,
                &mut worker,
                destination,
                codec,
                &actor_role,
                Inputs {
                    control,
                    frames,
                    changes,
                    wanted,
                },
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
            frames: [camera, screen],
            sequence: Default::default(),
            quality,
            sources,
            role,
            events,
            keyframes,
            limits,
            reported_limits: [None; 2],
            bandwidth,
            task,
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
}

struct Output<'a> {
    events: &'a mpsc::Sender<Result<QueuedEvent, Error>>,
    keyframes: &'a [mpsc::Sender<()>; 2],
    // Feedback updates carry the latest value without holding up media I/O.
    limits: &'a watch::Sender<SourceCaps>,
    bandwidth: &'a watch::Sender<Option<u64>>,
}

impl Output<'_> {
    fn keyframe(&self, index: usize) {
        let _ = self.keyframes[index].try_send(());
    }

    async fn call(&self, event: MediaEvent) -> bool {
        self.events.send(Ok(QueuedEvent::Call(event))).await.is_ok()
    }
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
    async fn connect(
        &mut self,
        connection: &mut Connection,
        peer: u64,
        publisher: bool,
    ) -> Result<(), Error> {
        if peer == 0 || peer == connection.welcome.peer || self.peer.is_some() {
            return Err(Error::Protocol("invalid native call peer"));
        }
        connection.signal(Action::Pair { peer, publisher }).await?;
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

struct Inputs {
    control: mpsc::Receiver<Input>,
    frames: [mpsc::Receiver<Video>; 2],
    changes: watch::Receiver<Sources>,
    wanted: watch::Receiver<QualityRequests>,
}

fn frame_channels<T>() -> ([mpsc::Sender<T>; 2], [mpsc::Receiver<T>; 2]) {
    let (camera, camera_in) = mpsc::channel(1);
    let (screen, screen_in) = mpsc::channel(1);
    ([camera, screen], [camera_in, screen_in])
}

async fn next_source<T>(channels: &mut [mpsc::Receiver<T>; 2]) -> (usize, Option<T>) {
    let [camera, screen] = channels;
    tokio::select! {
        frame = camera.recv() => (0, frame),
        frame = screen.recv() => (1, frame),
    }
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

fn video_index(purpose: Purpose) -> Result<usize, Error> {
    video_kind(purpose)
        .map(quality_index)
        .ok_or(Error::Protocol("invalid video source"))
}

async fn run(
    connection: &mut Connection,
    worker: &mut Worker,
    destination: SocketAddr,
    codec: rtc::Codec,
    role: &AtomicU8,
    mut inputs: Inputs,
    output: Output<'_>,
) -> Result<(), Error> {
    let mut pair = Pair {
        peer: None,
        ready: false,
    };
    let mut peers: Vec<_> = connection
        .welcome
        .peers
        .iter()
        .map(|peer| peer.id)
        .collect();
    let mut other = peers.first().copied();
    let mut offered: Option<Description> = None;
    let mut publish_tracks = Vec::<Track>::new();
    let mut source_change_deadline = None;
    let mut pending_source = Some(*inputs.changes.borrow_and_update());
    let mut source = Sources::default();
    let turn = connection
        .welcome
        .turn
        .clone()
        .filter(|_| connection.policy.relay_only);
    let subscribe_path = NetworkLeg::open(destination, turn.as_ref()).await?;
    let mut publish: Option<Leg> = None;
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
    let mut desired = [None::<u64>; 2];
    let mut direct_buffer = [0; rtc::MAX_DATAGRAM + 1];
    let mut subscribed = Vec::<Track>::new();
    let mut quality = ReceiveQuality::default();
    let mut pub_buffer = [0; rtc::MAX_DATAGRAM + 1];
    let mut sub_buffer = [0; rtc::MAX_DATAGRAM + 1];
    let mut sending = [None; 2];
    let mut send_gates = std::array::from_fn::<_, 2, _>(|_| FrameGate::new());
    let mut sequence = [0_u64; 2];
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
        let publishing = role.load(Ordering::Relaxed) == SENDING;
        tokio::select! {
            message = connection.next() => match message? {
                ServerMessage::Joined { peer, .. } => {
                    if peer == 0 || peer == connection.welcome.peer {
                        return Err(Error::Protocol("invalid call peer"));
                    }
                    if !peers.contains(&peer) {
                        if peers.len() >= 256 { return Err(Error::Protocol("invalid call roster")); }
                        peers.push(peer);
                    }
                    if other.is_none() {
                        other = Some(peer);
                        if publishing { pair.connect(connection, peer, true).await?; }
                    }
                }
                ServerMessage::Left { peer } => {
                    peers.retain(|candidate| *candidate != peer);
                    if other == Some(peer) {
                        if pair.peer.is_some() { return Ok(()); }
                        other = peers.first().copied();
                    }
                }
                ServerMessage::Offer { sdp, tracks } => {
                    if sdp.len() > rtc::MAX_SDP || tracks.len() > 16 { return Err(Error::Protocol("invalid native SFU offer")); }
                    let tracks = tracks.into_iter().map(|track| {
                        let purpose = match track.kind.as_ref() {
                            "camera" => Purpose::Camera,
                            "screen" => Purpose::ScreenVideo,
                            "audio" => Purpose::SystemAudio,
                            _ => return Err(Error::Protocol("invalid media source")),
                        };
                        if track.peer == 0 || !peers.contains(&track.peer) {
                            return Err(Error::Protocol("invalid media publisher"));
                        }
                        Ok(Track { mid: track.mid, purpose, publisher: Some(track.peer) })
                    }).collect::<Result<Vec<_>, _>>()?;
                    if pair.peer.is_none()
                        && let Some(peer) = tracks.first().and_then(|track| track.publisher)
                        && role.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |role| {
                            (role != SENDING).then_some(RECEIVING)
                        }).is_ok()
                    {
                        pair.connect(connection, peer, false).await?;
                        other = Some(peer);
                    }
                    let selected = if role.load(Ordering::Relaxed) == RECEIVING {
                        pair.peer
                    } else {
                        None
                    };
                    let mut seen = [false; 2];
                    let next = tracks.iter().filter_map(|track| {
                        let index = video_index(track.purpose).ok()?;
                        (track.publisher == selected && !std::mem::replace(&mut seen[index], true))
                            .then(|| track.clone())
                    }).collect::<Vec<_>>();
                    let description = Description { id: signaling_id(connection, SUBSCRIBE), sdp, tracks };
                    let answer = describe(worker, Command::AcceptOffer { leg: SUBSCRIBE, description }).await?;
                    write(&mut connection.socket, &ClientMessage::Answer { sdp: answer.sdp }).await?;
                    // End old sources after queued frames and before any media from the new offer.
                    for track in subscribed.iter().filter(|track| !next.contains(track)) {
                        if video_kind(track.purpose).is_none() || publishing { continue; }
                        retreat |= attempting_direct;
                        quality.source_ended(track.purpose);
                        if let Some(gate) = subscribe.video_gate(track.purpose) {
                            *gate = FrameGate::new();
                        }
                        if output.events.send(Ok(QueuedEvent::Call(MediaEvent::SourceEnded { purpose: track.purpose }))).await.is_err() {
                            return Ok(());
                        }
                    }
                    for track in next.iter().filter(|track| !subscribed.contains(track)) {
                        if let Some(publisher) = track.publisher
                            && video_kind(track.purpose).is_some()
                            && !publishing
                            && !output.call(MediaEvent::SourceAvailable { publisher, purpose: track.purpose }).await
                        {
                            return Ok(());
                        }
                    }
                    subscribed = next;
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
                ServerMessage::Quality { source: mid, max_height, max_fps, max_bitrate, .. }
                    if (16..=2160).contains(&max_height) && (1..=120).contains(&max_fps) && (64_000..=50_000_000).contains(&max_bitrate) => {
                    if let Some(track) = publish_tracks.iter().find(|track| track.mid == mid)
                        && let Ok(index) = video_index(track.purpose)
                        && let Some(revision) = source.tracks[index]
                    {
                        desired[index] = Some(max_bitrate);
                        output.limits.send_modify(|limits| limits[index] = Some(EncoderCaps {
                            revision, height: max_height, fps: max_fps, bitrate: max_bitrate,
                        }));
                    }
                }
                ServerMessage::Codec { codec: changed } if changed != connection.welcome.codec => return Err(Error::Protocol("native call codec changed")),
                ServerMessage::Error { reason } => return Err(Error::Refused(reason.into_owned())),
                _ => {}
            },
            changed = inputs.changes.changed() => {
                if changed.is_err() { return Ok(()); }
                pending_source = Some(*inputs.changes.borrow_and_update());
            }
            changed = inputs.wanted.changed() => {
                if changed.is_err() { return Ok(()); }
                quality.wanted = *inputs.wanted.borrow_and_update();
            }
            () = wake(next_quality.map(|(at, _)| at)) => {
                quality.wanted = *inputs.wanted.borrow_and_update();
                if let Some((at, request)) = quality.next_request(&subscribed)
                    && at <= Instant::now()
                {
                    write(&mut connection.socket, &ClientMessage::Quality {
                        request: hex::encode(request.to_bytes()),
                    }).await?;
                    quality.sent(request);
                }
            }
            input = inputs.control.recv() => match input {
                Some(Input::RequestKeyframe(purpose)) => {
                    if subscribed.iter().any(|track| track.purpose == purpose) {
                        let active = direct.as_mut()
                            .and_then(|direct| direct.leg.as_mut())
                            .filter(|attempt| attempt.transport.selected)
                            .map_or(&mut subscribe, |attempt| &mut attempt.transport);
                        active.request_keyframe(worker, purpose).await?;
                    }
                }
                #[cfg(test)]
                Some(Input::LoseDirect) => {
                    if let Some(attempt) = direct.as_mut().and_then(|direct| direct.leg.as_mut())
                        && let Network::Direct { remote, .. } = &mut attempt.transport.network
                    {
                        *remote = None;
                    }
                }
                None => return Ok(()),
            },
            (index, frame) = next_source(&mut inputs.frames) => {
                let Some(frame) = frame else { return Ok(()); };
                let active = if selected {
                    direct.as_mut().and_then(|direct| direct.leg.as_mut()).map(|attempt| &mut attempt.transport)
                } else {
                    publish.as_mut()
                };
                let allowed = Some(frame.revision) == inputs.changes.borrow().tracks[index];
                if allowed && Some(frame.revision) == sending[index] && let Some(active) = active {
                    let gate = &mut send_gates[index];
                    if frame.sequence != sequence[index].wrapping_add(1) {
                        gate.awaiting_key = true;
                    }
                    sequence[index] = frame.sequence;
                    if frame.queued.elapsed() <= MAX_PENDING_FRAME_AGE && (!gate.awaiting_key || frame.key_frame) {
                        let command = Command::Write { purpose: VIDEO[index], timestamp: frame.timestamp, key_frame: frame.key_frame };
                        match worker.exchange(command, frame.data).await?.kind {
                            Reply::Done => {
                                gate.arrived(frame.key_frame);
                                active.wake = Some(Instant::now());
                                continue;
                            }
                            Reply::Rejected => {}
                            _ => return Err(Error::Protocol("invalid RTC write reply")),
                        }
                    }
                    gate.awaiting_key = true;
                    if gate.request() {
                        output.keyframe(index);
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
            () = wake(subscribe_wake) => { drain(worker, &mut subscribe, other.filter(|_| !selected && !publishing), false, &output).await?; }
            () = wake(direct_wake) => {
                if let Some(attempt) = direct.as_mut().and_then(|direct| direct.leg.as_mut()) {
                    retreat = drain(worker, &mut attempt.transport, other.filter(|_| !publishing), selected && publishing, &output).await.is_err();
                }
            }
            () = wake(direct_deadline) => retreat = true,
            () = wake(source_change_deadline) => return Err(Error::Timeout),
            () = std::future::ready(()), if offered.is_none() && pending_source.is_some() => {}
        }
        if offered.is_none()
            && let Some(next) = pending_source.take()
            && source.tracks != next.tracks
        {
            if pair.peer.is_none()
                && let Some(peer) = other
            {
                pair.connect(connection, peer, true).await?;
            }
            let update = publish.is_some();
            if publish.is_none() {
                let path = NetworkLeg::open(destination, turn.as_ref()).await?;
                publish = Some(
                    Leg::start(
                        worker,
                        PUBLISH,
                        Network::Sfu(Box::new(path)),
                        codec,
                        LegRole::SfuPublish,
                    )
                    .await?,
                );
            }
            let leg = publish
                .as_mut()
                .ok_or(Error::Protocol("call has no publisher"))?;
            discovery = None;
            if let (Some(direct), Some(peer)) = (&mut direct, pair.peer)
                && direct.leg.is_some()
                && let Some(id) = direct.close(worker, Some(leg)).await?
            {
                direct::send(connection, peer, &Signal::Close { id }).await?;
                if selected {
                    output.bandwidth.send_replace(None);
                    for (index, gate) in send_gates.iter_mut().enumerate() {
                        *gate = FrameGate::new();
                        if sending[index].is_some() {
                            output.keyframe(index);
                        }
                    }
                    if !output.call(MediaEvent::Direct { active: false }).await {
                        return Ok(());
                    }
                }
            }
            let description = offer_publish(connection, worker, source, next, update).await?;
            publish_tracks.clone_from(&description.tracks);
            offered = Some(description);
            source_change_deadline = Some(Instant::now() + HANDSHAKE);
            leg.wake = Some(Instant::now());
            for index in 0..VIDEO.len() {
                if source.tracks[index] != next.tracks[index] {
                    desired[index] = None;
                    sending[index] = None;
                    send_gates[index] = FrameGate::new();
                    output.limits.send_modify(|limits| limits[index] = None);
                }
            }
            source = next;
        }
        if offered.is_none()
            && source.tracks != sending
            && publish.as_ref().is_some_and(|leg| leg.connected)
            && pair.ready
        {
            // The crypto worker delivers the epoch on a separate private socket.
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
            for (index, revision) in source.tracks.into_iter().enumerate() {
                if let Some(revision) = revision
                    && sending[index] != Some(revision)
                    && output
                        .events
                        .send(Ok(QueuedEvent::SourceReady { index, revision }))
                        .await
                        .is_err()
                {
                    return Ok(());
                }
            }
            sending = source.tracks;
        }

        let (Some(direct), Some(peer)) = (&mut direct, pair.peer) else {
            continue;
        };
        if pair.ready && !publishing && !direct.asked {
            direct.asked = true;
            let _ = direct::send(connection, peer, &Signal::Ready).await;
        }
        let desired = source
            .tracks
            .iter()
            .zip(desired)
            .try_fold(0_u64, |sum, (active, desired)| {
                if active.is_some() {
                    desired.map(|desired| sum + desired)
                } else {
                    Some(sum)
                }
            })
            .filter(|desired| *desired != 0);
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
                    if publishing {
                        output.bandwidth.send_replace(None);
                        for (index, gate) in send_gates.iter_mut().enumerate() {
                            *gate = FrameGate::new();
                            if sending[index].is_some() {
                                output.keyframe(index);
                            }
                        }
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
        match (publishing, publish.as_ref(), desired) {
            (true, Some(publish), Some(desired))
                if sending.iter().any(Option::is_some)
                    && direct::enough(publish.bandwidth, desired) =>
            {
                if direct.can_offer() && discovery.is_none() {
                    discovery = Some(direct.discover(direct::Setup::Offer(source.active())));
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
                    for (index, gate) in send_gates.iter_mut().enumerate() {
                        *gate = FrameGate::new();
                        if sending[index].is_some() {
                            output.keyframe(index);
                        }
                    }
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
            (false, _, _) => {
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
    previous: Sources,
    next: Sources,
    update: bool,
) -> Result<Description, Error> {
    let mut id = signaling_id(connection, PUBLISH);
    id[15] = next.revision;
    let purposes = next.active();
    let command = if update {
        let restart = VIDEO
            .into_iter()
            .enumerate()
            .filter_map(|(index, purpose)| {
                (previous.tracks[index].is_some()
                    && next.tracks[index].is_some()
                    && previous.tracks[index] != next.tracks[index])
                    .then_some(purpose)
            })
            .collect();
        Command::UpdatePublish {
            leg: PUBLISH,
            id,
            purposes: purposes.clone(),
            restart,
        }
    } else {
        Command::Offer {
            leg: PUBLISH,
            id,
            purposes: purposes.clone(),
        }
    };
    let mut description = describe(worker, command).await?;
    if description.tracks.len() != purposes.len()
        || purposes.iter().any(|purpose| {
            !description
                .tracks
                .iter()
                .any(|track| track.purpose == *purpose && track.publisher.is_none())
        })
    {
        return Err(Error::Protocol("invalid publish tracks"));
    }
    let screens = description
        .tracks
        .iter()
        .filter(|track| track.purpose == Purpose::ScreenVideo)
        .map(|track| track.mid.clone())
        .collect();
    let sdp = std::mem::take(&mut description.sdp);
    write(
        &mut connection.socket,
        &ClientMessage::Offer { sdp, screens },
    )
    .await?;
    Ok(description)
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
                Event::Keyframe { purpose } if sending => {
                    if let Ok(index) = video_index(purpose) {
                        output.keyframe(index);
                    }
                }
                Event::Frame {
                    publisher,
                    purpose,
                    timestamp,
                    contiguous,
                    key_frame,
                } if peer == Some(publisher) => {
                    let upgrading = leg.id > PUBLISH && !leg.selected;
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
