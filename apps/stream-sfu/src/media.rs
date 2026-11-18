// SPDX-License-Identifier: EUPL-1.2
//! The core: one task owns every room, every peer's WebRTC state (str0m in RTP
//! mode) and the one UDP socket. It forwards RTP packets from a publisher to
//! each subscriber with their payload, timestamp and marker unchanged, in the
//! codec the publisher negotiated (AV1, VP9 or Opus); it never decrypts,
//! depacketizes or transcodes. A source whose packets carry the Dependency
//! Descriptor reaches each receiver in the decode target chosen for it
//! (`quality`). Each subscriber gets its own sequence numbers from zero: frames
//! it skips leave no gap, and packets a budget drops leave one it sees.

use crate::{
    config::Config,
    protocol::{self, ClientMessage, PeerInfo, ServerMessage, Track, TurnCredentials},
    quality,
    rooms::{self, Bucket, Join, Nonce, PeerId, Refusal, RoomId, Rooms},
    routing::Routing,
    traffic::Traffic,
};
use nuschl_stream_quality::{
    DecodeCapacity, Kind, QualityRequest, Throttle, adapt::Stepper, budget, dd::Choice,
};
use std::{
    cmp::Reverse,
    collections::{BinaryHeap, HashMap},
    net::{IpAddr, SocketAddr},
    sync::Arc,
    time::{Duration, Instant, SystemTime},
};
use str0m::{
    Candidate, Event, IceConnectionState, Input, Output, Rtc,
    bwe::{Bitrate, BweKind},
    change::{SdpAnswer, SdpOffer, SdpPendingOffer},
    media::{Direction, KeyframeRequestKind, MediaKind, Mid, Rid},
    net::{Protocol, Receive},
    rtp::{RtpPacket, RtpWrite},
};
use tokio::{
    net::UdpSocket,
    sync::{mpsc, oneshot},
};

/// One keyframe request per source in this interval reaches its publisher.
const KEYFRAME_INTERVAL: Duration = Duration::from_secs(2);
/// Per-packet overhead counted against media budgets (IP, UDP, SRTP).
const PACKET_OVERHEAD: usize = 48;
/// The largest UDP datagram read; longer ones are dropped.
pub const MAX_DATAGRAM: usize = 2048;
/// A subscribe connection's bandwidth estimate before its first measurement.
const INITIAL_ESTIMATE: u64 = 2_000_000;

