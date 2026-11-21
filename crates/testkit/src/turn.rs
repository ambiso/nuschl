// SPDX-License-Identifier: EUPL-1.2
//! An owned coturn process with test-only REST credentials and loopback addresses.

use std::{
    fs::File,
    net::{Ipv4Addr, UdpSocket},
    process::{Child, Command},
    thread::sleep,
    time::{Duration, Instant},
};

pub(crate) const TEST_SECRET: &str = "test-secret";
pub const RELAY_IP: Ipv4Addr = Ipv4Addr::new(127, 0, 0, 2);

pub struct Turn {
    child: Child,
    _directory: tempfile::TempDir,
    pub url: String,
}

impl Drop for Turn {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

pub fn start() -> Turn {
    let directory = crate::temp_dir();
    let port = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0))
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let logs = directory.path().join("turn.log");
    let log = File::create(&logs).unwrap();
    let config = directory.path().join("turn.conf");
    std::fs::write(
        &config,
        format!(
            r#"
listening-ip=127.0.0.1
listening-port={port}
relay-ip={RELAY_IP}
realm=turn.test
use-auth-secret
static-auth-secret={TEST_SECRET}
allow-loopback-peers
no-cli
no-tcp
no-tls
no-dtls
no-tcp-relay
relay-threads=1
pidfile={}
userdb={}
log-file=stdout
simple-log
no-stdout-log
"#,
            directory.path().join("pid").display(),
            directory.path().join("db").display()
        ),
    )
    .unwrap();
    let child = Command::new(crate::program("turnserver"))
        .arg("-c")
        .arg(config)
        .stderr(log.try_clone().unwrap())
        .stdout(log)
        .spawn()
        .expect("start coturn");
    let mut turn = Turn {
        child,
        _directory: directory,
        url: format!("turn:127.0.0.1:{port}?transport=udp"),
    };
    let start = Instant::now();
    loop {
        let log = std::fs::read_to_string(&logs).unwrap();
        if log.contains("Total auth threads:") && turn.child.try_wait().unwrap().is_none() {
            return turn;
        }
        assert!(
            turn.child.try_wait().unwrap().is_none() && start.elapsed() < Duration::from_secs(10),
            "coturn failed to open its loopback listener: {log}"
        );
        sleep(Duration::from_millis(20));
    }
}
