// SPDX-License-Identifier: EUPL-1.2
//! Call signaling through the crypto worker. Only room metadata, proofs,
//! opaque envelopes and opened signaling cross IPC.

use super::base64_bytes;
use serde::{Deserialize, Serialize};
use std::fmt;

const MAX_MESSAGE: usize = 16 * 1024;
pub const MAX_REPLY_FRAME_BYTES: usize = MAX_MESSAGE.div_ceil(3) * 4 + 1024;

/// A call generation increases for the life of one crypto handle.
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Request {
    pub generation: u64,
    pub action: Action,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum Action {
    Open {
        host: String,
        link: String,
        relay_only: bool,
    },
    Admit {
        nonce: [u8; 32],
    },
    Pair {
        peer: u64,
        /// This client's side of the exchange.
        publisher: bool,
    },
    Key {
        peer: u64,
        #[serde(with = "base64_bytes")]
        data: Vec<u8>,
    },
    Seal {
        peer: u64,
        #[serde(with = "base64_bytes")]
        data: Vec<u8>,
    },
    Close,
}

impl Request {
    pub(super) fn check(&self) -> Result<(), &'static str> {
        if self.generation == 0 {
            return Err("call generation");
        }
        match &self.action {
            Action::Open { host, link, .. }
                if host.len() > nuschl_core::call::MAX_LINK
                    || link.len() > nuschl_core::call::MAX_LINK =>
            {
                Err("call link")
            }
            Action::Key { data, .. } | Action::Seal { data, .. }
                if data.is_empty() || data.len() > MAX_MESSAGE =>
            {
                Err("call signal")
            }
            Action::Pair { peer, .. } | Action::Key { peer, .. } | Action::Seal { peer, .. }
                if *peer == 0 =>
            {
                Err("call peer")
            }
            _ => Ok(()),
        }
    }
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Reply {
    pub generation: u64,
    pub outcome: Outcome,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum Outcome {
    Opened {
        room: [u8; 16],
        verifier: [u8; 32],
    },
    Admitted {
        proof: [u8; 32],
        host_proof: Option<[u8; 32]>,
        create: Option<Create>,
    },
    Send {
        peer: u64,
        #[serde(with = "base64_bytes")]
        data: Vec<u8>,
        /// The subscriber has the signaling key. Send this ack before signaling.
        ready: bool,
    },
    Ready {
        peer: u64,
    },
    Signal {
        peer: u64,
        #[serde(with = "base64_bytes")]
        data: Vec<u8>,
    },
    Done,
}

/// The credentials the creator sends to the configured SFU to create the room.
/// Keep them out of debug output and frontend events.
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Create {
    pub admit_key: [u8; 32],
    pub host_key: [u8; 32],
}

impl Reply {
    /// Check the worker's variable-size fields and request binding.
    pub fn matches(&self, request: &Request) -> bool {
        self.generation == request.generation
            && match (&request.action, &self.outcome) {
                (Action::Open { .. }, Outcome::Opened { .. })
                | (
                    Action::Pair {
                        publisher: true, ..
                    },
                    Outcome::Done,
                )
                | (Action::Key { .. }, Outcome::Done)
                | (Action::Close, Outcome::Done) => true,
                (
                    Action::Pair {
                        peer,
                        publisher: false,
                    }
                    | Action::Key { peer, .. }
                    | Action::Seal { peer, .. },
                    Outcome::Send { peer: to, data, .. },
                )
                | (Action::Key { peer, .. }, Outcome::Signal { peer: to, data }) => {
                    peer == to && !data.is_empty() && data.len() <= MAX_MESSAGE
                }
                (Action::Key { peer, .. }, Outcome::Ready { peer: from }) => peer == from,
                (
                    Action::Admit { .. },
                    Outcome::Admitted {
                        host_proof, create, ..
                    },
                ) => host_proof.is_some() == create.is_some(),
                _ => false,
            }
    }
}

impl fmt::Debug for Request {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CallRequest")
            .field("generation", &self.generation)
            .finish_non_exhaustive()
    }
}

impl fmt::Debug for Reply {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CallReply")
            .field("generation", &self.generation)
            .finish_non_exhaustive()
    }
}
