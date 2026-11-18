# The owner's repository command, and a test repository of the given .deb files
# signed with a key generated in the build.
{ pkgs, packages }:
rec {
  command = pkgs.writeShellApplication {
    name = "apt-repository";
    runtimeInputs = [
      pkgs.apt
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
    gpg --armor --export > "$out/key.asc"
    gpg --with-colons --fingerprint | awk -F: '$1 == "fpr" { print $10; exit }' > "$out/fingerprint"
  '';
}
