#!/bin/sh
# Graceful NVIDIA GPU teardown — REQUIRED for iterative VFIO runs on
# Blackwell (Epic #45 rung 5).
#
# Hard-killing qemu before the guest NVIDIA driver quiesces WEDGES the
# passed-through GPU: the GSP/WPR2 firmware state survives FLR, secondary
# bus reset, PCI remove/rescan and driver rebind — there is NO software
# un-wedge on RTX 50-series (community-confirmed; only a host power-cycle
# recovers a wedged card). So every NVIDIA test must, before qemu exits:
# stop any GPU user, unload the nvidia kernel modules WHILE THE GUEST IS
# FULLY ALIVE, then the test driver issues a clean ACPI poweroff
# (machine.shutdown). This lets the guest driver power the GPU down
# properly, so the NEXT run works WITHOUT rebooting the host.
#
# Invoke from each test's testScript as the penultimate step:
#     machine.execute("sh ${./lib/nvidia-teardown.sh}")
#     machine.shutdown()
#
# We cannot use the upstream `nvidia-drm modeset=0` workaround — halmasuit
# is a Wayland/KMS compositor and REQUIRES modeset — so we tear the modeset
# state down explicitly instead.
set -u

# Release the GPU from halmasuit (no-op where halmasuit isn't running).
systemctl stop halmasuit 2>/dev/null || true
sleep 1

# Unload the modeset/display modules first, then the core. Doing this
# while the guest is alive is the teardown path that quiesces GSP and
# avoids the reset wedge; the subsequent clean poweroff is the backstop.
modprobe -r nvidia_drm nvidia_modeset nvidia_uvm nvidia 2>&1 | tail -5 || true
echo "nvidia-teardown: remaining nvidia modules = $(lsmod | grep -c '^nvidia')"
