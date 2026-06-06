# tests/visual-nvidia-greeter.nix — Epic #45, rung 3.
#
# Boot halmasuit with the REAL DankGreeter on the passed-through 5070 Ti
# and assert whether the greeter's wlr-layer-shell surface attaches a
# buffer. Rung 2 proved halmasuit's COMPOSITOR renders on real NVIDIA;
# the gnomon hang is in the GREETER (Quickshell). This rung reproduces
# (or refutes) that hang in an automatable harness.
#
# CRUCIAL: this mirrors gnomon's PRODUCTION greeter — Quickshell run
# DIRECTLY as halmasuit's Wayland client via DMS_RUN_GREETER=1 (NOT the
# nested-niri dms-greeter path in visual-dankgreeter.nix), because the
# direct-Quickshell path is the one that hangs on gnomon. And it uses
# the REAL NVIDIA EGL path — NO LIBGL_ALWAYS_SOFTWARE / llvmpipe /
# QT_QUICK_BACKEND=software (Epic #45 req 3); forcing software here
# would defeat the entire reproduction.
#
# RUNNER-ONLY: `just test-vm-nvidia visual-nvidia-greeter` on stygian.
{
  system,
  nixpkgs,
  halmasuit,
  halmasuit-session,
  dms,
}:
let
  pkgs = import nixpkgs {
    inherit system;
    config.allowUnfree = true;
  };
  dmsShell      = dms.packages.${system}.dms-shell;
  dmsQuickshell = dms.packages.${system}.quickshell;
  cacheDir      = "/var/lib/halmasuit-greeter";

  # gnomon's production greeterCommand, verbatim minus the gnomon-host
  # cache seeding — Quickshell directly as halmasuit's client, real
  # NVIDIA EGL (Qt wayland platform, no software scenegraph).
  greeterCmd = pkgs.writeShellScript "halmasuit-nvidia-dankgreeter" ''
    export XDG_RUNTIME_DIR=/run/halmasuit-greeter
    export WAYLAND_DISPLAY=/run/halmasuit/wayland-0
    export QT_QPA_PLATFORM=wayland
    export QT_WAYLAND_DISABLE_WINDOWDECORATION=1
    export HOME=${cacheDir}
    export XDG_CACHE_HOME=$HOME/.cache
    export DMS_RUN_GREETER=1
    mkdir -p "$XDG_CACHE_HOME/dms-greeter"
    exec ${dmsQuickshell}/bin/quickshell \
      -p ${dmsShell}/share/quickshell/dms
  '';
in
pkgs.testers.runNixOSTest {
  name = "visual-nvidia-greeter";
  skipTypeCheck = true;

  nodes.machine =
    { config, pkgs, ... }:
    {
      imports = [
        ./lib/nvidia-passthrough.nix
        ../nix/module.nix
        ./lib/test-user.nix
      ];

      services.halmasuit = {
        enable          = true;
        package         = halmasuit; # halmasuit-debug (frame_audit)
        session.package = halmasuit-session;
        greeterUid      = 999;
        greeterGroup    = "halmasuit-greeter";
        compositorUid   = 998;

        rendering = {
          backend = "nvidia";
          extraInitrdStorePaths = [
            "${pkgs.egl-wayland}"
            "${pkgs.egl-gbm}"
          ];
        };
        drmDevice = "pci:0000:00:09.0";

        wallpaper = { type = "image"; source = ./fixtures/wallpaper.png; };
        greeterCommand = "${greeterCmd}";
      };

      systemd.tmpfiles.rules = [
        "d /run/hsnap 0777 root root -"
        # Greeter's XDG_RUNTIME_DIR (Quickshell SEGVs in
        # QsPaths::linkRunDir without it — gen-392) + its HOME/cache.
        "d /run/halmasuit-greeter 0700 halmasuit-greeter halmasuit-greeter -"
        "d ${cacheDir} 0750 halmasuit-greeter halmasuit-greeter -"
      ];
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
    };

  testScript = ''
    machine.start()
    machine.wait_for_unit("multi-user.target")
    machine.wait_until_succeeds(
        "lspci -nnk -d 10de: | grep -q 'Kernel driver in use: nvidia'", timeout=60
    )

    try:
        machine.wait_for_unit("halmasuit.service", timeout=120)
    except Exception:
        print("=== halmasuit failed to start ===")
        print(machine.execute("journalctl -u halmasuit --no-pager")[1])
        raise

    # Rung-2 invariant: the compositor renders the wallpaper on NVIDIA.
    machine.wait_until_succeeds(
        "journalctl -u halmasuit -o cat | grep -qF client_first_frame", timeout=120
    )

    # The greeter process is alive (Quickshell forked by halmasuit).
    machine.wait_until_succeeds("pgrep -f quickshell", timeout=120)

    # THE GNOMON BUG: does the greeter's layer-shell surface attach a
    # buffer on real NVIDIA? A healthy greeter does this within a few
    # seconds of Quickshell starting; 40s is a generous cold-NVIDIA
    # ceiling that fast-fails on a hang. If it never attaches, this is
    # the reproduced gnomon hang — dump everything so the stall point
    # (EGL init? Quickshell crash? has_buffer:false?) is captured.
    try:
        machine.wait_until_succeeds(
            "journalctl -u halmasuit -o cat | "
            "grep -iE 'client_first_frame.*(overlay|top)|layer-shell commit.*has_buffer: true'",
            timeout=40,
        )
    except Exception:
        print("=== GREETER OVERLAY NEVER ATTACHED A BUFFER — gnomon hang reproduced ===")
        print("=== is quickshell still alive? ===\n" +
              machine.execute("ps -eo pid,etimes,stat,comm,args | grep -iE 'quickshell|qs_' | grep -v grep || echo '(no quickshell process — it exited/crashed)'")[1])
        print("=== halmasuit journal tail ===\n" +
              machine.execute("journalctl -u halmasuit --no-pager | tail -120")[1])
        print("=== layer-shell commit lines (has_buffer state) ===\n" +
              machine.execute("journalctl -u halmasuit -o cat | grep -F 'layer-shell commit' || echo '(no layer-shell commit lines at all)'")[1])
        print("=== client_first_frame events (which roles fired) ===\n" +
              machine.execute("journalctl -u halmasuit -o cat | grep -F client_first_frame || true")[1])
        print("=== quickshell / qt / qml / egl / wayland errors ===\n" +
              machine.execute("journalctl -o cat | grep -iE 'quickshell|qml|qt\\.|egl|wayland|nvrm|error|warning|assert|segfault' | tail -60 || true")[1])
        raise

    print(machine.succeed(
        "journalctl -u halmasuit -o cat | grep -iE 'client_first_frame.*(overlay|top)|layer-shell commit.*has_buffer: true' | head"
    ))
    print("visual-nvidia-greeter: DankGreeter attached a buffer on real NVIDIA. PASS")

    # Graceful GPU teardown so the next run works without a host reboot
    # (Blackwell reset wedge — see tests/lib/nvidia-teardown.sh).
    machine.execute("sh ${./lib/nvidia-teardown.sh}")
    machine.shutdown()
  '';
}
