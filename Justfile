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

# NixOS VM tests (headless, CI-style). All gates are hard gates now —
# login-flash is GREEN under halmasuit v2 (the long-lived compositor
# preserves PID continuity across greeter→session). The inversion that
# the v1 baseline depended on is gone.
test-vm:
    #!/usr/bin/env bash
    set -euo pipefail
    echo "── smoke-boot ──"
    nix build .#checks.x86_64-linux.smoke-boot -L --print-build-logs --no-link
    echo
    echo "── halmasuit-vm ──"
    nix build .#checks.x86_64-linux.halmasuit-vm -L --print-build-logs --no-link
    echo
    echo "── halmasuit-spawn ──"
    nix build .#checks.x86_64-linux.halmasuit-spawn -L --print-build-logs --no-link
    echo
    echo "── login-flash ──"
    nix build .#checks.x86_64-linux.login-flash -L --print-build-logs --no-link
    echo
    echo "── halmasuit-input ──"
    nix build .#checks.x86_64-linux.halmasuit-input -L --print-build-logs --no-link
    echo
    echo "── visual-halmasuit-clear ──"
    nix build .#checks.x86_64-linux.visual-halmasuit-clear -L --print-build-logs --no-link
    echo
    echo "── visual-halmasuit-layer ──"
    nix build .#checks.x86_64-linux.visual-halmasuit-layer -L --print-build-logs --no-link
    echo
    echo "── visual-halmasuit-splash ──"
    nix build .#checks.x86_64-linux.visual-halmasuit-splash -L --print-build-logs --no-link
    echo
    echo "── visual-backdrop ──"
    nix build .#checks.x86_64-linux.visual-backdrop -L --print-build-logs --no-link

# Regenerate one or all visual-test goldens. Runs the named test
# interactively (driverInteractive), with HALMASUIT_GOLDEN_REGEN=1 and
# GOLDENS_DIR pointing at the source tree's tests/goldens so visual.py
# writes the captured PNGs in-place instead of comparing.
#
# IMPORTANT: After regeneration, manually inspect each updated golden
# before committing. A broken renderer that paints garbage will happily
# overwrite a previously-good golden with garbage; the human in the
# loop is the only check.
#
# Usage:
#   just update-goldens visual-halmasuit-clear   # one named test
#   just update-goldens visual-halmasuit-layer
update-goldens name="visual-halmasuit-clear":
    #!/usr/bin/env bash
    set -euo pipefail
    echo "Regenerating goldens for: {{name}}"
    echo "Source-tree goldens dir:  $(pwd)/tests/goldens"
    echo
    echo "Press Ctrl-C now to cancel; otherwise the captured PNG will"
    echo "OVERWRITE the existing golden. Inspect the result before committing."
    echo
    read -p "Continue? [y/N] " -n 1 -r REPLY
    echo
    if [[ ! $REPLY =~ ^[Yy]$ ]]; then
        echo "Aborted."
        exit 1
    fi
    HALMASUIT_GOLDEN_REGEN=1 \
    GOLDENS_DIR="$(pwd)/tests/goldens" \
    nix run .#checks.x86_64-linux.{{name}}.driver
    echo
    echo "Done. New golden(s) in $(pwd)/tests/goldens — inspect, then commit."

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

# Phase 2 research probe: test whether systemd's SurviveFinalKillSignal=yes
# (v255+) is a viable replacement for the @argv[0] mechanism Phase 1 uses.
# Same probe binary, PROBE_SKIP_ARGV0_MARK=1 env, SurviveFinalKillSignal=yes
# on the unit. Phase 2 passing means we have a supported upgrade path off
# the storage-only @argv[0] convention; failing means @argv[0] remains the
# only mechanism and we accept its upstream-policy risk.
test-drm-probe-phase2:
    nix build .#checks.x86_64-linux.drm-master-probe-phase2 -L --print-build-logs --no-link

# Phase 3 research probe: test whether halmasuit-in-initramfs can execve
# into the rootfs-resident binary path across switch_root while preserving
# its DRM master fd. Composes with Phase 2 (SurviveFinalKillSignal=yes for
# survival). Phase 3 passing means exec is a viable mechanism for v2's
# clean sd_notify handoff to rootfs systemd; failing means we stay with
# the orphan-unit-with-SIGTERM-ignore-handler pattern.
test-drm-probe-phase3:
    nix build .#checks.x86_64-linux.drm-master-probe-phase3 -L --print-build-logs --no-link

# Phase 4 research probe: gate for epic layer E (#11). Validates that a
# libseat/seatd-brokered session (DRM master + libinput fds +
# session-active) survives setresuid to a non-root uid — the inversion
# of Phases 0-3 (seatd brokers the fd instead of self-SET_MASTER).
# Passing means halmasuit can adopt libseat without regressing the
# privilege model; the conclusion is recorded in RESEARCH.md Phase 4.
test-drm-probe-phase4:
    nix build .#checks.x86_64-linux.drm-master-probe-phase4 -L --print-build-logs --no-link

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

# Fuzz halmasuit-spawn's parse_argv for `seconds` seconds (default 10).
# Requires `cargo install cargo-fuzz` + `rustup toolchain install nightly`
# locally; not run in CI (cargo-fuzz needs nightly and is hostile to
# sandboxes). The fuzz/ subworkspace is excluded from the main workspace,
# so `just check` is unaffected.
#
# Mutation-verify by inserting `panic!("seed")` into parse_argv and
# running for 30s — libfuzzer should crash within seconds.
fuzz-spawn seconds="10":
    cd crates/halmasuit-spawn/fuzz && \
        cargo +nightly fuzz run parse_argv -- -max_total_time={{seconds}}
