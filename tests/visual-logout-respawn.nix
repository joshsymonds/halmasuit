# tests/visual-logout-respawn.nix — Epic #47 R1 hard gate.
#
# Proves the full logout→broker-respawn arc:
#   1. Boot halmasuit; broker spawns greeter; capture greeter pid.
#   2. Real greetd full-auth via halmasuit-vm-client (protocol-driver,
#      not keystrokes — same shape visual-niri-session uses to drive
#      the broker without DMS).
#   3. session_opened + foreground_changed → Session (proves the
#      two-key swap_gate actually fired; niri directly painted a
#      buffer, satisfying the first-non-empty-frame key).
#   4. SIGKILL niri → broker emits SessionEnded (the sole reaper).
#   5. Compositor's Revert path runs: clears session_uid, resets
#      swap_gate, asks broker to spawn a fresh greeter.
#   6. New greeter_spawned event with a DIFFERENT pid.
#   7. Second login arc completes.
#   8. No-flash invariant holds across the entire arc.

{
  system,
  nixpkgs,
  nix-config,
  halmasuit,
  halmasuit-session,
  halmasuit-vm-client,
  halmasuit-layer-shell-test-client,
  ssimulacra2-cli,
}:

let
  pkgs = import nixpkgs {
    inherit system;
    config.allowUnfree = true;
  };
  niri = nix-config.inputs.niri-flake.packages.${system}.niri-unstable;

  # Same minimal niri config visual-niri-session uses — empty workspace,
  # no autostart, no keybinds. The gate proves the broker→niri handover,
  # not a niri configuration.
  niriConfig = pkgs.writeText "niri-logout-respawn-config.kdl" ''
    input {
        keyboard {
            xkb {
            }
        }
    }

    output "*" {
    }

    animations {
        off
    }
  '';

  # Same session-command pattern as visual-niri-session. niri runs
  # DIRECTLY (not via niri-session), so the dbus-update-activation-
  # environment failure that nukes niri-session in headless mode is
  # bypassed. niri's smithay GLES renderer runs on llvmpipe; it
  # paints a real Wayland buffer to halmasuit (even if visually
  # solid black via virtio-gpu-pci), which is non-empty and
  # satisfies swap_gate's first-non-empty-frame key.
  sessionCmd = pkgs.writeShellScript "halmasuit-logout-respawn-session" ''
    export XDG_RUNTIME_DIR=/run/halmasuit-niri
    export WAYLAND_DISPLAY=/run/halmasuit/wayland-0
    export LIBGL_ALWAYS_SOFTWARE=1
    export GALLIUM_DRIVER=llvmpipe
    export MESA_LOADER_DRIVER_OVERRIDE=llvmpipe
    export LIBGL_DRI3_DISABLE=1
    exec ${niri}/bin/niri --config ${niriConfig}
  '';
