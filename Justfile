# halmasuit task runner. Every entry-point is here; CI calls the same recipes.
# `just check` is the canonical local gate.

# Default = list available recipes.
default:
    @just --list

# ── Top-level gates ─────────────────────────────────────────────────────────

# Full local CI gate. Run before every push.
check: lint r14-gate vis-selftest test

# R14 epic close-gate (Amendment A9 fold-in, review G3/F3): the
# unprivileged compositor must never transitively link libpam. Exactly
# ONE workspace crate (`halmasuit-session`) links `pam-sys`; `halmasuit`
# with no features must show none. Structurally true today but
# previously unenforced — a future dep edit could regress the
# single-libpam-surface invariant silently. This makes it a hard gate.
r14-gate:
    #!/usr/bin/env bash
    set -euo pipefail
    if cargo tree -p halmasuit --no-default-features -e normal 2>/dev/null | grep -qw pam-sys; then
        echo "R14 VIOLATION (epic close-gate / Amendment A9 fold-in):"
        echo "  halmasuit transitively links pam-sys. The compositor must"
        echo "  hold NO libpam surface — only halmasuit-session may link it."
        cargo tree -p halmasuit --no-default-features -e normal 2>/dev/null | grep -n pam-sys || true
        exit 1
    fi
    echo "R14 close-gate OK: halmasuit (no features) links no pam-sys."

