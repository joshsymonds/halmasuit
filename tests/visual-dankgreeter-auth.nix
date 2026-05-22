# tests/visual-dankgreeter-auth.nix — R13(b) G3 keystroke auth arc.
#
# NOT a gated check — superseded by visual-qt6-greeter-auth.nix.
#
# This file is retained as documentation of the Round-4/4b
# investigation into the R13(b) blockage on the DMS+Quickshell path.
# It is NOT imported into flake.nix's `checks` attrset; running it
# requires manual nix-build invocation. The full diagnostic narrative
# is in the commit log + Task #37 — short summary below.
#
# Background:
# visual-dankgreeter (G2) renders the DMS DankGreeter UI under
# halmasuit and asserts the no-flash invariant. It deliberately
# stops short of driving keystrokes through the UI — diagnostic
# traces confirmed that halmasuit's input chain delivers keys to
# greeter-niri's xdg-toplevel, but greeter-niri (a nested niri
# compositor) does NOT forward those wl_keyboard events to its
# Quickshell child. That's upstream nested-niri behavior we cannot
# patch (CLAUDE.md hard rule).
#
# This test bypasses the nested-niri layer by running Quickshell
# directly as halmasuit's greeter wayland client. The DMS QML scene
# is the same as visual-dankgreeter (`shell.qml` from DMS); the
# `CompositorService.isNiri` guards in DMS gracefully handle the
# non-niri case (no keyboard-layout switcher, but the username +
# password TextFields still work). With this layer removed,
# keystrokes from halmasuit's wl_keyboard reach Quickshell directly,
# the DMS greetd client sends `create_session` +
# `post_auth_message_response` to halmasuit's relay socket, the
# privileged broker authenticates via real pam_unix, the session
# leader forks-and-drops, and halmasuit's foreground swaps to the
# session.
#
# Asserts the full R13(b) keystroke arc: send_chars("alice") + Tab
# + send_chars("testpassword") + Enter → foreground transitions to
# session, halmasuit PID unchanged across the swap, no-flash
# invariant intact over the FULL boot→witness→DankGreeter→session
# continuum.

{
  system,
  nixpkgs,
  nix-config,
  halmasuit,
  halmasuit-session,
  ssimulacra2-cli,
}:

let
  pkgs = import nixpkgs {
    inherit system;
    config.allowUnfree = true;
  };
  niri = nix-config.inputs.niri-flake.packages.${system}.niri-unstable;
  dmsShell = nix-config.inputs.dms.packages.${system}.dms-shell;
  dmsQuickshell = nix-config.inputs.dms.packages.${system}.quickshell;
  testInputs = nix-config.inputs // { inherit nix-config; };
