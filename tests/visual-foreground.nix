# tests/visual-foreground.nix — epic layer F2 gate.
#
# Proves the greetd-lifecycle-driven foreground swap on the REAL
# mechanism: splash (BACKGROUND, persistent) + a layer-shell greeter
# (halmasuit's tracked greeterCommand child) → halmasuit-vm-client
# drives a real greetd full-auth → halmasuit kills the greeter and
# `halmasuit-spawn` execs the session (an xdg_toplevel client) as the
# authenticated user → the session toplevel becomes foreground.
#
# Asserts: ForegroundChanged ordering (greeter→session), halmasuit
# PID continuous across the swap (login-flash invariant on the real
# path), and — the point — the FrameRendered continuity invariant
# holds across the WHOLE transition (no black frame, splash coverage
# never lost). Snapshots gate the greeter and session scenes.
#
# NOTE: the session user reaching halmasuit's wayland socket is
# arranged test-locally (group membership). The production
# session→compositor socket handover is layer G / an ARCHITECTURE
# open decision, out of F2's scope (F2 = the foreground machine +
# no-flash proof).

{
  system,
  nixpkgs,
  halmasuit,
  halmasuit-spawn,
  halmasuit-splash,
  halmasuit-layer-shell-test-client,
  halmasuit-toplevel-test-client,
  halmasuit-vm-client,
  ssimulacra2-cli,
}:

let
  pkgs = import nixpkgs { inherit system; };
  fixture = ./fixtures/splash-test.png;
  # The session halmasuit-spawn execs as the authenticated user: a
  # wrapper that gives it the wayland env then runs the xdg_toplevel
  # client (a stand-in for niri — G wires the real one).
  sessionCmd = pkgs.writeShellScript "halmasuit-test-session" ''
    export XDG_RUNTIME_DIR=/run/halmasuit
    export WAYLAND_DISPLAY=wayland-0
    export HALMASUIT_TESTCLIENT_COLOR=#FF22AA
    exec ${halmasuit-toplevel-test-client}/bin/halmasuit-toplevel-test-client
  '';
