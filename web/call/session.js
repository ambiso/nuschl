// SPDX-License-Identifier: EUPL-1.2
// One call on the page (docs/video-plan.md §2, §4): admission at the SFU, the
// subscribe connection with each publisher's frames decrypted in the stream worker,
// who is in the call, and this page's own camera and screen on the publish
// connection, encrypted in the stream worker and encoded in the call codec.
import { MAX_PEERS, join, parse } from "./signal.js";
import { preferences, sendCodecs } from "./support.js";

/** A publisher whose keys have not arrived is asked again after this. */
const RESUBSCRIBE_MS = 5000;
/** The stream worker's rekey clock while publishing (crates/stream-wasm). */
const TICK_MS = 250;
/** Each source's SFrame encoding (crates/sframe, below MAX_ENCODINGS). */
const ENCODINGS = { camera: 0, screen: 1 };

export class Call extends EventTarget {
  /** The other peers: id → role. */
  peers = new Map();
  /** Forwarded sources by mid on the subscribe connection: {peer, source, screen, kind,
   * receiver, stream, quality}. `quality` is "auto": the SFU's choice. Q1 (#414) adds the
   * tier ladder and sends each source's quality request from here. */
  tracks = new Map();
  /** Publishers asked for their keys: id → {keyed, asked}. */
  keys = new Map();
  /** This page's sources: "camera" or "screen" → {track, transceiver, stream, quality}.
   * `quality` is "auto"; Q1 (#414) adds the tier ladder and caps the encoding here. */
  sources = new Map();
  self = null;
  role = null;
  codec = null;
  /** Why the call ended for this page; null while it lasts. */
  ended = null;

  /** `stream`: a StreamClient. `codecs`: the codecs this browser decodes. */
  constructor(stream, { codecs, relay }) {
    super();
    this.stream = stream;
    this.codecs = codecs;
    this.relay = relay;
  }

  /** Opens the call `link` on the stream host `host` and joins it. */
  async start({ link, host }) {
    try {
      this.opened = await this.stream.request({ kind: "open", link, host });
    } catch {
      return this.end("link");
    }
    this.socket = new WebSocket(`wss://${host}/sfu`);
    let inbox = Promise.resolve();
    this.socket.addEventListener("message", ({ data }) => {
      inbox = inbox.then(() => this.receive(parse(data))).catch((error) => {
        console.error("call:", error);
        this.end("lost");
      });
    });
    // A close ends the call once the messages before it are handled: the last may say why.
    this.socket.addEventListener("close", () => {
      inbox = inbox.then(() => this.end("lost"));
    });
    this.timer = setInterval(() => this.resubscribe(), RESUBSCRIBE_MS);
  }

  get creator() {
    return Boolean(this.opened?.creator);
  }

  send(message) {
    if (this.socket?.readyState === WebSocket.OPEN) this.socket.send(JSON.stringify(message));
  }

  /** Sends a stream worker reply's key messages and notes its key events. */
  deliver({ send, events }) {
    for (const { to, data } of send) this.send({ type: "key", to, data });
    for (const event of events) {
      const keys = this.keys.get(event.peer);
      if (event.kind === "keyed" && keys) keys.keyed = true;
    }
  }

  async receive(message) {
    switch (message?.type) {
      case "hello":
        this.send(join(this.opened, await this.stream.request({ kind: "admit", nonce: message.nonce }), this.codecs));
        return;
      case "welcome":
        if (this.relay && !message.turn) return this.end("relay");
        this.self = message.peer;
        this.role = message.role;
        this.codec = message.codec;
        for (const { id, role } of message.peers) this.peers.set(id, role);
        this.configuration = {
          iceTransportPolicy: this.relay ? "relay" : "all",
          iceServers: message.turn ? [message.turn] : [],
        };
        this.subscriber = new RTCPeerConnection(this.configuration);
        this.subscriber.addEventListener("connectionstatechange", () => {
          if (this.subscriber.connectionState === "failed") this.end("lost");
        });
        break;
      case "joined":
        if (this.peers.size < MAX_PEERS) this.peers.set(message.peer, message.role);
        break;
      case "left":
        this.peers.delete(message.peer);
        this.keys.delete(message.peer);
        for (const [mid, track] of this.tracks) if (track.peer === message.peer) this.tracks.delete(mid);
        await this.stream.request({ kind: "leave", peer: message.peer });
        break;
      case "codec":
        this.codec = message.codec;
        if (this.sources.size) await this.recodec();
        break;
      case "answer":
        if (!this.publisher) throw new Error("answer without an offer");
        await this.publisher.setRemoteDescription({ type: "answer", sdp: message.sdp });
        this.offering = false;
        if (this.again) await this.offer();
        break;
      case "offer":
        await this.answer(message);
        break;
      case "key":
        // A key message that fails leaves the keys as they were; resubscribe asks again.
        this.deliver(await this.stream.request({ kind: "key", from: message.from, data: message.data }).catch(
          () => ({ send: [], events: [] }),
        ));
        break;
      case "error":
        return this.end(message.reason);
      default:
        if (message) this.dispatchEvent(new CustomEvent("message", { detail: message }));
        return;
    }
    this.changed();
  }

