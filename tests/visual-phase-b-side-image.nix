# tests/visual-phase-b-side-image.nix — Phase B golden-boot,
# side-volume LUKS × image wallpaper. Epic #35 cell (side, image).
#
# The composed end-to-end claim: cold boot into initramfs halmasuit
# (with image wallpaper composited from frame 0) + halmasuit-luks →
# LUKS side-volume unlocks via the production password-agent wire →
# switch_root → halmasuit chroots into rootfs via broker root-fd
# handoff → DankGreeter (DMS Quickshell) spawns as the configured
# greeter → real keyboard input (machine.send_chars) drives alice's
# PAM auth → broker forks-then-drops niri as the session → niri
# nests as a wayland client of halmasuit and maps its toplevel.
#
# Visual coverage layers:
#   1. assert_no_flash_stream over every frame_rendered event from
#      initramfs through session_opened (zero clear/degenerate frames,
#      constant pixel_count, wallpaper plane composited from frame 0).
#   2. SSIMULACRA2 session-scene golden
#      (tests/goldens/phase-b-side-image-session.png).
#   3. Lifecycle event cross-assertions (started → phase_entered
#      sequence including initramfs_init + rootfs_ready + greetd_ready
#      → greeter_spawned → foreground_changed → session_opened →
#      foreground_changed{session}). PID continuous across the swap.
#
# Full testScript body lives in tests/lib/phase_b_testscript.py;
# parametrically shared with the other five cells. This file just wires
# wallpaper variant + LUKS shape + decoder package and dispatches.

{
  system,
  nixpkgs,
  nix-config,
  halmasuit-debug,
  halmasuit-luks,
  halmasuit-session,
  halmasuit-vm-client,
  ssimulacra2-cli,
}:

let
  pkgs = import nixpkgs {
    inherit system;
    # niri-flake's niri-unstable may pull unfree deps transitively
    # (same rationale as visual-niri-session.nix).
    config.allowUnfree = true;
  };
in
pkgs.testers.runNixOSTest {
  name = "visual-phase-b-side-image";

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
          type = "image";
          source = ./fixtures/wallpaper.png;
        };
        lukshape = "side-volume";
        inherit halmasuit-debug halmasuit-luks halmasuit-session
                halmasuit-vm-client nix-config;
        wallpaperStorePaths = [ ./fixtures/wallpaper.png ];
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
        cell_name="phase-b-side-image",
        lukshape="side-volume",
        no_flash_mode="strict",
    )
  '';
}
