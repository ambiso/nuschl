// SPDX-License-Identifier: EUPL-1.2
//! The sharer's and the viewers' key-exchange state machines (wire format in the crate
//! documentation). The sharer grants only fresh epochs: [`Sharer::rekey`] moves every
//! viewer to K_{e+1} and grants K_{e+1} to the pending requests, so a newcomer receives
//! only keys created after it joined.

use std::fmt;

use hpke_rs::hpke_types::{AeadAlgorithm, KdfAlgorithm, KemAlgorithm};
use hpke_rs::{Context, Hpke, HpkePrivateKey, Mode};
use hpke_rs_libcrux::HpkeLibcrux;
use libcrux_kem::Algorithm::XWingKemDraft06;
use rand_core::{TryCryptoRng, TryRng};
use subtle::ConstantTimeEq as _;

use crate::link::{LinkKeys, label};
use crate::wire::Reader;
use crate::{Error, MAX_KEY_MESSAGE, hkdf_expand, random, sha256};

const VERSION: u8 = 1;
/// KEM X-Wing, KDF HKDF-SHA256, AEAD ChaCha20-Poly1305, SFrame AES_128_GCM_SHA256_128.
const SUITE: [u8; 8] = [0x64, 0x7a, 0x00, 0x01, 0x00, 0x03, 0x00, 0x04];
const PUBLIC_KEY_LEN: usize = 1216;
const ENC_LEN: usize = 1120;
const AEAD_TAG_LEN: usize = 16;
const EPOCH_KEY_LEN: usize = 4 + 32;
const SEALED_KEY_LEN: usize = EPOCH_KEY_LEN + AEAD_TAG_LEN;

/// How far ahead a request's `not_after` may be: its lifetime plus a minute of clock
/// difference between viewer and sharer.
pub const REQUEST_LIFETIME_MS: u64 = 60_000;
const MAX_NOT_AFTER_MS: u64 = 2 * REQUEST_LIFETIME_MS;
/// The shortest time between two epochs, unless the clock stepped back.
pub const MIN_EPOCH_INTERVAL_MS: u64 = 1_000;
/// When to send a rekey's messages a second time.
pub const RESEND_AFTER_MS: u64 = 500;
/// When to switch the encryptors to a new epoch, counted from the first send.
pub const SWITCH_AFTER_MS: u64 = 1_000;
/// Requests waiting for the next rekey.
pub const MAX_PENDING: usize = 32;
/// Viewers holding keys, plus pending requests.
pub const MAX_VIEWERS: usize = 256;
/// Request nonces remembered until their `not_after`.
pub const MAX_SEEN: usize = 256;
/// Rekeys a viewer may have missed and still catch up. Further behind, every Rekey
/// fails with [`Error::LimitReached`] and the viewer has to join again.
pub const MAX_MISSED_REKEYS: u64 = 64;

pub(crate) const REQUEST: u8 = 1;
pub(crate) const GRANT: u8 = 2;
pub(crate) const ACK: u8 = 3;
pub(crate) const REKEY: u8 = 4;

pub type ViewerId = [u8; 16];

// -------------------------------------------------------------------------------------
// Epoch keys

/// One epoch's media key. `Debug` leaves out the key.
///
/// The sharer gets each epoch's key once from [`Sharer::rekey`] and gives it to one
/// sender: a second sender under the same epoch repeats AES-GCM nonces. A restarted
/// sender (a new encoder, track or worker process) waits for the next rekey.
pub struct EpochKey {
    pub epoch: u32,
    pub key: [u8; 32],
}

impl fmt::Debug for EpochKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "EpochKey(epoch {})", self.epoch)
    }
}

fn encode_epoch_key(key: &EpochKey) -> Vec<u8> {
    [&key.epoch.to_be_bytes()[..], &key.key].concat()
}

fn decode_epoch_key(bytes: &[u8]) -> Result<EpochKey, Error> {
    let mut reader = Reader::new(bytes);
    let epoch = reader.u32()?;
    let key = reader.array()?;
    reader.finish()?;
    Ok(EpochKey { epoch, key })
}

// -------------------------------------------------------------------------------------
// Envelope, bodies and HPKE

