// SPDX-License-Identifier: EUPL-1.2
//! Known answers, the full exchange, and malformed input through the real parsers.

use hpke_rs::hpke_types::{AeadAlgorithm, KdfAlgorithm, KemAlgorithm};
use hpke_rs::{Hpke, Mode};
use hpke_rs_libcrux::{HpkeLibcrux, HpkeLibcruxPrng};
use libcrux_kem::Algorithm::XWingKemDraft06;

use crate::exchange::{
    ACK, GRANT, MAX_PENDING, MAX_SEEN, MAX_SUBSCRIBERS, REKEY, REQUEST, REQUEST_LIFETIME_MS,
    hpke_receiver, hpke_sender, open_message, seal_message, transcript,
};
use crate::*;

pub(crate) fn hex(text: &str) -> Vec<u8> {
    assert!(text.len().is_multiple_of(2), "odd hex length");
    (0..text.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&text[i..i + 2], 16).expect("hex"))
        .collect()
}

pub(crate) fn rng(seed: u8) -> HpkeLibcruxPrng {
    HpkeLibcruxPrng::from_seed([seed; 32])
}

pub(crate) const HOST: &str = "stream.nuschl.eu";
pub(crate) const T0: u64 = 1_800_000_000_000;

pub(crate) fn link(seed: u8) -> Link {
    Link::generate(HOST, &mut rng(seed)).unwrap()
}

pub(crate) fn publisher(link: &Link) -> Publisher {
    Publisher::new(link.keys())
}

pub(crate) fn request<R: rand_core::TryCryptoRng>(
    link: &Link,
    now: u64,
    rng: &mut R,
) -> (Subscriber, Vec<u8>) {
    Subscriber::join(link.keys(), now, rng).unwrap()
}

pub(crate) fn admit(publisher: &mut Publisher, request: &[u8], now: u64) -> SubscriberId {
    let PublisherEvent::Requested(subscriber) = publisher.receive(request, now).unwrap() else {
        panic!("expected a request");
    };
    subscriber
}

/// Runs one join to a confirmed key; returns the subscriber, its key and the rekey's other
/// messages.
pub(crate) fn join(
    link: &Link,
    publisher: &mut Publisher,
    now: u64,
    seed: u8,
) -> (Subscriber, EpochKey, Vec<(SubscriberId, Vec<u8>)>) {
    let mut rng = rng(seed);
    let (mut subscriber, request) = request(link, now, &mut rng);
    admit(publisher, &request, now);
    let mut others = Vec::new();
    let mut key = None;
    for (to, message) in publisher.rekey(now, &mut rng).unwrap().messages {
        if to != subscriber.id() {
            others.push((to, message));
            continue;
        }
        let SubscriberEvent::Granted { key: granted, ack } =
            subscriber.receive(&message, now, &mut rng).unwrap()
        else {
            panic!("expected a grant");
        };
        let confirmed = publisher.receive(&ack, now).unwrap();
        assert!(matches!(confirmed, PublisherEvent::Confirmed(id) if id == subscriber.id()));
        key = Some(granted);
    }
    (
        subscriber,
        key.expect("a grant for the new subscriber"),
        others,
    )
}

fn rekeyed(subscriber: &mut Subscriber, message: &[u8], now: u64) -> Result<EpochKey, Error> {
    match subscriber.receive(message, now, &mut rng(99))? {
        SubscriberEvent::Rekeyed(key) => Ok(key),
        other => panic!("expected a rekey, got {other:?}"),
    }
}

fn vector_fields(text: &str) -> Vec<(&str, Vec<u8>)> {
    text.lines()
        .filter(|line| !line.starts_with('#'))
        .map(|line| {
            line.split_once(' ')
                .map(|(name, value)| (name, hex(value)))
                .unwrap()
        })
        .collect()
}

// -------------------------------------------------------------------------------------
// Known answers

#[test]
fn xwing_draft_06_vectors() {
    let fields = vector_fields(include_str!("../test-vectors/xwing-draft-06.txt"));
    assert_eq!(fields.len(), 18);
    for vector in fields.chunks(6) {
        let field = |name| &vector.iter().find(|(n, _)| *n == name).unwrap().1;
        let (private, public) =
            libcrux_kem::key_gen_derand(XWingKemDraft06, field("seed")).unwrap();
        assert_eq!(&public.encode(), field("pk"));
        let (shared, ciphertext) = public.encapsulate_derand(field("eseed")).unwrap();
        assert_eq!(&ciphertext.encode(), field("ct"));
        assert_eq!(&shared.encode(), field("ss"));
        assert_eq!(
            &ciphertext.decapsulate(&private).unwrap().encode(),
            field("ss")
        );
    }
}

