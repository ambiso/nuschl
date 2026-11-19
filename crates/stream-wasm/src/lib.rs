// SPDX-License-Identifier: EUPL-1.2
//! The call page's stream worker (docs/video-plan.md §4, "Where keys live"): the key
//! exchange of `nuschl-stream` and the SFrame of `nuschl-sframe` behind one message
//! interface. One dedicated Web Worker per call loads this module; it is also the
//! encoded-transform worker (`RTCRtpScriptTransform`) of the call's senders and receivers.
//! It holds S, P and every key. The page receives the SFU's join fields and proofs,
//! encrypted key messages and frames, and locally opened direct signals for WebRTC.
//!
//! # Messages
//!
//! [`StreamWorker::handle`] takes one JSON request and the time (`Date.now()`) and returns
//! one JSON reply or throws an error string. Requests and replies carry `"v": 1` and a
//! `kind`; unknown versions, kinds and fields are errors. Binary values are lowercase hex,
//! as in the SFU's signalling (`apps/stream-sfu/src/protocol.rs`), so key messages pass
//! between the SFU's `key` messages and the worker unchanged. `peer` is the SFU's peer id.
//!
//! ```text
//! request                  reply
//! open {link, host, allow_direct?}   opened {room, verifier, creator}
//! admit {nonce}            admitted {proof, host_proof, create}
//! publish                  done {send, events}
//! subscribe {peer}         done
//! key {from, data}         done
//! direct {peer, publisher, data}  done
//! leave {peer}             done
//! tick                     done
//!
//! done   = {send: [{to, data}], events: [keyed {peer, epoch} | confirmed {peer} | direct {peer, publisher, data}]}
//! create = {admit_key, host_key}       host_proof, create: null without P
//! ```
//!
//! - `open`: the call link (`location.href`) on the stream host `host` (`location.host`),
//!   once per worker. `room`, `verifier` and the `admit` fields are the SFU `join` fields;
//!   `creator` says whether the link carries P.
//! - `admit`: the `nonce` of the SFU's `hello`.
//! - `publish`: starts this participant's sender keys; frames encrypt at once.
//! - `subscribe`: asks the publisher `peer` for its keys, again after a failure.
//! - `key`: an SFU `key {from, data}`.
//! - `direct`: UTF-8 signaling sealed for `peer` under the established pairwise
//!   key. `publisher` says which side of that exchange this worker is on. The
//!   incoming `direct` event has the same fields. `allow_direct` defaults to false;
//!   relay-only calls keep it false, for both sealing and opening.
//! - `leave`: the SFU's `left {peer}`; the next `tick` locks `peer` out.
//! - `tick`: every 250 ms while publishing. Rekeys after joins and leaves and every
//!   [`REKEY_INTERVAL_MS`], sends each rekey's messages a second time, and moves the
//!   encryptors to the new epoch ([`nuschl_stream`]'s delivery rules).
//!
//! Send each `send` entry as the SFU message `key {to, data}`. `keyed`: frames from
//! `peer` decrypt under `epoch`. `confirmed`: `peer` holds this participant's keys.
//!
//! # Frames
//!
//! The transform handlers call [`StreamWorker::encrypt`] with a sender's encoding index
//! (below [`nuschl_sframe::MAX_ENCODINGS`]; each of a participant's sources and simulcast
//! layers takes its own) and [`StreamWorker::decrypt`] with the frame's publisher. A frame
//! that fails to encrypt is not sent.
//!
//! # Bounds and errors
//!
//! Requests are at most [`MAX_REQUEST`] bytes and key messages
//! [`nuschl_stream::MAX_KEY_MESSAGE`], checked before parsing. The worker keeps keys for at
//! most [`MAX_PEERS`] publishers and as many subscribers; `nuschl-stream` bounds pending
//! requests (32) and replays, `nuschl-sframe` frame sizes and replay windows. An error
//! leaves the call's keys as they were. A resent key message fails as replayed or
//! unexpected at its receiver. A `WebAssembly.RuntimeError` (a trap) ends the worker.
//!
//! Randomness comes from `crypto.getRandomValues`.

use std::collections::BTreeMap;
use std::fmt::Display;

use nuschl_sframe::{FrameDecryptor, FrameEncryptor, MAX_ENCODINGS};
use nuschl_stream::{
    EpochKey, Link, MAX_KEY_MESSAGE, Publisher, PublisherEvent, RESEND_AFTER_MS, SWITCH_AFTER_MS,
    Subscriber, SubscriberEvent, SubscriberId,
};
use rand_core::TryCryptoRng;
use serde::{Deserialize, Serialize};

