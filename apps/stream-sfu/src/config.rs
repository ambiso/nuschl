// SPDX-License-Identifier: EUPL-1.2
//! The SFU's settings (`nuschl-stream-sfu.toml`). Every limit applies to input
//! from the network and fails only the peer or room that reaches it.

use serde::Deserialize;
use std::{
    net::{IpAddr, SocketAddr},
    path::PathBuf,
    str::FromStr,
};

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Listen {
    Tcp(SocketAddr),
    /// A Unix socket for the web server in front (`unix:/run/…/sfu.sock`). The
    /// client address then comes from the proxy's `X-Real-IP` header.
    Unix(PathBuf),
}

impl FromStr for Listen {
    type Err = String;
    fn from_str(text: &str) -> Result<Self, String> {
        match text.strip_prefix("unix:") {
            Some(path) if path.starts_with('/') => Ok(Self::Unix(path.into())),
            Some(_) => Err("a unix: listen path must be absolute".into()),
            None => text
                .parse()
                .map(Self::Tcp)
                .map_err(|_| format!("invalid listen address {text:?}")),
        }
    }
}

impl<'de> Deserialize<'de> for Listen {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        String::deserialize(deserializer)?
            .parse()
            .map_err(serde::de::Error::custom)
    }
}

/// TURN REST credentials (coturn's `use-auth-secret`) for admitted peers.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Turn {
    /// The URLs handed to peers, such as `turn:turn.example:3478?transport=udp`.
    pub urls: Vec<String>,
    /// The shared REST secret, one line.
    pub secret_file: PathBuf,
    #[serde(default = "default_turn_lifetime")]
    pub lifetime_secs: u64,
}

fn default_turn_lifetime() -> u64 {
    600
}

#[derive(Clone, Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    /// WebSocket signalling: `127.0.0.1:8091` or `unix:/run/nuschl-stream-sfu/sfu.sock`.
    pub listen: Listen,
    /// The one UDP socket all media uses.
    pub media_listen: SocketAddr,
    /// Addresses at which peers reach `media_listen`'s port; each one becomes a
    /// host candidate.
    pub media_addresses: Vec<IpAddr>,
    pub turn: Option<Turn>,
    pub max_rooms: usize,
    pub max_peers_per_room: usize,
    /// Tracks one peer may publish (a camera, a screen and its audio).
    pub max_sources_per_peer: usize,
    /// A room and its keys expire this long after it was created.
    pub room_lifetime_secs: u64,
    /// The largest WebSocket message; SDP and key messages must fit.
    pub max_message_bytes: usize,
    /// Signalling messages per second and peer; twice this is the burst.
    pub messages_per_second: u32,
    /// Room creations per client address (IPv6 per /64) and hour.
    pub creations_per_hour: u32,
    /// Forwarded media per receiver, in bits per second.
    pub receiver_bitrate: u64,
    /// Forwarded media per room, in bits per second.
    pub room_bitrate: u64,
    /// WebSocket connections at once, joined or not.
    pub max_connections: usize,
    /// WebSocket connections at once per client address (IPv6 per /64).
    pub connections_per_address: usize,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            listen: Listen::Tcp(SocketAddr::from(([127, 0, 0, 1], 8091))),
            media_listen: SocketAddr::from(([0, 0, 0, 0], 3479)),
            media_addresses: Vec::new(),
            turn: None,
            max_rooms: 1024,
            max_peers_per_room: 16,
            max_sources_per_peer: 3,
            room_lifetime_secs: 4 * 3600,
            max_message_bytes: 256 * 1024,
            messages_per_second: 32,
            creations_per_hour: 20,
            receiver_bitrate: 25_000_000,
            room_bitrate: 150_000_000,
            max_connections: 4096,
            connections_per_address: 32,
        }
    }
}

impl Config {
    pub fn load(path: &std::path::Path) -> Result<Self, String> {
        let text = std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
        toml::from_str(&text).map_err(|e| format!("{}: {e}", path.display()))
    }

    pub fn check(&self) -> Result<(), String> {
        if self.media_addresses.is_empty() {
            return Err("media_addresses must name at least one address".into());
        }
        if let Some(ip) = self
            .media_addresses
            .iter()
            .find(|ip| ip.is_unspecified() || ip.is_multicast())
        {
            return Err(format!("{ip} cannot be a media address"));
        }
        if self.max_rooms == 0
            || self.max_peers_per_room < 2
            || self.max_sources_per_peer == 0
            || self.room_lifetime_secs == 0
            || self.max_message_bytes < 16 * 1024
            || self.messages_per_second == 0
            || self.receiver_bitrate == 0
            || self.room_bitrate == 0
            || self.max_connections == 0
            || self.connections_per_address == 0
        {
            return Err("a limit is zero or too small".into());
        }
        if self
            .turn
            .as_ref()
            .is_some_and(|turn| turn.urls.is_empty() || turn.lifetime_secs == 0)
        {
            return Err("turn needs urls and a lifetime".into());
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_and_checks() {
        let config: Config = toml::from_str(
            r#"
            listen = "unix:/run/sfu/sfu.sock"
            media_listen = "[::]:3479"
            media_addresses = ["203.0.113.10", "2001:db8::10"]
            max_peers_per_room = 8
            [turn]
            urls = ["turn:turn.example:3478?transport=udp"]
            secret_file = "/run/credentials/sfu/turn-secret"
            "#,
        )
        .expect("config");
        assert_eq!(config.listen, Listen::Unix("/run/sfu/sfu.sock".into()));
        assert_eq!(config.max_peers_per_room, 8);
        assert_eq!(config.turn.as_ref().map(|t| t.lifetime_secs), Some(600));
        config.check().expect("valid");

        assert!(Config::default().check().is_err(), "no media address");
        assert!(toml::from_str::<Config>("listen = \"unix:relative\"").is_err());
        assert!(toml::from_str::<Config>("unknown = 1").is_err());
        let zero = Config {
            media_addresses: vec!["192.0.2.1".parse().expect("ip")],
            max_rooms: 0,
            ..Config::default()
        };
        assert!(zero.check().is_err());
    }
}
