# tests/visual-shutdown-pivot-survival.nix — Epic #47 R2 hard gate
# AND Epic #61 R3 shader-cell hard gate.
#
# DUAL SCOPE — this file is both:
#   (1) the canonical pivot-survival test (PID continuity across the
#       rootfs→shutdownRamfs pivot, no coredump, liveness lines past
#       the post-pivot marker), and
#   (2) the shader cell of the wallpaper-shutdown-survival matrix
#       (frame_counter advances across the shutdown window + phash
#       progression proves the shader is animating, not frozen).
#
# Sibling matrix cells:
#   visual-shutdown-image.nix — image wallpaper (SSIMULACRA2 golden,
#       no animation assertions; image is static).
#   visual-shutdown-video.nix — video wallpaper (same assertion
#       shape as this file but with testsrc-tuned phash thresholds).
#
# Why the dual scope: in R3.1 we made the wallpaper-engine tick
# drive renders continuously for animated backends. The existing
# pivot-survival test already used a shader wallpaper, so it became
# the natural home for the shader-cell animation assertions. A pure
# rename to `visual-shutdown-shader.nix` would lose the historical
# git blame trail; the docstring on this comment block + the
# matrix-shared shutdown_testscript.run() invocation below make the
# dual scope discoverable.
#
# Shared rig in tests/lib/shutdown-rig.nix; shared assertion body in
# tests/lib/shutdown_testscript.py. The matrix-shared survival
# invariants (post-pivot marker, heartbeat-after-pivot, PID
# continuity, no-coredump) are asserted there; the shader-cell
# extras (frame counter advancement + phash progression) are
# parametrized in via `assert_frame_advancement=True` and the
# `phash_min_*` thresholds below.

{
  system,
  nixpkgs,
  niri-flake,
  halmasuit,
  halmasuit-session,
  halmasuit-vm-client,
  halmasuit-layer-shell-test-client,
}:

let
  pkgs = import nixpkgs {
    inherit system;
    config.allowUnfree = true;
  };
  niri = niri-flake.packages.${system}.niri-unstable;

  niriConfig = pkgs.writeText "niri-pivot-survival-config.kdl" ''
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

  sessionCmd = pkgs.writeShellScript "halmasuit-pivot-survival-session" ''
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
  name = "visual-shutdown-pivot-survival";

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
    # 60-second sine on the R channel; the post-pivot window is wide
    # enough (~700 ms-ish, plus all the System-Power-Off → kernel-halt
    # frames before that) for the 8x8-average phash to flip enough
    # threshold bits to register 3+ distinct buckets. Empirically
    # passes phash_progression(min_distinct=3, min_hamming_max=8)
    # with 3 distinct phashes across ~55 frame_rendered events. A 1s
    # fast variant was tried and proved LESS reliable: faster hue
    # rotation washes out as a global brightness shift, and the
    # perceptual-hash design is robust to that — defeating the test.
    wallpaper = { type = "shader"; source = ./fixtures/wallpaper-shader.glsl; };
    extraStorePaths = [ sessionCmd niri ];
  };

  testScript = ''
    import sys

    sys.path.insert(0, "${./lib}")

    import shutdown_testscript

    shutdown_testscript.run(
        machine,
        cell_name="shutdown-pivot-survival",
        session_cmd="${sessionCmd}",
        # Shader cell: assert the wallpaper-engine tick is driving
        # render_one_frame across the shutdown window (frames=N
        # field advancing) AND that the rendered pixels are actually
        # different across frames (phash progression — closes the
        # "render same frame N times" gap).
        assert_frame_advancement=True,
        # The 1s-period shader gives ~7 distinct phashes across the
        # shutdown window with pairwise Hamming spread up to ~12;
        # thresholds chosen below the empirical floor for stability.
        phash_min_distinct=3,
        phash_min_hamming_max=8,
    )
  '';
}