# Synthetic negative-stream proof for the no-flash invariant
# (`assert_no_flash_stream`, epic R3/R9). Runs the contract test in
# tests/lib/visual.py with NO VM/GPU: a clean frame-0-anchored stream
# must pass and every flaw class (incl. the frame-0-anchor
# strengthening — a frame_rendered preceding the wallpaper cff) must be
# rejected. A hard gate so the load-bearing assertion can never
# silently weaken.
vis-selftest:
    python3 tests/lib/visual.py

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
    echo "── halmasuit-vm ──"
    nix build .#checks.x86_64-linux.halmasuit-vm -L --print-build-logs --no-link
    echo
    echo "── initrd-survival ──"
    nix build .#checks.x86_64-linux.initrd-survival -L --print-build-logs --no-link
    echo
    echo "── halmasuit-shutdown-probe-phase0 ──"
    nix build .#checks.x86_64-linux.halmasuit-shutdown-probe-phase0 -L --print-build-logs --no-link
    echo
    echo "── halmasuit-shutdown-probe-phase1 ──"
    nix build .#checks.x86_64-linux.halmasuit-shutdown-probe-phase1 -L --print-build-logs --no-link
    echo
    echo "── halmasuit-shutdown-probe-phase2 ──"
    nix build .#checks.x86_64-linux.halmasuit-shutdown-probe-phase2 -L --print-build-logs --no-link
    echo
    echo "── full-boot-flash ──"
    nix build .#checks.x86_64-linux.full-boot-flash -L --print-build-logs --no-link
    echo
    echo "── luks-unlock ──"
    nix build .#checks.x86_64-linux.luks-unlock -L --print-build-logs --no-link
    echo
    echo "── visual-initrd-pixmap ──"
    nix build .#checks.x86_64-linux.visual-initrd-pixmap -L --print-build-logs --no-link
    echo
    echo "── visual-phase-b-side-image ──"
    nix build .#checks.x86_64-linux.visual-phase-b-side-image -L --print-build-logs --no-link
    echo
    echo "── visual-phase-b-side-shader ──"
    nix build .#checks.x86_64-linux.visual-phase-b-side-shader -L --print-build-logs --no-link
    echo
    echo "── visual-phase-b-side-video ──"
    nix build .#checks.x86_64-linux.visual-phase-b-side-video -L --print-build-logs --no-link
    echo
    echo "── visual-phase-b-enc-image ──"
    nix build .#checks.x86_64-linux.visual-phase-b-enc-image -L --print-build-logs --no-link
    echo
    echo "── visual-phase-b-enc-shader ──"
    nix build .#checks.x86_64-linux.visual-phase-b-enc-shader -L --print-build-logs --no-link
    echo
    echo "── visual-phase-b-enc-video ──"
    nix build .#checks.x86_64-linux.visual-phase-b-enc-video -L --print-build-logs --no-link
    echo
    echo "── run-pam-auth ──"
    nix build .#checks.x86_64-linux.run-pam-auth -L --print-build-logs --no-link
    echo
    echo "── session-r5r6 ──"
    nix build .#checks.x86_64-linux.session-r5r6 -L --print-build-logs --no-link
    echo
    echo "── session-onehandle ──"
    nix build .#checks.x86_64-linux.session-onehandle -L --print-build-logs --no-link
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
    echo
    echo "── visual-halmasuit-toplevel ──"
    nix build .#checks.x86_64-linux.visual-halmasuit-toplevel -L --print-build-logs --no-link
    echo
    echo "── visual-foreground ──"
    nix build .#checks.x86_64-linux.visual-foreground -L --print-build-logs --no-link
    echo
    echo "── visual-revert ──"
    nix build .#checks.x86_64-linux.visual-revert -L --print-build-logs --no-link
    echo
    echo "── visual-pidfd-revert ──"
    nix build .#checks.x86_64-linux.visual-pidfd-revert -L --print-build-logs --no-link
    echo
    # Real broker-launched niri, software-rendered headless (llvmpipe).
    echo "── visual-niri-session ──"
    nix build .#checks.x86_64-linux.visual-niri-session -L --print-build-logs --no-link
    echo
    echo "── visual-logout-respawn ──"
    nix build .#checks.x86_64-linux.visual-logout-respawn -L --print-build-logs --no-link
    echo
    echo "── visual-shutdown-tear-down ──"
    nix build .#checks.x86_64-linux.visual-shutdown-tear-down -L --print-build-logs --no-link
    echo
    echo "── visual-shutdown-pivot-survival ──"
    nix build .#checks.x86_64-linux.visual-shutdown-pivot-survival -L --print-build-logs --no-link
    echo
    # R13 forcing function: real DMS DankGreeter (Quickshell+Qt6) as
    # halmasuit's greeter over the wallpaper, no-flash invariant intact.
    echo "── visual-dankgreeter ──"
    nix build .#checks.x86_64-linux.visual-dankgreeter -L --print-build-logs --no-link
    echo
    # R12 (GTK4 half): minimal GTK4 wayland client end-to-end.
    echo "── visual-gtk4-smoke ──"
    nix build .#checks.x86_64-linux.visual-gtk4-smoke -L --print-build-logs --no-link
    echo
    # R13(b): G3 keystroke auth arc end-to-end through real DMS
    # DankGreeter → broker → real pam_unix → session_opened.
    echo "── visual-dankgreeter-auth ──"
    nix build .#checks.x86_64-linux.visual-dankgreeter-auth -L --print-build-logs --no-link
    echo
    # Convergence epic R2: wl_surface.frame callbacks (no Mesa-EGL wedge).
    echo "── visual-frame-callbacks ──"
    nix build .#checks.x86_64-linux.visual-frame-callbacks -L --print-build-logs --no-link
    echo
    # Convergence epic R3: sync-subsurface commits aggregate to parent.
    echo "── visual-sync-subsurface ──"
    nix build .#checks.x86_64-linux.visual-sync-subsurface -L --print-build-logs --no-link
    echo
    # Convergence epic R4: initial xdg_surface.configure deferred to commit handler.
    echo "── visual-deferred-configure ──"
    nix build .#checks.x86_64-linux.visual-deferred-configure -L --print-build-logs --no-link
    echo
    # Convergence epic R5: smithay PopupManager + positioner-driven popup geometry.
    echo "── visual-popup ──"
    nix build .#checks.x86_64-linux.visual-popup -L --print-build-logs --no-link
    echo
    # Epic #12: real halmasuit-decoder sandbox + crash-recovery +
    # budget-exhaustion + login-flash continuity under video wallpaper.
    echo "── visual-wallpaper-video ──"
    nix build .#checks.x86_64-linux.visual-wallpaper-video -L --print-build-logs --no-link

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

