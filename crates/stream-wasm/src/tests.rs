// SPDX-License-Identifier: EUPL-1.2
//! The transcript in `test-vectors/transcript.txt`: two workers on the native target run a
//! call through the message interface, with the replies and errors they gave.
//! `tests/stream_wasm.mjs` replays every line through the wasm32 build and requires the
//! same bytes. Worker A publishes, B subscribes; B's lines take A's native messages.
//!
//! Regenerate it with `NUSCHL_WRITE_TRANSCRIPT=1 cargo test -p nuschl-stream-wasm`.

use std::convert::Infallible;
use std::fmt::Write as _;

use nuschl_stream::{MAX_KEY_MESSAGE, REQUEST_LIFETIME_MS};
use rand_core::{TryCryptoRng, TryRng};
use serde_json::{Value, json};

use crate::{MAX_REQUEST, REKEY_INTERVAL_MS, Worker};

/// xorshift32, one byte per step; the replay's `crypto.getRandomValues` is the same.
struct TestRng(u32);

impl TryRng for TestRng {
    type Error = Infallible;

    fn try_next_u32(&mut self) -> Result<u32, Infallible> {
        let mut bytes = [0; 4];
        self.try_fill_bytes(&mut bytes)?;
        Ok(u32::from_le_bytes(bytes))
    }

    fn try_next_u64(&mut self) -> Result<u64, Infallible> {
        let mut bytes = [0; 8];
        self.try_fill_bytes(&mut bytes)?;
        Ok(u64::from_le_bytes(bytes))
    }

    fn try_fill_bytes(&mut self, buffer: &mut [u8]) -> Result<(), Infallible> {
        for byte in buffer {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 17;
            self.0 ^= self.0 << 5;
            *byte = self.0 as u8;
        }
        Ok(())
    }
}

impl TryCryptoRng for TestRng {}

const T0: u64 = 1_800_000_000_000;
const HOST: &str = "stream.nuschl.eu";
/// S = 00..1f, P = 20..3f (crates/stream's key schedule vector).
const CREATOR_LINK: &str = concat!(
    "https://stream.nuschl.eu/call#v=1&k=AAECAwQFBgcICQoLDA0ODxAREhMUFRYXGBkaGxwdHh8",
    "&h=8FkoEkvVnGXqAGKF9u7ezIde49CL2ZPe5AQZJ3_d3tQ",
    "&p=ICEiIyQlJicoKSorLC0uLzAxMjM0NTY3ODk6Ozw9Pj8"
);
const PUBLISHER: u64 = 1;
const SUBSCRIBER: u64 = 2;

/// Two workers, A (SFU peer 1) and B (peer 2), and the transcript of what they did.
struct Call {
    workers: [Worker<TestRng>; 2],
    lines: String,
}

impl Call {
    fn new() -> Self {
        Self {
            workers: [Worker::new(TestRng(1)), Worker::new(TestRng(2))],
            lines: String::new(),
        }
    }

    /// Records a call and its result, a reply or a frame in hex.
    fn record(
        &mut self,
        worker: usize,
        now: u64,
        call: String,
        result: Result<String, String>,
    ) -> Result<String, String> {
        let name = ["A", "B"][worker];
        let reply = match &result {
            Ok(value) => format!("= {value}"),
            Err(error) => format!("! {error}"),
        };
        writeln!(self.lines, "{name} {now} {call}\n{reply}").expect("string");
        result
    }

    fn handle(&mut self, worker: usize, now: u64, request: &str) -> Result<Value, String> {
        let result = self.workers[worker].handle(request, now);
        let reply = self.record(worker, now, format!("handle {request}"), result)?;
        Ok(serde_json::from_str(&reply).expect("JSON reply"))
    }

    fn ok(&mut self, worker: usize, now: u64, request: Value) -> Value {
        self.handle(worker, now, &request.to_string())
            .expect("request succeeds")
    }

