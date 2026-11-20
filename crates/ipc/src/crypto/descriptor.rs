// SPDX-License-Identifier: EUPL-1.2
//! Attach one connected local stream to a framed crypto control request.

use crate::{Envelope, read_frame};
use rustix::net::{
    AddressFamily, RecvAncillaryBuffer, RecvAncillaryMessage, RecvFlags, ReturnFlags,
    SendAncillaryBuffer, SendAncillaryMessage, SendFlags, SocketType, getpeername, recvmsg,
    sendmsg, sockopt::socket_type,
};
use std::{
    io::{self, IoSlice, IoSliceMut, Read as _},
    mem::MaybeUninit,
    os::{
        fd::{AsFd, OwnedFd},
        unix::net::UnixStream,
    },
};
use tokio::{io::Interest, net::UnixStream as AsyncStream};

pub fn connected_stream(fd: OwnedFd) -> io::Result<UnixStream> {
    if socket_type(&fd)? != SocketType::STREAM
        || !getpeername(&fd)?.is_some_and(|address| address.address_family() == AddressFamily::UNIX)
    {
        return Err(invalid());
    }
    let socket = UnixStream::from(fd);
    if !socket.local_addr()?.is_unnamed() || !socket.peer_addr()?.is_unnamed() {
        return Err(invalid());
    }
    Ok(socket)
}

/// Carry a descriptor on the first byte of the ordinary four-byte length.
pub async fn send_first(socket: &AsyncStream, byte: u8, fd: &OwnedFd) -> io::Result<()> {
    socket
        .async_io(Interest::WRITABLE, || {
            let mut storage = [MaybeUninit::uninit(); rustix::cmsg_space!(ScmRights(1))];
            let mut control = SendAncillaryBuffer::new(&mut storage);
            let descriptors = [fd.as_fd()];
            if !control.push(SendAncillaryMessage::ScmRights(&descriptors)) {
                return Err(invalid());
            }
            match sendmsg(
                socket,
                &[IoSlice::new(&[byte])],
                &mut control,
                SendFlags::NOSIGNAL,
            ) {
                Ok(1) => Ok(()),
                Ok(_) => Err(io::ErrorKind::WriteZero.into()),
                Err(error) => Err(error.into()),
            }
        })
        .await
}

pub(crate) fn receive_first(socket: &UnixStream) -> io::Result<Option<(u8, Option<OwnedFd>)>> {
    let mut first = [0; 1];
    let mut storage = [MaybeUninit::uninit(); rustix::cmsg_space!(ScmRights(1))];
    let mut control = RecvAncillaryBuffer::new(&mut storage);
    let result = loop {
        match recvmsg(
            socket,
            &mut [IoSliceMut::new(&mut first)],
            &mut control,
            RecvFlags::CMSG_CLOEXEC,
        ) {
            Err(rustix::io::Errno::INTR) => continue,
            result => break result?,
        }
    };
    if result.flags.contains(ReturnFlags::CTRUNC) {
        return Err(invalid());
    }
    let mut endpoint = None;
    for message in control.drain() {
        let RecvAncillaryMessage::ScmRights(mut files) = message else {
            return Err(invalid());
        };
        let fd = files.next().ok_or_else(invalid)?;
        if endpoint.is_some() || files.next().is_some() {
            return Err(invalid());
        }
        endpoint = Some(fd);
    }
    Ok((result.bytes != 0).then_some((first[0], endpoint)))
}

pub fn read_request<T: serde::de::DeserializeOwned>(
    socket: &mut UnixStream,
) -> io::Result<Option<(Envelope<T>, Option<OwnedFd>)>> {
    let Some((first, endpoint)) = receive_first(socket)? else {
        return Ok(None);
    };
    let frame = read_frame(&mut [first].as_slice().chain(socket))?.ok_or_else(invalid)?;
    Ok(Some((frame, endpoint)))
}

fn invalid() -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        "invalid crypto worker descriptor",
    )
}
