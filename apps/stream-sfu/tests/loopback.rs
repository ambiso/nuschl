// SPDX-License-Identifier: EUPL-1.2
//! The SFU binary on loopback with three str0m peers standing in for browsers.
//! A host creates the room; two participants join with the link key alone. The
//! host publishes AV1 and one participant VP9; every peer receives the others'
//! RTP payloads byte for byte with consecutive sequence numbers. Key messages
//! reach the named peer only. Wrong proofs, unknown rooms, oversized key
//! messages and host actions by participants are refused. The call codec
//! follows the peers' decoders. The creator's lease renews with `host_key`, and
//! ending it disconnects everyone.

use futures_util::{SinkExt, StreamExt};
use hmac::{KeyInit, Mac};
use nuschl_stream_quality::{Kind, QualityRequest};
use nuschl_stream_sfu::quality::{DESCRIPTOR_ID, Descriptor, descriptor_extension};
use serde_json::{Value, json};
use std::{
    collections::HashMap,
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
    rtp::{ExtensionValues, RtpWrite},
};
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message;

const ADMIT: [u8; 32] = [1; 32];
const HOST: [u8; 32] = [2; 32];
const ROOM: [u8; 16] = [3; 16];

struct Sfu {
    child: Child,
    url: String,
    directory: nuschl_testkit::TempDir,
}

