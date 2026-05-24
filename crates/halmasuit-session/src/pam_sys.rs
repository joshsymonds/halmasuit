//! Hand-written libpam FFI for the privileged broker.
//!
//! The privileged crate's ONLY libpam-linking surface, replacing the
//! third-party `pam-sys` crate (Epic #5). Production builds link
//! `-lpam` directly: no `build.rs`, no `bindgen`, no `clang-sys`, no
//! `libclang.so` at production build time. Drift detection lives in
//! `tests/pam_ffi_parity.rs`, which keeps `pam-sys` as a
//! `[dev-dependencies]`-only audit lever and asserts struct layout +
//! constant values + function-pointer-signature compatibility match
//! what bindgen produces against the build host's libpam headers.
//!
//! Pattern follows sudo-rs's hand-curated bindings — the most security-
//! audited Rust libpam consumer in existence (Radically Open Security
//! audits 2023 and 2025; the FFI surface has never been a finding).
//! Only the symbols, structs, and constants `pam_ffi.rs` actually
//! consumes are declared here; expanding this module is a security-
//! review event.
//!
//! ## Sourcing
//!
//! Function prototypes, struct layouts, and constant values verified
//! against Linux-PAM 1.6.1 (github.com/linux-pam/linux-pam, tag
//! `v1.6.1`):
//!
//! - `<security/_pam_types.h>` — `pam_handle_t`, `pam_message`,
//!   `pam_response`, `pam_conv`, status codes, item types, flags,
//!   message styles.
//! - `<security/pam_appl.h>` — `pam_start`, `pam_end`, `pam_set_item`,
//!   `pam_get_item`, `pam_authenticate`, `pam_setcred`, `pam_acct_mgmt`,
//!   `pam_open_session`, `pam_close_session`, `pam_putenv`,
//!   `pam_getenvlist`.
//!
//! Constant values double-checked against sudo-rs's
//! `src/pam/sys_linuxpam.rs`
//! (github.com/trifectatechfoundation/sudo-rs).
//!
//! ## ABI trap: Linux-PAM vs OpenPAM `pam_message` array layout
//!
//! Linux-PAM's conversation callback receives `*mut *const pam_message`
//! — a *pointer to an array of pointers* to `pam_message` structs. The
//! caller indexes via `*msg.add(i)` (dereferencing each pointer in the
//! array of pointers).
//!
//! OpenPAM (Solaris, FreeBSD, macOS) uses `*const *const pam_message`
//! semantically — ONE pointer to a contiguous array of `pam_message`
//! structs. The same `*msg.add(i)` expression on OpenPAM would walk
//! past the end of the first `pam_message`, not into the next element.
//!
//! halmasuit targets Linux-PAM exclusively (CLAUDE.md hard rule: this
//! is a Linux system compositor). The conversation function-pointer
//! type [`pam_conv_fn`] and the marshalling code in
//! `pam_ffi.rs::conv_trampoline` encode the Linux layout. **Do not
//! "fix" the indirection to match OpenPAM** — it would break Linux and
//! we don't support OpenPAM.
//!
//! ## ABI notes for [`pam_conv_fn`]
//!
//! - Return type is [`c_int`], not `()`. Returning [`PAM_CONV_ERR`]
//!   aborts the PAM transaction; returning [`PAM_BUF_ERR`] signals
//!   allocation failure.
//! - `*resp` is OUT-only. libpam reads from `*msg`, writes via `*resp`.
//!   The callback must `calloc(num_msg, sizeof(pam_response))` the
//!   array and write into it. libpam frees both the array and each
//!   non-NULL `.resp` with `free(3)`.
//! - Panic-unwinding across the `extern "C"` boundary is UB. The
//!   trampoline in `pam_ffi.rs` wraps the callback body in
//!   `panic::catch_unwind`.
//!
//! ## ABI notes for `pam_getenvlist`
//!
//! Returns `*mut *mut c_char` — NULL-terminated array of strings.
//! **The application owns BOTH the array and each individual string**
//! and MUST free them with `free(3)`. The marshalling in
//! `pam_ffi.rs::PamHandle::getenvlist` does this.
//!
//! ## ABI notes for `pam_set_item(PAM_CONV, ...)`
//!
//! Takes `*const c_void` pointing at a [`pam_conv`] struct. The
//! `appdata_ptr` field inside that struct must remain valid for the
//! handle's lifetime; libpam dereferences it on every conversation
//! callback invocation. The existing `pam_ffi.rs::Pam` keeps the
//! `pam_conv` boxed and owned for exactly this reason.

