//! Wallpaper-facing projection of halmasuit's introspection events.
//!
//! halmasuit-introspect's [`Event`](halmasuit_introspect::Event) is the
//! schema-stable journald state-transition vocabulary (snake_case serde
//! tag). This crate projects each event into the *wallpaper* vocabulary:
//!
//! - [`canonical_name`] — a stable, dotted-lowercase name (e.g.
//!   `halmasuit.session.opened`, `halmasuit.phase.scanout_active`) that a
//!   shader binds against via the `EventTime` / `EventValue` uniform
//!   bindings in `wallpaper/config.rs`. Closed-enum payloads that name a
//!   distinct visual state (the compositor phase, the foreground client,
//!   the first-painted layer role) are expanded *into* the name; per-frame
//!   churn and internal-diagnostic events return [`None`] (not bindable).
//! - [`event_value`] — the `f32` an `EventValue` binding writes. In v1
//!   every bindable event is a latched `0.0 -> 1.0` "this happened / is
//!   active" gate; number-bearing events (idle duration, load fractions)
//!   gain explicit arms here when their publishers land.
//!
//! This is a pure projection layer over `halmasuit-introspect` — no
//! calloop, smithay, wayland, tokio, or serde. The dotted names live here
//! rather than in `halmasuit-introspect` so the journald schema crate
//! stays free of the wallpaper-binding concern.

#![forbid(unsafe_code)]

use halmasuit_introspect::{Event, Foreground, LayerRole, Phase};

/// The wallpaper-facing dotted name a shader binds against, or [`None`]
/// for events that are not meaningful binding targets (per-frame churn,
/// internal-diagnostic, and crash paths).
///
/// The match is intentionally exhaustive with no catch-all arm: adding a
/// new [`Event`] variant is a compile error here until its binding name
/// (or explicit `None`) is decided.
#[must_use]
pub const fn canonical_name(event: &Event) -> Option<&'static str> {
    Some(match event {
        Event::Started { .. } => "halmasuit.started",
        Event::PhaseEntered { phase } => phase_name(*phase),
        Event::Shutdown { .. } => "halmasuit.shutdown",
        Event::SessionRequested { .. } => "halmasuit.session.requested",
        Event::GreeterSpawned { .. } => "halmasuit.greeter.spawned",
        Event::GreeterTerminated { .. } => "halmasuit.greeter.terminated",
        Event::ClientFirstFrame { role } => layer_role_name(*role),
        Event::ForegroundChanged { to } => match to {
            Foreground::Greeter => "halmasuit.foreground.greeter",
            Foreground::Session => "halmasuit.foreground.session",
        },
        Event::SessionOpened => "halmasuit.session.opened",
        Event::SessionClientFirstFrame => "halmasuit.session.client_first_frame",
        Event::SessionEnded { .. } => "halmasuit.session.ended",
        // Not bindable: per-frame churn, internal-diagnostic markers, and
        // crash/error paths a wallpaper has no business animating.
        // WallpaperUniformApplied is the observability marker the wallpaper
        // itself emits — giving it a name would let it re-trigger a write.
        Event::FrameRendered { .. }
        | Event::Fatal { .. }
        | Event::GreeterKillFailed { .. }
        | Event::GreeterDiedPreAuth { .. }
        | Event::SessionLeaderPidfdArmed
        | Event::SessionLeaderExitedViaPidfd
        | Event::WallpaperUniformApplied { .. } => return None,
    })
}

/// `halmasuit.phase.<snake>` for each compositor phase. Distinct names
/// (not a generic `halmasuit.phase_entered`) so a shader binds the exact
/// boot/lifecycle state it wants to react to.
const fn phase_name(phase: Phase) -> &'static str {
    match phase {
        Phase::Init => "halmasuit.phase.init",
        Phase::WaylandReady => "halmasuit.phase.wayland_ready",
        Phase::GreetdReady => "halmasuit.phase.greetd_ready",
        Phase::Deprivileged => "halmasuit.phase.deprivileged",
        Phase::DrmMasterAcquired => "halmasuit.phase.drm_master_acquired",
        Phase::ScanoutActive => "halmasuit.phase.scanout_active",
        Phase::InitramfsInit => "halmasuit.phase.initramfs_init",
        Phase::RootfsReady => "halmasuit.phase.rootfs_ready",
    }
}

/// `halmasuit.client_first_frame.<snake>` for each layer role — the roles
/// name distinct visual states (the overlay role is a lockscreen/OSK), so
/// the role is expanded into the name like the phase and foreground.
const fn layer_role_name(role: LayerRole) -> &'static str {
    match role {
        LayerRole::Wallpaper => "halmasuit.client_first_frame.wallpaper",
        LayerRole::Background => "halmasuit.client_first_frame.background",
        LayerRole::Bottom => "halmasuit.client_first_frame.bottom",
        LayerRole::Top => "halmasuit.client_first_frame.top",
        LayerRole::Overlay => "halmasuit.client_first_frame.overlay",
    }
}

/// The `f32` an `EventValue` binding writes for this event.
///
/// v1: a latched gate — `1.0` for any bindable event ([`canonical_name`]
/// is `Some`), `0.0` otherwise. Number-bearing events will get explicit
/// arms here when their publishers exist.
#[must_use]
pub const fn event_value(event: &Event) -> f32 {
    // v1: any bindable event is a latched gate. Derived from
    // `canonical_name` so the bindable set never drifts between the two.
    // When number-bearing events land, give them explicit arms above this.
    if canonical_name(event).is_some() {
        1.0
    } else {
        0.0
    }
}

