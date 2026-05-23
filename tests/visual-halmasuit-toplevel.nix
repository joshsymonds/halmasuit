# tests/visual-halmasuit-toplevel.nix — epic layer F1 gate.
#
# halmasuit-debug with its internal wallpaper plane (composited from
# frame 0, the 4-quadrant fixture), plus a separate xdg_toplevel
# client (fullscreen solid magenta #FF22AA) as a systemd-launched
# wl_client. Proves halmasuit maps + composites a REAL xdg_toplevel
# fullscreen ABOVE the wallpaper (z-order: toplevel over
# Background). Capture is the in-process Snapshot() D-Bus method.
#
# Golden = uniform #FF22AA (wallpaper fully occluded by the toplevel) —
# distinct from every prior golden, so it unambiguously proves
# xdg-shell compositing (not layer-shell, not clear).

{
  system,
  nixpkgs,
  halmasuit,
  halmasuit-session,
  halmasuit-toplevel-test-client,
  ssimulacra2-cli,
}:

let
  pkgs = import nixpkgs { inherit system; };
  fixture = ./fixtures/splash-test.png;
in
pkgs.testers.runNixOSTest {
  name = "visual-halmasuit-toplevel";

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
        wallpaper = { type = "image"; source = fixture; };
      };

      # The xdg_toplevel client connects as the greeter uid after
      # halmasuit is up. NOT auto-managed by halmasuit's greeter spawn
      # — an independent wl_client (like visual-backdrop's clients).
      # NOT auto-started: `after=halmasuit.service` only orders
      # unit-start, not the wayland socket being bound (~7s into
      # halmasuit, Type=simple). The testScript starts it after
      # scanout_active, same pattern as visual-backdrop's clients.
      systemd.services.test-toplevel = {
        description = "halmasuit F1 xdg_toplevel test client";
        after    = [ "halmasuit.service" ];
        serviceConfig = {
          User  = "halmasuit-greeter";
          Group = "halmasuit-greeter";
          ExecStart = "${halmasuit-toplevel-test-client}/bin/halmasuit-toplevel-test-client";
          Environment = [
            "XDG_RUNTIME_DIR=/run/halmasuit"
            "WAYLAND_DISPLAY=wayland-0"
          ];
          Restart = "no";
        };
      };

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
    os.environ.setdefault("GOLDENS_DIR", "${./goldens}")

    import visual

    machine.start()
    machine.wait_for_unit("multi-user.target")
    machine.wait_for_unit("halmasuit.service")

    machine.wait_until_succeeds(
        "journalctl -u halmasuit | grep -qF scanout_active", timeout=30
    )
    # Socket is up + the wallpaper plane is composited — now launch the
    # xdg_toplevel client (boot-race-free).
    machine.succeed("systemctl start test-toplevel.service")
    # halmasuit accepted + configured the xdg_toplevel fullscreen …
    machine.wait_until_succeeds(
        "journalctl -u halmasuit | grep -qF "
        "'xdg_toplevel mapped as fullscreen foreground'",
        timeout=60,
    )
    # … and the client painted it (it's its own unit, not a child
    # of halmasuit, so its stderr is in its own journal).
    machine.wait_until_succeeds(
        "journalctl -u test-toplevel | grep -qF 'toplevel-test-client: painted'",
        timeout=60,
    )
    machine.wait_until_succeeds("busctl --system status org.halmasuit", timeout=30)

    # Let the composited toplevel frame settle into the snapshot buffer.
    time.sleep(1)
    visual.assert_matches_golden(machine, "halmasuit-toplevel")

    print("visual-halmasuit-toplevel: ALL ASSERTIONS PASSED")
  '';
}
