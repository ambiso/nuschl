// SPDX-License-Identifier: EUPL-1.2
//! The core: one task owns every room, every peer's WebRTC state (str0m in RTP
//! mode) and the one UDP socket. It forwards RTP packets from a publisher to
//! each subscriber with their payload, timestamp and marker unchanged, in
//! whatever codec the publisher negotiated; it never decrypts, depacketizes or
//! transcodes. Each subscriber gets its own sequence numbers, starting near
//! zero, and packets a budget drops leave a gap the receiver sees.

use crate::{
    config::Config,
    protocol::{self, ClientMessage, PeerInfo, ServerMessage, Track, TurnCredentials},
    rooms::{self, Bucket, Join, Nonce, PeerId, RoomId, Rooms},
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
    change::{SdpAnswer, SdpOffer, SdpPendingOffer},
    media::{Direction, KeyframeRequestKind, MediaKind, Mid, Rid},
    net::{Protocol, Receive},
    rtp::{RtpPacket, RtpWrite},
};
use tokio::{net::UdpSocket, sync::mpsc};

/// One keyframe request per source in this interval reaches its publisher.
const KEYFRAME_INTERVAL: Duration = Duration::from_secs(2);
/// Per-packet overhead counted against media budgets (IP, UDP, SRTP).
const PACKET_OVERHEAD: usize = 48;
/// The largest UDP datagram read; longer ones are dropped.
pub const MAX_DATAGRAM: usize = 2048;

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
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Leg {
    Publish,
    Subscribe,
}

struct Source {
    mid: Mid,
    kind: MediaKind,
    /// The simulcast layer forwarded, fixed by the first packet.
    layer: Option<Option<Rid>>,
    keyframe_asked: Option<Instant>,
}

struct Forward {
    from: PeerId,
    source: Mid,
    kind: MediaKind,
    /// The receiver-side mid, once offered.
    mid: Option<Mid>,
    open: bool,
    stopping: bool,
    /// The publisher's sequence number that maps to zero for this receiver.
    base: Option<u64>,
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
    routes: HashMap<SocketAddr, (PeerId, Leg)>,
    timers: BinaryHeap<Reverse<(Instant, PeerId, u8)>>,
    dirty: Vec<(PeerId, Leg)>,
    kicks: Vec<(PeerId, &'static str)>,
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
                creations_per_hour: config.creations_per_hour,
                room_bitrate: config.room_bitrate,
            }),
            config,
            socket,
            local,
            crypto: Arc::new(str0m::crypto::from_feature_flags()),
            turn,
            peers: HashMap::new(),
            routes: HashMap::new(),
            timers: BinaryHeap::new(),
            dirty: Vec::new(),
            kicks: Vec::new(),
        })
    }

    pub async fn run(mut self, mut commands: mpsc::Receiver<Command>) {
        let socket = self.socket.clone();
        let mut buffer = vec![0; MAX_DATAGRAM];
        let mut expiry = tokio::time::interval(Duration::from_secs(10));
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
                _ = expiry.tick() => {
                    for id in self.rooms.expire(Instant::now()) {
                        self.kicks.push((id, "expired"));
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
            Command::Message { id, message } => {
                if let Err(reason) = self.message(id, message, now) {
                    self.kicks.push((id, reason));
                }
            }
        }
    }

    fn new_rtc(&self, now: Instant) -> Rtc {
        let mut rtc = Rtc::builder()
            .set_crypto_provider(self.crypto.clone())
            .set_rtp_mode(true)
            .set_ice_lite(true)
            .build(now);
        for address in &self.local {
            if let Ok(candidate) = Candidate::host(*address, "udp") {
                rtc.add_local_candidate(candidate);
            }
        }
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
                forwards.push(forward(other.id, source.mid, source.kind));
            }
        }
        let peer = Peer {
            room: join.room,
            out,
            publish: None,
            subscribe: self.new_rtc(now),
            pending: None,
            sources: Vec::new(),
            forwards,
            budget: rooms::media_budget(self.config.receiver_bitrate, now),
            deadlines: [None; 2],
        };
        self.peers.insert(id, peer);
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
            ClientMessage::Offer { sdp } => {
                let offer = SdpOffer::from_sdp_string(&sdp).map_err(|_| "sdp")?;
                if self.peers[&id].publish.is_none() {
                    let rtc = self.new_rtc(now);
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
        self.routes.retain(|_, (route, _)| *route != id);
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
        let accepts = |peers: &mut HashMap<PeerId, Peer>, (id, leg): (PeerId, Leg)| {
            peers
                .get_mut(&id)
                .and_then(|peer| peer.rtc(leg))
                .is_some_and(|rtc| rtc.accepts(&input))
        };
        let target = match self.routes.get(&source) {
            Some(&route) if accepts(&mut self.peers, route) => Some(route),
            // Only ICE checks with a peer's credentials claim a new address.
            _ => self
                .peers
                .keys()
                .flat_map(|&id| [(id, Leg::Subscribe), (id, Leg::Publish)])
                .collect::<Vec<_>>()
                .into_iter()
                .find(|&route| accepts(&mut self.peers, route)),
        };
        let Some((id, leg)) = target else {
            return;
        };
        self.routes.insert(source, (id, leg));
        let rtc = self
            .peers
            .get_mut(&id)
            .and_then(|p| p.rtc(leg))
            .expect("accepted");
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
        peer.sources.push(Source {
            mid,
            kind,
            layer: None,
            keyframe_asked: None,
        });
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
                peer.forwards.push(forward(id, mid, kind));
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
        let mut dropped = false;
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
            let base = *forward.base.get_or_insert(*packet.seq_no);
            let Some(seq) = packet.seq_no.checked_sub(base) else {
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
                .marker(packet.header.marker)
                .nackable(video),
            );
            self.dirty.push((id, Leg::Subscribe));
        }
        if dropped {
            self.keyframe(from, mid, now);
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

fn forward(from: PeerId, source: Mid, kind: MediaKind) -> Forward {
    Forward {
        from,
        source,
        kind,
        mid: None,
        open: false,
        stopping: false,
        base: None,
    }
}
