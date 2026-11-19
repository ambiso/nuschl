// SPDX-License-Identifier: EUPL-1.2
//! Direct-path signaling over the SFU's opaque key-message transport.

use crate::{Error, MAX_KEY_MESSAGE, exchange::VERSION};

const TO_PUBLISHER: u8 = 5;
const TO_SUBSCRIBER: u8 = 6;
const HEADER: usize = 2 + 8;
const TAG: usize = 16;

pub const MAX_DIRECT_SIGNAL: usize = MAX_KEY_MESSAGE - HEADER - TAG;

/// The receiving role in the pairwise exchange, authenticated by `open`.
pub fn direct_for_publisher(message: &[u8]) -> Option<bool> {
    match *message.get(1)? {
        TO_PUBLISHER => Some(true),
        TO_SUBSCRIBER => Some(false),
        _ => None,
    }
}

/// A fresh HPKE export for this join. The direction and counter in the header
/// separate nonces. Rekeying media leaves the signaling channel intact.
pub(crate) struct Channel {
    key: [u8; 32],
    send_to: u8,
    receive_on: u8,
    sent: u64,
    received: u64,
}

impl Channel {
    pub(crate) fn new(key: [u8; 32], publisher: bool) -> Self {
        let (send_to, receive_on) = if publisher {
            (TO_SUBSCRIBER, TO_PUBLISHER)
        } else {
            (TO_PUBLISHER, TO_SUBSCRIBER)
        };
        Self {
            key,
            send_to,
            receive_on,
            sent: 0,
            received: 0,
        }
    }

    pub(crate) fn seal(&mut self, body: &[u8]) -> Result<Vec<u8>, Error> {
        if body.is_empty() {
            return Err(Error::Malformed("direct signal"));
        }
        if body.len() > MAX_DIRECT_SIGNAL {
            return Err(Error::TooLong);
        }
        let counter = self.sent.checked_add(1).ok_or(Error::Exhausted)?;
        let mut message = vec![0; HEADER + body.len() + TAG];
        let (header, ciphertext) = message.split_at_mut(HEADER);
        header[..2].copy_from_slice(&[VERSION, self.send_to]);
        header[2..].copy_from_slice(&counter.to_be_bytes());
        libcrux_chacha20poly1305::encrypt(&self.key, body, ciphertext, header, &nonce(header))
            .map_err(|_| Error::Authentication)?;
        self.sent = counter;
        Ok(message)
    }

    pub(crate) fn open(&mut self, message: &[u8]) -> Result<Vec<u8>, Error> {
        if message.len() > MAX_KEY_MESSAGE {
            return Err(Error::TooLong);
        }
        let (header, ciphertext) = message
            .split_at_checked(HEADER)
            .filter(|(_, ciphertext)| ciphertext.len() > TAG)
            .ok_or(Error::Malformed("direct signal"))?;
        if header[0] != VERSION {
            return Err(Error::UnsupportedVersion);
        }
        if header[1] != self.receive_on {
            return Err(Error::Unexpected);
        }
        let counter = u64::from_be_bytes(
            header[2..]
                .try_into()
                .map_err(|_| Error::Malformed("direct signal"))?,
        );
        if counter <= self.received {
            return Err(Error::Replay);
        }
        let mut body = vec![0; ciphertext.len() - TAG];
        libcrux_chacha20poly1305::decrypt(&self.key, &mut body, ciphertext, header, &nonce(header))
            .map_err(|_| Error::Authentication)?;
        self.received = counter;
        Ok(body)
    }
}

fn nonce(header: &[u8]) -> [u8; 12] {
    let mut nonce = [0; 12];
    nonce[..HEADER].copy_from_slice(header);
    nonce
}
