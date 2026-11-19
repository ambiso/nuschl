// SPDX-License-Identifier: EUPL-1.2
//! UDP paths restricted to one authenticated SFU media endpoint.

use nuschl_ipc::rtc::MAX_DATAGRAM;
use nuschl_protocol::sfu::TurnCredentials;
use std::{
    io,
    net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr},
    time::Duration,
};
use tokio::{
    net::{UdpSocket, lookup_host},
    time::{Instant, sleep, timeout},
};
use turn_client_proto::{
    api::{Data, Transmit, TurnConfig, TurnEvent, TurnPollRet, TurnRecvRet},
    prelude::{DelayedTransmitBuild, TurnClientApi},
    stun::Instant as TurnInstant,
    types::{AddressFamily, TransportType, TurnCredentials as Credentials},
    udp::TurnClientUdp,
};

const UDP: TransportType = TransportType::Udp;
const SETUP_TIMEOUT: Duration = Duration::from_secs(10);
const ENDPOINT_TIMEOUT: Duration = Duration::from_secs(3);
const TURN_WIRE: usize = MAX_DATAGRAM + 64;

pub(super) struct NetworkLeg {
    socket: UdpSocket,
    address: SocketAddr,
    relay: Option<Allocation>,
}

impl NetworkLeg {
    pub(super) async fn open(peer: SocketAddr, turn: Option<&TurnCredentials>) -> io::Result<Self> {
        let Some(turn) = turn else {
            return Self::host(connected(peer).await?);
        };
        if !(1..=128).contains(&turn.username.len())
            || !(1..=128).contains(&turn.credential.len())
            || !(1..=6).contains(&turn.urls.len())
        {
            return Err(failure("invalid TURN configuration"));
        }
        timeout(SETUP_TIMEOUT, async {
            for (host, port) in turn.urls.iter().filter_map(|url| turn_address(url)) {
                let Ok(addrs) = lookup_host((host, port)).await else {
                    continue;
                };
                for server in addrs.filter(|address| valid_address(*address)).take(4) {
                    if let Ok(Ok(leg)) =
                        timeout(ENDPOINT_TIMEOUT, Self::allocate(peer, server, turn)).await
                    {
                        return Ok(leg);
                    }
                }
            }
            Err(failure(
                "no configured UDP TURN relay could allocate this call",
            ))
        })
        .await
        .map_err(|_| failure("TURN setup timed out"))?
    }

    async fn allocate(
        peer: SocketAddr,
        server: SocketAddr,
        turn: &TurnCredentials,
    ) -> io::Result<Self> {
        let socket = connected(server).await?;
        let mut config = TurnConfig::new(Credentials::new(&turn.username, &turn.credential));
        if peer.is_ipv6() {
            config.set_address_family(AddressFamily::IPV6);
        }
        let local = socket.local_addr()?;
        let mut path = Self {
            address: local,
            relay: Some(Allocation {
                peer,
                started: Instant::now(),
                client: TurnClientUdp::allocate(local, server, config),
                pending: None,
            }),
            socket,
        };
        let relay = path
            .relay
            .as_mut()
            .ok_or_else(|| failure("missing TURN allocation"))?;
        path.address = loop {
            let wait = relay.progress(&path.socket).await?;
            if let Some(event) = relay.client.poll_event() {
                match event {
                    TurnEvent::AllocationCreated(UDP, address) if valid_address(address) => {
                        relay
                            .client
                            .bind_channel(UDP, peer, relay.now())
                            .map_err(|_| failure("TURN channel refused"))?;
                    }
                    TurnEvent::ChannelCreated(UDP, address) if address == peer => {
                        break relay
                            .client
                            .relayed_addresses()
                            .next()
                            .ok_or_else(|| failure("TURN allocation disappeared"))?
                            .1;
                    }
                    TurnEvent::PermissionCreated(UDP, ip) if ip == peer.ip() => {}
                    _ => return Err(failure("TURN setup refused")),
                }
                continue;
            }
            relay.receive(&path.socket, &mut [], wait).await?;
        };
        Ok(path)
    }

    fn host(socket: UdpSocket) -> io::Result<Self> {
        Ok(Self {
            address: socket.local_addr()?,
            socket,
            relay: None,
        })
    }

    pub(super) fn peer(&self) -> io::Result<SocketAddr> {
        match &self.relay {
            Some(relay) => Ok(relay.peer),
            None => self.socket.peer_addr(),
        }
    }
    pub(super) fn address(&self) -> SocketAddr {
        self.address
    }
    pub(super) fn is_relayed(&self) -> bool {
        self.relay.is_some()
    }

    pub(super) async fn recv(&mut self, output: &mut [u8]) -> io::Result<usize> {
        let Some(relay) = &mut self.relay else {
            return self.socket.recv(output).await;
        };
        loop {
            let wait = relay.progress(&self.socket).await?;
            while let Some(event) = relay.client.poll_event() {
                if matches!(
                    event,
                    TurnEvent::AllocationCreateFailed(_)
                        | TurnEvent::PermissionCreateFailed(..)
                        | TurnEvent::ChannelCreateFailed(..)
                ) {
                    return Err(failure("TURN allocation expired"));
                }
            }
            if let Some(size) = relay.receive(&self.socket, output, wait).await? {
                return Ok(size);
            }
        }
    }