/// draft-ietf-hpke-pq: X-Wing in HPKE with this crate's KDF and AEAD, base mode, through
/// the crate's own sender and receiver.
#[test]
fn hpke_pq_xwing_vector() {
    let fields = vector_fields(include_str!("../test-vectors/hpke-pq-xwing.txt"));
    let field = |name| &fields.iter().find(|(n, _)| *n == name).unwrap().1;
    let all = |name| {
        fields
            .iter()
            .filter(move |(n, _)| *n == name)
            .map(|(_, v)| v)
    };
    let eseed: [u8; 64] = field("ikmE")[..].try_into().unwrap();
    let (enc, mut sender) = hpke_sender(field("pkRm"), &eseed, field("info"), None).unwrap();
    assert_eq!(&enc, field("enc"));
    let skr: [u8; 32] = field("skRm")[..].try_into().unwrap();
    let mut receiver = hpke_receiver(&skr, &enc, field("info"), None).unwrap();
    for (aad, ciphertext) in all("aad").zip(all("ct")) {
        assert_eq!(&sender.seal(aad, field("pt")).unwrap(), ciphertext);
        assert_eq!(&receiver.open(aad, ciphertext).unwrap(), field("pt"));
    }
    for (context, value) in all("exporter_context").zip(all("exported_value")) {
        assert_eq!(&sender.export(context, 32).unwrap(), value);
        assert_eq!(&receiver.export(context, 32).unwrap(), value);
    }
}

/// RFC 9180 A.2.2: the PSK-mode key schedule, AEAD and exporter of the suite's KDF and
/// AEAD, through the same hpke-rs and libcrux code.
#[test]
fn rfc9180_psk_mode_vector() {
    let hpke = Hpke::<HpkeLibcrux>::new_with_rng(
        Mode::Psk,
        KemAlgorithm::DhKem25519,
        KdfAlgorithm::HkdfSha256,
        AeadAlgorithm::ChaCha20Poly1305,
        rng(0),
    );
    let mut context = hpke
        .key_schedule(
            &hex("4be079c5e77779d0215b3f689595d59e3e9b0455d55662d1f3666ec606e50ea7"),
            &hex("4f6465206f6e2061204772656369616e2055726e"),
            &hex("0247fd33b913760fa1fa51e1892d9f307fbe65eb171e8132c2af18555a738b82"),
            &hex("456e6e796e20447572696e206172616e204d6f726961"),
        )
        .unwrap();
    let plaintext = hex("4265617574792069732074727574682c20747275746820626561757479");
    for (aad, ciphertext) in [
        (
            "436f756e742d30",
            "4a177f9c0d6f15cfdf533fb65bf84aecdc6ab16b8b85b4cf65a370e07fc1d78d28fb073214525276f4a89608ff",
        ),
        (
            "436f756e742d31",
            "5c3cabae2f0b3e124d8d864c116fd8f20f3f56fda988c3573b40b09997fd6c769e77c8eda6cda4f947f5b704a8",
        ),
    ] {
        assert_eq!(
            context.seal(&hex(aad), &plaintext).unwrap(),
            hex(ciphertext)
        );
    }
    for (exporter_context, value) in [
        (
            "",
            "813c1bfc516c99076ae0f466671f0ba5ff244a41699f7b2417e4c59d46d39f40",
        ),
        (
            "00",
            "2745cf3d5bb65c333658732954ee7af49eb895ce77f8022873a62a13c94cb4e1",
        ),
        (
            "54657374436f6e74657874",
            "ad40e3ae14f21c99bfdebc20ae14ab86f4ca2dc9a4799d200f43a25f99fa78ae",
        ),
    ] {
        assert_eq!(
            context.export(&hex(exporter_context), 32).unwrap(),
            hex(value)
        );
    }
}

