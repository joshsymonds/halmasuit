# tests/visual-shutdown-image.nix — Epic #61 R3.4: image cell of the
# wallpaper-shutdown-survival matrix.
#
# Pairs with visual-shutdown-pivot-survival.nix (shader cell, asserts
# phash-progression + frame-counter advancing across the shutdown
# window) and visual-shutdown-video.nix (video cell, same animation
# invariants as shader with looser phash thresholds). Image wallpapers
# are STATIC by nature: the wallpaper-engine tick is NOT registered
# for them (no animation work to drive), so the kernel just keeps
# scanning out the last-flipped framebuffer. The shutdown survival
# invariants asserted here are accordingly narrower: process stays
# alive, same PID throughout, no coredump. Frame-counter advancement
# and phash-progression deliberately do NOT apply.
#
# Pre-shutdown assertion (this cell's unique hook): SSIMULACRA2
# `assert_matches_exact` against `tests/goldens/shutdown-image-session.png`
# — proves the image wallpaper is what's on screen right before
# halmasuit enters shutdown. Exact threshold (≥95), not the looser
# perceptual ≥90, because halmasuit's offscreen llvmpipe readback of
# a static image is deterministic (visual.py:281-303).
#
# Shared rig in tests/lib/shutdown-rig.nix; shared assertion body in
# tests/lib/shutdown_testscript.py.

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

  niriConfig = pkgs.writeText "niri-shutdown-image-config.kdl" ''
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

  sessionCmd = pkgs.writeShellScript "halmasuit-shutdown-image-session" ''
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
  name = "visual-shutdown-image";

  skipTypeCheck = true;

  interactive.nodes.machine = { ... }: {
    virtualisation.qemu.options = [
      "-device virtio-vga-gl"
      "-display gtk,gl=on"
    ];
  };

  nodes.machine = import ./lib/shutdown-rig.nix {
    inherit halmasuit halmasuit-session halmasuit-vm-client
      halmasuit-layer-shell-test-client;
    wallpaper = { type = "image"; source = ./fixtures/wallpaper.png; };
    extraStorePaths = [ sessionCmd niri ];
    # Image cell does pre-shutdown D-Bus Snapshot writes under
    # /run/hsnap (for the SSIMULACRA2 golden assertion); the
    # shader/video cells don't need this.
    extraTmpfilesRules = [ "d /run/hsnap 0777 root root -" ];
    extraReadWritePaths = [ "/run/hsnap" ];
  };

  testScript = ''
    import os
    import sys

    sys.path.insert(0, "${./lib}")
    os.environ["PATH"] = "${ssimulacra2-cli}/bin:" + os.environ.get("PATH", "")
    os.environ.setdefault("GOLDENS_DIR", "${./goldens}")

    import visual
    import shutdown_testscript

    shutdown_testscript.run(
        machine,
        cell_name="shutdown-image",
        session_cmd="${sessionCmd}",
        pre_shutdown_hook=lambda m, _pid: visual.assert_matches_exact(
            m, "shutdown-image-session"
        ),
        # Image wallpapers don't register the wallpaper-engine tick
        # (image is static; the kernel scans out the last framebuffer
        # without re-rendering). Frame-counter advancement and phash
        # progression DO NOT apply.
        assert_frame_advancement=False,
        phash_min_distinct=None,
        phash_min_hamming_max=None,
    )
  '';
}
