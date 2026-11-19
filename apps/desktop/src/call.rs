// SPDX-License-Identifier: EUPL-1.2
//! Calls (docs/desktop.md, "Calls"). The creator posts a link and renews the
//! room's lease. The creator and invitees join through the desktop or browser.
//!
//! The lease keeps the room the creator's page made: every [`RENEW`] the desktop
//! sends `POST <origin>/sfu/lease` with `{"stream_id", "host_key"}` in lowercase
//! hex (`nuschl-stream` derives them from S and P; P stays on this device). 204
//! means the lease is renewed, 404 that the call host has no such room (E1,
//! `apps/stream-sfu/src/protocol.rs`). Ending the call sends `DELETE` with the
//! same body. The call ends when the room is gone after it existed, when the
//! room fails to appear within [`GRACE`] (the page creates it), when renewals
//! fail for [`TIMEOUT`], and after [`MAX`]. Hosts `localhost` and `127.0.0.1` take
//! the lease over `http://`, for development and tests. With relay-only on, the page
//! opens as `/call?relay#…` and connects through the call host's TURN server only
//! (docs/web.md, "The call page").
use crate::{services::ServiceEvent, state::Desktop};
use gpui_kit::Context;
use nuschl_core::{
    call::{DEFAULT_HOST, page_url},
    model::TextTarget,
};
use nuschl_stream::Link;
use std::time::Duration;
use tokio::sync::{mpsc, oneshot};

pub const RENEW: Duration = Duration::from_secs(10);
pub const TIMEOUT: Duration = Duration::from_secs(30);
pub const GRACE: Duration = Duration::from_secs(120);
pub const MAX: Duration = Duration::from_secs(4 * 60 * 60);

/// Our call's creator link and its running lease; dropping it ends the lease.
pub struct Creator {
    pub link: Link,
    lease: u64,
    _stop: oneshot::Sender<()>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Reply {
    Renewed,
    NoRoom,
    Failed,
}

/// When the lease ends, given the call host's replies.
#[derive(Debug, Default)]
pub struct Clock {
    renewed: Option<Duration>,
}

impl Clock {
    /// Whether the lease is over after `reply`, `elapsed` since it started.
    pub fn over(&mut self, elapsed: Duration, reply: Reply) -> bool {
        if reply == Reply::Renewed {
            self.renewed = Some(elapsed);
        }
        elapsed >= MAX
            || match (reply, self.renewed) {
                (Reply::Renewed, _) => false,
                (Reply::NoRoom, Some(_)) => true,
                (Reply::Failed, Some(renewed)) => elapsed - renewed >= TIMEOUT,
                (_, None) => elapsed >= GRACE,
            }
    }
}

/// The lease address and body of the creator's `link`.
pub fn lease_request(link: &Link) -> (String, String) {
    let hex = |bytes: &[u8]| bytes.iter().map(|b| format!("{b:02x}")).collect::<String>();
    let host = link.host();
    let scheme = match host.split_once(':').map_or(host, |(name, _)| name) {
        "localhost" | "127.0.0.1" => "http",
        _ => "https",
    };
    let host_key = nuschl_stream::host_key(link.publisher().expect("the creator's link"));
    let body = serde_json::json!({
        "stream_id": hex(&nuschl_stream::stream_id(link.secret())),
        "host_key": hex(&host_key),
    });
    (format!("{scheme}://{host}/sfu/lease"), body.to_string())
}

/// Keep the lease at `url` until it is over or `stop` fires or is dropped,
/// then report `CallLeaseEnded` (over) or release it (stopped).
pub async fn run(
    id: u64,
    url: String,
    body: String,
    mut stop: oneshot::Receiver<()>,
    events: mpsc::Sender<ServiceEvent>,
) {
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_global(Some(Duration::from_secs(5)))
        .max_redirects(0)
        .http_status_as_error(false)
        .user_agent("Nuschl")
        .build()
        .into();
    let request = move |method: &'static str| {
        let (agent, url, body) = (agent.clone(), url.clone(), body.clone());
        tokio::task::spawn_blocking(move || {
            let reply = match method {
                "POST" => agent.post(&url),
                _ => agent.delete(&url).force_send_body(),
            }
            .content_type("application/json")
            .send(body.as_str());
            match reply.map(|response| response.status().as_u16()) {
                Ok(204) => Reply::Renewed,
                Ok(404) => Reply::NoRoom,
                _ => Reply::Failed,
            }
        })
    };
    let started = tokio::time::Instant::now();
    let mut clock = Clock::default();
    loop {
        let reply = request("POST").await.unwrap_or(Reply::Failed);
        if clock.over(started.elapsed(), reply) {
            let _ = events.send(ServiceEvent::CallLeaseEnded { id }).await;
            return;
        }
        if tokio::time::timeout(RENEW, &mut stop).await.is_ok() {
            let _ = request("DELETE").await;
            return;
        }
    }
}

impl Desktop {
    /// Start a call in the open conversation.
    pub fn start_call(
        &mut self,
        index: usize,
        target: nuschl_core::model::TextTarget,
        cx: &mut Context<Self>,
    ) {
        #[cfg(target_os = "linux")]
        if self.native_available(self.call_host()) {
            self.start_native_call(index, target, None, cx);
            return;
        }
        match self.create_call(index, target) {
            Ok(link) => self.open_call(link, "Call", cx),
            Err(error) => self.client.servers[index].set_notice(error),
        }
    }

