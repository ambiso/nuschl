// SPDX-License-Identifier: EUPL-1.2

use cameras::{Frame, PixelFormat, Resolution, StreamConfig};
use std::{io, time::Duration};

use super::Failure;

pub(super) fn run(
    mut on_frame: impl FnMut(u32, u32, usize, &[u8]) -> io::Result<()>,
) -> Result<(), Failure> {
    let devices = cameras::devices()?;
    let device = devices
        .first()
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "No camera is available."))?;
    let formats = cameras::probe(device)?.formats;
    // Media Foundation YUV frames have top-down rows, including when
    // supplied by the software color converter.
    let mut config = StreamConfig {
        resolution: Resolution {
            width: 1280,
            height: 720,
        },
        framerate: 30,
        pixel_format: PixelFormat::Nv12,
    };
    let preferred = formats
        .iter()
        .filter(|native| {
            matches!(native.pixel_format, PixelFormat::Nv12 | PixelFormat::Yuyv)
                && valid_dimensions(native.resolution.width, native.resolution.height)
        })
        .min_by_key(|native| {
            native.resolution.width.abs_diff(config.resolution.width)
                + native.resolution.height.abs_diff(config.resolution.height)
        });
    if let Some(native) = preferred {
        config.resolution = native.resolution;
        config.pixel_format = native.pixel_format;
    }
    let camera = cameras::open(device, config)?;
    loop {
        let frame = cameras::next_frame(&camera, Duration::from_secs(5))?;
        if !valid_rows(&frame) {
            return Err(io::Error::other("The camera returned incomplete pixel rows.").into());
        }
        let mut pixels = cameras::to_rgba8(&frame)?;
        for pixel in pixels.chunks_exact_mut(4) {
            pixel.swap(0, 2);
        }
        on_frame(
            frame.width,
            frame.height,
            usize::try_from(frame.width)? * 4,
            &pixels,
        )?;
    }
}

fn valid_rows(frame: &Frame) -> bool {
    if !valid_dimensions(frame.width, frame.height) {
        return false;
    }
    let (width, height) = (frame.width as usize, frame.height as usize);
    let stride = frame.stride as usize;
    let contains = |plane: &[u8], rows: usize, row_bytes: usize| {
        stride >= row_bytes
            && stride
                .checked_mul(rows - 1)
                .and_then(|start| start.checked_add(row_bytes))
                .is_some_and(|end| end <= plane.len())
    };
    match frame.pixel_format {
        PixelFormat::Nv12 => {
            contains(&frame.plane_primary, height, width)
                && contains(&frame.plane_secondary, height / 2, width)
        }
        PixelFormat::Yuyv => contains(&frame.plane_primary, height, width * 2),
        _ => false,
    }
}

fn valid_dimensions(width: u32, height: u32) -> bool {
    (16..=1080).contains(&width.min(height))
        && width.max(height) <= 1920
        && width.is_multiple_of(2)
        && height.is_multiple_of(2)
}
