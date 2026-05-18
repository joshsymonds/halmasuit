//! The Amendment-A5 two-key flash-free greeter→session swap gate.
//!
//! Pure state machine (no I/O, no smithay) so the load-bearing
//! sequencing — the literal reason this project exists — is unit
//! tested without a compositor or a broker.
//!
//! The VISIBLE greeter→session swap fires ONLY on
//! `AND(SessionOpened, session-client's first non-empty frame)`.
//! `SessionOpened` *authorizes/names* the session; swapping on it
//! alone (before the session client has actually painted) reintroduces
//! the exact flash halmasuit deletes (Mir/USC
//! `is_session_ready_for_display = session && ready`,
//! `WindowWlSurfaceRole` first-non-empty-buffer gate; HANDOFF §0.9).
//!
//! Revert (back to greeter/splash — on logout the splash is already
//! running and just becomes visible again, ARCHITECTURE.md) fires on
//! `SessionEnded` OR session-client disconnect (A5.5). Revert is
//! terminal: a post-revert key is inert (the episode is over).

/// What the caller must do to the visible scene after an input.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SwapAction {
    /// Nothing changes on screen yet.
    None,
    /// Both keys are in: make the session foreground (SIGKILL the
    /// greeter, flip `foreground` to `Session`, re-composite). Emitted
    /// exactly once.
    Swap,
    /// Revert the foreground to the greeter/splash (re-composite).
    /// Emitted at most once, and only if a `Swap` had happened — a
    /// session that ends before it ever became visible never produced
    /// a swap, so there is nothing to revert (the greeter was never
    /// torn down). Emitted exactly once in that case.
    Revert,
}

/// Lifecycle phase of the two-key gate for one greeter→session
/// episode. Keys arrive in EITHER order (`SessionOpened` may precede
/// or follow the session client's first frame); the swap fires on the
/// second.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase {
    /// Pre-swap: still collecting the two keys. `opened` = key 1
    /// (`SessionOpened`) seen; `first_frame` = key 2 (session client's
    /// first non-empty frame) seen.
    Arming { opened: bool, first_frame: bool },
    /// The swap became visible (greeter torn down, session foreground).
    Swapped,
    /// Terminal: a revert (post-swap) or disarm (pre-swap) already
    /// happened; all further input is inert.
    Done,
}

/// The two-key flash-free swap gate for one greeter→session episode.
///
/// After a swap, `SessionEnded`/disconnect reverts once. Before a
/// swap, `SessionEnded`/disconnect just disarms the gate (no visible
/// change — the greeter was never torn down).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SwapGate {
    phase: Phase,
}

impl Default for SwapGate {
    fn default() -> Self {
        Self::new()
    }
}

impl SwapGate {
    /// A fresh gate (no keys, not swapped).
    #[must_use]
    pub const fn new() -> Self {
        Self {
            phase: Phase::Arming {
                opened: false,
                first_frame: false,
            },
        }
    }

    /// Fold a newly-arrived key into the `Arming` phase; emit `Swap`
    /// exactly once when both keys are in. Inert in any other phase.
    const fn arm(&mut self, set_opened: bool, set_first_frame: bool) -> SwapAction {
        let Phase::Arming {
            mut opened,
            mut first_frame,
        } = self.phase
        else {
            return SwapAction::None;
        };
        opened |= set_opened;
        first_frame |= set_first_frame;
        if opened && first_frame {
            self.phase = Phase::Swapped;
            SwapAction::Swap
        } else {
            self.phase = Phase::Arming {
                opened,
                first_frame,
            };
            SwapAction::None
        }
    }

    /// Key 1: the compositor received `SessionOpened` from the broker.
    pub const fn session_opened(&mut self) -> SwapAction {
        self.arm(true, false)
    }

    /// Key 2: the session Wayland client committed its first buffer of
    /// non-zero size that halmasuit will composite.
    pub const fn session_first_frame(&mut self) -> SwapAction {
        self.arm(false, true)
    }