pub enum Command {
    Join {
        id: PeerId,
        client: IpAddr,
        nonce: Nonce,
        join: Join,
        out: mpsc::Sender<ServerMessage>,
    },
    Message {
        id: PeerId,
        message: ClientMessage,
    },
    Leave {
        id: PeerId,
    },
    /// The creator's device renews the lease of `room` or ends it; the reply is
    /// the HTTP status.
    Lease {
        room: RoomId,
        host_key: rooms::Key,
        end: bool,
        reply: oneshot::Sender<u16>,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
enum Leg {
    Publish,
    Subscribe,
}

struct Source {
    mid: Mid,
    kind: MediaKind,
    /// Video sources only.
    video: Option<quality::Video>,
    /// The simulcast layer forwarded, fixed by the first packet.
    layer: Option<Option<Rid>>,
    keyframe_asked: Option<Instant>,
}

struct Forward {
    from: PeerId,
    source: Mid,
    kind: MediaKind,
    /// `camera`, `screen` or `audio`.
    label: &'static str,
    /// The receiver-side mid, once offered.
    mid: Option<Mid>,
    open: bool,
    stopping: bool,
    view: quality::View,
}

struct Peer {
    room: RoomId,
    out: mpsc::Sender<ServerMessage>,
    publish: Option<Rtc>,
    subscribe: Rtc,
    pending: Option<SdpPendingOffer>,
    sources: Vec<Source>,
    forwards: Vec<Forward>,
    budget: Bucket,
    deadlines: [Option<Instant>; 2],
    /// The video mids of its publish connection that carry a screen.
    screens: Vec<Mid>,
    /// The subscribe connection's bandwidth estimate, bits per second.
    estimate: u64,
    decode: DecodeCapacity,
    decode_throttle: Throttle,
}

impl Source {
    fn label(&self) -> &'static str {
        match &self.video {
            None => "audio",
            Some(video) if video.kind == Kind::Screen => "screen",
            Some(_) => "camera",
        }
    }
}

impl Peer {
    fn rtc(&mut self, leg: Leg) -> Option<&mut Rtc> {
        match leg {
            Leg::Publish => self.publish.as_mut(),
            Leg::Subscribe => Some(&mut self.subscribe),
        }
    }
}

pub struct Turn {
    pub urls: Vec<String>,
    pub secret: Vec<u8>,
    pub lifetime: Duration,
}

pub struct Core {
    config: Arc<Config>,
    socket: Arc<UdpSocket>,
    /// Host candidates: the media addresses with the bound port.
    local: Vec<SocketAddr>,
    crypto: Arc<str0m::crypto::CryptoProvider>,
    turn: Option<Turn>,
    rooms: Rooms,
    peers: HashMap<PeerId, Peer>,
    routing: Routing<(PeerId, Leg)>,
    timers: BinaryHeap<Reverse<(Instant, PeerId, u8)>>,
    dirty: Vec<(PeerId, Leg)>,
    kicks: Vec<(PeerId, &'static str)>,
    /// The origin of the quality logic's time.
    origin: Instant,
    traffic: Option<Traffic>,
    /// How many tiers the traffic budget lowers every receiver's caps.
    step: u64,
}

fn canonical(address: SocketAddr) -> SocketAddr {
    match address.ip() {
        IpAddr::V6(v6) => v6.to_ipv4_mapped().map_or(address, |v4| {
            SocketAddr::new(IpAddr::V4(v4), address.port())
        }),
        IpAddr::V4(_) => address,
    }
}

impl Core {
    pub fn new(
        config: Arc<Config>,
        socket: Arc<UdpSocket>,
        turn: Option<Turn>,
        traffic: Option<Traffic>,
    ) -> std::io::Result<Self> {
        let port = socket.local_addr()?.port();
        let local = config
            .media_addresses
            .iter()
            .map(|ip| SocketAddr::new(*ip, port))
            .collect();
        Ok(Self {
            rooms: Rooms::new(rooms::Limits {
                rooms: config.max_rooms,
                peers_per_room: config.max_peers_per_room,
                lifetime: Duration::from_secs(config.room_lifetime_secs),
                lease: Duration::from_secs(config.lease_secs),
                creations_per_hour: config.creations_per_hour,
                room_bitrate: config.room_bitrate,
            }),
            config,
            socket,
            local,
            crypto: Arc::new(str0m::crypto::from_feature_flags()),
            turn,
            peers: HashMap::new(),
            routing: Routing::default(),
            timers: BinaryHeap::new(),
            dirty: Vec::new(),
            kicks: Vec::new(),
            origin: Instant::now(),
            traffic,
            step: 0,
        })
    }

    pub async fn run(mut self, mut commands: mpsc::Receiver<Command>) {
        let socket = self.socket.clone();
        let mut buffer = vec![0; MAX_DATAGRAM];
        let mut expiry = tokio::time::interval(Duration::from_secs(10));
        let mut quality = tokio::time::interval(Throttle::INTERVAL);
        loop {
            let next = self.timers.peek().map_or_else(
                || Instant::now() + Duration::from_secs(1),
                |Reverse((at, ..))| *at,
            );
            tokio::select! {
                command = commands.recv() => match command {
                    Some(command) => self.command(command, Instant::now()),
                    None => return,
                },
                received = socket.recv_from(&mut buffer) => {
                    if let Ok((length, source)) = received
                        && length < buffer.len()
                    {
                        self.datagram(canonical(source), &buffer[..length], Instant::now());
                    }
                }
                _ = tokio::time::sleep_until(next.into()) => self.timeouts(Instant::now()),
                _ = quality.tick() => self.quality(Instant::now()),
                _ = expiry.tick() => {
                    self.routing.expire(Instant::now());
                    for id in self.rooms.expire(Instant::now()) {
                        self.kicks.push((id, "ended"));
                    }
                }
            }
            self.flush(Instant::now());
        }
    }

    fn command(&mut self, command: Command, now: Instant) {
        match command {
            Command::Join {
                id,
                client,
                nonce,
                join,
                out,
            } => self.join(id, client, &nonce, join, out, now),
            Command::Leave { id } => self.remove(id, None),
            Command::Lease {
                room,
                host_key,
                end,
                reply,
            } => {
                let status = match self.rooms.lease(&room, &host_key, end, now) {
                    Ok(peers) => {
                        for peer in peers {
                            self.kicks.push((peer, "ended"));
                        }
                        204
                    }
                    Err(Refusal::NoRoom) => 404,
                    Err(Refusal::Rate) => 429,
                    Err(_) => 403,
                };
                let _ = reply.send(status);
            }
            Command::Message { id, message } => {
                if let Err(reason) = self.message(id, message, now) {
                    self.kicks.push((id, reason));
                }
            }
        }
    }

