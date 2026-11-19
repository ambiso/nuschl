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
static SERIAL: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

#[tokio::test]
async fn confined_worker_recovers_and_the_probe_produces_decodable_av1() {
    let _serial = SERIAL.lock().await;
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

/// A separate PipeWire daemon owns the generator and socket. Passing its
/// connected FD exercises the portal-FD launch path without opening a portal.
#[tokio::test]
async fn video_capture_consumes_only_the_inherited_pipewire_connection() {
    let _serial = SERIAL.lock().await;
    nuschl_testkit::sandbox();
    let directory = nuschl_testkit::temp_dir();
    let run = directory.path();
    std::fs::write(
        run.join("fixture.conf"),
        r#"
context.properties = { core.daemon = true core.name = pipewire-0 }
context.spa-libs = {
  support.* = support/libspa-support
  videotestsrc = videotestsrc/libspa-videotestsrc
}
context.modules = [
  { name = libpipewire-module-protocol-native }
  { name = libpipewire-module-spa-node-factory }
  { name = libpipewire-module-client-node }
  { name = libpipewire-module-link-factory }
  { name = libpipewire-module-access }
  { name = libpipewire-module-metadata }
]
context.objects = [
  { factory = spa-node-factory
    args = { factory.name = videotestsrc node.name = capture-fixture node.driver = true node.param.Props = { live = true } }
  }
]
"#,
    )
    .unwrap();
    let mut pipewire = tokio::process::Command::new(nuschl_testkit::program("pipewire"));
    pipewire
        .env_clear()
        .env("PIPEWIRE_RUNTIME_DIR", run)
        .env("PIPEWIRE_CONFIG_DIR", run)
        .args(["-c", "fixture.conf"])
        .kill_on_drop(true);
    let mut daemon = pipewire.spawn().unwrap();

    let pw = |name: &str| {
        let mut command = tokio::process::Command::new(nuschl_testkit::program(name));
        command.env_clear().env("PIPEWIRE_RUNTIME_DIR", run);
        command
    };
    let socket = run.join("pipewire-0");
    let remote = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if let Ok(socket) = std::os::unix::net::UnixStream::connect(&socket) {
                break socket;
            }
            assert!(
                daemon.try_wait().unwrap().is_none(),
                "PipeWire exited before its socket appeared"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
    let result = pw("pw-dump").output().await.unwrap();
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    let graph: Vec<serde_json::Value> = serde_json::from_slice(&result.stdout).unwrap();
    let node = graph
        .iter()
        .find(|object| object["info"]["props"]["node.name"] == "capture-fixture")
        .and_then(|object| object["id"].as_u64())
        .and_then(|id| u32::try_from(id).ok())
        .expect("the synthetic Nuschl source appears in its private graph");
    let mut capture = nuschl_ipc::video::Capture::start(
        Path::new(WORKER).to_owned(),
        remote.into(),
        Source::Screen,
        Some(node),
    )
    .unwrap();

    let join = tokio::time::timeout(Duration::from_secs(6), async {
        loop {
            let output = pw("pw-link").arg("-i").output().await.unwrap();
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
            let ports = String::from_utf8_lossy(&output.stdout);
            if let Some(port) = ports
                .lines()
                .find(|port| port.starts_with("nuschl-video-capture:"))
            {
                break port.to_owned();
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    });
    let port = join.await.expect("the worker creates its video input port");
    let outputs = pw("pw-link").arg("-o").output().await.unwrap();
    assert!(outputs.status.success());
    let available = String::from_utf8(outputs.stdout).unwrap();
    let producer = available
        .lines()
        .find(|line| line.starts_with("capture-fixture:"))
        .unwrap_or_else(|| panic!("video source has no output port in {available:?}"));
    let result = pw("pw-link")
        .args([producer, &port])
        .output()
        .await
        .unwrap();
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );

    let frame = capture
        .next()
        .await
        .expect("the worker returns bounded AV1");
    assert!(!frame.av1.is_empty());
    assert!(frame.width >= 16 && frame.height >= 16);
    assert!(frame.capture_time_us > 0);
    capture.stop().await.unwrap();
    nuschl_testkit::none_running(Path::new(WORKER), Duration::from_secs(3)).await;
    daemon.kill().await.unwrap();
}
