# halmasuit task runner. Every entry-point is here; CI calls the same recipes.
# `just check` is the canonical local gate.

# Default = list available recipes.
default:
    @just --list

# ── Top-level gates ─────────────────────────────────────────────────────────

# Full local CI gate. Run before every push.
check: lint test

# Lint everything (format + clippy + dep policy + spelling + dead deps).
lint:
    cargo fmt --check
    cargo clippy --workspace --all-targets --all-features -- -D warnings
    cargo deny check
    cargo machete
    typos

# Auto-format Rust sources.
fmt:
    cargo fmt

# Run unit + integration tests via nextest. Zero tests is fine while the
# workspace is still scaffolding; nextest is strict about this by default.
test:
    cargo nextest run --workspace --all-features --no-fail-fast --no-tests=pass

# NixOS VM tests (headless, CI-style). smoke-boot + halmasuit-introspect
# must pass; login-flash is expected to FAIL until halmasuit v2 (the failure
# is the v1 baseline measurement of the greetd→niri flash). An unexpected
# pass on login-flash means either v2 just landed (advance!) or the test
# broke.
test-vm:
    #!/usr/bin/env bash
    set -euo pipefail
    echo "── smoke-boot (must pass) ──"
    nix build .#checks.x86_64-linux.smoke-boot -L --print-build-logs --no-link
    echo
    echo "── halmasuit-introspect (must pass) ──"
    nix build .#checks.x86_64-linux.halmasuit-introspect -L --print-build-logs --no-link
    echo
    echo "── login-flash (expected RED until v2) ──"
    if nix build .#checks.x86_64-linux.login-flash -L --print-build-logs --no-link; then
        echo
        echo "ERROR: login-flash unexpectedly PASSED. Either the flash is gone (advance to v2!)"
        echo "       or the test is broken (audit before celebrating)."
        exit 1
    else
        echo
        echo "OK: login-flash FAILED as expected — v1 baseline holds (greetd→niri restart is the flash)."
    fi

# Phase 0 research probe: validate userspace DRM master persistence from
# rootfs boot through multi-user.target. Headless gate; for visual
# verification of the painted red frame, use `just test-vm-drive drm-master-probe`.
test-drm-probe:
    nix build .#checks.x86_64-linux.drm-master-probe -L --print-build-logs --no-link

# Phase 1 research probe: validate userspace DRM master persistence across
# the initramfs→rootfs switch_root boundary, with setresuid privilege drop.
# Probe is started in initramfs via boot.initrd.systemd.services, survives
# switch_root via systemd's @argv[0] convention, drops to UID 1000, asserts
# master still held.
test-drm-probe-phase1:
    nix build .#checks.x86_64-linux.drm-master-probe-phase1 -L --print-build-logs --no-link

# Same VM test, but interactive: opens a QEMU window so you can watch the
# guest boot, and drops you into a Python REPL inside the test driver.
# Useful for `machine.screenshot("name")`, `machine.send_chars(...)`, and
# poking at the VM state by hand.
#
# Usage: just test-vm-interactive smoke-boot
test-vm-interactive name:
    nix run .#checks.x86_64-linux.{{name}}.driverInteractive

# Drive a VM test interactively from elsewhere: the QEMU window opens (so a
# human can watch), and commands are sent by appending to a file. Useful for
# agent-driven debugging where the agent runs commands and the human observes.
#
# After running, two paths are printed:
#   /tmp/halmasuit-drive-cmds  — append Python commands here, one per line
#   /tmp/halmasuit-drive.log   — driver stdout/stderr accumulates here
#
# Send commands:  echo 'start_all()' >> /tmp/halmasuit-drive-cmds
#                 echo 'machine.screenshot("checkpoint")' >> /tmp/halmasuit-drive-cmds
# Watch output:   tail -f /tmp/halmasuit-drive.log
# Stop:           just test-vm-drive-stop
#
# Usage: just test-vm-drive smoke-boot
test-vm-drive name:
    #!/usr/bin/env bash
    set -euo pipefail
    CMDS=/tmp/halmasuit-drive-cmds
    LOG=/tmp/halmasuit-drive.log
    rm -f "$CMDS" "$LOG"
    touch "$CMDS"
    nohup setsid bash -c "exec tail -f $CMDS | nix run .#checks.x86_64-linux.{{name}}.driverInteractive > $LOG 2>&1" > /dev/null 2>&1 &
    disown
    echo "Driver spawned. QEMU window opens when start_all() runs."
    echo
    echo "  Send commands:  echo 'COMMAND' >> $CMDS"
    echo "  Watch output:   tail -f $LOG"
    echo "  Stop:           just test-vm-drive-stop"

# Stop a test-vm-drive session and reap the VM.
test-vm-drive-stop:
    @echo "machine.shutdown()" >> /tmp/halmasuit-drive-cmds 2>/dev/null || true
    @sleep 1
    @pkill -f "nixos-test-driver.*driverInteractive" 2>/dev/null || true
    @pkill -f "qemu-system-x86_64.*machine-test" 2>/dev/null || true
    @pkill -f "tail -f /tmp/halmasuit-drive-cmds" 2>/dev/null || true
    @echo "stopped"

# RustSec advisory check only (subset of `cargo deny check`).
audit:
    cargo deny check advisories

# ── Nightly / per-release gates ─────────────────────────────────────────────

# Mutation testing — slow; run nightly or on release PRs.
mutants:
    cargo mutants --workspace --in-place --no-shuffle

# UB / undefined-behavior detection via miri on unsafe-heavy crates.
miri:
    cargo +nightly miri nextest run --workspace --all-features

# API stability check against the last published version.
semver:
    cargo semver-checks check-release
