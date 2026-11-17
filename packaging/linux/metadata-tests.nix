# Cheap offline packaging-policy fixtures, without a Cargo dependency compile.
{ pkgs }:
let
  versionCheck = import ./version-tests.nix { inherit pkgs; };
in
pkgs.runCommand "nuschl-distro-metadata-tests"
  {
    nativeBuildInputs = [
      pkgs.python3
      pkgs.gitMinimal
      pkgs.dpkg
      pkgs.pacman
    ];
  }
  ''
    test -e ${versionCheck}
    mkdir -p packaging/linux packaging/debian
    cp ${./version.py} packaging/linux/version.py
    cp ${./version-tests.py} packaging/linux/version-tests.py
    python3 packaging/linux/version-tests.py --dpkg ${pkgs.dpkg}/bin/dpkg --vercmp ${pkgs.pacman}/bin/vercmp
    touch "$out"
  ''
