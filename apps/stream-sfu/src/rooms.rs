// SPDX-License-Identifier: EUPL-1.2
//! Rooms and admission (docs/video-plan.md §4). A room is named by the link's
//! `stream_id` and its host verifier `V = SHA-256(host_key)`, and holds two keys
//! its creator hands over: `admit_key`, which every link holder can derive from
//! S, and `host_key`, which only holders of P can derive. Creating a room needs
//! a `host_key` that hashes to V, so a link names exactly one room. A proof of `admit_key` lets a peer publish its own sources and
//! subscribe; a proof of `host_key` also lets it create the room, remove peers
//! and end the room. A peer proves a key with an HMAC over the nonce the SFU
//! sent on its connection, so a proof cannot be replayed on another connection.
//!
//! A room keeps its keys until it expires, including while it is empty, so the
//! room id names the same keys for the whole life of its link.

use hmac::{KeyInit, Mac};
use serde::Serialize;
use std::{
    collections::HashMap,
    net::IpAddr,
    time::{Duration, Instant, SystemTime},
};

pub type PeerId = u64;
/// `(stream_id, V)`.
pub type RoomId = ([u8; 16], [u8; 32]);
pub type Key = [u8; 32];
pub type Nonce = [u8; 32];

/// Labels of the two proofs: `HMAC-SHA256(key, label || nonce)`.
pub const ADMIT_LABEL: &[u8] = b"nuschl/stream/v2 sfu admit";
pub const HOST_LABEL: &[u8] = b"nuschl/stream/v2 sfu host";

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    /// Proved S: publishes its own sources, subscribes, exchanges key messages.
    Participant,
    /// Also proved P: removes peers and ends the room.
    Host,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Refusal {
    Proof,
    NoRoom,
    Ended,
    Full,
    Rooms,
    Rate,
    NotAllowed,
}

impl Refusal {
    pub fn reason(self) -> &'static str {
        match self {
            Self::Proof => "proof",
            Self::NoRoom => "no_room",
            Self::Ended => "ended",
            Self::Full => "full",
            Self::Rooms => "busy",
            Self::Rate => "rate",
            Self::NotAllowed => "not_allowed",
        }
    }
}

pub struct Join {
    pub room: RoomId,
    pub proof: [u8; 32],
    pub host_proof: Option<[u8; 32]>,
    /// `(admit_key, host_key)`: creates the room, or joins it if it exists
    /// with the same keys.
    pub create: Option<(Key, Key)>,
}

pub fn prove(key: &Key, label: &[u8], nonce: &Nonce) -> [u8; 32] {
    mac(key, label, nonce).finalize().into_bytes().into()
}

fn mac(key: &Key, label: &[u8], nonce: &Nonce) -> hmac::Hmac<sha2::Sha256> {
    let mut mac = hmac::Hmac::<sha2::Sha256>::new_from_slice(key).expect("any key length");
    mac.update(label);
    mac.update(nonce);
    mac
}

/// The host verifier the link carries: `V = SHA-256(host_key)`.
pub fn verifier(host_key: &Key) -> [u8; 32] {
    use sha2::Digest as _;
    sha2::Sha256::digest(host_key).into()
}

fn verify(key: &Key, label: &[u8], nonce: &Nonce, proof: &[u8; 32]) -> bool {
    mac(key, label, nonce).verify_slice(proof).is_ok()
}

/// A token bucket: `rate` units per second, `burst` at most.
#[derive(Clone, Copy, Debug)]
pub struct Bucket {
    tokens: f64,
    rate: f64,
    burst: f64,
    updated: Instant,
}

impl Bucket {
    pub fn new(rate: f64, burst: f64, now: Instant) -> Self {
        Self {
            tokens: burst,
            rate,
            burst,
            updated: now,
        }
    }

