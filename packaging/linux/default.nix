# Debian and Arch packages of the Linux desktop. Versions come from the
# workspace version; packages are published from release tags.
{
  pkgs,
  # bin/ with the desktop executable and its eight helpers.
  binaries,
  projectRoot,
  nativeNotices,
  desktopFiles,
  appstreamXML,
  revision ? "1",
  maintainer ? "Nuschl <packages@nuschl.eu>",
}:
let
  version =
    (builtins.fromTOML (builtins.readFile (projectRoot + "/Cargo.toml"))).workspace.package.version;
  programs = [
    "nuschl"
    "nuschl-content-worker"
    "nuschl-crypto-worker"
    "nuschl-math-worker"
    "nuschl-media-worker"
    "nuschl-storage-worker"
    "nuschl-transfer-worker"
    "nuschl-update-verify-ed25519"
    "nuschl-update-verify-mldsa44"
  ];
  # Helper discovery looks next to the executable, so all nine share a directory.
  stage = pkgs.runCommand "nuschl-stage-${version}" { } ''
    for program in ${pkgs.lib.escapeShellArgs programs}; do
      install -Dm755 ${binaries}/bin/$program "$out/usr/lib/nuschl/$program"
    done
    mkdir -p "$out/usr/bin"
    ln -s ../lib/nuschl/nuschl "$out/usr/bin/nuschl"
    cp -R --no-preserve=mode ${desktopFiles}/share "$out/usr/"
    cp -R --no-preserve=mode ${nativeNotices}/share/licenses "$out/usr/share/"
    install -Dm644 ${projectRoot + "/LICENSE"} "$out/usr/share/licenses/nuschl/EUPL-1.2"
    install -Dm644 ${
      projectRoot + "/crates/protocol/proto/LICENSE.mumble"
    } "$out/usr/share/licenses/nuschl/Mumble-BSD"
    install -Dm644 ${appstreamXML} "$out/usr/share/metainfo/org.nuschl.Nuschl.metainfo.xml"
  '';
  depends = [
    "bubblewrap"
    "util-linux"
    "desktop-file-utils"
    "hicolor-icon-theme"
    "libgcc-s1"
    "libstdc++6"
    "libsqlite3-0"
    "libfontconfig1"
    "libfreetype6"
    "libxkbcommon0"
    "libxkbcommon-x11-0"
    "libwayland-client0"
    "libwayland-cursor0"
    "libvulkan1"
    "libgl1"
    "libegl1"
    "libx11-6"
    "libxcursor1"
    "libxi6"
    "libxrandr2"
    "libxcb1"
    "libasound2t64 | libasound2"
    "libpipewire-0.3-0t64 | libpipewire-0.3-0"
    "pipewire-bin"
    "libpipewire-0.3-modules"
    "libspa-0.2-modules"
    "libpulse0"
  ];
  debWith =
    revision:
    import ../debian/package.nix {
      inherit
        pkgs
        stage
        maintainer
        depends
        ;
      version = "${version}-${revision}";
    };
  deb = debWith revision;
  # The owner's repository command, and a test repository with this package
  # and its next revision for the upgrade test.
  aptRepository = import ../debian/apt-repository.nix {
    inherit pkgs;
    packages = [
      deb
      (debWith (toString (pkgs.lib.toInt revision + 1)))
    ];
  };
in
{
  inherit stage deb aptRepository;
  archBin = import ../arch/package.nix {
    inherit pkgs version stage;
    packageRevision = revision;
    sourceDateEpoch = 1;
  };
}