#[cfg(test)]
mod tests {
    use super::{canonical_name, event_value};
    use halmasuit_introspect::{Event, Foreground, LayerRole, Phase, SessionExit, ShutdownReason};

    /// One sample of every `Event` variant paired with its expected
    /// canonical name. Phases and layer roles are enumerated in full
    /// (via [`phase_expectations`]/[`role_expectations`]) so the
    /// payload-in-name expansion is covered for each.
    fn variant_expectations() -> Vec<(Event, Option<&'static str>)> {
        let mut all = scalar_expectations();
        all.extend(phase_expectations());
        all.extend(role_expectations());
        all
    }

    /// Variants with no payload-in-name expansion (one sample each).
    fn scalar_expectations() -> Vec<(Event, Option<&'static str>)> {
        vec![
            (
                Event::Started {
                    pid: 1,
                    version: "0.1.0".to_owned(),
                },
                Some("halmasuit.started"),
            ),
            (
                Event::Shutdown {
                    reason: ShutdownReason::PrepareForShutdown,
                },
                Some("halmasuit.shutdown"),
            ),
            (
                Event::Fatal {
                    message: "boom".to_owned(),
                },
                None,
            ),
            (
                Event::SessionRequested {
                    uid: 1000,
                    gid: 1000,
                },
                Some("halmasuit.session.requested"),
            ),
            (
                Event::GreeterSpawned { pid: 2 },
                Some("halmasuit.greeter.spawned"),
            ),
            (
                Event::GreeterTerminated { pid: 2 },
                Some("halmasuit.greeter.terminated"),
            ),
            (
                Event::GreeterKillFailed {
                    pid: 2,
                    error: "e".to_owned(),
                },
                None,
            ),
            (Event::GreeterDiedPreAuth { pid: 2 }, None),
            (
                Event::FrameRendered {
                    frame_id: 1,
                    pixel_count: 4,
                    clear_pixel_count: 0,
                    black_pixel_count: 0,
                    degenerate: false,
                    phash: 7,
                },
                None,
            ),
            (
                Event::ForegroundChanged {
                    to: Foreground::Greeter,
                },
                Some("halmasuit.foreground.greeter"),
            ),
            (
                Event::ForegroundChanged {
                    to: Foreground::Session,
                },
                Some("halmasuit.foreground.session"),
            ),
            (Event::SessionOpened, Some("halmasuit.session.opened")),
            (
                Event::SessionClientFirstFrame,
                Some("halmasuit.session.client_first_frame"),
            ),
            (
                Event::SessionEnded {
                    outcome: SessionExit::Exited { code: 0 },
                },
                Some("halmasuit.session.ended"),
            ),
            (Event::SessionLeaderPidfdArmed, None),
            (Event::SessionLeaderExitedViaPidfd, None),
            (
                Event::WallpaperUniformApplied {
                    event_name: "halmasuit.session.opened".to_owned(),
                    uniform: "u_login_time".to_owned(),
                },
                None,
            ),
        ]
    }

    /// `PhaseEntered` for every phase, with its expected dotted name.
    fn phase_expectations() -> Vec<(Event, Option<&'static str>)> {
        [
            (Phase::Init, "halmasuit.phase.init"),
            (Phase::WaylandReady, "halmasuit.phase.wayland_ready"),
            (Phase::GreetdReady, "halmasuit.phase.greetd_ready"),
            (Phase::Deprivileged, "halmasuit.phase.deprivileged"),
            (
                Phase::DrmMasterAcquired,
                "halmasuit.phase.drm_master_acquired",
            ),
            (Phase::ScanoutActive, "halmasuit.phase.scanout_active"),
            (Phase::InitramfsInit, "halmasuit.phase.initramfs_init"),
            (Phase::RootfsReady, "halmasuit.phase.rootfs_ready"),
        ]
        .into_iter()
        .map(|(phase, name)| (Event::PhaseEntered { phase }, Some(name)))
        .collect()
    }

    /// `ClientFirstFrame` for every layer role, with its expected name.
    fn role_expectations() -> Vec<(Event, Option<&'static str>)> {
        [
            (
                LayerRole::Wallpaper,
                "halmasuit.client_first_frame.wallpaper",
            ),
            (
                LayerRole::Background,
                "halmasuit.client_first_frame.background",
            ),
            (LayerRole::Bottom, "halmasuit.client_first_frame.bottom"),
            (LayerRole::Top, "halmasuit.client_first_frame.top"),
            (LayerRole::Overlay, "halmasuit.client_first_frame.overlay"),
        ]
        .into_iter()
        .map(|(role, name)| (Event::ClientFirstFrame { role }, Some(name)))
        .collect()
    }

    #[test]
    fn canonical_name_matches_expected_for_every_variant() {
        for (event, expected) in variant_expectations() {
            assert_eq!(canonical_name(&event), expected, "for {event:?}");
        }
    }

    /// The session-opened name is a cross-crate contract: it MUST equal
    /// the literal already used in `crates/halmasuit/src/wallpaper/config.rs`.
    #[test]
    fn session_opened_matches_wallpaper_config_literal() {
        assert_eq!(
            canonical_name(&Event::SessionOpened),
            Some("halmasuit.session.opened"),
        );
    }

    #[test]
    fn event_value_is_a_latched_gate_for_bindable_events() {
        for (event, expected) in variant_expectations() {
            let expected_value: f32 = if expected.is_some() { 1.0 } else { 0.0 };
            // Bit-compare to assert exact equality without tripping
            // clippy::float_cmp; both sides are exact literals.
            assert_eq!(
                event_value(&event).to_bits(),
                expected_value.to_bits(),
                "value mismatch for {event:?}",
            );
        }
    }
}
