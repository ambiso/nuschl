// SPDX-License-Identifier: EUPL-1.2
//! Signalling: one JSON object per WebSocket text message, tagged by `type`.
//! Binary values are lowercase hex.
//!
//! Each peer has up to two WebRTC connections. On its publish connection the
//! peer offers and the SFU answers (`offer` → `answer`); on its subscribe
//! connection the SFU offers and the peer answers. Each side is always the
//! offerer on its own connection, so offers never cross. The subscribe
//! connection also carries an unused data channel, `sfu`, which keeps its
//! transport up while no source is forwarded.
//!
//! ```text
//! SFU → peer  hello {nonce}
//! peer → SFU  join {room, verifier, proof, host_proof?, create?: {admit_key, host_key}, codecs}
//! SFU → peer  welcome {peer, role, peers: [{id, role}], codec, turn?: {urls, username, credential}}
//!             | error {reason}
//! peer → SFU  offer {sdp, screens?: [mid]}  publish connection
//! SFU → peer  answer {sdp}
//! SFU → peer  offer {sdp, tracks: [{mid, peer, source, screen}]}   subscribe connection
//! peer → SFU  answer {sdp}
//! peer → SFU  key {to, data}               relayed as key {from, data}
//! peer → SFU  remove {peer} | end          hosts only
//! SFU → peer  joined {peer, role} | left {peer}
//! SFU → peer  codec {codec}
//! ```
//!
//! A publisher's `screens` names the mids of its offer that carry a shared
//! screen, at most `max_sources_per_peer`; the SFU marks their tracks `screen`
//! for the pages' large view. A later offer that stops or deactivates a mid
//! ends that source: its tracks leave the subscribers' next offers.
//!
//! **One codec per call** (docs/video-plan.md §6). `codecs` lists the video
//! codecs the peer decodes in WebRTC, `"av1"` and `"vp9"`, at least one. The
//! call codec is AV1 while every peer decodes it, otherwise VP9; a join that
//! leaves no codec every peer decodes fails with `error {reason: "codec"}`.
//! `welcome` carries the call codec and `codec` each change after a join or a
//! leave; publishers encode in it.
//!
//! **Keys and proofs.** S and P are the link's two 32-byte secrets (`crates/stream`):
//!
//! ```text
//! room       = HKDF-Expand(HKDF-Extract("nuschl/stream/v2", S), "nuschl/stream/v2 stream id", 16)
//! admit_key  = HKDF-Expand(HKDF-Extract("nuschl/stream/v2", S), "nuschl/stream/v2 admit", 32)
//! host_key   = HKDF-Expand(HKDF-Extract("nuschl/stream/v2", P), "nuschl/stream/v2 host", 32)
//! verifier   = SHA-256(host_key)                                      // V
//! proof      = HMAC-SHA256(admit_key, "nuschl/stream/v2 sfu admit" || nonce)
//! host_proof = HMAC-SHA256(host_key,  "nuschl/stream/v2 sfu host"  || nonce)
//! ```
//!
//! HKDF-Extract's first argument is the salt; `nonce` is the 32 bytes of `hello`.
//! `crates/stream` provides these as `stream_id`, `admit_key`, `host_key` and
//! `room_check` (V). Every link carries V, in the same unpadded base64url as S
//! and P: `/call#v=1&k=<S>&h=<V>`, and the creator's link adds `&p=<P>`. A room is named
//! by `(room, verifier)`. `create` succeeds only with a `host_key` that hashes
//! to `verifier`, so a holder of S alone cannot create the room a link names.
//!
//! ```text
//! P        = 000102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f
//! host_key = 714cce84239301bcc3f4fcaaf0d59ca0d242f7b0bc9cf276ab9a3568bf9a873f
//! V        = 078384c474855fcaa97ecbb155d67f03add025efa0c126d8a23c30e71d6c4c37
//! h=         B4OExHSFX8qpfsuxVdZ_A63QJe-gwSbYojww5x1sTDc
//! ```
//!
//! **Lease.** The creator's device keeps the room alive over plain HTTP on the
//! same origin; P stays on the device, the SFU receives `host_key`:
//!
//! ```text
//! POST   /sfu/lease  {"stream_id": "<hex>", "host_key": "<hex>"}   renew, every 10 s
//! DELETE /sfu/lease  {"stream_id": "<hex>", "host_key": "<hex>"}   end the room
//! → 204 done | 400 malformed | 404 no room for this stream_id and SHA-256(host_key)
//!   | 429 renewed less than a second ago, or too many connections from the address
//! ```
//!
//! The room `(stream_id, SHA-256(host_key))` ends 30 s (`lease_secs`) after its
//! creation or its last renewal; ending disconnects every peer with `error
//! {reason: "ended"}` and refuses joins until the room expires. A body is at
//! most 1 KiB with a `Content-Length`, a request head at most 8 KiB, and each
//! request uses one connection of the per-address limit.

