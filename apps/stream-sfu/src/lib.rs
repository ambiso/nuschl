// SPDX-License-Identifier: EUPL-1.2
//! The companion SFU for calls and screen sharing on stock Mumble servers
//! (docs/video-plan.md §2, §4, §8): rooms named by a call link's `stream_id`,
//! admission by proofs of the link's keys, forwarding of encrypted RTP to every
//! subscriber, a relay for the participants' key messages, and TURN credentials
//! for admitted peers. Media and key messages pass through unchanged; the SFU
//! holds no media key.

pub mod config;
pub mod media;
pub mod protocol;
pub mod rooms;

use config::{Config, Listen};
use futures_util::{SinkExt, StreamExt};
use media::{Command, Core};
use protocol::{ClientMessage, ServerMessage, from_hex};
use rooms::{Bucket, Join, PeerId};
use std::{
    net::{IpAddr, Ipv4Addr},
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};
use tokio::{
    io::{AsyncRead, AsyncWrite},
    net::{TcpListener, UdpSocket},
    sync::mpsc,
    time::timeout,
};
use tokio_tungstenite::tungstenite::{
    Message,
    handshake::server::{Request, Response},
    protocol::WebSocketConfig,
};

/// The WebSocket upgrade, and then the join, must each arrive within this.
const HANDSHAKE: Duration = Duration::from_secs(10);
const PING: Duration = Duration::from_secs(30);
/// Messages waiting for one peer's WebSocket; a peer that falls behind leaves.
const OUTGOING: usize = 64;

/// Binds both sockets, reports them on stdout and serves until the task ends.
pub async fn serve(config: Config) -> std::io::Result<()> {
    let config = Arc::new(config);
    let socket = Arc::new(UdpSocket::bind(config.media_listen).await?);
    let turn = match &config.turn {
        Some(turn) => Some(media::Turn {
            urls: turn.urls.clone(),
            secret: std::fs::read_to_string(&turn.secret_file)?
                .trim()
                .as_bytes()
                .to_vec(),
            lifetime: Duration::from_secs(turn.lifetime_secs),
        }),
        None => None,
    };
    let core = Core::new(config.clone(), socket.clone(), turn)?;
    let (commands, receiver) = mpsc::channel(1024);
    tokio::spawn(core.run(receiver));
    let connections = Arc::new(AtomicUsize::new(0));
    let mut next_id: PeerId = 0;
    match &config.listen {
        Listen::Tcp(address) => {
            let listener = TcpListener::bind(address).await?;
            println!(
                "nuschl-stream-sfu: signalling {} media {}",
                listener.local_addr()?,
                socket.local_addr()?
            );
            loop {
                let Ok((stream, client)) = listener.accept().await else {
                    tokio::time::sleep(Duration::from_millis(100)).await;
                    continue;
                };
                next_id += 1;
                spawn(
                    stream,
                    Some(client.ip()),
                    next_id,
                    &config,
                    &commands,
                    &connections,
                );
            }
        }
        Listen::Unix(path) => {
            // A socket left by an earlier run.
            let _ = std::fs::remove_file(path);
            let listener = tokio::net::UnixListener::bind(path)?;
            {
                use std::os::unix::fs::PermissionsExt as _;
                // The directory decides who may connect.
                std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o660))?;
            }
            println!(
                "nuschl-stream-sfu: signalling {} media {}",
                path.display(),
                socket.local_addr()?
            );
            loop {
                let Ok((stream, _)) = listener.accept().await else {
                    tokio::time::sleep(Duration::from_millis(100)).await;
                    continue;
                };
                next_id += 1;
                spawn(stream, None, next_id, &config, &commands, &connections);
            }
        }
    }
}

fn spawn<S: AsyncRead + AsyncWrite + Unpin + Send + 'static>(
    stream: S,
    client: Option<IpAddr>,
    id: PeerId,
    config: &Arc<Config>,
    commands: &mpsc::Sender<Command>,
    connections: &Arc<AtomicUsize>,
) {
    if connections.fetch_add(1, Ordering::Relaxed) >= config.max_connections {
        connections.fetch_sub(1, Ordering::Relaxed);
        return;
    }
    let (config, commands, connections) = (config.clone(), commands.clone(), connections.clone());
    tokio::spawn(async move {
        connection(stream, client, id, &config, &commands).await;
        connections.fetch_sub(1, Ordering::Relaxed);
    });
}

