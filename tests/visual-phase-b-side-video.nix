# tests/visual-phase-b-side-video.nix — Phase B golden-boot,
# side-volume LUKS × video wallpaper. Epic #35 cell (side, video).
#
# Same end-to-end claim as visual-phase-b-side-image.nix, with a real
# h264 video wallpaper (ffmpeg-built testsrc, looping) through the
# halmasuit-decoder sandbox + DecoderRelay. PNG fallback armed. The
# no-flash invariant uses the suffix-anchor variant (pre-decode
# startup window excluded — known gap in the video wallpaper backend:
# fallback PNG isn't composited pre-decode, tracked as a follow-up on
# the wallpaper-engine epic). Image and shader cells keep the strict
# frame-0 anchor.
#
# testScript body shared via tests/lib/phase_b_testscript.py.

{
  system,
  nixpkgs,
  niri-flake,
  dms,
  halmasuit-debug,
  halmasuit-decoder,
  halmasuit-luks,
  halmasuit-session,
  halmasuit-vm-client,
  ssimulacra2-cli,
}:

let
  pkgs = import nixpkgs {
    inherit system;
    config.allowUnfree = true;
  };

  # 2s, 320x240, 30fps, baseline-profile h264 in mp4 — short enough
  # that the loop-on-EOF path fires during the test window (same
  # rationale as tests/visual-wallpaper-video.nix).
  videoFixture = pkgs.runCommand "phase-b-side-video.mp4" {
    nativeBuildInputs = [ pkgs.ffmpeg ];
  } ''
    ffmpeg -y -hide_banner -loglevel error \
      -f lavfi -i 'testsrc=duration=2:size=320x240:rate=30' \
      -c:v libx264 -pix_fmt yuv420p -profile:v baseline -tune zerolatency \
      -movflags +faststart \
      $out
    test -s $out
  '';

  # PNG fallback: WallpaperEngine swaps to this if the decoder relay's
  # restart budget exhausts. Solid blue.
  fallbackFixture = pkgs.runCommand "phase-b-side-video-fallback.png" {
    nativeBuildInputs = [ pkgs.ffmpeg ];
  } ''
    ffmpeg -y -hide_banner -loglevel error \
      -f lavfi -i 'color=c=blue:s=320x240:d=1' \
      -frames:v 1 \
      $out
    test -s $out
  '';
in
pkgs.testers.runNixOSTest {
  name = "visual-phase-b-side-video";

  skipTypeCheck = true;

  interactive.nodes.machine = { ... }: {
    virtualisation.qemu.options = [
      "-device virtio-vga-gl"
      "-display gtk,gl=on"
    ];
  };

  nodes.machine = {
    imports = [
      ../nix/module.nix
      (import ./lib/phase-b-golden.nix {
        wallpaper = {
          type = "video";
          source = videoFixture;
          loop = true;
          fallback = fallbackFixture;
        };
        lukshape = "side-volume";
        inherit halmasuit-debug halmasuit-decoder halmasuit-luks
                halmasuit-session halmasuit-vm-client niri-flake dms;
        wallpaperStorePaths = [ videoFixture fallbackFixture ];
      })
    ];
  };

  testScript = ''
    import os
    import sys

    sys.path.insert(0, "${./lib}")
    os.environ["PATH"] = "${ssimulacra2-cli}/bin:" + os.environ.get("PATH", "")
    os.environ.setdefault("GOLDENS_DIR", "${./goldens}")

    import visual
    import phase_b_testscript

    phase_b_testscript.run(
        machine, visual,
        cell_name="phase-b-side-video",
        lukshape="side-volume",
        no_flash_mode="suffix",
    )
  '';
}
