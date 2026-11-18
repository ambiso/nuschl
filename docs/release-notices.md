# Release notices

Every shipped binary set carries its third-party notices in
`share/licenses/nuschl/` (Windows: `licenses/`, macOS:
`Contents/Resources/licenses/`, web: `licenses/nuschl/`):

- `THIRD-PARTY.txt`: the crate list with each crate's declared license, the
  license texts with the crates that use them, and the components outside Cargo.
- `THIRD-PARTY.json`: the scope, its policy entry, the crate list and the
  SHA-256 of `THIRD-PARTY.txt`.
- `Cargo.lock`.

## Scopes

`release/notice-roots.json` names each scope's targets, packaged root crates
(desktop with its workers, server, music bot, RTC probe, web WASM), features and
components outside Cargo. `nix/release-notices.nix` builds one bundle per scope;
`nix build .#release-notices-linux` (also `-windows`, `-macos`, `-web`,
`-musicbot`, `-rtc`) builds them on their own.

## Rust crates

[cargo-about](https://github.com/EmbarkStudios/cargo-about) reads the vendored
`Cargo.lock` graph offline, once per root and target, without build and dev
dependencies. `about.toml` holds the accepted licenses from `deny.toml`, ordered
by preference: of an OR expression, the first accepted alternative is the one
whose text ships. `release/notices.py` merges the reports and fails the build
when a crate has no license text of its own; cargo-about's canonical SPDX text
counts as missing. Proc-macro crates run only at
build time and are left out. Workspace members are first-party and covered by
`LICENSE` (EUPL-1.2).

A crate whose package lacks its license text, or whose file cargo-about does not
recognize, gets a clarification in `about.toml`: the license and the files, each
pinned by SHA-256. Files outside the package are upstream texts of the published
revision (`.cargo_vcs_info.json`), kept in `release/licenses/crates/`;
`@licenses@` in their paths becomes that directory during the build. Further
clarifications add the notices of bundled third-party code and data (Apache
`NOTICE` files, `LICENSE-THIRD-PARTY`, embedded fonts, Unicode tables).

hpke-rs, hpke-rs-crypto and hpke-rs-libcrux declare MPL-2.0 and publish no MPL
text; they ship the Mozilla text and the source locations in
`release/licenses/crates/hpke-rs/`. The modified hpke-rs-libcrux 0.7.0 is
`vendor/hpke-rs-libcrux` in the Nuschl source.

## Components outside Cargo

| Component | Texts | Scopes |
| --- | --- | --- |
| `rust-standard-library` | The toolchain's `COPYRIGHT-library.html` and `licenses/` | all |
| `mumble-protocol` | `crates/protocol/proto/LICENSE.mumble` | all |
| `newcm-math-font` | `release/licenses/newcm/` (the math worker's font) | native desktop |
| `syntect-default-assets` | `release/licenses/syntect-assets/` (syntax and theme grants) | desktop, web, music bot |
| `emoji-assets` | Twemoji (CC-BY 4.0) attribution, emojibase-data and CLDR licenses from `nix/emoji.nix` ([emoji](emoji.md)) | desktop, web |
| `gcc-runtime`, `mingw-w64-runtime` | `COPYING.RUNTIME`, `COPYING3`, the MinGW runtime and winpthreads licenses, from the pinned source archives | Windows |
| `web-runtime-assets` | The web app's `licenses/`: npm notices of the modules Vite emits, and Temml's | web |
| `musicbot-web-runtime-assets` | The music bot page's npm notices | music bot |

`web/app/build-runtime-notices.js` collects the npm notices during the Vite
build and stops it when an emitted package has no license file.

## Checks

The notice derivation fails on a crate without license text, a clarification
whose file hash differs, or a component missing from the scope.
`release/check-notices.py --scope <scope> <directory>` and the release tools
(`release/prepare-artifacts.py`, `release/local-release.py`) check a bundle's
scope, policy entry and text hash against `release/notice-roots.json` before
upload and signing.

After a dependency update, build the notice bundles. A new crate without license
text fails with its name; add its clarification to `about.toml`, with the
upstream text in `release/licenses/crates/` when the package has none.
