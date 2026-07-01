//! `kushuh` — a role/perspective-based nested Wayland compositor.
//!
//! Where every other Linux compositor treats *position* as primary and
//! *app identity* as secondary, `kushuh` inverts it: a **role** names a
//! position on screen and (optionally) binds an app to it, a
//! **perspective** is a named set of roles, and operations target roles
//! by name rather than positions by direction. See `ARCHITECTURE.md` for
//! the full design and `PLAN.md` for the build sequence.
//!
//! This crate currently hosts the [`config`] domain model — the data
//! structures the entire compositor manipulates, built and validated
//! before any rendering code exists. The smithay compositor binary lands
//! in Phase 2; this crate is deliberately lib-only and pulls in no
//! Wayland/smithay/GPU dependencies yet.
//!
//! `unsafe` is intentionally *not* forbidden at the crate root: Phase 2's
//! direct-DRM / libinput FFI will need it under justified per-item
//! `#![allow]`s. The workspace lints set `unsafe_code = "warn"`, so any
//! unsafe that appears before then is surfaced rather than silently
//! accepted.

pub mod config;
