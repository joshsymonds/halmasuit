#!/usr/bin/env bash
# tests/lib/nspawn-rig.sh — drive a NixOS toplevel through
# systemd-nspawn for halmasuit's non-pivot-coupled tests.
#
# Why this exists:
#   The full pkgs.testers.runNixOSTest substrate is QEMU-based; even
#   for tests that only need a Linux userspace with halmasuit's
#   broker/PAM stack, QEMU is heavy. systemd-nspawn boots the same
#   userspace against the host's kernel in seconds. The brainstorm
#   research confirmed nspawn can host ~25-30% of halmasuit's test
#   matrix (broker, PAM, greetd protocol, FFI parity, xdg-shell
#   contracts that don't need DRM). Tests that need
#   SurviveFinalKillSignal / switch_root / LUKS-from-initrd stay in
#   nixosTest VMs (kernel-boot territory; nspawn can't replicate it).
#
# Operational requirement:
#   systemd-nspawn requires either root (sudo) or the
#   `CAP_SYS_ADMIN` + `CAP_NET_ADMIN` ambient set. We assume sudo;
#   the wrapper expects to be invoked through `just check-nspawn-*`
#   recipes that pass through sudo. This makes nspawn tests a
#   developer-loop tool rather than a `nix build`-able CI gate —
#   the gnomon-side gnomon production deployment plan does NOT
#   require nspawn tests in CI; they're a faster iteration
#   substrate for halmasuit dev work.
#
# Usage:
#   nspawn-rig.sh <test-name> <toplevel-store-path> <test-commands-script>
#
#   <test-name>              human-readable label for logs / machine name
#   <toplevel-store-path>    output of `nix build` on a NixOS toplevel
#                            (the closure containing `init`, `etc/`, etc.)
#   <test-commands-script>   shell script run INSIDE the container post-boot.
#                            Exits 0 on pass, non-zero on fail. Stdout/stderr
#                            stream to this script's stdout/stderr.

set -euo pipefail

if [ "$#" -ne 3 ]; then
    echo "Usage: nspawn-rig.sh <test-name> <toplevel-store-path> <test-commands-script>" >&2
    exit 2
fi

test_name="$1"
toplevel="$2"
test_script="$3"

if [ "$(id -u)" -ne 0 ]; then
    echo "ERROR: nspawn-rig.sh must run as root (or via sudo)." >&2
    echo "       systemd-nspawn needs CAP_SYS_ADMIN + CAP_NET_ADMIN to boot the container." >&2
    exit 2
fi

if [ ! -e "$toplevel/init" ]; then
    echo "ERROR: $toplevel does not look like a NixOS toplevel (no init script found)." >&2
    exit 2
fi

if [ ! -x "$test_script" ]; then
    echo "ERROR: test-commands-script $test_script is not executable." >&2
    exit 2
fi

# Use a unique machine name so concurrent test runs don't collide
# and so any stale machine from a previous failed run is visible.
machine_name="halmasuit-${test_name}-$$"

# Cleanup function: terminate the machine and remove temp dirs on
# exit. Trapped on EXIT so it fires whether the test passes, fails,
# or is interrupted.
runtime_dir="$(mktemp -d /tmp/halmasuit-nspawn-XXXXXX)"
cleanup() {
    set +e
    machinectl terminate "$machine_name" >/dev/null 2>&1
    rm -rf "$runtime_dir"
}
trap cleanup EXIT

# Set up the container's writable rootfs. The toplevel itself is
# read-only nix store; nspawn needs writable /var, /etc/runtime, etc.
# Use --volatile=overlay so the read-only nix store is the base and
# writes go to a tmpfs overlay that's discarded on container exit.
echo "── nspawn-rig: $test_name (machine=$machine_name) ──"
echo "── booting nspawn against toplevel: $toplevel"

# Boot the container in the background. --quiet suppresses nspawn's
# own banner; we want only the boot stream + test output.
systemd-nspawn \
    --machine="$machine_name" \
    --directory="$toplevel" \
    --boot \
    --volatile=overlay \
    --bind-ro="$test_script:/tmp/run-test.sh" \
    --capability=CAP_SYS_ADMIN \
    --private-users=no \
    --register=yes \
    --keep-unit \
    --quiet \
    >"$runtime_dir/boot.log" 2>&1 &
nspawn_pid=$!

# Wait for the container to reach a running state. machinectl's
# `wait` semantic isn't quite right (it waits for the unit lifecycle,
# not for "userspace booted"); we poll for the container's PID 1
# answering systemd-style.
boot_deadline=$(( $(date +%s) + 60 ))
while true; do
    if [ "$(date +%s)" -gt "$boot_deadline" ]; then
        echo "ERROR: container did not reach running state within 60s." >&2
        echo "── boot.log ──" >&2
        cat "$runtime_dir/boot.log" >&2
        exit 1
    fi
    state=$(machinectl show "$machine_name" --property=State --value 2>/dev/null || true)
    if [ "$state" = "running" ]; then
        # Also wait for multi-user.target to be active inside the container.
        # Without this, the test script may fire before pam / system unit
        # activation completes.
        if machinectl shell "$machine_name" /bin/sh -c \
            "systemctl is-active multi-user.target >/dev/null 2>&1"; then
            break
        fi
    fi
    sleep 0.5
done

echo "── nspawn-rig: container reached multi-user.target; running test script"
echo "──"

# Run the test script inside the container. machinectl shell streams
# stdout/stderr back to the host; the exit code propagates.
if machinectl shell "$machine_name" /bin/sh /tmp/run-test.sh; then
    echo "──"
    echo "── nspawn-rig: $test_name PASSED ──"
    exit 0
else
    rc=$?
    echo "──"
    echo "── nspawn-rig: $test_name FAILED (exit $rc) ──"
    echo "── container boot.log tail ──"
    tail -20 "$runtime_dir/boot.log" || true
    exit 1
fi