    /// A connection for `route`, with its ufrag in the routing map.
    fn new_rtc(&mut self, route: (PeerId, Leg), now: Instant) -> Rtc {
        let mut builder = Rtc::builder();
        if route.1 == Leg::Subscribe {
            builder = builder.enable_bwe(Some(Bitrate::bps(INITIAL_ESTIMATE)));
        }
        let mut rtc = builder
            .set_crypto_provider(self.crypto.clone())
            .set_rtp_mode(true)
            .set_ice_lite(true)
            // Royalty-free codecs only: a call uses AV1 or VP9, and Opus for audio.
            .clear_codecs()
            .enable_av1(true)
            .enable_vp9(true)
            .enable_opus(true, false)
            .set_extension(quality::DESCRIPTOR_ID, quality::descriptor_extension())
            .build(now);
        for address in &self.local {
            if let Ok(candidate) = Candidate::host(*address, "udp") {
                rtc.add_local_candidate(candidate);
            }
        }
        let ufrag = rtc.direct_api().local_ice_credentials().ufrag;
        self.routing.add_ufrag(ufrag, route);
        rtc
    }

    fn join(
        &mut self,
        id: PeerId,
        client: IpAddr,
        nonce: &Nonce,
        join: Join,
        out: mpsc::Sender<ServerMessage>,
        now: Instant,
    ) {
        if self.traffic.as_ref().is_some_and(Traffic::exhausted) {
            let _ = out.try_send(ServerMessage::Error { reason: "traffic" });
            return;
        }
        let role = match self.rooms.join(id, &join, nonce, client, now) {
            Ok(role) => role,
            Err(refusal) => {
                let _ = out.try_send(ServerMessage::Error {
                    reason: refusal.reason(),
                });
                return;
            }
        };
        let room = self.rooms.get(&join.room).expect("joined");
        let others: Vec<_> = room
            .peers
            .iter()
            .filter(|(other, _)| *other != id)
            .map(|&(id, role)| PeerInfo { id, role })
            .collect();
        let turn = self.turn.as_ref().map(|turn| {
            let (username, credential) =
                rooms::turn_credentials(&turn.secret, turn.lifetime, SystemTime::now());
            TurnCredentials {
                urls: turn.urls.clone(),
                username,
                credential,
            }
        });
        let mut forwards = Vec::new();
        for other in &others {
            for source in &self.peers[&other.id].sources {
                forwards.push(forward(other.id, source.mid, source.kind, source.label()));
            }
        }
        let peer = Peer {
            room: join.room,
            out,
            publish: None,
            subscribe: self.new_rtc((id, Leg::Subscribe), now),
            pending: None,
            sources: Vec::new(),
            forwards,
            budget: rooms::media_budget(self.config.receiver_bitrate, now),
            deadlines: [None; 2],
            screens: Vec::new(),
            estimate: INITIAL_ESTIMATE,
            decode: quality::full_capacity(),
            decode_throttle: Throttle::default(),
        };
        self.peers.insert(id, peer);
        // A join fails when some stream's base layer no longer fits.
        let planned = self.plan().0;
        let rates = rates(&planned);
        let (receivers, streams) = self.streams(&planned, &rates);
        if !budget::admits(&self.limits(), &receivers, &streams) {
            let peer = self.peers.remove(&id).expect("inserted");
            self.rooms.leave(&peer.room, id);
            self.routing.remove(|(route, _)| route == id);
            let _ = peer.out.try_send(ServerMessage::Error {
                reason: "bandwidth",
            });
            return;
        }
        self.send(
            id,
            ServerMessage::Welcome {
                peer: id,
                role,
                peers: others.clone(),
                turn,
            },
        );
        for other in others {
            self.send(other.id, ServerMessage::Joined { peer: id, role });
        }
        self.negotiate(id);
    }

