// SPDX-License-Identifier: EUPL-1.2
//! Calls on stock and compatible servers (`nuschl_core::call`): the cards
//! others in a channel announced, and our own call, whose card goes to
//! everyone in its channel, again to whoever enters later.
//!
//! A received card belongs to its sender's presence and to the channel the
//! sender was in when it arrived; it is shown while the sender stays there.
//! Our own call ends when we leave its channel or the connection, on
//! [`Client::end_call`] and when the caller's lease on the call host ends.
use super::Client;
use crate::server::{ServerState, Status};
use nuschl_core::call::{CARD_DATA_ID, Card};
use nuschl_protocol::plugin::Payload;
use std::collections::{BTreeMap, BTreeSet};

use super::plugin::{MAX_PLUGIN_RECEIVERS, ReceivedPluginData};

/// Received calls kept per server.
pub const MAX_CALLS: usize = 64;

/// A call someone in a channel announced.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Call {
    pub sender: u32,
    pub sender_instance: u64,
    pub channel: u32,
    /// The Join link, as the card carried it.
    pub link: String,
}

impl Call {
    fn live(&self, server: &ServerState) -> bool {
        server.users.get(&self.sender).is_some_and(|user| {
            user.instance == self.sender_instance && user.channel == self.channel
        })
    }
}

/// Our call and its Join link.
#[derive(Debug)]
pub struct OwnCall {
    pub server: String,
    pub generation: u64,
    pub channel: u32,
    pub link: String,
    /// Who was sent the card, and the sends awaiting admission.
    told: BTreeSet<(u32, u64)>,
    pending: BTreeMap<u64, Vec<(u32, u64)>>,
}

impl Client {
    /// The calls others announced in `channel` of server `index`.
    pub fn calls_in(&self, index: usize, channel: u32) -> Vec<&Call> {
        let server = &self.servers[index];
        server
            .calls
            .iter()
            .filter(|call| call.channel == channel && call.live(server))
            .collect()
    }

    pub fn own_call(&self) -> Option<&OwnCall> {
        self.own_call.as_ref()
    }

    /// Start our call in our channel on server `index`, with its Join link.
    /// It replaces a call we had.
    pub fn start_call(&mut self, index: usize, link: String) -> Result<(), String> {
        let server = &self.servers[index];
        if server.status != Status::Connected || server.mls || !server.has_mumble_voice() {
            return Err("Calls need a connected server without end-to-end encryption.".into());
        }
        let channel = server
            .own_session
            .and_then(|session| server.users.get(&session))
            .map(|own| own.channel)
            .ok_or("Not connected.")?;
        let (id, generation) = (server.id.clone(), server.generation);
        self.end_call();
        self.own_call = Some(OwnCall {
            server: id,
            generation,
            channel,
            link,
            told: BTreeSet::new(),
            pending: BTreeMap::new(),
        });
        self.sync_call();
        Ok(())
    }

    /// Give our call a new link; everyone in its channel gets the new card.
    pub fn rotate_call(&mut self, link: String) {
        if let Some(call) = &mut self.own_call {
            call.link = link;
            call.told.clear();
            call.pending.clear();
        }
        self.sync_call();
    }

    /// End our call: everyone who got its card is told.
    pub fn end_call(&mut self) {
        let Some(call) = self.own_call.take() else {
            return;
        };
        let told: Vec<_> = call.told.into_iter().collect();
        if let Some(index) = self.current(&call.server, call.generation) {
            for receivers in told.chunks(MAX_PLUGIN_RECEIVERS) {
                let _ = self.send_payload(index, receivers, payload(&Card::Ended));
            }
        }
    }

    /// Our call's card to whoever in its channel still needs it; the call
    /// ends when we left its channel or connection.
    pub(super) fn sync_call(&mut self) {
        let Some(call) = &self.own_call else {
            return;
        };
        let index = self.current(&call.server, call.generation);
        let server = index.map(|index| &self.servers[index]);
        let own = server.and_then(|server| {
            server
                .own_session
                .and_then(|session| server.users.get(&session))
        });
        let (Some(index), Some(server), Some(own)) = (index, server, own) else {
            self.own_call = None;
            return;
        };
        if server.status != Status::Connected || own.channel != call.channel {
            self.own_call = None;
            return;
        }
        let members: BTreeSet<_> = server
            .users
            .values()
            .filter(|user| {
                user.channel == call.channel && user.session != own.session && user.instance != 0
            })
            .map(|user| (user.session, user.instance))
            .collect();
        let card = payload(&Card::Active(call.link.clone()));
        let new: Vec<_> = members.difference(&call.told).copied().collect();
        let mut sent = Vec::new();
        for receivers in new.chunks(MAX_PLUGIN_RECEIVERS) {
            if let Ok(request) = self.send_payload(index, receivers, card.clone()) {
                sent.push((request, receivers.to_vec()));
            }
        }
        let call = self.own_call.as_mut().expect("checked above");
        call.told.retain(|member| members.contains(member));
        for (request, receivers) in sent {
            call.told.extend(&receivers);
            call.pending.insert(request, receivers);
        }
    }

    pub(super) fn call_request(&self, request: u64) -> bool {
        self.own_call
            .as_ref()
            .is_some_and(|call| call.pending.contains_key(&request))
    }

    /// A card send was refused: its receivers get it on the next sync.
    pub(super) fn call_refused(&mut self, request: u64) {
        if let Some(call) = &mut self.own_call
            && let Some(receivers) = call.pending.remove(&request)
        {
            for receiver in receivers {
                call.told.remove(&receiver);
            }
        }
    }

    /// A card from someone in a channel, on server `index`'s current connection.
    pub(super) fn call_card(&mut self, index: usize, data: &ReceivedPluginData) {
        let server = &mut self.servers[index];
        let Ok(card) = Card::decode(data.payload.data()) else {
            return;
        };
        let Some(sender) = server
            .users
            .get(&data.sender)
            .filter(|user| user.instance == data.sender_instance)
            .filter(|user| Some(user.session) != server.own_session)
        else {
            return;
        };
        let channel = sender.channel;
        let calls = std::mem::take(&mut server.calls);
        server.calls = calls
            .into_iter()
            .filter(|call| call.sender != data.sender && call.live(server))
            .collect();
        if let Card::Active(link) = card
            && server.calls.len() < MAX_CALLS
        {
            server.calls.push(Call {
                sender: data.sender,
                sender_instance: data.sender_instance,
                channel,
                link,
            });
        }
    }
}

fn payload(card: &Card) -> Payload {
    Payload::new(CARD_DATA_ID.into(), card.encode()).expect("cards fit plugin data")
}
