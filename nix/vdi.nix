# SPDX-License-Identifier: GPL-3.0-or-later
#
# A disk somebody else can boot.
#
#   nix build .#vdi        # ./result/viewport.vdi, for VirtualBox
#
# nix/vm.nix is the same desktop for `nix run .#vm`, and is not this: it is
# stateless, it mounts the host's /nix/store through 9p, and it is booted by
# QEMU with `-kernel`. None of the three survives being handed to someone on
# another machine, so this one has a bootloader, a root filesystem and the
# whole closure on the disk.
#
# What is on it is a compositor from GitHub, fetched at login, with the build
# baked into the image as the fallback. A disk image is a thing people keep,
# and one handed out in August should not still be showing them August's
# compositor in December.
{
  config,
  lib,
  pkgs,
  ...
}:
{
  programs.viewport.enable = true;
  programs.viewport.shellBackend = "cef";
  programs.viewport.terminal = "${pkgs.foot}/bin/foot";

  # make-disk-image writes an MBR disk and labels the root filesystem; GRUB
  # goes in the MBR of whatever the hypervisor calls the first disk. VirtualBox
  # attaches a VDI to SATA by default, so this is /dev/sda in there.
  boot.loader.grub = {
    enable = true;
    device = "/dev/sda";
  };
  fileSystems."/" = {
    device = "/dev/disk/by-label/nixos";
    fsType = "ext4";
    autoResize = true;
  };
  boot.growPartition = true;

  # The DRM node the compositor needs, on the three display devices a guest is
  # likely to be given: vmwgfx is VirtualBox's VMSVGA and VMware's, virtio_gpu
  # is QEMU's, bochs is the plain VGA fallback. NixOS's initrd carries the
  # storage drivers already; the display ones it does not guess at, and without
  # one there is no /dev/dri and the compositor exits at startup.
  boot.initrd.kernelModules = [
    "vmwgfx"
    "virtio_gpu"
    "bochs"
  ];
  boot.consoleLogLevel = 7;

  # Both consoles, in that order, so the boot is legible on the screen *and*
  # readable from outside through a serial port. `nix run .#vm` gets this from
  # qemu-vm.nix and the first build of this image was made without it: the
  # window stopped somewhere in PCI enumeration and the serial log had nothing
  # in it, which is the same picture whether the kernel hung or the console
  # simply moved to a framebuffer nothing was drawing to.
  boot.kernelParams = [
    "console=tty0"
    "console=ttyS0,115200"
  ];

  # Mesa, which on VMSVGA is the svga gallium driver when 3D acceleration is
  # switched on in the VM's settings and llvmpipe when it is not. Both give the
  # compositor an EGL context; only one of them is fast.
  hardware.graphics.enable = true;

  services.getty.autologinUser = "viewport";
  users.users.viewport = {
    isNormalUser = true;
    password = "viewport";
    # seatd checks `seat` before handing over DRM master; `video` and `input`
    # are the devices themselves. `dialout` owns the serial ports, and without
    # it the compositor's log cannot be written to one — which is the only way
    # to read a failure out of a machine whose screen the compositor has taken.
    extraGroups = [
      "wheel"
      "seat"
      "video"
      "input"
      "dialout"
    ];
  };

  # tty1 starts the desktop; the other VTs stay a shell, which is where to look
  # when it does not come up.
  #
  # `--refresh` is what keeps this true on the second login as well: without it
  # the flake reference is resolved from the registry cache and a new commit is
  # not noticed for hours.
  #
  # The build in the image is the fallback, and it is the reason this is worth
  # doing at all — no network, GitHub down, or a broken commit on the default
  # branch all end with a desktop rather than a black screen. There is no
  # binary cache for this flake, so a login that does reach GitHub compiles the
  # compositor before it draws anything; its nixpkgs dependencies, CEF
  # included, come from cache.nixos.org.
  programs.bash.loginShellInit = ''
    if [ "$(tty)" = /dev/tty1 ]; then
      # The log goes to the serial port as well when there is one, because the
      # screen this is running on is about to be taken over by the compositor
      # and a failure leaves its reason on a console nobody can scroll back. A
      # VM whose serial port is switched off still has the device and the
      # writes go nowhere, which is the harmless half of this.
      targets=/var/log/viewport.log
      [ -w /dev/ttyS0 ] && targets="$targets /dev/ttyS0"
      {
        nix run --refresh github:codebam/viewport-smithay \
          || ${config.programs.viewport.package}/bin/viewport
      } 2>&1 | tee $targets
    fi
  '';

  # The log that launcher writes, given an owner before anything writes to it.
  # /var/log belongs to root and the compositor does not run as root, so
  # without this the `tee` fails with "permission denied", the desktop comes up
  # anyway, and the one file that would explain a desktop that did not come up
  # is the file that was never written.
  systemd.tmpfiles.rules = [ "f /var/log/viewport.log 0644 viewport users -" ];

  # What the compositor is going to look for, said out loud before it looks:
  # "no DRM device" and "a DRM device the compositor would not open" are the
  # same black screen, and the difference is the whole diagnosis. nix/vm.nix
  # carries the same service for the same reason.
  systemd.services.drm-devices = {
    description = "say which DRM devices exist";
    wantedBy = [ "multi-user.target" ];
    before = [ "getty@tty1.service" ];
    serviceConfig = {
      Type = "oneshot";
      RemainAfterExit = true;
      StandardOutput = "journal+console";
    };
    script = ''
      echo "DRM devices:"
      ls -l /dev/dri 2>&1 || echo "  /dev/dri does not exist"
    '';
  };

  nix.settings.experimental-features = [
    "nix-command"
    "flakes"
  ];

  # Autologin happens well before DHCP finishes, and a `nix run` that starts
  # first fails to resolve github.com and falls straight through to the
  # fallback — which works, and is never the version that was asked for.
  systemd.services."getty@tty1" = {
    after = [ "network-online.target" ];
    wants = [ "network-online.target" ];
  };

  environment.systemPackages = with pkgs; [
    foot
    wayland-utils
    mesa-demos
    htop
  ];

  # The bar's icons are Nerd Font glyphs named in data/shell/shell.css. Without
  # these every one of them is a replacement box.
  fonts.packages = with pkgs; [
    nerd-fonts.fira-code
    nerd-fonts.symbols-only
  ];

  networking.hostName = "viewport";
  networking.useDHCP = lib.mkDefault true;
  # networkd rather than the default dhcpcd, for one reason: it ships
  # `systemd-networkd-wait-online`, which is the only thing on this machine
  # that makes `network-online.target` mean anything. With dhcpcd nothing is
  # ordered before that target, so it is reached the moment it is asked for —
  # tty1 then starts while DHCP is still in flight, and the first thing the
  # desktop does is fail to resolve github.com and fall back to the build on
  # the disk. Measured, not guessed: that is what the first boot of this image
  # did, and the error it printed was
  #
  #   error: unable to download '...commits/HEAD': Could not resolve host
  #
  # on a machine whose network came up two seconds later.
  networking.useNetworkd = true;
  services.resolved.enable = true;
  # No services on this machine, and a firewall in front of none of them is
  # one more thing between somebody and a desktop that works.
  networking.firewall.enable = false;
  time.timeZone = lib.mkDefault "UTC";

  # Nothing in here runs `nixos-rebuild`, and the channel and the sources are
  # weight in a file that gets handed around.
  documentation.enable = false;
  documentation.nixos.enable = false;

  system.stateVersion = config.system.nixos.release;
}
