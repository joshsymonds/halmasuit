//! halmasuit-introspect — structured-event sink primitives for halmasuit.
//!
//! Defines the schema-stable [`Event`] enum (state transitions), supporting
//! [`Phase`] and [`ShutdownReason`] types, and the [`emit`] function that
//! routes events through `tracing` for capture by a JSON-formatting
//! subscriber. With a `tracing-subscriber` JSON formatter installed and
//! pointed at stderr, the systemd unit captures one JSON line per event and
//! `journalctl -u halmasuit -o json` produces a live feed.
//!
//! State-transition events flow through [`Event`]. Diagnostic events (DRM
//! activity, Wayland protocol traffic, etc.) use raw `tracing::info!` /
//! `warn!` / `error!` calls in the consumer — schema-flexible by design (see
//! PLAN.md "Build order" and ARCHITECTURE.md "Observability").
//!
//! Field-name convention: all serialized JSON keys are snake_case via serde's
//! `rename_all = "snake_case"`. PAM message text and any other
//! user-controlled string content must be redacted at the construction site
//! in the consuming crate before being embedded in an `Event` variant; this
//! crate enforces shape, not content.
//!
//! Versioning: no `schema_version` envelope is emitted in Phase A. The
//! versioned-envelope decision is deferred to when the live snapshot socket
//! lands (per PLAN.md and the epic anti-patterns).

#![forbid(unsafe_code)]

use serde::Serialize;

