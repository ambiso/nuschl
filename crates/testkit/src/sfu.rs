// SPDX-License-Identifier: EUPL-1.2
//! A real companion SFU with signaling and media on loopback.

use std::{
    io::{BufRead, BufReader},
    net::SocketAddr,
    process::{Child, Command, Stdio},
};

pub struct Sfu {
    child: Child,
    pub url: String,
    pub media: SocketAddr,
    pub directory: crate::TempDir,
}

impl Drop for Sfu {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// The SFU with `settings` added to its configuration; `{directory}` in them
/// names its temporary directory.
pub fn start(executable: impl AsRef<std::path::Path>, settings: &str) -> Sfu {
    let directory = crate::temp_dir();
    let secret = directory.path().join("turn-secret");
    std::fs::write(&secret, "test-secret\n").expect("secret");
    let config = directory.path().join("sfu.toml");
    std::fs::write(
        &config,
        format!(
            r#"
            listen = "127.0.0.1:0"
            media_listen = "127.0.0.1:0"
            media_addresses = ["127.0.0.1"]
            max_peers_per_room = 6
            {}
            [turn]
            urls = ["turn:turn.test:3478?transport=udp"]
            secret_file = "{}"
            "#,
            settings.replace("{directory}", &directory.path().display().to_string()),
            secret.display()
        ),
    )
    .expect("config");
    let mut child = Command::new(executable.as_ref())
        .arg("--config")
        .arg(&config)
        .stdout(Stdio::piped())
        .spawn()
        .expect("start the SFU");
    let mut line = String::new();
    BufReader::new(child.stdout.take().expect("stdout"))
        .read_line(&mut line)
        .expect("ready line");
    let signalling = line.split_whitespace().nth(2).expect("signalling address");
    let media = line
        .split_whitespace()
        .nth(4)
        .expect("media address")
        .parse()
        .expect("UDP address");
    Sfu {
        child,
        url: format!("ws://{signalling}/sfu"),
        media,
        directory,
    }
}