pub(crate) fn seal_message<R: TryCryptoRng + ?Sized>(
    keys: &LinkKeys,
    kind: u8,
    body: &[u8],
    rng: &mut R,
) -> Result<Vec<u8>, Error> {
    if 2 + 12 + body.len() + AEAD_TAG_LEN > MAX_KEY_MESSAGE {
        return Err(Error::TooLong);
    }
    let nonce: [u8; 12] = random(rng)?;
    let mut out = vec![VERSION, kind];
    out.extend_from_slice(&nonce);
    let start = out.len();
    out.resize(start + body.len() + AEAD_TAG_LEN, 0);
    libcrux_chacha20poly1305::encrypt(
        &keys.signal_key,
        body,
        &mut out[start..],
        &[VERSION, kind],
        &nonce,
    )
    .expect("buffer sized for body and tag");
    Ok(out)
}

pub(crate) fn open_message(keys: &LinkKeys, message: &[u8]) -> Result<(u8, Vec<u8>), Error> {
    if message.len() > MAX_KEY_MESSAGE {
        return Err(Error::TooLong);
    }
    let mut reader = Reader::new(message);
    let [version, kind] = reader.array()?;
    if version != VERSION {
        return Err(Error::UnsupportedVersion);
    }
    let nonce = reader.array()?;
    let sealed = reader.take(message.len() - 14)?;
    let mut body = vec![0; sealed.len().saturating_sub(AEAD_TAG_LEN)];
    libcrux_chacha20poly1305::decrypt(
        &keys.signal_key,
        &mut body,
        sealed,
        &[version, kind],
        &nonce,
    )
    .map_err(|_| Error::Authentication)?;
    Ok((kind, body))
}

struct Request<'a> {
    viewer: ViewerId,
    nonce: [u8; 32],
    not_after: u64,
    public_key: &'a [u8; PUBLIC_KEY_LEN],
}

impl<'a> Request<'a> {
    fn decode(body: &'a [u8]) -> Result<Self, Error> {
        let mut reader = Reader::new(body);
        let viewer = reader.array()?;
        let nonce = reader.array()?;
        let not_after = reader.u64()?;
        let public_key = reader.take(PUBLIC_KEY_LEN)?.try_into().expect("taken");
        reader.finish()?;
        Ok(Self {
            viewer,
            nonce,
            not_after,
            public_key,
        })
    }
}

pub(crate) fn transcript(request: &[u8], sharer_nonce: &[u8; 32]) -> [u8; 32] {
    sha256(&[
        label("transcript").as_bytes(),
        &[VERSION],
        &SUITE,
        request,
        sharer_nonce,
    ])
}

fn hpke_info(transcript: &[u8; 32]) -> Vec<u8> {
    [label("grant").as_bytes(), transcript].concat()
}

/// hpke-rs needs a PRNG type. This crate draws X-Wing's encapsulation randomness from
/// the caller's RNG itself, and the receiver and the key schedule draw none.
pub(crate) struct NoRandomness;

impl TryRng for NoRandomness {
    type Error = fmt::Error;

    fn try_next_u32(&mut self) -> Result<u32, fmt::Error> {
        Err(fmt::Error)
    }

    fn try_next_u64(&mut self) -> Result<u64, fmt::Error> {
        Err(fmt::Error)
    }

    fn try_fill_bytes(&mut self, _: &mut [u8]) -> Result<(), fmt::Error> {
        Err(fmt::Error)
    }
}

impl TryCryptoRng for NoRandomness {}

pub(crate) type HpkeContext = Context<HpkeLibcrux<NoRandomness>>;
/// `(psk, psk_id)` selects PSK mode; the KAT uses base mode.
pub(crate) type Psk<'a> = Option<(&'a [u8], &'a [u8])>;

fn hpke(psk: Psk<'_>) -> Hpke<HpkeLibcrux<NoRandomness>> {
    let mode = if psk.is_some() { Mode::Psk } else { Mode::Base };
    Hpke::new_with_rng(
        mode,
        KemAlgorithm::XWingDraft06,
        KdfAlgorithm::HkdfSha256,
        AeadAlgorithm::ChaCha20Poly1305,
        NoRandomness,
    )
}

/// X-Wing EncapsulateDerand with `eseed`, then the HPKE key schedule: what hpke-rs's
/// `setup_sender` does, with the caller's randomness. Returns `enc` and the context.
pub(crate) fn hpke_sender(
    public_key: &[u8],
    eseed: &[u8; 64],
    info: &[u8],
    psk: Psk<'_>,
) -> Result<(Vec<u8>, HpkeContext), Error> {
    let public = libcrux_kem::PublicKey::decode(XWingKemDraft06, public_key)
        .map_err(|_| Error::InvalidPublicKey)?;
    let (shared, enc) = public
        .encapsulate_derand(eseed)
        .map_err(|_| Error::InvalidPublicKey)?;
    let shared = shared.encode();
    let (key, id) = psk.unwrap_or_default();
    let context = hpke(psk)
        .key_schedule(&shared, info, key, id)
        .map_err(|_| Error::InvalidPublicKey)?;
    Ok((enc.encode(), context))
}

