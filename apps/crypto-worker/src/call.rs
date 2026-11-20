// SPDX-License-Identifier: EUPL-1.2
//! One ephemeral call and its two-peer signaling exchange. The session moves
//! opaque messages between workers. This worker keeps the cryptographic state.

use nuschl_ipc::crypto::call::{Action, Create, Outcome, Reply, Request};
use nuschl_ipc::crypto::epoch::{Direction, Update};
use nuschl_stream::{
    EpochKey, Link, Publisher, PublisherEvent, Subscriber, SubscriberEvent, SubscriberId,
};
use std::io::Write;
#[cfg(target_os = "linux")]
use std::{
    net::Shutdown,
    os::{fd::OwnedFd, unix::net::UnixStream},
    time::Duration,
};
use zeroize::Zeroize as _;

#[derive(Default)]
pub(crate) struct Signaling {
    generation: u64,
    call: Option<Call>,
    #[cfg(any(target_os = "macos", windows))]
    endpoint: Option<std::fs::File>,
}

struct Call {
    link: Link,
    relay_only: bool,
    pair: Option<Pair>,
    media: Option<Box<dyn Write + Send>>,
}

struct Pair {
    peer: u64,
    keys: Keys,
}

enum Keys {
    Publisher {
        exchange: Publisher,
        subscriber: Option<SubscriberId>,
        pending_media: Option<Update>,
    },
    Subscriber(Subscriber),
}

impl Signaling {
    pub(crate) fn handle(&mut self, request: Request, now: u64) -> Result<Reply, String> {
        let Request { generation, action } = request;
        let outcome = if let Action::Open {
            host,
            link,
            relay_only,
        } = action
        {
            if generation <= self.generation {
                return Err("call generation is already in use".into());
            }
            let link = Link::parse(&link, &host).map_err(text)?;
            let outcome = Outcome::Opened {
                room: *link.keys().stream_id(),
                verifier: *link.room_check(),
            };
            self.generation = generation;
            #[cfg(any(target_os = "macos", windows))]
            let media = self
                .endpoint
                .take()
                .map(|pipe| Box::new(pipe) as Box<dyn Write + Send>);
            #[cfg(not(any(target_os = "macos", windows)))]
            let media = None;
            self.call = Some(Call {
                link,
                relay_only,
                pair: None,
                media,
            });
            outcome
        } else if matches!(action, Action::Close)
            && (self.call.is_none() || generation == self.generation)
        {
            self.call = None;
            Outcome::Done
        } else {
            if generation != self.generation {
                return Err("stale call generation".into());
            }
            self.call
                .as_mut()
                .ok_or("no open call")?
                .handle(generation, action, now)?
        };
        Ok(Reply {
            generation,
            outcome,
        })
    }
    #[cfg(target_os = "linux")]
    pub(crate) fn attach_media(&mut self, generation: u64, fd: OwnedFd) -> Result<Reply, String> {
        if generation != self.generation {
            return Err("stale call media generation".into());
        }
        let call = self.call.as_mut().ok_or("no open call")?;
        if call.media.is_some() || call.pair.is_some() {
            return Err("call media endpoint is already committed".into());
        }
        let media = nuschl_ipc::crypto::descriptor::connected_stream(fd)
            .map_err(|_| "invalid call media endpoint")?;
        media
            .shutdown(Shutdown::Read)
            .map_err(|_| "invalid call media endpoint")?;
        media
            .set_write_timeout(Some(Duration::from_millis(100)))
            .map_err(|_| "cannot bound call media writes")?;
        call.media = Some(Box::new(media));
        Ok(Reply {
            generation,
            outcome: Outcome::Done,
        })
    }
}

impl Call {
    fn handle(&mut self, generation: u64, action: Action, now: u64) -> Result<Outcome, String> {
        match action {
            Action::Admit { nonce } => {
                let (proof, host_proof) = self.link.admission_proofs(&nonce);
                let create = self.link.publisher().map(|secret| Create {
                    admit_key: *self.link.keys().admit_key(),
                    host_key: nuschl_stream::host_key(secret),
                });
                Ok(Outcome::Admitted {
                    proof,
                    host_proof,
                    create,
                })
            }
            Action::Pair { peer, publisher } => {
                if self.pair.is_some() {
                    return Err("call peer already paired".into());
                }
                let keys = self.link.keys();
                let (keys, reply) = if publisher {
                    let exchange = Publisher::new(keys);
                    (
                        Keys::Publisher {
                            exchange,
                            subscriber: None,
                            pending_media: None,
                        },
                        Outcome::Done,
                    )
                } else {
                    let (subscriber, data) =
                        Subscriber::join(keys, now, &mut getrandom::SysRng).map_err(text)?;
                    (
                        Keys::Subscriber(subscriber),
                        Outcome::Send {
                            peer,
                            data,
                            ready: false,
                        },
                    )
                };
                self.pair = Some(Pair { peer, keys });
                Ok(reply)
            }
            Action::Key { peer, data } => {
                if nuschl_stream::direct_for_publisher(&data).is_some() {
                    self.direct_allowed()?;
                    let data = self.pair(peer)?.keys.open(&data)?;
                    return Ok(Outcome::Signal { peer, data });
                }
                let pair = self.pair(peer)?;
                let (reply, update) = pair.receive(generation, &data, now)?;
                self.deliver(update)?;
                Ok(reply)
            }
            Action::Seal { peer, data } => {
                self.direct_allowed()?;
                let data = self.pair(peer)?.keys.seal(&data)?;
                Ok(Outcome::Send {
                    peer,
                    data,
                    ready: false,
                })
            }
            _ => Err("invalid call action".into()),
        }
    }