#![expect(
    non_camel_case_types,
    reason = "matches the canonical libpam C type names (pam_handle_t, pam_message, ...); diverging from upstream names would obscure the FFI relationship to <security/pam_appl.h>"
)]

use std::ffi::{c_char, c_int, c_void};

// ============================================================================
// Opaque handle
// ============================================================================

/// Opaque PAM transaction handle. The application only ever holds a
/// pointer to this; libpam owns the storage.
///
/// `pub enum X {}` is the canonical Rust opaque-foreign-type idiom: it
/// refuses construction in safe Rust, refuses to lie about its size,
/// and matches the `extern type` semantics RFC 1861 awaits stabilizing.
/// Notably better than `pub type pam_handle_t = u8` (what pam-sys's
/// bindgen output emits via `.opaque_type()`) because the latter lies
/// about size and lets safe code construct a value.
pub enum pam_handle_t {}

// ============================================================================
// Return codes — `<security/_pam_types.h>`
// ============================================================================

/// Successful PAM call. `_pam_types.h`.
pub const PAM_SUCCESS: c_int = 0;
/// Memory buffer error inside libpam. `_pam_types.h`.
pub const PAM_BUF_ERR: c_int = 5;
/// Conversation callback returned an error / aborted the transaction.
/// `_pam_types.h`.
pub const PAM_CONV_ERR: c_int = 19;

// ============================================================================
// Item types — `pam_{set,get}_item` second argument — `_pam_types.h`
// ============================================================================

/// Service name (selects `/etc/pam.d/<service>`).
pub const PAM_SERVICE: c_int = 1;
/// Authenticated user name (libpam-canonical; read back after
/// `pam_authenticate`).
pub const PAM_USER: c_int = 2;
/// TTY associated with the auth attempt (`pam_securetty`, `pam_systemd`,
/// audit modules read this).
pub const PAM_TTY: c_int = 3;
/// Requesting user — the user initiating the auth on behalf of
/// [`PAM_USER`]. For halmasuit this is the greeter system user
/// (`greeter`, uid 999).
pub const PAM_RUSER: c_int = 8;

// ============================================================================
// `pam_setcred` flags — `<security/_pam_types.h>`
// ============================================================================

/// Initialize / set up the user's credentials (post-`pam_authenticate`).
pub const PAM_ESTABLISH_CRED: c_int = 0x0002;
/// Delete the user's credentials (on session logout).
pub const PAM_DELETE_CRED: c_int = 0x0004;

// ============================================================================
// `pam_message::msg_style` values — `<security/_pam_types.h>`
// ============================================================================

/// Password-style prompt (echo off, secret input expected).
pub const PAM_PROMPT_ECHO_OFF: c_int = 1;
/// Visible-input prompt (echo on, e.g. for username).
pub const PAM_PROMPT_ECHO_ON: c_int = 2;
/// Error-text display (no input expected).
pub const PAM_ERROR_MSG: c_int = 3;
/// Informational-text display (no input expected).
pub const PAM_TEXT_INFO: c_int = 4;

// ============================================================================
// Structs — `<security/_pam_types.h>`
// ============================================================================

/// Conversation message handed by libpam to the conversation callback.
///
/// `msg_style` is one of [`PAM_PROMPT_ECHO_OFF`] / [`PAM_PROMPT_ECHO_ON`]
/// / [`PAM_ERROR_MSG`] / [`PAM_TEXT_INFO`]. `msg` is a NUL-terminated C
/// string the callback should display (or use as a prompt).
#[repr(C)]
pub struct pam_message {
    pub msg_style: c_int,
    pub msg: *const c_char,
}

