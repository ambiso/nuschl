// SPDX-License-Identifier: EUPL-1.2
#![cfg(target_os = "linux")]

use nuschl_ipc::video::{Encoder, Output, Settings, Source};
use std::{
    io::Write as _,
    path::Path,
    process::{Command, Stdio},
    time::Duration,
};

const WORKER: &str = env!("CARGO_BIN_EXE_nuschl-video-worker");
const PROBE: &str = env!("CARGO_BIN_EXE_nuschl-video-probe");

#[tokio::test]
async fn confined_worker_recovers_and_the_probe_produces_decodable_av1() {
    nuschl_testkit::sandbox();
    let settings = Settings {
        source: Source::Screen,
        width: 160,
        height: 120,
        fps: 30,
        bitrate: 1_000_000,
    };
    let path = Path::new(WORKER);
    let mut encoder = Encoder::start(path.to_owned(), settings).unwrap();
    assert!(encoder.encode(vec![0; 1]).await.is_err());
    let input = vec![128; settings.frame_bytes().unwrap()];
    assert!(matches!(
        encoder.encode(input.clone()).await.unwrap(),
        Output::Keyframe(_)
    ));

    let workers = nuschl_testkit::running(path);
    assert_eq!(workers.len(), 1);
    assert!(
        Command::new(nuschl_testkit::program("kill"))
            .args(["-KILL", &workers[0].to_string()])
            .status()
            .unwrap()
            .success()
    );
    assert!(encoder.encode(input.clone()).await.is_err());
    assert!(matches!(
        encoder.encode(input).await.unwrap(),
        Output::Keyframe(_)
    ));
    encoder.stop().await.unwrap();
    nuschl_testkit::none_running(path, Duration::from_secs(3)).await;

    let output = Command::new(PROBE).arg("--obu").output().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let mut decoder = Command::new(nuschl_testkit::program("ffmpeg"))
        .args([
            "-v", "error", "-c:v", "libdav1d", "-f", "obu", "-i", "pipe:0", "-f", "rawvideo",
            "-pix_fmt", "yuv420p", "pipe:1",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    decoder
        .stdin
        .take()
        .unwrap()
        .write_all(&output.stdout)
        .unwrap();
    let decoded = decoder.wait_with_output().unwrap();
    assert!(decoded.status.success());
    assert_eq!(decoded.stdout.len(), 3 * settings.frame_bytes().unwrap());
    assert!(decoded.stdout[8] < 85);
    assert!(decoded.stdout[24] > 170);
}