use crate::rooms::{PeerId, Role};
use serde::{Deserialize, Serialize};

/// The largest key message (docs/video-plan.md §4), before hex encoding. Key
/// messages are `nuschl-stream`'s envelopes between a Publisher and its
/// Subscribers (a key request is 1,272 bytes); the SFU relays them unread.
pub const MAX_KEY_MESSAGE: usize = 16 * 1024;

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum ClientMessage {
    Join {
        room: String,
        verifier: String,
        proof: String,
        #[serde(default)]
        host_proof: Option<String>,
        #[serde(default)]
        create: Option<Create>,
        codecs: Vec<Codec>,
    },
    Offer {
        sdp: String,
        #[serde(default)]
        screens: Vec<String>,
    },
    Answer {
        sdp: String,
    },
    Key {
        to: PeerId,
        data: String,
    },
    Remove {
        peer: PeerId,
    },
    End,
}

/// A video codec a call can use.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Codec {
    Av1,
    Vp9,
}

/// The video codecs a peer decodes.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Decoders {
    pub av1: bool,
    pub vp9: bool,
}

impl Decoders {
    /// `None` for an empty list.
    pub fn new(codecs: &[Codec]) -> Option<Self> {
        let decoders = Self {
            av1: codecs.contains(&Codec::Av1),
            vp9: codecs.contains(&Codec::Vp9),
        };
        (decoders != Self::default()).then_some(decoders)
    }
}