/// The key schedule against an HKDF written separately over Python's hmac module.
#[test]
fn stream_id_known_answer() {
    let link = Link::parse(
        "https://stream.nuschl.eu/watch#v=1&k=AAECAwQFBgcICQoLDA0ODxAREhMUFRYXGBkaGxwdHh8",
        HOST,
    )
    .unwrap();
    assert_eq!(link.secret(), &core::array::from_fn(|i| i as u8));
    assert_eq!(
        link.keys().stream_id().to_vec(),
        hex("6e97e9849dc0269b159081f2dcfb96dc")
    );
}

// -------------------------------------------------------------------------------------
// Links

#[test]
fn links_round_trip_and_keep_secrets_out_of_debug() {
    let call = link(3);
    let parsed = Link::parse(&call.call_url().unwrap(), HOST).unwrap();
    assert_eq!(parsed.call_url(), call.call_url());
    let watch = Link::parse(&call.watch_url(), HOST).unwrap();
    assert_eq!(watch.secret(), call.secret());
    assert_eq!(watch.publisher(), None);
    assert_eq!(watch.call_url(), None);
    assert_eq!(watch.keys().stream_id(), call.keys().stream_id());
    assert_ne!(link(4).keys().stream_id(), call.keys().stream_id());

    let url = call.call_url().unwrap();
    let encoded_secret = url.split("k=").nth(1).unwrap().split('&').next().unwrap();
    for debug in [format!("{call:?}"), format!("{:?}", call.keys())] {
        assert!(!debug.contains(encoded_secret), "{debug}");
    }
}

#[test]
fn links_reject_every_other_form() {
    let k = "AAECAwQFBgcICQoLDA0ODxAREhMUFRYXGBkaGxwdHh8";
    let good = format!("https://stream.nuschl.eu/watch#v=1&k={k}");
    assert!(Link::parse(&good, HOST).is_ok());
    let with_port = format!("https://stream.nuschl.eu:8443/watch#v=1&k={k}");
    assert!(Link::parse(&with_port, "stream.nuschl.eu:8443").is_ok());
    for (link, error) in [
        (with_port.clone(), Error::Malformed("link origin")),
        (
            format!("http://stream.nuschl.eu/watch#v=1&k={k}"),
            Error::Malformed("link origin"),
        ),
        (
            format!("https://Stream.nuschl.eu/watch#v=1&k={k}"),
            Error::Malformed("link origin"),
        ),
        (
            format!("https://evil.example/watch#v=1&k={k}"),
            Error::Malformed("link origin"),
        ),
        (
            format!("https://stream.nuschl.eu.evil/watch#v=1&k={k}"),
            Error::Malformed("link origin"),
        ),
        (
            format!("https://stream.nuschl.eu/a/watch#v=1&k={k}"),
            Error::Malformed("link page"),
        ),
        (
            format!("https://stream.nuschl.eu/view#v=1&k={k}"),
            Error::Malformed("link page"),
        ),
        (
            "https://stream.nuschl.eu/watch".to_owned(),
            Error::Malformed("link fragment"),
        ),
        (
            format!("https://stream.nuschl.eu/watch#v=2&k={k}"),
            Error::UnsupportedVersion,
        ),
        (
            format!("https://stream.nuschl.eu/watch#k={k}&v=1"),
            Error::Malformed("link version"),
        ),
        (
            format!("https://stream.nuschl.eu/watch#v=1&k={k}&p={k}"),
            Error::Malformed("link parameters"),
        ),
        (
            format!("https://stream.nuschl.eu/call#v=1&k={k}"),
            Error::Malformed("link parameters"),
        ),
        (
            format!("https://stream.nuschl.eu/watch#v=1&k={k}A"),
            Error::Malformed("link secret"),
        ),
        (
            format!("https://stream.nuschl.eu/watch#v=1&k={}", &k[1..]),
            Error::Malformed("link secret"),
        ),
        // The last character carries 4 bits; nonzero padding bits are not canonical.
        (
            format!("https://stream.nuschl.eu/watch#v=1&k={}9", &k[..42]),
            Error::Malformed("link secret"),
        ),
        (
            format!("https://stream.nuschl.eu/watch#v=1&k={}", "a".repeat(500)),
            Error::TooLong,
        ),
    ] {
        assert_eq!(Link::parse(&link, HOST).err(), Some(error), "{link}");
    }
    for host in [
        "Stream.nuschl.eu",
        "A]:",
        "[::1]",
        "host:",
        "host:123456",
        "a..b",
        "",
        "a/b",
    ] {
        assert_eq!(
            Link::parse(&good, host).err(),
            Some(Error::Malformed("link host")),
            "{host}"
        );
        assert!(Link::generate(host, &mut rng(1)).is_err(), "{host}");
    }
}

