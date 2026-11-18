// SPDX-License-Identifier: EUPL-1.2
//! The SFU binary on loopback with three str0m peers standing in for browsers.
//! A host creates the room; two participants join with the link key alone. The
//! host publishes AV1 and one participant VP9; every peer receives the others'
//! RTP payloads byte for byte with consecutive sequence numbers. Key messages
//! reach the named peer only. Wrong proofs, unknown rooms, oversized key
//! messages and host actions by participants are refused. The creator's lease
//! renews with `host_key`, and ending it disconnects everyone.

use futures_util::{SinkExt, StreamExt};
use hmac::{KeyInit, Mac};
use serde_json::{Value, json};
use std::{
    collections::{HashMap, HashSet},
    io::{BufRead, BufReader},
    net::UdpSocket,
    process::{Child, Command, Stdio},
    sync::Arc,
    time::{Duration, Instant},
};
use str0m::{
    Candidate, Event, Input, Output, Rtc,
    change::{SdpAnswer, SdpOffer, SdpPendingOffer},
    format::Codec,
    media::{Direction, MediaKind, Mid},
    net::{Protocol, Receive},
    rtp::RtpWrite,
};
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message;

const ADMIT: [u8; 32] = [1; 32];
const HOST: [u8; 32] = [2; 32];
const ROOM: [u8; 16] = [3; 16];

struct Sfu {
    child: Child,
    url: String,
    _directory: nuschl_testkit::TempDir,
}

impl Drop for Sfu {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn start() -> Sfu {
    let directory = nuschl_testkit::temp_dir();
    let secret = directory.path().join("turn-secret");
    std::fs::write(&secret, "test-secret\n").expect("secret");
    let config = directory.path().join("sfu.toml");
    std::fs::write(
        &config,
        format!(
            r#"
            listen = "127.0.0.1:0"
            media_listen = "127.0.0.1:0"
            media_addresses = ["127.0.0.1"]
            max_peers_per_room = 6
            [turn]
            urls = ["turn:turn.test:3478?transport=udp"]
            secret_file = "{}"
            "#,
            secret.display()
        ),
    )
    .expect("config");
    let mut child = Command::new(env!("CARGO_BIN_EXE_nuschl-stream-sfu"))
        .arg("--config")
        .arg(&config)
        .stdout(Stdio::piped())
        .spawn()
        .expect("start the SFU");
    let mut line = String::new();
    BufReader::new(child.stdout.take().expect("stdout"))
        .read_line(&mut line)
        .expect("ready line");
    let signalling = line.split_whitespace().nth(2).expect("signalling address");
    Sfu {
        child,
        url: format!("ws://{signalling}/sfu"),
        _directory: directory,
    }
}

/// `POST` or `DELETE /sfu/lease`; returns the status code.
fn lease(url: &str, method: &str, stream_id: [u8; 16], host_key: [u8; 32]) -> u16 {
    use std::io::{Read, Write};
    let address = url.trim_start_matches("ws://").trim_end_matches("/sfu");
    let body =
        json!({"stream_id": hex::encode(stream_id), "host_key": hex::encode(host_key)}).to_string();
    let mut stream = std::net::TcpStream::connect(address).expect("connect");
    write!(
        stream,
        "{method} /sfu/lease HTTP/1.1\r\nHost: sfu\r\nContent-Length: {}\r\n\r\n{body}",
        body.len()
    )
    .expect("request");
    let mut response = String::new();
    stream.read_to_string(&mut response).expect("response");
    response
        .split_whitespace()
        .nth(1)
        .expect("status")
        .parse()
        .expect("code")
}

fn prove(key: &[u8; 32], label: &[u8], nonce: &[u8]) -> String {
    let mut mac = hmac::Hmac::<sha2::Sha256>::new_from_slice(key).expect("key");
    mac.update(label);
    mac.update(nonce);
    hex::encode(mac.finalize().into_bytes())
}

/// One WebRTC connection on its own loopback socket.
struct Leg {
    rtc: Rtc,
    socket: UdpSocket,
    events: Vec<Event>,
}

impl Leg {
    fn new() -> Self {
        let socket = UdpSocket::bind("127.0.0.1:0").expect("udp");
        socket.set_nonblocking(true).expect("nonblocking");
        let mut rtc = Rtc::builder()
            .set_crypto_provider(Arc::new(str0m::crypto::from_feature_flags()))
            .set_rtp_mode(true)
            .build(Instant::now());
        rtc.add_local_candidate(
            Candidate::host(socket.local_addr().expect("address"), "udp").expect("candidate"),
        );
        Self {
            rtc,
            socket,
            events: Vec::new(),
        }
    }

