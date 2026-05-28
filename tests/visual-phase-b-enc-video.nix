# tests/visual-phase-b-enc-video.nix — Phase B golden-boot,
# encrypted-root LUKS × video wallpaper. Epic #35 cell (enc, video).
# Final matrix cell.
#
# Combines enc-image's specialisation pattern (lukshape =
# "encrypted-root") with side-video's wallpaper config + relaxed
# suffix-anchored no-flash. testScript body shared via
# tests/lib/phase_b_testscript.py.

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

  videoFixture = pkgs.runCommand "phase-b-enc-video.mp4" {
    nativeBuildInputs = [ pkgs.ffmpeg ];
  } ''
    ffmpeg -y -hide_banner -loglevel error \
      -f lavfi -i 'testsrc=duration=2:size=320x240:rate=30' \
      -c:v libx264 -pix_fmt yuv420p -profile:v baseline -tune zerolatency \
      -movflags +faststart \
      $out
    test -s $out
  '';

  fallbackFixture = pkgs.runCommand "phase-b-enc-video-fallback.png" {
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
  name = "visual-phase-b-enc-video";

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
        lukshape = "encrypted-root";
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
        cell_name="phase-b-enc-video",
        lukshape="encrypted-root",
        no_flash_mode="suffix",
    )
  '';
}