// -------------------------------------------------------------------------------------
// The exchange

#[test]
fn subscribers_get_only_fresh_epochs_and_removed_subscribers_are_locked_out() {
    let link = link(5);
    let mut publisher = publisher(&link);
    let (mut first, key_1, _) = join(&link, &mut publisher, T0, 10);
    assert_eq!(key_1.epoch, 1);

    // A second join moves the first subscriber to epoch 2 in the same rekey.
    let (mut second, key_2, others) = join(&link, &mut publisher, T0 + 1000, 11);
    assert_eq!(key_2.epoch, 2);
    assert_eq!(others.len(), 1);
    assert_eq!(others[0].0, first.id());
    let first_2 = rekeyed(&mut first, &others[0].1, T0 + 1000).unwrap();
    assert_eq!(&first_2.key, &key_2.key);
    assert_ne!(&key_1.key, &key_2.key);

    // Removing the first subscriber: the next epoch goes to the second only, and the first
    // cannot open the second's Rekey.
    assert!(publisher.remove(&first.id()));
    let rekey = publisher.rekey(T0 + 2000, &mut rng(12)).unwrap();
    assert_eq!(rekey.key.epoch, 3);
    assert_eq!(rekey.messages.len(), 1);
    assert_eq!(rekey.messages[0].0, second.id());
    let key_3 = rekeyed(&mut second, &rekey.messages[0].1, T0 + 2000).unwrap();
    assert_eq!(&key_3.key, &rekey.key.key);
    assert_eq!(
        first
            .receive(&rekey.messages[0].1, T0 + 2000, &mut rng(13))
            .err(),
        Some(Error::UnknownSubscriber)
    );

    // A request removed before the rekey gets no grant.
    let (_, request) = self::request(&link, T0 + 3000, &mut rng(14));
    let turned_down = admit(&mut publisher, &request, T0 + 3000);
    assert!(publisher.remove(&turned_down));
    let rekey = publisher.rekey(T0 + 3000, &mut rng(15)).unwrap();
    assert_eq!(rekey.messages.len(), 1);
    assert_eq!(rekey.messages[0].0, second.id());
}

/// A party with S and every message cannot open a grant: the X-Wing secret is the
/// subscriber's alone, and the epoch keys appear in no message, even with every signal
/// envelope opened.
#[test]
fn passive_link_holder_cannot_decrypt() {
    let link = link(6);
    let mut publisher = publisher(&link);
    let mut rng = rng(12);
    let (mut subscriber, request) = request(&link, T0, &mut rng);
    admit(&mut publisher, &request, T0);
    let first = publisher.rekey(T0, &mut rng).unwrap();
    let grant = first.messages[0].1.clone();
    let SubscriberEvent::Granted { key, ack } = subscriber.receive(&grant, T0, &mut rng).unwrap()
    else {
        panic!("expected a grant");
    };
    let second = publisher.rekey(T0 + 1000, &mut rng).unwrap();
    let rekey = second.messages[0].1.clone();
    let next = rekeyed(&mut subscriber, &rekey, T0 + 1000).unwrap();

    let keys = link.keys();
    let (_, request_body) = open_message(&keys, &request).unwrap();
    let (_, grant_body) = open_message(&keys, &grant).unwrap();
    let mut recordings = vec![request.clone(), grant.clone(), ack, rekey];
    for message in recordings.clone() {
        recordings.push(open_message(&keys, &message).unwrap().1);
    }
    for epoch_key in [&key.key, &next.key] {
        for recording in &recordings {
            assert!(!recording.windows(32).any(|window| window == epoch_key));
        }
    }

    // Everything but sk_v: the transcript, info, psk and enc are all known.
    let transcript = transcript(&request_body, grant_body[16..48].try_into().unwrap());
    let info = [b"nuschl/stream/v2 grant".as_slice(), &transcript].concat();
    let enc = &grant_body[48..48 + 1120];
    let sealed = &grant_body[48 + 1120..];
    let psk_id = [b"nuschl/stream/v2".as_slice(), keys.stream_id()].concat();
    let psk = Some((keys.psk.as_slice(), psk_id.as_slice()));
    for guess in [[0u8; 32], [0xff; 32], *link.secret(), keys.psk] {
        let opened = hpke_receiver(&guess, enc, &info, psk)
            .and_then(|mut context| context.open(&[], sealed).map_err(|_| Error::Authentication));
        assert!(opened.is_err());
    }
}

