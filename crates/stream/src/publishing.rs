// SPDX-License-Identifier: EUPL-1.2
//! One publisher's roster and epoch delivery through the call's ordered signaling.

use crate::{
    EpochKey, Error, LinkKeys, Publisher, PublisherEvent, RESEND_AFTER_MS, SWITCH_AFTER_MS,
    SubscriberId,
};
use rand_core::TryCryptoRng;
use std::collections::BTreeMap;

pub const REKEY_INTERVAL_MS: u64 = 15 * 60 * 1000;

pub struct Publishing {
    publisher: Publisher,
    peers: BTreeMap<u64, SubscriberId>,
    max_peers: usize,
    next: Option<Next>,
    due: bool,
    last_rekey: u64,
}

struct Next {
    key: EpochKey,
    sent: u64,
    messages: Vec<(u64, Vec<u8>)>,
}

#[derive(Default)]
pub struct Delivery {
    pub messages: Vec<(u64, Vec<u8>)>,
    pub key: Option<EpochKey>,
}

impl Publishing {
    pub fn start(
        keys: LinkKeys,
        max_peers: usize,
        now: u64,
        rng: &mut (impl TryCryptoRng + ?Sized),
    ) -> Result<(Self, EpochKey), Error> {
        let mut publisher = Publisher::new(keys);
        let key = publisher.rekey(now, rng)?.key;
        Ok((
            Self {
                publisher,
                peers: BTreeMap::new(),
                max_peers,
                next: None,
                due: false,
                last_rekey: now,
            },
            key,
        ))
    }

    /// Returns whether this peer confirmed its grant.
    pub fn receive(&mut self, peer: u64, data: &[u8], now: u64) -> Result<bool, Error> {
        let previous = self.peers.get(&peer).copied();
        match self.publisher.receive_from(data, now, previous)? {
            PublisherEvent::Requested(subscriber) => {
                if previous.is_none() && self.peers.len() >= self.max_peers {
                    self.publisher.remove(&subscriber);
                    return Err(Error::LimitReached);
                }
                self.remove(peer);
                self.peers.insert(peer, subscriber);
                self.due = true;
                Ok(false)
            }
            PublisherEvent::Confirmed(_) => Ok(true),
        }
    }

    pub fn remove(&mut self, peer: u64) {
        if let Some(subscriber) = self.peers.remove(&peer) {
            self.due |= self.publisher.remove(&subscriber);
            if let Some(next) = &mut self.next {
                next.messages.retain(|(to, _)| *to != peer);
            }
        }
    }

    pub fn seal(&mut self, peer: u64, body: &[u8]) -> Result<Vec<u8>, Error> {
        let subscriber = self.peers.get(&peer).ok_or(Error::UnknownSubscriber)?;
        self.publisher.seal_direct(subscriber, body)
    }

    pub fn open(&mut self, peer: u64, message: &[u8]) -> Result<Vec<u8>, Error> {
        let subscriber = self.peers.get(&peer).ok_or(Error::UnknownSubscriber)?;
        self.publisher.open_direct(subscriber, message)
    }

    pub fn tick(
        &mut self,
        now: u64,
        rng: &mut (impl TryCryptoRng + ?Sized),
    ) -> Result<Delivery, Error> {
        let mut delivery = Delivery::default();
        if let Some(next) = &mut self.next {
            let elapsed = now.abs_diff(next.sent);
            if elapsed >= RESEND_AFTER_MS {
                delivery.messages.append(&mut next.messages);
            }
            if elapsed >= SWITCH_AFTER_MS {
                delivery.key = self.switch();
            }
        }
        if !self.due && now.abs_diff(self.last_rekey) < REKEY_INTERVAL_MS {
            return Ok(delivery);
        }
        let rekey = match self.publisher.rekey(now, rng) {
            Err(Error::TooSoon) => return Ok(delivery),
            rekey => rekey?,
        };
        if let Some(next) = &mut self.next {
            delivery.messages.append(&mut next.messages);
        }
        delivery.key = self.switch().or(delivery.key);
        self.due = false;
        self.last_rekey = now;
        let messages: Vec<_> = rekey
            .messages
            .into_iter()
            .filter_map(|(subscriber, data)| {
                let peer = self
                    .peers
                    .iter()
                    .find(|(_, known)| **known == subscriber)?
                    .0;
                Some((*peer, data))
            })
            .collect();
        let empty = messages.is_empty();
        delivery.messages.extend(messages.iter().cloned());
        self.next = Some(Next {
            key: rekey.key,
            sent: now,
            messages,
        });
        if empty {
            delivery.key = self.switch();
        }
        Ok(delivery)
    }

    fn switch(&mut self) -> Option<EpochKey> {
        self.next.take().map(|next| next.key)
    }
}