/// Conversation response written by the conversation callback for
/// libpam to read back.
///
/// `resp` is a NUL-terminated C string allocated with `malloc(3)` /
/// `calloc(3)` / `strdup(3)`; libpam frees it with `free(3)`. `resp`
/// may be NULL for message styles that don't require input
/// ([`PAM_TEXT_INFO`], [`PAM_ERROR_MSG`]). `resp_retcode` is unused on
/// Linux-PAM and should be zero (Linux-PAM treats it as reserved).
#[repr(C)]
pub struct pam_response {
    pub resp: *mut c_char,
    pub resp_retcode: c_int,
}

/// Conversation callback type for Linux-PAM. See module-level
/// "ABI trap" section for the Linux-vs-OpenPAM layout difference of the
/// `msg` parameter.
///
/// Semantics:
/// - `num_msg` — count of messages in the `msg` array.
/// - `msg` — pointer to an array of pointers to [`pam_message`]. The
///   callback indexes via `*msg.add(i)` to dereference each pointer.
/// - `resp` — out-parameter; callback writes a pointer to a freshly
///   `calloc(num_msg, sizeof(pam_response))`-allocated array. libpam
///   frees the array and each non-NULL `.resp` with `free(3)`.
/// - `appdata_ptr` — opaque application-side pointer carried from
///   [`pam_conv::appdata_ptr`].
/// - Returns [`PAM_SUCCESS`], [`PAM_CONV_ERR`], or [`PAM_BUF_ERR`].
pub type pam_conv_fn = unsafe extern "C" fn(
    num_msg: c_int,
    msg: *mut *const pam_message,
    resp: *mut *mut pam_response,
    appdata_ptr: *mut c_void,
) -> c_int;

/// Conversation function + opaque application data, handed to libpam
/// via `pam_set_item(PAM_CONV, ...)` (or `pam_start`).
///
/// **The application must keep this struct alive for the lifetime of
/// the PAM handle** — libpam stores the pointer, not a copy, and
/// dereferences it on every conversation invocation.
#[repr(C)]
pub struct pam_conv {
    pub conv: Option<pam_conv_fn>,
    pub appdata_ptr: *mut c_void,
}

// ============================================================================
// libpam extern block — `<security/pam_appl.h>`
// ============================================================================

#[expect(
    unsafe_code,
    reason = "this is the libpam FFI surface; SAFETY for each call site is documented in pam_ffi.rs at the #[expect(unsafe_code, ...)] sites"
)]
#[link(name = "pam")]
unsafe extern "C" {
    /// Initialize a PAM transaction.
    ///
    /// `service_name` selects `/etc/pam.d/<service>`. `user` may be NULL
    /// (libpam will prompt via the conversation callback when an
    /// authentication routine needs a username). On success, writes a
    /// new transaction handle to `*pamh`.
    pub fn pam_start(
        service_name: *const c_char,
        user: *const c_char,
        pam_conversation: *const pam_conv,
        pamh: *mut *mut pam_handle_t,
    ) -> c_int;

    /// Tear down a PAM transaction and free libpam-side storage.
    /// `pam_status` should be the last observed return code from a
    /// previous `pam_*` call on this handle (used by module cleanup
    /// hooks).
    pub fn pam_end(pamh: *mut pam_handle_t, pam_status: c_int) -> c_int;

    /// Run the configured `auth` stack. Drives the conversation
    /// callback for any prompts.
    pub fn pam_authenticate(pamh: *mut pam_handle_t, flags: c_int) -> c_int;

    /// Run the `setcred` stack. Called with [`PAM_ESTABLISH_CRED`] after
    /// `pam_authenticate` succeeds, then with [`PAM_DELETE_CRED`] on
    /// session close.
    pub fn pam_setcred(pamh: *mut pam_handle_t, flags: c_int) -> c_int;

    /// Run the `account` stack. Enforces account-level restrictions
    /// (password aging, time-of-day, etc.).
    pub fn pam_acct_mgmt(pamh: *mut pam_handle_t, flags: c_int) -> c_int;

    /// Run the `session` open stack. `pam_systemd` / `logind` register
    /// the session here.
    pub fn pam_open_session(pamh: *mut pam_handle_t, flags: c_int) -> c_int;

    /// Run the `session` close stack. `pam_systemd` / `logind`
    /// deregister the session here.
    pub fn pam_close_session(pamh: *mut pam_handle_t, flags: c_int) -> c_int;

    /// Set a PAM item by type (e.g. [`PAM_TTY`], [`PAM_RUSER`],
    /// `PAM_CONV`). libpam copies strings; for `PAM_CONV`, libpam
    /// stores the [`pam_conv`] pointer and the application keeps the
    /// allocation alive.
    pub fn pam_set_item(pamh: *mut pam_handle_t, item_type: c_int, item: *const c_void) -> c_int;

    /// Read back a PAM item. Writes a libpam-owned pointer into
    /// `*item`; do not free.
    ///
    /// The handle parameter is `*const pam_handle_t` here to match
    /// pam-sys's bindgen output (the canonical C signature varies in
    /// `const`-ness across libpam header versions; both forms have
    /// identical ABI on x86_64 / aarch64). The parity test in
    /// `tests/pam_ffi_parity.rs` verifies signature compatibility.
    pub fn pam_get_item(
        pamh: *const pam_handle_t,
        item_type: c_int,
        item: *mut *const c_void,
    ) -> c_int;

    /// Set a PAM environment variable for the transaction.
    /// `name_value` is `"NAME=value"` or `"NAME"` to unset. Must be
    /// called BEFORE `pam_open_session` so `pam_systemd`/`logind` see
    /// the right env (Epic Amendment A1.2).
    pub fn pam_putenv(pamh: *mut pam_handle_t, name_value: *const c_char) -> c_int;

    /// Return the PAM environment as a NULL-terminated `malloc(3)`-
    /// allocated array of `malloc(3)`-allocated `"NAME=value"` strings.
    /// **Caller frees both the array and each string** with `free(3)`.
    /// Returns NULL on libpam allocation failure (a successful empty
    /// environment is a non-NULL array whose first element is NULL,
    /// distinct from this NULL return).
    pub fn pam_getenvlist(pamh: *mut pam_handle_t) -> *mut *mut c_char;
}

