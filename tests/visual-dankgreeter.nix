# tests/visual-dankgreeter.nix — epic G-layer R2/R4 (HANDOFF §6 G2):
# the REAL DankGreeter as halmasuit's greeter, over the wallpaper
# plane.
#
# The real greeter stack (gnomon's, unmodified, pinned via nix-config's
# dms flake): the `dms` greeter NixOS module builds the `dms-greeter`
# greetd-client wrapper → a nested niri (greeter-mode compositor) →
# Quickshell/Qt running the DMS Material-You login UI. greetd is
# force-disabled — halmasuit is the display manager; it forks the
# dms-greeter as its `greeterCommand`, with GREETD_SOCK pointed at
# halmasuit's relay socket so the greeter's greetd client authenticates
# via halmasuit → the privileged broker.
#
# Greeter-niri inherits the niri-session lessons (G-layer 5): software
# render (llvmpipe), its OWN XDG_RUNTIME_DIR (it binds a wayland
# socket), absolute-path upstream WAYLAND_DISPLAY to halmasuit. Qt adds
# QT_QUICK_BACKEND=software.
#
# G2 scope: real DankGreeter LAUNCHES + RENDERS as halmasuit's greeter
# foreground over the wallpaper, no-flash across boot→wallpaper→greeter.
# The full keystroke auth arc (DankGreeter UI → broker → real niri) is
# G3 (next task) — not driven here.

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
  # The dms greeter NixOS module expects `inputs` (and the dms flake
  # injects `dmsPkgs`). Same pattern as smoke-boot.nix.
  testInputs = nix-config.inputs // { inherit nix-config; };
in
pkgs.testers.runNixOSTest {
  name = "visual-dankgreeter";

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
        # The real, unmodified greeter stack (exactly what gnomon runs).
        nix-config.inputs.niri-flake.nixosModules.niri
        nix-config.inputs.dms.nixosModules.greeter
      ];

      # niri-flake provides programs.niri.{enable,package}; the dms
      # greeter module's `compositorPackage` reads programs.niri.package.
      programs.niri.enable = true;
      programs.niri.package = niri;

      # Build the real DankGreeter (dms-greeter script + assets).
      programs.dank-material-shell.greeter = {
        enable = true;
        compositor.name = "niri";
      };

      # halmasuit is the display manager, NOT greetd. Keep greetd's
      # settings (the dms greeter module reads
      # settings.default_session.{user,command}) but never start it.
      services.greetd.enable = lib.mkForce false;
      services.greetd.settings.default_session.user = "halmasuit-greeter";

      services.halmasuit = {
        enable          = true;
        package         = halmasuit; # halmasuit-debug via flake
        session.package = halmasuit-session;
        greeterUid      = 999;
        greeterGroup    = "halmasuit-greeter";
        compositorUid   = 998;
        wallpaper = { type = "image"; source = ./fixtures/wallpaper.png; };
        # Launch the REAL dms-greeter (the module-produced greetd
        # command) as halmasuit's tracked greeter child. Env:
        #  - its own XDG_RUNTIME_DIR (greeter-niri binds a socket there;
        #    must be halmasuit-greeter-writable, NOT /run/halmasuit).
        #  - absolute WAYLAND_DISPLAY → halmasuit upstream (G-layer 5).
        #  - GREETD_SOCK → halmasuit's relay socket (greetd client auth
        #    flows compositor → privileged broker → real pam_unix).
        #  - llvmpipe software render (headless, G-layer 5) + Qt
        #    software scenegraph for Quickshell.
        greeterCommand = "${pkgs.writeShellScript "halmasuit-dankgreeter" ''
          export XDG_RUNTIME_DIR=/run/halmasuit-greeter
          export WAYLAND_DISPLAY=/run/halmasuit/wayland-0
          export GREETD_SOCK=/run/halmasuit/greetd.sock
          export LIBGL_ALWAYS_SOFTWARE=1
          export GALLIUM_DRIVER=llvmpipe
          export MESA_LOADER_DRIVER_OVERRIDE=llvmpipe
          export LIBGL_DRI3_DISABLE=1
          export QT_QUICK_BACKEND=software
          exec ${config.services.greetd.settings.default_session.command}
        ''}";
      };

      # The authenticated user the broker would fork-drop to (real
      # pam_unix). Own gid-1001 group clears the broker ≥1000 floor;
      # halmasuit-greeter membership lets its session reach halmasuit's
      # 0660 socket. uid/gid 1001 (test-user.nix takes 1000).
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
        # Greeter-niri's own XDG_RUNTIME_DIR (it binds a wayland
        # socket) — owned by the greeter user halmasuit forks as.
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
    # Wallpaper composited from frame 0.
    machine.wait_until_succeeds(
        "journalctl -u halmasuit -o cat | grep -qF client_first_frame", timeout=30
    )
    machine.wait_until_succeeds("busctl --system status org.halmasuit", timeout=30)

    # The real DankGreeter stack is alive: dms-greeter wrapper →
    # greeter-niri → Quickshell.
    machine.wait_until_succeeds("pgrep -f dms-greeter", timeout=90)
    machine.wait_until_succeeds("pgrep -x niri", timeout=90)
    machine.wait_until_succeeds("pgrep -f quickshell", timeout=90)

    # DankGreeter is halmasuit's foreground over the wallpaper plane.
    machine.wait_until_succeeds(
        "journalctl -u halmasuit | grep -qF foreground_changed", timeout=60
    )
    assert "greeter" in fg_events(), f"expected greeter foreground; got {fg_events()}"

    time.sleep(3)  # let the Quickshell UI settle into the snapshot buffer
    visual.assert_matches_golden(machine, "dankgreeter")

    # No black/uncovered/degenerate frame across boot→wallpaper→real
    # DankGreeter — 100% of the frame_rendered stream, frame-0
    # anchored, zero tolerance (epic G1/R3).
    visual.assert_no_flash_stream(machine)

    print("visual-dankgreeter: ALL ASSERTIONS PASSED")
  '';
}
