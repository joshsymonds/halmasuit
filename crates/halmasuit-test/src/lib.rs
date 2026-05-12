//! Test harness for halmasuit.
//!
//! The next task fills this crate with:
//!   - QMP `screendump` frame-capture loop (≥30Hz background sampling)
//!   - Black-frame detector
//!   - Frame-to-frame DSSIM diff
//!   - Compositor-PID tracker (via /proc inspection in the VM)
//!   - Structured failure reporting (assertion, frame index, timestamp, path)
//!
//! Consumed by `tests/seamless-boot.nix` (NixOS VM test driver, Python) via
//! a small Rust binary the test driver invokes between capture phases.