fn join(message: ClientMessage) -> Option<Join> {
    let ClientMessage::Join {
        room,
        verifier,
        proof,
        host_proof,
        create,
    } = message
    else {
        return None;
    };
    Some(Join {
        room: (from_hex(&room)?, from_hex(&verifier)?),
        proof: from_hex(&proof)?,
        host_proof: match host_proof {
            Some(proof) => Some(from_hex(&proof)?),
            None => None,
        },
        create: match create {
            Some(create) => Some((from_hex(&create.admit_key)?, from_hex(&create.host_key)?)),
            None => None,
        },
    })
}

fn text(message: &ServerMessage) -> Message {
    Message::text(serde_json::to_string(message).expect("serializable"))
}

/// One peer's WebSocket: hello, join, then messages both ways until either
/// side closes.
// tungstenite's handshake callback returns its HTTP error response by value.
#[allow(clippy::result_large_err)]
async fn connection<S: AsyncRead + AsyncWrite + Unpin>(
    stream: S,
    client: Option<IpAddr>,
    id: PeerId,
    config: &Config,
    commands: &mpsc::Sender<Command>,
) {
    // Behind the web server (a Unix socket), the client address comes from it.
    let mut forwarded = None;
    let callback = |request: &Request, response: Response| {
        forwarded = request
            .headers()
            .get("x-real-ip")
            .and_then(|value| value.to_str().ok()?.parse::<IpAddr>().ok());
        Ok(response)
    };
    let limits = WebSocketConfig::default()
        .max_message_size(Some(config.max_message_bytes))
        .max_frame_size(Some(config.max_message_bytes));
    let Ok(Ok(mut socket)) = timeout(
        HANDSHAKE,
        tokio_tungstenite::accept_hdr_async_with_config(stream, callback, Some(limits)),
    )
    .await
    else {
        return;
    };
    let client = client
        .or(forwarded)
        .unwrap_or(IpAddr::V4(Ipv4Addr::UNSPECIFIED));
    let nonce = rooms::random::<32>();
    let hello = ServerMessage::Hello {
        nonce: hex::encode(nonce),
    };
    if socket.send(text(&hello)).await.is_err() {
        return;
    }
    let first = match timeout(HANDSHAKE, socket.next()).await {
        Ok(Some(Ok(Message::Text(text)))) => serde_json::from_str(&text).ok().and_then(join),
        _ => None,
    };
    let Some(first) = first else {
        let _ = socket
            .send(text(&ServerMessage::Error { reason: "join" }))
            .await;
        return;
    };
    let (out, mut outgoing) = mpsc::channel(OUTGOING);
    let join = Command::Join {
        id,
        client,
        nonce,
        join: first,
        out,
    };
    if commands.send(join).await.is_err() {
        return;
    }
    let rate = f64::from(config.messages_per_second);
    let mut budget = Bucket::new(rate, 2.0 * rate, Instant::now());
    let mut ping = tokio::time::interval(PING);
    loop {
        tokio::select! {
            incoming = socket.next() => match incoming {
                Some(Ok(Message::Text(text))) => {
                    let message = serde_json::from_str::<ClientMessage>(&text);
                    let reason = match message {
                        _ if !budget.take(1.0, Instant::now()) => "rate",
                        Ok(message) => {
                            if commands.send(Command::Message { id, message }).await.is_err() {
                                break;
                            }
                            continue;
                        }
                        Err(_) => "message",
                    };
                    let _ = socket.send(self::text(&ServerMessage::Error { reason })).await;
                    break;
                }
                Some(Ok(Message::Ping(_) | Message::Pong(_))) => {}
                _ => break,
            },
            message = outgoing.recv() => match message {
                Some(message) => {
                    let last = matches!(message, ServerMessage::Error { .. });
                    if socket.send(text(&message)).await.is_err() || last {
                        break;
                    }
                }
                None => break,
            },
            _ = ping.tick() => {
                if socket.send(Message::Ping(Default::default())).await.is_err() {
                    break;
                }
            }
        }
    }
    let _ = commands.send(Command::Leave { id }).await;
    let _ = socket.close(None).await;
}
