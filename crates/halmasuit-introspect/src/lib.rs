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
}

/// Compositor phases.
///
/// Further variants (`Greeter`, `Session`, `Locked`, `Shutdown`) land
/// alongside the code that enters them.
#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Phase {
    /// Initial phase: process has started, no subsystems initialized yet.
    Init,
    /// Wayland socket is bound and accepting client connections. No
    /// protocol globals are advertised yet — clients see an empty global
    /// list. Globals are added as their consuming code lands.
    WaylandReady,
    /// greetd protocol socket is bound and accepting greeter connections.
    /// The compositor is ready to host a greeter and drive PAM.
    GreetdReady,
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
