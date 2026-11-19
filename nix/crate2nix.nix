# Per-crate Nix builds (crate2nix): every crate is its own derivation, so a
# change rebuilds only the crates that depend on it, and unchanged crates come
# from the store. `nix/crate2nix/Cargo.nix` is generated from Cargo.lock; run
# `just crate2nix` after changing dependencies (the flake checks it is current).
{
  pkgs,
  toolchain,
}:
let
  inherit (pkgs) lib;
  # Read by the desktop's build script (apps/desktop/build.rs).
  emojiAssets = import ./emoji.nix { inherit pkgs; };
  pkgConfig = [ pkgs.pkg-config ];
  # Libraries the final binaries link against (and whose -sys crates probe).
  linuxLibraries = [
    pkgs.alsa-lib
    pkgs.pipewire
    pkgs.fontconfig
    pkgs.freetype
    pkgs.libxkbcommon
    pkgs.wayland
    pkgs.libxcb
    pkgs.libx11
  ];
  withLibraries = _: {
    nativeBuildInputs = pkgConfig;
    buildInputs = linuxLibraries;
  };
  bindgen = _: {
    nativeBuildInputs = pkgConfig ++ [ pkgs.rustPlatform.bindgenHook ];
    buildInputs = [ pkgs.pipewire ];
  };
  videoCodec = _: {
    CARGO_CFG_TARGET_ARCH = pkgs.stdenv.hostPlatform.parsed.cpu.name;
    CARGO_CFG_TARGET_OS = pkgs.stdenv.hostPlatform.parsed.kernel.name;
    CARGO_ENCODED_RUSTFLAGS = "";
  };
  overrides = pkgs.defaultCrateOverrides // {
    # These build scripts read Cargo's target environment directly.
    av-scenechange = videoCodec;
    rav1e = videoCodec;
    pipewire-sys = bindgen;
    libspa-sys = bindgen;
    # Their build scripts probe the PipeWire version with pkg-config.
    pipewire = bindgen;
    libspa = bindgen;
    yeslogic-fontconfig-sys = withLibraries;
    freetype-sys = withLibraries;
    wayland-sys = withLibraries;
    xkbcommon = withLibraries;
    # prost-build compiles the Mumble protocol definitions.
    nuschl-protocol = _: {
      nativeBuildInputs = [ pkgs.protobuf ];
      PROTOC = "${pkgs.protobuf}/bin/protoc";
    };
    # Server integration tests read the unchanged workspace import fixture.
    # Isolated crate sources do not contain the repository's tests directory.
    nuschld = _: {
      NUSCHL_TEST_MURMUR_FIXTURE = "${../tests/fixtures/murmur-1.5.json}";
    };
    # The probe embeds the actual workspace fixture; isolated crate sources do
    # not contain ../../../tests. Keep identical frame bytes as an explicit input.
    nuschl-rtc-probe = _: {
      postPatch = ''
        substituteInPlace src/main.rs --replace-fail \
          '"../../../tests/fixtures/rtc-keyframe.vp8"' \
          '"${../tests/fixtures/rtc-keyframe.vp8}"'
      '';
    };
    # The Murmur tests read the workspace fixtures; isolated crate sources do
    # not contain ../../tests.
    nuschl-storage = _: {
      postPatch = ''
        substituteInPlace src/murmur.rs --replace-fail \
          '"../../tests/fixtures"' '"${../tests/fixtures}"'
      '';
    };
    nuschl = attrs: withLibraries attrs // { NUSCHL_EMOJI_ASSETS = emojiAssets; };
    nuschl-media-worker = withLibraries;
    # The cli's e2ee tests exec the crypto worker beside the cli binary the
    # harness records at test-compile time (CARGO_BIN_EXE_nuschl-cli); the
    # merged dev dependency nuschld marks that compilation, and the worker
    # joins its bin directory.
    nuschl-cli =
      attrs:
      withLibraries attrs
      //
        lib.optionalAttrs
          (lib.any (d: lib.hasInfix "rust_nuschld" (d.name or (toString d))) (attrs.dependencies or [ ]))
          {
            postInstall = (attrs.postInstall or "") + ''
              cp ${project.workspaceMembers.nuschl-crypto-worker.build}/bin/nuschl-crypto-worker \
                $out/bin/nuschl-crypto-worker
            '';
          };
  };
  project = import ./crate2nix/Cargo.nix {
    inherit pkgs;
    buildRustCrateForPkgs =
      cratePkgs:
      cratePkgs.buildRustCrate.override {
        rustc = toolchain;
        cargo = toolchain;
      };
    defaultCrateOverrides = overrides;
  };
in
{
  inherit project;
  # The programs of the desktop package (with the terminal client), each
  # built from cached crates.
  binaries = map (member: project.workspaceMembers.${member}.build) [
    "nuschl"
    "nuschl-cli"
    "nuschl-content-worker"
    "nuschl-crypto-worker"
    "nuschl-math-worker"
    "nuschl-media-worker"
    "nuschl-storage-worker"
    "nuschl-transfer-worker"
    "nuschl-tui"
    "nuschld"
    "nuschl-witness"
  ];
}