# Epic #47 R2 Phase 0 probe: empirical validation that
# SurviveFinalKillSignal=yes on a rootfs unit keeps the process alive
# through systemd-shutdown's "Sending SIGKILL to remaining processes"
# kill spree. Phase 2 of drm-master-probe proved the BOOT direction;
# this proves the SHUTDOWN direction. Probe writes heartbeats to
# /dev/kmsg, test inspects qemu's serial console capture post-halt
# for heartbeats appearing after the SIGKILL marker. Sub-phases 1
# and 2 (pivot survival + DRM master survival) land in follow-up
# tasks of Epic #47.
test-shutdown-probe-phase0:
    nix build .#checks.x86_64-linux.halmasuit-shutdown-probe-phase0 -L --print-build-logs --no-link

# Epic #47 R2 Phase 1 probe: same-PID survival across
# systemd-shutdown's pivot from rootfs to /run/initramfs. Adds
# `boot.initrd.systemd.shutdownRamfs.storePaths` to include the
# probe binary + closure so its executable + libs are backed by the
# shutdownRamfs tmpfs (not the about-to-unmount rootfs). Asserts
# heartbeats appear after the first `shutdown[1]:` log line (the
# post-pivot systemd-shutdown binary), proving the pivot didn't
# kill the probe.
test-shutdown-probe-phase1:
    nix build .#checks.x86_64-linux.halmasuit-shutdown-probe-phase1 -L --print-build-logs --no-link

# Epic #47 R2 Phase 2 probe (THE risky one): opens /dev/dri/card0,
# takes DRM master, paints magenta, then per-heartbeat re-issues
# set_crtc to assert both master + fd validity. Passing means the
# whole Epic #47 R2 ("wallpaper through shutdown to kernel halt")
# design is empirically grounded. Failing means we fall back to
# the partial-scope alternative (paint until SIGKILL).
test-shutdown-probe-phase2:
    nix build .#checks.x86_64-linux.halmasuit-shutdown-probe-phase2 -L --print-build-logs --no-link

# Phase B initrd-survival gate: the production halmasuit binary
# registered via `services.halmasuit.fromInitrd.enable`. Composes
# RESEARCH.md Phase 2 mechanism (`SurviveFinalKillSignal=yes` in
# unitConfig) with halmasuit's runtime initramfs detection. Asserts
# PID + DRM-master + Wayland-socket continuity across switch_root,
# and that the NDJSON event stream emits both `initramfs_init`
# (pre-pivot) and `rootfs_ready` (post-pivot) from the SAME pid.
test-vm-initrd-survival:
    nix build .#checks.x86_64-linux.initrd-survival -L --print-build-logs --no-link

# Boot-size regression gate. Builds a Phase B halmasuit initramfs
# and fails if it exceeds the threshold encoded in
# tests/initrd-size-gate.nix. Catches closure regressions before
# they hit deployment-time ESP overflow. Threshold bumps are
# deliberate — document the reason in the commit message.
check-initrd-size:
    nix build .#checks.x86_64-linux.initrd-size-gate -L --print-build-logs --no-link

# Phase B hard gate: full LUKS-backed boot + survival + chroot +
# greeter spawn + PAM auth → SessionOpened end-to-end.
test-vm-full-boot-flash:
    nix build .#checks.x86_64-linux.full-boot-flash -L --print-build-logs --no-link

# Phase B LUKS unlock gate: real cryptsetup + real
# systemd-cryptsetup ask-password producer + halmasuit-luks in
# non-interactive responder mode actually unlocks a real LUKS
# volume. Isolates the wire contract end-to-end without depending
# on a virtual-keyboard substrate for the Wayland UI path.
test-vm-luks-unlock:
    nix build .#checks.x86_64-linux.luks-unlock -L --print-build-logs --no-link

# Phase B kernel-handoff-to-session pixmap continuity gate. Extends
# the rootfs visual-* family's exact-stream no-flash mechanism
# (frame_audit + assert_no_flash_stream) to the boot-from-initrd
# timeline. The strongest empirical statement that halmasuit owns
# the pixel pipeline continuously — the Plymouth-removability proof.
test-vm-visual-initrd-pixmap:
    nix build .#checks.x86_64-linux.visual-initrd-pixmap -L --print-build-logs --no-link

