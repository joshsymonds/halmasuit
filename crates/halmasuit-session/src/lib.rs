//! `halmasuit-session` — the privileged PAM-lifecycle broker of the
//! unified session/pamd epic.
//!
//! Per Epic #1 R2/R14 this crate is the SOLE libpam-linking surface in
//! the workspace and the successor to `halmasuit-pam` (which is deleted
//! from the workspace once the broker is the live auth path).
//!
//! Module map / unsafe boundary:
//! - `transport` — SOCK_SEQPACKET framed channel + SO_PEERCRED; hard
//!   module-level `#![forbid(unsafe_code)]` (all syscalls via `nix`).
//! - `conv` — pure libpam-conversation ↔ frame mapping; hard
//!   module-level `#![forbid(unsafe_code)]`.
//! - `responder` / `auth` — pure composition; hard module-level
//!   `#![forbid(unsafe_code)]`.
//! - `pam_ffi` — unsafe surface #1: the libpam FFI shim.
//! - `worker` — unsafe surface #2: the ephemeral `fork`/pidfd auth
//!   child (Epic R4).
//!
//! The TWO unsafe modules (`pam_ffi`, `worker`) carry NO module
//! `#![forbid]`; every unsafe block in them has
//! `#[expect(unsafe_code, reason = "…")]`, so the workspace
//! `unsafe_code = "warn"` lint (denied under `clippy -D warnings`)
//! flags any stray or unjustified `unsafe` anywhere in the crate —
//! the same quarantine idiom as `halmasuit-pam`. Everything else
//! stays hard-`forbid`.

pub mod auth;
pub mod broker;
pub mod conv;
pub mod pam_ffi;
pub mod responder;
pub mod session;
pub mod session_leader;
pub mod slot;
pub mod transport;
pub mod worker;

pub use auth::{AuthError, ResolvedIdentity, run_pam_auth};
pub use broker::{BrokerError, Disposition, handle_connection};
pub use responder::ChannelResponder;
pub use session::{SessionError, run_session};
pub use session_leader::{SessionSpec, SpecError, merged_groups, sanitize_env, validate};
pub use slot::{AuthSlot, SlotError};
pub use transport::{SeqpacketChannel, TransportError, peer_uid};
pub use worker::{
    ParentMessage, WorkerHandle, WorkerOutcome, accept_seqpacket, spawn_auth_worker,
    spawn_session_leader, spawn_session_worker,
};
