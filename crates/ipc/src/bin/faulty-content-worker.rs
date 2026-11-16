// SPDX-License-Identifier: EUPL-1.2
//! A content worker that misbehaves on purpose, for the thumbnail supervisor's
//! tests (`tests/thumbnail.rs`), which alone start it. The image of its one
//! thumbnail request names the fault.
use base64::Engine as _;
use nuschl_core::MessageDocument;
use nuschl_ipc::{
    ContentRequest, ContentResponse, Envelope, ThumbnailPng, read_frame, write_frame,
};
use std::{
    io::{self, Write},
    time::Duration,
};

/// A 1×1 RGBA PNG as the worker makes them.
const PIXEL: &str = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR4nGM4EaDxHwAFZAJAZFKcOgAAAABJRU5ErkJggg==";

fn main() {
    let Ok(Some(request)) = read_frame::<ContentRequest>(&mut io::stdin().lock()) else {
        std::process::exit(2);
    };
    let ContentRequest::Thumbnail { image, .. } = request.payload else {
        std::process::exit(2);
    };
    let fault = base64::engine::general_purpose::STANDARD
        .decode(image)
        .unwrap_or_default();
    let mut output = io::stdout().lock();
    let mut send = |id, response| write_frame(&mut output, &Envelope::new(id, response)).unwrap();
    let thumbnail = |width| {
        ContentResponse::Thumbnail(ThumbnailPng {
            width,
            height: 1,
            png: PIXEL.into(),
        })
    };
    match &fault[..] {
        b"valid" => send(1, thumbnail(1)),
        b"wrong id" => send(2, thumbnail(1)),
        b"twice" => {
            send(1, thumbnail(1));
            send(1, thumbnail(1));
        }
        b"wrong size" => send(1, thumbnail(2)),
        b"not a png" => send(
            1,
            ContentResponse::Thumbnail(ThumbnailPng {
                width: 1,
                height: 1,
                png: "AQIDBA==".into(),
            }),
        ),
        b"wrong kind" => send(
            1,
            ContentResponse::Rendered(MessageDocument { blocks: Vec::new() }),
        ),
        b"huge" => {
            let size = nuschl_ipc::MAX_FRAME_BYTES as u32;
            output.write_all(&size.to_be_bytes()).unwrap();
        }
        b"crash" => std::process::exit(1),
        b"linger" => {
            send(1, thumbnail(1));
            std::thread::sleep(Duration::from_secs(60));
        }
        _ => std::thread::sleep(Duration::from_secs(60)),
    }
}
