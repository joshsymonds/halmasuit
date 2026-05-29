//! The `kushuh` config domain model.
//!
//! Pure data structures, built bottom-up and test-first, that describe a
//! role-based desktop layout. No Wayland, no smithay, no I/O beyond
//! parsing a KDL string — this module is the cheap, fast-to-test spine
//! that the Phase 3 layout engine and the star-map projection both
//! operate on.
//!
//! Build sequence (see `PLAN.md`):
//!
//! 1. [`Region`] — the atomic geometry leaf: a rectangle in
//!    monitor-percentage space.
//! 2. [`Role`] — a named position (monitor + [`Region`]) with a
//!    [`Binding`] that decides what fills it.
//! 3. [`System`] — a named set of roles: one whole-desktop layout you
//!    switch to (the project's "perspective", canonically a *system*).
//! 4. `Config` / `Layout` — the top-level model, separating role
//!    *definitions* from role *bindings*.
//! 5. The KDL parser (hand-written over the official `kdl` crate).
//! 6. The semantic validator.

mod region;
mod role;
mod system;

pub use region::{Region, RegionError};
pub use role::{AppRef, Binding, BindingError, Role, RoleError};
pub use system::{System, SystemError};
