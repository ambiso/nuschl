// SPDX-License-Identifier: EUPL-1.2

use super::*;

#[tokio::test]
async fn native_viewer_moves_to_direct_and_recovers_through_the_sfu() {
    let _media = MEDIA_TEST.lock().await;
    nuschl_testkit::sandbox();
    let crypto = fixture("nuschl-crypto-worker");
    let rtc = fixture("nuschl-rtc-worker");
    let sfu = nuschl_testkit::sfu::start(fixture("nuschl-stream-sfu"), "");
    let (host, roots) = tls_proxy(&sfu).await;
    let directory = nuschl_testkit::temp_dir();
    let posted = format!("https://{host}/call#{LINK}");
    let owner = format!("{posted}&p={OWNER}");
    let connect = |link| Connect {
        generation: 1,
        host: &host,
        link,
        codecs: &[Codec::Av1],
        policy: PeerPolicy::default(),
    };
    let make_media = |publish| MediaOptions {
        executable: rtc.clone(),
        sfu_port: sfu.media.port(),
        publish,
    };
    let mut publisher = Connection::connect_with_roots(
        CryptoHandle::start(crypto.clone(), directory.path().join("publisher")),
        connect(&owner),
        roots.clone(),
    )
    .await
    .unwrap();
    let from = publisher.welcome().peer;
    // The host path uses only loopback sockets. The STUN test owns its server.
    publisher.welcome.turn = None;
    let mut publisher = publisher
        .into_media(make_media(Some(Purpose::Camera)))
        .await
        .unwrap();
    let mut viewer = Connection::connect_with_roots(
        CryptoHandle::start(crypto, directory.path().join("viewer")),
        connect(&posted),
        roots,
    )
    .await
    .unwrap();
    // Version 1, the first camera and screen tier. A modest upload can
    // qualify without a high bitrate or long-running test stream.
    write(
        &mut viewer.socket,
        &ClientMessage::Capacity {
            decode: "010000".into(),
        },
    )
    .await
    .unwrap();
    viewer.welcome.turn = None;
    let mut viewer = viewer.into_media(make_media(None)).await.unwrap();

    // An independent synthetic AV1 frame plus a 4 KiB AV1 padding OBU.
    let mut picture = vec![
        0x12, 0, 0x0a, 0x0a, 0, 0, 0, 2, 0x27, 0xfe, 0x6d, 0x7c, 0x80, 0x20, 0x32, 0x0e, 0x10, 0,
        0x8c, 0, 0, 2, 0, 0, 0, 0, 0x3e, 0xfc, 0xc1, 8, 0x7a, 0x80, 0x20,
    ];
    picture.resize(picture.len() + 4096, 0);
    let mut ready = false;
    let mut over_sfu = false;
    let mut sender_direct = false;
    let mut receiver_direct = false;
    let mut outage = None;
    let mut sender_relayed = false;
    let mut receiver_relayed = false;
    let mut resumed = false;
    let mut timestamp = 0;
    let mut tick = tokio::time::interval(Duration::from_millis(33));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    timeout(Duration::from_secs(20), async {
        loop {
            tokio::select! {
                _ = tick.tick() => {
                    if ready {
                        timestamp += 2970;
                        publisher.send_keyframe(timestamp, picture.clone()).unwrap();
                    }
                }
                event = publisher.next() => match event.unwrap().unwrap() {
                    MediaEvent::SendReady => ready = true,
                    MediaEvent::Direct { active: true } => sender_direct = true,
                    MediaEvent::Direct { active: false } => sender_relayed = true,
                    _ => {}
                },
                event = viewer.next() => match event.unwrap().unwrap() {
                    MediaEvent::Direct { active: true } => receiver_direct = true,
                    MediaEvent::Direct { active: false } => receiver_relayed = true,
                    MediaEvent::Frame { publisher: sender, purpose, contiguous, key_frame, data, .. } => {
                        assert_eq!(sender, from);
                        assert_eq!(purpose, Purpose::Camera);
                        assert!(key_frame);
                        assert_eq!(data, picture);
                        if receiver_relayed && !resumed {
                            assert!(!contiguous);
                            resumed = true;
                        }
                        if sender_direct && receiver_direct && outage.is_none() {
                            // Drop packets at the publisher's broker while the SFU
                            // and both real media workers continue to run.
                            publisher.lose_direct().await;
                            outage = Some(tokio::time::Instant::now());
                        }
                        if !sender_direct {
                            over_sfu = true;
                        }
                    }
                    _ => {}
                },
            }
            if resumed && sender_relayed {
                break;
            }
        }
    })
    .await
    .unwrap_or_else(|_| panic!("direct did not deliver; SFU={over_sfu}, sender={sender_direct}, receiver={receiver_direct}"));
    assert!(over_sfu);
    eprintln!(
        "SFU resumed with both endpoints reporting fallback after {:?}",
        outage.unwrap().elapsed()
    );
    publisher.stop().await.unwrap();
    viewer.stop().await.unwrap();
}
