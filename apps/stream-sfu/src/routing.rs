// SPDX-License-Identifier: EUPL-1.2
//! Which WebRTC connection a UDP datagram belongs to, by two maps and no scan
//! over connections.
//!
//! - **ufrag → connection:** each connection's local ICE username fragment,
//!   added when the connection is created and removed with its peer. A STUN
//!   binding request names it first in its USERNAME (`ours:theirs`), so a check
//!   from any address reaches its connection.
//! - **address → connection:** an address the connection sent a datagram to:
//!   the response to a check, which str0m sends only when the check's message
//!   integrity holds, or media on the selected pair. Only a peer with the
//!   connection's ICE password adds an address. An entry ends
//!   - when its connection has sent nothing to it for [`CONSENT`] (RFC 7675),
//!     so after a NAT rebinding or a new selected pair the old address goes;
//!   - when another connection sends to the address, which then maps there;
//!   - when its connection holds more than [`ADDRESSES`] addresses, oldest first;
//!   - when its peer leaves.
//!
//! A datagram from an address in neither map, and a STUN request naming an
//! unknown ufrag, is dropped before any connection sees it.

use std::{
    collections::HashMap,
    hash::Hash,
    net::SocketAddr,
    time::{Duration, Instant},
};

pub const CONSENT: Duration = Duration::from_secs(30);
pub const ADDRESSES: usize = 8;

pub struct Routing<R> {
    ufrags: HashMap<String, R>,
    addresses: HashMap<SocketAddr, (R, Instant)>,
    /// Each connection's addresses, oldest first.
    by_route: HashMap<R, Vec<SocketAddr>>,
}

impl<R> Default for Routing<R> {
    fn default() -> Self {
        Self {
            ufrags: HashMap::new(),
            addresses: HashMap::new(),
            by_route: HashMap::new(),
        }
    }
}

impl<R: Copy + Eq + Hash> Routing<R> {
    pub fn add_ufrag(&mut self, ufrag: String, route: R) {
        self.ufrags.insert(ufrag, route);
    }

    pub fn route(&self, source: SocketAddr, datagram: &[u8]) -> Option<R> {
        match self.addresses.get(&source) {
            Some((route, _)) => Some(*route),
            None => self.ufrags.get(stun_ufrag(datagram)?).copied(),
        }
    }

    /// `route` sent a datagram to `address`.
    pub fn learn(&mut self, address: SocketAddr, route: R, now: Instant) {
        match self.addresses.insert(address, (route, now)) {
            Some((old, _)) if old == route => return,
            Some((old, _)) => self.forget(old, address),
            None => {}
        }
        let addresses = self.by_route.entry(route).or_default();
        addresses.push(address);
        if addresses.len() > ADDRESSES {
            let oldest = addresses.remove(0);
            self.addresses.remove(&oldest);
        }
    }

    fn forget(&mut self, route: R, address: SocketAddr) {
        if let Some(addresses) = self.by_route.get_mut(&route) {
            addresses.retain(|a| *a != address);
        }
    }

    /// Removes addresses without traffic for [`CONSENT`].
    pub fn expire(&mut self, now: Instant) {
        let by_route = &mut self.by_route;
        self.addresses.retain(|address, (route, sent)| {
            let keep = now.saturating_duration_since(*sent) < CONSENT;
            if !keep && let Some(addresses) = by_route.get_mut(route) {
                addresses.retain(|a| a != address);
            }
            keep
        });
        self.by_route.retain(|_, addresses| !addresses.is_empty());
    }

    /// Removes every entry of the routes `gone` selects.
    pub fn remove(&mut self, gone: impl Fn(R) -> bool) {
        self.ufrags.retain(|_, route| !gone(*route));
        self.addresses.retain(|_, (route, _)| !gone(*route));
        self.by_route.retain(|route, _| !gone(*route));
    }
}

