# tests/visual-shutdown-tear-down.nix — Task #21 plain-stop clean-exit
# gate (supersedes the Epic #47 R2.2 "SIGTERM keeps running" gate).
#
# Proves the plain-stop path with a full session up:
#   1. Boot halmasuit; broker-spawn greeter; auth → niri up.
#   2. assert_no_flash_stream over the OPERATION arc (boot → greeter →
#      session): the visible content stream had no degenerate/black
#      frame while halmasuit was running normally.
#   3. Send SIGTERM to halmasuit (a plain stop — no preceding logind
#      PrepareForShutdown, so it is NOT a real system shutdown).
#   4. Assert: Event::Shutdown{ reason: signal_term } emitted.
#   5. Assert: halmasuit EXITS CLEANLY (Task #21). Restart=on-failure +
#      exit 0 ⇒ no restart ⇒ the unit deactivates and the PID is gone.
#      Releasing DRM master hands scanout back to fbcon (the tty1
#      getty); a blink there is fine — the no-flash invariant covers
#      automatic transitions, not a human stopping the compositor.
#
# Real-shutdown survival (paint until halt across the
# rootfs→shutdownRamfs pivot, where halmasuit must NOT exit) is the
# OPPOSITE path and is covered by the
# halmasuit-shutdown-probe-phase{0,1,2} gates under real
# `systemctl poweroff`.

{
  system,
  nixpkgs,
  niri-flake,
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
  niri = niri-flake.packages.${system}.niri-unstable;

  niriConfig = pkgs.writeText "niri-shutdown-tear-down-config.kdl" ''
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

  sessionCmd = pkgs.writeShellScript "halmasuit-shutdown-tear-down-session" ''
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
  name = "visual-shutdown-tear-down";

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
        greeterCommand = "${pkgs.writeShellScript "halmasuit-shutdown-tear-down-greeter" ''
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

    sys.path.insert(0, "${./lib}")
    os.environ["PATH"] = "${ssimulacra2-cli}/bin:" + os.environ.get("PATH", "")
    os.environ.setdefault("GOLDENS_DIR", "${./goldens}")

    import visual

    machine.start()
    machine.wait_for_unit("multi-user.target")
    machine.wait_for_unit("halmasuit.service")

    machine.wait_until_succeeds(
        "journalctl -u halmasuit | grep -qF scanout_active", timeout=30
    )
    machine.wait_until_succeeds(
        "journalctl -u halmasuit -o cat | grep -qF client_first_frame", timeout=30
    )

    # Auth + niri session up.
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
    machine.wait_until_succeeds("pgrep -x niri", timeout=60)
    machine.wait_until_succeeds(
        "journalctl -u halmasuit | grep -qF "
        "'xdg_toplevel mapped as fullscreen foreground'",
        timeout=60,
    )

    halmasuit_pid = machine.succeed(
        "systemctl show -p MainPID --value halmasuit.service"
    ).strip()
    print(f"PASS: halmasuit pid {halmasuit_pid} with niri session up")

    # No-flash invariant over the OPERATION arc (boot → greeter →
    # session), while halmasuit is still running and serving Snapshot().
    # The wallpaper plane was visible throughout; no degenerate / all-
    # black frame. The deliberate stop below is allowed to blink.
    visual.assert_no_flash_stream(machine)
    print("PASS: no-flash invariant held over the operation arc")

    # ── Plain stop via SIGTERM (Task #21) ──────────────────────────
    # A SIGTERM with NO preceding logind PrepareForShutdown is a plain
    # `systemctl stop` / manual stop — the system is staying up. Sent
    # directly to the main PID; shutdown_armed is true from startup
    # (rootfs-only mode), and shutting_down is false (no
    # PrepareForShutdown), so this takes the clean-exit path.
    machine.succeed(f"kill -TERM {halmasuit_pid}")
    print(f"PASS: sent SIGTERM to halmasuit pid {halmasuit_pid}")

    # Event::Shutdown{reason=signal_term} is emitted before exit.
    # halmasuit's events go through tracing-subscriber's JSON formatter;
    # the payload is itself a JSON-encoded string field, so quotes are
    # double-escaped — match on the bare token.
    machine.wait_until_succeeds(
        "journalctl -u halmasuit -o cat | grep -qE 'event.*shutdown'",
        timeout=10,
    )
    machine.succeed(
        "journalctl -u halmasuit -o cat | grep -qE 'reason.*signal_term'"
    )
    print("PASS: Event::Shutdown{reason=signal_term} emitted")

    # Task #21 contract: a plain stop EXITS CLEANLY. halmasuit catches
    # SIGTERM, releases DRM master (→ kernel fbcon → tty1 getty) and
    # exits 0. Restart=on-failure + exit 0 ⇒ no restart ⇒ the unit
    # deactivates and the PID is gone. (Real shutdown is the opposite:
    # it would keep painting — but that path needs PrepareForShutdown,
    # which a bare SIGTERM does not carry.)
    machine.wait_until_succeeds(
        '[ "$(systemctl is-active halmasuit.service)" != active ]',
        timeout=15,
    )
    machine.succeed(f"! test -d /proc/{halmasuit_pid}")
    active_state = machine.succeed(
        "systemctl show -p ActiveState --value halmasuit.service"
    ).strip()
    assert active_state in ("inactive", "deactivating"), (
        f"Task #21: halmasuit must exit cleanly on a plain SIGTERM, "
        f"ActiveState={active_state!r} (expected inactive)"
    )
    print(f"PASS: halmasuit exited cleanly on SIGTERM (ActiveState={active_state})")

    print("visual-shutdown-tear-down: ALL ASSERTIONS PASSED")
  '';
}