    fn message(
        &mut self,
        id: PeerId,
        message: ClientMessage,
        now: Instant,
    ) -> Result<(), &'static str> {
        let room = self.peers.get(&id).ok_or("unknown")?.room;
        match message {
            ClientMessage::Join { .. } => return Err("joined"),
            ClientMessage::Offer { .. }
                if self.traffic.as_ref().is_some_and(Traffic::exhausted) =>
            {
                return Err("traffic");
            }
            ClientMessage::Offer { sdp, screens } => {
                let offer = SdpOffer::from_sdp_string(&sdp).map_err(|_| "sdp")?;
                let limit = self.config.max_sources_per_peer;
                self.peers.get_mut(&id).expect("exists").screens = screens
                    .iter()
                    .take(limit)
                    .map(|mid| Mid::from(mid.as_str()))
                    .collect();
                if self.peers[&id].publish.is_none() {
                    let rtc = self.new_rtc((id, Leg::Publish), now);
                    self.peers.get_mut(&id).expect("exists").publish = Some(rtc);
                }
                let rtc = self
                    .peers
                    .get_mut(&id)
                    .and_then(|p| p.publish.as_mut())
                    .expect("exists");
                let answer = rtc.sdp_api().accept_offer(offer).map_err(|_| "sdp")?;
                self.send(
                    id,
                    ServerMessage::Answer {
                        sdp: answer.to_sdp_string(),
                    },
                );
                self.dirty.push((id, Leg::Publish));
            }
            ClientMessage::Answer { sdp } => {
                let answer = SdpAnswer::from_sdp_string(&sdp).map_err(|_| "sdp")?;
                let peer = self.peers.get_mut(&id).expect("exists");
                let pending = peer.pending.take().ok_or("unexpected answer")?;
                peer.subscribe
                    .sdp_api()
                    .accept_answer(pending, answer)
                    .map_err(|_| "sdp")?;
                let mut opened = Vec::new();
                for forward in peer
                    .forwards
                    .iter_mut()
                    .filter(|f| f.mid.is_some() && !f.open)
                {
                    forward.open = true;
                    if !forward.stopping {
                        opened.push((forward.from, forward.source));
                    }
                }
                for (from, source) in opened {
                    self.keyframe(from, source, now);
                }
                self.dirty.push((id, Leg::Subscribe));
                self.negotiate(id);
            }
            ClientMessage::Quality { request } => {
                let request = protocol::from_hex::<{ QualityRequest::SIZE }>(&request)
                    .and_then(|bytes| QualityRequest::from_bytes(&bytes))
                    .ok_or("quality")?;
                let since = now.saturating_duration_since(self.origin);
                let publisher = self.peers.get(&request.publisher);
                let sources: Vec<Mid> = publisher
                    .map(|p| {
                        p.sources
                            .iter()
                            .filter(|s| s.video.as_ref().is_some_and(|v| v.kind == request.kind))
                            .map(|s| s.mid)
                            .collect()
                    })
                    .unwrap_or_default();
                let peer = self.peers.get_mut(&id).expect("exists");
                for forward in peer
                    .forwards
                    .iter_mut()
                    .filter(|f| f.from == request.publisher && sources.contains(&f.source))
                {
                    // A sooner request is dropped; the receiver sends its latest later.
                    if forward.view.throttle.allow(since) {
                        forward.view.request = Some(usize::from(request.tier));
                    }
                }
            }
            ClientMessage::Capacity { decode } => {
                let decode = protocol::from_hex::<{ DecodeCapacity::SIZE }>(&decode)
                    .and_then(|bytes| DecodeCapacity::from_bytes(&bytes))
                    .ok_or("capacity")?;
                let since = now.saturating_duration_since(self.origin);
                let peer = self.peers.get_mut(&id).expect("exists");
                if peer.decode_throttle.allow(since) {
                    peer.decode = decode;
                }
            }
            ClientMessage::Key { to, data } => {
                if !protocol::valid_key_message(&data) {
                    return Err("key");
                }
                if to != id && self.peers.get(&to).is_some_and(|peer| peer.room == room) {
                    self.send(to, ServerMessage::Key { from: id, data });
                }
            }
            ClientMessage::Remove { peer } => {
                if !self.rooms.is_host(&room, id) {
                    return Err("not_allowed");
                }
                if peer != id && self.peers.get(&peer).is_some_and(|p| p.room == room) {
                    self.kicks.push((peer, "removed"));
                }
            }
            ClientMessage::End => {
                let peers = self.rooms.end(&room, id).map_err(|r| r.reason())?;
                for peer in peers {
                    self.kicks.push((peer, "ended"));
                }
            }
        }
        Ok(())
    }

    /// Offers pending changes on `id`'s subscribe connection, one offer at a time.
    fn negotiate(&mut self, id: PeerId) {
        let Some(peer) = self.peers.get_mut(&id) else {
            return;
        };
        if peer.pending.is_some() {
            return;
        }
        // Without a pending offer, every forward with a mid is open.
        let mut api = peer.subscribe.sdp_api();
        peer.forwards
            .retain_mut(|forward| match (forward.mid, forward.stopping) {
                (None, true) => false,
                (Some(mid), true) => {
                    api.stop_media(mid);
                    false
                }
                (None, false) => {
                    forward.mid = Some(api.add_media(
                        forward.kind,
                        Direction::SendOnly,
                        Some(forward.from.to_string()),
                        Some(forward.source.to_string()),
                        None,
                    ));
                    true
                }
                (Some(_), false) => true,
            });
        let Some((offer, pending)) = api.apply() else {
            return;
        };
        peer.pending = Some(pending);
        let tracks = peer
            .forwards
            .iter()
            .filter_map(|f| {
                Some(Track {
                    mid: f.mid?.to_string(),
                    peer: f.from,
                    source: f.source.to_string(),
                    kind: f.label,
                })
            })
            .collect();
        self.send(
            id,
            ServerMessage::Offer {
                sdp: offer.to_sdp_string(),
                tracks,
            },
        );
    }

