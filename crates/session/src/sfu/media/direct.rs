// SPDX-License-Identifier: EUPL-1.2
//! One optional native peer leg. The SFU transports sealed signaling.

use super::{
    Command, Connection, Description, Error, Instant, Leg, LegRole, Purpose, Track, Worker,
    describe, done,
};
use nuschl_ipc::{crypto::call::Action, rtc::Codec};
use serde::{Deserialize, Serialize};
use std::{
    net::{IpAddr, SocketAddr},
    time::Duration,
};
use tokio::net::UdpSocket;

const SETUP: Duration = Duration::from_secs(10);
const COOLDOWN: Duration = Duration::from_secs(15);
const SAMPLE_AGE: Duration = Duration::from_secs(5);

#[derive(Serialize, Deserialize)]
#[serde(tag = "native_direct", rename_all = "snake_case", deny_unknown_fields)]
pub(super) enum Signal {
    Ready,
    Offer {
        endpoint: SocketAddr,
        description: Description,
    },
    Answer {
        endpoint: SocketAddr,
        description: Description,
    },
    Connected {
        id: [u8; 16],
    },
    Close {
        id: [u8; 16],
    },
}

pub(super) struct Direct {
    interface: IpAddr,
    next_leg: u32,
    pub(super) ready: bool,
    pub(super) asked: bool,
    pub(super) leg: Option<Attempt>,
    retry: Instant,
}

pub(super) struct Attempt {
    pub(super) transport: Leg,
    pub(super) id: [u8; 16],
    deadline: Instant,
    tracks: Vec<Track>,
    pub(super) answered: bool,
    pub(super) confirmed: bool,
}

impl Direct {
    pub(super) fn new(interface: IpAddr) -> Self {
        Self {
            interface,
            next_leg: 3,
            ready: false,
            asked: false,
            leg: None,
            retry: Instant::now(),
        }
    }

    pub(super) fn can_offer(&self) -> bool {
        self.ready && self.leg.is_none() && Instant::now() >= self.retry
    }

    pub(super) fn can_accept(&self) -> bool {
        self.asked && self.leg.is_none() && Instant::now() >= self.retry
    }

    pub(super) fn deadline(&self) -> Option<Instant> {
        self.leg
            .as_ref()
            .filter(|leg| !leg.transport.selected)
            .map(|leg| leg.deadline)
    }

    async fn open(&mut self, worker: &mut Worker, codec: Codec) -> Result<Leg, Error> {
        let socket = UdpSocket::bind(SocketAddr::new(self.interface, 0)).await?;
        let id = self.next_leg;
        self.next_leg = id
            .checked_add(1)
            .ok_or(Error::Protocol("direct leg limit"))?;
        Leg::start(
            worker,
            id,
            super::NetworkLeg::host(socket)?,
            codec,
            LegRole::Direct,
        )
        .await
    }

    pub(super) async fn offer(
        &mut self,
        worker: &mut Worker,
        codec: Codec,
        purpose: Purpose,
    ) -> Result<Signal, Error> {
        let transport = self.open(worker, codec).await?;
        let id = *uuid::Uuid::new_v4().as_bytes();
        let endpoint = transport.network.address();
        let leg = transport.id;
        self.leg = Some(Attempt {
            transport,
            id,
            deadline: Instant::now() + SETUP,
            tracks: Vec::new(),
            answered: false,
            confirmed: false,
        });
        let description = describe(
            worker,
            Command::Offer {
                leg,
                id,
                purposes: vec![purpose],
            },
        )
        .await?;
        if let Some(attempt) = &mut self.leg {
            attempt.tracks = description.tracks.clone();
        }
        Ok(Signal::Offer {
            endpoint,
            description,
        })
    }

