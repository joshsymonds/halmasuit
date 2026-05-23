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
//! - `pam_sys` — unsafe surface #1: the hand-rolled libpam FFI
//!   declarations (Epic #5). One `unsafe extern "C"` block with
//!   `#[link(name = "pam")]`; declarations only — no call sites, no
//!   unsafe-block dispatch.
//! - `pam_ffi` — unsafe surface #2: the libpam FFI shim — every
//!   `unsafe { … }` call site against `pam_sys`'s declarations.
//! - `worker` — unsafe surface #3: the ephemeral `fork`/pidfd auth
//!   child (Epic R4).
//!
//! The THREE unsafe modules (`pam_sys`, `pam_ffi`, `worker`) carry NO
//! module `#![forbid]`; every `unsafe` (extern block or call site) has
//! `#[expect(unsafe_code, reason = "…")]`, so the workspace
//! `unsafe_code = "warn"` lint (denied under `clippy -D warnings`)
//! flags any stray or unjustified `unsafe` anywhere in the crate —
//! the same quarantine idiom as `halmasuit-pam`. Everything else
//! stays hard-`forbid`.

pub mod auth;
pub mod broker;
pub mod conv;
pub mod pam_ffi;
pub mod pam_sys;
pub mod responder;
pub mod session;
pub mod session_leader;
pub mod slot;
pub mod transport;
pub mod worker;

pub use auth::{AuthError, ResolvedIdentity, run_pam_auth};
pub use broker::{BrokerError, Disposition, run_broker};
pub use responder::ChannelResponder;
pub use session::{SessionError, run_session};
pub use session_leader::{SessionSpec, SpecError, sanitize_env, user_groups, validate};
pub use slot::{AuthSlot, SlotError};
pub use transport::{SeqpacketChannel, TransportError, peer_uid};
pub use worker::{
    ParentMessage, WorkerHandle, WorkerOutcome, accept_seqpacket, own_raw_fd, spawn_auth_worker,
    spawn_session_leader, spawn_session_worker,
};