    pub fn take(&mut self, amount: f64, now: Instant) -> bool {
        let elapsed = now.saturating_duration_since(self.updated).as_secs_f64();
        self.tokens = (self.tokens + elapsed * self.rate).min(self.burst);
        self.updated = now;
        if self.tokens < amount {
            return false;
        }
        self.tokens -= amount;
        true
    }
}

/// A media budget in bytes for `bits_per_second`, with one second of burst so a
/// large key frame fits.
pub fn media_budget(bits_per_second: u64, now: Instant) -> Bucket {
    let bytes = bits_per_second as f64 / 8.0;
    Bucket::new(bytes, bytes, now)
}

pub struct Room {
    admit_key: Key,
    host_key: Key,
    expires: Instant,
    ended: bool,
    pub peers: Vec<(PeerId, Role)>,
    pub budget: Bucket,
}

pub struct Limits {
    pub rooms: usize,
    pub peers_per_room: usize,
    pub lifetime: Duration,
    pub creations_per_hour: u32,
    pub room_bitrate: u64,
}

pub struct Rooms {
    limits: Limits,
    rooms: HashMap<RoomId, Room>,
    /// Room creations per client network in the current hour.
    creations: HashMap<IpAddr, (Instant, u32)>,
}

/// IPv4 addresses as they are, IPv6 addresses by their /64.
fn network(ip: IpAddr) -> IpAddr {
    match ip {
        IpAddr::V6(v6) => match v6.to_ipv4_mapped() {
            Some(v4) => IpAddr::V4(v4),
            None => IpAddr::V6((u128::from(v6) & !u128::from(u64::MAX)).into()),
        },
        v4 => v4,
    }
}

const HOUR: Duration = Duration::from_secs(3600);

impl Rooms {
    pub fn new(limits: Limits) -> Self {
        Self {
            limits,
            rooms: HashMap::new(),
            creations: HashMap::new(),
        }
    }

    pub fn get(&self, room: &RoomId) -> Option<&Room> {
        self.rooms.get(room)
    }

    pub fn get_mut(&mut self, room: &RoomId) -> Option<&mut Room> {
        self.rooms.get_mut(room)
    }

    pub fn join(
        &mut self,
        peer: PeerId,
        join: &Join,
        nonce: &Nonce,
        client: IpAddr,
        now: Instant,
    ) -> Result<Role, Refusal> {
        let (admit_key, host_key) = match (self.rooms.get(&join.room), join.create) {
            (Some(room), _) if room.ended => return Err(Refusal::Ended),
            (Some(room), Some(keys)) if (room.admit_key, room.host_key) != keys => {
                return Err(Refusal::Proof);
            }
            (Some(room), _) => (room.admit_key, room.host_key),
            (None, Some(keys)) if verifier(&keys.1) == join.room.1 => keys,
            (None, Some(_)) => return Err(Refusal::Proof),
            (None, None) => return Err(Refusal::NoRoom),
        };
        if !verify(&admit_key, ADMIT_LABEL, nonce, &join.proof) {
            return Err(Refusal::Proof);
        }
        let role = match &join.host_proof {
            None => Role::Participant,
            Some(proof) if verify(&host_key, HOST_LABEL, nonce, proof) => Role::Host,
            Some(_) => return Err(Refusal::Proof),
        };
        if !self.rooms.contains_key(&join.room) {
            if role != Role::Host {
                return Err(Refusal::Proof);
            }
            self.create(join.room, admit_key, host_key, client, now)?;
        }
        let room = self.rooms.get_mut(&join.room).expect("exists");
        if room.peers.len() >= self.limits.peers_per_room {
            return Err(Refusal::Full);
        }
        room.peers.push((peer, role));
        Ok(role)
    }

