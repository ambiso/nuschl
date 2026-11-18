# nuschl-bin for the AUR: a PKGBUILD that downloads the .deb and its OpenPGP
# signature from dl.nuschl.eu, checks the sha256 and the signature by the
# archive key, and unpacks the .deb's data.tar. With the .deb's signature it
# also builds the package with makepkg and checks that makepkg refuses a
# missing signature, a signature by another key and a modified .deb.
{
  pkgs,
  version,
  deb,
  # The archive's armored public key.
  key,
  signature ? null,
  maintainer,
  # Arch package names of the libraries and tools the .deb's Depends names.
  depends,
  debianRevision ? "1",
  pkgrel ? "1",
}:
let
  name = "nuschl_${version}-${debianRevision}_amd64.deb";
  pkgbuild = pkgs.writeText "PKGBUILD" ''
    # Maintainer: ${maintainer}
    pkgname=nuschl-bin
    pkgver=${version}
    pkgrel=${pkgrel}
    _debrev=${debianRevision}
    pkgdesc='Voice and text chat for Mumble servers'
    arch=('x86_64')
    url='https://github.com/nuschl-eu/nuschl'
    license=('EUPL-1.2')
    depends=(${pkgs.lib.concatMapStringsSep " " (name: "'${name}'") depends})
    provides=('nuschl')
    conflicts=('nuschl')
    options=('!strip' '!debug')
    source=("https://dl.nuschl.eu/apt/pool/main/n/nuschl/nuschl_''${pkgver}-''${_debrev}_amd64.deb"{,.sig})
    sha256sums=('@sha256@' 'SKIP')
    validpgpkeys=('@fingerprint@')

    package() {
      bsdtar -xf data.tar.xz -C "$pkgdir"
    }
  '';
in
pkgs.runCommand "nuschl-aur-${version}"
  {
    nativeBuildInputs = [
      pkgs.pacman
      pkgs.fakeroot
      pkgs.gnupg
      pkgs.libarchive
      pkgs.zstd
      pkgs.xz
      pkgs.file
    ];
  }
  ''
    export GNUPGHOME=$TMPDIR/gnupg
    mkdir -m700 "$GNUPGHOME"
    gpg --batch --import ${key}
    fingerprint=$(gpg --with-colons --fingerprint | awk -F: '$1 == "fpr" { print $10; exit }')
    mkdir -p "$out/aur/keys/pgp"
    cp ${key} "$out/aur/keys/pgp/$fingerprint.asc"
    sed -e "s/@sha256@/$(sha256sum < ${deb} | cut -d' ' -f1)/" -e "s/@fingerprint@/$fingerprint/" \
      ${pkgbuild} > "$out/aur/PKGBUILD"
    cp ${pkgs.pacman}/etc/makepkg.conf makepkg.conf
    chmod u+w makepkg.conf
    echo "PKGEXT=.pkg.tar.zst" >> makepkg.conf
    cd "$out/aur"
    makepkg --config "$TMPDIR/makepkg.conf" --printsrcinfo > .SRCINFO
    ${pkgs.lib.optionalString (signature != null) ''
      export SRCDEST=$TMPDIR/sources BUILDDIR=$TMPDIR/build PKGDEST=$out PACKAGER=${pkgs.lib.escapeShellArg maintainer}
      mkdir "$SRCDEST" "$BUILDDIR"
      cp ${deb} "$SRCDEST/${name}"
      cp ${signature} "$SRCDEST/${name}.sig"
      verify() { makepkg --config "$TMPDIR/makepkg.conf" --verifysource --noconfirm; }
      verify
      # makepkg downloads a missing signature, and the build has no network.
      rm "$SRCDEST/${name}.sig"
      if verify; then echo "makepkg accepted a missing signature"; exit 1; fi
      gpg --batch --passphrase "" --quick-generate-key 'Other <other@nuschl.invalid>' ed25519 sign never
      gpg --batch --local-user Other --detach-sign --output "$SRCDEST/${name}.sig" "$SRCDEST/${name}"
      if verify; then echo "makepkg accepted a signature by another key"; exit 1; fi
      cp ${signature} "$SRCDEST/${name}.sig"
      cp PKGBUILD "$TMPDIR/PKGBUILD"
      chmod u+w "$SRCDEST/${name}"
      echo modified >> "$SRCDEST/${name}"
      sed -i "s/^sha256sums=('[0-9a-f]*'/sha256sums=('$(sha256sum < "$SRCDEST/${name}" | cut -d' ' -f1)'/" PKGBUILD
      if verify; then echo "makepkg accepted a modified .deb"; exit 1; fi
      cp "$TMPDIR/PKGBUILD" PKGBUILD
      cp ${deb} "$SRCDEST/${name}"
      # Nix is no Arch installation, so makepkg skips the dependency check here.
      makepkg --config "$TMPDIR/makepkg.conf" --nodeps --noconfirm
    ''}
  ''
