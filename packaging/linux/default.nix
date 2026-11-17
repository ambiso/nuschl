# Debian and Arch packages of the Linux desktop. Versions come from the
# workspace version; packages are published from release tags.
{
  pkgs,
  toolchain,
  sourceInfo,
  nativeNotices,
  desktopFiles,
  appstreamXML,
  revision ? "1",
  maintainer ? "Local package validation <nobody@example.invalid>",
}:
let
  projectRoot = sourceInfo.outPath;
  version =
    (builtins.fromTOML (builtins.readFile (projectRoot + "/Cargo.toml"))).workspace.package.version;
  portable = import ../../nix/portable-linux.nix {
    inherit
      pkgs
      toolchain
      projectRoot
      nativeNotices
      desktopFiles
      appstreamXML
      version
      ;
  };
  deb = import ../debian/package.nix {
    inherit pkgs projectRoot maintainer;
    stage = portable.stage;
    version = "${version}-${revision}";
    sourceDateEpoch = 1;
  };
in
{
  inherit deb;
  nativeStage = portable.stage;
  archBin = import ../arch/package.nix {
    inherit pkgs version;
    stage = portable.stage;
    packageRevision = revision;
    sourceDateEpoch = 1;
  };
  aptRepository = import ../debian/apt-repository.nix {
    inherit pkgs;
    packages = [ (deb + "/nuschl_${version}-${revision}_amd64.deb") ];
    suite = "trixie";
    publicationEpoch = 1;
  };
}