/// L1, pinned: PSK mode authenticates a grant as "from a holder of S". A second party
/// with the link can grant a subscriber its own key; media authenticity rests on the SFU's
/// P-based publish check (E1) and on identity-bound publishers (N1).
#[test]
fn any_link_holder_can_act_as_publisher() {
    let link = link(34);
    let mut genuine = publisher(&link);
    let mut impostor = publisher(&link);
    let mut rng = rng(35);
    let (mut subscriber, request) = request(&link, T0, &mut rng);
    genuine.receive(&request, T0).unwrap();
    admit(&mut impostor, &request, T0);
    let forged = impostor.rekey(T0, &mut rng).unwrap();
    let SubscriberEvent::Granted { key, .. } = subscriber
        .receive(&forged.messages[0].1, T0, &mut rng)
        .unwrap()
    else {
        panic!("the forged grant is accepted");
    };
    assert_eq!(&key.key, &forged.key.key);
}

#[test]
fn parties_without_the_link_secret_are_rejected() {
    let link = link(7);
    let other = self::link(8);
    let mut publisher = publisher(&link);
    let mut rng = rng(13);
    let (mut outsider, request) = self::request(&other, T0, &mut rng);
    assert_eq!(
        publisher.receive(&request, T0).err(),
        Some(Error::Authentication)
    );

    // An interceptor relaying between two links: the publisher's grant does not open under
    // the other link's keys.
    let (_, request) = self::request(&link, T0, &mut rng);
    admit(&mut publisher, &request, T0);
    let grant = publisher.rekey(T0, &mut rng).unwrap().messages.remove(0).1;
    assert_eq!(
        outsider.receive(&grant, T0, &mut rng).err(),
        Some(Error::Authentication)
    );
}

#[test]
fn replays_reflections_and_bad_confirmations_fail() {
    let link = link(9);
    let keys = link.keys();
    let mut publisher = publisher(&link);
    let mut rng = rng(14);
    let (mut subscriber, request) = request(&link, T0, &mut rng);
    admit(&mut publisher, &request, T0);
    assert_eq!(publisher.receive(&request, T0).err(), Some(Error::Replay));
    // Reflected to the subscriber itself.
    assert_eq!(
        subscriber.receive(&request, T0, &mut rng).err(),
        Some(Error::Unexpected)
    );

    let grant = publisher.rekey(T0, &mut rng).unwrap().messages.remove(0).1;
    assert_eq!(publisher.receive(&grant, T0).err(), Some(Error::Unexpected));
    // An ack carrying bytes other than the exported confirmation.
    let forged = seal_message(
        &keys,
        ACK,
        &[&subscriber.id()[..], &[0; 32]].concat(),
        &mut rng,
    )
    .unwrap();
    assert_eq!(
        publisher.receive(&forged, T0).err(),
        Some(Error::Authentication)
    );

    let SubscriberEvent::Granted { ack, .. } = subscriber.receive(&grant, T0, &mut rng).unwrap()
    else {
        panic!("expected a grant");
    };
    assert_eq!(
        subscriber.receive(&grant, T0, &mut rng).err(),
        Some(Error::Unexpected)
    );
    publisher.receive(&ack, T0).unwrap();
    assert_eq!(publisher.receive(&ack, T0).err(), Some(Error::Replay));
}

/// L4: a rekey's messages may be sent twice; a subscriber that misses one catches up with
/// the next, and duplicates and older ones are replays.
#[test]
fn resends_and_missed_rekeys() {
    let link = link(36);
    let mut publisher = publisher(&link);
    let (mut subscriber, _, _) = join(&link, &mut publisher, T0, 37);
    let mut rng = rng(38);
    let lost = publisher
        .rekey(T0 + 1000, &mut rng)
        .unwrap()
        .messages
        .remove(0)
        .1;
    let next = publisher.rekey(T0 + 2000, &mut rng).unwrap();
    let message = &next.messages[0].1;
    let key = rekeyed(&mut subscriber, message, T0 + 2000).unwrap();
    assert_eq!(key.epoch, 3);
    assert_eq!(&key.key, &next.key.key);
    assert_eq!(
        rekeyed(&mut subscriber, message, T0 + 2000).err(),
        Some(Error::Replay)
    );
    assert_eq!(
        rekeyed(&mut subscriber, &lost, T0 + 2000).err(),
        Some(Error::Replay)
    );
}

