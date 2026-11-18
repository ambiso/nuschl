// SPDX-License-Identifier: EUPL-1.2
//! The one HTTP request a connection makes: the WebSocket upgrade at `/sfu`,
//! or a lease renewal (`POST`) or end (`DELETE`) at `/sfu/lease`. The head is
//! at most [`MAX_HEAD`] bytes and a lease body at most [`MAX_BODY`].

use std::net::IpAddr;
use tokio::io::{AsyncRead, AsyncReadExt};

pub const MAX_HEAD: usize = 8 * 1024;
pub const MAX_BODY: usize = 1024;

#[derive(Debug, PartialEq, Eq)]
pub enum Request {
    /// A WebSocket upgrade with its `Sec-WebSocket-Key`.
    WebSocket(String),
    Lease {
        end: bool,
        body: Vec<u8>,
    },
    /// Anything else, answered with this status.
    Refused(u16),
}

/// Reads one request head (and a lease body). Returns the request and the
/// proxy's `X-Real-IP`, or `None` when the connection ends or overruns a bound.
pub async fn read<S: AsyncRead + Unpin>(stream: &mut S) -> Option<(Request, Option<IpAddr>)> {
    let mut buffer = Vec::with_capacity(1024);
    loop {
        let mut chunk = [0; 1024];
        let length = stream.read(&mut chunk).await.ok()?;
        if length == 0 {
            return None;
        }
        buffer.extend_from_slice(&chunk[..length]);
        if let Some(parsed) = parse(&buffer)? {
            return Some(parsed);
        }
    }
}

/// `Some(None)` while the request is incomplete.
fn parse(buffer: &[u8]) -> Option<Option<(Request, Option<IpAddr>)>> {
    let mut headers = [httparse::EMPTY_HEADER; 32];
    let mut request = httparse::Request::new(&mut headers);
    let head = match request.parse(buffer).ok()? {
        httparse::Status::Partial if buffer.len() <= MAX_HEAD => return Some(None),
        httparse::Status::Partial => return None,
        httparse::Status::Complete(head) => head,
    };
    let header = |name: &str| {
        request
            .headers
            .iter()
            .find(|h| h.name.eq_ignore_ascii_case(name))
            .and_then(|h| std::str::from_utf8(h.value).ok())
    };
    let real_ip = header("x-real-ip").and_then(|ip| ip.parse().ok());
    let request = match (request.method?, request.path?) {
        ("GET", "/sfu") => {
            let upgrade = header("upgrade").is_some_and(|u| u.eq_ignore_ascii_case("websocket"));
            match header("sec-websocket-key") {
                Some(key) if upgrade && header("sec-websocket-version") == Some("13") => {
                    Request::WebSocket(key.to_owned())
                }
                _ => Request::Refused(400),
            }
        }
        (method @ ("POST" | "DELETE"), "/sfu/lease") => {
            match header("content-length").and_then(|l| l.parse::<usize>().ok()) {
                Some(length) if length <= MAX_BODY => {
                    let Some(body) = buffer.get(head..head + length) else {
                        return Some(None);
                    };
                    Request::Lease {
                        end: method == "DELETE",
                        body: body.to_vec(),
                    }
                }
                Some(_) => Request::Refused(413),
                None => Request::Refused(411),
            }
        }
        _ => Request::Refused(404),
    };
    Some(Some((request, real_ip)))
}

/// A complete response without a body.
pub fn status(code: u16) -> String {
    let reason = match code {
        204 => "No Content",
        400 => "Bad Request",
        403 => "Forbidden",
        404 => "Not Found",
        411 => "Length Required",
        413 => "Content Too Large",
        429 => "Too Many Requests",
        _ => "Service Unavailable",
    };
    format!("HTTP/1.1 {code} {reason}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parsed(text: &str) -> Option<Option<(Request, Option<IpAddr>)>> {
        parse(text.as_bytes())
    }

    #[test]
    fn requests() {
        let upgrade = "GET /sfu HTTP/1.1\r\nHost: s\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n\
                       Sec-WebSocket-Key: a2V5\r\nSec-WebSocket-Version: 13\r\nX-Real-IP: 192.0.2.7\r\n\r\n";
        assert_eq!(
            parsed(upgrade),
            Some(Some((
                Request::WebSocket("a2V5".into()),
                Some("192.0.2.7".parse().expect("ip"))
            )))
        );
        assert_eq!(parsed(&upgrade[..40]), Some(None), "incomplete");
        let lease = "POST /sfu/lease HTTP/1.1\r\nContent-Length: 2\r\n\r\n{}";
        let request = Request::Lease {
            end: false,
            body: b"{}".to_vec(),
        };
        assert_eq!(parsed(lease), Some(Some((request, None))));
        assert_eq!(
            parsed(&lease[..lease.len() - 1]),
            Some(None),
            "body incomplete"
        );
        let end = "DELETE /sfu/lease HTTP/1.1\r\nContent-Length: 0\r\n\r\n";
        assert!(matches!(
            parsed(end),
            Some(Some((Request::Lease { end: true, .. }, _)))
        ));
        let refused = |text: &str| parsed(text).and_then(|p| p).map(|(r, _)| r);
        assert_eq!(
            refused("GET /sfu HTTP/1.1\r\n\r\n"),
            Some(Request::Refused(400))
        );
        assert_eq!(
            refused("GET /other HTTP/1.1\r\n\r\n"),
            Some(Request::Refused(404))
        );
        assert_eq!(
            refused("POST /sfu/lease HTTP/1.1\r\n\r\n"),
            Some(Request::Refused(411))
        );
        assert_eq!(
            refused("POST /sfu/lease HTTP/1.1\r\nContent-Length: 1025\r\n\r\n"),
            Some(Request::Refused(413))
        );
        let long = format!("GET /sfu HTTP/1.1\r\nX: {}", "a".repeat(MAX_HEAD));
        assert_eq!(parsed(&long), None, "the head bound");
        assert_eq!(parsed("\x00\x01 junk\r\n\r\n"), None);
    }
}