#[cfg(test)]
mod tests;

pub const VERSION: u8 = 1;
/// The longest request: a key message in hex, and room for the rest.
pub const MAX_REQUEST: usize = 2 * MAX_KEY_MESSAGE + 1024;
/// Publishers this worker subscribes to, and subscribers to its own sources.
pub const MAX_PEERS: usize = 256;
/// The periodic rekey (docs/video-plan.md §4, "Rekeying").
pub const REKEY_INTERVAL_MS: u64 = 15 * 60 * 1000;
const ADMIT_LABEL: &[u8] = b"nuschl/stream/v2 sfu admit";
const HOST_LABEL: &[u8] = b"nuschl/stream/v2 sfu host";

#[derive(Deserialize)]
struct Envelope {
    v: u8,
    #[serde(flatten)]
    request: Request,
}

#[derive(Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum Request {
    Open {
        link: String,
        host: String,
        #[serde(default)]
        allow_direct: bool,
    },
    Admit {
        nonce: String,
    },
    Publish {},
    Subscribe {
        peer: u64,
    },
    Key {
        from: u64,
        data: String,
    },
    Direct {
        peer: u64,
        publisher: bool,
        data: String,
    },
    Leave {
        peer: u64,
    },
    Tick {},
}

#[derive(Serialize)]
struct Versioned {
    v: u8,
    #[serde(flatten)]
    reply: Reply,
}

#[derive(Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Reply {
    Opened {
        room: String,
        verifier: String,
        creator: bool,
    },
    Admitted {
        proof: String,
        host_proof: Option<String>,
        create: Option<Create>,
    },
    Done {
        send: Vec<Message>,
        events: Vec<Event>,
    },
}

#[derive(Serialize)]
pub struct Create {
    pub admit_key: String,
    pub host_key: String,
}

#[derive(Clone, Serialize)]
pub struct Message {
    pub to: u64,
    pub data: String,
}

#[derive(Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Event {
    Keyed {
        peer: u64,
        epoch: u32,
    },
    Confirmed {
        peer: u64,
    },
    Direct {
        peer: u64,
        publisher: bool,
        data: String,
    },
}

// -------------------------------------------------------------------------------------
// The worker

/// One call's keys, with randomness from `R`.
pub struct Worker<R> {
    rng: R,
    link: Option<Link>,
    publishing: Option<Publishing>,
    remotes: BTreeMap<u64, Remote>,
    allow_direct: bool,
}

/// A publisher this worker subscribes to.
struct Remote {
    subscriber: Subscriber,
    frames: FrameDecryptor,
}

/// This participant's sender keys.
struct Publishing {
    publisher: Publisher,
    /// Each peer's current subscription.
    peers: BTreeMap<u64, SubscriberId>,
    encryptors: [FrameEncryptor; MAX_ENCODINGS as usize],
    /// The last rekey until its encryptors are in use.
    next: Option<Next>,
    /// A join or leave waits for the next rekey.
    due: bool,
    last_rekey: u64,
}

struct Next {
    key: EpochKey,
    sent: u64,
    /// Emptied once sent a second time.
    messages: Vec<Message>,
}

impl<R: TryCryptoRng> Worker<R> {
    pub fn new(rng: R) -> Self {
        Self {
            rng,
            link: None,
            publishing: None,
            remotes: BTreeMap::new(),
            allow_direct: false,
        }
    }

    pub fn handle(&mut self, request: &str, now: u64) -> Result<String, String> {
        if request.len() > MAX_REQUEST {
            return Err("request too long".into());
        }
        let envelope: Envelope = serde_json::from_str(request).map_err(|_| "malformed request")?;
        if envelope.v != VERSION {
            return Err("unsupported version".into());
        }
        let reply = match envelope.request {
            Request::Open {
                link,
                host,
                allow_direct,
            } => self.open(&link, &host, allow_direct)?,
            Request::Admit { nonce } => self.admit(&nonce)?,
            Request::Publish {} => self.publish(now)?,
            Request::Subscribe { peer } => self.subscribe(peer, now)?,
            Request::Key { from, data } => self.key(from, &data, now)?,
            Request::Direct {
                peer,
                publisher,
                data,
            } => self.send_direct(peer, publisher, data.as_bytes())?,
            Request::Leave { peer } => self.leave(peer),
            Request::Tick {} => match &mut self.publishing {
                Some(publishing) => done(publishing.tick(now, &mut self.rng)?, Vec::new()),
                None => done(Vec::new(), Vec::new()),
            },
        };
        let reply = Versioned { v: VERSION, reply };
        Ok(serde_json::to_string(&reply).expect("replies serialize"))
    }