    fn send(&mut self, id: PeerId, message: ServerMessage) {
        if let Some(peer) = self.peers.get(&id)
            && peer.out.try_send(message).is_err()
        {
            // A peer that does not read its messages leaves.
            self.kicks.push((id, "slow"));
        }
    }

    fn remove(&mut self, id: PeerId, reason: Option<&'static str>) {
        let Some(peer) = self.peers.remove(&id) else {
            return;
        };
        if let Some(reason) = reason {
            let _ = peer.out.try_send(ServerMessage::Error { reason });
        }
        self.rooms.leave(&peer.room, id);
        self.routing.remove(|(route, _)| route == id);
        let others: Vec<PeerId> = self
            .rooms
            .get(&peer.room)
            .map(|room| room.peers.iter().map(|(id, _)| *id).collect())
            .unwrap_or_default();
        for other in others {
            if let Some(other_peer) = self.peers.get_mut(&other) {
                for forward in other_peer.forwards.iter_mut().filter(|f| f.from == id) {
                    forward.stopping = true;
                }
            }
            self.send(other, ServerMessage::Left { peer: id });
            self.negotiate(other);
        }
    }

    fn datagram(&mut self, source: SocketAddr, data: &[u8], now: Instant) {
        let Some((id, leg)) = self.routing.route(source, data) else {
            return;
        };
        let Some(&destination) = self.local.iter().find(|a| a.is_ipv4() == source.is_ipv4()) else {
            return;
        };
        let Ok(contents) = data.try_into() else {
            return;
        };
        let input = Input::Receive(
            now,
            Receive {
                proto: Protocol::Udp,
                source,
                destination,
                contents,
            },
        );
        let Some(rtc) = self.peers.get_mut(&id).and_then(|p| p.rtc(leg)) else {
            return;
        };
        if rtc.handle_input(input).is_err() {
            self.kicks.push((id, "media"));
            return;
        }
        self.drain(id, leg, now);
    }

    fn timeouts(&mut self, now: Instant) {
        while let Some(&Reverse((at, id, leg))) = self.timers.peek() {
            if at > now {
                break;
            }
            self.timers.pop();
            let leg = if leg == 0 {
                Leg::Publish
            } else {
                Leg::Subscribe
            };
            let Some(peer) = self.peers.get_mut(&id) else {
                continue;
            };
            if peer.deadlines[leg as usize] != Some(at) {
                continue;
            }
            peer.deadlines[leg as usize] = None;
            if let Some(rtc) = peer.rtc(leg)
                && rtc.handle_input(Input::Timeout(now)).is_err()
            {
                self.kicks.push((id, "media"));
                continue;
            }
            self.drain(id, leg, now);
        }
    }

    /// Drains the connections that changed and disconnects kicked peers.
    fn flush(&mut self, now: Instant) {
        loop {
            if let Some((id, leg)) = self.dirty.pop() {
                self.drain(id, leg, now);
            } else if let Some((id, reason)) = self.kicks.pop() {
                self.remove(id, Some(reason));
            } else {
                return;
            }
        }
    }

    /// Polls one connection until it waits: sends its datagrams and handles
    /// its events.
    fn drain(&mut self, id: PeerId, leg: Leg, now: Instant) {
        loop {
            let Some(peer) = self.peers.get_mut(&id) else {
                return;
            };
            let Some(rtc) = peer.rtc(leg) else {
                return;
            };
            if !rtc.is_alive() {
                self.kicks.push((id, "media"));
                return;
            }
            match rtc.poll_output() {
                Err(_) => {
                    self.kicks.push((id, "media"));
                    return;
                }
                Ok(Output::Timeout(at)) => {
                    if peer.deadlines[leg as usize] != Some(at) {
                        peer.deadlines[leg as usize] = Some(at);
                        self.timers.push(Reverse((at, id, leg as u8)));
                    }
                    return;
                }
                Ok(Output::Transmit(transmit)) => {
                    self.routing.learn(transmit.destination, (id, leg), now);
                    if let Some(traffic) = &mut self.traffic {
                        // With the IPv4 and UDP headers.
                        traffic.add(transmit.contents.len() + 28);
                    }
                    let mut destination = transmit.destination;
                    if let (IpAddr::V4(v4), Ok(local)) =
                        (destination.ip(), self.socket.local_addr())
                        && local.is_ipv6()
                    {
                        destination =
                            SocketAddr::new(IpAddr::V6(v4.to_ipv6_mapped()), destination.port());
                    }
                    // A full socket buffer drops the datagram, as the network would.
                    let _ = self.socket.try_send_to(&transmit.contents, destination);
                }
                Ok(Output::Event(event)) => self.event(id, leg, event, now),
            }
        }
    }

