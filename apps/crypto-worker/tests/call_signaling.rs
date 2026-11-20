// SPDX-License-Identifier: EUPL-1.2
#![cfg(target_os = "linux")]

use nuschl_ipc::crypto::call::{Action, Outcome, Request};
use nuschl_ipc::crypto::{CryptoRequest, epoch};
use nuschl_session::identity::CryptoHandle;
use nuschl_stream::Link;
use std::{os::unix::net::UnixStream, time::Duration};

fn media_socket() -> (tokio::net::UnixStream, std::os::fd::OwnedFd) {
    let (receive, send) = UnixStream::pair().unwrap();
    receive.set_nonblocking(true).unwrap();
    (
        tokio::net::UnixStream::from_std(receive).unwrap(),
        send.into(),
    )
}

async fn next_epoch(socket: &mut tokio::net::UnixStream) -> Option<epoch::Update> {
    tokio::time::timeout(Duration::from_secs(2), epoch::read_async(socket))
        .await
        .unwrap()
        .unwrap()
}

async fn request(worker: &CryptoHandle, generation: u64, action: Action) -> Outcome {
    worker
        .call_signal(Request { generation, action })
        .await
        .unwrap()
        .outcome
}

fn send(outcome: Outcome, to: u64, ready: bool) -> Vec<u8> {
    let Outcome::Send {
        peer,
        data,
        ready: keyed,
    } = outcome
    else {
        panic!("expected a message for the peer");
    };
    assert_eq!((peer, keyed), (to, ready));
    data
}

