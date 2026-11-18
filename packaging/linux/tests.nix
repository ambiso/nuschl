# Clean-system tests of the packages. Debian 13 and Ubuntu 24.04 install the
# .deb from the signed test repository with apt, upgrade, remove and purge it.
# Arch installs nuschl-bin with pacman into a root made from packages of the
# Arch Linux Archive, and removes it. ubuntuAppArmor repeats the sandbox check
# on Ubuntu's kernel with Ubuntu's AppArmor policy.
{
  pkgs,
  aptRepository,
  aur,
  archDepends,
  version,
  revision,
  depends,
}:
let
  inherit (pkgs) lib vmTools;
  image =
    distro:
    vmTools.diskImageFuns.${distro} {
      size = 8192;
      # The package's dependencies come preinstalled, the test repository holds only Nuschl.
      extraPackages =
        lib.optionals (distro == "ubuntu2404x86_64") ubuntuKernel
        ++ map (dependency: lib.head (lib.splitString " " dependency)) depends
        ++ [
          "apt"
          "xdg-utils"
        ];
    };
  # Ubuntu's kernel restricts unprivileged user namespaces through AppArmor.
  # vmTools boots the nixpkgs kernel, so the AppArmor test boots Ubuntu's.
  ubuntuKernel = [
    "linux-image-virtual"
    "initramfs-tools"
    "apparmor"
  ];
  ubuntuInstalled = vmTools.runInLinuxImage (
    pkgs.runCommand "nuschl-ubuntu-installed"
      {
        diskImage = image "ubuntu2404x86_64";
        memSize = 4096;
        postVM = ''mv disk-image.qcow2 "$out/"'';
      }
      ''
        env -i PATH=$PATH sh ${./install-test.sh} apt-install ${aptRepository.test} noble
        install -m755 ${./apparmor-test.sh} /apparmor-test
        install -m644 ${./install-test.sh} /install-test.sh
        mkdir "$out"
        cp /boot/vmlinuz-* "$out/vmlinuz"
        cp /boot/initrd.img-* "$out/initrd"
      ''
  );
  ubuntuAppArmor =
    pkgs.runCommand "nuschl-ubuntu-apparmor"
      {
        nativeBuildInputs = [ pkgs.qemu_kvm ];
        requiredSystemFeatures = [ "kvm" ];
      }
      ''
        qemu-img create -f qcow2 -F qcow2 -b ${ubuntuInstalled}/disk-image.qcow2 disk.qcow2
        timeout 900 qemu-kvm -m 4096 -smp 2 -nographic -no-reboot -drive file=disk.qcow2,if=virtio \
          -kernel ${ubuntuInstalled}/vmlinuz -initrd ${ubuntuInstalled}/initrd \
          -append "root=/dev/vda rw console=ttyS0 init=/apparmor-test panic=-1" | tee log
        grep -q "apparmor test passed" log
        touch "$out"
      '';
  apt =
    distro: suite:
    vmTools.runInLinuxImage (
      pkgs.runCommand "nuschl-install-${distro}"
        {
          diskImage = image distro;
          memSize = 4096;
        }
        ''
          env -i PATH=$PATH sh ${./install-test.sh} apt ${aptRepository.test} ${suite} ${version}-${revision} \
            ${version}-${toString (lib.toInt revision + 1)}
          touch "$out"
        ''
    );
  # nuschl-bin's dependencies and xdg-utils from one day of the archive; the
  # output hash pins the packages.
  archPackages =
    pkgs.runCommand "nuschl-arch-packages"
      {
        nativeBuildInputs = [
          pkgs.pacman
          pkgs.fakeroot
        ];
        NIX_SSL_CERT_FILE = "${pkgs.cacert}/etc/ssl/certs/ca-bundle.crt";
        outputHashMode = "recursive";
        outputHash = "sha256-40L+RLLTi1N11KhVMy2vwhPj424jtf/iKTTEwy3c4XM=";
      }
      ''
        cat > pacman.conf <<'EOF'
        [options]
        Architecture = x86_64
        SigLevel = Never
        [core]
        Server = https://archive.archlinux.org/repos/2026/11/15/$repo/os/$arch
        [extra]
        Server = https://archive.archlinux.org/repos/2026/11/15/$repo/os/$arch
        EOF
        mkdir -p db root "$out"
        fakeroot pacman --config pacman.conf --dbpath db --root root --cachedir "$out" --logfile pacman.log \
          --sync --refresh --downloadonly --noconfirm base xdg-utils \
          ${lib.escapeShellArgs archDepends}
      '';
  # A disk image: the kernel refuses user namespaces in a chroot.
  archImage = vmTools.runInLinuxVM (
    pkgs.runCommand "arch-image"
      {
        nativeBuildInputs = [ pkgs.pacman ];
        preVM = vmTools.createEmptyImage {
          size = 4096;
          fullName = "Arch Linux";
        };
        memSize = 2048;
      }
      ''
        ${vmTools.defaultCreateRootFS}
        # pacman's hooks run in the Arch root with this environment.
        export PATH=$PATH:/usr/bin
        unset XDG_DATA_DIRS
        mkdir -p /mnt/var/lib/pacman
        printf '%s\n' '[options]' 'Architecture = x86_64' 'SigLevel = Never' > pacman.conf
        pacman --config pacman.conf --root /mnt --dbpath /mnt/var/lib/pacman --upgrade --noconfirm \
          ${archPackages}/*.pkg.tar.zst
        rm /mnt/.debug
      ''
  );
  arch = vmTools.runInLinuxImage (
    pkgs.runCommand "nuschl-install-arch"
      {
        diskImage = archImage;
        memSize = 4096;
      }
      ''
        env -i PATH=$PATH sh ${./install-test.sh} pacman ${aur}/nuschl-bin-*.pkg.tar.zst
        touch "$out"
      ''
  );
in
{
  inherit arch ubuntuAppArmor;
  debian = apt "debian13x86_64" "stable";
  ubuntu = apt "ubuntu2404x86_64" "noble";
}
