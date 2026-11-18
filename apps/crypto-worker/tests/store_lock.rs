// SPDX-License-Identifier: EUPL-1.2
//! Store ownership across processes. The test spawns a copy of this binary, and a
//! forked child shares every open file description of its parent until it execs,
//! flock locks included. Keep store tests that drop a lock and reopen the store at
//! once in the unit tests, a binary that spawns no processes.
use nuschl_crypto_worker::{KeySource, Store};
use std::{
    fs,
    io::{self, Read},
    path::PathBuf,
    process::{Child, Command, Stdio},
    time::{Duration, Instant},
};

#[test]
fn exclusive_store_lock_child() {
    let Some(path) = std::env::var_os("NUSCHL_STORE_LOCK_TEST_DIRECTORY") else {
        return;
    };
    let directory = PathBuf::from(path);
    let store = Store::open(&directory, KeySource::File(directory.join("store.key"))).unwrap();
    store.save("state", b"child-owned state").unwrap();
    fs::write(directory.join("ready"), b"ready").unwrap();
    // The parent holds stdin open and kills us, exercising OS cleanup rather
    // than Rust Drop. Never use a real profile or user identity in this test.
    let mut byte = [0];
    let _ = io::stdin().read(&mut byte);
    drop(store);
}

#[test]
fn another_process_is_excluded_and_crash_releases_ownership() {
    struct Owner(Child);
    impl Drop for Owner {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
    let directory = tempfile::tempdir().unwrap();
    let mut child = Owner(
        Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "exclusive_store_lock_child", "--nocapture"])
            .env("NUSCHL_STORE_LOCK_TEST_DIRECTORY", directory.path())
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::inherit())
            .spawn()
            .unwrap(),
    );
    let deadline = Instant::now() + Duration::from_secs(10);
    while !directory.path().join("ready").exists() {
        assert!(
            child.0.try_wait().unwrap().is_none(),
            "lock owner exited early"
        );
        assert!(Instant::now() < deadline, "lock owner did not become ready");
        std::thread::sleep(Duration::from_millis(10));
    }
    let key_path = directory.path().join("store.key");
    let key_before = fs::read(&key_path).unwrap();
    let error = Store::open(directory.path(), KeySource::File(key_path.clone()))
        .err()
        .expect("another process owns this store");
    assert_eq!(error.kind(), io::ErrorKind::WouldBlock);
    child.0.kill().unwrap();
    child.0.wait().unwrap();
    let reopened = Store::open(directory.path(), KeySource::File(key_path.clone()))
        .expect("the OS releases the lock after process death");
    assert_eq!(fs::read(&key_path).unwrap(), key_before);
    assert_eq!(
        &reopened.load("state").unwrap().unwrap()[..],
        b"child-owned state"
    );
}