#[tokio::test]
async fn native_call_signaling_keeps_keys_in_two_confined_workers() {
    nuschl_testkit::sandbox();
    let directory = nuschl_testkit::temp_dir();
    let executable = env!("CARGO_BIN_EXE_nuschl-crypto-worker");
    let alice = CryptoHandle::start(executable.into(), directory.path().join("alice"));
    let bob = CryptoHandle::start(executable.into(), directory.path().join("bob"));
    let host = "127.0.0.1:443";
    let link = Link::generate(host, &mut getrandom::SysRng).unwrap();
    let open = |creator, relay_only| Action::Open {
        host: host.into(),
        link: if creator {
            link.creator_url().unwrap()
        } else {
            link.url()
        },
        relay_only,
    };
    for (handle, creator) in [(&alice, true), (&bob, false)] {
        assert!(matches!(
            request(handle, 1, open(creator, false)).await,
            Outcome::Opened { room, verifier }
                if room == *link.keys().stream_id() && verifier == *link.room_check()
        ));
        let nonce = [9; 32];
        assert!(
            handle
                .request(CryptoRequest::Call(Request {
                    generation: 1,
                    action: Action::Admit { nonce },
                }))
                .await
                .is_err()
        );
        let reply = handle
            .call_signal(Request {
                generation: 1,
                action: Action::Admit { nonce },
            })
            .await
            .unwrap();
        assert!(!format!("{reply:?}").contains("admit_key"));
        let Outcome::Admitted {
            proof,
            host_proof,
            create,
        } = reply.outcome
        else {
            panic!("expected SFU admission");
        };
        assert_eq!(proof, link.admission_proofs(&nonce).0);
        assert_eq!(host_proof.is_some(), creator);
        if let Some(create) = create {
            assert!(creator);
            assert_eq!(create.admit_key, *link.keys().admit_key());
            assert_eq!(
                nuschl_stream::room_check(&create.host_key),
                *link.room_check()
            );
        } else {
            assert!(!creator);
        }
    }

    let (mut alice_rtc, alice_media) = media_socket();
    let (mut bob_rtc, bob_media) = media_socket();
    let file = std::fs::File::open("/dev/null").unwrap();
    assert!(alice.attach_media(1, file.into()).await.is_err());
    alice.attach_media(1, alice_media).await.unwrap();
    bob.attach_media(1, bob_media).await.unwrap();
    let (mut duplicate, endpoint) = media_socket();
    assert!(alice.attach_media(1, endpoint).await.is_err());
    assert!(next_epoch(&mut duplicate).await.is_none());

    request(&alice, 1, Action::Publish { peer: 1 }).await;
    let initial = next_epoch(&mut alice_rtc).await.unwrap();
    assert_eq!((initial.peer, initial.epoch), (1, 1));
    let join = send(
        request(&bob, 1, Action::Subscribe { peer: 1 }).await,
        1,
        false,
    );
    let key = |peer, data| Action::Key { peer, data };
    let seal = |peer, data| Action::Seal {
        peer,
        publisher: true,
        data,
    };
    assert!(matches!(
        request(&alice, 1, key(2, join)).await,
        Outcome::Done
    ));
    let grant = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if let Outcome::KeyBatch { messages } = request(&alice, 1, Action::Tick).await {
                let [message]: [_; 1] = messages.try_into().ok().unwrap();
                assert_eq!(message.peer, 2);
                break message.data;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
    let ack = send(request(&bob, 1, key(1, grant)).await, 1, true);
    let receive = next_epoch(&mut bob_rtc).await.unwrap();
    assert_eq!(
        (
            receive.generation,
            receive.peer,
            receive.epoch,
            receive.direction
        ),
        (1, 1, 2, epoch::Direction::Receive)
    );
    let mut byte = [0];
    assert_eq!(
        alice_rtc.try_read(&mut byte).unwrap_err().kind(),
        std::io::ErrorKind::WouldBlock
    );

    let candidate = b"a=candidate:1 1 UDP 1 127.0.0.1 54321 typ host";
    assert!(
        alice
            .call_signal(Request {
                generation: 1,
                action: seal(2, candidate.to_vec())
            })
            .await
            .is_err()
    );
    assert!(
        alice
            .call_signal(Request {
                generation: 1,
                action: key(3, ack.clone())
            })
            .await
            .is_err()
    );
    assert!(matches!(
        request(&alice, 1, key(2, ack)).await,
        Outcome::Ready { peer: 2 }
    ));

    tokio::time::sleep(Duration::from_millis(1_010)).await;
    request(&alice, 1, Action::Tick).await;
    let send_key = next_epoch(&mut alice_rtc).await.unwrap();
    assert_eq!(
        (
            send_key.generation,
            send_key.peer,
            send_key.epoch,
            send_key.direction
        ),
        (1, 1, 2, epoch::Direction::Send)
    );
    assert!(send_key.key == receive.key);

    let offer = send(
        request(&alice, 1, seal(2, candidate.to_vec())).await,
        2,
        false,
    );
    assert!(
        !offer
            .windows(candidate.len())
            .any(|bytes| bytes == candidate)
    );
    assert!(
        bob.call_signal(Request {
            generation: 1,
            action: key(3, offer.clone())
        })
        .await
        .is_err()
    );
    assert!(
        bob.call_signal(Request {
            generation: 2,
            action: key(1, offer.clone())
        })
        .await
        .is_err()
    );
    assert!(matches!(request(&bob, 1, key(1, offer.clone())).await,
        Outcome::Signal { peer: 1, publisher: false, data } if data == candidate));
    assert!(
        bob.call_signal(Request {
            generation: 1,
            action: key(1, offer.clone())
        })
        .await
        .is_err()
    );

    request(&bob, 1, Action::Left { peer: 1 }).await;
    let forgotten = next_epoch(&mut bob_rtc).await.unwrap();
    assert_eq!(
        (forgotten.peer, forgotten.direction),
        (1, epoch::Direction::ForgetReceive)
    );
    request(&bob, 1, Action::Close).await;
    assert!(next_epoch(&mut bob_rtc).await.is_none());
    request(&alice, 2, open(true, false)).await;
    assert!(next_epoch(&mut alice_rtc).await.is_none());
    let (mut old, endpoint) = media_socket();
    assert!(alice.attach_media(1, endpoint).await.is_err());
    assert!(next_epoch(&mut old).await.is_none());

    request(&bob, 2, open(false, true)).await;
    let disabled = bob
        .call_signal(Request {
            generation: 2,
            action: key(1, offer),
        })
        .await
        .unwrap_err();
    assert!(disabled.to_string().contains("direct signaling disabled"));
    let disabled = bob
        .call_signal(Request {
            generation: 2,
            action: seal(1, candidate.to_vec()),
        })
        .await
        .unwrap_err();
    assert!(disabled.to_string().contains("direct signaling disabled"));
}

#[tokio::test]
async fn worker_failure_keeps_the_call_generation_reserved() {
    nuschl_testkit::sandbox();
    let directory = nuschl_testkit::temp_dir();
    let executable = directory.path().join("crypto-worker");
    std::fs::copy(env!("CARGO_BIN_EXE_nuschl-crypto-worker"), &executable).unwrap();
    let worker = CryptoHandle::start(executable.clone(), directory.path().join("keys"));
    let host = "127.0.0.1";
    let link = Link::generate(host, &mut getrandom::SysRng).unwrap();
    let open = || Action::Open {
        host: host.into(),
        link: link.url(),
        relay_only: true,
    };
    request(&worker, 1, open()).await;

    // The test copy's inode belongs to this worker alone.
    let [pid] = nuschl_testkit::running(&executable)[..] else {
        panic!("expected one confined worker");
    };
    assert!(
        std::process::Command::new(nuschl_testkit::program("kill"))
            .args(["-KILL", &pid.to_string()])
            .status()
            .unwrap()
            .success()
    );
    nuschl_testkit::none_running(&executable, std::time::Duration::from_secs(2)).await;

    let old = |action| Request {
        generation: 1,
        action,
    };
    assert!(
        worker
            .call_signal(old(Action::Admit { nonce: [1; 32] }))
            .await
            .is_err()
    );
    assert!(matches!(
        request(&worker, 1, Action::Close).await,
        Outcome::Done
    ));
    assert!(worker.call_signal(old(open())).await.is_err());
    request(&worker, 2, open()).await;
    assert!(worker.call_signal(old(Action::Close)).await.is_err());
    assert!(matches!(
        request(&worker, 2, Action::Admit { nonce: [1; 32] }).await,
        Outcome::Admitted { .. }
    ));
}
