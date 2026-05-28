//! [`Role`] — a named position on a monitor with an app [`Binding`].

use thiserror::Error;

use super::Region;

/// A reference to a particular app *instance* a slot launches and owns.
///
/// `app_id` is the application (matched against a window's Wayland
/// `app_id`); `profile` optionally distinguishes separate-state instances
/// of the same app — a `code` Firefox and a `personal` Firefox are one
/// `app_id` with different profiles, launched as separate processes with
/// independent history and logins. The model only records the label; how
/// it maps to a launch (`firefox -P code`) is a Phase 3 runtime concern.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppRef {
    app_id: String,
    profile: Option<String>,
}

/// The rule for which window is attracted to a [`Role`]'s slot.
///
/// A binding records *what belongs here*, not how many windows pile in or
/// how they are displayed — that is the layout engine's concern (Phase 3).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Binding {
    /// Launch and own one specific app instance (the editor's `kitty`, the
    /// Code Firefox). `kushuh` spawns it if absent.
    Sticky(AppRef),
    /// Host one of several candidate instances; the user swaps which is
    /// shown (a spotify-or-claude nook). At least two candidates.
    Cycle(Vec<AppRef>),
    /// Catch an *existing* window by `app_id` + optional title regex and
    /// route it here — the app spawns the window itself (the Zoom meeting
    /// window, a reading-titled Firefox); `kushuh` does not launch it.
    Pattern {
        /// The application whose window to catch.
        app_id: String,
        /// Optional title regex; `None` catches any window of `app_id`.
        title_regex: Option<String>,
    },
    /// No rule — a free / scratch slot. Whatever is placed here by hand
    /// stays.
    Flex,
}

/// A named position on a monitor: a [`Region`] plus the [`Binding`] that
/// decides what fills it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Role {
    name: String,
    monitor: String,
    region: Region,
    binding: Binding,
}

/// Why a set of values does not describe a valid [`AppRef`] or [`Binding`].
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum BindingError {
    /// An `app_id` was empty or blank.
    #[error("app id must be non-empty")]
    EmptyAppId,
    /// A `profile` label was present but empty or blank.
    #[error("profile label must be non-empty when present")]
    EmptyProfile,
    /// A [`Binding::Cycle`] had fewer than two candidates.
    #[error("a cycle binding needs at least two candidates")]
    CycleNeedsCandidates,
    /// A [`Binding::Pattern`] title regex was present but empty or blank.
    #[error("title pattern must be non-empty when present")]
    EmptyTitle,
}

/// Why a set of values does not describe a valid [`Role`].
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum RoleError {
    /// The role name was empty or blank.
    #[error("role name must be non-empty")]
    EmptyName,
    /// The role's monitor reference was empty or blank.
    #[error("role monitor must be non-empty")]
    EmptyMonitor,
}

impl AppRef {
    /// Construct an [`AppRef`], validating a non-empty `app_id` and (if
    /// present) a non-empty `profile`.
    ///
    /// # Errors
    ///
    /// [`BindingError::EmptyAppId`] if `app_id` is blank;
    /// [`BindingError::EmptyProfile`] if `profile` is `Some` but blank.
    pub fn new(app_id: impl Into<String>, profile: Option<String>) -> Result<Self, BindingError> {
        let app_id = app_id.into();
        if app_id.trim().is_empty() {
            return Err(BindingError::EmptyAppId);
        }
        if profile.as_ref().is_some_and(|p| p.trim().is_empty()) {
            return Err(BindingError::EmptyProfile);
        }
        Ok(Self { app_id, profile })
    }

    /// The application id (matched against a window's Wayland `app_id`).
    #[must_use]
    pub fn app_id(&self) -> &str {
        &self.app_id
    }

    /// The separate-state instance label, if any.
    #[must_use]
    pub fn profile(&self) -> Option<&str> {
        self.profile.as_deref()
    }
}

impl Binding {
    /// Construct a [`Binding::Cycle`], validating at least two candidates.
    ///
    /// # Errors
    ///
    /// [`BindingError::CycleNeedsCandidates`] if fewer than two candidates
    /// are given (one candidate is just a [`Binding::Sticky`]).
    pub fn cycle(candidates: Vec<AppRef>) -> Result<Self, BindingError> {
        if candidates.len() < 2 {
            return Err(BindingError::CycleNeedsCandidates);
        }
        Ok(Self::Cycle(candidates))
    }

    /// Construct a [`Binding::Pattern`], validating a non-empty `app_id`
    /// and (if present) a non-empty title regex.
    ///
    /// # Errors
    ///
    /// [`BindingError::EmptyAppId`] if `app_id` is blank;
    /// [`BindingError::EmptyTitle`] if `title_regex` is `Some` but blank.
    pub fn pattern(
        app_id: impl Into<String>,
        title_regex: Option<String>,
    ) -> Result<Self, BindingError> {
        let app_id = app_id.into();
        if app_id.trim().is_empty() {
            return Err(BindingError::EmptyAppId);
        }
        if title_regex.as_ref().is_some_and(|t| t.trim().is_empty()) {
            return Err(BindingError::EmptyTitle);
        }
        Ok(Self::Pattern {
            app_id,
            title_regex,
        })
    }
}

impl Role {
    /// Construct a [`Role`], validating a non-empty `name` and `monitor`.
    /// The [`Binding`] is already valid by construction.
    ///
    /// # Errors
    ///
    /// [`RoleError::EmptyName`] if `name` is blank;
    /// [`RoleError::EmptyMonitor`] if `monitor` is blank.
    pub fn new(
        name: impl Into<String>,
        monitor: impl Into<String>,
        region: Region,
        binding: Binding,
    ) -> Result<Self, RoleError> {
        let name = name.into();
        let monitor = monitor.into();
        if name.trim().is_empty() {
            return Err(RoleError::EmptyName);
        }
        if monitor.trim().is_empty() {
            return Err(RoleError::EmptyMonitor);
        }
        Ok(Self {
            name,
            monitor,
            region,
            binding,
        })
    }

