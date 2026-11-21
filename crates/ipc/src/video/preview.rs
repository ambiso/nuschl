// SPDX-License-Identifier: EUPL-1.2

use super::DecodedFrame;
#[cfg(target_os = "linux")]
use super::{CaptureLimits, HEADER_BYTES, Source};
use super::{Header, Kind, invalid};
#[cfg(target_os = "linux")]
use crate::crypto::descriptor::{connected_stream, receive_first, send_first};
use std::io::{self, Write};
#[cfg(target_os = "linux")]
use std::{
    io::Read,
    os::{fd::OwnedFd, unix::net::UnixStream},
};
use tokio::io::{AsyncRead, AsyncReadExt};
#[cfg(target_os = "linux")]
use tokio::{io::AsyncWriteExt, net::UnixStream as AsyncStream};

pub const PREVIEW_WIDTH: u16 = 1280;
pub const PREVIEW_HEIGHT: u16 = 720;
const DIMENSIONS_BYTES: usize = 4;

fn frame_bytes(width: u16, height: u16) -> io::Result<usize> {
    if !(1..=PREVIEW_WIDTH).contains(&width) || !(1..=PREVIEW_HEIGHT).contains(&height) {
        return Err(invalid());
    }
    Ok(usize::from(width) * usize::from(height) * 4)
}

pub fn fit_capture_preview(width: u16, height: u16) -> (u16, u16) {
    let (w, h) = (u32::from(width), u32::from(height));
    let (max_w, max_h) = (u32::from(PREVIEW_WIDTH), u32::from(PREVIEW_HEIGHT));
    if w <= max_w && h <= max_h {
        (width, height)
    } else if w * max_h > h * max_w {
        (PREVIEW_WIDTH, (h * max_w / w).max(1) as u16)
    } else {
        ((w * max_h / h).max(1) as u16, PREVIEW_HEIGHT)
    }
}

#[cfg(target_os = "linux")]
const START: Header = Header {
    kind: Kind::CaptureStart,
    key: false,
    id: 0,
    bytes: CaptureLimits::BYTES as u32,
};

#[cfg(target_os = "linux")]
pub(super) async fn send_capture_start(
    socket: &mut AsyncStream,
    limits: CaptureLimits,
    preview: Option<OwnedFd>,
) -> io::Result<()> {
    let header = START.to_bytes();
    match preview {
        Some(fd) => {
            send_first(socket, header[0], &fd).await?;
            socket.write_all(&header[1..]).await?;
        }
        None => socket.write_all(&header).await?,
    }
    socket.write_all(&limits.to_bytes()).await
}

#[cfg(target_os = "linux")]
pub fn read_capture_start(
    socket: &mut UnixStream,
    source: Source,
) -> io::Result<(CaptureLimits, Option<UnixStream>)> {
    let (first, fd) = receive_first(socket)?.ok_or_else(invalid)?;
    let mut header = [0; HEADER_BYTES];
    header[0] = first;
    socket.read_exact(&mut header[1..])?;
    if Header::from_bytes(header)? != START {
        return Err(invalid());
    }
    let mut target = [0; CaptureLimits::BYTES];
    socket.read_exact(&mut target)?;
    Ok((
        CaptureLimits::from_bytes(source, target)?,
        fd.map(connected_stream).transpose()?,
    ))
}

pub fn write_capture_preview(
    output: &mut impl Write,
    width: u16,
    height: u16,
    bgra: &[u8],
) -> io::Result<()> {
    let size = frame_bytes(width, height)?;
    if bgra.len() != size {
        return Err(invalid());
    }
    let header = Header {
        bytes: (size + DIMENSIONS_BYTES) as u32,
        ..Header::empty(Kind::CapturePreview, 0)
    };
    output.write_all(&header.to_bytes())?;
    output.write_all(&width.to_be_bytes())?;
    output.write_all(&height.to_be_bytes())?;
    output.write_all(bgra)
}

#[cfg(target_os = "linux")]
pub(super) async fn read_capture_preview(
    socket: &mut AsyncStream,
    first: u8,
) -> io::Result<DecodedFrame> {
    let mut header = [0; HEADER_BYTES];
    header[0] = first;
    socket.read_exact(&mut header[1..]).await?;
    let header = Header::from_bytes(header)?;
    read_preview_pixels(socket, header).await
}

pub(super) async fn read_preview_pixels(
    socket: &mut (impl AsyncRead + Unpin),
    header: Header,
) -> io::Result<DecodedFrame> {
    if header.kind != Kind::CapturePreview || header.id != 0 {
        return Err(invalid());
    }
    let width = socket.read_u16().await?;
    let height = socket.read_u16().await?;
    let needed = frame_bytes(width, height)?;
    if header.bytes as usize != needed + DIMENSIONS_BYTES {
        return Err(invalid());
    }
    let mut bgra = Vec::with_capacity(needed);
    let got = socket.take(needed as u64).read_to_end(&mut bgra).await?;
    if got != needed {
        return Err(io::ErrorKind::UnexpectedEof.into());
    }
    Ok(DecodedFrame {
        width,
        height,
        bgra,
    })
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;

    #[tokio::test]
    async fn preview_socket_is_passed_at_start_and_carries_bounded_pixels() -> io::Result<()> {
        let (supervisor, mut worker) = UnixStream::pair()?;
        supervisor.set_nonblocking(true)?;
        let mut supervisor = AsyncStream::from_std(supervisor)?;
        let (reader, writer) = UnixStream::pair()?;
        reader.set_nonblocking(true)?;
        let mut reader = AsyncStream::from_std(reader)?;
        let limits = CaptureLimits::for_source(Source::Screen, 2_000_000);
        send_capture_start(&mut supervisor, limits, Some(writer.into())).await?;
        let (received_limits, output) = read_capture_start(&mut worker, Source::Screen)?;
        assert_eq!(received_limits, limits);
        let mut output = output.expect("the preview descriptor is received");
        let pixels = [127; 8];
        write_capture_preview(&mut output, 2, 1, &pixels)?;
        let first = reader.read_u8().await?;
        let frame = read_capture_preview(&mut reader, first).await?;
        assert_eq!((frame.width, frame.height), (2, 1));
        assert_eq!(frame.bgra, pixels);

        let header = Header {
            bytes: u32::MAX,
            ..Header::empty(Kind::CapturePreview, 0)
        };
        output.write_all(&header.to_bytes())?;
        output.write_all(&2u16.to_be_bytes())?;
        output.write_all(&1u16.to_be_bytes())?;
        let first = reader.read_u8().await?;
        assert_eq!(
            read_capture_preview(&mut reader, first)
                .await
                .err()
                .map(|error| error.kind()),
            Some(io::ErrorKind::InvalidData)
        );
        Ok(())
    }
}
