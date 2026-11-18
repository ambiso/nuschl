// SPDX-License-Identifier: EUPL-1.2
//! Call links (docs/video-plan.md §3), and the keys derived from the link secret S and
//! the creator's secret P:
//!
//! ```text
//! https://<host>/call#v=1&k=<S>&h=<V>          posted to the channel
//! https://<host>/call#v=1&k=<S>&h=<V>&p=<P>    opened by the call's creator only
//!
//! host_key = HKDF-Expand(HKDF-Extract("nuschl/stream/v2", P), "nuschl/stream/v2 host", 32)
//! V        = SHA-256(host_key)                 the room check
//! ```
//!
//! S, V and P are 32 bytes in unpadded base64url. The host is the configured stream
//! host, in lowercase, with an optional port. Parsing accepts one canonical form of at
//! most [`Link::MAX_LEN`] bytes, on that host only: page code served from the link's
//! origin sees S. P stays on the creator's device; the SFU receives `host_key`, and the
//! link's V lets every holder check that a room's `host_key` is the creator's.

use std::fmt;

use base64ct::{Base64UrlUnpadded, Encoding as _};
use rand_core::TryCryptoRng;

use crate::{Error, hkdf_expand, hkdf_extract, random, sha256};

pub(crate) const LABEL: &str = "nuschl/stream/v2";
const ENCODED_SECRET_LEN: usize = 43;

/// The SFU room of the link secret S.
pub fn stream_id(secret: &[u8; 32]) -> [u8; 16] {
    LinkKeys::derive(secret).stream_id
}

/// The key every holder of S proves to the SFU to join the room.
pub fn admit_key(secret: &[u8; 32]) -> [u8; 32] {
    LinkKeys::derive(secret).admit_key
}

/// The key the creator proves to the SFU to create, moderate and end the room. Only
/// `host_key` goes to the SFU; P stays on the creator's device.
pub fn host_key(publisher: &[u8; 32]) -> [u8; 32] {
    hkdf_expand(
        &hkdf_extract(LABEL.as_bytes(), publisher),
        label("host").as_bytes(),
    )
}

/// V, the link's check value for a room's `host_key`.
pub fn room_check(host_key: &[u8; 32]) -> [u8; 32] {
    sha256(&[host_key])
}

/// A parsed call link. `Debug` leaves out the secrets.
pub struct Link {
    host: String,
    secret: [u8; 32],
    room_check: [u8; 32],
    publisher: Option<[u8; 32]>,
}

impl Link {
    pub const MAX_LEN: usize = 512;

    /// A new creator's link on the stream host (`host[:port]`) with fresh S and P.
    pub fn generate<R: TryCryptoRng + ?Sized>(host: &str, rng: &mut R) -> Result<Self, Error> {
        check_host(host)?;
        let secret = random(rng)?;
        let publisher = random(rng)?;
        Ok(Self {
            host: host.to_owned(),
            secret,
            room_check: room_check(&host_key(&publisher)),
            publisher: Some(publisher),
        })
    }

    /// Parses a link on the stream host `host`. A link with P must carry P's V.
    pub fn parse(link: &str, host: &str) -> Result<Self, Error> {
        if link.len() > Self::MAX_LEN {
            return Err(Error::TooLong);
        }
        check_host(host)?;
        let fragment = link
            .strip_prefix("https://")
            .and_then(|rest| rest.strip_prefix(host))
            .and_then(|rest| rest.strip_prefix('/'))
            .ok_or(Error::Malformed("link origin"))?
            .strip_prefix("call#")
            .ok_or(Error::Malformed("link page"))?;
        let mut params = fragment.split('&');
        match params.next() {
            Some("v=1") => {}
            Some(version) if version.starts_with("v=") => return Err(Error::UnsupportedVersion),
            _ => return Err(Error::Malformed("link version")),
        }
        let secret = decode_param(params.next(), "k=")?;
        let check = decode_param(params.next(), "h=")?;
        let publisher = params
            .next()
            .map(|param| decode_param(Some(param), "p="))
            .transpose()?;
        if params.next().is_some() {
            return Err(Error::Malformed("link parameters"));
        }
        if publisher.is_some_and(|publisher| room_check(&host_key(&publisher)) != check) {
            return Err(Error::Malformed("link room check"));
        }
        Ok(Self {
            host: host.to_owned(),
            secret,
            room_check: check,
            publisher,
        })
    }

