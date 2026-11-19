// SPDX-License-Identifier: EUPL-1.2
//! The crypto worker (docs/e2ee.md §8.1): a sandboxed process without network that
//! owns every identity secret and every key-transparency decision.
//!
//! - **Identity.** Created on first need: the identity root, the device-signing key of
//!   generation 1 and this device's keys (`nuschl-identity`). The root is sealed
//!   under a fresh recovery key, which the worker keeps (sealed under the store key)
//!   so the user can see it whenever they ask.
//! - **Keystore.** The encrypted device keystore and the worker's own state in its
//!   directory ([`store`]); the store key in a 0600 file there for now, with the OS
//!   credential store as the follow-up.
//! - **Signing.** Account statements for the servers the user is registered on, and
//!   this device's certificate (renewed 30 days before it expires).
//! - **Key transparency.** Pinned log descriptors, persisted `KnownHeads`, audits of
//!   every newer head, self-monitoring of our own bindings, contact lookups and
//!   histories, and the §5.4 schedule ([`CryptoRequest::Due`]).
//! - **Trust on first use** per contact (`ContactTrust`), verification per identity
//!   (valid on every server whose log binds the same identity), safety numbers.
//!
//! The session moves the bytes; see `nuschl_ipc::crypto` for the protocol.
//!
//! [`CryptoRequest::Due`]: nuschl_ipc::crypto::CryptoRequest::Due
#![forbid(unsafe_code)]

mod call;
pub mod link;
pub mod mls;
pub mod state;
pub mod store;
pub mod witness;
pub mod worker;

pub use store::{KeySource, Store};
pub use worker::Worker;
