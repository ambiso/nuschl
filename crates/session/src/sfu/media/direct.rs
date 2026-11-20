// SPDX-License-Identifier: EUPL-1.2
//! One optional native peer leg. The SFU transports sealed signaling.

use super::{
    Command, Connection, Description, Error, Instant, Leg, LegRole, Network, Purpose, Track,
    Worker, describe, done,
};
use crate::sfu::turn::turn_address;
use futures_util::{FutureExt, future::BoxFuture};
use nuschl_ipc::{crypto::call::Action, rtc::Codec};
use serde::{Deserialize, Serialize};
use std::{
    future::pending,
    net::{IpAddr, SocketAddr},
    time::Duration,
};
use tokio::{
    net::{UdpSocket, lookup_host},
    time::timeout,
};
use turn_client_proto::stun::types::{
    attribute::XorMappedAddress,
    message::{BINDING, Message, MessageClass, MessageWrite, MessageWriteVec},
};

const SETUP: Duration = Duration::from_secs(10);
const COOLDOWN: Duration = Duration::from_secs(15);
const SAMPLE_AGE: Duration = Duration::from_secs(5);
const DISCOVERY: Duration = Duration::from_millis(900);
// str0m emits ICE checks at least every three seconds on an idle live leg.
const SILENCE: Duration = Duration::from_secs(8);

#[derive(Clone, Copy, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Candidates {
    host: SocketAddr,
    reflexive: Option<SocketAddr>,
}

impl Candidates {
    pub(super) fn contains(self, address: SocketAddr) -> bool {
        address == self.host || self.reflexive == Some(address)
    }

    fn valid(self, interface: IpAddr) -> bool {
        allowed(interface, self.host)
            && self
                .reflexive
                .is_none_or(|address| address != self.host && allowed(interface, address))
    }
}

pub(super) enum Setup {
    Offer(Vec<Purpose>),
    Answer {
        description: Description,
        remote: Candidates,
    },
}

pub(super) struct Prepared {
    socket: UdpSocket,
    local: Candidates,
    setup: Setup,
}

pub(super) type Discovery = BoxFuture<'static, Result<Prepared, Error>>;

pub(super) async fn next_prepared(active: &mut Option<Discovery>) -> Result<Prepared, Error> {
    match active {
        Some(wake) => wake.await,
        None => pending().await,
    }
}

