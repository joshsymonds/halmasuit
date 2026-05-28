# tests/visual-shutdown-video.nix — Epic #61 R3.5: video cell of the
# wallpaper-shutdown-survival matrix.
#
# Pairs with visual-shutdown-pivot-survival.nix (shader cell) and
# visual-shutdown-image.nix (static cell). Video wallpapers go
# through the same wallpaper-engine tick path as shaders and
# benefit from the R3.1 trait-method fix: render counter advances
# every tick, decoder relay frames keep reaching the screen.
#
# Asserts the same matrix-shared survival invariants the shader cell
# does (post-pivot marker, heartbeat, PID continuity, no coredump)
# AND the animation invariants (frame counter advances across
# shutdown window + phash progression). Video testsrc has high
# inter-frame visual variation but the 8x8-average phash quantizes
# many similar frames into the same hash bucket — empirically 5-6
# distinct phashes across ~75 frames, with large pairwise Hamming
# spread (clearly not frozen). Thresholds chosen accordingly: low
# distinct count, high pairwise Hamming.
#
# Video fixture is generated at build time via `ffmpeg -f lavfi
# testsrc` — same pattern as visual-phase-b-side-video.nix.
#
# Shared rig in tests/lib/shutdown-rig.nix; shared assertion body in
# tests/lib/shutdown_testscript.py.

{
  system,
  nixpkgs,
  nix-config,
  halmasuit,
  halmasuit-decoder,
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

  # 2s, 320x240, 30fps, baseline-profile h264 in mp4 — same shape
  # as visual-phase-b-side-video.nix's videoFixture. testsrc has
  # large inter-frame variation, ideal for phash-progression.
  videoFixture = pkgs.runCommand "shutdown-video-fixture.mp4" {
    nativeBuildInputs = [ pkgs.ffmpeg ];
  } ''
    ffmpeg -y -hide_banner -loglevel error \
      -f lavfi -i 'testsrc=duration=2:size=320x240:rate=30' \
      -c:v libx264 -pix_fmt yuv420p -profile:v baseline -tune zerolatency \
      -movflags +faststart \
      $out
    test -s $out
  '';

  # Solid-blue PNG fallback: the WallpaperEngine swaps to this if
  # the decoder relay's restart budget exhausts. Not exercised in
  # this test (decoder is healthy) but the wallpaper config requires
  # it to be set.
  fallbackFixture = pkgs.runCommand "shutdown-video-fallback.png" {
    nativeBuildInputs = [ pkgs.ffmpeg ];
  } ''
    ffmpeg -y -hide_banner -loglevel error \
      -f lavfi -i 'color=c=blue:s=320x240:d=1' \
      -frames:v 1 \
      $out
    test -s $out
  '';

  niriConfig = pkgs.writeText "niri-shutdown-video-config.kdl" ''
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

  sessionCmd = pkgs.writeShellScript "halmasuit-shutdown-video-session" ''
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
  name = "visual-shutdown-video";

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
    wallpaper = {
      type = "video";
      source = videoFixture;
      loop = true;
      fallback = fallbackFixture;
    };
    decoder = halmasuit-decoder;
    extraStorePaths = [ sessionCmd niri videoFixture fallbackFixture ];
  };

  testScript = ''
    import sys

    sys.path.insert(0, "${./lib}")

    import shutdown_testscript

    shutdown_testscript.run(
        machine,
        cell_name="shutdown-video",
        session_cmd="${sessionCmd}",
        # Video cell: assert the wallpaper-engine tick is driving
        # render_one_frame and that the rendered pixels carry
        # decoder-produced motion (phash progression).
        assert_frame_advancement=True,
        # testsrc's 8x8 phash quantizes to ~5-6 buckets but with
        # large Hamming spread; thresholds reflect that empirical
        # shape (low distinct count, high pairwise Hamming).
        phash_min_distinct=3,
        phash_min_hamming_max=20,
        # The decoder relay produces frames asynchronously; gate
        # shutdown on observing at least one non-zero phash so the
        # phash-progression assertion isn't racing the cold-start
        # window of all-black placeholder frames.
        wait_for_nonzero_phash=True,
    )
  '';
}
