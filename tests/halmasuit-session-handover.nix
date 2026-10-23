# tests/halmasuit-session-handover.nix — the production session path:
# a session launched the way a real greeter launches it reaches
# halmasuit and becomes a logind session.
#
# DMS's greeter starts the command from the session's .desktop entry
# with nothing but `XDG_SESSION_TYPE` in its env. Under halmasuit that
# entry is the module's `halmasuit-niri` session (`niriSession`), which
# starts niri as a systemd user service, nested in halmasuit. The
# session only works when the broker's PAM session registers with
# logind (pam_systemd: XDG_RUNTIME_DIR, the user manager), the session
# leader's env carries halmasuit's WAYLAND_DISPLAY into that manager,
# and niri's own display then replaces it there for the services that
# graphical-session.target starts. This test drives a real greetd
# full-auth whose start_session command is `halmasuit-niri-session`,
# with no env of its own, and asserts:
#   - niri runs as the authenticated user and halmasuit swaps the
#     foreground to it (greeter → session), PID continuous;
#   - the user has a logind session of type wayland on seat0 that is
#     active;
#   - the user manager's WAYLAND_DISPLAY is niri's socket and
#     graphical-session.target is active;
#   - the user's shell (DMS, a user unit wanted by
#     graphical-session.target, as home-manager installs it) starts
#     and maps its layer surfaces in the nested niri;
#   - no flash frame across the swap.
#
# The session user is in the greeter group, as a production config
# puts it, so niri can open halmasuit's 0660 socket; the SO_PEERCRED
# gate admits only the PAM-resolved session uid.

{
  system,
  nixpkgs,
  niri-flake,
  dms,
  halmasuit,
  halmasuit-session,
  halmasuit-layer-shell-test-client,
  halmasuit-vm-client,
}:

let
  pkgs = import nixpkgs {
    inherit system;
    config.allowUnfree = true;
  };
  niri = niri-flake.packages.${system}.niri-unstable;
  dmsShell = dms.packages.${system}.dms-shell;

  # Minimal system niri config: no autostart, no animations.
  niriConfig = ''
    input {
        keyboard {
            xkb {
            }
        }
    }

    layout {
    }

    animations {
        off
    }
  '';