    pub fn encrypt(
        &mut self,
        encoding: u8,
        key_frame: bool,
        frame: &[u8],
    ) -> Result<Vec<u8>, String> {
        let publishing = self.publishing.as_mut().ok_or("not publishing")?;
        let encryptor = publishing
            .encryptors
            .get_mut(usize::from(encoding))
            .ok_or("unknown encoding")?;
        encryptor.encrypt(key_frame, frame).map_err(text)
    }

    pub fn decrypt(&mut self, peer: u64, frame: &[u8], now: u64) -> Result<Vec<u8>, String> {
        let remote = self.remotes.get_mut(&peer).ok_or("unknown peer")?;
        let (_, frame) = remote.frames.decrypt(frame, now).map_err(text)?;
        Ok(frame)
    }

    fn link(&self) -> Result<&Link, String> {
        self.link.as_ref().ok_or_else(|| "no call link".into())
    }

    fn open(&mut self, link: &str, host: &str, allow_direct: bool) -> Result<Reply, String> {
        if self.link.is_some() {
            return Err("call link already open".into());
        }
        let link = Link::parse(link, host).map_err(text)?;
        let reply = Reply::Opened {
            room: hex::encode(link.keys().stream_id()),
            verifier: hex::encode(link.room_check()),
            creator: link.publisher().is_some(),
        };
        self.link = Some(link);
        self.allow_direct = allow_direct;
        Ok(reply)
    }

    fn admit(&self, nonce: &str) -> Result<Reply, String> {
        let link = self.link()?;
        let mut bytes = [0; 32];
        hex::decode_to_slice(nonce, &mut bytes).map_err(|_| "malformed nonce")?;
        let keys = link.keys();
        let host_key = link.publisher().map(nuschl_stream::host_key);
        Ok(Reply::Admitted {
            proof: proof(keys.admit_key(), ADMIT_LABEL, &bytes),
            host_proof: host_key.map(|key| proof(&key, HOST_LABEL, &bytes)),
            create: host_key.map(|host_key| Create {
                admit_key: hex::encode(keys.admit_key()),
                host_key: hex::encode(host_key),
            }),
        })
    }

    fn publish(&mut self, now: u64) -> Result<Reply, String> {
        if self.publishing.is_some() {
            return Err("already publishing".into());
        }
        let mut publisher = Publisher::new(self.link()?.keys());
        let key = publisher.rekey(now, &mut self.rng).map_err(text)?.key;
        self.publishing = Some(Publishing {
            publisher,
            peers: BTreeMap::new(),
            encryptors: nuschl_sframe::encryptors(key.epoch, &key.key),
            next: None,
            due: false,
            last_rekey: now,
        });
        Ok(done(Vec::new(), Vec::new()))
    }

    fn subscribe(&mut self, peer: u64, now: u64) -> Result<Reply, String> {
        let link = self.link.as_ref().ok_or("no call link")?;
        if !self.remotes.contains_key(&peer) && self.remotes.len() >= MAX_PEERS {
            return Err("limit reached".into());
        }
        let (subscriber, request) =
            Subscriber::join(link.keys(), now, &mut self.rng).map_err(text)?;
        match self.remotes.get_mut(&peer) {
            Some(remote) => remote.subscriber = subscriber,
            None => {
                let frames = FrameDecryptor::new();
                self.remotes.insert(peer, Remote { subscriber, frames });
            }
        }
        Ok(done(vec![message(peer, &request)], Vec::new()))
    }

    fn key(&mut self, from: u64, data: &str, now: u64) -> Result<Reply, String> {
        if data.len() > 2 * MAX_KEY_MESSAGE {
            return Err(nuschl_stream::Error::TooLong.to_string());
        }
        let message = hex::decode(data).map_err(|_| "malformed key message")?;
        if let Some(publisher) = nuschl_stream::direct_for_publisher(&message) {
            return self.receive_direct(from, publisher, &message);
        }
        if nuschl_stream::for_publisher(&message) {
            let publishing = self.publishing.as_mut().ok_or("not publishing")?;
            let events = publishing.receive(from, &message, now)?;
            return Ok(done(Vec::new(), events));
        }
        let remote = self.remotes.get_mut(&from).ok_or("unknown peer")?;
        let (key, send) = match remote.subscriber.receive(&message, now, &mut self.rng) {
            Ok(SubscriberEvent::Granted { key, ack }) => (key, vec![self::message(from, &ack)]),
            Ok(SubscriberEvent::Rekeyed(key)) => (key, Vec::new()),
            Err(error) => return Err(error.to_string()),
        };
        remote
            .frames
            .install(key.epoch, &key.key, now)
            .map_err(text)?;
        let keyed = Event::Keyed {
            peer: from,
            epoch: key.epoch,
        };
        Ok(done(send, vec![keyed]))
    }

