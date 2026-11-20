// SPDX-License-Identifier: EUPL-1.2
//! Call media receives commands and media epochs on separate inherited channels.

use std::process::ExitCode;

mod call;

fn main() -> ExitCode {
    #[cfg(windows)]
    nuschl_sandbox_windows::launch_if_requested();
    let mut args = std::env::args_os().skip(1);
    if args.next().as_deref() != Some(std::ffi::OsStr::new("--call")) || args.next().is_some() {
        eprintln!("usage: nuschl-rtc-worker --call");
        return ExitCode::FAILURE;
    }
    match call::run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(_) => ExitCode::FAILURE,
    }
}
