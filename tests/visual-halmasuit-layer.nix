# tests/visual-halmasuit-layer.nix — B.3 visual gate (Snapshot()).
#
# Boots halmasuit (halmasuit-debug, frame_audit on), then runs
# `halmasuit-layer-shell-test-client` as a wl_client. The client binds
# wlr-layer-shell BACKGROUND with a fullscreen solid-green (`#16C44E`)
# shm buffer; halmasuit imports it, composites it, and scans it out.
# Capture is the in-process `Snapshot()` D-Bus method (a CPU readback
# of the exact composited frame), NOT a QMP screendump.
#
# No `wallpaper option` is configured here — this gate isolates the
# layer-shell compositing path (the legacy clear-only base, preserved
# for non-wallpaper mechanism tests). The golden is uniform green,
# proving halmasuit actually composites the client.

{
  system,
  nixpkgs,
  halmasuit,
  halmasuit-session,
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
        package        = halmasuit; # halmasuit-debug (frame_audit) via flake
        session.package   = halmasuit-session;
        greeterUid     = 999;
        greeterGroup   = "halmasuit-greeter";
        compositorUid  = 998;
        # Launch the layer-shell test client AS the greeter (uid 999).
        # It connects to halmasuit's wayland socket, binds layer-shell
        # BACKGROUND, paints a solid green buffer, holds.
        greeterCommand = "${halmasuit-layer-shell-test-client}/bin/halmasuit-layer-shell-test-client";
      };

      # Snapshot() writes its PNG post-privilege-drop (uid 998) under
      # ProtectSystem=strict — a world-writable, non-PrivateTmp /run
      # dir added to ReadWritePaths. In sync with visual.py's
      # GUEST_SNAPSHOT_DIR.
      systemd.tmpfiles.rules = [ "d /run/hsnap 0777 root root -" ];
      systemd.services.halmasuit.serviceConfig.ReadWritePaths = [ "/run/hsnap" ];

      # Greeter system user needs access to the wayland socket. The
      # module's `SupplementaryGroups`/0660 socket + greeter-group
      # ownership handle that.
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
    os.environ.setdefault("GOLDENS_DIR", "${./goldens}")

    import visual

    machine.start()
    machine.wait_for_unit("multi-user.target")
    machine.wait_for_unit("halmasuit.service")

    # Wait for halmasuit scanout, then the test client to paint, then
    # the D-Bus name. The client's paint triggers a commit → halmasuit
    # re-composites → audit_frame republishes the snapshot buffer with
    # the green frame.
    machine.wait_until_succeeds(
        "journalctl -u halmasuit | grep -qF 'scanout_active'",
        timeout=30,
    )
    machine.wait_until_succeeds(
        "journalctl -u halmasuit | grep -qF 'layer-shell-test-client: painted'",
        timeout=30,
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

    visual.assert_matches_golden(machine, "halmasuit-layer-green")

    print("visual-halmasuit-layer: ALL ASSERTIONS PASSED")
  '';
}