impl Drop for Sfu {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// The SFU with `settings` added to its configuration; `{directory}` in them
/// names its temporary directory.
fn start(settings: &str) -> Sfu {
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
            {}
            [turn]
            urls = ["turn:turn.test:3478?transport=udp"]
            secret_file = "{}"
            "#,
            settings.replace("{directory}", &directory.path().display().to_string()),
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
        directory,
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
            .set_extension(DESCRIPTOR_ID, descriptor_extension())
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

/// A packet a subscriber received from one publisher.
struct Received {
    seq: u64,
    pt: u8,
    payload: Vec<u8>,
    descriptor: Option<Descriptor>,
}

/// An L1T3 Dependency Descriptor with libwebrtc's decode target indications:
/// templates T0, T1, T2 and decode targets for a quarter, half and all of the
/// frame rate. T0 frames are switch points for all three, T1 frames for the
/// full rate.
fn l1t3(template: u32, number: u16, structure: bool) -> Vec<u8> {
    let mut bits = Vec::new();
    let mut put = |value: u32, n: u32| {
        for i in (0..n).rev() {
            bits.push((value >> i) & 1 == 1);
        }
    };
    put(0b11, 2); // start and end of frame
    put(template, 6);
    put(u32::from(number), 16);
    if structure {
        put(0b10000, 5);
        put(0, 6); // template id offset
        put(2, 5); // three decode targets
        put(0b01_01_11, 6); // templates T0, T1, T2
        put(0b10_10_10, 6); // T0: switch for all
        put(0b00_01_10, 6); // T1: discardable for half, switch for full
        put(0b00_00_01, 6); // T2: discardable for full
        put(0, 3); // no frame diffs
        put(0, 2); // no chains
        put(0, 1); // no resolutions
    }
    while bits.len() % 8 != 0 {
        bits.push(false);
    }
    bits.chunks(8)
        .map(|byte| byte.iter().fold(0, |a, &b| (a << 1) | u8::from(b)))
        .collect()
}

struct Peer {
    to_sfu: mpsc::UnboundedSender<String>,
    from_sfu: mpsc::UnboundedReceiver<Value>,
    id: u64,
    publish: Option<(Leg, Option<SdpPendingOffer>, Mid, Codec)>,
    subscribe: Option<Leg>,
    tracks: HashMap<String, u64>,
    received: HashMap<u64, Vec<Received>>,
    /// Payloads and descriptors sent.
    sent: Vec<(Vec<u8>, Option<Descriptor>)>,
    seq: u64,
    last_frame: Option<Instant>,
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

/// Joins with the admit proof, the host proof and the room's keys as asked,
/// decoding AV1 and VP9; returns the peer and the SFU's first answer.
async fn join(url: &str, admit: &[u8; 32], host: bool, create: bool) -> (Peer, Value) {
    join_decoding(url, admit, host, create, &["av1", "vp9"]).await
}

async fn join_decoding(
    url: &str,
    admit: &[u8; 32],
    host: bool,
    create: bool,
    codecs: &[&str],
) -> (Peer, Value) {
    let (to_sfu, mut from_sfu) = connect(url).await;
    let hello = next(&mut from_sfu).await;
    assert_eq!(hello["type"], "hello");
    let nonce = hex::decode(hello["nonce"].as_str().expect("nonce")).expect("hex");
    let mut message = json!({
        "type": "join",
        "room": hex::encode(ROOM),
        "verifier": hex::encode(<sha2::Sha256 as sha2::Digest>::digest(HOST)),
        "proof": prove(admit, b"nuschl/stream/v2 sfu admit", &nonce),
        "codecs": codecs,
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
        last_frame: None,
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

    /// Stops the published source with a new offer.
    fn stop_publishing(&mut self) {
        let (leg, pending, mid, _) = self.publish.as_mut().expect("publishing");
        let mut api = leg.rtc.sdp_api();
        api.stop_media(*mid);
        let (offer, next) = api.apply().expect("offer");
        *pending = Some(next);
        self.send(json!({"type": "offer", "sdp": offer.to_sdp_string()}));
    }

    fn step(&mut self) {
        while let Ok(message) = self.from_sfu.try_recv() {
            match message["type"].as_str() {
                Some("offer") => {
                    let leg = self.subscribe.get_or_insert_with(Leg::new);
                    let offer = SdpOffer::from_sdp_string(message["sdp"].as_str().expect("sdp"))
                        .expect("offer");
                    let answer = leg.rtc.sdp_api().accept_offer(offer).expect("accept");
                    self.tracks.clear();
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
                    self.received.entry(from).or_default().push(Received {
                        seq: *packet.seq_no,
                        pt: *packet.header.payload_type,
                        payload: packet.payload.to_vec(),
                        descriptor: packet
                            .header
                            .ext_vals
                            .user_values
                            .get::<Descriptor>()
                            .cloned(),
                    });
                }
            }
        }
        if let Some((leg, ..)) = &mut self.publish {
            leg.step();
            leg.events.clear();
        }
    }

    /// Sends one synthetic encrypted frame in one packet, once connected, at 30
    /// frames a second.
    fn send_frame(&mut self) {
        let Some((leg, None, mid, codec)) = &mut self.publish else {
            return;
        };
        if !leg.rtc.is_connected()
            || self
                .last_frame
                .is_some_and(|at| at.elapsed() < Duration::from_millis(33))
        {
            return;
        }
        self.last_frame = Some(Instant::now());
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
        // AV1 frames carry an L1T3 Dependency Descriptor: T0 T2 T1 T2, with the
        // template structure on every T0 frame.
        let number = (self.seq - 1) as u16;
        let template = [0, 2, 1, 2][usize::from(number) % 4];
        let descriptor =
            (*codec == Codec::Av1).then(|| Descriptor(l1t3(template, number, template == 0)));
        let mut values = ExtensionValues::default();
        if let Some(descriptor) = &descriptor {
            values.user_values.set(descriptor.clone());
        }
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
            .ext_vals(values)
            .nackable(true),
        );
        self.sent.push((payload, descriptor));
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

/// The frame numbers a receiver got from `publisher`, read from the payloads.
fn frames(receiver: &Peer, publisher: &Peer) -> Vec<u64> {
    receiver
        .received
        .get(&publisher.id)
        .map_or_else(Vec::new, |packets| {
            packets
                .iter()
                .map(|p| u64::from_be_bytes(p.payload[9..17].try_into().expect("8 bytes")) - 1)
                .collect()
        })
}

fn quality_request(publisher: u64, tier: u8) -> Value {
    let request = QualityRequest {
        publisher,
        kind: Kind::Camera,
        tier,
    };
    json!({"type": "quality", "request": hex::encode(request.to_bytes())})
}

fn assert_forwarded(receiver: &Peer, publisher: &Peer, codec: Codec) {
    let sent: HashMap<&Vec<u8>, &Option<Descriptor>> =
        publisher.sent.iter().map(|(p, d)| (p, d)).collect();
    let received = &receiver.received[&publisher.id];
    let leg = receiver.subscribe.as_ref().expect("subscribed");
    for (index, packet) in received.iter().enumerate() {
        let descriptor = sent
            .get(&packet.payload)
            .expect("payload changed or invented");
        assert_eq!(
            **descriptor, packet.descriptor,
            "the descriptor arrives as sent"
        );
        assert_eq!(
            packet.seq, index as u64,
            "consecutive sequence numbers from zero"
        );
        let params = leg
            .rtc
            .codec_config()
            .params()
            .iter()
            .find(|p| *p.pt() == packet.pt);
        assert_eq!(params.map(|p| p.spec().codec), Some(codec));
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn forwards_media_and_key_messages_unchanged() {
    let sfu = start("");
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
    assert_eq!(welcome["codec"], "av1");
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

    // The publisher hears the largest tier its receivers ask for.
    alice.send(quality_request(host.id, 0));
    bob.send(quality_request(host.id, 1));
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let message = host.expect("quality").await;
        let bytes = hex::decode(message["request"].as_str().expect("hex")).expect("hex");
        let demand = QualityRequest::from_bytes(&bytes).expect("a request");
        assert_eq!((demand.publisher, demand.kind), (host.id, Kind::Camera));
        if demand.tier == 1 {
            break;
        }
        assert!(Instant::now() < deadline, "no demand of tier 1");
    }

    // A publisher that stops its source leaves the subscribers' offers.
    alice.stop_publishing();
    let deadline = Instant::now() + Duration::from_secs(10);
    while bob.tracks.values().any(|&peer| peer == alice.id) {
        assert!(Instant::now() < deadline, "the stopped source stayed");
        for peer in [&mut host, &mut alice, &mut bob] {
            peer.step();
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    assert!(bob.tracks.values().any(|&peer| peer == host.id));

    // A peer without AV1 switches the call to VP9 and back when it leaves; a
    // peer without VP9 cannot join then.
    let (vp9, welcome) = join_decoding(url, &ADMIT, false, false, &["vp9"]).await;
    assert_eq!(welcome["codec"], "vp9");
    assert_eq!(bob.expect("codec").await["codec"], "vp9");
    let (_, refused) = join_decoding(url, &ADMIT, false, false, &["av1"]).await;
    assert_eq!(refused, json!({"type": "error", "reason": "codec"}));
    let (_, refused) = join_decoding(url, &ADMIT, false, false, &[]).await;
    assert_eq!(refused["reason"], "join");
    let vp9_id = vp9.id;
    drop(vp9);
    assert_eq!(bob.expect("codec").await["codec"], "av1");
    assert_eq!(host.expect("left").await["peer"], vp9_id);

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

/// One AV1 publisher with an L1T3 descriptor at 30 frames a second and two
/// receivers. One asks for the 15 fps camera tier and gets the T0 and T1 frames;
/// the other gets every frame. Each switches at switch points, and skipped
/// frames leave no gaps in its sequence numbers.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn each_receiver_gets_the_layers_of_its_tier() {
    let sfu = start("");
    let url = sfu.url.as_str();
    let (mut host, _) = join(url, &ADMIT, true, true).await;
    let (mut low, _) = join(url, &ADMIT, false, false).await;
    let (mut full, _) = join(url, &ADMIT, false, false).await;
    host.start_publishing(Codec::Av1);
    let mut asked = false;
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        for peer in [&mut host, &mut low, &mut full] {
            peer.step();
        }
        host.send_frame();
        if !asked && !low.tracks.is_empty() {
            low.send(quality_request(host.id, 0));
            asked = true;
        }
        let tail = frames(&low, &host);
        let settled = tail.iter().rev().take_while(|n| *n % 2 == 0).count();
        if settled >= 20 && frames(&full, &host).len() >= 90 {
            break;
        }
        assert!(Instant::now() < deadline, "low {tail:?}");
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    for receiver in [&low, &full] {
        let packets = &receiver.received[&host.id];
        assert!(
            packets.iter().enumerate().all(|(i, p)| p.seq == i as u64),
            "consecutive sequence numbers across skipped frames"
        );
    }
    // Every frame from the first one on, all layers.
    let all = frames(&full, &host);
    assert!(all.windows(2).all(|w| w[1] == w[0] + 1), "{all:?}");
    // T2 frames are the odd ones. The low receiver skips only those, and its
    // first skip follows a T0 frame, the switch point for the 15 fps target.
    let got = frames(&low, &host);
    let skipped: Vec<u64> = got.windows(2).flat_map(|w| w[0] + 1..w[1]).collect();
    assert!(
        !skipped.is_empty() && skipped.iter().all(|n| n % 2 == 1),
        "{got:?}"
    );
    let first_skip = got.windows(2).find(|w| w[1] > w[0] + 1).expect("a skip")[0];
    assert_eq!(first_skip % 4, 0, "{got:?}");
}

/// A receiver whose budget covers the T0 and T1 frames but not the T2 frames
/// steps down to the 15 fps decode target.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_receiver_budget_limits_its_layers() {
    // T0 and T1 together: 15 frames of 617 bytes a second, 74 kbit/s; with
    // the step-up allowance 96 kbit/s. All three: 148 kbit/s.
    let sfu = start("receiver_bitrate = 110000");
    let url = sfu.url.as_str();
    let (mut host, _) = join(url, &ADMIT, true, true).await;
    let (mut receiver, _) = join(url, &ADMIT, false, false).await;
    host.start_publishing(Codec::Av1);
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        for peer in [&mut host, &mut receiver] {
            peer.step();
        }
        host.send_frame();
        let got = frames(&receiver, &host);
        let settled: Vec<u64> = got
            .iter()
            .rev()
            .take_while(|n| *n % 2 == 0)
            .copied()
            .collect();
        if settled.len() >= 20 {
            // The T0 and T1 frames, each one.
            assert!(settled.windows(2).all(|w| w[0] == w[1] + 2), "{got:?}");
            break;
        }
        assert!(Instant::now() < deadline, "{got:?}");
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_overloaded_single_layer_publisher_receives_lower_encoder_limits() {
    let sfu = start("receiver_bitrate = 40000");
    let (mut host, _) = join(&sfu.url, &ADMIT, true, true).await;
    let (mut viewer, _) = join(&sfu.url, &ADMIT, false, false).await;
    // VP9 carries no dependency descriptor in this fixture.
    host.start_publishing(Codec::Vp9);
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut began = false;
    loop {
        host.step();
        viewer.step();
        if began {
            host.send_frame();
        }
        if let Some(index) = host
            .messages
            .iter()
            .position(|msg| msg["type"] == "quality")
        {
            let message = host.messages.remove(index);
            let bytes = hex::decode(message["request"].as_str().expect("hex")).expect("request");
            let demand = QualityRequest::from_bytes(&bytes).expect("demand");
            assert_eq!((demand.publisher, demand.kind), (host.id, Kind::Camera));
            let (_, _, mid, _) = host.publish.as_ref().expect("publisher");
            assert_eq!(message["source"], mid.to_string());
            let tier = usize::from(demand.tier);
            let cap = demand.kind.ladder()[tier];
            assert_eq!(message["max_height"], cap.height);
            assert_eq!(message["max_fps"], cap.fps);
            assert_eq!(message["max_bitrate"], cap.ceiling);
            if began && tier < demand.kind.top() {
                assert!((1..host.sent.len()).contains(&viewer.count(host.id)));
                break;
            }
            assert_eq!(tier, demand.kind.top());
            began = true;
        }
        assert!(Instant::now() < deadline, "no lower encoder limits");
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn paced_video_sustains_small_packet_bursts() {
    const FRAMES: u32 = 120;
    const PACKETS: u32 = 48;
    const INTERVAL: Duration = Duration::from_millis(25);
    let sfu = start("");
    let (mut host, _) = join(&sfu.url, &ADMIT, true, true).await;
    let (mut viewer, _) = join(&sfu.url, &ADMIT, false, false).await;
    host.start_publishing(Codec::Vp9);
    let connect = Instant::now() + Duration::from_secs(5);
    loop {
        host.step();
        viewer.step();
        if host
            .publish
            .as_ref()
            .expect("publisher")
            .0
            .rtc
            .is_connected()
            && viewer
                .subscribe
                .as_ref()
                .is_some_and(|leg| leg.rtc.is_connected())
            && viewer.tracks.values().any(|&id| id == host.id)
        {
            break;
        }
        assert!(Instant::now() < connect, "media legs did not connect");
        tokio::time::sleep(Duration::from_millis(1)).await;
    }

    let (leg, _, mid, _) = host.publish.as_ref().expect("publisher");
    let remote = leg.rtc.media(*mid).expect("video").remote_pts();
    let pt = leg
        .rtc
        .codec_config()
        .params()
        .iter()
        .find(|params| params.spec().codec == Codec::Vp9 && remote.contains(&params.pt()))
        .expect("negotiated VP9")
        .pt();
    // This packet rate exercises the pacer while the small payload fits its
    // starting bitrate. Large RTP packets hit the same limit at about 10 Mb/s.
    let start = Instant::now();
    for frame in 0..FRAMES {
        let due = start + INTERVAL * frame;
        while Instant::now() < due {
            host.step();
            viewer.step();
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
        let (leg, _, mid, _) = host.publish.as_mut().expect("publisher");
        let mut api = leg.rtc.direct_api();
        let stream = api.stream_tx_by_mid(*mid, None).expect("video sender");
        let now = Instant::now();
        for packet in 0..PACKETS {
            let seq = u64::from(frame * PACKETS + packet + 1);
            stream.write_rtp(
                RtpWrite::new(pt, seq.into(), (frame + 1) * 2250, now, vec![0x10; 64])
                    .marker(packet + 1 == PACKETS),
            );
        }
        host.step();
        viewer.step();
    }
    let minimum = (FRAMES * PACKETS * 4 / 5) as usize;
    let deadline = start + INTERVAL * FRAMES + Duration::from_secs(1);
    while viewer.count(host.id) < minimum && Instant::now() < deadline {
        host.step();
        viewer.step();
        tokio::time::sleep(Duration::from_millis(1)).await;
    }
    let received = viewer.count(host.id);
    assert!(
        received >= minimum,
        "received {received} of {} RTP packets",
        FRAMES * PACKETS
    );
}

/// At its traffic budget the SFU refuses new peers, and the count is on disk.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_traffic_budget_refuses_new_video() {
    let sfu = start("traffic_budget = 30000\nperiod = \"day\"\nstate_directory = \"{directory}\"");
    let url = sfu.url.as_str();
    let (mut host, _) = join(url, &ADMIT, true, true).await;
    let (mut receiver, _) = join(url, &ADMIT, false, false).await;
    host.start_publishing(Codec::Av1);
    // About 20 kB a second reach the receiver; the count is saved each second.
    let count = sfu.directory.path().join("traffic");
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        for peer in [&mut host, &mut receiver] {
            peer.step();
        }
        host.send_frame();
        let sent = std::fs::read_to_string(&count)
            .ok()
            .and_then(|text| text.split_whitespace().nth(1)?.parse::<u64>().ok());
        if sent.is_some_and(|sent| sent >= 30_000) {
            break;
        }
        assert!(Instant::now() < deadline, "{sent:?}");
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    let (_, refused) = join(url, &ADMIT, false, false).await;
    assert_eq!(refused, json!({"type": "error", "reason": "traffic"}));
}
