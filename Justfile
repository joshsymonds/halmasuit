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
    cargo +nightly fmt --check
    cargo clippy --workspace --all-targets --all-features -- -D warnings
    cargo deny check
    cargo machete
    typos

# Auto-format Rust sources.
fmt:
    cargo +nightly fmt

# Run unit + integration tests via nextest. Zero tests is fine while the
# workspace is still scaffolding; nextest is strict about this by default.
test:
    cargo nextest run --workspace --all-features --no-fail-fast --no-tests=pass

# NixOS VM tests (frame-capture pipeline against the current desktop stack).
# Stub in v1 — the seamless-boot test lands in a follow-up task.
test-vm:
    @echo "v1 test infrastructure not yet implemented — see Task #10 epic."
    @exit 0

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
