// SPDX-License-Identifier: EUPL-1.2
//! Commands use duplex stdout. Media epochs arrive on the private stdin socket.

use std::process::ExitCode;

#[cfg(target_os = "linux")]
mod call;

#[cfg(target_os = "linux")]
fn main() -> ExitCode {
    let mut args = std::env::args_os().skip(1);
    if args.next().as_deref() != Some(std::ffi::OsStr::new("--call")) || args.next().is_some() {
        eprintln!("usage: nuschl-rtc-worker --call");
        return ExitCode::FAILURE;
    }
    match call::run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(_) => {
            eprintln!("nuschl-rtc-worker: call media stopped");
            ExitCode::FAILURE
        }
    }
}

#[cfg(not(target_os = "linux"))]
fn main() -> ExitCode {
    ExitCode::FAILURE
}