/// State-transition events emitted by halmasuit during its lifetime.
///
/// Variants form the stable schema of halmasuit's introspection output. New
/// variants are added as new phases land in the architecture (see
/// ARCHITECTURE.md "Boot timeline and phases").
///
/// Future variants carrying user-controlled text (e.g., a `PamChallenge`
/// variant) must redact the text at construction time in the consuming crate;
/// see the crate-level documentation.
#[derive(Debug, Serialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum Event {
    /// Process startup. Emitted once at `main()` line 1.
    Started {
        /// Linux PIDs are 32-bit signed in `/proc` but always non-negative;
        /// `u32` is the correct domain type.
        pid: u32,
        /// Compositor crate version, typically `env!("CARGO_PKG_VERSION")`.
        version: &'static str,
    },
    /// Phase transition. Emitted on every state change in the compositor's
    /// phase state machine.
    PhaseEntered {
        /// New phase the compositor has entered.
        phase: Phase,
    },
    /// Clean shutdown initiated. Emitted before process exit.
    Shutdown {
        /// What triggered the shutdown.
        reason: ShutdownReason,
    },
    /// Fatal error path. Emitted best-effort before crash exit.
    Fatal {
        /// Operator-facing description of the failure. Must not contain
        /// credential material or unredacted user input.
        message: String,
    },
    /// A greeter session reached the `Spawning` state — PAM authentication
    /// completed and `StartSession` was received. The compositor relays
    /// the resolved uid/gid to the privileged `halmasuit-session`
    /// broker, which forks-then-drops the session leader (the
    /// corresponding `cmd` and `env` from the greetd protocol are not
    /// emitted on this event surface because they carry user-influenced
    /// strings; the redaction policy lives in the snapshot-socket task).
    SessionRequested {
        /// Resolved Linux uid for the authenticated user, as returned by
        /// PAM + pwent lookup.
        uid: u32,
        /// Resolved Linux gid.
        gid: u32,
    },
    /// halmasuit fork+exec'd the configured greeter as a child of
    /// itself. The greeter runs as the greeter system user; the FD
    /// table inherits the Wayland + greetd socket paths via env vars.
    /// Emitted once at startup, when `HALMASUIT_GREETER_COMMAND` is
    /// set and the spawn succeeded.
    GreeterSpawned {
        /// PID of the spawned greeter process.
        pid: u32,
    },
    /// halmasuit sent SIGKILL to the greeter child as part of the
    /// session-start handover. Per ARCHITECTURE.md / Epic #1: "the
    /// greeter wl_client is killed before the session becomes
    /// foreground" — emitted between `SessionRequested` and the
    /// `ForegroundChanged { to: session }` flip so the greeter
    /// releases halmasuit's foreground slot before the user session
    /// asks for it.
    GreeterTerminated {
        /// PID of the greeter we killed.
        pid: u32,
    },
    /// halmasuit attempted to terminate the greeter on session start
    /// but the signal call failed (typically `ESRCH` — greeter had
    /// already exited before we got here). The session start
    /// proceeds regardless; an already-exited greeter is the
    /// architecturally-desired state, just reached via a different
    /// path than our explicit kill.
    GreeterKillFailed {
        /// PID we attempted to signal.
        pid: u32,
        /// `Display` form of the kernel error.
        error: String,
    },
    /// The greeter process exited BEFORE authentication completed
    /// (`session_uid` was still unset when the SIGCHLD reaper observed
    /// the death). The unexpected-death path: distinct from
    /// `GreeterTerminated` (the deliberate post-auth SIGKILL) and
    /// `GreeterKillFailed` (that SIGKILL racing an already-exited
    /// greeter). Emitted so a greeter crash/exit pre-auth surfaces as
    /// an explicit event instead of silently wedging the compositor
    /// with no greeter, no session, and a discarded waitpid status.
    GreeterDiedPreAuth {
        /// PID of the greeter that exited pre-auth.
        pid: u32,
    },
    /// A composited frame was scanned out, with the test-only
    /// `frame_audit` analysis of its pixels. The variant is defined
    /// unconditionally so `halmasuit-introspect` (and its consumers'
    /// JSON schema) stay feature-independent; only halmasuit's
    /// *emission* of it is gated behind the `frame_audit` Cargo
    /// feature (the analysis costs a GPU readback per frame, which is
    /// unacceptable in the production binary — see Epic #1 req 6/7).
    ///
    /// The no-flash invariant in `tests/visual-backdrop.nix` and
    /// `tests/visual-foreground.nix` is asserted over the WHOLE stream
    /// of these events (`visual.assert_no_flash_stream`), as EXACT
    /// integer/boolean facts — never fuzzy aggregate float thresholds.
    /// Anchored at FRAME 0 (epic amendment G1/R3): halmasuit
    /// composites the wallpaper plane internally from the first
    /// frame, so the single `client_first_frame{role:wallpaper}`
    /// precedes every `FrameRendered` and EVERY frame (frame 0
    /// onward, not a suffix) must have `clear_pixel_count == 0` (no
    /// sentinel-clear pixel leaked through — there is no pre-client
    /// solid phase) and `degenerate == false` (no all-clear,
    /// all-black, or empty/dropped frame). `pixel_count` stays
    /// constant across the whole stream (the composited target is
    /// never resized/recreated under the live output mode).
    FrameRendered {
        /// Monotonic per-process frame counter, starting at 0 on the
        /// first composited frame.
        frame_id: u64,
        /// Total pixels in the composited frame readback
        /// (`width * height`). Constant once the output mode is set;
        /// the stream gate uses its stability to prove the backdrop
        /// surface was not resized/recreated mid-stream.
        pixel_count: u64,
        /// EXACT count of pixels byte-equal to the brand clear color
        /// `#0a0014` (the uncovered-sentinel). After the background
        /// client's first frame this must be exactly `0`: any nonzero
        /// count is the clear sentinel showing through — a flash.
        clear_pixel_count: u64,
        /// EXACT count of pixels byte-equal to pure black `#000000`.
        /// Used to detect an all-black frame (`== pixel_count`), the
        /// canonical flash this project exists to prevent.
        black_pixel_count: u64,
        /// EXACT boolean: the frame is degenerate — all clear, all
        /// black, or empty/dropped (`pixel_count == 0`). After the
        /// first composited frame this must be `false`. This is the
        /// zero-tolerance flash predicate (no luminance threshold).
        degenerate: bool,
        /// 64-bit average-hash perceptual fingerprint of the frame
        /// (8x8 luma downscale, thresholded at the mean). An exact
        /// integer; lets tests detect "the frame stopped changing" /
        /// gross content shifts without pixel-exact comparison.
        phash: u64,
    },
    /// First composite of a given layer role. For `Wallpaper` this
    /// is halmasuit's INTERNAL wallpaper plane (epic amendment
    /// G1/R3) — emitted once, before the first composited frame,
    /// since the wallpaper is composited from frame 0 (there is no
    /// external background client). `Bottom`/`Top`/`Overlay` are the
    /// first buffer a real wlr-layer-shell client of that role
    /// committed. Emitted once per role per process lifetime.
    /// Unconditional (NOT `frame_audit`-gated): a cheap
    /// state-transition marker, like `GreeterSpawned`. The exact
    /// no-flash stream gate (`visual.assert_no_flash_stream`)
    /// requires the single `ClientFirstFrame { role: Wallpaper }` to
    /// precede every `FrameRendered`, each of which must then have
    /// `clear_pixel_count == 0` (Epic #1 req 11 / R3).
    ClientFirstFrame {
        /// Which layer-shell role first painted.
        role: LayerRole,
    },
    /// The compositor's foreground client changed, driven by the
    /// greetd lifecycle (NOT process/connection identity). `Greeter`
    /// from startup; `Session` once `start_session` succeeds and the
    /// greeter is torn down. Unconditional state-transition marker
    /// (like `GreeterSpawned`); `tests/visual-foreground.nix` keys
    /// the no-flash continuity assertion off the ordering of these
    /// against the `FrameRendered` stream (Epic #1 req 17).
    ForegroundChanged {
        /// The new foreground.
        to: Foreground,
    },
    /// Key 1 of the Amendment-A5 two-key flash-free swap: the
    /// compositor received `BrokerToCompositor::SessionOpened` (the
    /// broker forked+dropped the session leader and `pam_open_session`
    /// succeeded). This *authorizes/names* the session; it does NOT by
    /// itself make the session visible — swapping on this alone
    /// reintroduces the flash this project deletes. Paired with
    /// `SessionClientFirstFrame` (key 2) to gate `ForegroundChanged {
    /// to: session }`.
    SessionOpened,
    /// Key 2 of the two-key swap: the session Wayland client committed
    /// its first buffer of non-zero size that halmasuit will
    /// composite. The greeter stays visible underneath until BOTH this
    /// and `SessionOpened` are observed (Amendment A5; Mir/USC
    /// `is_session_ready_for_display`).
    SessionClientFirstFrame,
    /// The session ended (Amendment A5.5 revert trigger): the broker
    /// sent `BrokerToCompositor::SessionEnded`. Crash-vs-clean is
    /// preserved (A5.2; GDM `SESSION_EXITED`/`SESSION_DIED` — NOT
    /// collapsed like greetd). The compositor reverts the foreground
    /// to the greeter/splash on this OR session-client disconnect.
    SessionEnded {
        /// How the session leader exited.
        outcome: SessionExit,
    },
    /// Amendment A5.6: the compositor received the leader pidfd the
    /// broker passed via `SCM_RIGHTS` on `SessionOpened` and armed it
    /// as a POLL-ONLY calloop liveness source. Proves the
    /// privilege-crossing fd transfer (worker→broker→compositor)
    /// succeeded end to end. The compositor never
    /// waitid/reap/`pidfd_send_signal`s it (the broker is the sole
    /// reaper, R9/A5); `EPOLLIN` (leader exited) is a
    /// latency/broker-crash-resilient accelerator for the revert, not
    /// the authoritative signal (that is `SessionEnded`).
    SessionLeaderPidfdArmed,
    /// Amendment A5.6: the poll-only leader pidfd became readable — the
    /// session leader EXITED. The compositor drives the revert from
    /// this WITHOUT the `SessionEnded` frame (broker-crash resilience);
    /// it does NOT reap/signal (poll-only). The [`SessionEnded`] frame
    /// remains the authoritative signal when the broker is alive; the
    /// swap gate makes whichever trigger arrives later inert.
    SessionLeaderExitedViaPidfd,
}