  /** Answers the SFU's offer on the subscribe connection; every receiver decrypts
   * in the stream worker under its publisher's keys. */
  async answer({ sdp, tracks }) {
    const pc = this.subscriber;
    if (!pc) throw new Error("offer before welcome");
    const offered = new Map(tracks.map((track) => [track.mid, track]));
    await pc.setRemoteDescription({ type: "offer", sdp });
    const current = new Map();
    for (const transceiver of pc.getTransceivers()) {
      const track = offered.get(transceiver.mid);
      if (!track || transceiver.currentDirection === "stopped") continue;
      const { receiver } = transceiver;
      const known = this.tracks.get(transceiver.mid);
      if (!known || known.receiver !== receiver) {
        receiver.transform = new RTCRtpScriptTransform(this.stream.worker, { side: "receive", peer: track.peer });
        try {
          transceiver.setCodecPreferences(preferences(globalThis, receiver.track.kind));
        } catch {
          // The browser keeps its own order.
        }
      }
      current.set(transceiver.mid, known?.receiver === receiver ? known : {
        peer: track.peer,
        source: track.source,
        screen: track.screen,
        kind: receiver.track.kind,
        receiver,
        stream: new MediaStream([receiver.track]),
        quality: "auto",
      });
    }
    this.tracks = current;
    const answer = await pc.createAnswer();
    await pc.setLocalDescription(answer);
    this.send({ type: "answer", sdp: answer.sdp });
    const publishers = new Set([...current.values()].map((track) => track.peer));
    for (const peer of this.keys.keys()) if (!publishers.has(peer)) this.keys.delete(peer);
    for (const peer of publishers) if (!this.keys.has(peer)) await this.subscribe(peer);
  }

  async subscribe(peer) {
    this.keys.set(peer, { keyed: false, asked: Date.now() });
    this.deliver(await this.stream.request({ kind: "subscribe", peer }).catch(() => ({ send: [], events: [] })));
  }

  resubscribe() {
    for (const [peer, { keyed, asked }] of this.keys) {
      if (!keyed && Date.now() - asked >= RESUBSCRIBE_MS) this.subscribe(peer);
    }
  }

  /** Whether this browser encodes the call codec. */
  get canPublish() {
    return this.codec !== null && sendCodecs(globalThis, this.codec).length > 0;
  }

  /** Publishes `track`, from the user's own Start camera or Share screen, as this
   * page's `kind` source ("camera" or "screen"). */
  async publish(kind, track) {
    if (this.ended || this.sources.has(kind)) return track.stop();
    if (!this.publisher) {
      this.deliver(await this.stream.request({ kind: "publish" }));
      this.publisher = new RTCPeerConnection(this.configuration);
      // An unused data channel, offered first, holds the transport: stopping a source's
      // media section, also its first, leaves it up.
      this.publisher.createDataChannel("sfu");
      await this.offer();
      this.ticker = setInterval(() => {
        this.stream.request({ kind: "tick" }).then((reply) => this.deliver(reply), () => {});
      }, TICK_MS);
    }
    const transceiver = this.sender(kind, track);
    this.sources.set(kind, { track, transceiver, stream: new MediaStream([track]), quality: "auto" });
    // The browser's own Stop sharing, or an unplugged camera.
    track.addEventListener("ended", () => this.unpublish(kind));
    this.changed();
    await this.offer();
  }

  /** A send transceiver for `track`, encrypted in the stream worker. */
  sender(kind, track) {
    const transceiver = this.publisher.addTransceiver(track, { direction: "sendonly" });
    transceiver.sender.transform = new RTCRtpScriptTransform(this.stream.worker, {
      side: "send",
      encoding: ENCODINGS[kind],
    });
    transceiver.codec = this.codec;
    return transceiver;
  }

  /** Restarts each source's encoder in the new call codec on a new transceiver: the
   * SFU keeps a media section's first codec. The first frame is a key frame, and
   * the encryptors continue, so no SFrame nonce repeats. */
  async recodec() {
    for (const [kind, source] of this.sources) {
      if (source.transceiver.codec === this.codec) continue;
      source.transceiver.stop();
      source.transceiver = this.sender(kind, source.track);
    }
    await this.offer();
  }

  /** Stops this page's `kind` source: the capture ends and the SFU drops its tracks. */
  async unpublish(kind) {
    const source = this.sources.get(kind);
    if (!source) return;
    this.sources.delete(kind);
    source.track.stop();
    source.transceiver.stop();
    this.changed();
    await this.offer();
  }

  /** Offers the publish connection in the call codec, one offer at a time. */
  async offer() {
    if (this.offering) {
      this.again = true;
      return;
    }
    this.offering = true;
    this.again = false;
    const codecs = sendCodecs(globalThis, this.codec);
    for (const { transceiver } of this.sources.values()) transceiver.setCodecPreferences(codecs);
    await this.publisher.setLocalDescription();
    const screen = this.sources.get("screen")?.transceiver.mid;
    this.send({ type: "offer", sdp: this.publisher.localDescription.sdp, screens: screen ? [screen] : [] });
  }

  /** Host actions (P): end the call for everyone, or remove one peer. */
  endForEveryone() {
    this.send({ type: "end" });
  }

  remove(peer) {
    this.send({ type: "remove", peer });
  }

  /** Ends the call for this page; the first reason stays. */
  end(reason) {
    if (this.ended) return;
    this.ended = reason;
    clearInterval(this.timer);
    clearInterval(this.ticker);
    for (const { track } of this.sources.values()) track.stop();
    this.sources.clear();
    this.socket?.close();
    this.subscriber?.close();
    this.publisher?.close();
    this.changed();
  }

  changed() {
    this.dispatchEvent(new Event("change"));
  }
}