    fn event(&mut self, id: PeerId, leg: Leg, event: Event, now: Instant) {
        match (leg, event) {
            (_, Event::IceConnectionStateChange(IceConnectionState::Disconnected)) => {
                self.kicks.push((id, "media"));
            }
            (Leg::Publish, Event::MediaAdded(added)) if added.direction.is_receiving() => {
                self.add_source(id, added.mid, added.kind);
            }
            (Leg::Publish, Event::RtpPacket(packet)) => self.forward(id, packet, now),
            (Leg::Subscribe, Event::EgressBitrateEstimate(BweKind::Twcc { estimate, .. })) => {
                if let Some(peer) = self.peers.get_mut(&id) {
                    peer.estimate = estimate.as_u64();
                }
            }
            (Leg::Subscribe, Event::KeyframeRequest(request)) => {
                let Some(peer) = self.peers.get(&id) else {
                    return;
                };
                if let Some(forward) = peer.forwards.iter().find(|f| f.mid == Some(request.mid)) {
                    let (from, source) = (forward.from, forward.source);
                    self.keyframe(from, source, now);
                }
            }
            _ => {}
        }
    }

    fn add_source(&mut self, id: PeerId, mid: Mid, kind: MediaKind) {
        let Some(peer) = self.peers.get_mut(&id) else {
            return;
        };
        if peer.sources.iter().any(|s| s.mid == mid) {
            return;
        }
        if peer.sources.len() >= self.config.max_sources_per_peer {
            self.kicks.push((id, "sources"));
            return;
        }
        let video = (kind == MediaKind::Video).then(|| {
            quality::Video::new(if peer.screens.contains(&mid) {
                Kind::Screen
            } else {
                Kind::Camera
            })
        });
        peer.sources.push(Source {
            mid,
            kind,
            video,
            layer: None,
            keyframe_asked: None,
        });
        let label = peer.sources.last().expect("pushed").label();
        let room = peer.room;
        let others: Vec<PeerId> = self.rooms.get(&room).map_or_else(Vec::new, |r| {
            r.peers
                .iter()
                .map(|(p, _)| *p)
                .filter(|p| *p != id)
                .collect()
        });
        for other in others {
            if let Some(peer) = self.peers.get_mut(&other) {
                peer.forwards.push(forward(id, mid, kind, label));
                self.negotiate(other);
            }
        }
    }

