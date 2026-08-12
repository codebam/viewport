# SPDX-License-Identifier: GPL-3.0-or-later
#
# A whole desktop in a window, with a way out.
#
#   nix run .#vm
#
# Running this compositor to try it has meant one of two things: nested under
# another compositor, which is not how it runs for real — it never takes DRM
# master, never drives a mode, and every chord it binds is a chord the host saw
# first — or from a TTY, which is how it runs for real and takes the screen
# with it. There was no third option, and three times during development the
# second one was chosen by accident.
#
# This is the third option. The compositor inside boots on a virtual GPU and
# takes DRM master for real, because there is a real DRM device in there and
# nothing else is using it. What it takes is a window on the machine outside,
# and QEMU's own Ctrl+Alt+G gives the keyboard back — which is the same chord
# the nested-capture path uses, for the same reason, and is why it was chosen
# there.
#
# Stateless on purpose: `diskImage = null` roots the VM on a tmpfs and mounts
# the host's /nix/store through, so a run leaves nothing behind and starting
# again is the same fresh desktop rather than the last one's leftovers. It also
# means the whole thing boots without copying an engine into a disk image.
{
  config,
  lib,
  pkgs,
  modulesPath,
  ...
}:
{
  imports = [ "${modulesPath}/virtualisation/qemu-vm.nix" ];

  programs.viewport.enable = true;
  # Whatever the flake recommends, so this tries the thing people install
  # rather than a backend chosen here to make the VM look good. Not named at
  # all, so it follows `programs.viewport.shellBackend`'s own default — which
  # is `servoshell` — and moves when that moves. Any other is one line:
  #
  #     programs.viewport.shellBackend = "cef";

  virtualisation = {
    memorySize = 6144; # An engine, a compositor, and a root filesystem in RAM.
    cores = 4;
    diskImage = null;
    # A GPU with a DRM node, which is what the compositor needs to exist at
    # all: it enumerates DRM devices through udev and there is nothing to find
    # behind QEMU's default VGA. `-vga none` because QEMU adds that default
    # regardless, and two display devices means the kernel picks the wrong one.
    #
    # No Vulkan device in here owns that GPU: Venus is not wired up, and the
    # only ICD that loads is lavapipe, which owns no DRM node at all. So the
    # compositor takes GLES on the GPU that does own the display — the path
    # every virtual machine will take, and one that had never actually been
    # run until this VM existed. See the renderer choice in udev.rs.
    qemu.options = [
      "-vga none"
      # OpenGL through virgl, which is the host's GPU: the compositor's EGL
      # comes up on /dev/dri/card0 and draws with it, and so does anything in
      # here that draws with GL. Nothing on the display path is software.
      #
      # Vulkan is the part a guest does not get, and it is not for want of
      # asking. `-device virtio-gpu-gl-pci,venus=on,blob=on,hostmem=4G` does
      # hand the guest a real Vulkan device — "Virtio-GPU Venus (AMD Radeon RX
      # 7900 XTX (RADV NAVI31))", the host's own card — and every output then
      # fails to initialise on it:
      #
      #   Virtual-1: could not initialise: Virtio-GPU Venus (...) does not
      #   support DrmFourcc(AR24) with modifier 0xffffffffffffff
      #
      # which is `DRM_FORMAT_MOD_INVALID`, the implicit modifier the virtio
      # plane advertises. So Venus buys a Vulkan device that cannot drive the
      # display, and the compositor draws with OpenGL either way — it tries
      # Vulkan, every output refuses, and it rebuilds the renderer and comes up.
      # That path is worth having and was tested by turning this on; it is not
      # worth 4G of host memory on every run, so the plain device is what stays.
      "-device virtio-vga-gl"
      "-display gtk,gl=on,show-cursor=on"
      # The kernel is told to log to both `tty0` and `ttyS0`, and without this
      # the serial half goes to a QEMU tab nobody opens. On the terminal that
      # started the VM it is the whole boot and then the compositor's own
      # output — which is the difference between "the window is black" and a
      # line saying which device it failed to open.
      "-serial stdio"
    ];
  };

  # Without this the QEMU window says "Display output is not active" and stays
  # black through the whole boot. A NixOS VM is booted with `-kernel` and there
  # is no BIOS behind it, so nothing sets up a framebuffer before Linux does:
  # there is no vesafb to inherit and no simple-framebuffer handover, and the
  # display has no scanout until `virtio_gpu` binds the device. Loading it from
  # the initrd is what makes the boot visible — and it is the same module that
  # creates the DRM node this compositor needs to start at all, so without it
  # the run fails twice over and only complains about the second one.
  boot.initrd.kernelModules = [ "virtio_gpu" ];

  # NixOS boots a VM at `loglevel=4`, which is errors and worse. Every line
  # about whether the GPU bound, which DRM node it made and what the driver
  # thought of the host is `KERN_INFO` and is dropped — so a VM with no display
  # produces a boot log with nothing in it about displays.
  boot.consoleLogLevel = 7;

  # What the compositor is going to look for, said out loud before it looks.
  # "no DRM device" and "a DRM device the compositor would not open" are the
  # same black window, and the difference is the whole diagnosis.
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

  # Straight to the desktop. A login prompt would be one more thing between
  # starting this and seeing whether the compositor came up, and there is
  # nothing in here worth protecting.
  services.getty.autologinUser = "viewport";
  users.users.viewport = {
    isNormalUser = true;
    password = "viewport";
    # `seat` is what seatd checks before handing over DRM master; `video` and
    # `input` are the devices themselves. Missing any of the three shows up as
    # the compositor exiting at startup rather than as a permissions message.
    extraGroups = [
      "wheel"
      "seat"
      "video"
      "input"
    ];
  };

  # On tty1 only: the other VTs stay a shell, which is where to look when the
  # compositor does not come up, and is the reason this is a login-shell hook
  # rather than a systemd service that would restart into the same failure.
  # /tmp/xchg is the directory QEMU shares with whoever started the VM, so the
  # log lands outside as well as in. Inside, a compositor that fails leaves its
  # reason on a screen that has already gone black; the copy in the share is
  # readable from the host while the VM is still running and after it is gone.
  programs.bash.loginShellInit = ''
    if [ "$(tty)" = /dev/tty1 ]; then
      # How fast the shell is painting, once a second. The question a test VM
      # is always asking is "is the desktop actually being drawn", and without
      # this the log has a first-frame line and then nothing — a shell painting
      # four frames a second and one painting none look identical.
      export VIEWPORT_SHELL_RATE=1
      exec ${config.programs.viewport.package}/bin/viewport 2>&1 \
        | tee /tmp/xchg/viewport.log
    fi
  '';

  # Enough of a desktop for the shell to have something to lay out. A
  # compositor with no windows in it looks the same whether it is working or
  # only painting a wallpaper.
  environment.systemPackages = with pkgs; [
    foot
    wayland-utils
    vulkan-tools
    mesa-demos
    htop
  ];
  programs.viewport.terminal = lib.mkDefault "${pkgs.foot}/bin/foot";

  # The bar draws its icons in a Nerd Font, and data/shell/shell.css names the
  # families as fontconfig reports them: "FiraCode Nerd Font" first, "Symbols
  # Nerd Font" as the fallback that carries the glyphs without the monospace
  # face. Without them the bar lays out correctly and every icon in it is a
  # replacement box — which is a working desktop and a useless screenshot, and
  # this VM exists to be looked at.
  fonts.packages = with pkgs; [
    nerd-fonts.fira-code
    nerd-fonts.symbols-only
  ];

  networking.hostName = "viewport-vm";
  system.stateVersion = lib.mkDefault config.system.nixos.release;
}