/// How the session leader process terminated (Amendment A5.2).
///
/// The crash-vs-clean distinction is preserved end to end (mirrors
/// `halmasuit_session_ipc::SessionOutcome`; duplicated here so the
/// introspect schema crate stays dependency-light, like
/// [`LayerRole`]/[`Foreground`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionExit {
    /// The leader `_exit`ed with this status code (clean — GDM
    /// `SESSION_EXITED`).
    Exited {
        /// Process exit status code.
        code: i32,
    },
    /// The leader was killed by this signal (crash — GDM
    /// `SESSION_DIED`).
    Signaled {
        /// Terminating signal number.
        signal: i32,
    },
}

/// Which client halmasuit treats as the foreground (composited above
/// the splash background, keyboard focus target). Decided by the
/// greetd lifecycle, never by which client connected.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Foreground {
    /// The greeter is foreground (pre-auth).
    Greeter,
    /// The user session is foreground (post `start_session`).
    Session,
}

/// Layer role used by the `ClientFirstFrame` event.
///
/// Four of the variants (`Background`/`Bottom`/`Top`/`Overlay`)
/// mirror the `wlr-layer-shell` protocol layer of the same name and
/// mark the first frame any external wlr-layer-shell client of that
/// role committed. `Wallpaper` is halmasuit's INTERNAL wallpaper
/// plane — no external client; the wallpaper engine composites it
/// from frame 0 (epic amendment G1/R3) and emits the
/// `ClientFirstFrame { role: Wallpaper }` anchor before the first
/// composited frame. `Background` therefore represents an external
/// wlr-layer-shell `background` client (composited above the
/// wallpaper plane, below normal windows). Duplicated here so
/// `halmasuit-introspect` needs no smithay dependency; halmasuit
/// maps the smithay layer value onto this at the emission site.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum LayerRole {
    /// The wallpaper plane — halmasuit's internal bottom-most
    /// composited content surface, hosting image / shader / video
    /// backends via the wallpaper engine. NO external client; the
    /// engine emits this exactly once per process lifetime, before
    /// the first composited frame.
    Wallpaper,
    /// An external wlr-layer-shell `background` layer client
    /// (composited above the wallpaper plane, below normal windows).
    Background,
    /// Below normal windows (wlr-layer-shell `bottom` layer).
    Bottom,
    /// Above normal windows (wlr-layer-shell `top` layer).
    Top,
    /// Topmost — lockscreens, OSKs, notifications (wlr-layer-shell
    /// `overlay` layer).
    Overlay,
}