in
pkgs.testers.runNixOSTest {
  name = "visual-foreground";

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
        package        = halmasuit; # halmasuit-debug via flake
        spawnPackage   = halmasuit-spawn;
        greeterUid     = 999;
        greeterGroup   = "halmasuit-greeter";
        compositorUid  = 998;
        # The greeter: a fullscreen keyboard-interactive layer client
        # over the splash. halmasuit's tracked child — killed on
        # start_session.
        greeterCommand = "${pkgs.writeShellScript "halmasuit-fg-greeter" ''
          export HALMASUIT_TESTCLIENT_KEYBOARD=1
          export HALMASUIT_TESTCLIENT_LAYER=top
          export HALMASUIT_TESTCLIENT_COLOR=#2255FF
          exec ${halmasuit-layer-shell-test-client}/bin/halmasuit-layer-shell-test-client
        ''}";
      };

      # Splash: persistent BACKGROUND, independent of the greeter
      # (must survive the greeter being killed). testScript-launched.
      systemd.services.test-splash = {
        description = "halmasuit F2 splash background";
        after = [ "halmasuit.service" ];
        serviceConfig = {
          User  = "halmasuit-greeter";
          Group = "halmasuit-greeter";
          ExecStart = "${pkgs.writeShellScript "fg-splash" ''
            export HALMASUIT_SPLASH_IMAGE=${fixture}
            exec ${halmasuit-splash}/bin/halmasuit-splash
          ''}";
          Environment = [
            "XDG_RUNTIME_DIR=/run/halmasuit"
            "WAYLAND_DISPLAY=wayland-0"
          ];
          Restart = "no";
        };
      };

      # The authenticated session user. halmasuit-spawn's load-bearing
      # UID floor refuses uid OR gid < 1000 — a normal NixOS user
      # lands in group `users` (gid 100) which the floor (correctly!)
      # rejects, so alice gets her own gid-1000 group (same pattern as
      # halmasuit-vm.nix). Test-local `halmasuit-greeter` membership
      # lets the session she spawns open halmasuit's 0660 wayland
      # socket; the production session→compositor socket handover is
      # layer G's concern.
      # uid/gid 1001 (not 1000): test-user.nix already takes uid 1000;
      # 1001 still clears halmasuit-spawn's ≥1000 floor.
      users.users.alice = {
        isNormalUser = true;
        uid          = 1001;
        group        = "alice";
        password     = "testpassword";
        extraGroups  = [ "halmasuit-greeter" ];
      };
      users.groups.alice.gid = 1001;

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

      environment.systemPackages = [ halmasuit-vm-client ];

      systemd.tmpfiles.rules = [ "d /run/hsnap 0777 root root -" ];
      systemd.services.halmasuit.serviceConfig.ReadWritePaths = [ "/run/hsnap" ];

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

    def fg_events():
        return [
            e["to"] for e in visual.introspect_events(machine)
            if e["event"] == "foreground_changed"
        ]

    machine.start()
    machine.wait_for_unit("multi-user.target")
    machine.wait_for_unit("seatd.service")
    machine.wait_for_unit("halmasuit.service")

    machine.wait_until_succeeds(
        "journalctl -u halmasuit | grep -qF scanout_active", timeout=30
    )
    # Splash first (persistent background).
    machine.succeed("systemctl start test-splash.service")
    machine.wait_until_succeeds(
        "journalctl -u test-splash | grep -qF 'halmasuit-splash: presented'", timeout=90
    )
    machine.wait_until_succeeds(
        "journalctl -u halmasuit -o cat | grep -qF client_first_frame", timeout=30
    )
    machine.wait_until_succeeds("busctl --system status org.halmasuit", timeout=30)

    # Greeter (greeterCommand child) up + foreground=Greeter.
    machine.wait_until_succeeds(
        "journalctl -u halmasuit | grep -qF foreground_changed", timeout=30
    )
    assert "greeter" in fg_events(), f"expected greeter foreground; got {fg_events()}"
    time.sleep(1)
    visual.assert_matches_golden(machine, "foreground-greeter")

    halmasuit_pid = machine.succeed(
        "systemctl show -p MainPID --value halmasuit.service"
    ).strip()

    # Drive a REAL greetd full-auth as the greeter uid; the session
    # halmasuit-spawn execs is the xdg_toplevel client.
    machine.succeed("printf 'testpassword' > /tmp/alice.pw")
    machine.succeed("chown halmasuit-greeter:halmasuit-greeter /tmp/alice.pw")
    machine.succeed("chmod 600 /tmp/alice.pw")
    machine.succeed(
        "runuser -u halmasuit-greeter -- "
        "halmasuit-vm-client full-auth /run/halmasuit/greetd.sock alice "
        "--password-file /tmp/alice.pw "
        "--cmd ${sessionCmd} "
        "--timeout 20"
    )

    # greetd lifecycle → foreground=Session, session toplevel maps.
    machine.wait_until_succeeds(
        "journalctl -u halmasuit | grep -qF "
        "'xdg_toplevel mapped as fullscreen foreground'",
        timeout=60,
    )
    assert fg_events()[:2] == ["greeter", "session"], (
        f"foreground ordering wrong: {fg_events()}"
    )

    # halmasuit did NOT restart across the real greeter→session swap.
    pid_now = machine.succeed(
        "systemctl show -p MainPID --value halmasuit.service"
    ).strip()
    assert pid_now == halmasuit_pid, (
        f"halmasuit restarted across the swap: {halmasuit_pid} -> {pid_now}"
    )
    print(f"PASS: halmasuit pid {halmasuit_pid} continuous across greeter→session")

    time.sleep(1)
    visual.assert_matches_golden(machine, "foreground-session")

    # The point: no black/uncovered frame across the REAL transition.
    visual.assert_frame_continuity(machine)

    print("visual-foreground: ALL ASSERTIONS PASSED")
  '';
}
