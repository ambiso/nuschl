# Standalone server and RTC packages

`nix build .#server .#rtc-probe` now assembles existing
crate2nix members. No workspace/package manifest, Cargo.lock or generated graph
changed. Windows/Darwin cross builds and the explicit `default-crane` fallback
remain on their separately tracked migration paths. Notice metadata tools may
still use locked Crane vendoring; they do not compile another Rust graph.

The server output contains regular copies of `nuschld` and
`nuschl-storage-worker`, plus its existing complete native-family notice superset.
A symlink join would be wrong: `current_exe()` chooses the sibling storage worker,
so a symlink to the original isolated server crate would lose that adjacency.
Both installed bodies match the already-tested per-crate outputs byte for byte.
Server storage mode still directly spawns its cleared-environment child. NixOS
service hardening applies to the whole service; the client storage bwrap path is
separate. A stronger standalone server-worker boundary remains an explicit TODO.

The RTC probe contains one compiled executable and its own complete scoped
notices. Its real 4,297-byte VP8 frame was previously included relative to the
whole workspace, which fails in an isolated crate. A checked probe-only Nix
`postPatch` points that compile-time inclusion at the exact declared Nix fixture
input. The original frame bytes remain embedded; production Rust is unchanged.
The initial failed build is retained, not counted as a successful probe.

## Notice authority

`release/notice-roots.json` has an `rtc-probe-<RustTarget>` scope for each
supported native target: the probe's root crate, the Rust standard library and
the Mumble protocol notice ([release notices](release-notices.md)). The published
`is`, `str0m-proto` and patched `str0m-rust-crypto` packages omit license files;
`about.toml` clarifies them with str0m's MIT text at revision
`5f2183ce7c67117cd1960dfeb11bed11fc188b66`, kept in `release/licenses/crates/str0m`.
The probe's notices contain neither ring nor AWS-LC.

## Actual Linux validation

The targeted product build passed in 15 seconds without a Cargo dependency pass.
The exact embedded VP8 bytes match the source. The migrated probe completed real
owned-loopback UDP transport: 50 Opus frames each way and 17 VP8 frames each way,
with multi-packet 4,297-byte frames and a measured maximum encoded delivery delay
of 9ms in this run. This is synthetic encoded delivery, not microphone latency,
capture/rendering, video calling or A/V synchronization.

`nix/vm-server.nix` now accepts `serverPackage ? nuschl`: the four server services
use the actual standalone product, while client and witness packages stay as
before. Each service's `/proc/MainPID/exe` is checked against that product. The
entire existing six-node suite passed in 300 seconds, including both database
backends, history/persistence, stock client/voice/protocol compatibility, key
transparency/witnesses, encrypted conversations, restarts and Murmur import.
This exercises real default sibling-storage discovery and module hardening;
it does not replace the separately queued stronger worker boundary.

The warm driver build took 20 seconds (metadata/configuration only). The driver
then ran outside the Nix daemon with disk-backed cwd/TMPDIR, no host display or
audio backend and a five-second 8GiB `/tmp` guard. The pinned driver actually
prioritized inherited `XDG_RUNTIME_DIR`: its six machine state directories used
`/run/user/1000` (tmpfs), occupying 197,558,272 allocated bytes after the run.
Final log inspection caught and corrected the initial disk-state assumption. All
six guests stopped; only their six owned state directories were then removed.
Future runs must set both `XDG_RUNTIME_DIR` and `TMPDIR` to an owned short disk
path before startup, avoiding generic host runtime state names. The first launch
was rejected before boot because its output directory did not exist; that
diagnostic is retained, and the complete retry passed. No host devices, public
Mumble server, production key, publication or global/daemon configuration change
was used.

See [the structured evidence](reviews/2026-11-12-server-rtc-packages.json). This
proof freezes the tested package/driver paths and source revision; later unrelated
source changes are not silently covered. Cross/fallback migration, source-filter
cleanup and other platform/runtime validation remain in TODO.

`tests/vm_driver_scratch.py` now probes the actual pinned driver without booting a
VM. With both variables pointing inside a fresh owned 0700 disk directory, it
confirmed all six resolved state paths on btrfs. It rejects tmpfs/ramfs, unexpected
ownership/modes/symlinks and escaped state paths before guest startup. This path
probe does not retroactively change the functional suite's tmpfs location. Later
manual runs go through `just vm-driver` (docs/development.md), which applies that
environment and asserts the actual paths inside the driver before boot.

The actual probe used the frozen driver above with `--test-script` pointing to
this repository's `tests/vm_driver_scratch.py`, and these environment values:

```sh
NUSCHL_VM_SCRATCH_ROOT=/home/ambiso/.cache/ns-sr-probe
XDG_RUNTIME_DIR=/home/ambiso/.cache/ns-sr-probe/run
TMPDIR=/home/ambiso/.cache/ns-sr-probe/tmp
```

Create a fresh root/run/tmp/output tree first (root and runtime mode 0700), pass
its existing output directory with `-o`, and keep the normal no-host-device
environment. Retained probe evidence is `~/.cache/ns-sr-probe/scratch-proof.json`
and `~/.cache/nuschl-server-rtc-build/vm-scratch-probe.log`.
