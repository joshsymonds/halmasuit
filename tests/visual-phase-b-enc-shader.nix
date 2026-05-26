# tests/visual-phase-b-enc-shader.nix — Phase B golden-boot,
# encrypted-root LUKS × shader wallpaper. Epic #35 cell (enc, shader).
#
# Same end-to-end claim as visual-phase-b-enc-image.nix; only the
# wallpaper variant differs. testScript body shared via
# tests/lib/phase_b_testscript.py.

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
    config.allowUnfree = true;
  };
in
pkgs.testers.runNixOSTest {
  name = "visual-phase-b-enc-shader";

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
          type = "shader";
          source = ./fixtures/wallpaper-shader.glsl;
        };
        lukshape = "encrypted-root";
        inherit halmasuit-debug halmasuit-luks halmasuit-session
                halmasuit-vm-client nix-config;
        wallpaperStorePaths = [ ./fixtures/wallpaper-shader.glsl ];
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
        cell_name="phase-b-enc-shader",
        lukshape="encrypted-root",
        no_flash_mode="strict",
    )
  '';
}
