# tests/visual-halmasuit-layer.nix — B.3 visual gate.
#
# Boots halmasuit, then runs `halmasuit-layer-shell-test-client` as a
# wl_client of halmasuit. The client binds `wlr-layer-shell` BACKGROUND
# with a 1280×800 solid-green (`#16C44E`) shm buffer; halmasuit's
# commit handler imports the buffer, composes it into the next frame,
# and queues a page-flip. QMP screendump captures the result;
# ssimulacra2 compares to the golden.
#
# The new golden differs from `halmasuit-clear-color.png` (uniform
# brand purple) — confirming halmasuit is actually compositing the
# client and not just falling back to the clear-only path.

{
  system,
  nixpkgs,
  halmasuit,
  halmasuit-spawn,
  halmasuit-layer-shell-test-client,
  ssimulacra2-cli,
}:

let
  pkgs = import nixpkgs { inherit system; };
in
pkgs.testers.runNixOSTest {
  name = "visual-halmasuit-layer";

  skipTypeCheck = true;

  interactive.nodes.machine = { ... }: {
    virtualisation.qemu.options = [
      "-device virtio-vga-gl"
      "-display gtk,gl=on"
    ];
  };

  nodes.machine =
    { ... }:
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
        # Launch the layer-shell test client AS the greeter (uid 999).
        # It connects to halmasuit's wayland socket, binds layer-shell
        # BACKGROUND, paints a solid green buffer, holds.
        greeterCommand = "${halmasuit-layer-shell-test-client}/bin/halmasuit-layer-shell-test-client";
      };

      # Greeter system user needs access to the wayland socket. The
      # module's `SupplementaryGroups = ["shadow"]` + the socket's
      # 0660 mode + greeter-group ownership handle that; the greeter
      # also needs XDG_RUNTIME_DIR pointing at /run/halmasuit so its
      # wayland-client discovery picks up the socket.
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

    # Wait for halmasuit to reach scanout_active (it's painting #0a0014
    # standalone). Then wait for the test client to paint — eprintln
    # `layer-shell-test-client: painted 1280x800` lands in the unit's
    # journal because the greeter inherits halmasuit's stdio.
    machine.wait_until_succeeds(
        "journalctl -u halmasuit | grep -qF 'scanout_active'",
        timeout=30,
    )
    machine.wait_until_succeeds(
        "journalctl -u halmasuit | grep -qF 'layer-shell-test-client: painted'",
        timeout=30,
    )

    # Halmasuit's commit handler renders synchronously on every client
    # commit. Give QEMU's display layer a beat for the resulting page-
    # flip to land in QMP's screendump buffer.
    import time
    time.sleep(0.5)

    visual.assert_matches_golden(machine, "halmasuit-layer-green")

    print("visual-halmasuit-layer: ALL ASSERTIONS PASSED")
  '';
}