    fn send_direct(&mut self, peer: u64, publisher: bool, body: &[u8]) -> Result<Reply, String> {
        self.check_direct()?;
        let sealed = if publisher {
            let (publisher, subscriber) = self.publishing_pair(peer)?;
            publisher.seal_direct(&subscriber, body)
        } else {
            self.subscriber(peer)?.seal_direct(body)
        }
        .map_err(text)?;
        Ok(done(vec![message(peer, &sealed)], Vec::new()))
    }

    fn receive_direct(
        &mut self,
        peer: u64,
        publisher: bool,
        message: &[u8],
    ) -> Result<Reply, String> {
        self.check_direct()?;
        let body = if publisher {
            let (publisher, subscriber) = self.publishing_pair(peer)?;
            publisher.open_direct(&subscriber, message)
        } else {
            self.subscriber(peer)?.open_direct(message)
        }
        .map_err(text)?;
        let data = String::from_utf8(body).map_err(|_| "malformed direct signal")?;
        Ok(done(
            Vec::new(),
            vec![Event::Direct {
                peer,
                publisher,
                data,
            }],
        ))
    }

    fn check_direct(&self) -> Result<(), String> {
        self.allow_direct
            .then_some(())
            .ok_or_else(|| "direct signaling disabled".into())
    }

    fn publishing_pair(&mut self, peer: u64) -> Result<(&mut Publisher, SubscriberId), String> {
        let publishing = self.publishing.as_mut().ok_or("not publishing")?;
        let subscriber = *publishing.peers.get(&peer).ok_or("unknown peer")?;
        Ok((&mut publishing.publisher, subscriber))
    }

    fn subscriber(&mut self, peer: u64) -> Result<&mut Subscriber, String> {
        self.remotes
            .get_mut(&peer)
            .map(|remote| &mut remote.subscriber)
            .ok_or_else(|| "unknown peer".into())
    }

    fn leave(&mut self, peer: u64) -> Reply {
        self.remotes.remove(&peer);
        if let Some(publishing) = &mut self.publishing
            && let Some(subscriber) = publishing.peers.remove(&peer)
        {
            publishing.due |= publishing.publisher.remove(&subscriber);
        }
        done(Vec::new(), Vec::new())
    }
}

impl Publishing {
    fn receive(&mut self, from: u64, message: &[u8], now: u64) -> Result<Vec<Event>, String> {
        match self.publisher.receive(message, now).map_err(text)? {
            PublisherEvent::Requested(subscriber) => {
                if let Some(previous) = self.peers.get(&from) {
                    self.publisher.remove(previous);
                } else if self.peers.len() >= MAX_PEERS {
                    self.publisher.remove(&subscriber);
                    return Err("limit reached".into());
                }
                self.peers.insert(from, subscriber);
                self.due = true;
                Ok(Vec::new())
            }
            PublisherEvent::Confirmed(subscriber) => Ok(self
                .peer(&subscriber)
                .map(|peer| Event::Confirmed { peer })
                .into_iter()
                .collect()),
        }
    }

    fn peer(&self, subscriber: &SubscriberId) -> Option<u64> {
        self.peers
            .iter()
            .find_map(|(peer, known)| (known == subscriber).then_some(*peer))
    }

    fn tick<R: TryCryptoRng>(&mut self, now: u64, rng: &mut R) -> Result<Vec<Message>, String> {
        let mut send = Vec::new();
        if let Some(next) = &mut self.next {
            let elapsed = now.abs_diff(next.sent);
            if elapsed >= RESEND_AFTER_MS {
                send.append(&mut next.messages);
            }
            if elapsed >= SWITCH_AFTER_MS {
                self.switch();
            }
        }
        if !self.due && now.abs_diff(self.last_rekey) < REKEY_INTERVAL_MS {
            return Ok(send);
        }
        let rekey = match self.publisher.rekey(now, rng) {
            Err(nuschl_stream::Error::TooSoon) => return Ok(send),
            rekey => rekey.map_err(text)?,
        };
        // The epoch before goes into use now, and its messages out a second time.
        if let Some(next) = &mut self.next {
            send.append(&mut next.messages);
        }
        self.switch();
        self.due = false;
        self.last_rekey = now;
        let messages: Vec<Message> = rekey
            .messages
            .iter()
            .filter_map(|(subscriber, data)| Some(message(self.peer(subscriber)?, data)))
            .collect();
        send.extend(messages.iter().cloned());
        self.next = Some(Next {
            key: rekey.key,
            sent: now,
            messages,
        });
        if rekey.messages.is_empty() {
            // Without subscribers, nobody waits for the key.
            self.switch();
        }
        Ok(send)
    }

