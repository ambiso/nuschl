# SPDX-License-Identifier: EUPL-1.2
# Third-party notices for each shipped binary set in release/notice-roots.json:
# cargo-about reads the vendored Cargo.lock graph offline (about.toml), and
# release/notices.py adds the components outside Cargo (docs/release-notices.md).
{
  pkgs,
  toolchain,
  craneLib,
  projectRoot,
  webRuntimeNotices ? null,
  musicbotRuntimeNotices ? null,
}:
let
  inherit (pkgs) lib;
  policy = lib.importJSON (projectRoot + "/release/notice-roots.json");
  source = lib.cleanSourceWith {
    src = projectRoot;
    filter =
      path: _:
      !(builtins.elem (builtins.baseNameOf path) [
        ".git"
        ".jj"
        ".direnv"
        "target"
        "node_modules"
      ]);
  };
  emoji = import ./emoji.nix { inherit pkgs; };
  gcc = pkgs.pkgsCross.mingwW64.stdenv.cc.cc;
  mingw = pkgs.pkgsCross.mingwW64.windows.mingw_w64;
  # Only the runtime license files, from the pinned source archives.
  fromArchive =
    name: archive: members:
    pkgs.runCommand "nuschl-${name}-notices" { } ''
      mkdir "$out"
      tar -xf ${archive} -C "$out" --strip-components=1 --wildcards --no-wildcards-match-slash ${
        lib.concatMapStringsSep " " (member: "'*/${member}'") members
      }
    '';
  components = {
    rust-standard-library = [
      "${toolchain}/share/doc/rust/COPYRIGHT-library.html"
      "${toolchain}/share/doc/rust/licenses"
    ];
    # Paths in the source tree.
    mumble-protocol = [ "crates/protocol/proto/LICENSE.mumble" ];
    newcm-math-font = [ "release/licenses/newcm" ];
    syntect-default-assets = [ "release/licenses/syntect-assets" ];
    emoji-assets = map (file: "${emoji}/${file}") [
      "ATTRIBUTION.txt"
      "LICENSE-GRAPHICS"
      "LICENSE-emojibase"
      "LICENSE-unicode-cldr"
    ];
    gcc-runtime = [
      (fromArchive "gcc-runtime" gcc.src [
        "COPYING.RUNTIME"
        "COPYING3"
      ])
    ];
    mingw-w64-runtime = [
      (fromArchive "mingw-w64-runtime" mingw.src [
        "COPYING.MinGW-w64-runtime/COPYING.MinGW-w64-runtime.txt"
        "mingw-w64-libraries/winpthreads/COPYING"
      ])
    ];
    # The C runtime zig links into the portable build, from the zig package
    # that builds it: musl in the static programs, compiler-rt (zig's own, MIT)
    # and libunwind in all of them, and glibc's start-up objects and
    # libc_nonshared in the glibc-linked desktop and media worker.
    musl = [ "${pkgs.zig}/lib/zig/libc/musl/COPYRIGHT" ];
    zig-runtime = [
      "${pkgs.zig.src}/LICENSE"
      "${pkgs.zig}/lib/zig/libunwind/LICENSE.TXT"
    ];
    glibc-startup = [
      "${pkgs.zig}/lib/zig/libc/glibc/LICENSES"
      (fromArchive "glibc" pkgs.glibc.src [ "COPYING.LESSERv2" ])
    ];
    web-runtime-assets = [ webRuntimeNotices ];
    musicbot-web-runtime-assets = [ musicbotRuntimeNotices ];
  };
  forScope =
    scope:
    let
      set = policy.${scope} or (throw "unknown release notice scope ${scope}");
      features =
        package:
        lib.concatMapStrings (feature: " --features ${lib.removePrefix "${package}/" feature}") (
          builtins.filter (lib.hasPrefix "${package}/") set.features
        );
      reports = lib.concatMapStrings (
        target:
        lib.concatMapStrings (package: ''
          report=reports/${target}-${package}
          cargo-about generate --frozen --format json --fail -c about.toml \
            --manifest-path "$(jq -r '.packages[] | select(.name == "${package}") | .manifest_path' metadata.json)" \
            --target ${target}${features package} -o $report.json 2> $report.log \
            && ! grep -q 'failed to validate' $report.log || { cat $report.log; exit 1; }
        '') set.packages
      ) set.targets;
      componentArgs = lib.concatStrings (
        lib.mapAttrsToList (
          name: paths: lib.concatMapStrings (path: " --component ${name}=${path}") paths
        ) (lib.getAttrs set.components components)
      );
      bundle = craneLib.mkCargoDerivation {
        pname = "nuschl-release-notices-${scope}";
        version = "1";
        src = source;
        cargoArtifacts = null;
        cargoVendorDir = craneLib.vendorCargoDeps { src = source; };
        nativeBuildInputs = [
          pkgs.cargo-about
          pkgs.jq
          pkgs.python3
        ];
        doCheck = false;
        # cargo-about reads clarified license files by absolute path. It warns
        # about each clarified crate outside a root's graph, so its log is shown
        # only when it fails or a clarified file's checksum differs.
        buildPhaseCargoCommand = ''
          substituteInPlace about.toml --replace-fail @licenses@ "$PWD/release/licenses"
          mkdir reports
          cargo metadata --frozen --no-deps --format-version 1 > metadata.json
          ${reports}
          python3 release/notices.py --scope ${scope} --policy release/notice-roots.json${componentArgs} \
            --output "$out/share/licenses/nuschl" reports/*.json
          cp Cargo.lock "$out/share/licenses/nuschl/Cargo.lock"
        '';
        installPhaseCommand = "";
      };
    in
    {
      inherit bundle;
    };
in
{
  inherit forScope;
  linux = forScope "linux-nix";
  portable = forScope "linux-portable";
  serverStatic = forScope "server-static";
  windows = forScope "windows-x86_64";
  macos = forScope "macos-universal";
  web = forScope "web";
  callPage = forScope "call-page";
}
