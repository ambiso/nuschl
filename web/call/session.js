// SPDX-License-Identifier: EUPL-1.2
// One call on the page (docs/video-plan.md §2, §4): admission at the SFU, the
// subscribe connection with each publisher's frames decrypted in the stream worker,
// and who is in the call. Publishing (B1) adds a publish connection with
// `configuration`, sends its `offer`, takes the SFU's `answer` from the "message"
// event and passes the stream worker's `publish` and `tick` replies to `deliver`.
import { MAX_PEERS, join, parse } from "./signal.js";
import { preferences } from "./support.js";

/** A publisher whose keys have not arrived is asked again after this. */
const RESUBSCRIBE_MS = 5000;

export class Call extends EventTarget {
  /** The other peers: id → role. */
  peers = new Map();
  /** Forwarded sources by mid on the subscribe connection: {peer, source, screen, kind,
   * receiver, stream, quality}. `quality` is "auto": the SFU's choice. Q1 (#414) adds the
   * tier ladder and sends each source's quality request from here. */
  tracks = new Map();
  /** Publishers asked for their keys: id → {keyed, asked}. */
  keys = new Map();
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
    this.socket?.close();
    this.subscriber?.close();
    this.changed();
  }

  changed() {
    this.dispatchEvent(new Event("change"));
  }
}