    pub(super) async fn send(&mut self, data: &[u8]) -> io::Result<()> {
        if !(1..=MAX_DATAGRAM).contains(&data.len()) {
            return Err(failure("invalid SFU datagram size"));
        }
        if let Some(relay) = &mut self.relay {
            let mut packet = [0; TURN_WIRE];
            let transmit = relay
                .client
                .send_to(UDP, relay.peer, data, relay.now())
                .map_err(|_| failure("TURN permission expired"))?
                .filter(|packet| {
                    packet.to == relay.client.remote_addr() && packet.data.len() <= TURN_WIRE
                })
                .ok_or_else(|| failure("invalid TURN transmit"))?;
            self.socket
                .send(transmit.write_into(&mut packet).data)
                .await?;
        } else {
            self.socket.send(data).await?;
        }
        Ok(())
    }
}

impl Drop for NetworkLeg {
    fn drop(&mut self) {
        if let Some(relay) = &mut self.relay {
            let now = relay.now();
            if relay.client.delete(now).is_ok()
                && let Some(packet) = relay.client.poll_transmit(now)
                && packet.to == relay.client.remote_addr()
            {
                let _ = self.socket.try_send(packet.data.as_ref());
            }
        }
    }
}

struct Allocation {
    peer: SocketAddr,
    started: Instant,
    client: TurnClientUdp,
    // A socket write may wait across a cancelled recv future.
    pending: Option<Transmit<Data<'static>>>,
}

impl Allocation {
    fn now(&self) -> TurnInstant {
        TurnInstant::from_nanos(
            self.started
                .elapsed()
                .as_nanos()
                .try_into()
                .unwrap_or(i64::MAX),
        )
    }

    async fn progress(&mut self, socket: &UdpSocket) -> io::Result<Duration> {
        let now = self.now();
        for _ in 0..8 {
            if self.pending.is_none() {
                self.pending = self.client.poll_transmit(now);
            }
            let Some(packet) = &self.pending else { break };
            if packet.to != self.client.remote_addr() || packet.data.as_ref().len() > TURN_WIRE {
                return Err(failure("invalid TURN control transmit"));
            }
            socket.send(packet.data.as_ref()).await?;
            self.pending = None;
        }
        match self.client.poll(now) {
            TurnPollRet::WaitUntil(deadline) => Ok(deadline.duration_since(now)),
            _ => Err(failure("TURN allocation closed")),
        }
    }

    async fn receive(
        &mut self,
        socket: &UdpSocket,
        output: &mut [u8],
        wait: Duration,
    ) -> io::Result<Option<usize>> {
        let mut packet = [0; TURN_WIRE + 1];
        let len = tokio::select! {
            result = socket.recv(&mut packet) => result?,
            () = sleep(wait.max(Duration::from_millis(1))) => return Ok(None),
        };
        if len > TURN_WIRE {
            return Ok(None);
        }
        let datagram = Transmit::new(
            &packet[..len],
            UDP,
            self.client.remote_addr(),
            self.client.local_addr(),
        );
        if let TurnRecvRet::PeerData(data) = self.client.recv(datagram, self.now())
            && data.transport == UDP
            && data.peer == self.peer
            && (1..=MAX_DATAGRAM.min(output.len())).contains(&data.data().len())
        {
            let data = data.data();
            output[..data.len()].copy_from_slice(data);
            return Ok(Some(data.len()));
        }
        Ok(None)
    }
}

async fn connected(peer: SocketAddr) -> io::Result<UdpSocket> {
    let address = match peer {
        SocketAddr::V4(_) => IpAddr::V4(Ipv4Addr::UNSPECIFIED),
        SocketAddr::V6(_) => IpAddr::V6(Ipv6Addr::UNSPECIFIED),
    };
    let socket = UdpSocket::bind((address, 0)).await?;
    socket.connect(peer).await?;
    Ok(socket)
}

fn valid_address(address: SocketAddr) -> bool {
    let ip = address.ip();
    address.port() != 0 && !ip.is_unspecified() && !ip.is_multicast() && ip != Ipv4Addr::BROADCAST
}

pub(super) fn turn_address(url: &str) -> Option<(&str, u16)> {
    if url.len() > 272 {
        return None;
    }
    let address = url.strip_prefix("turn:")?;
    let address = match address.split_once('?') {
        Some((address, transport)) if transport.eq_ignore_ascii_case("transport=udp") => address,
        Some(_) => return None,
        None => address,
    };
    let (host, port) = if let Some(v6) = address.strip_prefix('[') {
        let (host, rest) = v6.split_once(']')?;
        host.parse::<Ipv6Addr>().ok()?;
        (
            host,
            if rest.is_empty() {
                3478
            } else {
                rest.strip_prefix(':')?.parse().ok()?
            },
        )
    } else {
        let (host, port) = match address.split_once(':') {
            Some((host, port)) => (host, port.parse().ok()?),
            None => (address, 3478),
        };
        if host.is_empty()
            || !host
                .bytes()
                .all(|c| c.is_ascii_alphanumeric() || b".-".contains(&c))
        {
            return None;
        }
        (host, port)
    };
    (port != 0).then_some((host, port))
}

fn failure(message: &'static str) -> io::Error {
    io::Error::other(message)
}