    fn poll(&mut self) {
        loop {
            match self.rtc.poll_output().expect("poll") {
                Output::Timeout(_) => return,
                Output::Transmit(t) => {
                    let _ = self.socket.send_to(&t.contents, t.destination);
                }
                Output::Event(event) => self.events.push(event),
            }
        }
    }

    fn step(&mut self) {
        self.poll();
        let local = self.socket.local_addr().expect("address");
        let mut buffer = [0; 2048];
        while let Ok((length, source)) = self.socket.recv_from(&mut buffer) {
            let input = Input::Receive(
                Instant::now(),
                Receive {
                    proto: Protocol::Udp,
                    source,
                    destination: local,
                    contents: buffer[..length].try_into().expect("datagram"),
                },
            );
            self.rtc.handle_input(input).expect("input");
            self.poll();
        }
        self.rtc
            .handle_input(Input::Timeout(Instant::now()))
            .expect("timeout");
        self.poll();
    }
}

struct Peer {
    to_sfu: mpsc::UnboundedSender<String>,
    from_sfu: mpsc::UnboundedReceiver<Value>,
    id: u64,
    publish: Option<(Leg, Option<SdpPendingOffer>, Mid, Codec)>,
    subscribe: Option<Leg>,
    tracks: HashMap<String, u64>,
    /// Per publisher: received (sequence number, payload type, payload).
    received: HashMap<u64, Vec<(u64, u8, Vec<u8>)>>,
    sent: Vec<Vec<u8>>,
    seq: u64,
    messages: Vec<Value>,
}

async fn connect(
    url: &str,
) -> (
    mpsc::UnboundedSender<String>,
    mpsc::UnboundedReceiver<Value>,
) {
    let (mut socket, _) = tokio_tungstenite::connect_async(url)
        .await
        .expect("connect");
    let (to_sfu, mut outgoing) = mpsc::unbounded_channel::<String>();
    let (incoming, from_sfu) = mpsc::unbounded_channel();
    tokio::spawn(async move {
        loop {
            tokio::select! {
                message = outgoing.recv() => match message {
                    Some(text) => if socket.send(Message::text(text)).await.is_err() { return },
                    None => return,
                },
                message = socket.next() => match message {
                    Some(Ok(Message::Text(text))) => {
                        let _ = incoming.send(serde_json::from_str(&text).expect("json"));
                    }
                    Some(Ok(_)) => {}
                    _ => {
                        let _ = incoming.send(json!({"type": "closed"}));
                        return;
                    }
                },
            }
        }
    });
    (to_sfu, from_sfu)
}

async fn next(from_sfu: &mut mpsc::UnboundedReceiver<Value>) -> Value {
    tokio::time::timeout(Duration::from_secs(10), from_sfu.recv())
        .await
        .expect("a message in time")
        .expect("open")
}

/// Joins with the admit proof, the host proof and the room's keys as asked;
/// returns the peer and the SFU's first answer.
async fn join(url: &str, admit: &[u8; 32], host: bool, create: bool) -> (Peer, Value) {
    let (to_sfu, mut from_sfu) = connect(url).await;
    let hello = next(&mut from_sfu).await;
    assert_eq!(hello["type"], "hello");
    let nonce = hex::decode(hello["nonce"].as_str().expect("nonce")).expect("hex");
    let mut message = json!({
        "type": "join",
        "room": hex::encode(ROOM),
        "verifier": hex::encode(<sha2::Sha256 as sha2::Digest>::digest(HOST)),
        "proof": prove(admit, b"nuschl/stream/v2 sfu admit", &nonce),
    });
    if host {
        message["host_proof"] = prove(&HOST, b"nuschl/stream/v2 sfu host", &nonce).into();
    }
    if create {
        message["create"] = json!({"admit_key": hex::encode(ADMIT), "host_key": hex::encode(HOST)});
    }
    to_sfu.send(message.to_string()).expect("send");
    let answer = next(&mut from_sfu).await;
    let id = answer["peer"].as_u64().unwrap_or(0);
    let peer = Peer {
        to_sfu,
        from_sfu,
        id,
        publish: None,
        subscribe: None,
        tracks: HashMap::new(),
        received: HashMap::new(),
        sent: Vec::new(),
        seq: 0,
        messages: Vec::new(),
    };
    (peer, answer)
}

impl Peer {
    /// Sends a message; a closed connection shows in the messages received.
    fn send(&self, message: Value) {
        let _ = self.to_sfu.send(message.to_string());
    }