pub(crate) fn hpke_receiver(
    kem_seed: &[u8; 32],
    enc: &[u8],
    info: &[u8],
    psk: Psk<'_>,
) -> Result<HpkeContext, Error> {
    let private = HpkePrivateKey::new(kem_seed.to_vec());
    hpke(psk)
        .setup_receiver(
            enc,
            &private,
            info,
            psk.map(|p| p.0),
            psk.map(|p| p.1),
            None,
        )
        .map_err(|_| Error::Authentication)
}

fn export(context: &HpkeContext, name: &str) -> Result<[u8; 32], Error> {
    let bytes = context
        .export(label(name).as_bytes(), 32)
        .map_err(|_| Error::Authentication)?;
    Ok(bytes.try_into().expect("32 bytes"))
}

/// The next channel key; the caller drops the current one.
fn ratchet(channel: &[u8; 32]) -> [u8; 32] {
    hkdf_expand(channel, label("channel ratchet").as_bytes())
}

fn channel_nonce(counter: u64) -> [u8; 12] {
    let mut nonce = [0; 12];
    nonce[4..].copy_from_slice(&counter.to_be_bytes());
    nonce
}

fn rekey_header(viewer: &ViewerId, counter: u64) -> Vec<u8> {
    [&viewer[..], &counter.to_be_bytes()].concat()
}

// -------------------------------------------------------------------------------------
// Sharer

#[derive(Debug)]
pub enum SharerEvent {
    /// A viewer asked to join. The next [`Sharer::rekey`] grants it, unless
    /// [`Sharer::remove`] turns it down first.
    Requested(ViewerId),
    Confirmed(ViewerId),
}

/// What [`Sharer::rekey`] produces: send `messages`, each to its viewer, then switch to
/// `key` as the crate documentation describes.
#[derive(Debug)]
pub struct Rekey {
    pub key: EpochKey,
    pub messages: Vec<(ViewerId, Vec<u8>)>,
}

struct Pending {
    viewer: ViewerId,
    body: Vec<u8>,
    not_after: u64,
}

struct Granted {
    viewer: ViewerId,
    /// The key for Rekey number `counter + 1`.
    channel: [u8; 32],
    counter: u64,
    confirm: [u8; 32],
    confirmed: bool,
    /// The request's `not_after`: an unconfirmed viewer is dropped at the next rekey
    /// after it.
    not_after: u64,
}

pub struct Sharer {
    keys: LinkKeys,
    epoch: u32,
    last_rekey: Option<u64>,
    pending: Vec<Pending>,
    viewers: Vec<Granted>,
    seen: Vec<([u8; 32], u64)>,
}

impl Sharer {
    /// A sharer without an epoch; the first [`Sharer::rekey`] creates epoch 1.
    pub fn new(keys: LinkKeys) -> Self {
        Self {
            keys,
            epoch: 0,
            last_rekey: None,
            pending: Vec::new(),
            viewers: Vec::new(),
            seen: Vec::new(),
        }
    }

    /// Handles a KeyRequest or KeyAck.
    pub fn receive(&mut self, message: &[u8], now: u64) -> Result<SharerEvent, Error> {
        let (kind, body) = open_message(&self.keys, message)?;
        match kind {
            REQUEST => self.request(body, now),
            ACK => self.ack(&body),
            _ => Err(Error::Unexpected),
        }
    }

    fn request(&mut self, body: Vec<u8>, now: u64) -> Result<SharerEvent, Error> {
        let request = Request::decode(&body)?;
        if request.not_after < now || request.not_after > now.saturating_add(MAX_NOT_AFTER_MS) {
            return Err(Error::Expired);
        }
        libcrux_kem::PublicKey::decode(XWingKemDraft06, request.public_key)
            .map_err(|_| Error::InvalidPublicKey)?;
        self.seen.retain(|(_, not_after)| *not_after >= now);
        if self.seen.iter().any(|(nonce, _)| *nonce == request.nonce)
            || self.pending.iter().any(|p| p.viewer == request.viewer)
            || self.viewers.iter().any(|v| v.viewer == request.viewer)
        {
            return Err(Error::Replay);
        }
        if self.seen.len() >= MAX_SEEN
            || self.pending.len() >= MAX_PENDING
            || self.pending.len() + self.viewers.len() >= MAX_VIEWERS
        {
            return Err(Error::LimitReached);
        }
        self.seen.push((request.nonce, request.not_after));
        let viewer = request.viewer;
        let not_after = request.not_after;
        self.pending.push(Pending {
            viewer,
            body,
            not_after,
        });
        Ok(SharerEvent::Requested(viewer))
    }