    fn forward(&mut self, from: PeerId, packet: RtpPacket, now: Instant) {
        let Some(publisher) = self.peers.get_mut(&from) else {
            return;
        };
        let room = publisher.room;
        let Some(rtc) = publisher.publish.as_mut() else {
            return;
        };
        let Some((mid, rid)) = rtc
            .direct_api()
            .stream_rx(&packet.header.ssrc)
            .map(|stream| (stream.mid(), stream.rid()))
        else {
            return;
        };
        let Some(params) = rtc
            .codec_config()
            .params()
            .iter()
            .find(|p| p.pt() == packet.header.payload_type)
            .copied()
        else {
            return;
        };
        let Some(source) = publisher.sources.iter_mut().find(|s| s.mid == mid) else {
            return;
        };
        let descriptor = packet
            .header
            .ext_vals
            .user_values
            .get::<quality::Descriptor>();
        // None: no descriptor, every receiver gets every packet. Some(None): a
        // descriptor this source's structure cannot read yet, which a key frame
        // brings.
        let since = now.saturating_duration_since(self.origin);
        let frame = match (&mut source.video, descriptor) {
            (Some(video), Some(descriptor)) => Some(video.layers.packet(
                &descriptor.0,
                packet.payload.len(),
                since,
            )),
            _ => None,
        };
        // Q1 hook: per-receiver layer choice (docs/video-plan.md §5.3) replaces
        // this. Today every receiver gets the first layer a source sends, and
        // all of a single stream's packets, including every temporal layer.
        if *source.layer.get_or_insert(rid) != rid {
            return;
        }
        let video = source.kind == MediaKind::Video;
        let size = (packet.payload.len() + PACKET_OVERHEAD) as f64;
        let receivers: Vec<PeerId> = self.rooms.get(&room).map_or_else(Vec::new, |r| {
            r.peers
                .iter()
                .map(|(p, _)| *p)
                .filter(|p| *p != from)
                .collect()
        });
        let mut dropped = matches!(frame, Some(None));
        for id in receivers {
            let Some(peer) = self.peers.get_mut(&id) else {
                continue;
            };
            let Some(forward) = peer
                .forwards
                .iter_mut()
                .find(|f| f.from == from && f.source == mid && f.open)
            else {
                continue;
            };
            let Some(out_mid) = forward.mid else {
                continue;
            };
            let view = &mut forward.view;
            let keep = match &frame {
                None => true,
                Some(None) => false,
                Some(Some(frame)) => view.selector.forward(frame),
            };
            dropped |= view.selector.waiting();
            if !keep {
                view.rewrite.skip(*packet.seq_no);
                continue;
            }
            // The last packet of the highest spatial layer forwarded ends the
            // receiver's picture.
            let marker = packet.header.marker
                || frame
                    .flatten()
                    .is_some_and(|frame| frame.end && Some(frame.layer.spatial) == view.spatial());
            let Some(pt) = peer
                .subscribe
                .codec_config()
                .match_params(params)
                .map(|p| p.pt())
                .filter(|pt| {
                    peer.subscribe
                        .media(out_mid)
                        .is_some_and(|m| m.remote_pts().contains(pt))
                })
            else {
                continue;
            };
            let room_budget = &mut self.rooms.get_mut(&room).expect("room").budget;
            if !peer.budget.take(size, now) || !room_budget.take(size, now) {
                dropped = true;
                continue;
            }
            let Some(seq) = forward.view.rewrite.forward(*packet.seq_no) else {
                continue;
            };
            let mut api = peer.subscribe.direct_api();
            let Some(stream) = api.stream_tx_by_mid(out_mid, None) else {
                continue;
            };
            stream.write_rtp(
                RtpWrite::new(
                    pt,
                    seq.into(),
                    packet.header.timestamp,
                    packet.timestamp,
                    packet.payload.clone(),
                )
                .marker(marker)
                .ext_vals(quality::forwarded(&packet.header.ext_vals))
                .nackable(video),
            );
            self.dirty.push((id, Leg::Subscribe));
        }
        if dropped {
            self.keyframe(from, mid, now);
        }
    }

    /// The decode targets each receiver's tier allows of each layered source,
    /// and the largest tier asked of each video source.
    fn plan(&self) -> (Vec<Planned>, HashMap<(PeerId, Mid), usize>) {
        let mut demands: HashMap<(PeerId, Mid), usize> = HashMap::new();
        let mut planned = Vec::new();
        for (&id, peer) in &self.peers {
            for (index, forward) in peer.forwards.iter().enumerate() {
                let Some(video) = self.peers.get(&forward.from).and_then(|p| {
                    p.sources
                        .iter()
                        .find(|s| s.mid == forward.source)
                        .and_then(|s| s.video.as_ref())
                }) else {
                    continue;
                };
                let tier = quality::tier(video.kind, &forward.view, peer.decode, self.step);
                let mut choices = video.layers.choices(video.kind, tier);
                // Past the lowest tier the traffic budget leaves base layers only.
                if self.step > video.kind.top() as u64 {
                    choices.truncate(1);
                }
                let demand = demands.entry((forward.from, forward.source)).or_default();
                *demand = (*demand).max(tier);
                planned.push(Planned {
                    receiver: id,
                    forward: index,
                    choices,
                    ceiling: video.kind.ladder()[tier].ceiling,
                });
            }
        }
        (planned, demands)
    }

    fn limits(&self) -> budget::Limits {
        budget::Limits {
            host: self.config.upload_bitrate / 10 * 7,
            room: self.config.room_bitrate,
            receiver: self.config.receiver_bitrate,
        }
    }

    /// Q1's budget inputs for `planned`: each receiver's bandwidth estimate and
    /// one stream per receiver and source.
    fn streams<'a>(
        &self,
        planned: &[Planned],
        rates: &'a [Vec<u64>],
    ) -> (Vec<u64>, Vec<budget::Stream<'a>>) {
        let mut rooms = HashMap::new();
        let mut receivers = HashMap::new();
        let mut estimates = Vec::new();
        let mut streams = Vec::new();
        for (planned, rates) in planned.iter().zip(rates) {
            let peer = &self.peers[&planned.receiver];
            let next = rooms.len();
            let room = *rooms.entry(peer.room).or_insert(next);
            let receiver = *receivers.entry(planned.receiver).or_insert_with(|| {
                estimates.push(peer.estimate);
                estimates.len() - 1
            });
            let stepper = peer.forwards[planned.forward].view.stepper;
            streams.push(budget::Stream {
                room,
                receiver,
                rates,
                current: stepper.map_or(0, |s| s.current() + 1),
            });
        }
        (estimates, streams)
    }