    /// Puts the next epoch's encryptors in use.
    fn switch(&mut self) {
        if let Some(Next { key, .. }) = self.next.take() {
            self.encryptors = nuschl_sframe::encryptors(key.epoch, &key.key);
        }
    }
}

fn done(send: Vec<Message>, events: Vec<Event>) -> Reply {
    Reply::Done { send, events }
}

fn message(to: u64, data: &[u8]) -> Message {
    Message {
        to,
        data: hex::encode(data),
    }
}

/// `HMAC-SHA256(key, label || nonce)`, the SFU's admission proofs.
fn proof(key: &[u8; 32], label: &[u8], nonce: &[u8; 32]) -> String {
    let mut tag = [0; 32];
    libcrux_hmac::hmac_sha2_256(&mut tag, key, &[label, nonce].concat());
    hex::encode(tag)
}

fn text(error: impl Display) -> String {
    error.to_string()
}

// -------------------------------------------------------------------------------------
// The browser binding

#[cfg(all(target_arch = "wasm32", target_os = "unknown"))]
mod browser {
    use std::fmt;

    use rand_core::{TryCryptoRng, TryRng};
    use wasm_bindgen::prelude::*;

    use crate::Worker;

    #[wasm_bindgen]
    extern "C" {
        #[wasm_bindgen(js_namespace = crypto, js_name = getRandomValues, catch)]
        fn get_random_values(buffer: &mut [u8]) -> Result<(), JsValue>;
    }

    /// `crypto.getRandomValues`, which fills at most 65,536 bytes a call; the key
    /// exchange draws at most 64.
    pub struct BrowserRng;

    impl TryRng for BrowserRng {
        type Error = fmt::Error;

        fn try_next_u32(&mut self) -> Result<u32, fmt::Error> {
            let mut bytes = [0; 4];
            self.try_fill_bytes(&mut bytes)?;
            Ok(u32::from_le_bytes(bytes))
        }

        fn try_next_u64(&mut self) -> Result<u64, fmt::Error> {
            let mut bytes = [0; 8];
            self.try_fill_bytes(&mut bytes)?;
            Ok(u64::from_le_bytes(bytes))
        }

        fn try_fill_bytes(&mut self, buffer: &mut [u8]) -> Result<(), fmt::Error> {
            get_random_values(buffer).map_err(|_| fmt::Error)
        }
    }

    impl TryCryptoRng for BrowserRng {}

    /// See the crate documentation. Peer ids and times are JavaScript numbers.
    #[wasm_bindgen]
    pub struct StreamWorker(Worker<BrowserRng>);

    #[wasm_bindgen]
    impl StreamWorker {
        #[wasm_bindgen(constructor)]
        #[allow(clippy::new_without_default)]
        pub fn new() -> Self {
            Self(Worker::new(BrowserRng))
        }

        pub fn handle(&mut self, request: &str, now: f64) -> Result<String, String> {
            self.0.handle(request, integer(now)?)
        }

        pub fn encrypt(
            &mut self,
            encoding: u8,
            key_frame: bool,
            frame: &[u8],
        ) -> Result<Vec<u8>, String> {
            self.0.encrypt(encoding, key_frame, frame)
        }

        pub fn decrypt(&mut self, peer: f64, frame: &[u8], now: f64) -> Result<Vec<u8>, String> {
            self.0.decrypt(integer(peer)?, frame, integer(now)?)
        }
    }

    /// A non-negative safe integer.
    fn integer(value: f64) -> Result<u64, String> {
        if value.fract() == 0. && (0. ..=9_007_199_254_740_991.).contains(&value) {
            Ok(value as u64)
        } else {
            Err("invalid number".into())
        }
    }
}

#[cfg(all(target_arch = "wasm32", target_os = "unknown"))]
pub use browser::StreamWorker;
