//! Which keyboard-interactive layer surface holds keyboard focus while
//! the greeter is in the foreground.
//!
//! A greeter can map one keyboard-interactive layer surface per output
//! (DMS's Quickshell greeter does), and each of them commits a buffer
//! whenever it repaints. Focus goes to such a surface when nothing
//! live holds it, when a non-layer surface holds it, or when the
//! surface sits on the primary output and the holder does not. A
//! surface never takes focus from a layer surface that ranks the same
//! or higher, so repaints never move focus between them. The primary
//! output is the first output halmasuit advertises; Quickshell treats
//! the first screen it is told about as its primary screen, and DMS
//! puts its text cursor there.

/// What currently holds keyboard focus, as seen by a committing
/// keyboard-interactive layer surface.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FocusHolder {
    /// No focus, or the focused surface is dead.
    Nothing,
    /// A live surface that is not a layer surface on any output.
    NonLayer,
    /// A live layer surface on the primary output.
    PrimaryLayer,
    /// A live layer surface on another output.
    SecondaryLayer,
}

/// Whether a committing keyboard-interactive layer surface takes
/// keyboard focus from `holder`.
pub const fn takes_focus(candidate_on_primary: bool, holder: FocusHolder) -> bool {
    match holder {
        FocusHolder::Nothing | FocusHolder::NonLayer => true,
        FocusHolder::SecondaryLayer => candidate_on_primary,
        FocusHolder::PrimaryLayer => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn takes_unheld_focus_on_any_output() {
        assert!(takes_focus(true, FocusHolder::Nothing));
        assert!(takes_focus(false, FocusHolder::Nothing));
    }

    #[test]
    fn takes_focus_from_a_non_layer_surface() {
        assert!(takes_focus(true, FocusHolder::NonLayer));
        assert!(takes_focus(false, FocusHolder::NonLayer));
    }

    #[test]
    fn never_takes_focus_from_a_primary_layer() {
        assert!(!takes_focus(true, FocusHolder::PrimaryLayer));
        assert!(!takes_focus(false, FocusHolder::PrimaryLayer));
    }

    #[test]
    fn primary_layer_takes_focus_from_a_secondary_layer() {
        assert!(takes_focus(true, FocusHolder::SecondaryLayer));
    }

    #[test]
    fn secondary_layer_never_takes_focus_from_a_secondary_layer() {
        assert!(!takes_focus(false, FocusHolder::SecondaryLayer));
    }
}
