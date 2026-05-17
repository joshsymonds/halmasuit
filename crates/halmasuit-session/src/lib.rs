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
//! - `pam_ffi` — the ONLY `unsafe` surface: the libpam FFI shim. Every
//!   unsafe block carries `#[expect(unsafe_code, reason = "…")]`, so the
//!   workspace `unsafe_code = "warn"` lint (denied under `clippy -D
//!   warnings`) flags any stray or unjustified `unsafe` anywhere in the
//!   crate — the same quarantine idiom as `halmasuit-pam`.

pub mod conv;
pub mod pam_ffi;
pub mod transport;

pub use transport::{SeqpacketChannel, TransportError, peer_uid};