    fn create(
        &mut self,
        id: RoomId,
        admit_key: Key,
        host_key: Key,
        client: IpAddr,
        now: Instant,
    ) -> Result<(), Refusal> {
        if self.rooms.len() >= self.limits.rooms {
            return Err(Refusal::Rooms);
        }
        // The table holds at most one entry per room slot; older ones go first.
        if self.creations.len() >= self.limits.rooms {
            self.creations
                .retain(|_, (start, _)| now.saturating_duration_since(*start) < HOUR);
            if self.creations.len() >= self.limits.rooms {
                return Err(Refusal::Rate);
            }
        }
        let entry = self.creations.entry(network(client)).or_insert((now, 0));
        if now.saturating_duration_since(entry.0) >= HOUR {
            *entry = (now, 0);
        }
        if entry.1 >= self.limits.creations_per_hour {
            return Err(Refusal::Rate);
        }
        entry.1 += 1;
        self.rooms.insert(
            id,
            Room {
                admit_key,
                host_key,
                expires: now + self.limits.lifetime,
                ended: false,
                peers: Vec::new(),
                budget: media_budget(self.limits.room_bitrate, now),
            },
        );
        Ok(())
    }

    pub fn leave(&mut self, room: &RoomId, peer: PeerId) {
        if let Some(room) = self.rooms.get_mut(room) {
            room.peers.retain(|(id, _)| *id != peer);
        }
    }

    /// Whether `peer` may remove peers and end the room.
    pub fn is_host(&self, room: &RoomId, peer: PeerId) -> bool {
        self.rooms
            .get(room)
            .is_some_and(|room| room.peers.contains(&(peer, Role::Host)))
    }

    /// A host ends the room; it refuses joins until it expires. Returns the
    /// peers to disconnect.
    pub fn end(&mut self, room: &RoomId, by: PeerId) -> Result<Vec<PeerId>, Refusal> {
        if !self.is_host(room, by) {
            return Err(Refusal::NotAllowed);
        }
        let room = self.rooms.get_mut(room).ok_or(Refusal::NoRoom)?;
        room.ended = true;
        Ok(room.peers.drain(..).map(|(id, _)| id).collect())
    }

    /// Removes expired rooms and returns their peers to disconnect.
    pub fn expire(&mut self, now: Instant) -> Vec<PeerId> {
        let mut peers = Vec::new();
        self.rooms.retain(|_, room| {
            let keep = room.expires > now;
            if !keep {
                peers.extend(room.peers.iter().map(|(id, _)| *id));
            }
            keep
        });
        peers
    }
}