/// The receiver's ufrag from a STUN binding request (RFC 8489 header and
/// USERNAME attribute), or `None` for anything else.
fn stun_ufrag(datagram: &[u8]) -> Option<&str> {
    const BINDING_REQUEST: [u8; 2] = [0x00, 0x01];
    const MAGIC: [u8; 4] = [0x21, 0x12, 0xa4, 0x42];
    const USERNAME: u16 = 0x0006;
    let (header, mut attributes) = datagram.split_at_checked(20)?;
    if header[..2] != BINDING_REQUEST
        || header[4..8] != MAGIC
        || usize::from(u16::from_be_bytes([header[2], header[3]])) != attributes.len()
    {
        return None;
    }
    while attributes.len() >= 4 {
        let kind = u16::from_be_bytes([attributes[0], attributes[1]]);
        let length = usize::from(u16::from_be_bytes([attributes[2], attributes[3]]));
        let value = attributes.get(4..4 + length)?;
        if kind == USERNAME {
            return std::str::from_utf8(value).ok()?.split(':').next();
        }
        attributes = attributes.get((4 + length).next_multiple_of(4)..)?;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stun(username: &str) -> Vec<u8> {
        let mut attribute = vec![0x00, 0x06];
        attribute.extend((username.len() as u16).to_be_bytes());
        attribute.extend(username.as_bytes());
        attribute.resize(attribute.len().next_multiple_of(4), 0);
        let mut datagram = vec![0x00, 0x01];
        datagram.extend((attribute.len() as u16).to_be_bytes());
        datagram.extend([0x21, 0x12, 0xa4, 0x42]);
        datagram.extend([7; 12]);
        datagram.extend(attribute);
        datagram
    }

    fn address(port: u16) -> SocketAddr {
        SocketAddr::from(([192, 0, 2, 1], port))
    }

    #[test]
    fn parses_the_receiver_ufrag() {
        assert_eq!(stun_ufrag(&stun("sfu1:peer")), Some("sfu1"));
        let mut response = stun("sfu1:peer");
        response[1] = 0x01;
        response[0] = 0x01;
        assert_eq!(stun_ufrag(&response), None, "a binding response");
        let mut short = stun("sfu1:peer");
        short.pop();
        assert_eq!(stun_ufrag(&short), None, "length mismatch");
        assert_eq!(stun_ufrag(&[0x80; 100]), None, "RTP");
        assert_eq!(stun_ufrag(&[]), None);
    }

    #[test]
    fn junk_from_unknown_addresses_reaches_no_connection() {
        let mut routing = Routing::default();
        for peer in 0..1000u64 {
            routing.add_ufrag(format!("ufrag{peer}"), peer);
            routing.learn(address(peer as u16), peer, Instant::now());
        }
        // Lookups are two hash probes; nothing iterates over connections.
        let stranger = SocketAddr::from(([198, 51, 100, 7], 4000));
        assert_eq!(routing.route(stranger, &[0x80; 1200]), None);
        assert_eq!(routing.route(stranger, &stun("nobody:x")), None);
        assert_eq!(routing.route(stranger, &stun("ufrag7:x")), Some(7));
        assert_eq!(routing.route(address(9), &[0x80; 1200]), Some(9));
    }

    #[test]
    fn a_nat_rebinding_moves_the_mapping() {
        let start = Instant::now();
        let mut routing = Routing::default();
        routing.add_ufrag("a".into(), 1u64);
        routing.learn(address(1), 1, start);
        // The peer's NAT rebinds: its next check arrives from a new address.
        assert_eq!(routing.route(address(2), &[0x80; 10]), None);
        assert_eq!(routing.route(address(2), &stun("a:x")), Some(1));
        routing.learn(address(2), 1, start + Duration::from_secs(1));
        routing.learn(address(2), 1, start + CONSENT);
        routing.expire(start + CONSENT);
        assert_eq!(routing.route(address(1), &[0x80; 10]), None);
        assert_eq!(routing.route(address(2), &[0x80; 10]), Some(1));

        // An address another connection sends to moves there.
        routing.learn(address(2), 2, start + CONSENT);
        assert_eq!(routing.route(address(2), &[0x80; 10]), Some(2));
        assert_eq!(routing.by_route.get(&1).map_or(0, Vec::len), 0);

        // Each connection keeps a bounded number of addresses.
        for port in 10..10 + ADDRESSES as u16 + 1 {
            routing.learn(address(port), 3, start);
        }
        assert_eq!(routing.route(address(10), &[0x80; 10]), None);
        assert_eq!(routing.route(address(11), &[0x80; 10]), Some(3));

        routing.remove(|route| route == 3 || route == 2);
        assert_eq!(routing.route(address(11), &[0x80; 10]), None);
        assert_eq!(routing.route(address(2), &[0x80; 10]), None);
        assert_eq!(routing.route(address(5), &stun("a:x")), Some(1));
    }
}
