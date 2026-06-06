# tests/nvidia-passthrough-smoke.nix — Epic #45, rung 1.
#
# The bare-substrate smoke: prove a `runNixOSTest` driver-path VM can
# pass through the real RTX 5070 Ti (VFIO) and bind the NVIDIA driver
# inside the guest. NO halmasuit — this isolates the riskiest layer
# (OVMF + KVM + /dev/vfio + memlock + GPU reset) before anything is
# built on top of it.
#
# RUN ONLY via the driver path ON stygianlibrary (GPU bound to
# vfio-pci):
#     just test-vm-nvidia nvidia-passthrough-smoke
# i.e. `nix run .#checks.x86_64-linux.nvidia-passthrough-smoke.driver`.
# It will FAIL under `nix build` (sandbox has no /dev/vfio) — that is
# expected and intended (Epic #45 requirement 1).
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
  name = "nvidia-passthrough-smoke";

  nodes.machine =
    { ... }:
    {
      imports = [ ./lib/nvidia-passthrough.nix ];
    };

  testScript = ''
    machine.start()
    machine.wait_for_unit("multi-user.target")

    # Diagnostics first (non-fatal): root-cause why the nvidia bind
    # fails. Printed every run so a failure carries its own context.
    print("=== cmdline ===\n"               + machine.succeed("cat /proc/cmdline"))
    print("=== modprobe.d nouveau/nvidia ===\n" + machine.succeed("grep -rEl 'nouveau|nvidia' /etc/modprobe.d/ 2>/dev/null | xargs -r grep -E 'nouveau|nvidia' || true"))
    print("=== nvidia.ko present in booted kernel modules? ===\n" + machine.succeed("find /run/booted-system/kernel-modules -name 'nvidia*' 2>/dev/null | head || true"))
    print("=== lsmod nvidia/nouveau ===\n"   + machine.succeed("lsmod | grep -iE 'nvidia|nouveau' || true"))
    print("=== manual modprobe -v nvidia ===\n" + machine.succeed("modprobe -v nvidia 2>&1 || true"))
    print("=== after modprobe lspci ===\n"   + machine.succeed("lspci -nnk -d 10de: || true"))
    print("=== /dev/nvidia* ===\n"           + machine.succeed("ls -l /dev/nvidia* 2>&1 || true"))
    print("=== dmesg nvidia/nouveau/NVRM/gsp ===\n" + machine.succeed("dmesg | grep -iE 'nvidia|nouveau|NVRM|gsp|nvrm' || true"))

    # The passed-through GPU must appear AND be claimed by the nvidia
    # driver in-guest. Dump lspci to stderr unconditionally so a failure
    # shows exactly what the guest saw (device absent? bound to vfio?
    # bound to nouveau?).
    lspci = machine.succeed("lspci -nnk -d 10de:")
    print("=== guest lspci -nnk -d 10de: ===\n" + lspci)
    assert "Kernel driver in use: nvidia" in lspci, (
        "5070 Ti not bound to the nvidia driver inside the guest:\n" + lspci
    )

    # nvidia-smi (as root) creates the /dev/nvidia* nodes on first use
    # and queries the card. It only succeeds if RmInitAdapter succeeded
    # — i.e. the GSP firmware loaded and the passed-through GPU actually
    # initialized. This is the real proof of a working driver stack.
    smi = machine.succeed("nvidia-smi")
    print("=== guest nvidia-smi ===\n" + smi)
    assert "5070 Ti" in smi, (
        "nvidia-smi did not report the RTX 5070 Ti:\n" + smi
    )

    # The character device the userspace stack needs now exists.
    machine.succeed("test -e /dev/nvidia0")

    print("nvidia-passthrough-smoke: GPU passed through and bound. PASS")

    # Graceful GPU teardown so the next run works without a host reboot
    # (Blackwell reset wedge — see tests/lib/nvidia-teardown.sh).
    machine.execute("sh ${./lib/nvidia-teardown.sh}")
    machine.shutdown()
  '';
}
