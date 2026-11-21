// SPDX-License-Identifier: EUPL-1.2
//! `nuschl-crypto-worker --data DIR`: serves `nuschl_ipc::crypto` requests on
//! stdin/stdout until stdin closes. Started by the session in its sandbox
//! (`nuschl_sandbox::Role::Crypto`); never logs secrets, and logs
//! nothing at all on stdout.
use nuschl_crypto_worker::{KeySource, Store, Worker};
use nuschl_ipc::{
    Envelope,
    crypto::{CryptoRequest, CryptoResponse},
    read_frame, write_frame,
};
use std::{io, path::PathBuf, process::ExitCode};

fn run() -> Result<(), Box<dyn std::error::Error>> {
    let mut arguments = std::env::args_os().skip(1);
    let mut data = None;
    while let Some(argument) = arguments.next() {
        match argument.to_str() {
            Some("--data") => data = arguments.next().map(PathBuf::from),
            _ => return Err("usage: nuschl-crypto-worker --data DIR".into()),
        }
    }
    let data = data.ok_or("usage: nuschl-crypto-worker --data DIR")?;
    // Two threads for OpenMLS's TreeKEM work: one per CPU would not fit the
    // sandbox's address-space limit (nuschl-sandbox) on a large machine.
    nuschl_e2ee::limit_threads(2);
    let store = Store::open(&data, KeySource::File(data.join("store.key")))?;
    let mut worker = Worker::open(store)?;
    let mut output = io::stdout().lock();
    #[cfg(target_os = "linux")]
    if let Some(mut socket) = control_socket()? {
        use nuschl_ipc::crypto::{
            call::{self, Action},
            descriptor,
        };
        while let Some((request, endpoint)) =
            descriptor::read_request::<CryptoRequest>(&mut socket)?
        {
            let response = match (request.payload, endpoint) {
                (
                    CryptoRequest::Call(call::Request {
                        generation,
                        action: Action::AttachMedia,
                    }),
                    Some(endpoint),
                ) => worker.attach_call_media(generation, endpoint),
                (_, Some(_)) => return Err("unexpected descriptor in crypto request".into()),
                (request, None) => worker.handle(request),
            };
            write_frame(&mut output, &Envelope::new(request.request_id, response))?;
        }
        return Ok(());
    }
    let mut input = io::stdin().lock();
    while let Some(request) = read_frame::<CryptoRequest>(&mut input)? {
        let response: CryptoResponse = worker.handle(request.payload);
        write_frame(&mut output, &Envelope::new(request.request_id, response))?;
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn control_socket() -> io::Result<Option<std::os::unix::net::UnixStream>> {
    use std::os::{fd::AsFd as _, unix::fs::FileTypeExt as _};
    if !std::fs::metadata("/proc/self/fd/0")?
        .file_type()
        .is_socket()
    {
        return Ok(None);
    }
    let fd = io::stdin().as_fd().try_clone_to_owned()?;
    nuschl_ipc::crypto::descriptor::connected_stream(fd).map(Some)
}

fn main() -> ExitCode {
    // On Windows, the launch stage starts this worker again in its AppContainer.
    #[cfg(windows)]
    nuschl_sandbox_windows::launch_if_requested();
    #[cfg(any(target_os = "macos", windows))]
    if std::env::args_os()
        .skip(1)
        .eq([std::ffi::OsStr::new("--call-keys")])
    {
        return if nuschl_crypto_worker::serve_call_keys().is_ok() {
            ExitCode::SUCCESS
        } else {
            ExitCode::FAILURE
        };
    }
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("nuschl-crypto-worker: {error}");
            ExitCode::FAILURE
        }
    }
}
