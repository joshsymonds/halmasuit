# tests/nvidia-kmscube.nix — Epic #45 rung-4 isolation (gambit:debugging).
#
# Isolates WHY halmasuit's scanout is black on the passed-through 5070 Ti
# (real flip, mode set, monitor wakes, but black content — format-
# independent). kmscube is a standard, tiny GBM/EGL/KMS app that draws a
# spinning cube straight to the connector via the SAME present path
# halmasuit uses (GBM scanout buffer → atomic page-flip), but with ZERO
# halmasuit code.
#
#   cube shows  → the GBM/EGL/KMS→monitor path works under VFIO; the
#                 black is halmasuit-specific (its render-into-scanout).
#   cube black  → the NVIDIA GBM-scanout-to-physical-monitor path is
#                 broken in this VM for ANY compositor (driver/qemu/EGL).
#
# RUNNER-ONLY: `just test-vm-nvidia nvidia-kmscube` on stygian, monitor
# attached to DP-2/DP-3.
{
  system,
  nixpkgs,
}:
let
  pkgs = import nixpkgs {
    inherit system;
    config.allowUnfree = true;
  };
in
pkgs.testers.runNixOSTest {
  name = "nvidia-kmscube";
  skipTypeCheck = true;

  nodes.machine =
    { config, pkgs, ... }:
    let
      nvidia = config.boot.kernelPackages.nvidiaPackages.production;
      # kmscube needs the same NVIDIA EGL/GBM env halmasuit's unit sets
      # (libglvnd → libEGL_nvidia + the egl-gbm external platform).
      kmscubeNvidia = pkgs.writeShellScriptBin "kmscube-nvidia" ''
        export __EGL_VENDOR_LIBRARY_FILENAMES=${nvidia.out}/share/glvnd/egl_vendor.d/10_nvidia.json
        export __EGL_EXTERNAL_PLATFORM_CONFIG_DIRS=${pkgs.egl-wayland}/share/egl/egl_external_platform.d:${pkgs.egl-gbm}/share/egl/egl_external_platform.d
        export __GLX_VENDOR_LIBRARY_NAME=nvidia
        export LD_LIBRARY_PATH=/run/opengl-driver/lib
        exec ${pkgs.kmscube}/bin/kmscube -D /dev/dri/card1 "$@"
      '';
    in
    {
      imports = [ ./lib/nvidia-passthrough.nix ]; # GPU + nvidia, NO halmasuit
      environment.systemPackages = [ kmscubeNvidia pkgs.libdrm ];
    };

  testScript = ''
    machine.start()
    machine.wait_for_unit("multi-user.target")
    machine.wait_until_succeeds(
        "lspci -nnk -d 10de: | grep -q 'Kernel driver in use: nvidia'", timeout=60
    )
    # Create /dev/nvidia* (no halmasuit devnodes script here); nvidia-smi
    # does it as root.
    machine.succeed("nvidia-smi > /dev/null")

    print("=== DRM connector status ===\n" +
          machine.execute("for s in /sys/class/drm/card*/status; do echo \"$s=$(cat $s)\"; done")[1])

    # Draw the cube for ~90s. WATCH marker prints right before it starts.
    print("=== WATCH THE PHYSICAL MONITOR NOW for ~90s — expect a SPINNING CUBE "
          "(no halmasuit; pure GBM/EGL/KMS). Black = VFIO scanout path broken. ===")
    out = machine.execute("timeout 90 kmscube-nvidia 2>&1 | tail -50")[1]
    print("=== kmscube output ===\n" + out)
    print("=== kmscube watch done ===")

    # Graceful GPU teardown so the next run works without a host reboot
    # (Blackwell reset wedge — see tests/lib/nvidia-teardown.sh).
    machine.execute("sh ${./lib/nvidia-teardown.sh}")
    machine.shutdown()
  '';
}