    fn ack(&mut self, body: &[u8]) -> Result<SharerEvent, Error> {
        let mut reader = Reader::new(body);
        let viewer: ViewerId = reader.array()?;
        let confirm: [u8; 32] = reader.array()?;
        reader.finish()?;
        let granted = self
            .viewers
            .iter_mut()
            .find(|granted| granted.viewer == viewer)
            .ok_or(Error::UnknownViewer)?;
        if granted.confirmed {
            return Err(Error::Replay);
        }
        if !bool::from(granted.confirm.ct_eq(&confirm)) {
            return Err(Error::Authentication);
        }
        granted.confirmed = true;
        Ok(SharerEvent::Confirmed(viewer))
    }

    /// Turns down a request or removes a viewer; the next [`Sharer::rekey`] locks it
    /// out. Returns whether it was known.
    pub fn remove(&mut self, viewer: &ViewerId) -> bool {
        let before = self.pending.len() + self.viewers.len();
        self.pending.retain(|pending| pending.viewer != *viewer);
        self.viewers.retain(|granted| granted.viewer != *viewer);
        before != self.pending.len() + self.viewers.len()
    }

    /// Starts the next epoch: a Rekey for every viewer, then a KeyGrant for every
    /// pending request. Viewers whose confirmation is overdue and expired requests are
    /// dropped first. Refused within
    /// [`MIN_EPOCH_INTERVAL_MS`] of the last rekey, unless the clock stepped back.
    pub fn rekey<R: TryCryptoRng + ?Sized>(
        &mut self,
        now: u64,
        rng: &mut R,
    ) -> Result<Rekey, Error> {
        if let Some(last) = self.last_rekey
            && (last..last.saturating_add(MIN_EPOCH_INTERVAL_MS)).contains(&now)
        {
            return Err(Error::TooSoon);
        }
        let epoch = self.epoch.checked_add(1).ok_or(Error::Exhausted)?;
        let key = EpochKey {
            epoch,
            key: random(rng)?,
        };
        self.viewers
            .retain(|granted| granted.confirmed || granted.not_after >= now);
        self.pending.retain(|pending| pending.not_after >= now);
        if self
            .viewers
            .iter()
            .any(|granted| granted.counter == u64::MAX)
        {
            return Err(Error::Exhausted);
        }

        let plaintext = encode_epoch_key(&key);
        let mut grants = Vec::new();
        let mut granted = Vec::new();
        for pending in &self.pending {
            let (message, viewer) = self.grant(pending, &plaintext, rng)?;
            grants.push((pending.viewer, message));
            granted.push(viewer);
        }
        let mut messages = Vec::with_capacity(self.viewers.len() + grants.len());
        for viewer in &mut self.viewers {
            viewer.counter += 1;
            let header = rekey_header(&viewer.viewer, viewer.counter);
            let mut sealed = [0; SEALED_KEY_LEN];
            libcrux_chacha20poly1305::encrypt(
                &viewer.channel,
                &plaintext,
                &mut sealed,
                &header,
                &channel_nonce(viewer.counter),
            )
            .expect("buffer sized for key and tag");
            viewer.channel = ratchet(&viewer.channel);
            let body = [&header[..], &sealed].concat();
            messages.push((viewer.viewer, seal_message(&self.keys, REKEY, &body, rng)?));
        }
        messages.extend(grants);
        self.pending.clear();
        self.viewers.extend(granted);
        self.epoch = epoch;
        self.last_rekey = Some(now);
        Ok(Rekey { key, messages })
    }

