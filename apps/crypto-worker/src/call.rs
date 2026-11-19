// SPDX-License-Identifier: EUPL-1.2
//! One ephemeral call and its two-peer signaling exchange. The session moves
//! opaque messages between workers. This worker keeps the cryptographic state.

use nuschl_ipc::crypto::call::{Action, Create, Outcome, Reply, Request};
use nuschl_stream::{Link, Publisher, PublisherEvent, Subscriber, SubscriberEvent, SubscriberId};

#[derive(Default)]
pub(crate) struct Signaling {
    generation: u64,
    call: Option<Call>,
}

struct Call {
    link: Link,
    relay_only: bool,
    pair: Option<Pair>,
}

struct Pair {
    peer: u64,
    keys: Keys,
}

enum Keys {
    Publisher {
        exchange: Publisher,
        subscriber: Option<SubscriberId>,
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
            self.call = Some(Call {
                link,
                relay_only,
                pair: None,
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
                .handle(action, now)?
        };
        Ok(Reply {
            generation,
            outcome,
        })
    }
}

impl Call {
    fn handle(&mut self, action: Action, now: u64) -> Result<Outcome, String> {
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
                pair.receive(&data, now)
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

impl Pair {
    fn receive(&mut self, data: &[u8], now: u64) -> Result<Outcome, String> {
        let peer = self.peer;
        match &mut self.keys {
            Keys::Publisher {
                exchange,
                subscriber,
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
                    let [(_, data)] = rekey
                        .messages
                        .try_into()
                        .map_err(|_| "missing call grant")?;
                    *subscriber = Some(id);
                    Ok(Outcome::Send {
                        peer,
                        data,
                        ready: false,
                    })
                }
                PublisherEvent::Confirmed(_) => Ok(Outcome::Ready { peer }),
            },
            Keys::Subscriber(subscriber) => match subscriber
                .receive(data, now, &mut getrandom::SysRng)
                .map_err(text)?
            {
                SubscriberEvent::Granted { ack, .. } => Ok(Outcome::Send {
                    peer,
                    data: ack,
                    ready: true,
                }),
                SubscriberEvent::Rekeyed(_) => Ok(Outcome::Done),
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
            } => exchange.open_direct(id, data),
            Self::Subscriber(subscriber) => subscriber.open_direct(data),
            _ => return Err("call peer is waiting for a grant".into()),
        }
        .map_err(text)
    }
}

fn text(error: nuschl_stream::Error) -> String {
    error.to_string()
}
