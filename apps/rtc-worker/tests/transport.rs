// SPDX-License-Identifier: EUPL-1.2
#![cfg(target_os = "linux")]

use nuschl_ipc::{
    crypto::epoch::{self, Direction, Update},
    rtc::{Codec, Command, Description, Event, LegRole, MAX_DATAGRAM, Purpose, Reply, Worker},
};
use std::{os::unix::net::UnixStream, path::PathBuf, time::Duration};
use tokio::{
    net::UdpSocket,
    time::{sleep, timeout},
};

const VIDEO: &[u8] = include_bytes!("fixtures/screen.av1");
const GENERATION: u64 = 12;

async fn command(worker: &mut Worker, cmd: Command) -> Reply {
    worker.exchange(cmd, Vec::new()).await.unwrap().kind
}

fn description(reply: Reply, publisher: u64) -> Description {
    let Reply::Description {
        mut description, ..
    } = reply
    else {
        panic!("expected a track description");
    };
    for track in &mut description.tracks {
        track.publisher = Some(publisher);
    }
    description
}

struct Leg {
    id: u32,
    sockets: [UdpSocket; 2],
    connected: [bool; 2],
}

impl Leg {
    async fn open(id: u32, workers: &mut [Worker; 2]) -> Self {
        let sockets = [
            UdpSocket::bind("127.0.0.1:0").await.unwrap(),
            UdpSocket::bind("127.0.0.1:0").await.unwrap(),
        ];
        for (worker, socket) in workers.iter_mut().zip(&sockets) {
            assert!(matches!(
                command(
                    worker,
                    Command::OpenLeg {
                        relayed: false,
                        leg: id,
                        local: socket.local_addr().unwrap(),
                        codec: Codec::Av1,
                        role: LegRole::Direct,
                    }
                )
                .await,
                Reply::Done
            ));
        }
        let offer = description(
            command(
                &mut workers[0],
                Command::Offer {
                    leg: id,
                    id: [id as u8; 16],
                    purposes: vec![Purpose::ScreenVideo],
                },
            )
            .await,
            1,
        );
        let answer = description(
            command(
                &mut workers[1],
                Command::AcceptOffer {
                    leg: id,
                    description: offer,
                },
            )
            .await,
            2,
        );
        assert!(matches!(
            command(
                &mut workers[0],
                Command::AcceptAnswer {
                    leg: id,
                    description: answer
                }
            )
            .await,
            Reply::Done
        ));

        let mut leg = Self {
            id,
            sockets,
            connected: [false; 2],
        };
        timeout(Duration::from_secs(5), async {
            while leg.connected != [true; 2] {
                assert!(leg.pump(workers).await.is_none());
                sleep(Duration::from_millis(1)).await;
            }
        })
        .await
        .expect("two connected ICE legs");
        leg
    }

