// SPDX-License-Identifier: EUPL-1.2
//! Native admission and key signaling to the configured call host over WSS.

use crate::identity::CryptoHandle;
use futures_util::{SinkExt, StreamExt};
use nuschl_core::network::PeerPolicy;
use nuschl_ipc::crypto::call::{Action, Outcome, Request};
use nuschl_protocol::sfu::{self, ClientMessage, Codec, ServerMessage, Welcome};
use std::{sync::Arc, time::Duration};
use tokio::{net::TcpStream, time::timeout};
use tokio_rustls::{TlsConnector, client::TlsStream};
use tokio_tungstenite::{
    WebSocketStream, client_async_with_config,
    tungstenite::{Message, protocol::WebSocketConfig},
};

const HANDSHAKE: Duration = Duration::from_secs(10);
type Socket = WebSocketStream<TlsStream<TcpStream>>;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error(transparent)]
    Worker(#[from] crate::Error),
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error("call signaling: {0}")]
    Protocol(&'static str),
    #[error("call host refused admission ({0})")]
    Refused(String),
    #[error("call signaling timed out")]
    Timeout,
}

pub struct Connect<'a> {
    pub generation: u64,
    /// The configured SFU hostname, with an optional port.
    pub host: &'a str,
    pub link: &'a str,
    pub codecs: &'a [Codec],
    pub policy: PeerPolicy,
}

pub struct Connection {
    welcome: Welcome,
    socket: Socket,
    crypto: CryptoHandle,
    generation: u64,
}