    fn fails(&mut self, worker: usize, now: u64, request: &str, error: &str) {
        assert_eq!(
            self.handle(worker, now, request).err().as_deref(),
            Some(error),
            "{request}"
        );
    }

    /// Hands every `send` entry of `reply` from `from` to the other worker.
    fn deliver(&mut self, from: usize, now: u64, reply: &Value) -> Vec<Value> {
        let peer = [PUBLISHER, SUBSCRIBER][from];
        reply["send"]
            .as_array()
            .expect("send")
            .iter()
            .map(|message| {
                let to = if message["to"] == PUBLISHER { 0 } else { 1 };
                self.ok(
                    to,
                    now,
                    json!({"v": 1, "kind": "key", "from": peer, "data": message["data"]}),
                )
            })
            .collect()
    }

    fn encrypt(&mut self, now: u64, key_frame: bool, frame: &[u8]) -> Vec<u8> {
        let result = self.workers[0].encrypt(0, key_frame, frame);
        let kind = if key_frame { "key" } else { "delta" };
        let call = format!("encrypt 0 {kind} {}", hex::encode(frame));
        let frame = self
            .record(0, now, call, result.map(hex::encode))
            .expect("frame encrypts");
        hex::decode(frame).expect("hex")
    }

    fn decrypt(&mut self, now: u64, frame: &[u8]) -> Result<Vec<u8>, String> {
        let result = self.workers[1].decrypt(PUBLISHER, frame, now);
        let call = format!("decrypt {PUBLISHER} {}", hex::encode(frame));
        let frame = self.record(1, now, call, result.map(hex::encode))?;
        Ok(hex::decode(frame).expect("hex"))
    }
}

fn keyed(reply: &Value, epoch: u32) {
    assert_eq!(
        reply["events"],
        json!([{"kind": "keyed", "peer": PUBLISHER, "epoch": epoch}])
    );
}

