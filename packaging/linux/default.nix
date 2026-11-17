# Pure public builder entrypoint. The coordinator owns flake output registration.
{
  pkgs,
  toolchain,
  sourceInfo,
  versionMetadata ? null,
  nativeNotices,
  desktopFiles,
  appstreamXML,
  maintainer ? "Local package validation <nobody@example.invalid>",
}:
let
  inherit (pkgs) lib;
  projectRoot = sourceInfo.outPath;
  baseVersion =
    (builtins.fromTOML (builtins.readFile (projectRoot + "/Cargo.toml"))).workspace.package.version;
  versions = import ./version.nix {
    inherit baseVersion sourceInfo;
    metadata = versionMetadata;
  };
  portable = import ../../nix/portable-linux.nix {
    inherit
      pkgs
      toolchain
      projectRoot
      nativeNotices
      desktopFiles
      appstreamXML
      ;
    version = versions.debianUpstream;
  };
  deb = import ../debian/package.nix {
    inherit pkgs projectRoot maintainer;
    stage = portable.stage;
    version = versions.debianVersion;
    sourceDateEpoch = versions.sourceDateEpoch;
  };
  archBin = import ../arch/package.nix {
    inherit pkgs;
    stage = portable.stage;
    version = versions.archPkgver;
    packageRevision = versions.archPkgrel;
    sourceDateEpoch = versions.sourceDateEpoch;
  };
in
{
  inherit
    versions
    deb
    archBin
    ;
  nativeStage = portable.stage;
  metadataTests = import ./metadata-tests.nix { inherit pkgs; };
}
// lib.optionalAttrs versions.officialVersion {
  aptRepository = import ../debian/apt-repository.nix {
    inherit pkgs;
    packages = [ (deb + "/nuschl_${versions.debianVersion}_amd64.deb") ];
    suite = "trixie";
    publicationEpoch = versions.sourceDateEpoch;
  };
}