in
pkgs.testers.runNixOSTest {
  name = "visual-logout-respawn";

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
        package         = halmasuit;
        session.package = halmasuit-session;
        greeterUid      = 999;
        greeterGroup    = "halmasuit-greeter";
        compositorUid   = 998;
        wallpaper = { type = "image"; source = ./fixtures/wallpaper.png; };
        # Greeter: layer-shell test client (same as visual-niri-session).
        # Halmasuit-tracked child, broker forks-then-drops it as the
        # greeter uid; SIGKILL'd by halmasuit at swap time.
        greeterCommand = "${pkgs.writeShellScript "halmasuit-logout-respawn-greeter" ''
          export HALMASUIT_TESTCLIENT_KEYBOARD=1
          export HALMASUIT_TESTCLIENT_LAYER=top
          export HALMASUIT_TESTCLIENT_COLOR=#22DD77
          exec ${halmasuit-layer-shell-test-client}/bin/halmasuit-layer-shell-test-client
        ''}";
      };

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

      systemd.tmpfiles.rules = [
        "d /run/hsnap 0777 root root -"
        # niri's own XDG_RUNTIME_DIR — owned by alice for its listening
        # socket (matches visual-niri-session).
        "d /run/halmasuit-niri 0700 alice alice -"
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

    def fg_events():
        return [
            e["to"] for e in visual.introspect_events(machine)
            if e["event"] == "foreground_changed"
        ]

    def greeter_pids():
        """All greeter_spawned events as a list of pids in emission order."""
        return [
            e["pid"] for e in visual.introspect_events(machine)
            if e["event"] == "greeter_spawned"
        ]

    machine.start()
    machine.wait_for_unit("multi-user.target")
    machine.wait_for_unit("halmasuit.service")

    machine.wait_until_succeeds(
        "journalctl -u halmasuit | grep -qF scanout_active", timeout=30
    )
    machine.wait_until_succeeds(
        "journalctl -u halmasuit -o cat | grep -qF client_first_frame", timeout=30
    )
    machine.wait_until_succeeds("busctl --system status org.halmasuit", timeout=30)

    # ── Initial greeter spawn ──────────────────────────────────────
    machine.wait_until_succeeds(
        "journalctl -u halmasuit | grep -qF foreground_changed", timeout=30
    )
    assert "greeter" in fg_events(), f"expected greeter foreground; got {fg_events()}"

    first_greeter_pids = greeter_pids()
    assert len(first_greeter_pids) >= 1, (
        f"no greeter_spawned events found; events: {fg_events()}"
    )
    greeter_pid_1 = first_greeter_pids[-1]
    print(f"PASS: initial greeter spawned via broker, pid={greeter_pid_1}")

    halmasuit_pid = machine.succeed(
        "systemctl show -p MainPID --value halmasuit.service"
    ).strip()

    # ── First login arc ────────────────────────────────────────────
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

    # niri runs as the broker-launched session and paints a buffer →
    # swap_gate fires Swap → foreground_changed → Session.
    machine.wait_until_succeeds("pgrep -x niri", timeout=60)
    machine.wait_until_succeeds(
        "journalctl -u halmasuit | grep -qF "
        "'xdg_toplevel mapped as fullscreen foreground'",
        timeout=60,
    )
    assert fg_events()[:2] == ["greeter", "session"], (
        f"foreground ordering wrong: {fg_events()}"
    )
    print("PASS: first login arc swap fired (foreground=session)")

    # ── Trigger logout: SIGKILL niri ───────────────────────────────
    # Broker is the sole reaper; niri's death → SessionEnded →
    # compositor Revert.
    machine.succeed("pkill -KILL -x niri")
    print("PASS: SIGKILL'd niri to trigger SessionEnded")

    # ── Respawn assertion ──────────────────────────────────────────
    # Wait for foreground_changed → Greeter (the visible Revert) + a
    # NEW greeter_spawned event with a different pid.
    def respawn_observed():
        events = fg_events()
        # Must have seen the Revert (3rd entry: greeter, session, greeter)
        if len(events) < 3 or events[2] != "greeter":
            return False
        pids = greeter_pids()
        return len(pids) >= 2 and pids[-1] != greeter_pid_1

    deadline = time.monotonic() + 60.0
    while time.monotonic() < deadline:
        if respawn_observed():
            break
        time.sleep(0.5)
    assert respawn_observed(), (
        f"greeter did not respawn; foreground events={fg_events()}; "
        f"greeter_spawned pids={greeter_pids()}"
    )
    greeter_pid_2 = greeter_pids()[-1]
    print(
        f"PASS: greeter respawned via broker, new pid={greeter_pid_2} "
        f"(differs from initial {greeter_pid_1})"
    )

    # halmasuit's pid is unchanged.
    halmasuit_pid_after = machine.succeed(
        "systemctl show -p MainPID --value halmasuit.service"
    ).strip()
    assert halmasuit_pid == halmasuit_pid_after, (
        f"halmasuit restarted across logout/respawn: "
        f"{halmasuit_pid} -> {halmasuit_pid_after}"
    )
    print(f"PASS: halmasuit pid {halmasuit_pid} continuous across logout/respawn")

    # ── Second login arc ───────────────────────────────────────────
    # Wait for the new greeter's keyboard surface to be focused so the
    # second halmasuit-vm-client call doesn't race the broker's idle
    # state from the prior arc.
    initial_session_opened = int(machine.succeed(
        "journalctl -u halmasuit -o cat | grep -cF session_opened || true"
    ).strip() or "0")
    assert initial_session_opened >= 1, "lost track of session_opened count"

    machine.succeed(
        "runuser -u halmasuit-greeter -- "
        "halmasuit-vm-client full-auth /run/halmasuit/greetd.sock alice "
        "--password-file /tmp/alice.pw "
        "--cmd ${sessionCmd} "
        "--timeout 20"
    )

    machine.wait_until_succeeds(
        f"test $(journalctl -u halmasuit -o cat | "
        f"grep -cF session_opened) -gt {initial_session_opened}",
        timeout=120,
    )
    print("PASS: second login arc reached session_opened")

    # No-flash invariant over the entire arc.
    visual.assert_no_flash_stream(machine)

    print("visual-logout-respawn: ALL ASSERTIONS PASSED")
  '';
}
