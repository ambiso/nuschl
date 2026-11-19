// SPDX-License-Identifier: EUPL-1.2
#![cfg(target_os = "linux")]

use nuschl_ipc::crypto::call::{Action, Outcome, Request};
use nuschl_session::identity::CryptoHandle;
use nuschl_stream::Link;

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
        assert!(matches!(
            request(handle, 1, Action::Admit { nonce }).await,
            Outcome::Admitted { proof, host_proof }
                if proof == link.admission_proofs(&nonce).0 && host_proof.is_some() == creator
        ));
    }

    request(
        &alice,
        1,
        Action::Pair {
            peer: 2,
            publisher: true,
        },
    )
    .await;
    let join = send(
        request(
            &bob,
            1,
            Action::Pair {
                peer: 1,
                publisher: false,
            },
        )
        .await,
        1,
        false,
    );
    let key = |peer, data| Action::Key { peer, data };
    let seal = |peer, data| Action::Seal { peer, data };
    let grant = send(request(&alice, 1, key(2, join)).await, 2, false);
    let ack = send(request(&bob, 1, key(1, grant)).await, 1, true);
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
    assert!(matches!(
        request(&alice, 1, key(2, ack)).await,
        Outcome::Ready { peer: 2 }
    ));

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
        Outcome::Signal { peer: 1, data } if data == candidate));
    assert!(
        bob.call_signal(Request {
            generation: 1,
            action: key(1, offer.clone())
        })
        .await
        .is_err()
    );

    request(&bob, 1, Action::Close).await;
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
