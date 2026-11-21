// SPDX-License-Identifier: EUPL-1.2
//! Encode a synthetic test pattern through the confined video worker.

#[cfg(target_os = "linux")]
#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    use nuschl_ipc::video::{Encoder, Output, Settings, Source};
    use std::io::{self, Write as _};

    let mut args = std::env::args_os().skip(1).peekable();
    let source = match args.peek().and_then(|arg| arg.to_str()) {
        Some("--screen") => Some(Source::Screen),
        Some("--camera") => Some(Source::Camera),
        _ => None,
    };
    if let Some(source) = source {
        args.next();
        if args.next().is_some() {
            return Err("usage: nuschl-video-probe [--obu | --screen | --camera]".into());
        }
        return portal_probe(source).await;
    }
    let write_obu = match (args.next(), args.next()) {
        (None, None) => false,
        (Some(option), None) if option == "--obu" => true,
        _ => return Err("usage: nuschl-video-probe [--obu | --screen | --camera]".into()),
    };
    let settings = Settings {
        source: Source::Screen,
        width: 160,
        height: 120,
        fps: 30,
        bitrate: 1_000_000,
    };
    let mut encoder = Encoder::start(nuschl_sandbox::sibling("nuschl-video-worker")?, settings)?;
    let mut stdout = io::stdout().lock();
    let mut bytes = 0;
    for n in 0..3 {
        let mut pixels = vec![128; settings.frame_bytes()?];
        let width = usize::from(settings.width);
        let luma = width * usize::from(settings.height);
        for (offset, y) in pixels[..luma].iter_mut().enumerate() {
            *y = if (offset / width / 16 + (offset % width + n * 8) / 16).is_multiple_of(2) {
                48
            } else {
                208
            };
        }
        let Output::Keyframe(packet) = encoder.encode(pixels).await? else {
            return Err("the test pattern exceeded the keyframe budget".into());
        };
        bytes += packet.len();
        if write_obu {
            stdout.write_all(&packet)?;
        }
    }
    encoder.stop().await?;
    if !write_obu {
        writeln!(
            stdout,
            "Encoded 3 synthetic 160×120 AV1 frames ({bytes} bytes)"
        )?;
    }
    Ok(())
}

#[cfg(target_os = "linux")]
async fn portal_probe(source: nuschl_ipc::video::Source) -> Result<(), Box<dyn std::error::Error>> {
    let grant = nuschl_ipc::video::portal::request(source, None).await?;
    let mut capture = grant.start(nuschl_sandbox::sibling("nuschl-video-worker")?, 4_000_000)?;
    let result = tokio::time::timeout(std::time::Duration::from_secs(20), capture.next()).await;
    let stopped = tokio::time::timeout(std::time::Duration::from_secs(2), capture.stop()).await;
    let frame = result??;
    stopped??;
    println!(
        "Captured one {}×{} AV1 frame ({} bytes)",
        frame.width,
        frame.height,
        frame.av1.len()
    );
    Ok(())
}

#[cfg(not(target_os = "linux"))]
fn main() -> Result<(), &'static str> {
    Err("the video probe requires the Linux sandbox")
}
