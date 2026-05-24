# tests/visual-dankgreeter-auth.nix — R13(b) G3 keystroke auth arc.
#
# halmasuit's load-bearing R13(b) success criterion: real keystrokes
# typed at DMS DankGreeter drive the broker to a real pam_unix
# authentication. This is the test that proves the FULL chain
# end-to-end with the actual upstream client.
#
# Chain proven by this test:
#   QEMU keystroke
#     → halmasuit libinput
#     → halmasuit's wl_keyboard.{enter,key} on Quickshell's wlr-
#       layer-shell surface
#     → Qt's QtWayland plugin → DMS QML TextField (text rendering
#       damage visible in the wayland-debug trace)
#     → DMS's Quickshell.Services.Greetd → halmasuit's greetd
#       socket
#     → halmasuit-greetd state machine (with PATH-resolution for
#       relative session commands — broker_session.rs's
#       `resolve_command_path`)
#     → halmasuit-session broker relay (SO_PEERCRED-gated)
#     → real pam_unix → session_opened
#
# The downstream session leader (niri-session) is environment-
# dependent: in production it sets up dbus-activation-environment,
# user@1001.service, etc. — none of which exist in the headless
# test VM. So this test asserts the chain ending at
# `Event::SessionOpened`, which is the load-bearing R13(b)
# milestone: "credentials typed at DMS authenticate through the
# broker." Downstream niri boot is gated by visual-niri-session
# (which uses a direct niri command, no niri-session wrapper, to
# isolate from the same VM environment gap).

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

  # Run DMS Quickshell directly as halmasuit's greeter — no nested
  # compositor in the chain.
  greeterCmd = pkgs.writeShellScript "halmasuit-dankgreeter-auth" ''
    export XDG_RUNTIME_DIR=/run/halmasuit-greeter
    export WAYLAND_DISPLAY=/run/halmasuit/wayland-0
    export GREETD_SOCK=/run/halmasuit/greetd.sock
    # XDG_DATA_DIRS so DMS discovers niri.desktop via the standard
    # wayland-sessions path (Quickshell's wrapper merges /run/
    # current-system/sw/share into XDG_DATA_DIRS already; this is
    # belt-and-suspenders).
    export XDG_DATA_DIRS=/run/current-system/sw/share:/usr/share
    export LIBGL_ALWAYS_SOFTWARE=1
    export GALLIUM_DRIVER=llvmpipe
    export MESA_LOADER_DRIVER_OVERRIDE=llvmpipe
    export LIBGL_DRI3_DISABLE=1
    export QT_QPA_PLATFORM=wayland
    export QT_QUICK_BACKEND=software
    export QT_WAYLAND_DISABLE_WINDOWDECORATION=1
    export HOME=/run/halmasuit-greeter
    export XDG_CACHE_HOME=$HOME/.cache
    # DMS reads DMS_RUN_GREETER to take the greeter QML code path.
    export DMS_RUN_GREETER=1
    mkdir -p "$XDG_CACHE_HOME/dms-greeter"
    exec ${dmsQuickshell}/bin/quickshell \
      -p ${dmsShell}/share/quickshell/dms
  '';
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
        nix-config.inputs.niri-flake.nixosModules.niri
        nix-config.inputs.dms.nixosModules.greeter
      ];

      programs.niri.enable = true;
      programs.niri.package = niri;

      programs.dank-material-shell.greeter = {
        enable = true;
        compositor.name = "niri";
      };

      services.greetd.enable = lib.mkForce false;
      # DMS greeter module reads default_session.user even when
      # greetd is disabled; set it so the module evaluates cleanly.
      services.greetd.settings.default_session.user = "halmasuit-greeter";

      services.halmasuit = {
        enable          = true;
        package         = halmasuit;
        session.package = halmasuit-session;
        greeterUid      = 999;
        greeterGroup    = "halmasuit-greeter";
        compositorUid   = 998;
        wallpaper       = { type = "image"; source = ./fixtures/wallpaper.png; };
        greeterCommand  = "${greeterCmd}";
      };

      # niri.desktop ships with `Exec=niri-session` (relative).
      # halmasuit-greetd's PATH-resolution helper resolves it
      # against /run/current-system/sw/bin et al. before relaying
      # to the broker (which requires absolute paths). Linking
      # /share/wayland-sessions into the system profile makes the
      # .desktop file discoverable by DMS.
      environment.pathsToLink = [ "/share/wayland-sessions" ];

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

    # DMS Quickshell running directly as halmasuit's greeter.
    machine.wait_until_succeeds("pgrep -f quickshell", timeout=60)

    # Wait for Quickshell to advertise its layer surface and for
    # halmasuit's layer-shell focus path to fire (this is the
    # state-based gate that the keystrokes will land on a focused
    # surface, not a fortunate sleep).
    machine.wait_until_succeeds(
        "journalctl -u halmasuit -o cat | grep -qF 'new layer surface'",
        timeout=30,
    )
    machine.wait_until_succeeds(
        "journalctl -u halmasuit | grep -qF foreground_changed",
        timeout=30,
    )
    assert "greeter" in fg_events(), (
        f"expected greeter foreground; got {fg_events()}"
    )

    # ── R13(b) keystroke arc ──────────────────────────────────────
    halmasuit_pid = machine.succeed(
        "systemctl show -p MainPID --value halmasuit.service"
    ).strip()

    # DMS uses ONE TextField that toggles between username and
    # password mode on Enter. The username→password transition is
    # client-side state inside Quickshell's QML scene — neither
    # halmasuit nor the broker emit a journal-visible marker for
    # it (the broker's auth_message is logged only at Quickshell
    # debug-log level, which requires fragile env-var setup that
    # depends on Qt's category-resolver internals). 1-second sleep
    # is the best-available proxy; in practice the transition is
    # sub-100ms once DMS sees Enter.
    #
    # Pre-existing wart on this family of tests (visual-dankgreeter
    # has a similar settle sleep). Replace with a state-based gate
    # if/when DMS or halmasuit emits a structured log line at
    # password-mode entry — would need an upstream change to DMS or
    # a new halmasuit-greetd log emission.
    machine.send_chars("alice")
    machine.send_key("ret")
    time.sleep(1)
    machine.send_chars("testpassword")
    machine.send_key("ret")

    # R13(b) load-bearing milestone: real pam_unix authenticated
    # through the broker. session_opened proves the FULL chain —
    # DMS QML → halmasuit's wl_keyboard → DMS greetd client →
    # halmasuit-greetd → broker → real PAM.
    # halmasuit emits `Event::SessionOpened` as a JSON-escaped
    # entry inside its own structured log line; match on the bare
    # token to avoid quote-escaping gymnastics.
    machine.wait_until_succeeds(
        "journalctl -u halmasuit | grep -qF session_opened",
        timeout=120,
    )

    # halmasuit PID continuous across the auth arc.
    pid_now = machine.succeed(
        "systemctl show -p MainPID --value halmasuit.service"
    ).strip()
    assert pid_now == halmasuit_pid, (
        f"R13(b) violated: halmasuit restarted during auth: "
        f"{halmasuit_pid} -> {pid_now}"
    )

    # Real-PAM identity check: the broker actually opened a session
    # for alice (uid 1001), not a mocked auth.
    machine.succeed(
        "journalctl -u halmasuit-session.service | "
        "grep -qF 'pam_unix(halmasuit:session): session opened for user alice'"
    )

    print(
        f"R13(b): halmasuit pid {halmasuit_pid} continuous; "
        "DMS keystrokes → broker → real pam_unix session_opened"
    )

    # No-flash invariant over the entire keystroke arc.
    visual.assert_no_flash_stream(machine)

    print("visual-dankgreeter-auth: ALL ASSERTIONS PASSED")
  '';
}