    pub fn host(&self) -> &str {
        &self.host
    }

    pub fn secret(&self) -> &[u8; 32] {
        &self.secret
    }

    /// V, which a room's `host_key` must hash to.
    pub fn room_check(&self) -> &[u8; 32] {
        &self.room_check
    }

    /// P, for the creator's link only.
    pub fn publisher(&self) -> Option<&[u8; 32]> {
        self.publisher.as_ref()
    }

    /// The link to post: S and V.
    pub fn url(&self) -> String {
        format!(
            "https://{}/call#v=1&k={}&h={}",
            self.host,
            encode(&self.secret),
            encode(&self.room_check)
        )
    }

    /// The creator's link, with P.
    pub fn creator_url(&self) -> Option<String> {
        let publisher = self.publisher.as_ref()?;
        Some(format!("{}&p={}", self.url(), encode(publisher)))
    }

    pub fn keys(&self) -> LinkKeys {
        LinkKeys::derive(&self.secret)
    }
}

impl fmt::Debug for Link {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Link")
            .field("host", &self.host)
            .field("creator", &self.publisher.is_some())
            .finish_non_exhaustive()
    }
}

/// Lowercase DNS labels, then an optional `:port` of 1–5 digits.
fn check_host(host: &str) -> Result<(), Error> {
    let (name, port) = host.split_once(':').unwrap_or((host, "1"));
    let label = |label: &str| {
        !label.is_empty()
            && label
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
    };
    let port_ok = (1..=5).contains(&port.len()) && port.bytes().all(|b| b.is_ascii_digit());
    if name.split('.').all(label) && port_ok {
        Ok(())
    } else {
        Err(Error::Malformed("link host"))
    }
}

fn decode_param(param: Option<&str>, name: &str) -> Result<[u8; 32], Error> {
    let value = param
        .and_then(|param| param.strip_prefix(name))
        .ok_or(Error::Malformed("link parameters"))?;
    let mut bytes = [0; 32];
    if value.len() != ENCODED_SECRET_LEN
        || Base64UrlUnpadded::decode(value, &mut bytes).map(<[u8]>::len) != Ok(32)
    {
        return Err(Error::Malformed("link secret"));
    }
    Ok(bytes)
}

fn encode(secret: &[u8; 32]) -> String {
    let mut out = [0; ENCODED_SECRET_LEN];
    Base64UrlUnpadded::encode(secret, &mut out)
        .expect("43 characters encode 32 bytes")
        .to_owned()
}

/// The keys every link holder derives from S. The SFU gets `stream_id` and `admit_key`
/// only.
pub struct LinkKeys {
    stream_id: [u8; 16],
    admit_key: [u8; 32],
    pub(crate) psk: [u8; 32],
    pub(crate) signal_key: [u8; 32],
}

impl LinkKeys {
    pub fn derive(secret: &[u8; 32]) -> Self {
        let prk = hkdf_extract(LABEL.as_bytes(), secret);
        let expand = |name: &str| hkdf_expand(&prk, label(name).as_bytes());
        Self {
            stream_id: hkdf_expand(&prk, label("stream id").as_bytes()),
            admit_key: expand("admit"),
            psk: expand("psk"),
            signal_key: expand("signal"),
        }
    }

    /// The SFU room.
    pub fn stream_id(&self) -> &[u8; 16] {
        &self.stream_id
    }

    /// What the SFU checks subscriber admission with.
    pub fn admit_key(&self) -> &[u8; 32] {
        &self.admit_key
    }

    pub(crate) fn psk_id(&self) -> Vec<u8> {
        [LABEL.as_bytes(), &self.stream_id].concat()
    }
}

impl fmt::Debug for LinkKeys {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("LinkKeys")
            .field("stream_id", &self.stream_id)
            .finish_non_exhaustive()
    }
}

pub(crate) fn label(name: &str) -> String {
    format!("{LABEL} {name}")
}
