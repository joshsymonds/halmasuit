# tests/visual-sync-subsurface.nix — convergence epic R3: regression
# test for `is_sync_subsurface` commit aggregation.
#
# Wayland spec (`wl_subsurface`, wayland.xml): a synchronized
# subsurface's committed state is CACHED at the parent and applied
# only when the parent surface commits. A compositor that re-renders
# on a sync-subsurface commit can paint a partial-tree state — the
# subsurface's new buffer with the parent's old state — which is a
# protocol violation observable to anyone using sync subsurfaces
# (DMS/Quickshell's layered UI being the immediate concern).
#
# The gate: halmasuit-subsurface-test-client drives a deterministic
# 4-phase commit sequence (xdg_toplevel parent + sync wl_subsurface
# child). The test driver records halmasuit's `frame_rendered` counts
# at each phase boundary and asserts:
#
#   PHASE 2 boundary (5s, after sync-subsurface-only commit at 3s):
#     count[2] == count[1]
#     The sync subsurface commit was NOT visible — no render fired.
#
#   PHASE 3 boundary (8s, after parent commit at 6s):
#     count[3] > count[2]
#     The parent commit applied the cached state — render fired.

{
  system,
  nixpkgs,
  halmasuit,
  halmasuit-session,
  halmasuit-subsurface-test-client,
  ssimulacra2-cli,
}:

let
  pkgs = import nixpkgs { inherit system; config.allowUnfree = true; };
in
pkgs.testers.runNixOSTest {
  name = "visual-sync-subsurface";

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
        enable          = true;
        package         = halmasuit; # halmasuit-debug via flake (frame_audit events)
        session.package = halmasuit-session;
        greeterUid      = 999;
        greeterGroup    = "halmasuit-greeter";
        compositorUid   = 998;
        wallpaper       = { type = "image"; source = ./fixtures/wallpaper.png; };
        greeterCommand  = "${halmasuit-subsurface-test-client}/bin/halmasuit-subsurface-test-client";
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

      systemd.tmpfiles.rules = [
        "d /run/hsnap 0777 root root -"
      ];
      systemd.services.halmasuit.serviceConfig.ReadWritePaths = [ "/run/hsnap" ];

      virtualisation = {
        memorySize = 4096;
        cores      = 4;
        diskSize   = 4096;
        qemu.options = [
          "-vga none"
          "-device virtio-gpu-pci"
        ];
      };
    };

  testScript = ''
    import os
    import sys
    import time

    sys.path.insert(0, "${./lib}")
    os.environ["PATH"] = "${ssimulacra2-cli}/bin:" + os.environ.get("PATH", "")
    os.environ.setdefault("GOLDENS_DIR", "${./goldens}")

    import visual

    def count_frame_rendered():
        return len([
            e for e in visual.introspect_events(machine)
            if e["event"] == "frame_rendered"
        ])

    machine.start()
    machine.wait_for_unit("multi-user.target")
    machine.wait_for_unit("halmasuit.service")

    machine.wait_until_succeeds(
        "journalctl -u halmasuit | grep -qF scanout_active", timeout=30
    )

    # Anchor samples on the test client's own journal markers, not
    # on wall-clock sleeps. The test client emits three log lines
    # marking its commit-sequence phases:
    #   - "initial mapping committed"   (parent + child first commit)
    #   - "PHASE 2 — sync subsurface commit"   (sync-only commit)
    #   - "PHASE 3 — parent commit"      (parent commit applies cache)
    # Each phase boundary blocks until the client log appears, then
    # a short settle pause lets any in-flight VBlank propagate to
    # the introspection event stream. No timing race with the client.

    machine.wait_until_succeeds(
        "journalctl -b | grep -qF "
        "'subsurface-test-client: initial mapping committed'",
        timeout=30,
    )
    # Quiescence pause (PHASE 1 baseline must be a stable count).
    # MEMORY rule carve-out: state-based polling cannot prove a
    # negative ("no more events will arrive"). 1 s is many VBlanks
    # at 60Hz; comfortably above any commit→VBlank→introspect
    # propagation delay.
    time.sleep(1)
    phase1 = count_frame_rendered()
    print(f"PHASE 1 (initial mapping done): frame_rendered count = {phase1}")

    machine.wait_until_succeeds(
        "journalctl -b | grep -qF "
        "'subsurface-test-client: PHASE 2'",
        timeout=30,
    )
    # Quiescence pause (PHASE 2 is the must-be-flat assertion; we
    # are asserting NO event arrives, which is fundamentally a
    # negative — same carve-out as the PHASE 1 baseline above).
    time.sleep(1)
    phase2 = count_frame_rendered()
    print(f"PHASE 2 (after sync-subsurface-only commit): frame_rendered count = {phase2}")

    # The CONTRACT under test (R3 / wl_subsurface spec):
    # A sync-subsurface commit MUST NOT trigger a render. With the
    # smithay smallvil pattern (is_sync_subsurface guard + root-walk
    # before any compositor work), no new frame_rendered events
    # should appear between PHASE 1 and PHASE 2.
    assert phase2 == phase1, (
        f"sync-subsurface commit triggered a render: "
        f"PHASE 1={phase1}, PHASE 2={phase2} (delta {phase2 - phase1}). "
        f"halmasuit MUST aggregate sync-subsurface commits to the "
        f"parent atomic state (wl_subsurface spec)."
    )

    machine.wait_until_succeeds(
        "journalctl -b | grep -qF "
        "'subsurface-test-client: PHASE 3'",
        timeout=30,
    )
    # State-based poll: PHASE 3 asserts a POSITIVE delta (parent
    # commit triggers a render), so we can poll for the count to
    # exceed phase2 rather than sleep blindly. 10s ceiling fails
    # fast if the parent commit silently dropped.
    deadline = time.monotonic() + 10
    phase3 = phase2
    while time.monotonic() < deadline:
        phase3 = count_frame_rendered()
        if phase3 > phase2:
            break
        time.sleep(0.1)
    print(f"PHASE 3 (after parent commit): frame_rendered count = {phase3}")

    # The POSITIVE half of the contract: a parent commit that
    # aggregates the child's pending state MUST trigger a render
    # (visible state changed; halmasuit must repaint).
    assert phase3 > phase2, (
        f"parent commit did NOT trigger a render: "
        f"PHASE 2={phase2}, PHASE 3={phase3}. "
        f"halmasuit must repaint when a parent commit applies cached "
        f"sync-subsurface state (wl_subsurface spec)."
    )

    # No black/uncovered/degenerate frame across the run — the
    # witness threads continuously underneath.
    visual.assert_no_flash_stream(machine)

    print("visual-sync-subsurface: ALL ASSERTIONS PASSED")
  '';
}