in
pkgs.testers.runNixOSTest {
  name = "halmasuit-session-handover";

  skipTypeCheck = true;

  nodes.machine =
    { pkgs, ... }:
    {
      imports = [
        ../nix/module.nix
        ./lib/test-user.nix
        niri-flake.nixosModules.niri
      ];

      programs.niri.enable  = true;
      programs.niri.package = niri;
      services.halmasuit.niriSession = {
        enable  = true;
        package = niri;
      };
      environment.etc."niri/config.kdl".text = niriConfig;

      services.halmasuit = {
        enable          = true;
        package         = halmasuit;
        session.package = halmasuit-session;
        greeterUid      = 999;
        greeterGroup    = "halmasuit-greeter";
        compositorUid   = 998;
        wallpaper       = { type = "image"; source = ./fixtures/wallpaper.png; };
        greeterCommand = "${pkgs.writeShellScript "halmasuit-handover-greeter" ''
          export HALMASUIT_TESTCLIENT_KEYBOARD=1
          export HALMASUIT_TESTCLIENT_LAYER=top
          exec ${halmasuit-layer-shell-test-client}/bin/halmasuit-layer-shell-test-client
        ''}";
      };

      users.users.alice = {
        isNormalUser = true;
        uid          = 1001;
        group        = "alice";
        password     = "testpassword";
        extraGroups  = [ "halmasuit-greeter" ];
        # DMS execs `qs` from PATH. On gnomon it is in the per-user
        # profile, reached through the login PATH the session imports
        # into the user manager; the same here.
        packages     = [ pkgs.quickshell ];
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

      # The user's shell, as home-manager's DMS module installs it on
      # gnomon: started by graphical-session.target, so it starts only
      # once niri's display is in the user manager. Raw unit text, as
      # home-manager writes it: a NixOS-declared service would carry a
      # fixed Environment=PATH, while this one inherits the user
      # manager's PATH, which the session imports from the login shell.
      systemd.user.units."dms.service" = {
        wantedBy = [ "graphical-session.target" ];
        text = ''
          [Unit]
          PartOf=graphical-session.target
          After=graphical-session.target

          [Service]
          Environment=QS_DISABLE_CRASH_HANDLER=1
          Environment=QT_QUICK_BACKEND=software
          ExecStart=${dmsShell}/bin/dms run --session
        '';
      };

      # VM substrate only: the headless virtio-gpu has no usable
      # EGL device, so niri renders through llvmpipe. The user manager
      # reads these through pam_env, the way any session variable
      # reaches a systemd user service.
      environment.sessionVariables = {
        LIBGL_ALWAYS_SOFTWARE       = "1";
        GALLIUM_DRIVER              = "llvmpipe";
        MESA_LOADER_DRIVER_OVERRIDE = "llvmpipe";
        LIBGL_DRI3_DISABLE          = "1";
      };

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
    import sys

    sys.path.insert(0, "${./lib}")
    import visual

    def fg_events():
        return [
            e["to"] for e in visual.introspect_events(machine)
            if e["event"] == "foreground_changed"
        ]

    machine.start()
    machine.wait_for_unit("multi-user.target")
    machine.wait_for_unit("halmasuit.service")
    machine.wait_until_succeeds(
        "journalctl -u halmasuit | grep -qF scanout_active", timeout=30
    )
    machine.wait_until_succeeds(
        "journalctl -u halmasuit -o cat | grep foreground_changed | grep -q greeter",
        timeout=60,
    )
    halmasuit_pid = machine.succeed(
        "systemctl show -p MainPID --value halmasuit.service"
    ).strip()

    machine.succeed("printf 'testpassword' > /tmp/alice.pw")
    machine.succeed("chown halmasuit-greeter:halmasuit-greeter /tmp/alice.pw")
    machine.succeed("chmod 600 /tmp/alice.pw")
    machine.succeed(
        "runuser -u halmasuit-greeter -- "
        "halmasuit-vm-client full-auth /run/halmasuit/greetd.sock alice "
        "--password-file /tmp/alice.pw "
        "--cmd halmasuit-niri-session "
        "--timeout 20"
    )

    machine.wait_until_succeeds(
        "journalctl -u halmasuit -o cat | grep -qF session_opened", timeout=60
    )
    machine.wait_until_succeeds("pgrep -u alice -x niri", timeout=90)
    machine.wait_until_succeeds(
        "journalctl -u halmasuit -o cat | "
        "grep foreground_changed | grep -q session",
        timeout=90,
    )
    assert fg_events()[:2] == ["greeter", "session"], (
        f"foreground ordering wrong: {fg_events()}"
    )

    # The user manager holds a `manager`-class session of its own; the
    # login is the `user`-class one.
    props = [
        machine.succeed(
            f"loginctl show-session {sid} -p Type -p Class -p Seat -p Active -p State"
        )
        for sid in machine.succeed(
            "loginctl show-user alice -p Sessions --value"
        ).split()
    ]
    logins = [p for p in props if "Class=user" in p.splitlines()]
    assert len(logins) == 1, f"expected one user-class logind session for alice: {props}"
    props = logins[0]
    print(props)
    for want in ("Type=wayland", "Class=user", "Seat=seat0", "Active=yes"):
        assert want in props.splitlines(), f"alice's session lacks {want}: {props}"

    user_env = machine.succeed("systemctl --user -M alice@ show-environment")
    displays = [l for l in user_env.splitlines() if l.startswith("WAYLAND_DISPLAY=")]
    assert len(displays) == 1 and displays[0].startswith("WAYLAND_DISPLAY=wayland-"), (
        f"alice's user manager should hold niri's display: {displays}"
    )
    machine.wait_until_succeeds(
        "systemctl --user -M alice@ is-active graphical-session.target", timeout=60
    )

    machine.wait_until_succeeds(
        "systemctl --user -M alice@ is-active dms.service", timeout=60
    )
    niri_socket = next(
        l.split("=", 1)[1] for l in user_env.splitlines() if l.startswith("NIRI_SOCKET=")
    )
    # DMS draws its bar and widgets as layer surfaces of the nested niri.
    machine.wait_until_succeeds(
        f"runuser -u alice -- env NIRI_SOCKET={niri_socket} niri msg layers "
        "| grep -qi quickshell",
        timeout=120,
    )
    print(machine.succeed(
        f"runuser -u alice -- env NIRI_SOCKET={niri_socket} niri msg layers"
    ))

    pid_now = machine.succeed(
        "systemctl show -p MainPID --value halmasuit.service"
    ).strip()
    assert pid_now == halmasuit_pid, (
        f"halmasuit restarted across the swap: {halmasuit_pid} -> {pid_now}"
    )

    visual.assert_no_flash_stream(machine)

    print("halmasuit-session-handover: ALL ASSERTIONS PASSED")
  '';
}
