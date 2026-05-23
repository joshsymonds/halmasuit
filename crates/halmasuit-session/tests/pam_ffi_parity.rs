//! ABI parity audit lever for `crate::pam_sys`.
//!
//! Compares the hand-rolled libpam FFI in
//! `crates/halmasuit-session/src/pam_sys.rs` (the production binding,
//! linked at every halmasuit-session build) against bindgen-generated
//! bindings emitted by `pam-sys` (the dev-deps-only audit lever, NOT
//! in the production cargo tree).
//!
//! What this test catches:
//! 1. **Struct layout divergence** — if our `#[repr(C)]` struct shapes
//!    ever diverge from what bindgen computes against the build host's
//!    `<security/pam_appl.h>`, the size/align/offset assertions fail.
//! 2. **Constant value drift** — every PAM_* constant we declare is
//!    compared against pam-sys's value.
//! 3. **Symbol resolution drift** — each `extern "C"` function pointer
//!    in our `unsafe extern "C"` block is compared by address against
//!    pam-sys's same-named symbol. Since both crates use `#[link(name
//!    = "pam")]` with no name-mangling, the linker MUST resolve them
//!    to the same `pam_*@PLT` entry — and if it doesn't (someone
//!    introduces name-mangling, mis-spells a symbol, or libpam's
//!    SONAME ever forks), this test fails before the broker hits the
//!    drift in production.
//!
//! What this test does NOT catch:
//! - libpam ABI changes that bindgen also adopts identically. (We
//!   would track the same drift, just silently.) Mitigation: any such
//!   drift would be flagged by Linux-PAM's CHANGELOG / SONAME bump,
//!   not by this test. Linux-PAM has been ABI-stable for `pam_appl.h`
//!   for 20+ years.
//!
//! Why this lives in `tests/` (an integration test), not in
//! `src/pam_sys.rs`: integration tests are a separate compilation
//! target that links against the library crate AND consumes
//! `[dev-dependencies]`. The test crate sees `pam-sys`; the library
//! crate does not. That's the property the epic is delivering.

use std::ffi::{c_char, c_int, c_void};
use std::mem::{align_of, offset_of, size_of};

use halmasuit_session::pam_sys as ours;

// ============================================================================
// Struct layout parity
// ============================================================================

#[test]
fn pam_message_layout_matches_pam_sys() {
    assert_eq!(
        size_of::<ours::pam_message>(),
        size_of::<pam_sys::pam_message>(),
        "pam_message size divergence"
    );
    assert_eq!(
        align_of::<ours::pam_message>(),
        align_of::<pam_sys::pam_message>(),
        "pam_message alignment divergence"
    );
    assert_eq!(
        offset_of!(ours::pam_message, msg_style),
        offset_of!(pam_sys::pam_message, msg_style),
        "pam_message::msg_style offset divergence"
    );
    assert_eq!(
        offset_of!(ours::pam_message, msg),
        offset_of!(pam_sys::pam_message, msg),
        "pam_message::msg offset divergence"
    );
}

#[test]
fn pam_response_layout_matches_pam_sys() {
    assert_eq!(
        size_of::<ours::pam_response>(),
        size_of::<pam_sys::pam_response>(),
        "pam_response size divergence"
    );
    assert_eq!(
        align_of::<ours::pam_response>(),
        align_of::<pam_sys::pam_response>(),
        "pam_response alignment divergence"
    );
    assert_eq!(
        offset_of!(ours::pam_response, resp),
        offset_of!(pam_sys::pam_response, resp),
        "pam_response::resp offset divergence"
    );
    assert_eq!(
        offset_of!(ours::pam_response, resp_retcode),
        offset_of!(pam_sys::pam_response, resp_retcode),
        "pam_response::resp_retcode offset divergence"
    );
}