    /// Once a second: shares the budgets (Q1's `allocate`) over every
    /// receiver's layered sources, moves each receiver's decode target toward
    /// its share at Q1's `Stepper` pace, asks each subscribe connection's
    /// bandwidth estimator to probe for the receiver's tiers, and tells each
    /// publisher the largest tier asked of each of its video sources, when it
    /// changed.
    fn quality(&mut self, now: Instant) {
        let since = now.saturating_duration_since(self.origin);
        if let Some(traffic) = &mut self.traffic {
            let step = traffic.step(unix_seconds());
            if step != self.step {
                tracing::info!(step, sent = traffic.sent(), "traffic budget step");
                self.step = step;
            }
            if let Err(error) = traffic.save() {
                tracing::warn!("saving the traffic count: {error}");
            }
        }
        let (planned, demands) = self.plan();
        let rates = rates(&planned);
        let (receivers, streams) = self.streams(&planned, &rates);
        let shares = budget::allocate(&self.limits(), &receivers, &streams);
        let mut desired: HashMap<PeerId, u64> = HashMap::new();
        for (planned, share) in planned.into_iter().zip(shares) {
            *desired.entry(planned.receiver).or_default() += planned.ceiling;
            let view = &mut self
                .peers
                .get_mut(&planned.receiver)
                .expect("listed")
                .forwards[planned.forward]
                .view;
            let target = share.checked_sub(1).map(|ceiling| {
                let resolutions: Vec<u32> = planned
                    .choices
                    .iter()
                    .map(|c| u32::from(c.layer.spatial))
                    .collect();
                let point = view
                    .stepper
                    .get_or_insert_with(|| Stepper::new(ceiling))
                    .update(&resolutions, ceiling, since);
                planned.choices[point].target
            });
            view.selector.want(target);
            view.choices = planned.choices;
        }
        for (id, desired) in desired {
            let peer = self.peers.get_mut(&id).expect("listed");
            let desired = desired.min(self.config.receiver_bitrate);
            peer.subscribe
                .bwe()
                .set_bitrate(Bitrate::bps(0), Bitrate::bps(desired));
            self.dirty.push((id, Leg::Subscribe));
        }
        let mut messages = Vec::new();
        for (&id, peer) in &mut self.peers {
            for source in &mut peer.sources {
                let Some(video) = &mut source.video else {
                    continue;
                };
                let demand = demands.get(&(id, source.mid)).copied();
                if demand.is_some() && demand != video.demand && video.throttle.allow(since) {
                    video.demand = demand;
                    let request = QualityRequest {
                        publisher: id,
                        kind: video.kind,
                        tier: demand.unwrap_or(0) as u8,
                    };
                    messages.push((id, hex::encode(request.to_bytes())));
                }
            }
        }
        for (id, request) in messages {
            self.send(id, ServerMessage::Quality { request });
        }
    }

    /// Asks a publisher for a keyframe of a video source, at most once per
    /// [`KEYFRAME_INTERVAL`].
    fn keyframe(&mut self, from: PeerId, mid: Mid, now: Instant) {
        let Some(peer) = self.peers.get_mut(&from) else {
            return;
        };
        let Some(source) = peer.sources.iter_mut().find(|s| s.mid == mid) else {
            return;
        };
        if source.kind != MediaKind::Video
            || source
                .keyframe_asked
                .is_some_and(|at| now.saturating_duration_since(at) < KEYFRAME_INTERVAL)
        {
            return;
        }
        source.keyframe_asked = Some(now);
        let rid = source.layer.flatten();
        if let Some(rtc) = peer.publish.as_mut()
            && let Some(stream) = rtc.direct_api().stream_rx_by_mid(mid, rid)
        {
            stream.request_keyframe(KeyframeRequestKind::Pli);
            self.dirty.push((from, Leg::Publish));
        }
    }
}

pub fn unix_seconds() -> i64 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64)
}

/// One receiver's stream of one video source, for the budgets.
struct Planned {
    receiver: PeerId,
    /// An index into the receiver's forwards.
    forward: usize,
    /// The decode targets its tier allows, lowest first; none without a descriptor.
    choices: Vec<Choice>,
    /// The most its tier needs, bits per second.
    ceiling: u64,
}

fn rates(planned: &[Planned]) -> Vec<Vec<u64>> {
    planned
        .iter()
        .map(|p| p.choices.iter().map(|c| c.rate).collect())
        .collect()
}

fn forward(from: PeerId, source: Mid, kind: MediaKind, label: &'static str) -> Forward {
    Forward {
        from,
        source,
        kind,
        label,
        view: quality::View::default(),
        mid: None,
        open: false,
        stopping: false,
    }
}