/// TURN REST credentials: `expiry:random` and base64(HMAC-SHA1(secret, username)).
pub fn turn_credentials(secret: &[u8], lifetime: Duration, now: SystemTime) -> (String, String) {
    use base64::Engine as _;
    let expiry = (now + lifetime)
        .duration_since(SystemTime::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    let username = format!("{expiry}:{}", hex::encode(random::<8>()));
    let mut mac = hmac::Hmac::<sha1::Sha1>::new_from_slice(secret).expect("any key length");
    mac.update(username.as_bytes());
    let password = base64::engine::general_purpose::STANDARD.encode(mac.finalize().into_bytes());
    (username, password)
}

pub fn random<const N: usize>() -> [u8; N] {
    let mut bytes = [0; N];
    getrandom::fill(&mut bytes).expect("the OS random number generator");
    bytes
}

#[cfg(test)]
mod tests {
    use super::*;

    const ADMIT: Key = [1; 32];
    const HOST: Key = [2; 32];
    static ROOM: std::sync::LazyLock<RoomId> =
        std::sync::LazyLock::new(|| ([7; 16], verifier(&HOST)));

    fn rooms() -> Rooms {
        Rooms::new(Limits {
            rooms: 2,
            peers_per_room: 3,
            lifetime: HOUR,
            creations_per_hour: 2,
            room_bitrate: 1_000_000,
        })
    }

    fn join(nonce: &Nonce, host: bool, create: bool) -> Join {
        Join {
            room: *ROOM,
            proof: prove(&ADMIT, ADMIT_LABEL, nonce),
            host_proof: host.then(|| prove(&HOST, HOST_LABEL, nonce)),
            create: create.then_some((ADMIT, HOST)),
        }
    }

    fn client() -> IpAddr {
        "192.0.2.1".parse().expect("ip")
    }

    #[test]
    fn admission_needs_proofs_bound_to_the_nonce() {
        let now = Instant::now();
        let mut rooms = rooms();
        let n = [9; 32];
        assert_eq!(
            rooms.join(1, &join(&n, false, false), &n, client(), now),
            Err(Refusal::NoRoom)
        );
        // Creating needs P.
        assert_eq!(
            rooms.join(1, &join(&n, false, true), &n, client(), now),
            Err(Refusal::Proof)
        );
        assert_eq!(
            rooms.join(1, &join(&n, true, true), &n, client(), now),
            Ok(Role::Host)
        );
        // A proof made for another nonce fails.
        let other = [8; 32];
        assert_eq!(
            rooms.join(2, &join(&other, false, false), &n, client(), now),
            Err(Refusal::Proof)
        );
        let mut bad_host = join(&n, false, false);
        bad_host.host_proof = Some(prove(&ADMIT, HOST_LABEL, &n));
        assert_eq!(
            rooms.join(2, &bad_host, &n, client(), now),
            Err(Refusal::Proof)
        );
        assert_eq!(
            rooms.join(2, &join(&n, false, false), &n, client(), now),
            Ok(Role::Participant)
        );
        // The same keys join; other keys for the same room id are refused.
        assert_eq!(
            rooms.join(3, &join(&n, true, true), &n, client(), now),
            Ok(Role::Host)
        );
        let mut squat = join(&n, true, true);
        squat.create = Some(([3; 32], [4; 32]));
        squat.proof = prove(&[3; 32], ADMIT_LABEL, &n);
        squat.host_proof = Some(prove(&[4; 32], HOST_LABEL, &n));
        assert_eq!(
            rooms.join(4, &squat, &n, client(), now),
            Err(Refusal::Proof)
        );
        // Keys of its own make another room: the link's V names the first one.
        squat.room.1 = verifier(&[4; 32]);
        assert_eq!(rooms.join(4, &squat, &n, client(), now), Ok(Role::Host));
        assert_eq!(rooms.get(&ROOM).expect("room").peers.len(), 3);
        // Creating needs a host_key that hashes to V.
        let mut empty = self::rooms();
        let mut wrong_v = join(&n, true, true);
        wrong_v.room.1 = verifier(&[5; 32]);
        assert_eq!(
            empty.join(5, &wrong_v, &n, client(), now),
            Err(Refusal::Proof)
        );
    }

    #[test]
    fn verifier_matches_the_link_vector() {
        // host_key from P = 00 01 … 1f (protocol.rs).
        let host_key =
            hex::decode("714cce84239301bcc3f4fcaaf0d59ca0d242f7b0bc9cf276ab9a3568bf9a873f")
                .expect("hex");
        assert_eq!(
            hex::encode(verifier(&host_key.try_into().expect("32 bytes"))),
            "078384c474855fcaa97ecbb155d67f03add025efa0c126d8a23c30e71d6c4c37"
        );
    }

    #[test]
    fn bookkeeping_and_bounds() {
        let now = Instant::now();
        let mut rooms = rooms();
        let n = [9; 32];
        for peer in 1..=3 {
            rooms
                .join(peer, &join(&n, peer == 1, peer == 1), &n, client(), now)
                .expect("join");
        }
        assert_eq!(
            rooms.join(4, &join(&n, false, false), &n, client(), now),
            Err(Refusal::Full)
        );
        rooms.leave(&ROOM, 3);
        assert_eq!(
            rooms.join(4, &join(&n, false, false), &n, client(), now),
            Ok(Role::Participant)
        );

        // Only a host ends the room.
        assert!(rooms.is_host(&ROOM, 1) && !rooms.is_host(&ROOM, 4) && !rooms.is_host(&ROOM, 9));
        assert_eq!(rooms.end(&ROOM, 4), Err(Refusal::NotAllowed));
        let mut ended = rooms.end(&ROOM, 1).expect("end");
        ended.sort();
        assert_eq!(ended, vec![1, 2, 4]);
        assert_eq!(
            rooms.join(5, &join(&n, true, true), &n, client(), now),
            Err(Refusal::Ended)
        );

        // Empty rooms keep their keys until they expire.
        assert!(rooms.expire(now + HOUR / 2).is_empty());
        assert!(rooms.get(&ROOM).is_some());
        assert!(rooms.expire(now + HOUR).is_empty());
        assert!(rooms.get(&ROOM).is_none());
    }

    #[test]
    fn expiry_disconnects_peers() {
        let now = Instant::now();
        let mut rooms = rooms();
        let n = [9; 32];
        rooms
            .join(1, &join(&n, true, true), &n, client(), now)
            .expect("join");
        rooms
            .join(2, &join(&n, false, false), &n, client(), now)
            .expect("join");
        let mut expired = rooms.expire(now + HOUR);
        expired.sort();
        assert_eq!(expired, vec![1, 2]);
        assert_eq!(
            rooms.join(3, &join(&n, false, false), &n, client(), now),
            Err(Refusal::NoRoom)
        );
    }

    #[test]
    fn room_count_and_creation_rate() {
        let now = Instant::now();
        let mut rooms = Rooms::new(Limits {
            rooms: 3,
            ..rooms().limits
        });
        let n = [9; 32];
        let create = |room: u8| Join {
            room: ([room; 16], verifier(&HOST)),
            ..join(&n, true, true)
        };
        let ip = |text: &str| text.parse::<IpAddr>().expect("ip");
        // Two creations per /64 and hour.
        rooms
            .join(1, &create(1), &n, ip("2001:db8::1"), now)
            .expect("first");
        rooms
            .join(2, &create(2), &n, ip("2001:db8::2"), now)
            .expect("second");
        assert_eq!(
            rooms.join(3, &create(3), &n, ip("2001:db8::3"), now),
            Err(Refusal::Rate)
        );
        rooms
            .join(3, &create(3), &n, ip("2001:db8:0:1::1"), now)
            .expect("other /64");
        // The room table is full.
        assert_eq!(
            rooms.join(4, &create(4), &n, ip("192.0.2.9"), now),
            Err(Refusal::Rooms)
        );
        assert_eq!(rooms.expire(now + HOUR).len(), 3);
        rooms
            .join(5, &create(5), &n, ip("2001:db8::ffff"), now + HOUR)
            .expect("next hour");
    }

    #[test]
    fn buckets() {
        let now = Instant::now();
        let mut bucket = Bucket::new(10.0, 20.0, now);
        assert!(bucket.take(20.0, now));
        assert!(!bucket.take(1.0, now));
        assert!(bucket.take(10.0, now + Duration::from_secs(1)));
        assert!(
            !bucket.take(25.0, now + Duration::from_secs(100)),
            "capped at the burst"
        );
    }

    #[test]
    fn turn_credentials_follow_the_rest_scheme() {
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000);
        let (username, password) = turn_credentials(b"secret", Duration::from_secs(600), now);
        let (expiry, user) = username.split_once(':').expect("expiry:user");
        assert_eq!(expiry, "1000600");
        assert_eq!(user.len(), 16);
        use base64::Engine as _;
        let mut mac = hmac::Hmac::<sha1::Sha1>::new_from_slice(b"secret").expect("key");
        mac.update(username.as_bytes());
        let expected =
            base64::engine::general_purpose::STANDARD.encode(mac.finalize().into_bytes());
        assert_eq!(password, expected);
        assert_ne!(
            turn_credentials(b"secret", Duration::from_secs(600), now).0,
            username
        );
    }
}