    fn deliver(&mut self, update: Option<Update>) -> Result<(), String> {
        if let (Some(media), Some(update)) = (&mut self.media, &update)
            && nuschl_ipc::crypto::epoch::write(media, update).is_err()
        {
            self.media = None;
            return Err("RTC media endpoint closed".into());
        }
        drop(update);
        Ok(())
    }

    fn direct_allowed(&self) -> Result<(), String> {
        if self.relay_only {
            Err("direct signaling disabled".into())
        } else {
            Ok(())
        }
    }

    fn pair(&mut self, peer: u64) -> Result<&mut Pair, String> {
        self.pair
            .as_mut()
            .filter(|pair| pair.peer == peer)
            .ok_or_else(|| "unknown call peer".into())
    }
}

/// Serve one ephemeral call with a separate inherited write-only epoch pipe.
#[cfg(any(target_os = "macos", windows))]
pub fn serve() -> Result<(), Box<dyn std::error::Error>> {
    use nuschl_ipc::{
        Envelope, crypto::call::MAX_REPLY_FRAME_BYTES, read_frame_within, write_frame_within,
    };
    use std::io;
    #[cfg(target_os = "macos")]
    let endpoint = {
        use std::os::fd::AsFd as _;
        std::fs::File::from(io::stderr().as_fd().try_clone_to_owned()?)
    };
    #[cfg(windows)]
    let endpoint = {
        use std::os::windows::io::AsHandle as _;
        std::fs::File::from(io::stderr().as_handle().try_clone_to_owned()?)
    };
    let mut signaling = Signaling {
        endpoint: Some(endpoint),
        ..Signaling::default()
    };
    let start = std::time::Instant::now();
    let mut input = io::stdin().lock();
    let mut output = io::stdout().lock();
    while let Some(request) = read_frame_within::<Request>(&mut input, MAX_REPLY_FRAME_BYTES)? {
        request.payload.check()?;
        let closing = matches!(request.payload.action, Action::Close);
        if signaling.generation != 0 && matches!(request.payload.action, Action::Open { .. }) {
            return Err("the call key worker already has a call".into());
        }
        let now = u64::try_from(start.elapsed().as_millis())?;
        let answer = signaling.handle(request.payload, now)?;
        write_frame_within(
            &mut output,
            &Envelope::new(request.request_id, answer),
            MAX_REPLY_FRAME_BYTES,
        )?;
        if closing {
            break;
        }
    }
    Ok(())
}

impl Pair {
    fn receive(
        &mut self,
        generation: u64,
        data: &[u8],
        now: u64,
    ) -> Result<(Outcome, Option<Update>), String> {
        let peer = self.peer;
        match &mut self.keys {
            Keys::Publisher {
                exchange,
                subscriber,
                pending_media,
            } => match exchange.receive(data, now).map_err(text)? {
                PublisherEvent::Requested(id) => {
                    if subscriber.is_some() {
                        exchange.remove(&id);
                        return Err("call peer already granted".into());
                    }
                    let rekey = match exchange.rekey(now, &mut getrandom::SysRng) {
                        Ok(rekey) => rekey,
                        Err(error) => {
                            exchange.remove(&id);
                            return Err(text(error));
                        }
                    };
                    let update = media_update(generation, peer, Direction::Send, rekey.key);
                    let [(_, data)] = rekey
                        .messages
                        .try_into()
                        .map_err(|_| "missing call grant")?;
                    *subscriber = Some(id);
                    *pending_media = Some(update);
                    Ok((
                        Outcome::Send {
                            peer,
                            data,
                            ready: false,
                        },
                        None,
                    ))
                }
                PublisherEvent::Confirmed(_) => Ok((Outcome::Ready { peer }, pending_media.take())),
            },
            Keys::Subscriber(subscriber) => match subscriber
                .receive(data, now, &mut getrandom::SysRng)
                .map_err(text)?
            {
                SubscriberEvent::Granted { ack, key } => Ok((
                    Outcome::Send {
                        peer,
                        data: ack,
                        ready: true,
                    },
                    Some(media_update(generation, peer, Direction::Receive, key)),
                )),
                SubscriberEvent::Rekeyed(key) => Ok((
                    Outcome::Done,
                    Some(media_update(generation, peer, Direction::Receive, key)),
                )),
            },
        }
    }
}

impl Keys {
    fn seal(&mut self, data: &[u8]) -> Result<Vec<u8>, String> {
        match self {
            Self::Publisher {
                exchange,
                subscriber: Some(id),
                ..
            } => exchange.seal_direct(id, data),
            Self::Subscriber(subscriber) => subscriber.seal_direct(data),
            _ => return Err("call peer is waiting for a grant".into()),
        }
        .map_err(text)
    }

    fn open(&mut self, data: &[u8]) -> Result<Vec<u8>, String> {
        match self {
            Self::Publisher {
                exchange,
                subscriber: Some(id),
                ..
            } => exchange.open_direct(id, data),
            Self::Subscriber(subscriber) => subscriber.open_direct(data),
            _ => return Err("call peer is waiting for a grant".into()),
        }
        .map_err(text)
    }
}

fn media_update(generation: u64, peer: u64, direction: Direction, mut key: EpochKey) -> Update {
    let update = Update {
        generation,
        peer,
        direction,
        epoch: key.epoch,
        key: key.key,
    };
    key.key.zeroize();
    update
}

fn text(error: nuschl_stream::Error) -> String {
    error.to_string()
}