#[test]
fn time_limits_and_clock_steps() {
    let link = link(15);
    let mut publisher = publisher(&link);
    let mut rng = rng(16);

    // Expired, or dated further ahead than a minute of clock difference allows.
    let (_, request) = self::request(&link, T0, &mut rng);
    assert_eq!(
        publisher
            .receive(&request, T0 + REQUEST_LIFETIME_MS + 1)
            .err(),
        Some(Error::Expired)
    );
    let (_, request) = self::request(&link, T0 + 2 * REQUEST_LIFETIME_MS, &mut rng);
    assert_eq!(
        publisher.receive(&request, T0 - 1).err(),
        Some(Error::Expired)
    );

    // A grant after the request's lifetime, and a subscriber that never confirms.
    let (mut slow, request) = self::request(&link, T0, &mut rng);
    admit(&mut publisher, &request, T0);
    let grant = publisher.rekey(T0, &mut rng).unwrap().messages.remove(0).1;
    assert_eq!(
        publisher.rekey(T0 + 999, &mut rng).err(),
        Some(Error::TooSoon)
    );
    assert_eq!(
        slow.receive(&grant, T0 + REQUEST_LIFETIME_MS + 1, &mut rng)
            .err(),
        Some(Error::Expired)
    );
    let rekey = publisher
        .rekey(T0 + REQUEST_LIFETIME_MS + 1, &mut rng)
        .unwrap();
    assert!(rekey.messages.is_empty());

    // L3: after the clock steps back an hour, a removal can still rekey at once.
    let now = T0 + REQUEST_LIFETIME_MS + 1 - 3_600_000;
    assert!(publisher.rekey(now, &mut rng).is_ok());
    assert_eq!(
        publisher.rekey(now + 1, &mut rng).err(),
        Some(Error::TooSoon)
    );
}

#[test]
fn caps() {
    let link = link(17);
    let mut rng = rng(18);

    // At most MAX_PENDING requests wait for a rekey.
    let mut publisher = publisher(&link);
    for _ in 0..MAX_PENDING {
        let (_, request) = request(&link, T0, &mut rng);
        publisher.receive(&request, T0).unwrap();
    }
    let (_, extra) = request(&link, T0, &mut rng);
    assert_eq!(
        publisher.receive(&extra, T0).err(),
        Some(Error::LimitReached)
    );

    // The nonce cache holds MAX_SEEN requests until their not_after, removed or not.
    let mut publisher = self::publisher(&link);
    for _ in 0..MAX_SEEN {
        let (subscriber, request) = request(&link, T0, &mut rng);
        publisher.receive(&request, T0).unwrap();
        publisher.remove(&subscriber.id());
    }
    assert_eq!(
        publisher.receive(&extra, T0).err(),
        Some(Error::LimitReached)
    );
    let (_, later) = request(&link, T0 + REQUEST_LIFETIME_MS + 1, &mut rng);
    assert!(
        publisher
            .receive(&later, T0 + REQUEST_LIFETIME_MS + 1)
            .is_ok()
    );

    // At most MAX_SUBSCRIBERS subscribers and pending requests together.
    let mut publisher = self::publisher(&link);
    let mut now = T0;
    for _ in 0..MAX_SUBSCRIBERS / MAX_PENDING {
        for _ in 0..MAX_PENDING {
            let (_, request) = request(&link, now, &mut rng);
            admit(&mut publisher, &request, now);
        }
        publisher.rekey(now, &mut rng).unwrap();
        now += 1000;
    }
    // Once every nonce has expired, the subscriber cap alone refuses.
    let now = now + 3 * REQUEST_LIFETIME_MS;
    let (_, extra) = request(&link, now, &mut rng);
    assert_eq!(
        publisher.receive(&extra, now).err(),
        Some(Error::LimitReached)
    );

    // An X-Wing key that fails the ML-KEM modulus check.
    let (_, request) = self::request(&link, T0, &mut rng);
    let (_, mut body) = open_message(&link.keys(), &request).unwrap();
    body[56..56 + 1216].fill(0xff);
    let forged = seal_message(&link.keys(), REQUEST, &body, &mut rng).unwrap();
    assert_eq!(
        self::publisher(&link).receive(&forged, T0).err(),
        Some(Error::InvalidPublicKey)
    );
}

