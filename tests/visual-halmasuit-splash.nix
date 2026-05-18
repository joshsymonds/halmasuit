# tests/visual-halmasuit-splash.nix — epic layer C visual gate
# (Snapshot()).
#
# Boots halmasuit (halmasuit-debug, frame_audit on) and runs
# `halmasuit-splash` as a wl_client. Splash binds wlr-layer-shell
# BACKGROUND and renders the HALMASUIT_SPLASH_IMAGE PNG fullscreen via
# wgpu (GL backend over Mesa swrast → wl_shm buffers, which halmasuit
# composites). Capture is the in-process `Snapshot()` D-Bus method.
#
# The fixture is a deterministic four-colour quadrant image (see
# tests/fixtures/README.md), stretched to fill — so the golden proves
# halmasuit-splash actually textured a PNG, distinct from both the
# brand clear and the solid-colour layer client.

{
  system,
  nixpkgs,
  halmasuit,
  halmasuit-session,
  halmasuit-splash,
  ssimulacra2-cli,
}:

let
  pkgs = import nixpkgs { inherit system; };
  fixture = ./fixtures/splash-test.png;
in
pkgs.testers.runNixOSTest {
  name = "visual-halmasuit-splash";

  skipTypeCheck = true;

  interactive.nodes.machine = { ... }: {
    virtualisation.qemu.options = [
      "-device virtio-vga-gl"
      "-display gtk,gl=on"
    ];
  };

  nodes.machine =
    { pkgs, ... }:
    {
      imports = [
        ../nix/module.nix
        ./lib/test-user.nix
      ];

      services.halmasuit = {
        enable         = true;
        package        = halmasuit; # halmasuit-debug (frame_audit) via flake
        session.package   = halmasuit-session;
        greeterUid     = 999;
        greeterGroup   = "halmasuit-greeter";
        compositorUid  = 998;
        # Exercises the module option's option→unit-Environment
        # wiring (Epic #1 req 4). halmasuit sanitizes the greeter's
        # env to an allowlist, so the splash client below ALSO gets
        # the path explicitly via its wrapper — the test stays
        # deterministic regardless of the env-allowlist policy.
        splashImage    = fixture;
        # Launch halmasuit-splash AS the greeter (uid 999), with the
        # image path set in its own environment.
        greeterCommand = "${pkgs.writeShellScript "halmasuit-splash-launch" ''
          export HALMASUIT_SPLASH_IMAGE=${fixture}
          exec ${halmasuit-splash}/bin/halmasuit-splash
        ''}";
      };

      # Snapshot() writes post-privilege-drop (uid 998) under
      # ProtectSystem=strict — world-writable non-PrivateTmp /run dir
      # in ReadWritePaths. In sync with visual.py's GUEST_SNAPSHOT_DIR.
      systemd.tmpfiles.rules = [ "d /run/hsnap 0777 root root -" ];
      systemd.services.halmasuit.serviceConfig.ReadWritePaths = [ "/run/hsnap" ];

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
        memorySize = 2048; # wgpu + Mesa swrast needs more headroom
        cores      = 2;
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

    machine.wait_until_succeeds(
        "journalctl -u halmasuit | grep -qF 'scanout_active'",
        timeout=30,
    )
    # wgpu init + first present is heavier than the shm test client;
    # give it a generous window.
    machine.wait_until_succeeds(
        "journalctl -u halmasuit | grep -qF 'halmasuit-splash: presented'",
        timeout=90,
    )
    machine.wait_until_succeeds(
        "busctl --system status org.halmasuit",
        timeout=30,
    )

    introspect = machine.succeed(
        "busctl --system introspect org.halmasuit "
        "/org/halmasuit/Debug/Introspect"
    )
    assert "Snapshot" in introspect, (
        f"Snapshot method missing from Introspect interface:\n{introspect}"
    )

    visual.assert_matches_golden(machine, "halmasuit-splash-fixture")

    print("visual-halmasuit-splash: ALL ASSERTIONS PASSED")
  '';
}
