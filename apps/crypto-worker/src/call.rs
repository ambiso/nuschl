// SPDX-License-Identifier: EUPL-1.2
//! One ephemeral call and its independent key exchanges. The session moves
//! opaque messages between workers. This worker keeps the cryptographic state.

use nuschl_ipc::crypto::call::{
    Action, Create, KeyMessage, MAX_MEDIA_PEERS, Outcome, Reply, Request,
};
use nuschl_ipc::crypto::epoch::{Direction, Update};
use nuschl_stream::{EpochKey, Link, Publishing, Subscriber, SubscriberEvent};
use std::{collections::BTreeMap, io::Write};
#[cfg(target_os = "linux")]
use std::{net::Shutdown, os::fd::OwnedFd, time::Duration};
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
    publishing: Option<(u64, Publishing)>,
    subscribers: BTreeMap<u64, Subscriber>,
    media: Option<Box<dyn Write + Send>>,
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
                publishing: None,
                subscribers: BTreeMap::new(),
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
        if call.media.is_some() || call.publishing.is_some() || !call.subscribers.is_empty() {
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
            Action::Publish { peer } => {
                if self.publishing.is_some() || self.subscribers.contains_key(&peer) {
                    return Err("invalid call publisher".into());
                }
                let (keys, key) = Publishing::start(
                    self.link.keys(),
                    MAX_MEDIA_PEERS,
                    now,
                    &mut getrandom::SysRng,
                )
                .map_err(text)?;
                self.publishing = Some((peer, keys));
                self.deliver(Some(media_update(generation, peer, Direction::Send, key)))?;
                Ok(Outcome::Done)
            }
            Action::Subscribe { peer } => {
                if self.subscribers.contains_key(&peer) {
                    return Ok(Outcome::Done);
                }
                if self.subscribers.len() >= MAX_MEDIA_PEERS
                    || self.publishing.as_ref().is_some_and(|(id, _)| *id == peer)
                {
                    return Err("invalid call subscription".into());
                }
                let (subscriber, data) =
                    Subscriber::join(self.link.keys(), now, &mut getrandom::SysRng)
                        .map_err(text)?;
                self.subscribers.insert(peer, subscriber);
                Ok(Outcome::Send {
                    peer,
                    data,
                    ready: false,
                })
            }
            Action::Left { peer } => {
                if let Some((_, publishing)) = &mut self.publishing {
                    publishing.remove(peer);
                }
                if self.subscribers.remove(&peer).is_some() {
                    self.deliver(Some(Update {
                        generation,
                        peer,
                        direction: Direction::ForgetReceive,
                        epoch: 0,
                        key: [0; 32],
                    }))?;
                }
                Ok(Outcome::Done)
            }
            Action::Tick => {
                let Some((peer, publishing)) = &mut self.publishing else {
                    return Ok(Outcome::Done);
                };
                let delivery = publishing.tick(now, &mut getrandom::SysRng).map_err(text)?;
                let sender = *peer;
                self.deliver(
                    delivery
                        .key
                        .map(|key| media_update(generation, sender, Direction::Send, key)),
                )?;
                if delivery.messages.is_empty() {
                    Ok(Outcome::Done)
                } else {
                    Ok(Outcome::KeyBatch {
                        messages: delivery
                            .messages
                            .into_iter()
                            .map(|(peer, data)| KeyMessage { peer, data })
                            .collect(),
                    })
                }
            }
            Action::Key { peer, data } => self.receive(generation, peer, &data, now),
            Action::Seal {
                peer,
                publisher,
                data,
            } => {
                self.direct_allowed()?;
                let data = if publisher {
                    self.publisher()?.seal(peer, &data)
                } else {
                    self.subscriber(peer)?.seal_direct(&data)
                }
                .map_err(text)?;
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

    fn publisher(&mut self) -> Result<&mut Publishing, String> {
        self.publishing
            .as_mut()
            .map(|(_, keys)| keys)
            .ok_or_else(|| "call has no local publisher".into())
    }

    fn subscriber(&mut self, peer: u64) -> Result<&mut Subscriber, String> {
        self.subscribers
            .get_mut(&peer)
            .ok_or_else(|| "unknown call publisher".into())
    }

    fn receive(
        &mut self,
        generation: u64,
        peer: u64,
        data: &[u8],
        now: u64,
    ) -> Result<Outcome, String> {
        if let Some(publisher) = nuschl_stream::direct_for_publisher(data) {
            self.direct_allowed()?;
            let data = if publisher {
                self.publisher()?.open(peer, data)
            } else {
                self.subscriber(peer)?.open_direct(data)
            }
            .map_err(text)?;
            return Ok(Outcome::Signal {
                peer,
                publisher,
                data,
            });
        }
        if nuschl_stream::for_publisher(data) {
            return if self.publisher()?.receive(peer, data, now).map_err(text)? {
                Ok(Outcome::Ready { peer })
            } else {
                Ok(Outcome::Done)
            };
        }
        let (outcome, key) = match self
            .subscriber(peer)?
            .receive(data, now, &mut getrandom::SysRng)
            .map_err(text)?
        {
            SubscriberEvent::Granted { ack, key } => (
                Outcome::Send {
                    peer,
                    data: ack,
                    ready: true,
                },
                key,
            ),
            SubscriberEvent::Rekeyed(key) => (Outcome::Done, key),
        };
        self.deliver(Some(media_update(
            generation,
            peer,
            Direction::Receive,
            key,
        )))?;
        Ok(outcome)
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
    let mut input = io::stdin().lock();
    let mut output = io::stdout().lock();
    while let Some(request) = read_frame_within::<Request>(&mut input, MAX_REPLY_FRAME_BYTES)? {
        request.payload.check()?;
        let closing = matches!(request.payload.action, Action::Close);
        if signaling.generation != 0 && matches!(request.payload.action, Action::Open { .. }) {
            return Err("the call key worker already has a call".into());
        }
        let answer = signaling
            .handle(request.payload, crate::worker::now_ms())
            .ok();
        let close = closing && answer.is_some();
        write_frame_within(
            &mut output,
            &Envelope::new(request.request_id, answer),
            MAX_REPLY_FRAME_BYTES,
        )?;
        if close {
            break;
        }
    }
    Ok(())
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