    /// The role's name (how it is referenced from perspectives and keys).
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// The connector name of the monitor this role lives on (e.g. `DP-1`).
    #[must_use]
    pub fn monitor(&self) -> &str {
        &self.monitor
    }

    /// The rectangle this role occupies on its monitor.
    #[must_use]
    pub const fn region(&self) -> Region {
        self.region
    }

    /// The rule for what fills this role's slot.
    #[must_use]
    pub const fn binding(&self) -> &Binding {
        &self.binding
    }
}

#[cfg(test)]
mod tests {
    use super::{AppRef, Binding, BindingError, Role, RoleError};
    use crate::config::Region;

    fn full() -> Region {
        Region::new(0, 0, 100, 100).expect("full monitor is valid")
    }

    // --- AppRef ---

    #[test]
    fn app_ref_with_profile_is_valid() {
        let app = AppRef::new("firefox", Some("code".to_owned())).expect("valid");
        assert_eq!(app.app_id(), "firefox");
        assert_eq!(app.profile(), Some("code"));
    }

    #[test]
    fn app_ref_without_profile_is_valid() {
        let app = AppRef::new("kitty", None).expect("valid");
        assert_eq!(app.app_id(), "kitty");
        assert_eq!(app.profile(), None);
    }

    #[test]
    fn app_ref_rejects_empty_app_id() {
        assert_eq!(AppRef::new("", None), Err(BindingError::EmptyAppId));
        assert_eq!(AppRef::new("   ", None), Err(BindingError::EmptyAppId));
    }

    #[test]
    fn app_ref_rejects_blank_profile() {
        assert_eq!(
            AppRef::new("firefox", Some(String::new())),
            Err(BindingError::EmptyProfile),
        );
        assert_eq!(
            AppRef::new("firefox", Some("  ".to_owned())),
            Err(BindingError::EmptyProfile),
        );
    }

    // --- Binding ---

    #[test]
    fn sticky_binding_holds_one_instance() {
        let app = AppRef::new("kitty", None).expect("valid");
        assert_eq!(Binding::Sticky(app.clone()), Binding::Sticky(app));
    }

    #[test]
    fn cycle_binding_accepts_two_or_more() {
        let spotify = AppRef::new("spotify", None).expect("valid");
        let claude = AppRef::new("claude-desktop", None).expect("valid");
        let binding = Binding::cycle(vec![spotify, claude]).expect("two candidates is valid");
        assert!(matches!(binding, Binding::Cycle(ref c) if c.len() == 2));
    }

    #[test]
    fn cycle_binding_rejects_single_candidate() {
        let only = AppRef::new("spotify", None).expect("valid");
        assert_eq!(
            Binding::cycle(vec![only]),
            Err(BindingError::CycleNeedsCandidates),
        );
    }

    #[test]
    fn cycle_binding_rejects_empty() {
        assert_eq!(
            Binding::cycle(vec![]),
            Err(BindingError::CycleNeedsCandidates),
        );
    }

    #[test]
    fn pattern_binding_with_title_is_valid() {
        let binding =
            Binding::pattern("Zoom", Some("^Meeting$".to_owned())).expect("valid pattern");
        assert_eq!(
            binding,
            Binding::Pattern {
                app_id: "Zoom".to_owned(),
                title_regex: Some("^Meeting$".to_owned()),
            },
        );
    }

    #[test]
    fn pattern_binding_without_title_is_valid() {
        // Catch any window of an app the compositor does not launch.
        let binding = Binding::pattern("firefox", None).expect("valid pattern");
        assert_eq!(
            binding,
            Binding::Pattern {
                app_id: "firefox".to_owned(),
                title_regex: None,
            },
        );
    }

    #[test]
    fn pattern_binding_rejects_empty_app_id() {
        assert_eq!(
            Binding::pattern("", Some("x".to_owned())),
            Err(BindingError::EmptyAppId),
        );
    }

    #[test]
    fn pattern_binding_rejects_blank_title() {
        assert_eq!(
            Binding::pattern("Zoom", Some("  ".to_owned())),
            Err(BindingError::EmptyTitle),
        );
    }

    // --- Role ---

    #[test]
    fn role_with_sticky_binding_is_valid() {
        let app = AppRef::new("kitty", None).expect("valid");
        let role = Role::new("editor", "DP-1", full(), Binding::Sticky(app)).expect("valid role");
        assert_eq!(role.name(), "editor");
        assert_eq!(role.monitor(), "DP-1");
        assert_eq!(role.region(), full());
        assert!(matches!(role.binding(), Binding::Sticky(_)));
    }

    #[test]
    fn role_with_flex_binding_is_valid() {
        Role::new("scratch", "DP-2", full(), Binding::Flex).expect("flex role is valid");
    }

    #[test]
    fn role_rejects_empty_name() {
        assert_eq!(
            Role::new("", "DP-1", full(), Binding::Flex),
            Err(RoleError::EmptyName),
        );
        assert_eq!(
            Role::new("   ", "DP-1", full(), Binding::Flex),
            Err(RoleError::EmptyName),
        );
    }

    #[test]
    fn role_rejects_empty_monitor() {
        assert_eq!(
            Role::new("editor", "", full(), Binding::Flex),
            Err(RoleError::EmptyMonitor),
        );
    }
}