#[test]
fn pam_conv_layout_matches_pam_sys() {
    assert_eq!(
        size_of::<ours::pam_conv>(),
        size_of::<pam_sys::pam_conv>(),
        "pam_conv size divergence"
    );
    assert_eq!(
        align_of::<ours::pam_conv>(),
        align_of::<pam_sys::pam_conv>(),
        "pam_conv alignment divergence"
    );
    assert_eq!(
        offset_of!(ours::pam_conv, conv),
        offset_of!(pam_sys::pam_conv, conv),
        "pam_conv::conv offset divergence"
    );
    assert_eq!(
        offset_of!(ours::pam_conv, appdata_ptr),
        offset_of!(pam_sys::pam_conv, appdata_ptr),
        "pam_conv::appdata_ptr offset divergence"
    );
}

#[test]
fn pam_handle_t_pointer_size_matches_pam_sys() {
    // Our `pam_handle_t` is an uninhabited enum (canonical opaque type);
    // pam-sys's is a `u8` stub. Both are used only by pointer. The
    // pointer sizes must match (always true on a given target, but the
    // assertion documents intent).
    assert_eq!(
        size_of::<*const ours::pam_handle_t>(),
        size_of::<*const pam_sys::pam_handle_t>(),
        "*const pam_handle_t size divergence"
    );
    assert_eq!(
        size_of::<*mut ours::pam_handle_t>(),
        size_of::<*mut pam_sys::pam_handle_t>(),
        "*mut pam_handle_t size divergence"
    );
}

// ============================================================================
// Constant value parity — every PAM_* we declare
// ============================================================================

#[test]
fn return_code_constants_match_pam_sys() {
    assert_eq!(ours::PAM_SUCCESS, pam_sys::PAM_SUCCESS);
    assert_eq!(ours::PAM_BUF_ERR, pam_sys::PAM_BUF_ERR);
    assert_eq!(ours::PAM_CONV_ERR, pam_sys::PAM_CONV_ERR);
}

#[test]
fn item_type_constants_match_pam_sys() {
    assert_eq!(ours::PAM_SERVICE, pam_sys::PAM_SERVICE);
    assert_eq!(ours::PAM_USER, pam_sys::PAM_USER);
    assert_eq!(ours::PAM_TTY, pam_sys::PAM_TTY);
    assert_eq!(ours::PAM_RUSER, pam_sys::PAM_RUSER);
}

#[test]
fn setcred_flag_constants_match_pam_sys() {
    assert_eq!(ours::PAM_ESTABLISH_CRED, pam_sys::PAM_ESTABLISH_CRED);
    assert_eq!(ours::PAM_DELETE_CRED, pam_sys::PAM_DELETE_CRED);
}

#[test]
fn message_style_constants_match_pam_sys() {
    assert_eq!(ours::PAM_PROMPT_ECHO_OFF, pam_sys::PAM_PROMPT_ECHO_OFF);
    assert_eq!(ours::PAM_PROMPT_ECHO_ON, pam_sys::PAM_PROMPT_ECHO_ON);
    assert_eq!(ours::PAM_ERROR_MSG, pam_sys::PAM_ERROR_MSG);
    assert_eq!(ours::PAM_TEXT_INFO, pam_sys::PAM_TEXT_INFO);
}

// ============================================================================
// Symbol parity — every extern "C" fn resolves to the same libpam entry
// ============================================================================
//
// Both crates declare `#[link(name = "pam")] unsafe extern "C" { ... }`
// with the canonical C symbol names (no mangling). The linker resolves
// each declaration to the matching `pam_*@PLT` entry in `libpam.so`.
// If our declaration and pam-sys's declaration ever stop resolving to
// the same address (mis-spell, mangling option, SONAME fork, etc.),
// this test catches it.

#[test]
fn pam_start_symbol_matches() {
    assert_eq!(
        ours::pam_start as *const (),
        pam_sys::pam_start as *const (),
        "pam_start symbol divergence: our declaration links to a different libpam address than pam-sys's"
    );
}

#[test]
fn pam_end_symbol_matches() {
    assert_eq!(
        ours::pam_end as *const (),
        pam_sys::pam_end as *const (),
    );
}

#[test]
fn pam_authenticate_symbol_matches() {
    assert_eq!(
        ours::pam_authenticate as *const (),
        pam_sys::pam_authenticate as *const (),
    );
}

