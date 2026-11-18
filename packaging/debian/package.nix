# The .deb: the stage plus a control file. It has no maintainer scripts; the
# dpkg triggers of desktop-file-utils and hicolor-icon-theme refresh the caches.
{
  pkgs,
  stage,
  version,
  maintainer,
  # The libraries the programs load, also at run time, and the sandbox tools.
  depends,
}:
pkgs.runCommand "nuschl_${version}_amd64.deb"
  {
    nativeBuildInputs = [
      pkgs.dpkg
      pkgs.binutils
    ];
  }
  ''
    cp -R ${stage} root
    chmod -R u+w root
    for program in root/usr/lib/nuschl/*; do
      readelf -lW "$program" | grep -q 'interpreter: /lib64/ld-linux-x86-64.so.2]'
      if readelf -dW "$program" | grep -E 'RPATH|RUNPATH|/nix/store'; then exit 1; fi
    done
    glibc=$(readelf -VW root/usr/lib/nuschl/* | grep -o 'GLIBC_2\.[0-9.]*' | sort -uV | tail -n 1)
    mkdir root/DEBIAN
    cat > root/DEBIAN/control <<EOF
    Package: nuschl
    Version: ${version}
    Architecture: amd64
    Maintainer: ${maintainer}
    Installed-Size: $(du -sk root | cut -f1)
    Depends: libc6 (>= ''${glibc#GLIBC_}), ${pkgs.lib.concatStringsSep ", " depends}
    Section: net
    Priority: optional
    Homepage: https://github.com/nuschl-eu/nuschl
    Description: Voice and text chat for Mumble servers
     Desktop client with sandboxed helpers for content, cryptography, storage and media.
    EOF
    dpkg-deb --root-owner-group -Zxz --build root "$out"
  ''
