# tests/visual-phase-b-side-shader-gl.nix — Phase B golden-boot
# variant exercising the hardware-accelerated virtio-gpu-gl
# substrate instead of headless virtio-gpu-pci.
#
# Why: the default headless virtio-gpu-pci has no GBM allocator
# (CLAUDE.md: "niri runs but paints nothing, screenshots are solid
# black"). Several frame-timing + GBM-allocator paths in halmasuit
# only get exercised when there's a real GL backend on the substrate.
# This cell catches that class of regression.
#
# Operational note:
#   This test requires the test runner to expose a DRM render node
#   (typically /dev/dri/renderD128). On gnomon itself the render
#   node is held by halmasuit (the host compositor), so running this
#   test on gnomon means stopping halmasuit first — same operational
#   cost as VFIO. On CI / dev machines with a free GPU it runs in
#   the normal test sweep.
#
#   For that reason this cell is NOT in the default `just test-vm`
#   sweep — run explicitly via `just test-vm-virtio-gpu-gl`.
#
# Coverage scope:
#   Tier between "software" (LIBGL_ALWAYS_SOFTWARE=1 in the
#   existing matrix) and "real hardware" (deploy-time validation
#   on gnomon). The guest in-process still sees virtio-gpu, NOT
#   nvidia-drm — research established no API-level sharing
#   mechanism delivers nvidia-drm to a guest while the host keeps
#   the GPU. So this cell does NOT validate halmasuit's nvidia path;
#   it validates the GBM allocator + real frame timing on whatever
#   real GL substrate the host provides.

{
  system,
  nixpkgs,
  niri-flake,
  dms,
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
  name = "visual-phase-b-side-shader-gl";

  skipTypeCheck = true;

  # Same nodes.machine config as visual-phase-b-side-shader, plus
  # the hardware-accel QEMU substrate. APPEND to qemu.options — do
  # not mkForce, the framework adds -kernel/-initrd/-append via
  # mkIf cfg.directBoot.enable and mkForce wipes those out. QEMU is
  # fine with multiple -device args (virtio-vga-gl coexists
  # alongside the base virtio-gpu-pci).
  nodes.machine = {
    imports = [
      ../nix/module.nix
      (import ./lib/phase-b-golden.nix {
        wallpaper = {
          type = "shader";
          source = ./fixtures/wallpaper-shader.glsl;
        };
        lukshape = "side-volume";
        inherit halmasuit-debug halmasuit-luks halmasuit-session
                halmasuit-vm-client niri-flake dms;
        wallpaperStorePaths = [ ./fixtures/wallpaper-shader.glsl ];
      })
      ({ ... }: {
        virtualisation.qemu.options = [
          "-device virtio-vga-gl"
          "-display egl-headless,rendernode=/dev/dri/renderD128"
        ];
      })
    ];
  };

  interactive.nodes.machine = { ... }: {
    virtualisation.qemu.options = [
      "-device virtio-vga-gl"
      "-display gtk,gl=on"
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
        cell_name="phase-b-side-shader-gl",
        lukshape="side-volume",
        no_flash_mode="strict",
    )
  '';
}