fn transcript() -> String {
    let mut call = Call::new();
    let (a, b) = (0, 1);

    // Requests outside the interface fail before any state exists.
    call.fails(a, T0, r#"{"v":2,"kind":"tick"}"#, "unsupported version");
    call.fails(a, T0, r#"{"kind":"tick"}"#, "malformed request");
    call.fails(
        a,
        T0,
        r#"{"v":1,"kind":"tick","extra":0}"#,
        "malformed request",
    );
    call.fails(
        a,
        T0,
        r#"{"v":1,"kind":"leave","peer":1,"extra":0}"#,
        "malformed request",
    );
    call.fails(a, T0, r#"{"v":1,"kind":"rekey"}"#, "malformed request");
    call.fails(a, T0, &" ".repeat(MAX_REQUEST + 1), "request too long");
    call.fails(a, T0, r#"{"v":1,"kind":"publish"}"#, "no call link");

    let opened = call.ok(
        a,
        T0,
        json!({"v": 1, "kind": "open", "link": CREATOR_LINK, "host": HOST, "allow_direct": true}),
    );
    assert_eq!(opened["creator"], true);
    let posted = CREATOR_LINK.split("&p=").next().expect("link");
    let opened_b = call.ok(
        b,
        T0,
        json!({"v": 1, "kind": "open", "link": posted, "host": HOST, "allow_direct": true}),
    );
    assert_eq!(opened_b["room"], opened["room"]);
    assert_eq!(opened_b["verifier"], opened["verifier"]);
    assert_eq!(opened_b["creator"], false);

    let nonce = hex::encode([7; 32]);
    let admitted = call.ok(a, T0, json!({"v": 1, "kind": "admit", "nonce": nonce}));
    let admitted_b = call.ok(b, T0, json!({"v": 1, "kind": "admit", "nonce": nonce}));
    assert_eq!(admitted["proof"], admitted_b["proof"]);
    assert_eq!(admitted_b["host_proof"], Value::Null);
    assert_eq!(admitted_b["create"], Value::Null);

    // The exchange: B's request waits for A's next rekey, a second after the first.
    call.ok(a, T0, json!({"v": 1, "kind": "publish"}));
    let request = call.ok(
        b,
        T0,
        json!({"v": 1, "kind": "subscribe", "peer": PUBLISHER}),
    );
    call.deliver(b, T0, &request);
    let early = call.ok(a, T0 + 500, json!({"v": 1, "kind": "tick"}));
    assert_eq!(early["send"], json!([]));
    let rekey = call.ok(a, T0 + 1000, json!({"v": 1, "kind": "tick"}));
    let [granted] = &call.deliver(a, T0 + 1000, &rekey)[..] else {
        panic!("one grant");
    };
    keyed(granted, 2);
    let [confirmed] = &call.deliver(b, T0 + 1000, granted)[..] else {
        panic!("one ack");
    };
    assert_eq!(
        confirmed["events"],
        json!([{"kind": "confirmed", "peer": SUBSCRIBER}])
    );

    for (from, to, publisher, data) in [
        (b, PUBLISHER, false, r#"{"type":"ready"}"#),
        (
            a,
            SUBSCRIBER,
            true,
            "a=candidate:1 1 UDP 1 127.0.0.1 54321 typ host",
        ),
    ] {
        let direct = call.ok(
            from,
            T0 + 1000,
            json!({"v": 1, "kind": "direct", "peer": to, "publisher": publisher, "data": data}),
        );
        let [received] = &call.deliver(from, T0 + 1000, &direct)[..] else {
            panic!("one direct signal");
        };
        let peer = [PUBLISHER, SUBSCRIBER][from];
        assert_eq!(
            received["events"],
            json!([{"kind": "direct", "peer": peer, "publisher": !publisher, "data": data}])
        );
    }

    // The resend fails as unexpected at B; frames switch to epoch 2 a second after the rekey.
    let resend = call.ok(a, T0 + 1500, json!({"v": 1, "kind": "tick"}));
    assert_eq!(resend["send"], rekey["send"]);
    let grant = resend["send"][0]["data"]
        .as_str()
        .expect("grant")
        .to_owned();
    let replay = json!({"v": 1, "kind": "key", "from": PUBLISHER, "data": grant});
    call.fails(b, T0 + 1500, &replay.to_string(), "unexpected message");
    let frame = call.encrypt(T0 + 1500, true, b"epoch 1");
    assert_eq!(
        call.decrypt(T0 + 1500, &frame),
        Err("unknown key id".into())
    );
    call.ok(a, T0 + 2000, json!({"v": 1, "kind": "tick"}));
    let frame = call.encrypt(T0 + 2000, true, b"epoch 2 key frame");
    assert_eq!(
        call.decrypt(T0 + 2000, &frame).as_deref(),
        Ok(&b"epoch 2 key frame"[..])
    );
    assert_eq!(call.decrypt(T0 + 2000, &frame), Err("replayed".into()));
    let delta = call.encrypt(T0 + 2000, false, b"epoch 2 delta");
    assert!(call.decrypt(T0 + 2000, &delta).is_ok());

    // The periodic rekey goes over the channel ratchet.
    let later = T0 + 1000 + REKEY_INTERVAL_MS;
    let rekey = call.ok(a, later, json!({"v": 1, "kind": "tick"}));
    let [rekeyed] = &call.deliver(a, later, &rekey)[..] else {
        panic!("one rekey");
    };
    keyed(rekeyed, 3);
    let message = rekey["send"][0]["data"].as_str().expect("rekey").to_owned();
    let replay = json!({"v": 1, "kind": "key", "from": PUBLISHER, "data": message});
    call.fails(b, later, &replay.to_string(), "replayed");
    call.ok(a, later + 1000, json!({"v": 1, "kind": "tick"}));
    let frame = call.encrypt(later + 1000, false, b"epoch 3");
    assert!(call.decrypt(later + 1000, &frame).is_ok());

    // Oversized, malformed and expired key messages.
    let oversized = "00".repeat(MAX_KEY_MESSAGE + 1);
    let oversized = json!({"v": 1, "kind": "key", "from": SUBSCRIBER, "data": oversized});
    call.fails(a, later, &oversized.to_string(), "too long");
    let odd = json!({"v": 1, "kind": "key", "from": SUBSCRIBER, "data": "0"});
    call.fails(a, later, &odd.to_string(), "malformed key message");
    let request = call.ok(
        b,
        later,
        json!({"v": 1, "kind": "subscribe", "peer": PUBLISHER}),
    );
    let data = request["send"][0]["data"]
        .as_str()
        .expect("request")
        .to_owned();
    let expired = json!({"v": 1, "kind": "key", "from": SUBSCRIBER, "data": data});
    let after = later + REQUEST_LIFETIME_MS + 1;
    call.fails(a, after, &expired.to_string(), "expired");

    // B leaves: A's next rekey goes to nobody, and B cannot read A's frames.
    call.ok(
        a,
        after,
        json!({"v": 1, "kind": "leave", "peer": SUBSCRIBER}),
    );
    let rekey = call.ok(a, after, json!({"v": 1, "kind": "tick"}));
    assert_eq!(rekey["send"], json!([]));
    let frame = call.encrypt(after, true, b"epoch 4");
    assert_eq!(call.decrypt(after, &frame), Err("unknown key id".into()));
    call.lines
}

#[test]
fn relay_only_worker_has_no_direct_signaling() {
    let mut worker = Worker::new(TestRng(3));
    let open = json!({"v": 1, "kind": "open", "link": CREATOR_LINK, "host": HOST});
    worker.handle(&open.to_string(), T0).unwrap();
    assert_eq!(
        worker.handle(
            r#"{"v":1,"kind":"direct","peer":2,"publisher":true,"data":"candidate"}"#,
            T0
        ),
        Err("direct signaling disabled".into())
    );
    assert_eq!(
        worker.handle(r#"{"v":1,"kind":"key","from":2,"data":"0105"}"#, T0),
        Err("direct signaling disabled".into())
    );
}

#[test]
fn transcript_matches_the_recorded_one() {
    let generated = transcript();
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/test-vectors/transcript.txt");
    if std::env::var_os("NUSCHL_WRITE_TRANSCRIPT").is_some() {
        std::fs::write(path, &generated).expect("write the transcript");
    }
    assert!(
        generated == include_str!("../test-vectors/transcript.txt"),
        "the transcript changed; regenerate it (src/tests.rs)"
    );
}

/// The SFU's admission proofs, against Python's hmac module:
/// `hmac.new(admit_key, b"nuschl/stream/v2 sfu admit" + bytes([7]) * 32, "sha256")`.
#[test]
fn admission_proofs_known_answers() {
    let mut worker = Worker::new(TestRng(3));
    let open = json!({"v": 1, "kind": "open", "link": CREATOR_LINK, "host": HOST});
    worker.handle(&open.to_string(), T0).expect("open");
    let admit = json!({"v": 1, "kind": "admit", "nonce": hex::encode([7; 32])});
    let reply: Value =
        serde_json::from_str(&worker.handle(&admit.to_string(), T0).expect("admit")).expect("JSON");
    assert_eq!(
        reply,
        json!({
            "v": 1,
            "kind": "admitted",
            "proof": "6153c3327c2a83aab5e64d9c08bc07e0e44eb4cf06662d9e447d0f270e14af6f",
            "host_proof": "81c1e67f6c6d7d698e9e2359be43d122f9bcc34f3adb69c6ab93eedc36d8f145",
            "create": {
                "admit_key": "351c31140d48a2cc4a1a738b543bf220c48f0cb2f828363b52e79ae95870e0b8",
                "host_key": "8956ee0f52f292a9d547563b15118898448f91b8606e4d39cd57509a8ba0e6e8",
            },
        })
    );
}