impl Connection {
    pub async fn connect(crypto: CryptoHandle, options: Connect<'_>) -> Result<Self, Error> {
        let roots =
            rustls::RootCertStore::from_iter(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
        Self::connect_with_roots(crypto, options, roots).await
    }

    async fn connect_with_roots(
        crypto: CryptoHandle,
        options: Connect<'_>,
        roots: rustls::RootCertStore,
    ) -> Result<Self, Error> {
        let generation = options.generation;
        if options.codecs.is_empty() || options.codecs.len() > 2 {
            return Err(Error::Protocol("invalid decoder list"));
        }
        let Outcome::Opened { room, verifier } = call(
            &crypto,
            generation,
            Action::Open {
                host: options.host.to_owned(),
                link: options.link.to_owned(),
                relay_only: options.policy.relay_only,
            },
        )
        .await?
        else {
            return Err(Error::Protocol("invalid room metadata"));
        };
        let result = timeout(HANDSHAKE, async {
            let mut socket = open(options.host, roots).await?;
            let ServerMessage::Hello { nonce } = read(&mut socket).await? else {
                return Err(Error::Protocol("expected a challenge"));
            };
            let nonce = sfu::from_hex(&nonce).ok_or(Error::Protocol("invalid challenge"))?;
            let Outcome::Admitted {
                proof,
                host_proof,
                create,
            } = call(&crypto, generation, Action::Admit { nonce }).await?
            else {
                return Err(Error::Protocol("invalid admission proof"));
            };
            write(
                &mut socket,
                &ClientMessage::Join {
                    room: hex::encode(room),
                    verifier: hex::encode(verifier),
                    proof: hex::encode(proof),
                    host_proof: host_proof.map(hex::encode),
                    create: create.map(|keys| sfu::Create {
                        admit_key: hex::encode(keys.admit_key),
                        host_key: hex::encode(keys.host_key),
                    }),
                    codecs: options.codecs.to_vec(),
                },
            )
            .await?;
            match read(&mut socket).await? {
                ServerMessage::Welcome(welcome)
                    if welcome.peer > 0
                        && welcome.peers.len() <= 256
                        && options.codecs.contains(&welcome.codec) =>
                {
                    Ok((socket, welcome))
                }
                ServerMessage::Error { reason }
                    if reason.len() <= 32
                        && reason.bytes().all(|b| b.is_ascii_lowercase() || b == b'_') =>
                {
                    Err(Error::Refused(reason.into_owned()))
                }
                _ => Err(Error::Protocol("invalid welcome")),
            }
        })
        .await
        .unwrap_or(Err(Error::Timeout));
        match result {
            Ok((socket, welcome)) => Ok(Self {
                welcome,
                socket,
                crypto,
                generation,
            }),
            Err(error) => {
                let _ = call(&crypto, generation, Action::Close).await;
                Err(error)
            }
        }
    }

    pub fn welcome(&self) -> &Welcome {
        &self.welcome
    }

    pub async fn next(&mut self) -> Result<ServerMessage, Error> {
        read(&mut self.socket).await
    }

    /// Pair with the selected peer, apply its key message, or send direct
    /// signaling. The worker seals or opens the payload and checks this call's
    /// generation, peer and relay policy.
    pub async fn signal(&mut self, action: Action) -> Result<Outcome, Error> {
        if !matches!(
            action,
            Action::Pair { .. } | Action::Key { .. } | Action::Seal { .. }
        ) {
            return Err(Error::Protocol("invalid key action"));
        }
        let outcome = call(&self.crypto, self.generation, action).await?;
        if let Outcome::Send { peer, data, .. } = &outcome {
            write(
                &mut self.socket,
                &ClientMessage::Key {
                    to: *peer,
                    data: hex::encode(data),
                },
            )
            .await?;
        }
        Ok(outcome)
    }

    pub async fn close(mut self) -> Result<(), Error> {
        let _ = timeout(HANDSHAKE, self.socket.close(None)).await;
        call(&self.crypto, self.generation, Action::Close).await?;
        Ok(())
    }
}

async fn call(crypto: &CryptoHandle, generation: u64, action: Action) -> Result<Outcome, Error> {
    Ok(crypto
        .call_signal(Request { generation, action })
        .await?
        .outcome)
}

async fn open(host: &str, roots: rustls::RootCertStore) -> Result<Socket, Error> {
    let (name, port) = match host.split_once(':') {
        Some((name, port)) => (
            name,
            port.parse().map_err(|_| Error::Protocol("invalid port"))?,
        ),
        None => (host, 443),
    };
    let name = rustls::pki_types::ServerName::try_from(name.to_owned())
        .map_err(|_| Error::Protocol("invalid host"))?;
    let config = rustls::ClientConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .map_err(|_| Error::Protocol("cannot configure TLS"))?
    .with_root_certificates(roots)
    .with_no_client_auth();
    let stream = TcpStream::connect((name.to_str().as_ref(), port)).await?;
    let tls = TlsConnector::from(Arc::new(config))
        .connect(name, stream)
        .await?;
    let limits = WebSocketConfig::default()
        .max_message_size(Some(sfu::MAX_MESSAGE_BYTES))
        .max_frame_size(Some(sfu::MAX_MESSAGE_BYTES));
    let (socket, _) = client_async_with_config(format!("wss://{host}/sfu"), tls, Some(limits))
        .await
        .map_err(|_| Error::Protocol("WebSocket handshake failed"))?;
    Ok(socket)
}

async fn write(socket: &mut Socket, message: &ClientMessage) -> Result<(), Error> {
    let message = serde_json::to_string(message).map_err(|_| Error::Protocol("invalid request"))?;
    timeout(HANDSHAKE, socket.send(Message::text(message)))
        .await
        .map_err(|_| Error::Timeout)?
        .map_err(|_| Error::Protocol("cannot send request"))
}

async fn read(socket: &mut Socket) -> Result<ServerMessage, Error> {
    loop {
        match socket.next().await {
            Some(Ok(Message::Text(text))) => {
                let message =
                    serde_json::from_str(&text).map_err(|_| Error::Protocol("invalid response"))?;
                if let ServerMessage::Key { from, data } = &message
                    && (*from == 0 || !sfu::valid_key_message(data))
                {
                    return Err(Error::Protocol("invalid key message"));
                }
                return Ok(message);
            }
            Some(Ok(Message::Ping(_))) => socket
                .flush()
                .await
                .map_err(|_| Error::Protocol("closed"))?,
            Some(Ok(Message::Pong(_))) => {}
            _ => return Err(Error::Protocol("closed")),
        }
    }
}

#[cfg(all(test, target_os = "linux"))]
mod tests;