// ============================================================================
// Layout assertions — pin our `#[repr(C)]` struct shapes against the C
// ABI rules directly, with hardcoded LP64 expectations.
//
// These checks are *complementary* to `tests/pam_ffi_parity.rs`, which
// compares our types against bindgen's output. The parity test catches
// divergence between us and bindgen; this in-module check additionally
// catches the (admittedly remote) case where us AND bindgen drift in
// the same direction — bindgen's reference is the build host's libpam
// headers, so a host whose headers are wrong would mask the drift in
// the parity test but not here. They also run when pam-sys is
// excluded (e.g. a future downstream consumer of halmasuit-session
// that builds with `--no-default-features` and skips dev-deps).
//
// Assertions are LP64-correct (x86_64, aarch64, riscv64, etc.) —
// halmasuit's supported targets are all LP64 Linux. No cfg gate is
// needed.
// ============================================================================

#[cfg(test)]
mod layout {
    use std::mem::{align_of, offset_of, size_of};

    use super::{pam_conv, pam_message, pam_response};

    #[test]
    fn pam_message_layout() {
        // sizeof(struct pam_message) on LP64:
        //   int msg_style (4) + pad (4) + const char *msg (8) = 16
        assert_eq!(size_of::<pam_message>(), 16);
        assert_eq!(align_of::<pam_message>(), 8);
        assert_eq!(offset_of!(pam_message, msg_style), 0);
        assert_eq!(offset_of!(pam_message, msg), 8);
    }

    #[test]
    fn pam_response_layout() {
        // sizeof(struct pam_response) on LP64:
        //   char *resp (8) + int resp_retcode (4) + pad (4) = 16
        assert_eq!(size_of::<pam_response>(), 16);
        assert_eq!(align_of::<pam_response>(), 8);
        assert_eq!(offset_of!(pam_response, resp), 0);
        assert_eq!(offset_of!(pam_response, resp_retcode), 8);
    }

    #[test]
    fn pam_conv_layout() {
        // sizeof(struct pam_conv) on LP64:
        //   fn pointer (8, via Option<fn>'s null-pointer optimization)
        //   + void *appdata_ptr (8) = 16
        assert_eq!(size_of::<pam_conv>(), 16);
        assert_eq!(align_of::<pam_conv>(), 8);
        assert_eq!(offset_of!(pam_conv, conv), 0);
        assert_eq!(offset_of!(pam_conv, appdata_ptr), 8);
    }
}
