# tests/visual-halmasuit-clear.nix — first visual gate against the real
# halmasuit binary (halmasuit-debug, frame_audit on).
#
# Pins the precondition that halmasuit alone — no wl_client connected —
# scans out the brand color #0a0014. Capture is the in-process
# `Snapshot()` D-Bus method (a CPU readback of the exact composited
# frame), NOT a QMP screendump of QEMU's display: Snapshot reads
# halmasuit's own framebuffer, so the QEMU display substrate is
# irrelevant and transient/pre-flip sampling cannot occur. ssimulacra2
# compares the PNG to the checked-in golden.
#
# Greeter is `sleep infinity` — we never drive auth here; the test
# waits for halmasuit's `scanout_active` event and for the D-Bus name
# to be owned, then calls Snapshot().

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
    { pkgs, ... }:
    {
      imports = [
        ../nix/module.nix
        ./lib/test-user.nix
      ];

      services.halmasuit = {
        enable         = true;
        package        = halmasuit; # halmasuit-debug (frame_audit) via flake
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

      # Snapshot() runs inside halmasuit AFTER its privilege drop (uid
      # 998) and halmasuit runs under ProtectSystem=strict, so it needs
      # an explicitly-writable, non-PrivateTmp path to write the PNG.
      # A world-writable tmpfiles dir on /run (not namespaced by
      # PrivateTmp) added to the unit's ReadWritePaths. Kept in sync
      # with visual.py's GUEST_SNAPSHOT_DIR.
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

    # scanout_active fires after the first frame is composited+queued;
    # audit_frame then publishes it into the snapshot buffer.
    machine.wait_until_succeeds(
        "journalctl -u halmasuit | grep -qF 'scanout_active'",
        timeout=30,
    )
    # The frame_audit D-Bus server owns the name once it's ready.
    machine.wait_until_succeeds(
        "busctl --system status org.halmasuit",
        timeout=30,
    )

    # Task #7's deferred live proof: Snapshot is exposed on
    # org.halmasuit.Debug.Introspect (and nowhere else).
    introspect = machine.succeed(
        "busctl --system introspect org.halmasuit "
        "/org/halmasuit/Debug/Introspect"
    )
    assert "Snapshot" in introspect, (
        f"Snapshot method missing from Introspect interface:\n{introspect}"
    )

    visual.assert_matches_golden(machine, "halmasuit-clear-color")

    print("visual-halmasuit-clear: ALL ASSERTIONS PASSED")
  '';
}