# Phase B golden-boot, side-volume LUKS × image wallpaper (Epic #35,
# first cell of the matrix). Full composition: initramfs halmasuit +
# halmasuit-luks → LUKS side-volume unlocks via the production wire →
# switch_root → DankGreeter (real keyboard via machine.send_chars) →
# alice's real pam_unix auth → niri spawned by broker → goldens at
# greeter scene + session scene + no-flash invariant across the
# whole timeline.
test-vm-visual-phase-b-side-image:
    nix build .#checks.x86_64-linux.visual-phase-b-side-image -L --print-build-logs --no-link

# Phase B cell — LUKS side-volume × shader wallpaper. Same end-to-end
# arc; animated GLSL fragment shader (iTime-driven sine hue cycle, 60s
# period for SSIMULACRA2 golden stability) replaces the image plane.
test-vm-visual-phase-b-side-shader:
    nix build .#checks.x86_64-linux.visual-phase-b-side-shader -L --print-build-logs --no-link

# Phase B cell — LUKS side-volume × video wallpaper. Real h264 (built
# with ffmpeg's testsrc) looped through halmasuit-decoder's sandbox +
# DecoderRelay; PNG fallback armed (a fallback swap during the run is
# itself caught by assert_no_flash_stream).
test-vm-visual-phase-b-side-video:
    nix build .#checks.x86_64-linux.visual-phase-b-side-video -L --print-build-logs --no-link

# Phase B cell — LUKS-encrypted ROOT × image wallpaper. Same arc but
# the rootfs itself is on a LUKS volume; dual-boot specialisation
# pattern unlocks /dev/mapper/cryptroot in initramfs via the
# halmasuit-luks ask-password responder.
test-vm-visual-phase-b-enc-image:
    nix build .#checks.x86_64-linux.visual-phase-b-enc-image -L --print-build-logs --no-link

# Phase B cell — LUKS-encrypted ROOT × shader wallpaper.
test-vm-visual-phase-b-enc-shader:
    nix build .#checks.x86_64-linux.visual-phase-b-enc-shader -L --print-build-logs --no-link

# Phase B cell — LUKS-encrypted ROOT × video wallpaper (final cell
# of the 6-cell matrix).
test-vm-visual-phase-b-enc-video:
    nix build .#checks.x86_64-linux.visual-phase-b-enc-video -L --print-build-logs --no-link

# Same VM test, but interactive: opens a QEMU window so you can watch the
# guest boot, and drops you into a Python REPL inside the test driver.
# Useful for `machine.screenshot("name")`, `machine.send_chars(...)`, and
# poking at the VM state by hand.
#
# Usage: just test-vm-interactive halmasuit-vm
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
# Usage: just test-vm-drive halmasuit-vm
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

# Regenerate crates/halmasuit-decoder/ffmpeg_binding.rs from the
# devShell-pinned ffmpeg-headless headers. Bindgen runs ONCE here;
# the resulting bindings.rs is checked in, and the production flake
# derivation reuses them via FFMPEG_BINDING_PATH (no libclang at
# production build time — Epic #12 task #28 / Epic #5 commitment).
#
# Run this when the ffmpeg-headless pin changes (rare). The git
# diff will be enormous (27K lines of generated bindings); inspect
# the commit message, not the diff.
regenerate-decoder-bindings:
    #!/usr/bin/env bash
    set -euo pipefail
    echo "Regenerating crates/halmasuit-decoder/ffmpeg_binding.rs"
    echo "This invokes rusty_ffmpeg's build.rs bindgen against the"
    echo "current devShell's ffmpeg-headless headers."
    echo
    # The build needs libclang + ffmpeg headers in scope; devShell
    # provides both. Force a rebuild by removing the rusty_ffmpeg
    # build artifact dir so bindgen reruns even if no source changed.
    rm -rf target/debug/build/rusty_ffmpeg-*
    cargo build -p halmasuit-decoder
    # Pick the most-recently-built binding.rs (there may be stale
    # ones from prior incremental builds with different feature
    # flag sets).
    src="$(ls -t target/debug/build/rusty_ffmpeg-*/out/binding.rs | head -1)"
    dst="crates/halmasuit-decoder/ffmpeg_binding.rs"
    cp "$src" "$dst"
    echo "Wrote $dst (size: $(wc -c < $dst) bytes)"
    echo "Inspect the commit diff and ship if it looks like a pure"
    echo "ffmpeg-version bump."

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

