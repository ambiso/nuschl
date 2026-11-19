// SPDX-License-Identifier: EUPL-1.2
// The call link and the SFU's signalling as the call page sees them
// (apps/stream-sfu/src/protocol.rs): every message from the SFU is checked
// against the page's bounds before it is used.

/** The SFU's largest signalling message (`max_message_bytes`). */
export const MAX_MESSAGE = 256 * 1024;
/** Peers in a call, the stream worker's limit. */
export const MAX_PEERS = 256;
/** Forwarded tracks, three sources per peer. */
export const MAX_TRACKS = 3 * MAX_PEERS;
/** A key message in hex (16 KiB). */
const MAX_KEY = 2 * 16 * 1024;

const id = (value) => Number.isSafeInteger(value) && value >= 0;
const text = (max) => (value) => typeof value === "string" && value.length <= max;
const hex = (min, max) => (value) =>
  text(max)(value) && value.length >= min && value.length % 2 === 0 && /^[0-9a-f]*$/.test(value);
const oneOf = (...values) => (value) => values.includes(value);
const list = (max, item) => (value) => Array.isArray(value) && value.length <= max && value.every(item);
const object = (fields) => (value) =>
  value !== null && typeof value === "object" && !Array.isArray(value) &&
  Object.entries(fields).every(([name, check]) => check(value[name]));
const nullable = (check) => (value) => value === null || check(value);

const role = oneOf("host", "participant");
const codec = oneOf("av1", "vp9");
const sdp = text(MAX_MESSAGE);
const mid = text(16);
const messages = {
  hello: { nonce: hex(64, 64) },
  welcome: {
    peer: id,
    role,
    peers: list(MAX_PEERS, object({ id, role })),
    codec,
    turn: nullable(object({ urls: list(8, text(256)), username: text(256), credential: text(256) })),
  },
  joined: { peer: id, role },
  left: { peer: id },
  codec: { codec },
  offer: { sdp, tracks: list(MAX_TRACKS, object({ mid, peer: id, source: mid, kind: oneOf("camera", "screen", "audio") })) },
  answer: { sdp },
  key: { from: id, data: hex(2, MAX_KEY) },
  error: { reason: text(64) },
};

/** The SFU message in `data`, or null for a type the page does not use. Throws when
 * the message breaks a bound. */
export function parse(data) {
  if (typeof data !== "string" || data.length > MAX_MESSAGE) throw new Error("message too long");
  const message = JSON.parse(data);
  if (!object({ type: text(16) })(message)) throw new Error("malformed message");
  if (!Object.hasOwn(messages, message.type)) return null;
  if (!object(messages[message.type])(message)) throw new Error(`malformed ${message.type}`);
  return message;
}

/** What the stream worker's `open` reads from the page's address: the call link
 * `https://<host>/call#v=1&k=…`, and relay-only from `/call?relay#…`. */
export function linkOf(location) {
  return {
    link: `${location.origin}${location.pathname}${location.hash}`,
    host: location.host,
    relay: new URLSearchParams(location.search).has("relay"),
  };
}

/** The SFU `join` from the stream worker's `opened` and `admitted` replies and the
 * codecs this browser decodes. */
export function join(opened, admitted, codecs) {
  return {
    type: "join",
    room: opened.room,
    verifier: opened.verifier,
    proof: admitted.proof,
    host_proof: admitted.host_proof,
    create: admitted.create,
    codecs,
  };
}
