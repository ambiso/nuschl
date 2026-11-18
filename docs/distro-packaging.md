# Debian, Ubuntu and Arch packages

Nuschl publishes one `.deb` for amd64 in an APT repository at
`https://dl.nuschl.eu/apt`, and the AUR package `nuschl-bin`, which repackages
that `.deb`. [install.md](install.md#debian-and-ubuntu) shows how users install them.

## The package

`packaging/linux/default.nix` builds both from a directory of the nine Linux
executables. The stage puts them next to each other in `/usr/lib/nuschl/`, since
helper discovery looks in the executable's directory:

```
nuschl                        nuschl-storage-worker
nuschl-content-worker         nuschl-transfer-worker
nuschl-crypto-worker          nuschl-update-verify-ed25519
nuschl-math-worker            nuschl-update-verify-mldsa44
nuschl-media-worker
```

`/usr/bin/nuschl` links to the executable. The stage adds the desktop entry,
which registers `mumble://` and `nuschl://`, the hicolor icons, the AppStream
file and the license notices in `/usr/share/licenses/nuschl/`. The programs use
the distribution's glibc, Vulkan, X11, Wayland, PipeWire and ALSA libraries, and
`bwrap` and `prlimit` for the worker sandbox (SECURITY.md). The package has no
maintainer scripts and changes no system policy. The in-app updater is off on
Linux; APT and pacman update the package.

The `.deb` build checks that every program uses the system loader, has no
RUNPATH and no Nix store path, and sets `libc6 (>= …)` from the newest glibc
symbol the programs use. The version is the workspace version with revision
`1`; packages are published from release tags.

## Tests

The checks `distro-debian`, `distro-ubuntu` and `distro-arch`
(`packaging/linux/tests.nix`, running `install-test.sh`) install
the packages on clean systems: Debian 13 and Ubuntu 24.04 images from nixpkgs'
`vmTools`, and an Arch root made from one day of the Arch Linux Archive. Each
installs the package, runs `nuschl --version` with the Nix store hidden, checks
`xdg-mime query default` for both URL schemes, and renders a message with
`nuschl render` as an unprivileged user, which runs the content worker in its
sandbox; the same command fails without `bwrap` on the `PATH`. Debian and
Ubuntu install from the signed test repository with apt, upgrade to the next
revision, remove and purge. Arch installs `nuschl-bin` with `pacman -U` and
removes it.

`nix build .#distroTests.ubuntuAppArmor` boots Ubuntu 24.04's own kernel with
Ubuntu's AppArmor policy loaded and `kernel.apparmor_restrict_unprivileged_userns=1`,
Ubuntu's default since 23.10. It fails: the restriction gives bwrap's user
namespace no capabilities (`bwrap: loopback: Failed RTM_NEWADDR: Operation not
permitted`), so the workers do not start. An AppArmor profile that allows user
namespaces for `/usr/lib/nuschl/nuschl` makes it pass; shipping one needs the
owner's approval and a SECURITY.md change (#148).

Building the AUR package also checks that makepkg refuses the `.deb` with its
signature missing, with a signature by another key, and with modified contents.

## Publishing

The owner publishes from a local copy of the repository directory. The
repository and the AUR package use one Ed25519 OpenPGP key.

### The archive key (once)

```sh
gpg --quick-generate-key 'Nuschl packages <packages@nuschl.eu>' ed25519 sign 3y
fpr=$(gpg --with-colons --list-keys packages@nuschl.eu | awk -F: '$1 == "fpr" { print $10; exit }')
gpg --armor --export "$fpr" > packaging/nuschl-archive-keyring.asc
gpg --armor --export-secret-keys "$fpr" > nuschl-archive-key.secret.asc  # offline backup
```

Commit `packaging/nuschl-archive-keyring.asc` in a pull request and add the
fingerprint to install.md. The flake output `aur` exists once that file does.
Extending the key's expiry later keeps its fingerprint: `gpg --quick-set-expire "$fpr" 3y`,
then export and commit the public key again.

### A release

```sh
nix build .#deb -o nuschl.deb
nix run .#apt-repository -- ~/nuschl-apt "$fpr" nuschl.deb
rsync -a ~/nuschl-apt/pool ~/nuschl-apt/nuschl-archive-keyring.gpg HOST:/srv/dl.nuschl.eu/apt/
rsync -a ~/nuschl-apt/dists HOST:/srv/dl.nuschl.eu/apt/
```

`apt-repository` copies the `.deb` into `pool/`, writes a detached signature
`.deb.sig` for every `.deb` without one, indexes the pool with `apt-ftparchive`
for the suites `stable`, `trixie` and `noble` (same contents), and signs each
suite's `Release` (`InRelease`, `Release.gpg`) with `Valid-Until` 30 days ahead.
Uploading `pool/` first puts every file the indexes name on the server before them. `HOST`
and the path stand for the server behind dl.nuschl.eu.

Then update the AUR package:

```sh
nix build .#aur -o aur
git clone ssh://aur@aur.archlinux.org/nuschl-bin.git ~/nuschl-bin
cp -rT aur/aur ~/nuschl-bin && chmod -R u+w ~/nuschl-bin
git -C ~/nuschl-bin add -A && git -C ~/nuschl-bin commit -m "nuschl-bin <version>" && git -C ~/nuschl-bin push
```

### Re-signing

Clients reject the repository once `Valid-Until` passes. At least every 30 days,
re-sign it and upload the indexes:

```sh
nix run .#apt-repository -- ~/nuschl-apt "$fpr"
rsync -a ~/nuschl-apt/dists HOST:/srv/dl.nuschl.eu/apt/
```