    pub fn call_host(&self) -> &str {
        match self.settings.call_host.as_str() {
            "" => DEFAULT_HOST,
            host => host,
        }
    }

    pub fn create_call(&mut self, index: usize, target: TextTarget) -> Result<Link, String> {
        let link = Link::generate(self.call_host(), &mut getrandom::SysRng)
            .map_err(|error| format!("No call: {error}"))?;
        self.client.start_call(index, target, link.url())?;
        Ok(link)
    }

    /// Our call gets a new link; the page opens again on it.
    pub fn rotate_call(&mut self, cx: &mut Context<Self>) {
        let Some(link) = self
            .call
            .as_ref()
            .and_then(|call| Link::generate(call.link.host(), &mut getrandom::SysRng).ok())
        else {
            return;
        };
        self.client.rotate_call(link.url());
        self.open_call(link, "New call link", cx);
    }

    /// Open the creator's page, post the Join link as text for stock Mumble
    /// clients and keep the lease; a previous lease ends.
    fn open_call(&mut self, link: Link, label: &str, cx: &mut Context<Self>) {
        let url = link.creator_url().expect("the creator's link");
        cx.open_url(&page_url(&url, self.settings.relay_only));
        self.announce_call(link, label);
    }

    pub fn announce_call(&mut self, link: Link, label: &str) {
        let _ = self.client.send_text(format!("{label}: {}", link.url()));
        let (url, body) = lease_request(&link);
        self.call_leases += 1;
        let (stop, stopped) = oneshot::channel();
        self.services.lease(self.call_leases, url, body, stopped);
        self.call = Some(Creator {
            link,
            lease: self.call_leases,
            _stop: stop,
        });
    }

    pub fn end_call(&mut self) {
        #[cfg(target_os = "linux")]
        self.stop_native_owner();
        if self.client.own_call().is_some() {
            self.client.end_call();
            let _ = self.client.send_text("Call ended".into());
        }
        self.call = None;
    }

    /// The lease runs while the client has our call on this link.
    pub fn sync_call(&mut self) {
        let link = self.client.own_call().map(|call| call.link.as_str());
        if self
            .call
            .as_ref()
            .is_some_and(|call| Some(call.link.url().as_str()) != link)
        {
            self.call = None;
        }
    }

    /// Lease `id` is over: its call ends, if it is still ours.
    pub fn call_lease_ended(&mut self, id: u64) {
        if self.call.as_ref().is_some_and(|call| call.lease == id) {
            self.call = None;
            self.client.end_call();
        }
    }
}

/// A Join link someone posted, checked by `nuschl-stream`'s parser.
pub fn join_link(link: &str) -> Option<Link> {
    Link::parse(link, nuschl_core::call::link_host(link)?).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn secs(seconds: u64) -> Duration {
        Duration::from_secs(seconds)
    }

    #[test]
    fn the_room_has_until_the_grace_period_to_appear() {
        let mut clock = Clock::default();
        assert!(!clock.over(secs(0), Reply::NoRoom));
        assert!(!clock.over(secs(110), Reply::Failed));
        assert!(clock.over(GRACE, Reply::NoRoom));
        assert!(Clock::default().over(GRACE, Reply::Failed));
    }

    #[test]
    fn the_lease_sends_the_room_and_host_key_and_keeps_p() {
        let link = Link::generate("stream.example", &mut getrandom::SysRng).unwrap();
        let (url, body) = lease_request(&link);
        assert_eq!(url, "https://stream.example/sfu/lease");
        let hex = |bytes: &[u8]| bytes.iter().map(|b| format!("{b:02x}")).collect::<String>();
        let publisher = link.publisher().unwrap();
        let sent: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(
            sent["stream_id"],
            hex(&nuschl_stream::stream_id(link.secret()))
        );
        assert_eq!(sent["host_key"], hex(&nuschl_stream::host_key(publisher)));
        assert!(!body.contains(&hex(publisher)));
        let local = Link::generate("127.0.0.1:8443", &mut getrandom::SysRng).unwrap();
        assert_eq!(lease_request(&local).0, "http://127.0.0.1:8443/sfu/lease");
    }

    #[test]
    fn join_links_pass_the_stream_parser() {
        let link = Link::generate("stream.example", &mut getrandom::SysRng).unwrap();
        assert_eq!(
            join_link(&link.url()).map(|link| link.url()),
            Some(link.url())
        );
        assert!(join_link(&link.url().replace("&h=", "&x=")).is_none());
        assert!(join_link("https://stream.example/call#v=1").is_none());
    }

    #[test]
    fn a_renewed_lease_ends_with_its_room_or_after_the_timeout() {
        let mut clock = Clock::default();
        assert!(!clock.over(secs(20), Reply::Renewed));
        assert!(!clock.over(secs(49), Reply::Failed));
        assert!(clock.over(secs(50), Reply::Failed));
        let mut clock = Clock::default();
        assert!(!clock.over(secs(1000), Reply::Renewed));
        assert!(clock.over(secs(1010), Reply::NoRoom));
        assert!(Clock::default().over(MAX, Reply::Renewed));
    }
}