/// Compositor phases.
///
/// Further variants (`Greeter`, `Session`, `Locked`, `Shutdown`) land
/// alongside the code that enters them.
#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Phase {
    /// Compositor initialization completed: smithay protocol state
    /// constructed, Wayland and signal sources registered on the
    /// event loop. Emitted immediately before `WaylandReady` under
    /// the current ordering — the two are effectively adjacent.
    Init,
    /// Wayland socket is bound and accepting client connections. No
    /// protocol globals are advertised yet — clients see an empty global
    /// list. Globals are added as their consuming code lands.
    WaylandReady,
    /// greetd protocol socket is bound and accepting greeter connections.
    /// The compositor is ready to host a greeter and drive PAM.
    GreetdReady,
    /// In-process privilege drop completed: the compositor started as
    /// root (to bind sockets under `/run/halmasuit/` and to acquire
    /// DRM master) and has now `setresuid`'d to the configured
    /// compositor system user. Emitted after the drop succeeds;
    /// subsequent code runs unprivileged.
    Deprivileged,
    /// DRM master acquired on `/dev/dri/card0` (or the device named
    /// by `HALMASUIT_DRM_DEVICE`). The file descriptor lives for the
    /// process lifetime — drm-master-probe Phase 1 validated that
    /// the master designation survives `setresuid`, so subsequent
    /// phases run as the compositor user with master still held.
    DrmMasterAcquired,
    /// CRTC drive armed: a dumb buffer (or, after the GLES subtask,
    /// a GBM-backed framebuffer) has been wrapped as a DRM framebuffer
    /// and pushed via SETCRTC, so the chosen connector is now
    /// scanning out halmasuit-owned pixels. Pre-client scanout shows
    /// the brand clear color `#0a0014` — observable in tests as
    /// evidence that halmasuit is alive but no wl_client has yet
    /// committed a buffer. Emitted exactly once per process lifetime,
    /// after `DrmMasterAcquired` and before any client connects.
    ScanoutActive,
}

/// Reason a clean shutdown was initiated.
#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ShutdownReason {
    /// SIGTERM from systemd or another external signal source.
    SignalTerm,
    /// SIGINT (Ctrl-C in the foreground; rarely seen in production).
    SignalInt,
    /// Internal request (e.g., logind `PrepareForShutdown` once D-Bus
    /// integration lands).
    Internal,
}