/// The call codec of peers that decode `peers`: AV1 when all of them do, else
/// VP9 when all of them do.
pub fn call_codec(peers: impl IntoIterator<Item = Decoders>) -> Option<Codec> {
    let (av1, vp9) = peers
        .into_iter()
        .fold((true, true), |(av1, vp9), d| (av1 && d.av1, vp9 && d.vp9));
    match (av1, vp9) {
        (true, _) => Some(Codec::Av1),
        (false, true) => Some(Codec::Vp9),
        (false, false) => None,
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Create {
    pub admit_key: String,
    pub host_key: String,
}

#[derive(Clone, Debug, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ServerMessage {
    Hello {
        nonce: String,
    },
    Welcome {
        peer: PeerId,
        role: Role,
        peers: Vec<PeerInfo>,
        codec: Codec,
        turn: Option<TurnCredentials>,
    },
    Joined {
        peer: PeerId,
        role: Role,
    },
    Left {
        peer: PeerId,
    },
    Codec {
        codec: Codec,
    },
    Offer {
        sdp: String,
        tracks: Vec<Track>,
    },
    Answer {
        sdp: String,
    },
    Key {
        from: PeerId,
        data: String,
    },
    Error {
        reason: &'static str,
    },
}

#[derive(Clone, Debug, Serialize)]
pub struct PeerInfo {
    pub id: PeerId,
    pub role: Role,
}

#[derive(Clone, Debug, Serialize)]
pub struct TurnCredentials {
    pub urls: Vec<String>,
    pub username: String,
    pub credential: String,
}

/// A forwarded source: `mid` on the receiver's subscribe connection carries
/// the source with `source` as its mid on the publisher's connection;
/// `screen` when the publisher named it a shared screen.
#[derive(Clone, Debug, Serialize)]
pub struct Track {
    pub mid: String,
    pub peer: PeerId,
    pub source: String,
    pub screen: bool,
}

pub fn from_hex<const N: usize>(text: &str) -> Option<[u8; N]> {
    let mut bytes = [0; N];
    hex::decode_to_slice(text, &mut bytes).ok()?;
    Some(bytes)
}

/// A key message is relayed as it came: hex of at most [`MAX_KEY_MESSAGE`] bytes.
pub fn valid_key_message(data: &str) -> bool {
    !data.is_empty()
        && data.len() <= 2 * MAX_KEY_MESSAGE
        && data.len().is_multiple_of(2)
        && data.bytes().all(|b| b.is_ascii_hexdigit())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wire_format() {
        let join: ClientMessage = serde_json::from_str(
            r#"{"type":"join","room":"00","verifier":"44","proof":"11","create":{"admit_key":"22","host_key":"33"},"codecs":["vp9","av1"]}"#,
        )
        .expect("join");
        assert!(matches!(
            join,
            ClientMessage::Join {
                host_proof: None,
                create: Some(_),
                ref codecs,
                ..
            } if codecs == &[Codec::Vp9, Codec::Av1]
        ));
        assert!(
            serde_json::from_str::<ClientMessage>(
                r#"{"type":"join","room":"00","verifier":"44","proof":"11","codecs":["h264"]}"#
            )
            .is_err()
        );
        assert!(matches!(
            serde_json::from_str(r#"{"type":"end"}"#).expect("end"),
            ClientMessage::End
        ));
        assert!(serde_json::from_str::<ClientMessage>(r#"{"type":"bogus"}"#).is_err());
        let welcome = serde_json::to_string(&ServerMessage::Welcome {
            peer: 3,
            role: Role::Host,
            peers: vec![PeerInfo {
                id: 1,
                role: Role::Participant,
            }],
            codec: Codec::Vp9,
            turn: None,
        })
        .expect("json");
        assert_eq!(
            welcome,
            r#"{"type":"welcome","peer":3,"role":"host","peers":[{"id":1,"role":"participant"}],"codec":"vp9","turn":null}"#
        );
    }

    #[test]
    fn one_codec_per_call() {
        let decoders = |codecs: &[Codec]| Decoders::new(codecs).expect("a codec");
        let both = decoders(&[Codec::Av1, Codec::Vp9]);
        let vp9 = decoders(&[Codec::Vp9]);
        let av1 = decoders(&[Codec::Av1]);
        assert_eq!(Decoders::new(&[]), None);
        assert_eq!(call_codec([both, both]), Some(Codec::Av1));
        assert_eq!(call_codec([both, vp9]), Some(Codec::Vp9));
        assert_eq!(call_codec([av1, both]), Some(Codec::Av1));
        assert_eq!(call_codec([av1, vp9]), None);
        assert_eq!(call_codec([vp9]), Some(Codec::Vp9));
    }

    #[test]
    fn hex_and_key_bounds() {
        assert_eq!(from_hex::<2>("0aff"), Some([0x0a, 0xff]));
        assert_eq!(from_hex::<2>("0aff00"), None);
        assert_eq!(from_hex::<2>("zz00"), None);
        assert!(valid_key_message(&"ab".repeat(MAX_KEY_MESSAGE)));
        assert!(!valid_key_message(&"ab".repeat(MAX_KEY_MESSAGE + 1)));
        assert!(!valid_key_message("abc"));
        assert!(!valid_key_message("xy"));
        assert!(!valid_key_message(""));
    }
}