    async fn pump(&mut self, workers: &mut [Worker; 2]) -> Option<(Event, Vec<u8>)> {
        for (index, worker) in workers.iter_mut().enumerate() {
            let other = self.sockets[1 - index].local_addr().unwrap();
            let socket = &self.sockets[index];
            let mut buffer = [0; MAX_DATAGRAM + 1];
            for _ in 0..32 {
                match socket.try_recv_from(&mut buffer) {
                    Ok((length, source)) => {
                        assert_eq!(source, other);
                        assert!(matches!(
                            worker
                                .exchange(
                                    Command::Datagram {
                                        leg: self.id,
                                        source
                                    },
                                    buffer[..length].to_vec()
                                )
                                .await
                                .unwrap()
                                .kind,
                            Reply::Done
                        ));
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => break,
                    Err(error) => panic!("{error}"),
                }
            }
            for _ in 0..32 {
                let packet = worker
                    .exchange(Command::Poll { leg: self.id }, Vec::new())
                    .await
                    .unwrap();
                match packet.kind {
                    Reply::Wake { .. } => break,
                    Reply::Datagram { destination, .. } => {
                        assert_eq!(destination, other);
                        socket.send_to(&packet.data, other).await.unwrap();
                    }
                    Reply::Event {
                        event: Event::Connected,
                        ..
                    } => self.connected[index] = true,
                    Reply::Event {
                        event: event @ Event::Frame { .. },
                        ..
                    } => {
                        assert_eq!(index, 1);
                        return Some((event, packet.data));
                    }
                    Reply::Event { .. } => {}
                    _ => panic!("unexpected RTC poll reply"),
                }
            }
        }
        None
    }

    async fn frame(&mut self, workers: &mut [Worker; 2]) -> (Event, Vec<u8>) {
        timeout(Duration::from_secs(5), async {
            loop {
                if let Some(frame) = self.pump(workers).await {
                    return frame;
                }
                sleep(Duration::from_millis(1)).await;
            }
        })
        .await
        .expect("decrypted AV1 frame")
    }
}

async fn start(executable: &std::path::Path) -> (Worker, UnixStream) {
    let (tx, rx) = UnixStream::pair().unwrap();
    let worker = Worker::start(executable.into(), GENERATION, rx.into())
        .await
        .unwrap();
    (worker, tx)
}

#[tokio::test]
async fn confined_workers_switch_udp_legs_without_reusing_the_sframe_counter() {
    nuschl_testkit::sandbox();
    let executable = PathBuf::from(env!("CARGO_BIN_EXE_nuschl-rtc-worker"));
    let (left, mut left_keys) = start(&executable).await;
    let (right, mut right_keys) = start(&executable).await;
    let mut workers = [left, right];
    let mut initial = Leg::open(1, &mut workers).await;
    assert!(matches!(
        command(&mut workers[0], Command::SetSendLeg { leg: 1 }).await,
        Reply::Rejected
    ));
    for (channel, peer, direction) in [
        (&mut left_keys, 2, Direction::Send),
        (&mut right_keys, 1, Direction::Receive),
    ] {
        epoch::write(
            channel,
            &Update {
                generation: GENERATION,
                peer,
                direction,
                epoch: 1,
                key: [37; 32],
            },
        )
        .unwrap();
    }
    timeout(Duration::from_secs(1), async {
        while !matches!(
            command(&mut workers[0], Command::SetSendLeg { leg: 1 }).await,
            Reply::Done
        ) {
            sleep(Duration::from_millis(1)).await;
        }
    })
    .await
    .unwrap();

    let write = |timestamp, key_frame| Command::Write {
        purpose: Purpose::ScreenVideo,
        timestamp,
        key_frame,
    };
    assert!(matches!(
        workers[0]
            .exchange(write(9000, false), VIDEO.to_vec())
            .await
            .unwrap()
            .kind,
        Reply::Rejected
    ));
    assert!(matches!(
        workers[0]
            .exchange(write(9000, true), VIDEO.to_vec())
            .await
            .unwrap()
            .kind,
        Reply::Done
    ));
    let frame = initial.frame(&mut workers).await;
    assert!(matches!(
        frame.0,
        Event::Frame {
            publisher: 1,
            purpose: Purpose::ScreenVideo,
            timestamp: 9000,
            key_frame: true,
            ..
        }
    ));
    assert_eq!(frame.1, VIDEO);

    let mut replacement = Leg::open(2, &mut workers).await;
    assert!(matches!(
        command(&mut workers[0], Command::SetSendLeg { leg: 2 }).await,
        Reply::Done
    ));
    assert!(matches!(
        command(&mut workers[0], Command::Poll { leg: 1 }).await,
        Reply::Rejected
    ));
    assert!(matches!(
        workers[0]
            .exchange(write(18000, true), VIDEO.to_vec())
            .await
            .unwrap()
            .kind,
        Reply::Done
    ));
    let frame = replacement.frame(&mut workers).await;
    assert!(matches!(
        frame.0,
        Event::Frame {
            publisher: 1,
            purpose: Purpose::ScreenVideo,
            timestamp: 18000,
            key_frame: true,
            ..
        }
    ));
    assert_eq!(frame.1, VIDEO);

    let [left, right] = workers;
    right.stop().await.unwrap();
    drop(right_keys);
    // Revocation retires the worker while the normal control stream stays open.
    drop(left_keys);
    nuschl_testkit::none_running(&executable, Duration::from_secs(3)).await;
    left.stop().await.unwrap();
}
