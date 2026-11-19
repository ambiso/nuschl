// SPDX-License-Identifier: EUPL-1.2
//! Call cards: the typed plugin data (#93) a call's creator sends to the people
//! in its channel, or to the other person of a direct conversation
//! ([desktop](../../../docs/desktop.md#calls)). The call link
//! itself, its secrets and their keys are `nuschl-stream`'s (#267):
//!
//! ```text
//! https://<host>/call#v=1&k=<S>&h=<V>          the Join link, posted as text
//! https://<host>/call#v=1&k=<S>&h=<V>&p=<P>    the creator's page
//! ```
//!
//! The card has the data ID [`CARD_DATA_ID`] and at most [`MAX_CARD`] bytes:
//!
//! ```text
//! uint8 version = 1; uint8 kind;
//! kind 1, a channel call:  the Join link, UTF-8
//! kind 2, ended:           nothing
//! kind 3, a direct call:   the Join link, UTF-8
//! ```
//!
//! Decoding checks the card's shape and the link's origin; `nuschl-stream`'s
//! parser checks the link itself before anyone opens it. A card belongs to its
//! sender's presence: a channel call to the channel the sender is in when it
//! arrives, which receivers drop when the sender leaves that channel or the
//! server; a direct call to the direct conversation with the sender, dropped
//! when the sender leaves the server. Any
//! client on the server can send one, so its link is the sender's claim and the
//! Mumble server sees S.

use std::borrow::Cow;
use url::Url;

/// The project's call host.
pub const DEFAULT_HOST: &str = "stream.nuschl.eu";
/// The plugin data ID of call cards.
pub const CARD_DATA_ID: &str = "nuschl/call";
/// The longest link, as `nuschl-stream` parses it.
pub const MAX_LINK: usize = 512;
/// The longest card.
pub const MAX_CARD: usize = 2 + MAX_LINK;
const VERSION: u8 = 1;
const ACTIVE: u8 = 1;
const ENDED: u8 = 2;
const DIRECT: u8 = 3;

/// What a call card says.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Card {
    /// A call in the sender's channel and its Join link.
    Active(String),
    /// A call with the receiver alone and its Join link.
    Direct(String),
    Ended,
}

impl Card {
    pub fn encode(&self) -> Vec<u8> {
        match self {
            Self::Active(link) => [&[VERSION, ACTIVE], link.as_bytes()].concat(),
            Self::Direct(link) => [&[VERSION, DIRECT], link.as_bytes()].concat(),
            Self::Ended => vec![VERSION, ENDED],
        }
    }

    pub fn decode(data: &[u8]) -> Result<Self, &'static str> {
        if data.len() > MAX_CARD {
            return Err("call card too long");
        }
        match data {
            [VERSION, kind @ (ACTIVE | DIRECT), link @ ..] => {
                let link = std::str::from_utf8(link)
                    .ok()
                    .filter(|link| link_host(link).is_some())
                    .ok_or("call card link")?
                    .to_owned();
                Ok(if *kind == ACTIVE {
                    Self::Active(link)
                } else {
                    Self::Direct(link)
                })
            }
            [VERSION, ENDED] => Ok(Self::Ended),
            [VERSION, ..] => Err("call card kind"),
            _ => Err("unsupported call card"),
        }
    }
}

/// The host of a call link `https://<host>/call#…`.
pub fn link_host(link: &str) -> Option<&str> {
    let (host, page) = link.strip_prefix("https://")?.split_once('/')?;
    (!host.is_empty() && page.starts_with("call#")).then_some(host)
}

/// The browser URL for a call, including the local relay-only choice.
pub fn page_url(link: &str, relay_only: bool) -> Cow<'_, str> {
    if relay_only
        && let Ok(mut url) = Url::parse(link)
        && url.scheme() == "https"
        && url.path() == "/call"
        && url
            .fragment()
            .is_some_and(|hash| hash.starts_with("v=1&k="))
        && !url.query_pairs().any(|(name, _)| name == "relay")
    {
        url.query_pairs_mut().append_key_only("relay");
        return Cow::Owned(url.into());
    }
    Cow::Borrowed(link)
}

#[cfg(test)]
mod tests {
    use super::*;

    const LINK: &str = "https://stream.nuschl.eu/call#v=1&k=BwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwc&h=CQkJCQkJCQkJCQkJCQkJCQkJCQkJCQkJCQkJCQkJCQk";

    #[test]
    fn cards_are_versioned_and_bounded() {
        let card = Card::Active(LINK.into());
        let encoded = card.encode();
        assert_eq!(encoded[..2], [1, 1]);
        assert_eq!(Card::decode(&encoded), Ok(card));
        let direct = Card::Direct(LINK.into());
        assert_eq!(direct.encode()[..2], [1, 3]);
        assert_eq!(Card::decode(&direct.encode()), Ok(direct));
        assert_eq!(Card::decode(&Card::Ended.encode()), Ok(Card::Ended));
        let link = |link: &str| [&[1, 1], link.as_bytes()].concat();
        for data in [
            &[][..],
            &[1],
            &[2, 1],
            &[1, 3],
            &[1, 2, 0],
            &[1, 4],
            &[1, 1, 0xff],
            &link("http://stream.nuschl.eu/call#v=1"),
            &link("https:///call#v=1"),
            &link("https://stream.nuschl.eu/watch#v=1"),
            &link(&format!("https://{}/call#", "a".repeat(MAX_LINK))),
        ] {
            assert!(Card::decode(data).is_err(), "{data:?}");
        }
        assert!(encoded.len() <= MAX_CARD && MAX_CARD < 1000);
    }

    #[test]
    fn link_hosts() {
        assert_eq!(link_host(LINK), Some(DEFAULT_HOST));
        assert_eq!(
            link_host("https://localhost:8443/call#v=1"),
            Some("localhost:8443")
        );
        assert_eq!(link_host("https://stream.nuschl.eu/call"), None);
    }

    #[test]
    fn relay_only_survives_browser_url_normalization_and_preserves_the_call() {
        assert_eq!(page_url(LINK, false), LINK);
        let relay = LINK.replace("/call#", "/call?relay#");
        assert_eq!(page_url(LINK, true), relay);
        assert_eq!(page_url(&relay, true), relay);

        let pasted = LINK.replace(
            "https://stream.nuschl.eu/call#",
            "HTTPS://STREAM.NUSCHL.EU/call?view=grid#",
        );
        assert_eq!(
            page_url(&pasted, true),
            LINK.replace("/call#", "/call?view=grid&relay#")
        );
        let unrelated = "https://stream.nuschl.eu/call#about";
        assert_eq!(page_url(unrelated, true), unrelated);
    }
}