// -------------------------------------------------------------------------------------
// Malformed input through the real parsers

/// Bit flips, truncations and extensions of real messages: always an error, never a
/// panic. The signal envelope rejects them first.
#[test]
fn mutated_messages_are_rejected() {
    let link = link(21);
    let mut publisher = publisher(&link);
    let mut rng = rng(22);
    let (mut subscriber, request) = request(&link, T0, &mut rng);
    admit(&mut publisher, &request, T0);
    let grant = publisher.rekey(T0, &mut rng).unwrap().messages.remove(0).1;
    let mut noise = self::rng(23);
    for message in [&request, &grant] {
        for _ in 0..200 {
            let mut mutated = message.clone();
            let [position, bit, kind] = random::<3, _>(&mut noise).unwrap();
            let index = usize::from(position) * mutated.len() / 256;
            match kind % 3 {
                0 => mutated[index] ^= 1 << (bit % 8),
                1 => mutated.truncate(index),
                _ => mutated.extend_from_slice(&[bit; 3]),
            }
            assert!(publisher.receive(&mutated, T0).is_err());
            assert!(subscriber.receive(&mutated, T0, &mut rng).is_err());
        }
    }
    assert!(
        publisher
            .receive(&vec![1; MAX_KEY_MESSAGE + 1], T0)
            .is_err()
    );
}

/// Bodies sealed correctly under `signal_key`, so the body parsers themselves see the
/// damage: every truncation and extension fails, and byte changes never panic.
#[test]
fn mutated_bodies_reach_the_parsers() {
    let link = link(24);
    let keys = link.keys();
    let mut rng = rng(25);
    let mut noise = self::rng(26);
    let mut publisher = publisher(&link);
    let (mut subscriber, request) = request(&link, T0, &mut rng);
    let (_, request_body) = open_message(&keys, &request).unwrap();

    for len in 0..request_body.len() + 2 {
        if len == request_body.len() {
            continue;
        }
        let mut body = request_body.clone();
        body.resize(len, 0);
        let message = seal_message(&keys, REQUEST, &body, &mut rng).unwrap();
        assert!(publisher.receive(&message, T0).is_err(), "length {len}");
    }
    for _ in 0..500 {
        let mut body = request_body.clone();
        let [position, value] = random::<2, _>(&mut noise).unwrap();
        body[usize::from(position) * request_body.len() / 256] = value;
        let message = seal_message(&keys, REQUEST, &body, &mut rng).unwrap();
        let _ = publisher.receive(&message, T0);
    }

    // Grant bodies at a new subscriber, Rekey bodies at a granted one.
    let (mut granted, _, _) = join(&link, &mut self::publisher(&link), T0, 27);
    for (kind, len) in [(GRANT, 16 + 32 + 1120 + 52), (REKEY, 16 + 8 + 52)] {
        let target = if kind == REKEY {
            &mut granted
        } else {
            &mut subscriber
        };
        for cut in [16, 17, len - 1, len + 1] {
            let mut body = target.id().to_vec();
            body.push(2);
            body.resize(cut, 0);
            let message = seal_message(&keys, kind, &body, &mut rng).unwrap();
            assert!(
                target.receive(&message, T0, &mut rng).is_err(),
                "kind {kind} length {cut}"
            );
        }
    }
}

#[test]
fn link_parser_survives_noise() {
    let mut noise = rng(28);
    let good = link(29).call_url().unwrap();
    for _ in 0..2000 {
        let [position, value, kind] = random::<3, _>(&mut noise).unwrap();
        let mut bytes = good.clone().into_bytes();
        let index = usize::from(position) * bytes.len() / 256;
        match kind % 3 {
            0 => bytes[index] = value,
            1 => bytes.truncate(index),
            _ => bytes.insert(index, value),
        }
        if let Ok(text) = String::from_utf8(bytes)
            && let Ok(parsed) = Link::parse(&text, HOST)
        {
            let canonical = parsed.call_url().unwrap_or_else(|| parsed.watch_url());
            assert_eq!(canonical, text);
        }
    }
}
