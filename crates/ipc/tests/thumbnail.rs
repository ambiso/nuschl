// SPDX-License-Identifier: EUPL-1.2
//! The cover thumbnail supervisor (`nuschl_ipc::host::thumbnail`) and the
//! real sandbox against a faulty content worker
//! (`src/bin/faulty-content-worker.rs`). The real worker's thumbnails are
//! tested in `apps/content-worker/tests/worker.rs`.
#![cfg(target_os = "linux")]
use nuschl_ipc::host::{self, Error, Thumbnail};
use std::{
    path::Path,
    time::{Duration, Instant},
};
use tokio::sync::Mutex;

const FAULTY: &str = env!("CARGO_BIN_EXE_faulty-content-worker");
/// Tests that count faulty workers run one at a time.
static ONE_AT_A_TIME: Mutex<()> = Mutex::const_new(());

async fn thumbnail(worker: &str, image: &[u8]) -> Result<Thumbnail, Error> {
    nuschl_testkit::sandbox();
    host::thumbnail(Path::new(worker), image, 400, 400).await
}

#[tokio::test]
async fn malformed_replies_are_refused_and_their_workers_reaped() {
    let _one = ONE_AT_A_TIME.lock().await;
    let made = thumbnail(FAULTY, b"valid").await.unwrap();
    assert_eq!((made.width, made.height), (1, 1));
    for fault in ["wrong id", "twice", "wrong size", "not a png", "wrong kind"] {
        let result = thumbnail(FAULTY, fault.as_bytes()).await;
        assert!(
            matches!(result, Err(Error::InvalidResponse)),
            "{fault}: {result:?}"
        );
    }
    // The advertised length is refused before anything is read or allocated.
    let result = thumbnail(FAULTY, b"huge").await;
    assert!(
        matches!(result, Err(Error::Frame(nuschl_ipc::Error::TooLarge))),
        "{result:?}"
    );
    assert!(matches!(
        thumbnail(FAULTY, b"crash").await,
        Err(Error::Io(_))
    ));
    nuschl_testkit::none_running(Path::new(FAULTY), Duration::from_secs(3)).await;
}

#[tokio::test]
async fn oversized_input_and_a_missing_worker_fail_before_a_launch() {
    let oversized = vec![0; nuschl_ipc::MAX_THUMBNAIL_INPUT_BYTES + 1];
    assert!(matches!(
        thumbnail(FAULTY, &oversized).await,
        Err(Error::InvalidResponse)
    ));
    assert!(
        thumbnail("/nonexistent/content-worker", b"valid")
            .await
            .is_err()
    );
}

/// Run once more by itself with an empty `PATH`, where the sandbox's
/// programs are missing: the supervisor refuses to start the worker unconfined.
#[tokio::test]
async fn missing_sandbox_programs_fail_the_request() {
    if std::env::var_os("PATH").is_some_and(|path| path.is_empty()) {
        let result = host::thumbnail(Path::new(FAULTY), b"valid", 400, 400).await;
        assert!(
            matches!(
                result,
                Err(Error::Sandbox(nuschl_sandbox::Error::MissingSandboxHelper(
                    _
                )))
            ),
            "{result:?}"
        );
        return;
    }
    let run = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "missing_sandbox_programs_fail_the_request"])
        .env_clear()
        .env("PATH", "")
        .output()
        .unwrap();
    assert!(
        run.status.success(),
        "{}",
        String::from_utf8_lossy(&run.stdout)
    );
}

#[tokio::test]
async fn a_worker_that_stays_after_its_reply_is_stopped_after_two_seconds() {
    let _one = ONE_AT_A_TIME.lock().await;
    let started = Instant::now();
    assert!(matches!(
        thumbnail(FAULTY, b"linger").await,
        Err(Error::Timeout)
    ));
    assert!(started.elapsed() >= Duration::from_secs(2));
    nuschl_testkit::none_running(Path::new(FAULTY), Duration::from_secs(3)).await;
}

/// The bot runs the supervisor in its own task, which keeps the cover job
/// until the worker has exited, whenever the HTTP client stops waiting.
#[tokio::test]
async fn a_caller_that_stops_waiting_leaves_the_worker_to_its_deadline() {
    let _one = ONE_AT_A_TIME.lock().await;
    let started = Instant::now();
    let mut job = tokio::spawn(thumbnail(FAULTY, b"hang"));
    // The HTTP client gives up after a second; the job keeps the worker.
    assert!(
        tokio::time::timeout(Duration::from_secs(1), &mut job)
            .await
            .is_err()
    );
    assert_eq!(nuschl_testkit::running(Path::new(FAULTY)).len(), 1);
    assert!(matches!(job.await.unwrap(), Err(Error::Timeout)));
    assert!(started.elapsed() >= Duration::from_secs(5));
    nuschl_testkit::none_running(Path::new(FAULTY), Duration::from_secs(3)).await;
}
