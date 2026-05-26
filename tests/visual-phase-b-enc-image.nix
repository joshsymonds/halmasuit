# tests/visual-phase-b-enc-image.nix — Phase B golden-boot,
# encrypted-root LUKS × image wallpaper. Epic #35 cell (enc, image).
#
# Same end-to-end claim as visual-phase-b-side-image.nix, but the
# rootfs ITSELF is on a LUKS volume rather than a side data volume:
# halmasuit-luks's production wire unlocks /dev/vdb → switch_root
# mounts /dev/mapper/cryptroot as / → halmasuit's post-pivot chroot
# lands in the LUKS-rooted rootfs view.
#
# Dual-boot specialisation pattern (per
# nixpkgs/nixos/tests/systemd-initrd-luks-password.nix):
#   1. First boot — default config, no LUKS rootfs declared. Plain
#      qcow2 root. testScript luksFormats /dev/vdb, switches the
#      bootloader default to the `cryptroot` specialisation entry,
#      crashes.
#   2. Second boot — `cryptroot` specialisation activates:
#      `boot.initrd.luks.devices.cryptroot.device = "/dev/vdb"` +
#      `virtualisation.rootDevice = "/dev/mapper/cryptroot"` +
#      `autoFormat = true`. systemd-cryptsetup asks for the
#      passphrase; halmasuit-luks responds via the SAME wire as
#      side-volume; cryptroot unlocks; ext4 is auto-created;
#      multi-user.target reaches with `/` mounted from
#      /dev/mapper/cryptroot.
#
# testScript body shared via tests/lib/phase_b_testscript.py.

{
  system,
  nixpkgs,
  nix-config,
  halmasuit-debug,
  halmasuit-luks,
  halmasuit-session,
  halmasuit-vm-client,
  ssimulacra2-cli,
}:

let
  pkgs = import nixpkgs {
    inherit system;
    config.allowUnfree = true;
  };
in
pkgs.testers.runNixOSTest {
  name = "visual-phase-b-enc-image";

  skipTypeCheck = true;

  interactive.nodes.machine = { ... }: {
    virtualisation.qemu.options = [
      "-device virtio-vga-gl"
      "-display gtk,gl=on"
    ];
  };

  nodes.machine = {
    imports = [
      ../nix/module.nix
      (import ./lib/phase-b-golden.nix {
        wallpaper = {
          type = "image";
          source = ./fixtures/wallpaper.png;
        };
        lukshape = "encrypted-root";
        inherit halmasuit-debug halmasuit-luks halmasuit-session
                halmasuit-vm-client nix-config;
        wallpaperStorePaths = [ ./fixtures/wallpaper.png ];
      })
    ];
  };

  testScript = ''
    import os
    import sys

    sys.path.insert(0, "${./lib}")
    os.environ["PATH"] = "${ssimulacra2-cli}/bin:" + os.environ.get("PATH", "")
    os.environ.setdefault("GOLDENS_DIR", "${./goldens}")

    import visual
    import phase_b_testscript

    phase_b_testscript.run(
        machine, visual,
        cell_name="phase-b-enc-image",
        lukshape="encrypted-root",
        no_flash_mode="strict",
    )
  '';
}
