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
//! 4. [`Config`] — the top-level model: the set of systems a desktop
//!    offers.
//! 5. The KDL parser (hand-written over the official `kdl` crate).
//! 6. The semantic validator.

use std::collections::HashSet;

use thiserror::Error;

mod parse;
mod region;
mod role;
mod system;

pub use parse::{ParseError, Span, parse};
pub use region::{Region, RegionError};
pub use role::{AppRef, Binding, BindingError, Role, RoleError};
pub use system::{System, SystemError};

/// The whole desktop layout model: the set of [`System`]s a config offers.
///
/// This is the resolved top-level type — schema-agnostic. However the KDL
/// source is written (shared slots, inline, or a role pool), it resolves
/// to a `Config`: a non-empty set of uniquely-named systems.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Config {
    systems: Vec<System>,
}

/// Why a set of systems does not describe a valid [`Config`].
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum ConfigError {
    /// No systems were given — a desktop with no layouts is meaningless.
    #[error("config must define at least one system")]
    Empty,
    /// Two systems shared a name.
    #[error("duplicate system name: {name}")]
    DuplicateSystem {
        /// The system name that appeared more than once.
        name: String,
    },
}

impl Config {
    /// Construct a [`Config`], validating at least one system and unique
    /// system names.
    ///
    /// # Errors
    ///
    /// [`ConfigError::Empty`] if `systems` is empty;
    /// [`ConfigError::DuplicateSystem`] if two systems share a name
    /// (case-sensitive, exact match).
    pub fn new(systems: Vec<System>) -> Result<Self, ConfigError> {
        if systems.is_empty() {
            return Err(ConfigError::Empty);
        }
        let mut seen = HashSet::new();
        for system in &systems {
            if !seen.insert(system.name()) {
                return Err(ConfigError::DuplicateSystem {
                    name: system.name().to_owned(),
                });
            }
        }
        Ok(Self { systems })
    }

    /// The systems this config offers, in definition order.
    #[must_use]
    pub fn systems(&self) -> &[System] {
        &self.systems
    }

    /// The system with the given name, if any.
    #[must_use]
    pub fn system(&self, name: &str) -> Option<&System> {
        self.systems.iter().find(|s| s.name() == name)
    }
}

#[cfg(test)]
mod tests {
    use super::{Config, ConfigError, System};

    fn system(name: &str) -> System {
        System::new(name, vec![]).expect("valid system")
    }

    #[test]
    fn config_with_systems_is_valid() {
        let config = Config::new(vec![system("code"), system("personal")]).expect("valid config");
        assert_eq!(config.systems().len(), 2);
    }

    #[test]
    fn empty_config_is_rejected() {
        assert_eq!(Config::new(vec![]), Err(ConfigError::Empty));
    }

    #[test]
    fn config_rejects_duplicate_system_name() {
        assert_eq!(
            Config::new(vec![system("code"), system("code")]),
            Err(ConfigError::DuplicateSystem {
                name: "code".to_owned(),
            }),
        );
    }

    #[test]
    fn system_lookup_finds_by_name() {
        let config = Config::new(vec![system("code"), system("personal")]).expect("valid config");
        assert_eq!(
            config.system("personal").map(System::name),
            Some("personal")
        );
    }

    #[test]
    fn system_lookup_misses_unknown_name() {
        let config = Config::new(vec![system("code")]).expect("valid config");
        assert!(config.system("meeting").is_none());
    }
}