    /// Revert trigger: the broker sent `SessionEnded`. Reverts iff a
    /// swap had become visible; otherwise just disarms (the greeter
    /// was never torn down — nothing on screen to undo).
    pub const fn session_ended(&mut self) -> SwapAction {
        self.revert_or_disarm()
    }

    /// Revert trigger: the session Wayland client disconnected (its
    /// last surface/connection went away). Same semantics as
    /// [`Self::session_ended`]; the authoritative signal is still the
    /// `SessionEnded` frame (A5.5) — whichever arrives first reverts,
    /// the later one is inert.
    pub const fn session_client_gone(&mut self) -> SwapAction {
        self.revert_or_disarm()
    }

    const fn revert_or_disarm(&mut self) -> SwapAction {
        match self.phase {
            Phase::Swapped => {
                self.phase = Phase::Done;
                SwapAction::Revert
            }
            Phase::Arming { .. } => {
                self.phase = Phase::Done;
                SwapAction::None
            }
            Phase::Done => SwapAction::None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{SwapAction, SwapGate};

    #[test]
    fn neither_key_alone_swaps() {
        let mut g = SwapGate::new();
        assert_eq!(g.session_opened(), SwapAction::None);
        assert_eq!(g, SwapGate::new().tap_opened());

        let mut g = SwapGate::new();
        assert_eq!(g.session_first_frame(), SwapAction::None);
    }

    #[test]
    fn opened_then_frame_swaps_on_the_second_key() {
        let mut g = SwapGate::new();
        assert_eq!(g.session_opened(), SwapAction::None);
        assert_eq!(g.session_first_frame(), SwapAction::Swap);
    }

    #[test]
    fn frame_then_opened_also_swaps_on_the_second_key() {
        // Order independence: the session client can paint before the
        // broker's SessionOpened lands (or after) — the swap is the
        // AND, not a sequence.
        let mut g = SwapGate::new();
        assert_eq!(g.session_first_frame(), SwapAction::None);
        assert_eq!(g.session_opened(), SwapAction::Swap);
    }

    #[test]
    fn swap_is_emitted_exactly_once() {
        let mut g = SwapGate::new();
        g.session_opened();
        assert_eq!(g.session_first_frame(), SwapAction::Swap);
        // Redundant repeats of either key never re-swap.
        assert_eq!(g.session_first_frame(), SwapAction::None);
        assert_eq!(g.session_opened(), SwapAction::None);
    }

    #[test]
    fn session_ended_after_swap_reverts_once() {
        let mut g = SwapGate::new();
        g.session_opened();
        assert_eq!(g.session_first_frame(), SwapAction::Swap);
        assert_eq!(g.session_ended(), SwapAction::Revert);
        // Terminal: a trailing disconnect is inert (no double revert).
        assert_eq!(g.session_client_gone(), SwapAction::None);
    }

    #[test]
    fn client_gone_after_swap_reverts_once() {
        let mut g = SwapGate::new();
        g.session_first_frame();
        assert_eq!(g.session_opened(), SwapAction::Swap);
        assert_eq!(g.session_client_gone(), SwapAction::Revert);
        assert_eq!(g.session_ended(), SwapAction::None);
    }

    #[test]
    fn session_ended_before_swap_disarms_no_revert() {
        // Session that never became visible (died between
        // SessionOpened and its first frame): the greeter was never
        // torn down, so there is nothing to revert — and a late first
        // frame must NOT then swap to a dead session.
        let mut g = SwapGate::new();
        assert_eq!(g.session_opened(), SwapAction::None);
        assert_eq!(g.session_ended(), SwapAction::None);
        assert_eq!(g.session_first_frame(), SwapAction::None);
    }

    #[test]
    fn client_gone_before_swap_disarms_then_keys_inert() {
        let mut g = SwapGate::new();
        assert_eq!(g.session_first_frame(), SwapAction::None);
        assert_eq!(g.session_client_gone(), SwapAction::None);
        assert_eq!(g.session_opened(), SwapAction::None);
    }

    impl SwapGate {
        /// Test helper: apply `session_opened` and return self, for a
        /// terse state-equality assertion.
        fn tap_opened(mut self) -> Self {
            self.session_opened();
            self
        }
    }
}
