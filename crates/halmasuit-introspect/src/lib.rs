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
    /// completed and `StartSession` was received. The compositor hands
    /// the resolved uid/gid to `halmasuit-spawn` (the corresponding `cmd`
    /// and `env` from the greetd protocol are not emitted on this event
    /// surface because they carry user-influenced strings; the redaction
    /// policy lives in the snapshot-socket task).
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
    /// greeter wl_client is killed before niri becomes foreground" —
    /// emitted between `SessionRequested` and the `halmasuit-spawn`
    /// invocation so the greeter releases halmasuit's foreground
    /// slot before the user session asks for it.
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
    /// A composited frame was scanned out, with the test-only
    /// `frame_audit` analysis of its pixels. The variant is defined
    /// unconditionally so `halmasuit-introspect` (and its consumers'
    /// JSON schema) stay feature-independent; only halmasuit's
    /// *emission* of it is gated behind the `frame_audit` Cargo
    /// feature (the analysis costs a GPU readback per frame, which is
    /// unacceptable in the production binary — see Epic #1 req 6/7).
    ///
    /// Continuity invariants in `tests/visual-backdrop.nix` are
    /// asserted over the stream of these events: after the first
    /// background client commits, every frame must have
    /// `mean_luminance >= 0.01` (no black frame) and, once a
    /// BACKGROUND client has painted, `backdrop_coverage > 0.95`.
    FrameRendered {
        /// Monotonic per-process frame counter, starting at 0 on the
        /// first composited frame.
        frame_id: u64,
        /// Mean Rec.709 perceptual luma over the whole frame,
        /// normalized to `0.0..=1.0`. A black frame is `~0.0`.
        mean_luminance: f64,
        /// Fraction of pixels (`0.0..=1.0`) that differ from the brand
        /// clear color `#0a0014` — i.e. the share of the frame a
        /// wl_client actually painted over halmasuit's clear.
        backdrop_coverage: f64,
        /// 64-bit average-hash perceptual fingerprint of the frame
        /// (8x8 luma downscale, thresholded at the mean). Lets tests
        /// detect "the frame stopped changing" / gross content shifts
        /// without pixel-exact comparison.
        phash: u64,
    },
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
    use super::{Event, Phase, ShutdownReason, emit};
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
    fn event_frame_rendered_carries_audit_fields() {
        let v = round_trip(&Event::FrameRendered {
            frame_id: 42,
            mean_luminance: 0.375,
            backdrop_coverage: 0.987,
            phash: 0xDEAD_BEEF_0000_1234,
        });
        assert_eq!(v["event"], "frame_rendered");
        assert_eq!(v["frame_id"], 42);
        assert_eq!(v["mean_luminance"], 0.375);
        assert_eq!(v["backdrop_coverage"], 0.987);
        assert_eq!(v["phash"], 0xDEAD_BEEF_0000_1234u64);
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