    fn grant<R: TryCryptoRng + ?Sized>(
        &self,
        pending: &Pending,
        epoch_key: &[u8],
        rng: &mut R,
    ) -> Result<(Vec<u8>, Granted), Error> {
        let request = Request::decode(&pending.body)?;
        let sharer_nonce: [u8; 32] = random(rng)?;
        let eseed = random::<64, R>(rng)?;
        let transcript = transcript(&pending.body, &sharer_nonce);
        let psk_id = self.keys.psk_id();
        let (enc, mut context) = hpke_sender(
            request.public_key,
            &eseed,
            &hpke_info(&transcript),
            Some((&self.keys.psk, &psk_id)),
        )?;
        let sealed = context
            .seal(&[], epoch_key)
            .map_err(|_| Error::Authentication)?;
        let body = [&pending.viewer[..], &sharer_nonce, &enc, &sealed].concat();
        let granted = Granted {
            viewer: pending.viewer,
            channel: export(&context, "channel sharer->viewer")?,
            counter: 0,
            confirm: export(&context, "confirm viewer->sharer")?,
            confirmed: false,
            not_after: pending.not_after,
        };
        Ok((seal_message(&self.keys, GRANT, &body, rng)?, granted))
    }
}

impl fmt::Debug for Sharer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Sharer")
            .field("epoch", &self.epoch)
            .field("pending", &self.pending.len())
            .field("viewers", &self.viewers.len())
            .finish_non_exhaustive()
    }
}

// -------------------------------------------------------------------------------------
// Viewer

#[derive(Debug)]
pub enum ViewerEvent {
    /// The first key; send `ack` to the sharer.
    Granted {
        key: EpochKey,
        ack: Vec<u8>,
    },
    Rekeyed(EpochKey),
}

enum State {
    Joining {
        request: Vec<u8>,
        not_after: u64,
        kem_seed: [u8; 32],
    },
    /// Holds the key for Rekey number `counter + 1`, and nothing that opens the grant.
    Granted {
        channel: [u8; 32],
        counter: u64,
        epoch: u32,
    },
}

/// One join: a fresh X-Wing key pair, used for this request only.
pub struct Viewer {
    keys: LinkKeys,
    id: ViewerId,
    state: State,
}

impl Viewer {
    /// Draws the viewer id, nonce and X-Wing key pair, and seals the KeyRequest, valid
    /// for [`REQUEST_LIFETIME_MS`].
    pub fn join<R: TryCryptoRng + ?Sized>(
        keys: LinkKeys,
        now: u64,
        rng: &mut R,
    ) -> Result<(Self, Vec<u8>), Error> {
        let id: ViewerId = random(rng)?;
        let nonce: [u8; 32] = random(rng)?;
        let kem_seed: [u8; 32] = random(rng)?;
        let (_, public) = libcrux_kem::key_gen_derand(XWingKemDraft06, &kem_seed)
            .map_err(|_| Error::Randomness)?;
        let not_after = now.saturating_add(REQUEST_LIFETIME_MS);
        let request = [&id[..], &nonce, &not_after.to_be_bytes(), &public.encode()].concat();
        let message = seal_message(&keys, REQUEST, &request, rng)?;
        let viewer = Self {
            keys,
            id,
            state: State::Joining {
                request,
                not_after,
                kem_seed,
            },
        };
        Ok((viewer, message))
    }

    pub fn id(&self) -> ViewerId {
        self.id
    }

    /// Handles a KeyGrant or Rekey. Messages for other viewers fail
    /// with [`Error::UnknownViewer`].
    pub fn receive<R: TryCryptoRng + ?Sized>(
        &mut self,
        message: &[u8],
        now: u64,
        rng: &mut R,
    ) -> Result<ViewerEvent, Error> {
        let (kind, body) = open_message(&self.keys, message)?;
        let mut reader = Reader::new(&body);
        if reader.array::<16>()? != self.id {
            return Err(Error::UnknownViewer);
        }
        match (kind, &self.state) {
            (GRANT, State::Joining { .. }) => self.grant(reader, now, rng),
            (REKEY, State::Granted { .. }) => self.rekey(reader),
            _ => Err(Error::Unexpected),
        }
    }

