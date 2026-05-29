# Phase B: kernel-handoff-to-session pixmap continuity gate.
#
# THE Plymouth-removability proof: halmasuit owns the pixmap from
# initramfs boot all the way through SessionOpened, with no flash
# anywhere on the timeline. Asserts via the same exact-stream
# mechanism the rootfs visual-* tests already use
# (`visual.assert_no_flash_stream` over halmasuit-debug's
# `frame_rendered` event stream), extended to the Phase B timeline:
#
#   initramfs_init → drm_master_acquired → wayland_ready
#   → scanout_active → rootfs_ready → greetd_ready → deprivileged
#   → greeter scene → session scene → SessionOpened
#
# Every composited frame across the whole timeline must satisfy:
#   - wallpaper plane composited from frame 0 (G1/R3 invariant —
#     no pre-client solid phase / flash)
#   - clear_pixel_count == 0
#   - degenerate == False
#   - pixel_count constant
#
# This is the same contract as the rootfs visual-* family, applied
# to the boot-from-initrd timeline. A pass here is the strongest
# empirical statement of Phase B's Plymouth-removability claim:
# halmasuit owns the pixel pipeline continuously, no other process
# (Plymouth, systemd-cryptsetup TTY, getty, greetd-the-daemon)
# needs to or should run in parallel.

{
  system,
  nixpkgs,
  halmasuit,
  halmasuit-luks,
  halmasuit-session,
  halmasuit-vm-client,
  ssimulacra2-cli,
}:

let
  pkgs = import nixpkgs { inherit system; };
in
pkgs.testers.runNixOSTest {
  name = "visual-initrd-pixmap";

  skipTypeCheck = true;

  nodes.machine =
    { config, lib, pkgs, ... }:
    let
      testGreeter = pkgs.writeShellScript "halmasuit-test-greeter" ''
        exec ${pkgs.coreutils}/bin/sleep infinity
      '';
    in
    {
      imports = [
        ../nix/module.nix
      ];

      virtualisation = {
        memorySize = 1024;
        cores      = 1;
        diskSize   = 1024;
        qemu.options = [
          "-vga none"
          "-device virtio-gpu-pci"
        ];
      };

      # fromInitrd deployment, pointing at the frame_audit build so
      # halmasuit emits the `frame_rendered` events
      # `assert_no_flash_stream` consumes. The production
      # `halmasuit` package omits frame_audit (no arbitrary-file-write
      # surface in production); halmasuit-debug is the test substrate.
      services.halmasuit = {
        fromInitrd.enable = true;
        package           = halmasuit;
        luks.package      = halmasuit-luks;
        session.package   = halmasuit-session;
        greeterCommand    = "${testGreeter}";
        # Wallpaper plane MUST be live from frame 0 (G1/R3 — no
        # pre-client solid phase). The module's wallpaperEnv helper
        # plumbs HALMASUIT_WALLPAPER_CONFIG into both deployment
        # shapes; the asset is added to the initramfs closure via
        # boot.initrd.systemd.storePaths.
        wallpaper = {
          type   = "image";
          source = ./fixtures/splash-test.png;
        };
      };

      system.extraDependencies = [ testGreeter ];

      environment.systemPackages = [ halmasuit-vm-client ];

      users.users.alice = {
        isNormalUser = true;
        uid          = 1000;
        group        = "alice";
        password     = "testpassword";
      };
      users.groups.alice.gid = 1000;
    };

  testScript = ''
    import os
    import sys

    sys.path.insert(0, "${./lib}")
    os.environ["PATH"] = "${ssimulacra2-cli}/bin:" + os.environ.get("PATH", "")
    os.environ.setdefault("GOLDENS_DIR", "${./goldens}")

    import visual

    machine.start()
    machine.wait_for_unit("multi-user.target")

    # Wait for the post-pivot setup to land: same anchor as
    # full-boot-flash.nix uses (greeter_spawned is the last event
    # emitted from run_post_pivot_setup).
    machine.wait_until_succeeds(
        "journalctl -b --output=cat --no-pager "
        "| grep -F '\\\"event\\\":\\\"greeter_spawned\\\"'",
        timeout=30,
    )
    print("PASS: greeter spawned post-pivot")

    # Drive a real PAM auth to push the timeline through to
    # SessionOpened, matching full-boot-flash.nix's auth shape.
    machine.succeed("printf 'testpassword' > /tmp/alice.pw")
    machine.succeed("chown halmasuit-greeter:halmasuit-greeter /tmp/alice.pw")
    machine.succeed("chmod 600 /tmp/alice.pw")
    machine.succeed(
        "runuser -u halmasuit-greeter -- "
        "halmasuit-vm-client full-auth /run/halmasuit/greetd.sock alice "
        "--password-file /tmp/alice.pw "
        "--cmd /run/current-system/sw/bin/true "
        "--timeout 30"
    )
    machine.wait_until_succeeds(
        "journalctl -b --output=cat --no-pager "
        "| grep -F '\\\"event\\\":\\\"session_opened\\\"'",
        timeout=15,
    )
    print("PASS: full PAM auth → SessionOpened")

    # THE assertion: 100% of the frame_rendered stream across the
    # entire kernel-handoff-to-SessionOpened timeline checks as
    # exact facts. No flash anywhere.
    visual.assert_no_flash_stream(machine)
    print("PASS: kernel-handoff-to-session pixmap continuity")

    print(
      "visual-initrd-pixmap: halmasuit owns the full "
      "kernel-handoff-to-session pixmap — Plymouth removable"
    )
  '';
}
