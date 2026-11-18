// SPDX-License-Identifier: EUPL-1.2
//! Call keys on stock and compatible Mumble servers (docs/video-plan.md §4): links, the
//! keys derived from the link secret S, the X-Wing HPKE-PSK exchange between a publisher
//! and each subscriber, and rekeying. The epoch keys encrypt media through
//! `nuschl-sframe`, so this crate links no AES.
//!
//! The crate holds protocol and key state only: callers pass the time (Unix
//! milliseconds) and a cryptographic RNG, and carry the messages over the SFU. cargo-deny
//! admits the crypto worker as its one caller; the SDK driver (R1), the desktop (D1) and
//! the browser stream worker (K2) wait for their SECURITY.md rows.
//!
//! ```text
//! prk        = HKDF-Extract(salt = "nuschl/stream/v2", S)
//! stream_id  = HKDF-Expand(prk, "nuschl/stream/v2 stream id", 16)   // the SFU room
//! admit_key  = HKDF-Expand(prk, "nuschl/stream/v2 admit", 32)       // given to the SFU
//! psk        = HKDF-Expand(prk, "nuschl/stream/v2 psk", 32)
//! signal_key = HKDF-Expand(prk, "nuschl/stream/v2 signal", 32)
//! psk_id     = "nuschl/stream/v2" || stream_id
//!
//! subscriber: fresh X-Wing (sk_v, pk_v)
//!   KeyRequest{subscriber_id, nonce_v, not_after, pk_v}             ──► publisher
//!   ◄── KeyGrant{subscriber_id, nonce_s, enc, Seal(epoch, K)}      at the next rekey
//!   KeyAck{subscriber_id, confirm}                                  ──►
//!   ◄── Rekey{subscriber_id, counter, ChaCha20-Poly1305(channel_n, epoch, K)}
//!
//! transcript  = SHA-256("nuschl/stream/v2 transcript" || version || suite
//!                       || KeyRequest body || nonce_s)
//! HPKE        = X-Wing / HKDF-SHA256 / ChaCha20-Poly1305, PSK mode (psk, psk_id),
//!               info = "nuschl/stream/v2 grant" || transcript, aad = ""
//! confirm     = Export("nuschl/stream/v2 confirm subscriber->publisher", 32)
//! channel_1   = Export("nuschl/stream/v2 channel publisher->subscriber", 32)
//! channel_n+1 = HKDF-Expand(channel_n, "nuschl/stream/v2 channel ratchet", 32)
//! ```
//!
//! The grant's AEAD tag verifies only under the HPKE context, which binds psk and the
//! transcript; the exported confirmation proves the subscriber opened it. Each Rekey uses
//! the next channel key, and both sides drop the one before.
//!
//! Every key message is one ChaCha20-Poly1305 envelope under `signal_key`, so the SFU
//! sees no subscriber ids or keys, and messages from parties without S fail at the tag
//! (the plan's `mac_key` is this tag):
//!
//! ```text
//! struct { uint8 version = 1; uint8 kind; opaque nonce[12]; opaque sealed<..16 KiB>; }
//!   sealed = ChaCha20-Poly1305(signal_key, nonce, aad = version || kind, body)
//!
//! kind 1  KeyRequest  opaque subscriber_id[16]; opaque nonce_v[32]; uint64 not_after;
//!                     opaque pk_v[1216];
//! kind 2  KeyGrant    opaque subscriber_id[16]; opaque nonce_s[32]; opaque enc[1120];
//!                     opaque sealed_key[52];
//! kind 3  KeyAck      opaque subscriber_id[16]; opaque confirm[32];
//! kind 4  Rekey       opaque subscriber_id[16]; uint64 counter; opaque sealed_key[52];
//!                     (nonce = 0u32 || counter, aad = subscriber_id || counter)
//!
//! struct { uint32 epoch; opaque key[32]; } EpochKey;
//! ```
//!
//! **Delivery.** Send the messages from [`Publisher::rekey`], send them again after
//! [`RESEND_AFTER_MS`] (subscribers drop duplicates as replays), and switch the
//! encryptors to the new [`EpochKey`] [`SWITCH_AFTER_MS`] after the first send. A
//! subscriber that missed up to [`MAX_MISSED_REKEYS`] Rekeys catches up with the next
//! one; further behind, it joins again.
//!
//! **What S grants.** Every sk_v and epoch key is fresh randomness, so S and every
//! recorded message decrypt no media. PSK mode authenticates a grant as coming from a
//! holder of S, and every subscriber holds the epoch key: media authenticity rests on the
//! SFU admitting only publishers that prove P (plan E1).

mod exchange;
mod link;
mod wire;

#[cfg(test)]
mod tests;

use rand_core::TryCryptoRng;

pub use exchange::{
    EpochKey, MAX_MISSED_REKEYS, MIN_EPOCH_INTERVAL_MS, Publisher, PublisherEvent,
    REQUEST_LIFETIME_MS, RESEND_AFTER_MS, Rekey, SWITCH_AFTER_MS, Subscriber, SubscriberEvent,
    SubscriberId,
};
pub use link::{Link, LinkKeys, admit_key, host_key, room_check, stream_id};

/// The largest key message.
pub const MAX_KEY_MESSAGE: usize = 16 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum Error {
    #[error("malformed {0}")]
    Malformed(&'static str),
    #[error("too long")]
    TooLong,
    #[error("unsupported version")]
    UnsupportedVersion,
    #[error("authentication failed")]
    Authentication,
    #[error("invalid public key")]
    InvalidPublicKey,
    #[error("expired")]
    Expired,
    #[error("replayed")]
    Replay,
    #[error("stale epoch")]
    StaleEpoch,
    #[error("unknown subscriber")]
    UnknownSubscriber,
    #[error("unexpected message")]
    Unexpected,
    #[error("limit reached")]
    LimitReached,
    #[error("rekey less than a second after the last one")]
    TooSoon,
    #[error("counter exhausted")]
    Exhausted,
    #[error("randomness unavailable")]
    Randomness,
}

fn random<const N: usize, R: TryCryptoRng + ?Sized>(rng: &mut R) -> Result<[u8; N], Error> {
    let mut out = [0; N];
    rng.try_fill_bytes(&mut out)
        .map_err(|_| Error::Randomness)?;
    Ok(out)
}

fn sha256(parts: &[&[u8]]) -> [u8; 32] {
    use libcrux_sha2::Digest as _;
    let mut hasher = libcrux_sha2::Sha256::new();
    for part in parts {
        hasher.update(part);
    }
    let mut out = [0; 32];
    hasher.finish(&mut out);
    out
}

fn hkdf_extract(salt: &[u8], ikm: &[u8]) -> [u8; 32] {
    let mut prk = [0; 32];
    libcrux_hkdf::extract(libcrux_hkdf::Algorithm::Sha256, &mut prk, salt, ikm)
        .expect("inputs are far below the HKDF limits");
    prk
}

fn hkdf_expand<const N: usize>(prk: &[u8; 32], info: &[u8]) -> [u8; N] {
    let mut okm = [0; N];
    libcrux_hkdf::expand(libcrux_hkdf::Algorithm::Sha256, &mut okm, prk, info)
        .expect("outputs are at most 32 bytes");
    okm
}