    fn grant<R: TryCryptoRng + ?Sized>(
        &mut self,
        mut reader: Reader<'_>,
        now: u64,
        rng: &mut R,
    ) -> Result<ViewerEvent, Error> {
        let State::Joining {
            request,
            not_after,
            kem_seed,
        } = &self.state
        else {
            return Err(Error::Unexpected);
        };
        let sharer_nonce = reader.array()?;
        let enc = reader.take(ENC_LEN)?;
        let sealed = reader.take(SEALED_KEY_LEN)?;
        reader.finish()?;
        if now > *not_after {
            return Err(Error::Expired);
        }
        let transcript = transcript(request, &sharer_nonce);
        let psk_id = self.keys.psk_id();
        let mut context = hpke_receiver(
            kem_seed,
            enc,
            &hpke_info(&transcript),
            Some((&self.keys.psk, &psk_id)),
        )?;
        let plaintext = context
            .open(&[], sealed)
            .map_err(|_| Error::Authentication)?;
        let key = decode_epoch_key(&plaintext)?;
        let confirm = export(&context, "confirm viewer->sharer")?;
        let ack = [&self.id[..], &confirm].concat();
        let ack = seal_message(&self.keys, ACK, &ack, rng)?;
        self.state = State::Granted {
            channel: export(&context, "channel sharer->viewer")?,
            counter: 0,
            epoch: key.epoch,
        };
        Ok(ViewerEvent::Granted { key, ack })
    }

    fn rekey(&mut self, mut reader: Reader<'_>) -> Result<ViewerEvent, Error> {
        let State::Granted {
            channel,
            counter: last,
            epoch,
        } = &mut self.state
        else {
            return Err(Error::Unexpected);
        };
        let counter = reader.u64()?;
        let sealed = reader.take(SEALED_KEY_LEN)?;
        reader.finish()?;
        if counter <= *last {
            return Err(Error::Replay);
        }
        if counter - *last - 1 > MAX_MISSED_REKEYS {
            return Err(Error::LimitReached);
        }
        let mut skipped = None;
        for _ in *last + 1..counter {
            skipped = Some(ratchet(skipped.as_ref().unwrap_or(channel)));
        }
        let key = skipped.as_ref().unwrap_or(channel);
        let mut plaintext = [0; EPOCH_KEY_LEN];
        libcrux_chacha20poly1305::decrypt(
            key,
            &mut plaintext,
            sealed,
            &rekey_header(&self.id, counter),
            &channel_nonce(counter),
        )
        .map_err(|_| Error::Authentication)?;
        let received = decode_epoch_key(&plaintext)?;
        if received.epoch <= *epoch {
            return Err(Error::StaleEpoch);
        }
        let next = ratchet(key);
        *channel = next;
        *last = counter;
        *epoch = received.epoch;
        Ok(ViewerEvent::Rekeyed(received))
    }
}

impl fmt::Debug for Viewer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Viewer")
            .field("id", &self.id)
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tests::{T0, join, link, rng, sharer};

    #[test]
    fn epochs_and_counters_are_bounded() {
        let link = link(40);
        let mut sharer = sharer(&link);
        sharer.epoch = u32::MAX;
        assert_eq!(sharer.rekey(T0, &mut rng(1)).err(), Some(Error::Exhausted));

        let mut sharer = self::sharer(&link);
        join(&link, &mut sharer, T0, 41);
        sharer.viewers[0].counter = u64::MAX;
        assert_eq!(
            sharer.rekey(T0 + 1000, &mut rng(2)).err(),
            Some(Error::Exhausted)
        );
    }

    /// L2: after a Rekey both sides hold only the next channel key, which opens no
    /// earlier Rekey, and the viewer keeps nothing that opens its grant.
    #[test]
    fn channel_keys_ratchet_forward() {
        let link = link(42);
        let keys = link.keys();
        let mut sharer = sharer(&link);
        let (mut viewer, _, _) = join(&link, &mut sharer, T0, 43);
        let mut rng = rng(44);
        let mut sealed = Vec::new();
        for now in [T0 + 1000, T0 + 2000] {
            let message = sharer.rekey(now, &mut rng).unwrap().messages.remove(0).1;
            assert!(viewer.receive(&message, now, &mut rng).is_ok());
            let (_, body) = open_message(&keys, &message).unwrap();
            sealed.push((
                u64::from_be_bytes(body[16..24].try_into().unwrap()),
                body[24..].to_vec(),
            ));
        }
        let State::Granted { channel, .. } = &viewer.state else {
            panic!("granted");
        };
        assert_eq!(channel, &sharer.viewers[0].channel);
        for (counter, sealed) in &sealed {
            let mut plaintext = [0; EPOCH_KEY_LEN];
            let opened = libcrux_chacha20poly1305::decrypt(
                channel,
                &mut plaintext,
                sealed,
                &rekey_header(&viewer.id, *counter),
                &channel_nonce(*counter),
            );
            assert!(opened.is_err(), "counter {counter}");
        }
    }
}
