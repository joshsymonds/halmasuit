# tests/lib/nvidia-passthrough.nix — the VFIO-passthrough seam for
# halmasuit's NixOS VM tests (Epic #45).
#
# A NixOS module that turns an otherwise-ordinary `runNixOSTest` guest
# into one that owns the REAL RTX 5070 Ti via VFIO passthrough, so the
# test exercises the actual NVIDIA EGL/GBM path instead of headless
# llvmpipe. Import it into a test's `nodes.machine`.
#
# HARD CONSTRAINT: a test using this module can ONLY run via the
# non-sandboxed driver path on a host with the GPU bound to vfio-pci
# (stygianlibrary) —
#     nix run .#checks.x86_64-linux.<name>.driver
# It CANNOT run via `nix build .#checks…`: the build sandbox has no
# /dev/vfio and no host PCI access. The portable `just test-vm` sweep
# must never pull a passthrough test in. (Epic #45 requirements 1 + 2.)
#
# Host prerequisites (provided by nix-config on stygianlibrary):
#   - the 5070 Ti (IOMMU group 13: 0000:01:00.0 + 0000:01:00.1) bound
#     to vfio-pci at boot,
#   - /dev/vfio/<group> accessible to the runner user (udev → kvm group),
#   - RLIMIT_MEMLOCK unlimited for the runner (qemu pins all guest RAM).
#
# The host BDFs are hardcoded to stygian's group 13 — this rig is
# hardware-specific by definition; there is no portable abstraction to
# build here.
{
  config,
  lib,
  pkgs,
  ...
}:
let
  # IOMMU group 13 on stygianlibrary (hardware-identical to gnomon):
  #   0000:01:00.0 — GB203 [RTX 5070 Ti] VGA
  #   0000:01:00.1 — GB203 HDMI audio (same group; must pass together)
  hostBdfs = [ "0000:01:00.0" "0000:01:00.1" ];

  # Patched open kernel module (Epic #45 req 9): adds a bounded wait for
  # one real scanout vblank inside nvReadCRC32Evo so the HW per-head
  # scanout CRC (read via DRM_IOCTL_NVIDIA_GET_CRTC_CRC32_V2) reflects a
  # fully scanned-out frame. Without it the one-shot read latches the
  # notifier before scanout and returns 0x0 for every tap. Contained to
  # src/nvidia-modeset (nvidia-modeset.ko) — no GSP/firmware change. This
  # is what gives rung 4 a true, content-sensitive post-NVIDIA signal.
  patchedNvidiaOpen =
    config.boot.kernelPackages.nvidiaPackages.production.open.overrideAttrs
      (o: { patches = (o.patches or [ ]) ++ [ ./crc-vblank-wait.patch ]; });
in
{
  # OVMF/UEFI — the GPU's vbios is UEFI; SeaBIOS can't init it.
  virtualisation.useEFIBoot = true;

  # q35 (PCIe-native, NVIDIA strongly prefers it over i440fx) + host CPU
  # passthrough + the GPU functions. Exact device placement is refined
  # empirically on stygian; this is the first cut.
  virtualisation.qemu.options =
    [
      "-machine" "q35,kernel_irqchip=on"
      "-cpu" "host,kvm=on"
    ]
    ++ lib.concatMap
      (bdf: [ "-device" "vfio-pci,host=${bdf},multifunction=on" ])
      [ (builtins.head hostBdfs) ]
    ++ lib.concatMap
      (bdf: [ "-device" "vfio-pci,host=${bdf}" ])
      (builtins.tail hostBdfs);

  # In-guest NVIDIA driver — mirrors gnomon's gpu-nvidia.nix: the open
  # kernel modules (mandatory for Blackwell) + production driver
  # (595.71.05 in this nixpkgs, Blackwell-capable) + modesetting. This
  # is what binds the passed-through card and provides nvidia-smi.
  # (allowUnfree comes from the test's `import nixpkgs { config.… }`;
  # the nixosTest framework pins nixpkgs.config read-only, so we must
  # NOT set it again here.)
  hardware.graphics.enable = true;

  # The NVIDIA userspace EGL/GBM stack in the /run/opengl-driver farm.
  # The inert videoDrivers path never adds it, so libgbm finds only
  # mesa's dri_gbm.so for the nvidia device — "driver (null)", then
  # "Unable to find suitable EGL platform". The DRIVER's `.out` output
  # carries the load-bearing piece: lib/gbm/nvidia-drm_gbm.so (the
  # nvidia GBM backend, → libnvidia-allocator) + libEGL_nvidia. The
  # package's DEFAULT output does NOT have lib/gbm/, which is why an
  # earlier attempt without `.out` left the backend dir mesa-only.
  # Mirrors nixpkgs nvidia.nix, which uses `nvidia_x11.out`. egl-wayland
  # / egl-gbm supply the external-platform JSON descriptors.
  hardware.graphics.extraPackages = [
    config.boot.kernelPackages.nvidiaPackages.production.out
    pkgs.egl-wayland
    pkgs.egl-gbm
  ];
  hardware.nvidia = {
    open = true;
    modesetting.enable = true;
    nvidiaSettings = false;
    package = config.boot.kernelPackages.nvidiaPackages.production;
  };
  services.xserver.videoDrivers = [ "nvidia" ];

  # The `services.xserver.videoDrivers` path does NOT wire the kernel
  # module or the nouveau blacklist in a headless test guest (verified
  # empirically: `modprobe nvidia` → "Module nvidia not found", and
  # nouveau bound the GPU instead, then died on missing GSP firmware).
  # So provide them DIRECTLY, independent of the X activation:
  #   - blacklist nouveau/nvidiafb so they don't grab the device,
  #   - build the OPEN kernel module (mandatory for Blackwell) against
  #     the guest kernel and add it to the module set,
  #   - force-load the nvidia stack at boot (no udev/X trigger here).
  boot.blacklistedKernelModules = [ "nouveau" "nvidiafb" ];
  boot.extraModulePackages = [ patchedNvidiaOpen ];
  boot.kernelModules = [ "nvidia" "nvidia_modeset" "nvidia_uvm" "nvidia_drm" ];

  # Blackwell's open module needs the GSP firmware blob
  # (nvidia/595.71.05/gsp_ga10x.bin) in the firmware search path, or
  # RmInitAdapter fails ("No firmware image found"). nvidia.nix would
  # wire this via `hardware.firmware = [ nvidia_x11.firmware ]`, but
  # that path is inert here — add it directly.
  hardware.firmware = [
    config.boot.kernelPackages.nvidiaPackages.production.firmware
  ];

  # Tools the smoke/diagnostics need in-guest. The inert videoDrivers
  # path does not put nvidia-smi/nvidia-modprobe on PATH, so add the
  # nvidia `bin` output explicitly (nvidia-smi creates the /dev/nvidia*
  # nodes on first use when run as root). pciutils (lspci) likewise
  # isn't in a minimal test guest.
  environment.systemPackages = [
    pkgs.pciutils
    config.boot.kernelPackages.nvidiaPackages.production.bin
  ];

  # Give the guest enough RAM for a real GPU + EGL; passthrough pins all
  # of it (host memlock must cover this).
  virtualisation.memorySize = lib.mkDefault 4096;
  virtualisation.cores = lib.mkDefault 4;
}
