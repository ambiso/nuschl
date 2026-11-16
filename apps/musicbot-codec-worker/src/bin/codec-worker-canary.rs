// SPDX-License-Identifier: EUPL-1.2
//! Reports what a confined codec child can reach, for the launcher test in
//! `tests/confined.rs`, which alone starts it. Started like the codec worker,
//! it reads a file path and a loopback port from standard input.
use std::{io::Read, net::TcpStream, path::Path, time::Duration};

fn main() {
    let mut input = String::new();
    std::io::stdin().read_to_string(&mut input).unwrap();
    let (file, port) = input.rsplit_once(' ').unwrap();
    let port: u16 = port.trim().parse().unwrap();
    let loopback = ([127, 0, 0, 1], port).into();
    let connected = TcpStream::connect_timeout(&loopback, Duration::from_secs(1)).is_ok();
    let environment: Vec<_> = std::env::vars_os().map(|(name, _)| name).collect();
    println!("file visible: {}", Path::new(file).exists());
    println!("loopback connected: {connected}");
    println!("environment: {environment:?}");
    println!("directory: {}", std::env::current_dir().unwrap().display());
}
