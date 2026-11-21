# The owner's repository command, and a test repository of the given .deb files
# signed with a key generated in the build.
{ pkgs, packages }:
rec {
  command = pkgs.writeShellApplication {
    name = "apt-repository";
    runtimeInputs = [
      pkgs.apt
      pkgs.diffutils
      pkgs.dpkg
      pkgs.gnupg
      pkgs.gzip
    ];
    text = builtins.readFile ./apt-repository.sh;
  };
  test = pkgs.runCommand "nuschl-apt-test" { nativeBuildInputs = [ pkgs.gnupg ]; } ''
    export GNUPGHOME=$TMPDIR/gnupg
    mkdir -m700 "$GNUPGHOME"
    gpg --batch --passphrase "" --quick-generate-key 'Nuschl test <test@nuschl.invalid>' ed25519 sign never
    ${command}/bin/apt-repository "$out" 'Nuschl test' ${pkgs.lib.escapeShellArgs packages}
    grep -Fx 'Acquire-By-Hash: yes' "$out/dists/stable/Release"
    indices=$out/dists/stable/main/binary-amd64
    digest=$(sha512sum "$indices/Packages.gz")
    test -f "$indices/by-hash/SHA512/''${digest%% *}"
    cp ${pkgs.lib.escapeShellArg (toString (builtins.head packages))} "$TMPDIR/changed.deb"
    chmod u+w "$TMPDIR/changed.deb"
    # Change the timestamp of the first ar member and keep the package metadata.
    printf '1234567890  ' | dd of="$TMPDIR/changed.deb" bs=1 seek=24 conv=notrunc status=none
    if ${command}/bin/apt-repository "$out" 'Nuschl test' "$TMPDIR/changed.deb" 2>"$TMPDIR/refused"; then
      echo 'APT accepted different bytes for an existing version' >&2
      exit 1
    fi
    cat "$TMPDIR/refused"
    grep -qF 'APT package version already exists with different bytes' "$TMPDIR/refused"
    gpg --armor --export > "$out/key.asc"
    gpg --with-colons --fingerprint | awk -F: '$1 == "fpr" { print $10; exit }' > "$out/fingerprint"
  '';
}