    fn start_publishing(&mut self, codec: Codec) {
        let mut leg = Leg::new();
        let mut api = leg.rtc.sdp_api();
        let mid = api.add_media(MediaKind::Video, Direction::SendOnly, None, None, None);
        let (offer, pending) = api.apply().expect("offer");
        self.send(json!({"type": "offer", "sdp": offer.to_sdp_string()}));
        self.publish = Some((leg, Some(pending), mid, codec));
    }

    fn step(&mut self) {
        while let Ok(message) = self.from_sfu.try_recv() {
            match message["type"].as_str() {
                Some("offer") => {
                    let leg = self.subscribe.get_or_insert_with(Leg::new);
                    let offer = SdpOffer::from_sdp_string(message["sdp"].as_str().expect("sdp"))
                        .expect("offer");
                    let answer = leg.rtc.sdp_api().accept_offer(offer).expect("accept");
                    for track in message["tracks"].as_array().expect("tracks") {
                        let mid = track["mid"].as_str().expect("mid").to_owned();
                        self.tracks
                            .insert(mid, track["peer"].as_u64().expect("peer"));
                    }
                    self.send(json!({"type": "answer", "sdp": answer.to_sdp_string()}));
                }
                Some("answer") => {
                    let (leg, pending, ..) = self.publish.as_mut().expect("publishing");
                    let answer = SdpAnswer::from_sdp_string(message["sdp"].as_str().expect("sdp"))
                        .expect("answer");
                    let pending = pending.take().expect("one offer");
                    leg.rtc
                        .sdp_api()
                        .accept_answer(pending, answer)
                        .expect("accept");
                }
                _ => self.messages.push(message),
            }
        }
        if let Some(leg) = &mut self.subscribe {
            leg.step();
            for event in leg.events.drain(..) {
                if let Event::RtpPacket(packet) = event {
                    let mid = leg
                        .rtc
                        .direct_api()
                        .stream_rx(&packet.header.ssrc)
                        .map(|s| s.mid().to_string())
                        .expect("a stream");
                    let from = self.tracks[&mid];
                    self.received.entry(from).or_default().push((
                        *packet.seq_no,
                        *packet.header.payload_type,
                        packet.payload.to_vec(),
                    ));
                }
            }
        }
        if let Some((leg, ..)) = &mut self.publish {
            leg.step();
            leg.events.clear();
        }
    }

    /// Sends one synthetic encrypted frame in one packet, once connected.
    fn send_frame(&mut self) {
        let Some((leg, None, mid, codec)) = &mut self.publish else {
            return;
        };
        if !leg.rtc.is_connected() {
            return;
        }
        let media = leg.rtc.media(*mid).expect("media");
        let pt = leg
            .rtc
            .codec_config()
            .params()
            .iter()
            .find(|p| p.spec().codec == *codec && media.remote_pts().contains(&p.pt()))
            .expect("negotiated codec")
            .pt();
        self.seq += 1;
        // An AV1 aggregation header, then bytes that stand in for SFrame ciphertext.
        let mut payload = vec![if self.seq % 10 == 1 { 0x18 } else { 0x10 }];
        payload.extend_from_slice(&self.id.to_be_bytes());
        payload.extend_from_slice(&self.seq.to_be_bytes());
        payload.extend((0..600).map(|i| (i as u64 * 31 + self.seq) as u8));
        let mut api = leg.rtc.direct_api();
        let stream = api.stream_tx_by_mid(*mid, None).expect("stream");
        stream.write_rtp(
            RtpWrite::new(
                pt,
                self.seq.into(),
                (self.seq * 3000) as u32,
                Instant::now(),
                payload.clone(),
            )
            .marker(true)
            .nackable(true),
        );
        self.sent.push(payload);
    }

    fn count(&self, from: u64) -> usize {
        self.received.get(&from).map_or(0, Vec::len)
    }