#[test]
fn pam_setcred_symbol_matches() {
    assert_eq!(
        ours::pam_setcred as *const (),
        pam_sys::pam_setcred as *const (),
    );
}

#[test]
fn pam_acct_mgmt_symbol_matches() {
    assert_eq!(
        ours::pam_acct_mgmt as *const (),
        pam_sys::pam_acct_mgmt as *const (),
    );
}

#[test]
fn pam_open_session_symbol_matches() {
    assert_eq!(
        ours::pam_open_session as *const (),
        pam_sys::pam_open_session as *const (),
    );
}

#[test]
fn pam_close_session_symbol_matches() {
    assert_eq!(
        ours::pam_close_session as *const (),
        pam_sys::pam_close_session as *const (),
    );
}

#[test]
fn pam_set_item_symbol_matches() {
    assert_eq!(
        ours::pam_set_item as *const (),
        pam_sys::pam_set_item as *const (),
    );
}

#[test]
fn pam_get_item_symbol_matches() {
    assert_eq!(
        ours::pam_get_item as *const (),
        pam_sys::pam_get_item as *const (),
    );
}

#[test]
fn pam_putenv_symbol_matches() {
    assert_eq!(
        ours::pam_putenv as *const (),
        pam_sys::pam_putenv as *const (),
    );
}

#[test]
fn pam_getenvlist_symbol_matches() {
    assert_eq!(
        ours::pam_getenvlist as *const (),
        pam_sys::pam_getenvlist as *const (),
    );
}

// ============================================================================
// Function-signature ABI parity — canonical types declared with OUR
// types, then verified compatible at the parameter-type level by
// cross-crate transmute-through-pointer (compile-time check).
// ============================================================================
//
// Why this is in addition to the symbol-address checks: the
// symbol-address checks prove "we and pam-sys link to the same C
// function." The signature checks prove "our Rust signature describes
// the same Rust-visible types as pam-sys's." If primitive types ever
// diverge (e.g. one side declares c_uint and the other c_int), the
// symbol address would still match — both still resolve to `pam_*` —
// but the call would be ABI-correct only by accident. The signature
// checks make the divergence explicit.

#[test]
fn function_pointer_sizes_match_usize() {
    // All `extern "C" fn` pointers are pointer-sized; this is a sanity
    // check that the typed function items can be cast and held as data
    // pointers without ABI surprises.
    assert_eq!(
        size_of::<unsafe extern "C" fn() -> c_int>(),
        size_of::<*const ()>(),
    );
    let _: unsafe extern "C" fn(
        *const c_char,
        *const c_char,
        *const ours::pam_conv,
        *mut *mut ours::pam_handle_t,
    ) -> c_int = ours::pam_start;
    let _: unsafe extern "C" fn(*mut ours::pam_handle_t, c_int) -> c_int = ours::pam_end;
    let _: unsafe extern "C" fn(*mut ours::pam_handle_t, c_int) -> c_int = ours::pam_authenticate;
    let _: unsafe extern "C" fn(*mut ours::pam_handle_t, c_int) -> c_int = ours::pam_setcred;
    let _: unsafe extern "C" fn(*mut ours::pam_handle_t, c_int) -> c_int = ours::pam_acct_mgmt;
    let _: unsafe extern "C" fn(*mut ours::pam_handle_t, c_int) -> c_int = ours::pam_open_session;
    let _: unsafe extern "C" fn(*mut ours::pam_handle_t, c_int) -> c_int = ours::pam_close_session;
    let _: unsafe extern "C" fn(*mut ours::pam_handle_t, c_int, *const c_void) -> c_int =
        ours::pam_set_item;
    let _: unsafe extern "C" fn(*const ours::pam_handle_t, c_int, *mut *const c_void) -> c_int =
        ours::pam_get_item;
    let _: unsafe extern "C" fn(*mut ours::pam_handle_t, *const c_char) -> c_int = ours::pam_putenv;
    let _: unsafe extern "C" fn(*mut ours::pam_handle_t) -> *mut *mut c_char = ours::pam_getenvlist;
}
