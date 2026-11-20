// SPDX-License-Identifier: EUPL-1.2
#![cfg(target_os = "linux")]

use nuschl_ipc::video::{DecodedFrame, Decoder, Source};
use rav1e::prelude::*;
use std::{path::Path, process::Command, time::Duration};

const WORKER: &str = env!("CARGO_BIN_EXE_nuschl-video-worker");
const WIDTH: usize = 160;
const HEIGHT: usize = 120;
static SERIAL: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

#[tokio::test]
async fn confined_decoder_renders_key_and_delta_frames_and_recovers_after_exit() {
    let _serial = SERIAL.lock().await;
    nuschl_testkit::sandbox();
    let [key, delta] = sequence::<u8>(8);
    let path = Path::new(WORKER);
    let mut decoder = Decoder::start(path.to_owned(), Source::Camera).unwrap();

    assert!(
        decoder
            .decode(false, delta.clone())
            .await
            .unwrap()
            .is_none()
    );
    let first = decoder.decode(true, key.clone()).await.unwrap().unwrap();
    let second = decoder.decode(false, delta.clone()).await.unwrap().unwrap();
    check_pixels(&first);
    check_pixels(&second);
    assert!(first.bgra[4] + 8 < second.bgra[4]);

    decoder.reset();
    assert!(
        decoder
            .decode(false, delta.clone())
            .await
            .unwrap()
            .is_none()
    );
    check_pixels(&decoder.decode(true, key.clone()).await.unwrap().unwrap());

    let workers = nuschl_testkit::running(path);
    assert_eq!(workers.len(), 1);
    assert!(
        Command::new(nuschl_testkit::program("kill"))
            .args(["-KILL", &workers[0].to_string()])
            .status()
            .unwrap()
            .success()
    );
    assert!(decoder.decode(false, delta.clone()).await.is_err());
    assert!(decoder.decode(false, delta).await.unwrap().is_none());
    check_pixels(&decoder.decode(true, key).await.unwrap().unwrap());
    decoder.stop().await.unwrap();
    nuschl_testkit::none_running(path, Duration::from_secs(3)).await;
}

#[tokio::test]
async fn confined_decoder_scales_ten_bit_luma_and_respects_row_stride() {
    let _serial = SERIAL.lock().await;
    nuschl_testkit::sandbox();
    let [key, delta] = sequence::<u16>(10);
    let mut decoder = Decoder::start(WORKER.into(), Source::Screen).unwrap();
    let first = decoder.decode(true, key).await.unwrap().unwrap();
    let second = decoder.decode(false, delta).await.unwrap().unwrap();
    check_pixels(&first);
    check_pixels(&second);
    assert!(first.bgra[4] + 8 < second.bgra[4]);
    decoder.stop().await.unwrap();
}

#[cfg(feature = "vaapi")]
#[tokio::test]
#[ignore = "requires a host DRM render node with VA-API AV1 decode"]
async fn vaapi_decoder_renders_key_and_delta_frames() {
    let _serial = SERIAL.lock().await;
    nuschl_testkit::sandbox();
    let [key, delta] = sequence::<u8>(8);
    let path = Path::new(WORKER);
    let mut decoder = Decoder::start(path.to_owned(), Source::Screen).unwrap();
    check_pixels(&decoder.decode(true, key).await.unwrap().unwrap());
    check_pixels(&decoder.decode(false, delta).await.unwrap().unwrap());
    let workers = nuschl_testkit::running(path);
    assert_eq!(workers.len(), 1);
    let command = std::fs::read(format!("/proc/{}/cmdline", workers[0])).unwrap();
    assert!(
        command
            .split(|byte| *byte == 0)
            .any(|arg| arg == b"--decode-vaapi")
    );
    decoder.stop().await.unwrap();
    nuschl_testkit::none_running(path, Duration::from_secs(3)).await;
}

fn check_pixels(frame: &DecodedFrame) {
    assert_eq!(
        (usize::from(frame.width), usize::from(frame.height)),
        (WIDTH, HEIGHT)
    );
    assert_eq!(frame.bgra.len(), WIDTH * HEIGHT * 4);
    let top = 4;
    let bottom = (WIDTH * (HEIGHT - 4) + 1) * 4;
    assert!(frame.bgra[top] < 100);
    assert!(frame.bgra[bottom] > 170);
    assert_eq!(frame.bgra[top + 3], 255);
    assert_eq!(frame.bgra[bottom + 3], 255);
}

fn sequence<T: Pixel>(depth: usize) -> [Vec<u8>; 2] {
    let mut settings = EncoderConfig::with_speed_preset(10);
    settings.width = WIDTH;
    settings.height = HEIGHT;
    settings.bit_depth = depth;
    settings.low_latency = true;
    settings.min_key_frame_interval = 30;
    settings.max_key_frame_interval = 30;
    settings.speed_settings.rdo_lookahead_frames = 1;
    let mut encoder = Config::new()
        .with_threads(1)
        .with_encoder_config(settings)
        .new_context::<T>()
        .unwrap();

    for rise in [0, 20] {
        let bytes_per_sample = if depth == 8 { 1 } else { 2 };
        let mut frame = encoder.new_frame();
        let mut luma = Vec::with_capacity(WIDTH * HEIGHT * bytes_per_sample);
        for y in 0..HEIGHT {
            let level = if y < HEIGHT / 2 { 45 + rise } else { 205 };
            let sample = u16::try_from(level << (depth - 8)).unwrap().to_le_bytes();
            for _ in 0..WIDTH {
                luma.extend_from_slice(&sample[..bytes_per_sample]);
            }
        }
        frame.planes[0].copy_from_raw_u8(&luma, WIDTH * bytes_per_sample, bytes_per_sample);
        let sample = (128u16 << (depth - 8)).to_le_bytes();
        let chroma = sample[..bytes_per_sample].repeat(WIDTH * HEIGHT / 4);
        for plane in &mut frame.planes[1..] {
            plane.copy_from_raw_u8(&chroma, WIDTH * bytes_per_sample / 2, bytes_per_sample);
        }
        encoder.send_frame(frame).unwrap();
    }
    encoder.flush();
    let mut packets = Vec::new();
    loop {
        match encoder.receive_packet() {
            Ok(packet) => packets.push(packet),
            Err(EncoderStatus::Encoded) => {}
            Err(EncoderStatus::LimitReached) => break,
            Err(error) => panic!("AV1 test source: {error}"),
        }
    }
    let [key, delta] = <[_; 2]>::try_from(packets).unwrap();
    assert_eq!(key.frame_type, FrameType::KEY);
    assert_eq!(delta.frame_type, FrameType::INTER);
    [key.data, delta.data]
}
