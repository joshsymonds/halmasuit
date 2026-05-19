# tests/visual-backdrop.nix — epic layer D.
#
# The no-flash proof harness on stand-in clients. Boots
# halmasuit-debug with its internal witness plane (composited from
# frame 0, the 4-quadrant fixture) and drives four scenes by
# starting/stopping extra layer-shell clients (the env-parametrised
# test client) as systemd services that connect to halmasuit's
# wayland socket as the greeter uid:
#
#   splash-only          — only the witness (background fixture)
#   greeter-over-splash  — + a centred opaque rect on the TOP layer
#   session-fullscreen   — + a fullscreen opaque OVERLAY client
#   post-session-splash  — session client stopped; witness visible again
#
# Two orthogonal gates run together:
#
#   * Per-scene sampled exact-IMAGE goldens (ssimulacra2) — content
#     correctness at each SETTLED scene: splash-only →
#     greeter-over-splash → session-fullscreen → splash-only again.
#
#   * The 100%-of-`frame_rendered`-stream no-flash gate
#     (`visual.assert_no_flash_stream`, Epic #1 req 11 / R3) — asserted
#     over EVERY composited frame as EXACT facts (clear_pixel_count==0
#     post-background, never degenerate, backdrop mapped once and never
#     resized). A sampled settled-scene golden cannot observe a
#     transient one-frame flash mid-transition; the stream gate can,
#     with zero tolerance. Both gates are live — neither replaces the
#     other.

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
  fixture = ./fixtures/splash-test.png;
  testClient = "${halmasuit-layer-shell-test-client}/bin/halmasuit-layer-shell-test-client";
  # A non-autostarted client service running as the greeter uid,
  # connecting to halmasuit's wayland socket. Started/stopped from
  # the testScript to drive scene transitions.
  clientService = env: {
    description = "halmasuit visual-backdrop test client";
    after = [ "halmasuit.service" ];
    serviceConfig = {
      User = "halmasuit-greeter";
      Group = "halmasuit-greeter";
      ExecStart = testClient;
      Environment = [
        "XDG_RUNTIME_DIR=/run/halmasuit"
        "WAYLAND_DISPLAY=wayland-0"
      ] ++ env;
    };
  };
in
pkgs.testers.runNixOSTest {
  name = "visual-backdrop";

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
        witnessImage   = fixture;
      };

      # Scene clients — NOT auto-started; the testScript starts/stops
      # them. test-greeter: a centred opaque orange rect on TOP, so
      # the splash shows around it. test-session: fullscreen opaque
      # blue on OVERLAY, fully occluding the splash.
      systemd.services.test-greeter = clientService [
        "HALMASUIT_TESTCLIENT_LAYER=top"
        "HALMASUIT_TESTCLIENT_COLOR=#FF8800"
        "HALMASUIT_TESTCLIENT_SIZE=640x400"
      ];
      systemd.services.test-session = clientService [
        "HALMASUIT_TESTCLIENT_LAYER=overlay"
        "HALMASUIT_TESTCLIENT_COLOR=#1133AA"
      ];

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
        memorySize = 2048;
        cores      = 2;
        diskSize   = 2048;
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
    os.environ["GOLDENS_DIR"] = "${./goldens}"

    import visual

    def cff_seen(role):
        # client_first_frame{role} present in the journal yet?
        # (emitted once per role per process; parsed from the nested
        # tracing JSON, robust to journalctl escaping).
        return any(
            e["event"] == "client_first_frame" and e.get("role") == role
            for e in visual.introspect_events(machine)
        )

    def wait_cff(role, timeout=60):
        deadline = time.time() + timeout
        while time.time() < deadline:
            if cff_seen(role):
                return
            time.sleep(1)
        raise TimeoutError(
            f"client_first_frame{{role:{role}}} not seen within {timeout}s; "
            f"events: {[e['event'] for e in visual.introspect_events(machine)]}"
        )

    machine.start()
    machine.wait_for_unit("multi-user.target")
    machine.wait_for_unit("halmasuit.service")

    machine.wait_until_succeeds(
        "journalctl -u halmasuit | grep -qF 'scanout_active'", timeout=30
    )
    machine.wait_until_succeeds("busctl --system status org.halmasuit", timeout=30)
    introspect = machine.succeed(
        "busctl --system introspect org.halmasuit /org/halmasuit/Debug/Introspect"
    )
    assert "Snapshot" in introspect, f"Snapshot missing:\n{introspect}"

    # ── scene: splash-only ──────────────────────────────────────────
    # The Event is logged as an escaped JSON string inside the tracing
    # line's fields.json, so grep for the bare token, not quoted JSON.
    machine.wait_until_succeeds(
        "journalctl -u halmasuit | grep -qF client_first_frame", timeout=30
    )
    wait_cff("background", 30)
    visual.assert_matches_golden(machine, "backdrop-splash-only")

    # ── scene: greeter-over-splash ──────────────────────────────────
    machine.succeed("systemctl start test-greeter.service")
    wait_cff("top")
    time.sleep(1)  # let the composited frame land in the snapshot buffer
    visual.assert_matches_golden(machine, "backdrop-greeter-over-splash")

    # ── scene: session-fullscreen ───────────────────────────────────
    machine.succeed("systemctl start test-session.service")
    wait_cff("overlay")
    time.sleep(1)
    visual.assert_matches_golden(machine, "backdrop-session-fullscreen")

    # ── scene: post-session-splash ──────────────────────────────────
    # Tear down BOTH overlay/top clients (in the real flow the greeter
    # is already gone by the time the session exits). halmasuit must
    # re-composite the splash beneath on each layer_destroyed — NOT
    # leave the last (session) frame on screen. The result returns to
    # the splash-only baseline: proves teardown is idempotent and the
    # background is never lost.
    machine.succeed("systemctl stop test-session.service")
    machine.wait_until_fails("systemctl is-active --quiet test-session.service", timeout=30)
    machine.succeed("systemctl stop test-greeter.service")
    machine.wait_until_fails("systemctl is-active --quiet test-greeter.service", timeout=30)
    time.sleep(2)
    visual.assert_matches_golden(machine, "backdrop-post-session-splash")

    # The 100%-stream no-flash proof: EVERY frame_rendered halmasuit
    # emitted across all four scenes and the three transitions between
    # them is checked as exact facts (clear_pixel_count==0 once the
    # background mapped, never degenerate, backdrop mapped once and
    # never resized), zero tolerance. This catches a transient
    # one-frame flash at a layer transition that the sampled
    # settled-scene goldens above cannot observe.
    visual.assert_no_flash_stream(machine)

    print("visual-backdrop: ALL ASSERTIONS PASSED")
  '';
}