/// Emit a state-transition event through `tracing`.
///
/// Thread-safe: `tracing` is thread-safe by design; `emit` may be called from
/// any thread without external synchronization.
///
/// If no `tracing` subscriber is installed, this is a silent no-op (per
/// `tracing`'s default behavior).
///
/// Serialization failure — which should be unreachable given that all
/// [`Event`] variants are simple data structures — is logged via
/// `tracing::error!` and silently swallowed. The observability path must
/// never take down the compositor.
pub fn emit(event: &Event) {
    match serde_json::to_string(event) {
        Ok(json) => {
            tracing::info!(target: "halmasuit::event", json = %json);
        }
        Err(err) => {
            tracing::error!(
                target: "halmasuit::event",
                "halmasuit-introspect failed to serialize Event: {err}"
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{Event, Foreground, LayerRole, Phase, ShutdownReason, emit};
    use serde_json::Value;

    fn round_trip(event: &Event) -> Value {
        let s = serde_json::to_string(event).expect("Event serialization is infallible");
        serde_json::from_str(&s).expect("round-trip parse should succeed")
    }

    #[test]
    fn event_started_serializes_with_tag() {
        let v = round_trip(&Event::Started {
            pid: 42,
            version: "0.1.0",
        });
        assert_eq!(v["event"], "started");
        assert_eq!(v["pid"], 42);
        assert_eq!(v["version"], "0.1.0");
    }

    #[test]
    fn event_phase_entered_includes_phase() {
        let v = round_trip(&Event::PhaseEntered { phase: Phase::Init });
        assert_eq!(v["event"], "phase_entered");
        assert_eq!(v["phase"], "init");
    }

    #[test]
    fn event_phase_entered_wayland_ready_serializes() {
        let v = round_trip(&Event::PhaseEntered {
            phase: Phase::WaylandReady,
        });
        assert_eq!(v["event"], "phase_entered");
        assert_eq!(v["phase"], "wayland_ready");
    }

    #[test]
    fn event_phase_entered_drm_master_acquired_serializes() {
        let v = round_trip(&Event::PhaseEntered {
            phase: Phase::DrmMasterAcquired,
        });
        assert_eq!(v["event"], "phase_entered");
        assert_eq!(v["phase"], "drm_master_acquired");
    }

    #[test]
    fn event_phase_entered_deprivileged_serializes() {
        let v = round_trip(&Event::PhaseEntered {
            phase: Phase::Deprivileged,
        });
        assert_eq!(v["event"], "phase_entered");
        assert_eq!(v["phase"], "deprivileged");
    }

    #[test]
    fn event_phase_entered_scanout_active_serializes() {
        let v = round_trip(&Event::PhaseEntered {
            phase: Phase::ScanoutActive,
        });
        assert_eq!(v["event"], "phase_entered");
        assert_eq!(v["phase"], "scanout_active");
    }

    #[test]
    fn event_shutdown_serializes_reason() {
        let v = round_trip(&Event::Shutdown {
            reason: ShutdownReason::SignalTerm,
        });
        assert_eq!(v["event"], "shutdown");
        assert_eq!(v["reason"], "signal_term");
    }

    #[test]
    fn event_fatal_carries_message() {
        let v = round_trip(&Event::Fatal {
            message: "boom".to_owned(),
        });
        assert_eq!(v["event"], "fatal");
        assert_eq!(v["message"], "boom");
    }

    #[test]
    fn event_greeter_spawned_carries_pid() {
        let v = round_trip(&Event::GreeterSpawned { pid: 1234 });
        assert_eq!(v["event"], "greeter_spawned");
        assert_eq!(v["pid"], 1234);
    }

    #[test]
    fn event_greeter_terminated_carries_pid() {
        let v = round_trip(&Event::GreeterTerminated { pid: 1234 });
        assert_eq!(v["event"], "greeter_terminated");
        assert_eq!(v["pid"], 1234);
    }

    #[test]
    fn event_greeter_kill_failed_carries_pid_and_error() {
        let v = round_trip(&Event::GreeterKillFailed {
            pid: 1234,
            error: "No such process (os error 3)".to_owned(),
        });
        assert_eq!(v["event"], "greeter_kill_failed");
        assert_eq!(v["pid"], 1234);
        assert_eq!(v["error"], "No such process (os error 3)");
    }

    #[test]
    fn event_greeter_died_pre_auth_carries_pid() {
        let v = round_trip(&Event::GreeterDiedPreAuth { pid: 4242 });
        assert_eq!(v["event"], "greeter_died_pre_auth");
        assert_eq!(v["pid"], 4242);
    }

    #[test]
    fn event_frame_rendered_carries_exact_audit_fields() {
        // The no-flash stream gate is EXACT integer/boolean facts, not
        // fuzzy float thresholds. A frame that is 1920x1080 with three
        // sentinel-clear pixels still present and no all-clear/all-black
        // degeneracy must serialize those exact counts.
        let v = round_trip(&Event::FrameRendered {
            frame_id: 42,
            pixel_count: 1920 * 1080,
            clear_pixel_count: 3,
            black_pixel_count: 0,
            degenerate: false,
            phash: 0xDEAD_BEEF_0000_1234,
        });
        assert_eq!(v["event"], "frame_rendered");
        assert_eq!(v["frame_id"], 42);
        assert_eq!(v["pixel_count"], 1920u64 * 1080);
        assert_eq!(v["clear_pixel_count"], 3);
        assert_eq!(v["black_pixel_count"], 0);
        assert_eq!(v["degenerate"], false);
        assert_eq!(v["phash"], 0xDEAD_BEEF_0000_1234u64);
        // No fuzzy aggregate floats remain on the schema.
        assert!(v.get("mean_luminance").is_none());
        assert!(v.get("backdrop_coverage").is_none());
    }

    #[test]
    fn event_frame_rendered_marks_all_clear_and_all_black_degenerate() {
        let all_clear = round_trip(&Event::FrameRendered {
            frame_id: 0,
            pixel_count: 256,
            clear_pixel_count: 256,
            black_pixel_count: 0,
            degenerate: true,
            phash: 0,
        });
        assert_eq!(all_clear["degenerate"], true);
        let all_black = round_trip(&Event::FrameRendered {
            frame_id: 1,
            pixel_count: 256,
            clear_pixel_count: 0,
            black_pixel_count: 256,
            degenerate: true,
            phash: 0,
        });
        assert_eq!(all_black["degenerate"], true);
    }

    #[test]
    fn event_foreground_changed_carries_target() {
        let v = round_trip(&Event::ForegroundChanged {
            to: Foreground::Greeter,
        });
        assert_eq!(v["event"], "foreground_changed");
        assert_eq!(v["to"], "greeter");
        assert_eq!(
            round_trip(&Event::ForegroundChanged {
                to: Foreground::Session
            })["to"],
            "session"
        );
    }

    #[test]
    fn event_client_first_frame_carries_role() {
        let v = round_trip(&Event::ClientFirstFrame {
            role: LayerRole::Wallpaper,
        });
        assert_eq!(v["event"], "client_first_frame");
        assert_eq!(v["role"], "wallpaper");
        for (role, name) in [
            (LayerRole::Background, "background"),
            (LayerRole::Bottom, "bottom"),
            (LayerRole::Top, "top"),
            (LayerRole::Overlay, "overlay"),
        ] {
            assert_eq!(round_trip(&Event::ClientFirstFrame { role })["role"], name);
        }
    }

    #[test]
    fn emit_without_subscriber_does_not_panic() {
        // No subscriber installed; tracing events become no-ops by design.
        emit(&Event::Started {
            pid: 1,
            version: "test",
        });
        emit(&Event::PhaseEntered { phase: Phase::Init });
        emit(&Event::Shutdown {
            reason: ShutdownReason::SignalInt,
        });
        emit(&Event::Fatal {
            message: "x".to_owned(),
        });
    }

    #[test]
    fn emit_routes_through_tracing_subscriber() {
        use std::sync::{Arc, Mutex};

        // A `tracing-subscriber`-compatible writer that accumulates bytes
        // into a shared buffer the test can inspect after the fact.
        #[derive(Clone, Default)]
        struct Capture(Arc<Mutex<Vec<u8>>>);

        impl std::io::Write for Capture {
            fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
                self.0
                    .lock()
                    .expect("capture lock poisoned")
                    .extend_from_slice(buf);
                Ok(buf.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }

        impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for Capture {
            type Writer = Self;
            fn make_writer(&'a self) -> Self::Writer {
                self.clone()
            }
        }

        let capture = Capture::default();
        let subscriber = tracing_subscriber::fmt()
            .json()
            .with_writer(capture.clone())
            .finish();

        tracing::subscriber::with_default(subscriber, || {
            emit(&Event::Started {
                pid: 99,
                version: "0.1.0",
            });
        });

        // Copy the captured bytes out of the mutex so the guard releases
        // before we touch the assertions below (clippy::significant_drop_tightening).
        let captured = {
            let bytes = capture.0.lock().expect("capture lock poisoned");
            String::from_utf8(bytes.clone()).expect("tracing-subscriber output should be utf-8")
        };
        // The JSON formatter wraps the event in its own envelope; the inner
        // JSON we emit appears as a string-valued `json` field. We assert
        // substring presence rather than exact shape so the test isn't
        // coupled to tracing-subscriber's envelope format (which may evolve).
        assert!(
            captured.contains("started"),
            "expected 'started' in: {captured}"
        );
        assert!(
            captured.contains("halmasuit::event"),
            "expected target 'halmasuit::event' in: {captured}"
        );
        assert!(captured.contains("99"), "expected pid '99' in: {captured}");
    }
}