#[derive(Serialize, Deserialize)]
#[serde(tag = "native_direct", rename_all = "snake_case", deny_unknown_fields)]
pub(super) enum Signal {
    Ready,
    Offer {
        candidates: Candidates,
        description: Description,
    },
    Answer {
        candidates: Candidates,
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
    stun: Option<(String, u16)>,
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
    pub(super) fn new(interface: IpAddr, urls: &[String]) -> Self {
        Self {
            interface,
            stun: urls
                .iter()
                .take(6)
                .find_map(|url| turn_address(url))
                .map(|(host, port)| (host.to_owned(), port)),
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
        self.leg.as_ref().map(|leg| {
            if leg.transport.selected {
                leg.transport
                    .last_packet
                    .map_or(leg.deadline, |last| last + SILENCE)
            } else {
                leg.deadline
            }
        })
    }

    pub(super) fn discover(&self, setup: Setup) -> Discovery {
        let interface = self.interface;
        let stun = self.stun.clone();
        async move {
            let socket = UdpSocket::bind(SocketAddr::new(interface, 0)).await?;
            let host = socket.local_addr()?;
            let reflexive = match stun {
                Some((server, port)) => {
                    timeout(DISCOVERY, binding(&socket, &server, port, interface))
                        .await
                        .ok()
                        .flatten()
                        .filter(|&address| address != host && allowed(interface, address))
                }
                None => None,
            };
            Ok(Prepared {
                socket,
                local: Candidates { host, reflexive },
                setup,
            })
        }
        .boxed()
    }

    pub(super) fn accept(
        &self,
        peer: u64,
        candidates: Candidates,
        mut description: Description,
        subscribed: &[Track],
    ) -> Option<Discovery> {
        if !(1..=2).contains(&description.tracks.len())
            || description.tracks.iter().any(|track| {
                track.publisher.is_some()
                    || !subscribed.iter().any(|known| {
                        known.publisher == Some(peer) && known.purpose == track.purpose
                    })
            })
            || !valid_sdp(self.interface, candidates, &description.sdp)
        {
            return None;
        }
        for track in &mut description.tracks {
            track.publisher = Some(peer);
        }
        Some(self.discover(Setup::Answer {
            description,
            remote: candidates,
        }))
    }

    pub(super) async fn complete(
        &mut self,
        worker: &mut Worker,
        codec: Codec,
        Prepared {
            socket,
            mut local,
            setup,
        }: Prepared,
    ) -> Result<Signal, Error> {
        let leg = self.next_leg;
        self.next_leg = leg
            .checked_add(1)
            .ok_or(Error::Protocol("direct leg limit"))?;
        let id = match &setup {
            Setup::Offer(_) => *uuid::Uuid::new_v4().as_bytes(),
            Setup::Answer { description, .. } => description.id,
        };
        let transport = Leg::start(
            worker,
            leg,
            Network::Direct {
                socket,
                remote: None,
            },
            codec,
            LegRole::Direct,
        )
        .await?;
        let attempt = self.leg.insert(Attempt {
            transport,
            id,
            deadline: Instant::now() + SETUP,
            tracks: Vec::new(),
            answered: false,
            confirmed: false,
        });
        if let Some(address) = local.reflexive
            && !matches!(
                worker
                    .exchange(Command::AddReflexive { leg, address }, Vec::new())
                    .await?
                    .kind,
                nuschl_ipc::rtc::Reply::Done
            )
        {
            local.reflexive = None;
        }
        match setup {
            Setup::Offer(purposes) => {
                let description = describe(worker, Command::Offer { leg, id, purposes }).await?;
                attempt.tracks = description.tracks.clone();
                Ok(Signal::Offer {
                    candidates: local,
                    description,
                })
            }
            Setup::Answer {
                description,
                remote,
            } => {
                attempt.transport.set_video_tracks(&description.tracks);
                let description =
                    describe(worker, Command::AcceptOffer { leg, description }).await?;
                attempt.connect(remote);
                Ok(Signal::Answer {
                    candidates: local,
                    description,
                })
            }
        }
    }

    pub(super) async fn answer(
        &mut self,
        worker: &mut Worker,
        peer: u64,
        candidates: Candidates,
        mut description: Description,
    ) -> Result<(), Error> {
        let Some(attempt) = self
            .leg
            .as_mut()
            .filter(|leg| leg.id == description.id && !leg.answered)
        else {
            return Ok(());
        };
        if description.tracks != attempt.tracks
            || !valid_sdp(self.interface, candidates, &description.sdp)
        {
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
        attempt.connect(candidates);
        Ok(())
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
    fn connect(&mut self, candidates: Candidates) {
        if let Network::Direct { remote, .. } = &mut self.transport.network {
            *remote = Some(candidates);
        }
        self.transport.wake = Some(Instant::now());
        self.answered = true;
    }
}

pub(super) async fn send(
    connection: &mut Connection,
    peer: u64,
    publisher: bool,
    signal: &Signal,
) -> Result<(), Error> {
    let data = serde_json::to_vec(signal).map_err(|_| Error::Protocol("invalid direct signal"))?;
    connection
        .signal(Action::Seal {
            peer,
            publisher,
            data,
        })
        .await?;
    Ok(())
}

pub(super) fn enough(sample: Option<(Instant, u64)>, desired: u64) -> bool {
    sample.is_some_and(|(observed, rate)| {
        observed.elapsed() <= SAMPLE_AGE && rate.saturating_mul(4) / 5 >= desired
    })
}

async fn binding(
    socket: &UdpSocket,
    host: &str,
    port: u16,
    interface: IpAddr,
) -> Option<SocketAddr> {
    let server = lookup_host((host, port)).await.ok()?.find(|address| {
        address.is_ipv4() == interface.is_ipv4()
            && !address.ip().is_unspecified()
            && !address.ip().is_multicast()
    })?;
    let request = Message::builder_request(BINDING, MessageWriteVec::new()).finish();
    let id = Message::from_bytes(&request).ok()?.transaction_id();
    socket.send_to(&request, server).await.ok()?;
    let mut packet = [0; nuschl_ipc::rtc::MAX_DATAGRAM + 1];
    loop {
        let (size, source) = socket.recv_from(&mut packet).await.ok()?;
        if source != server || size == packet.len() {
            continue;
        }
        let Ok(reply) = Message::from_bytes(&packet[..size]) else {
            continue;
        };
        if reply.transaction_id() == id
            && reply.method() == BINDING
            && reply.class() == MessageClass::Success
        {
            return Some(reply.attribute::<XorMappedAddress>().ok()?.addr(id));
        }
    }
}

fn valid_sdp(interface: IpAddr, candidates: Candidates, sdp: &str) -> bool {
    if !candidates.valid(interface) {
        return false;
    }
    let mut host = false;
    let mut reflexive = false;
    for line in sdp
        .lines()
        .filter_map(|line| line.strip_prefix("a=candidate:"))
    {
        let mut fields = line.split_ascii_whitespace();
        if fields.nth(1) != Some("1")
            || !fields
                .next()
                .is_some_and(|transport| transport.eq_ignore_ascii_case("udp"))
        {
            return false;
        }
        let Some(ip) = fields.nth(1).and_then(|field| field.parse().ok()) else {
            return false;
        };
        let Some(port) = fields.next().and_then(|field| field.parse().ok()) else {
            return false;
        };
        if fields.next() != Some("typ") {
            return false;
        }
        let endpoint = SocketAddr::new(ip, port);
        match fields.next() {
            Some("host") if endpoint == candidates.host && !host => host = true,
            Some("srflx") if Some(endpoint) == candidates.reflexive && !reflexive => {
                reflexive = true
            }
            _ => return false,
        }
    }
    host && (candidates.reflexive.is_some() == reflexive)
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

#[cfg(test)]
mod tests {
    use super::*;
    use turn_client_proto::stun::types::message::MessageWriteExt;

    #[tokio::test]
    async fn binding_observes_the_mapping_on_the_same_socket() {
        let host = "127.0.0.1";
        let socket = UdpSocket::bind((host, 0)).await.unwrap();
        let server = UdpSocket::bind((host, 0)).await.unwrap();
        let mapped = "127.0.0.2:49000".parse().unwrap();
        let request = binding(
            &socket,
            host,
            server.local_addr().unwrap().port(),
            host.parse().unwrap(),
        );
        let respond = async {
            let mut data = [0; 256];
            let (size, source) = server.recv_from(&mut data).await.unwrap();
            assert_eq!(source, socket.local_addr().unwrap());
            let incoming = Message::from_bytes(&data[..size]).unwrap();
            assert_eq!(incoming.method(), BINDING);
            let mut response = Message::builder_success(&incoming, MessageWriteVec::new());
            response
                .add_attribute(&XorMappedAddress::new(mapped, incoming.transaction_id()))
                .unwrap();
            server.send_to(&response.finish(), source).await.unwrap();
        };
        let (address, ()) = timeout(DISCOVERY, async { tokio::join!(request, respond) })
            .await
            .unwrap();
        assert_eq!(address, Some(mapped));
    }
}
