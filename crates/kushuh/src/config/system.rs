//! [`System`] — a named whole-desktop layout: a set of roles.

use std::collections::HashSet;

use thiserror::Error;

use super::Role;

/// A named whole-desktop layout — the unit you switch to as one.
///
/// "System" is canonical project vocabulary (the star-map metaphor: each
/// layout is a *star system* you fly between; its roles are the bodies in
/// it). A `System` is a set of [`Role`]s spanning whatever monitors those
/// roles name; switching systems reshapes the entire desktop at once.
///
/// A `System` *owns* its roles — roles are isolated per system, so a
/// `code` system's stateful apps (a `code`-profile Firefox) are distinct
/// from a `personal` system's. The implicit ambient role for unbound
/// windows is structural and is **not** a member of [`System::roles`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct System {
    name: String,
    roles: Vec<Role>,
}

/// Why a name and role set do not describe a valid [`System`].
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum SystemError {
    /// The system name was empty or blank.
    #[error("system name must be non-empty")]
    EmptyName,
    /// Two roles within the system shared a name.
    #[error("duplicate role name within system: {name}")]
    DuplicateRole {
        /// The role name that appeared more than once.
        name: String,
    },
}

impl System {
    /// Construct a [`System`], validating a non-empty `name` and that role
    /// names are unique within it.
    ///
    /// An empty system (no roles) is valid: it is visible structure — a
    /// star with no planets yet — that later roles can populate.
    ///
    /// # Errors
    ///
    /// [`SystemError::EmptyName`] if `name` is blank;
    /// [`SystemError::DuplicateRole`] if two roles share a name
    /// (case-sensitive, exact match).
    pub fn new(name: impl Into<String>, roles: Vec<Role>) -> Result<Self, SystemError> {
        let name = name.into();
        if name.trim().is_empty() {
            return Err(SystemError::EmptyName);
        }
        let mut seen = HashSet::new();
        for role in &roles {
            if !seen.insert(role.name()) {
                return Err(SystemError::DuplicateRole {
                    name: role.name().to_owned(),
                });
            }
        }
        Ok(Self { name, roles })
    }

    /// The system's name (how it is referenced from keybinds / the
    /// switcher).
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// The roles this system lays out, in definition order.
    #[must_use]
    pub fn roles(&self) -> &[Role] {
        &self.roles
    }
}

#[cfg(test)]
mod tests {
    use super::{System, SystemError};
    use crate::config::{AppRef, Binding, Region, Role};

    fn role(name: &str, monitor: &str) -> Role {
        let region = Region::new(0, 0, 100, 100).expect("full monitor is valid");
        Role::new(name, monitor, region, Binding::Flex).expect("valid role")
    }

    fn kitty_role(name: &str) -> Role {
        let region = Region::new(0, 0, 100, 100).expect("full monitor is valid");
        let app = AppRef::new("kitty", None).expect("valid app");
        Role::new(name, "DP-1", region, Binding::Sticky(app)).expect("valid role")
    }

    #[test]
    fn system_with_roles_is_valid() {
        let system = System::new("code", vec![kitty_role("editor"), role("browser", "DP-2")])
            .expect("valid system");
        assert_eq!(system.name(), "code");
        assert_eq!(system.roles().len(), 2);
    }

    #[test]
    fn empty_system_is_valid() {
        let system = System::new("blank", vec![]).expect("an empty system is valid");
        assert_eq!(system.name(), "blank");
        assert!(system.roles().is_empty());
    }

    #[test]
    fn system_preserves_role_order() {
        let system = System::new(
            "code",
            vec![role("a", "DP-1"), role("b", "DP-1"), role("c", "DP-1")],
        )
        .expect("valid system");
        let names: Vec<&str> = system.roles().iter().map(Role::name).collect();
        assert_eq!(names, ["a", "b", "c"]);
    }

    #[test]
    fn system_rejects_empty_name() {
        assert_eq!(
            System::new("", vec![role("editor", "DP-1")]),
            Err(SystemError::EmptyName),
        );
        assert_eq!(
            System::new("   ", vec![role("editor", "DP-1")]),
            Err(SystemError::EmptyName),
        );
    }

    #[test]
    fn system_rejects_duplicate_role_name() {
        assert_eq!(
            System::new("code", vec![role("editor", "DP-1"), role("editor", "DP-2")]),
            Err(SystemError::DuplicateRole {
                name: "editor".to_owned(),
            }),
        );
    }

    #[test]
    fn system_allows_same_role_name_in_different_systems() {
        // Isolation: an "editor" role can exist in many systems; uniqueness
        // is only required *within* a single system.
        System::new("code", vec![role("editor", "DP-1")]).expect("valid");
        System::new("personal", vec![role("editor", "DP-1")]).expect("valid");
    }
}
