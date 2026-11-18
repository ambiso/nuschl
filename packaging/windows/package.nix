# SPDX-License-Identifier: EUPL-1.2
# The per-user installer, nuschl-<version>-windows-x86_64-setup.exe, holding
# the files of the portable zip: the programs and licenses/ (nuschl.nsi).
{
  pkgs,
  windows,
  version,
}:
pkgs.runCommand "nuschl-${version}-windows-x86_64-setup" { nativeBuildInputs = [ pkgs.nsis ]; } ''
  cp -R ${windows}/bin payload
  chmod u+w payload
  cp -R ${windows}/share/licenses/nuschl payload/licenses
  mkdir "$out"
  makensis -WX -V2 -DVERSION=${version} -DPAYLOAD="$PWD/payload" \
    -DICON=${../../apps/desktop/assets/nuschl.ico} \
    -DOUTFILE="$out/nuschl-${version}-windows-x86_64-setup.exe" ${./nuschl.nsi}
''
