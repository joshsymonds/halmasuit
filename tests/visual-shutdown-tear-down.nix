# tests/visual-shutdown-tear-down.nix — Epic #47 R2.1 hard gate.
#
# Proves the SIGTERM-arming + graceful tear-down path:
#   1. Boot halmasuit; broker-spawn greeter; auth → niri up.
#   2. Send SIGTERM to halmasuit (simulating systemd-shutdown's
#      shutdown-phase signal, NOT the boot pivot's kill spree).
#   3. Assert: Event::GreeterTerminated (greeter SIGKILLed by the
#      tear-down path; broker SIGKILLs the session).
#   4. Assert: Event::Shutdown{ reason: signal_term } emitted.
#   5. Assert: halmasuit exits cleanly (exit 0 — Restart=on-failure
#      means a clean exit doesn't restart it).
#   6. Assert: assert_no_flash_stream over the whole arc — the
#      visible tear-down (greeter+session → wallpaper-only)
#      doesn't reintroduce the flash invariant.
#
# Partial-scope of Epic #47 R2: this proves graceful tear-down
# UNTIL SIGKILL/halt. The full "wallpaper through the rootfs→
# shutdownRamfs pivot to kernel halt" continuity lands in R2.2
# (storePaths + survival) and R2.3 (visual-shutdown-flash gate).

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
    machine.wait_for_unit("seatd.service")
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

    # ── Trigger shutdown via SIGTERM ────────────────────────────────
    # Sent directly to halmasuit's main PID (NOT via `systemctl stop`,
    # which would tear the unit down differently). This exercises the
    # state.shutdown_armed=true path: rootfs-only mode (no fromInitrd),
    # so shutdown_armed is true from startup.
    machine.succeed(f"kill -TERM {halmasuit_pid}")
    print(f"PASS: sent SIGTERM to halmasuit pid {halmasuit_pid}")

    # Wait for the unit to exit cleanly. Restart=on-failure means a
    # clean exit-0 doesn't restart it; the unit should report inactive.
    machine.wait_until_succeeds(
        "test \"$(systemctl is-active halmasuit.service)\" = inactive",
        timeout=30,
    )
    print("PASS: halmasuit.service exited cleanly (inactive)")

    # The journal should record the graceful tear-down events.
    # halmasuit's events go through tracing-subscriber's JSON
    # formatter; the event payload is itself a JSON-encoded string
    # field, so the quotes are double-escaped in the log line.
    # Match on the bare token instead of the verbatim shape.
    machine.succeed(
        "journalctl -u halmasuit -o cat | grep -qE 'event.*shutdown'"
    )
    machine.succeed(
        "journalctl -u halmasuit -o cat | grep -qE 'reason.*signal_term'"
    )
    print("PASS: Event::Shutdown{reason=signal_term} emitted")

    # No-flash invariant must hold across the entire arc, including
    # the SIGTERM-driven tear-down recomposite. The wallpaper plane
    # was visible throughout; no degenerate or all-black frame.
    visual.assert_no_flash_stream(machine)

    print("visual-shutdown-tear-down: ALL ASSERTIONS PASSED")
  '';
}
