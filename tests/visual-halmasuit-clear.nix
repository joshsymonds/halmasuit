# tests/visual-halmasuit-clear.nix — first visual gate against the real
# halmasuit binary.
#
# Proves that halmasuit's DRM backend (the subtask-B.1 slice: dumb buffer
# + mode-set + SETCRTC) actually drives the display. Before this test
# halmasuit only HELD the master designation as a token, painting nothing;
# any client connection had to be the first thing to put pixels on screen.
# This test pins the precondition: halmasuit alone (with no client) shows
# the brand color #0a0014.
#
# Captured via QMP screendump on virtio-gpu-pci — the dumb-buffer path
# populates QEMU's display console without needing virtio-vga-gl or
# egl-headless (validated by tests/visual-standin.nix using the same
# substrate). When B.2 (GLES + DrmCompositor) lands, this test will need
# to swap to a GL-capable virtio device because the renderer changes;
# the assertion (#0a0014 visible before any client commits) stays.
#
# Greeter is `sleep infinity` — same as halmasuit-vm.nix's test
# greeter. We never actually drive auth here; the test waits for
# halmasuit's `scanout_active` event in the journal, screenshots,
# compares to the golden.

{
  system,
  nixpkgs,
  halmasuit,
  halmasuit-spawn,
  ssimulacra2-cli,
}:

let
  pkgs = import nixpkgs { inherit system; };
in
pkgs.testers.runNixOSTest {
  name = "visual-halmasuit-clear";

  # The testScript imports the visual helper module via sys.path
  # injection at runtime, which the upstream nixos-test-driver type
  # checker can't trace statically.
  skipTypeCheck = true;

  interactive.nodes.machine = { ... }: {
    virtualisation.qemu.options = [
      "-device virtio-vga-gl"
      "-display gtk,gl=on"
    ];
  };

  nodes.machine =
    { pkgs, lib, ... }:
    {
      imports = [
        ../nix/module.nix
        ./lib/test-user.nix
      ];

      services.halmasuit = {
        enable         = true;
        package        = halmasuit;
        spawnPackage   = halmasuit-spawn;
        greeterUid     = 999;
        greeterGroup   = "halmasuit-greeter";
        compositorUid  = 998;
        # No greeter activity needed — we test halmasuit's
        # standalone clear-color paint, not greeter flow.
        greeterCommand = "${pkgs.writeShellScript "halmasuit-clear-test-greeter" ''
          exec ${pkgs.coreutils}/bin/sleep infinity
        ''}";
      };

      users.users.halmasuit-greeter = {
        isSystemUser = true;
        uid          = 999;
        group        = "halmasuit-greeter";
      };
      users.groups.halmasuit-greeter.gid = 999;

      users.users.halmasuit-compositor = {
        isSystemUser = true;
        uid          = 998;
        group        = "halmasuit-greeter";
      };

      # halmasuit's GLES renderer needs Mesa + EGL inside the guest;
      # `hardware.graphics.enable` sets up /run/opengl-driver and
      # installs Mesa's DRI drivers. `LIBGL_ALWAYS_SOFTWARE=1` then
      # forces llvmpipe — deterministic CPU rendering, no host EGL
      # backend required, no /dev/dri/renderD128 host pass-through
      # required. Goldens stay stable until `nix flake update` shifts
      # the Mesa pin.
      #
      # `LD_LIBRARY_PATH` is required because smithay uses libloading
      # to dlopen `libEGL.so.1` at runtime; without /run/opengl-driver/lib
      # on the search path the dlopen fails (NixOS's libglvnd lives
      # there, not in /etc/ld.so.cache).
      hardware.graphics.enable = true;
      # libglvnd provides libEGL.so.1; Mesa provides the swrast DRI
      # driver. Both surface at /run/opengl-driver/lib via NixOS's
      # opengl-driver wrapper. Installing them in systemPackages
      # ensures they make it into the closure; the env vars route
      # halmasuit's runtime dlopens at them.
      environment.systemPackages = [ pkgs.libglvnd pkgs.mesa ];
      systemd.services.halmasuit.environment = {
        LIBGL_ALWAYS_SOFTWARE = "1";
        LD_LIBRARY_PATH       = "/run/opengl-driver/lib";
      };

      virtualisation = {
        memorySize = 1024;
        cores      = 1;
        diskSize   = 1024;
        qemu.options = [
          "-vga none"
          "-device virtio-gpu-pci"
        ];
      };
    };

  testScript = ''
    import os
    import sys

    sys.path.insert(0, "${./lib}")
    os.environ["PATH"] = "${ssimulacra2-cli}/bin:" + os.environ.get("PATH", "")
    os.environ["GOLDENS_DIR"] = "${./goldens}"

    import visual

    machine.start()
    machine.wait_for_unit("multi-user.target")
    machine.wait_for_unit("halmasuit.service")

    # Wait for halmasuit to emit scanout_active before screenshotting.
    # That event fires after SETCRTC succeeds, so the brand clear
    # color is on the display by the time we sample.
    machine.wait_until_succeeds(
        "journalctl -u halmasuit | grep -qF 'scanout_active'",
        timeout=30,
    )

    # Give QEMU's display layer a beat to settle on the painted frame.
    import time
    time.sleep(0.5)

    visual.assert_matches_golden(machine, "halmasuit-clear-color")

    print("visual-halmasuit-clear: ALL ASSERTIONS PASSED")
  '';
}
