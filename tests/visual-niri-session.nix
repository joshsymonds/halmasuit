# tests/visual-niri-session.nix — epic G-layer R2/R4: the REAL niri
# as the broker-launched session, over the internal witness.
#
# Forks visual-foreground.nix, swapping the xdg_toplevel STAND-IN
# session for unmodified upstream niri (pinned via nix-config's
# niri-flake — the same revision gnomon runs). The arc:
#
#   halmasuit (witness from frame 0) + a layer-shell greeter stand-in
#   → halmasuit-vm-client drives a REAL greetd full-auth → compositor
#   relays to the privileged halmasuit-session broker (real pam_unix)
#   → halmasuit kills the greeter and the broker forks-then-drops the
#   session as the authenticated user → that session is `niri`,
#   nested as a Wayland client of halmasuit (WAYLAND_DISPLAY set ⇒
#   niri's winit backend, not TTY/DRM).
#
# `$HOME`/`$XDG_RUNTIME_DIR` come from the broker's pam_open_session
# (pam_systemd) — NO ProtectHome=false / synthetic-HOME / namespace
# carve-out (that would unwind the §0 broker thesis this proves).
#
# Asserts: real auth → niri running (`pgrep -x niri`) as the
# fullscreen foreground xdg_toplevel; halmasuit PID continuous across
# the real greeter→niri swap; `assert_no_flash_stream` (frame-0
# anchored) over the WHOLE transition; a human-inspected niri-session
# golden. Greeter stays the layer-shell stand-in (real DankGreeter is
# the next task).

{
  system,
  nixpkgs,
  nix-config,
  halmasuit,
  halmasuit-session,
  halmasuit-layer-shell-test-client,
  halmasuit-vm-client,
  ssimulacra2-cli,
}:

let
  pkgs = import nixpkgs {
    inherit system;
    # niri-flake's pinned niri-unstable may pull unfree deps
    # transitively (same rationale as smoke-boot.nix).
    config.allowUnfree = true;
  };

  # Unmodified upstream niri, the exact revision gnomon runs (via
  # nix-config's own niri-flake input — never vendored or patched).
  niri = nix-config.inputs.niri-flake.packages.${system}.niri-unstable;

  # Minimal valid niri config: empty workspace, no autostart, no
  # keybinds needed — the gate only needs niri to come up as
  # halmasuit's nested Wayland client and map its toplevel. Kept
  # deliberately tiny so the gate proves the broker→niri handover, not
  # a niri configuration.
  niriConfig = pkgs.writeText "niri-config.kdl" ''
    input {
        keyboard {
            xkb {
            }
        }
    }

    output "*" {
    }

    layout {
    }

    // No spawn-at-startup, no animations — minimal, deterministic.
    animations {
        off
    }
  '';

  # The command the broker's session-leader child execs as the
  # authenticated user. Wayland env points niri at halmasuit's socket
  # (nested winit backend). XDG_RUNTIME_DIR/WAYLAND_DISPLAY mirror the
  # F2 stand-in; the rest of the env (HOME etc.) is the broker's
  # pam_open_session environment — no carve-out.
  sessionCmd = pkgs.writeShellScript "halmasuit-niri-session" ''
    export XDG_RUNTIME_DIR=/run/halmasuit
    export WAYLAND_DISPLAY=wayland-0
    exec ${niri}/bin/niri --config ${niriConfig}
  '';
in
pkgs.testers.runNixOSTest {
  name = "visual-niri-session";

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
        package         = halmasuit; # halmasuit-debug via flake
        session.package = halmasuit-session;
        greeterUid      = 999;
        greeterGroup    = "halmasuit-greeter";
        compositorUid   = 998;
        witnessImage    = ./fixtures/witness.png;
        # Greeter: the layer-shell stand-in over the witness (real
        # DankGreeter is the next task). halmasuit's tracked child —
        # killed on start_session.
        greeterCommand = "${pkgs.writeShellScript "halmasuit-niri-greeter" ''
          export HALMASUIT_TESTCLIENT_KEYBOARD=1
          export HALMASUIT_TESTCLIENT_LAYER=top
          export HALMASUIT_TESTCLIENT_COLOR=#2255FF
          exec ${halmasuit-layer-shell-test-client}/bin/halmasuit-layer-shell-test-client
        ''}";
      };

      # The authenticated session user (real pam_unix). Own gid-1001
      # group so the broker's ≥1000 UID/GID floor (Epic R8/R11) passes;
      # test-local halmasuit-greeter membership lets niri reach
      # halmasuit's 0660 wayland socket (production socket handover is
      # a later concern). uid/gid 1001: test-user.nix takes 1000.
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

    machine.start()
    machine.wait_for_unit("multi-user.target")
    machine.wait_for_unit("seatd.service")
    machine.wait_for_unit("halmasuit.service")

    machine.wait_until_succeeds(
        "journalctl -u halmasuit | grep -qF scanout_active", timeout=30
    )
    # Witness composited internally from frame 0.
    machine.wait_until_succeeds(
        "journalctl -u halmasuit -o cat | grep -qF client_first_frame", timeout=30
    )
    machine.wait_until_succeeds("busctl --system status org.halmasuit", timeout=30)

    machine.wait_until_succeeds(
        "journalctl -u halmasuit | grep -qF foreground_changed", timeout=30
    )
    assert "greeter" in fg_events(), f"expected greeter foreground; got {fg_events()}"

    halmasuit_pid = machine.succeed(
        "systemctl show -p MainPID --value halmasuit.service"
    ).strip()

    # Drive a REAL greetd full-auth as the greeter uid; the broker
    # authenticates against real pam_unix and its session-leader child
    # execs niri as alice.
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

    # Real niri is the broker-launched session.
    machine.wait_until_succeeds("pgrep -x niri", timeout=60)
    machine.wait_until_succeeds(
        "journalctl -u halmasuit | grep -qF "
        "'xdg_toplevel mapped as fullscreen foreground'",
        timeout=60,
    )
    assert fg_events()[:2] == ["greeter", "session"], (
        f"foreground ordering wrong: {fg_events()}"
    )

    # halmasuit did NOT restart across the real greeter→niri swap.
    pid_now = machine.succeed(
        "systemctl show -p MainPID --value halmasuit.service"
    ).strip()
    assert pid_now == halmasuit_pid, (
        f"halmasuit restarted across the swap: {halmasuit_pid} -> {pid_now}"
    )
    print(f"PASS: halmasuit pid {halmasuit_pid} continuous across greeter→niri")

    time.sleep(2)
    visual.assert_matches_golden(machine, "niri-session")

    # No black/uncovered/degenerate frame ANYWHERE across the REAL
    # greeter→niri swap — 100% of the frame_rendered stream, frame-0
    # anchored, zero tolerance (epic G1/R3).
    visual.assert_no_flash_stream(machine)

    print("visual-niri-session: ALL ASSERTIONS PASSED")
  '';
}