    pub(super) async fn accept(
        &mut self,
        worker: &mut Worker,
        codec: Codec,
        peer: u64,
        endpoint: SocketAddr,
        mut description: Description,
        subscribed: &[Track],
    ) -> Result<Signal, Error> {
        let [track] = description.tracks.as_mut_slice() else {
            return Err(Error::Protocol("invalid native direct track"));
        };
        if track.publisher.is_some()
            || !subscribed
                .iter()
                .any(|known| known.publisher == Some(peer) && known.purpose == track.purpose)
            || !allowed(self.interface, endpoint)
        {
            return Err(Error::Protocol("invalid native direct offer"));
        }
        track.publisher = Some(peer);
        let id = description.id;
        let transport = self.open(worker, codec).await?;
        let local = transport.network.address();
        let leg = transport.id;
        self.leg = Some(Attempt {
            transport,
            id,
            deadline: Instant::now() + SETUP,
            tracks: Vec::new(),
            answered: false,
            confirmed: false,
        });
        let answer = describe(worker, Command::AcceptOffer { leg, description }).await?;
        if let Some(attempt) = &mut self.leg {
            attempt.connect(endpoint).await?;
        }
        Ok(Signal::Answer {
            endpoint: local,
            description: answer,
        })
    }

    pub(super) async fn answer(
        &mut self,
        worker: &mut Worker,
        peer: u64,
        endpoint: SocketAddr,
        mut description: Description,
    ) -> Result<(), Error> {
        let Some(attempt) = self
            .leg
            .as_mut()
            .filter(|leg| leg.id == description.id && !leg.answered)
        else {
            return Ok(());
        };
        if description.tracks != attempt.tracks || !allowed(self.interface, endpoint) {
            return Err(Error::Protocol("invalid native direct answer"));
        }
        for track in &mut description.tracks {
            track.publisher = Some(peer);
        }
        done(
            worker,
            Command::AcceptAnswer {
                leg: attempt.transport.id,
                description,
            },
        )
        .await?;
        attempt.connect(endpoint).await
    }

    pub(super) async fn close(
        &mut self,
        worker: &mut Worker,
        publish: Option<&mut Leg>,
    ) -> Result<Option<[u8; 16]>, Error> {
        self.retry = Instant::now() + COOLDOWN;
        let Some(attempt) = self.leg.take() else {
            return Ok(None);
        };
        if let Some(publish) = publish.filter(|_| attempt.transport.selected) {
            // Switching retires the direct send leg in the worker.
            done(worker, Command::SetSendLeg { leg: publish.id }).await?;
            publish.wake = Some(Instant::now());
        } else {
            done(
                worker,
                Command::CloseLeg {
                    leg: attempt.transport.id,
                },
            )
            .await?;
        }
        Ok(Some(attempt.id))
    }
}

impl Attempt {
    async fn connect(&mut self, endpoint: SocketAddr) -> Result<(), Error> {
        self.transport.network.connect(endpoint).await?;
        self.transport.wake = Some(Instant::now());
        self.answered = true;
        Ok(())
    }
}

pub(super) async fn send(
    connection: &mut Connection,
    peer: u64,
    signal: &Signal,
) -> Result<(), Error> {
    let data = serde_json::to_vec(signal).map_err(|_| Error::Protocol("invalid direct signal"))?;
    connection.signal(Action::Seal { peer, data }).await?;
    Ok(())
}

pub(super) fn enough(sample: Option<(Instant, u64)>, desired: u64) -> bool {
    sample.is_some_and(|(observed, rate)| {
        observed.elapsed() <= SAMPLE_AGE && rate.saturating_mul(4) / 5 >= desired
    })
}

fn allowed(interface: IpAddr, endpoint: SocketAddr) -> bool {
    if endpoint.port() < 1024
        || endpoint.ip().is_loopback() != interface.is_loopback()
        || endpoint.ip() == interface && !interface.is_loopback()
    {
        return false;
    }
    match (interface, endpoint) {
        (IpAddr::V4(_), SocketAddr::V4(remote)) => {
            let ip = remote.ip();
            !ip.is_unspecified() && !ip.is_broadcast() && !ip.is_multicast() && !ip.is_link_local()
        }
        (IpAddr::V6(_), SocketAddr::V6(remote)) => {
            let ip = remote.ip();
            !ip.is_unspecified()
                && !ip.is_multicast()
                && !ip.is_unicast_link_local()
                && ip.to_ipv4_mapped().is_none()
                && remote.scope_id() == 0
        }
        _ => false,
    }
}
