// SPDX-License-Identifier: EUPL-1.2

use super::*;
use nuschl_protocol::sfu::Role;
use tokio::net::TcpListener;

const CA: &str = "127.0.0.1";
const LINK: &str = "v=1&k=AAECAwQFBgcICQoLDA0ODxAREhMUFRYXGBkaGxwdHh8&h=8FkoEkvVnGXqAGKF9u7ezIde49CL2ZPe5AQZJ3_d3tQ";
const OWNER: &str = "ICEiIyQlJicoKSorLC0uLzAxMjM0NTY3ODk6Ozw9Pj8";

fn fixture(name: &str) -> std::path::PathBuf {
    // Cargo's workspace test builds standalone binaries beside the deps folder.
    // The Nix unit leaf supplies the same binaries on PATH.
    let current = std::env::current_exe().unwrap();
    if let Some(built) = current
        .parent()
        .and_then(|deps| deps.parent())
        .map(|bin| bin.join(name))
        && built.is_file()
    {
        return built;
    }
    nuschl_testkit::program(name)
}

/// Owned loopback TLS terminator in front of the real SFU's HTTP listener.
async fn tls_proxy(sfu: &nuschl_testkit::sfu::Sfu) -> (String, rustls::RootCertStore) {
    let upstream: std::net::SocketAddr = sfu
        .url
        .strip_prefix("ws://")
        .unwrap()
        .strip_suffix("/sfu")
        .unwrap()
        .parse()
        .unwrap();
    let listener = TcpListener::bind((CA, 0)).await.unwrap();
    let host = listener.local_addr().unwrap().to_string();
    let cert = rcgen::generate_simple_self_signed(vec![CA.into()]).unwrap();
    let der = cert.cert.der().clone();
    let mut roots = rustls::RootCertStore::empty();
    roots.add(der.clone()).unwrap();
    let config = rustls::ServerConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .unwrap()
    .with_no_client_auth()
    .with_single_cert(
        vec![der],
        rustls::pki_types::PrivatePkcs8KeyDer::from(cert.signing_key.serialize_der()).into(),
    )
    .unwrap();
    let acceptor = tokio_rustls::TlsAcceptor::from(Arc::new(config));
    tokio::spawn(async move {
        while let Ok((socket, _)) = listener.accept().await {
            let acceptor = acceptor.clone();
            tokio::spawn(async move {
                if let Ok(mut tls) = acceptor.accept(socket).await {
                    let mut plain = TcpStream::connect(upstream).await.unwrap();
                    let _ = tokio::io::copy_bidirectional(&mut tls, &mut plain).await;
                }
            });
        }
    });
    (host, roots)
}

async fn next_key(connection: &mut Connection) -> Outcome {
    timeout(HANDSHAKE, async {
        loop {
            if let ServerMessage::Key { from, data } = connection.next().await.unwrap() {
                return connection
                    .signal(Action::Key {
                        peer: from,
                        data: hex::decode(data).unwrap(),
                    })
                    .await
                    .unwrap();
            }
        }
    })
    .await
    .unwrap()
}

#[tokio::test]
async fn two_native_peers_admit_and_signal_through_the_sfu_over_wss() {
    nuschl_testkit::sandbox();
    let worker = fixture("nuschl-crypto-worker");
    let sfu = nuschl_testkit::sfu::start(fixture("nuschl-stream-sfu"), "");
    let (host, roots) = tls_proxy(&sfu).await;
    let directory = nuschl_testkit::temp_dir();
    let alice = CryptoHandle::start(worker.clone(), directory.path().join("alice"));
    let bob = CryptoHandle::start(worker, directory.path().join("bob"));
    let posted = format!("https://{host}/call#{LINK}");
    let owner = format!("{posted}&p={OWNER}");
    let connect = |generation, link| Connect {
        generation,
        host: &host,
        link,
        codecs: &[Codec::Av1, Codec::Vp9],
        policy: PeerPolicy::default(),
    };
    assert!(matches!(
        Connection::connect(alice.clone(), connect(1, &owner)).await,
        Err(Error::Io(_))
    ));
    let mut alice = Connection::connect_with_roots(alice, connect(2, &owner), roots.clone())
        .await
        .unwrap();
    let mut bob = Connection::connect_with_roots(bob, connect(1, &posted), roots)
        .await
        .unwrap();
    let (a, b) = (alice.welcome().peer, bob.welcome().peer);
    assert_eq!(alice.welcome().role, Role::Host);
    assert_eq!(bob.welcome().role, Role::Participant);

    alice
        .signal(Action::Pair {
            peer: b,
            publisher: true,
        })
        .await
        .unwrap();
    bob.signal(Action::Pair {
        peer: a,
        publisher: false,
    })
    .await
    .unwrap();
    assert!(
        matches!(next_key(&mut alice).await, Outcome::Send { peer, ready: false, .. } if peer == b)
    );
    assert!(
        matches!(next_key(&mut bob).await, Outcome::Send { peer, ready: true, .. } if peer == a)
    );
    assert!(matches!(next_key(&mut alice).await, Outcome::Ready { peer } if peer == b));

    let candidate = b"a=candidate:1 1 UDP 1 127.0.0.1 54321 typ host";
    alice
        .signal(Action::Seal {
            peer: b,
            data: candidate.to_vec(),
        })
        .await
        .unwrap();
    assert!(
        matches!(next_key(&mut bob).await, Outcome::Signal { peer, data } if peer == a && data == candidate)
    );
    alice.close().await.unwrap();
    bob.close().await.unwrap();
}