    /// The first message of `kind` received so far, or within ten seconds.
    async fn expect(&mut self, kind: &str) -> Value {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            if let Some(index) = self.messages.iter().position(|m| m["type"] == kind) {
                return self.messages.remove(index);
            }
            assert!(Instant::now() < deadline, "no {kind}: {:?}", self.messages);
            self.step();
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    }
}

fn assert_forwarded(receiver: &Peer, publisher: &Peer, codec: Codec) {
    let sent: HashSet<&Vec<u8>> = publisher.sent.iter().collect();
    let received = &receiver.received[&publisher.id];
    let leg = receiver.subscribe.as_ref().expect("subscribed");
    for (index, (seq, pt, payload)) in received.iter().enumerate() {
        assert!(sent.contains(payload), "payload changed or invented");
        assert_eq!(*seq, index as u64, "consecutive sequence numbers from zero");
        let params = leg
            .rtc
            .codec_config()
            .params()
            .iter()
            .find(|p| *p.pt() == *pt);
        assert_eq!(params.map(|p| p.spec().codec), Some(codec));
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn forwards_media_and_key_messages_unchanged() {
    let sfu = start();
    let url = sfu.url.as_str();

    // Refusals before the room exists, and creating it without P.
    let (_, refused) = join(url, &ADMIT, false, false).await;
    assert_eq!(refused, json!({"type": "error", "reason": "no_room"}));
    let (_, refused) = join(url, &ADMIT, false, true).await;
    assert_eq!(refused["reason"], "proof");

    let (mut host, welcome) = join(url, &ADMIT, true, true).await;
    assert_eq!(welcome["type"], "welcome");
    // The creator's device renews the lease with host_key.
    assert_eq!(lease(url, "POST", ROOM, HOST), 204);
    assert_eq!(lease(url, "POST", ROOM, HOST), 429, "at most once a second");
    assert_eq!(
        lease(url, "POST", ROOM, ADMIT),
        404,
        "another key names no room"
    );
    assert_eq!(lease(url, "POST", [4; 16], HOST), 404);
    assert_eq!(welcome["role"], "host");
    let turn = &welcome["turn"];
    assert_eq!(turn["urls"], json!(["turn:turn.test:3478?transport=udp"]));
    assert!(turn["username"].as_str().expect("username").contains(':'));
    let (mut alice, welcome) = join(url, &ADMIT, false, false).await;
    assert_eq!(welcome["role"], "participant");
    assert_eq!(welcome["peers"], json!([{"id": host.id, "role": "host"}]));
    let (mut bob, _) = join(url, &ADMIT, false, false).await;
    let (_, refused) = join(url, &[9; 32], false, false).await;
    assert_eq!(refused["reason"], "proof", "a wrong link key");
    assert_eq!(host.expect("joined").await["peer"], alice.id);

    // The host publishes AV1, a participant VP9.
    host.start_publishing(Codec::Av1);
    alice.start_publishing(Codec::Vp9);
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        for peer in [&mut host, &mut alice, &mut bob] {
            peer.step();
        }
        host.send_frame();
        alice.send_frame();
        if [
            bob.count(host.id),
            bob.count(alice.id),
            alice.count(host.id),
            host.count(alice.id),
        ]
        .iter()
        .all(|&n| n >= 50)
        {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "media did not arrive: {:?} {:?} {:?}",
            host.received.keys(),
            alice.received.keys(),
            bob.received.keys()
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    assert_forwarded(&bob, &host, Codec::Av1);
    assert_forwarded(&bob, &alice, Codec::Vp9);
    assert_forwarded(&alice, &host, Codec::Av1);
    assert_forwarded(&host, &alice, Codec::Vp9);
    assert!(!bob.received.contains_key(&bob.id) && !host.received.contains_key(&host.id));

    // Key messages reach the named peer, unchanged, with the sender's id.
    let data = "a5".repeat(16 * 1024);
    bob.send(json!({"type": "key", "to": host.id, "data": data}));
    let key = host.expect("key").await;
    assert_eq!(key, json!({"type": "key", "from": bob.id, "data": data}));
    assert!(!alice.messages.iter().any(|m| m["type"] == "key"));

    // Participants cannot act as hosts; the host removes a peer.
    alice.send(json!({"type": "remove", "peer": bob.id}));
    assert_eq!(alice.expect("error").await["reason"], "not_allowed");
    host.send(json!({"type": "remove", "peer": bob.id}));
    assert_eq!(bob.expect("error").await["reason"], "removed");
    assert_eq!(host.expect("left").await["peer"], alice.id);
    assert_eq!(host.expect("left").await["peer"], bob.id);

    // An oversized key message ends the sender's connection.
    let (mut carol, _) = join(url, &ADMIT, false, false).await;
    carol.send(json!({"type": "key", "to": host.id, "data": "a5".repeat(16 * 1024 + 1)}));
    assert_eq!(carol.expect("error").await["reason"], "key");

    // Ending the lease disconnects everyone and refuses new joins.
    assert_eq!(lease(url, "DELETE", ROOM, HOST), 204);
    assert_eq!(host.expect("error").await["reason"], "ended");
    assert_eq!(lease(url, "POST", ROOM, HOST), 404);
    let (_, refused) = join(url, &ADMIT, false, false).await;
    assert_eq!(refused["reason"], "ended");
}
