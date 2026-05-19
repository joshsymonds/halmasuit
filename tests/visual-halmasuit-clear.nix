# tests/visual-halmasuit-clear.nix — the witness gate: the real
# halmasuit binary (halmasuit-debug, frame_audit on) compositing the
# LOCKED witness art.
#
# Pins that halmasuit alone — no greeter, no session, no external
# client — composites the locked witness (`tests/fixtures/witness.png`,
# the Ḫalmašuit emblem) as its internal bottom plane from frame 0
# (epic amendment G1/R3/R6: there is no pre-client solid phase). Two
# orthogonal, always-live gates:
#
#   * Exact-image content: `assert_matches_witness` compares the
#     in-process `Snapshot()` PNG — a CPU readback of the OFFSCREEN
#     GLES render target (Mesa llvmpipe, no GPU, no GBM scanout; the
#     exact scene the production pipeline composites, deterministic
#     run-to-run even fully headless on virtio-gpu-pci) — to the
#     checked-in `halmasuit-witness` golden with ssimulacra2 ≥ 95.0.
#     The golden is the 1280×800 render itself, HUMAN-INSPECTED
#     against the 2560×1600 `tests/fixtures/witness.png` source before
#     commit (the human bridges fixture→golden faithfulness; the gate
#     then pins regression). Never CI-regenerated.
#
#   * 100%-stream no-flash: `assert_no_flash_stream` over every
#     `frame_rendered` — the witness cff precedes frame 0 and every
#     frame is witness-covered (epic G1/R3, frame-0 anchor). This
#     gate uniquely proves the witness-ALONE no-flash from frame 0.
#
# Greeter is `sleep infinity` — no auth is driven here; the test
# waits for `scanout_active` + the D-Bus name, then Snapshot().

{
  system,
  nixpkgs,
  halmasuit,
  halmasuit-session,
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
        session.package   = halmasuit-session;
        greeterUid     = 999;
        greeterGroup   = "halmasuit-greeter";
        compositorUid  = 998;
        # The locked witness, composited by halmasuit internally as
        # its bottom plane from frame 0 (epic G1/R3/R6).
        witnessImage   = ./fixtures/witness.png;
        # No greeter activity — this gate tests halmasuit's own
        # witness compositing, not greeter flow.
        greeterCommand = "${pkgs.writeShellScript "halmasuit-witness-test-greeter" ''
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
    os.environ.setdefault("GOLDENS_DIR", "${./goldens}")

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

    # Exact-image gate (ssimulacra2 >= 95.0): the offscreen readback
    # of halmasuit's internal witness plane vs the human-inspected
    # `halmasuit-witness` golden. Headless llvmpipe; no GPU, no GBM
    # scanout — the offscreen GLES target is what makes this
    # pixel-correct and reproducible run-to-run.
    visual.assert_matches_witness(machine, "halmasuit-witness")

    # 100%-stream no-flash, frame-0 anchored (epic G1/R3): the witness
    # is composited from frame 0, so every frame_rendered must already
    # be witness-covered. This gate uniquely exercises the
    # witness-ALONE stream (no greeter/session frames in it).
    visual.assert_no_flash_stream(machine)

    print("visual-halmasuit-clear: ALL ASSERTIONS PASSED")
  '';
}