in
pkgs.testers.runNixOSTest {
  name = "visual-dankgreeter-auth";

  skipTypeCheck = true;

  node.specialArgs = { inputs = testInputs; };

  interactive.nodes.machine = { ... }: {
    virtualisation.qemu.options = [
      "-device virtio-vga-gl"
      "-display gtk,gl=on"
    ];
  };

  nodes.machine =
    { config, lib, pkgs, ... }:
    {
      imports = [
        ../nix/module.nix
        ./lib/test-user.nix
        # niri-flake (only needed for niri-as-SESSION; greeter is
        # plain Quickshell, no niri-as-greeter).
        nix-config.inputs.niri-flake.nixosModules.niri
        nix-config.inputs.dms.nixosModules.greeter
      ];

      programs.niri.enable = true;
      programs.niri.package = niri;

      # We still enable the DMS greeter module so its assets (icons,
      # locales, settings) install into the system; but we bypass
      # its `dms-greeter` wrapper and run Quickshell directly so
      # halmasuit's wl_keyboard reaches Quickshell without an
      # intermediate compositor.
      programs.dank-material-shell.greeter = {
        enable = true;
        compositor.name = "niri";  # required by the module schema; unused
      };

      services.greetd.enable = lib.mkForce false;
      services.greetd.settings.default_session.user = "halmasuit-greeter";

      services.halmasuit = {
        enable          = true;
        package         = halmasuit; # halmasuit-debug via flake
        session.package = halmasuit-session;
        greeterUid      = 999;
        greeterGroup    = "halmasuit-greeter";
        compositorUid   = 998;
        witnessImage    = ./fixtures/witness.png;
        # Direct Quickshell launch — no nested compositor. The DMS
        # QML scene at ${dmsShell}/share/quickshell/dms is the same
        # scene visual-dankgreeter loads via dms-greeter+niri; here
        # it's loaded by Quickshell directly as halmasuit's wayland
        # client.
        greeterCommand = "${pkgs.writeShellScript "halmasuit-dankgreeter-auth" ''
          export XDG_RUNTIME_DIR=/run/halmasuit-greeter
          export WAYLAND_DISPLAY=/run/halmasuit/wayland-0
          export GREETD_SOCK=/run/halmasuit/greetd.sock
          export LIBGL_ALWAYS_SOFTWARE=1
          export GALLIUM_DRIVER=llvmpipe
          export MESA_LOADER_DRIVER_OVERRIDE=llvmpipe
          export LIBGL_DRI3_DISABLE=1
          export QT_QPA_PLATFORM=wayland
          export QT_QUICK_BACKEND=software
          export QT_WAYLAND_DISABLE_WINDOWDECORATION=1
          export HOME=/run/halmasuit-greeter
          export XDG_CACHE_HOME=$HOME/.cache
          # DMS expects this env var to detect greeter mode (set by
          # dms-greeter wrapper's niri config); replicate it here so
          # the QML scene takes the greeter-mode code path.
          export DMS_RUN_GREETER=1
          mkdir -p "$XDG_CACHE_HOME/dms-greeter"
          exec ${dmsQuickshell}/bin/quickshell \
            -p ${dmsShell}/share/quickshell/dms
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

      systemd.tmpfiles.rules = [
        "d /run/hsnap 0777 root root -"
        "d /run/halmasuit-greeter 0700 halmasuit-greeter halmasuit-greeter -"
      ];
      systemd.services.halmasuit.serviceConfig.ReadWritePaths = [ "/run/hsnap" ];

      virtualisation = {
        memorySize = 4096;
        cores      = 4;
        diskSize   = 8192;
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
    machine.wait_until_succeeds(
        "journalctl -u halmasuit -o cat | grep -qF client_first_frame", timeout=30
    )

    # Quickshell connected directly to halmasuit as a wayland client
    # (no greeter-niri in the chain).
    machine.wait_until_succeeds("pgrep -f quickshell", timeout=60)

    # Foreground transitions to greeter (Quickshell's xdg-toplevel
    # maps and gets keyboard focus via halmasuit's
    # maybe_focus_foreground_toplevel).
    machine.wait_until_succeeds(
        "journalctl -u halmasuit | grep -qF foreground_changed", timeout=60
    )
    assert "greeter" in fg_events(), f"expected greeter; got {fg_events()}"

    # ── G3 keystroke arc ───────────────────────────────────────────
    halmasuit_pid = machine.succeed(
        "systemctl show -p MainPID --value halmasuit.service"
    ).strip()

    # Let Quickshell's QML scene finish loading + auto-focus the
    # username TextField.
    time.sleep(3)

    machine.send_chars("alice")
    machine.send_key("tab")
    machine.send_chars("testpassword")
    machine.send_key("ret")

    # Foreground change: greeter → session within 60s (niri-as-
    # session spin-up after broker fork-drop).
    machine.wait_until_succeeds(
        "journalctl -u halmasuit | grep -c 'foreground_changed' | grep -q '^[2-9]'",
        timeout=60,
    )
    events = fg_events()
    assert events[:2] == ["greeter", "session"], (
        f"R13b foreground ordering wrong (expected [greeter, session]): {events}"
    )

    # halmasuit PID continuous across the swap — the load-bearing
    # R13b assertion.
    pid_now = machine.succeed(
        "systemctl show -p MainPID --value halmasuit.service"
    ).strip()
    assert pid_now == halmasuit_pid, (
        f"R13b violated: halmasuit restarted across greeter→session swap: "
        f"{halmasuit_pid} -> {pid_now}"
    )
    print(
        f"R13b: halmasuit pid {halmasuit_pid} continuous across "
        "DankGreeter→session swap"
    )

    visual.assert_no_flash_stream(machine)

    print("visual-dankgreeter-auth: ALL ASSERTIONS PASSED")
  '';
}
