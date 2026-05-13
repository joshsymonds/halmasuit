// halmasuit-spawn — setuid-root privilege-drop helper.
//
// This binary is the ONLY privileged code path in halmasuit. It must
// stay microscopic and audited. The unsafe-code forbid is non-negotiable.
//
// ## Expected argv schema
//
//   halmasuit-spawn <uid> <gid> <user> -- <command> [args...]
//
// - <uid>: target real/effective/saved UID (parsed as integer; must
//   match <user>'s pwent.pw_uid).
// - <gid>: target real/effective/saved GID (parsed as integer).
// - <user>: target username (used for initgroups() — the ONLY file-I/O
//   path in this helper, gated to a single NSS pwent lookup).
// - --: required separator.
// - <command> [args...]: the program to exec after privilege drop.
//   <command> is the absolute path passed to execve(2); we do NOT search
//   PATH at exec time.
//
// ## Refusal rules (load-bearing security property)
//
// halmasuit-spawn refuses to set target_uid or target_gid below UID_MIN
// (1000). This is what makes the privilege split *not* theater. A
// compromised halmasuit can invoke this helper — that is in the threat
// model and not preventable. What the floor prevents is using the helper
// to escalate to root or another system user. With the floor in place,
// the worst-case outcome of full RCE in halmasuit is "spawn arbitrary
// commands as the currently-logged-in user." Without it, the split is
// theater. Do not remove this check.
//
// ## Sequence
//
// 1. Validate argv shape (parse_argv).
// 2. Confirm current EUID == 0 (geteuid).
// 3. Enforce UID floor (enforce_uid_floor; >= 1000).
// 4. Sanitize envp via allowlist (sanitize_env).
// 5. setresgid(target_gid, target_gid, target_gid).
// 6. initgroups(target_user, target_gid).
// 7. setresuid(target_uid, target_uid, target_uid).
// 8. prctl(PR_SET_NO_NEW_PRIVS).
// 9. execve(command[0], command, sanitized_envp) — never returns on success.
//
// No allocations and no user-controlled-state syscalls between privilege
// drop and execve. Steps 5–9 are straight-line.

#![forbid(unsafe_code)]

use std::ffi::CString;
use std::process::ExitCode;

use halmasuit_spawn::{enforce_uid_floor, parse_argv, sanitize_env};
use nix::sys::prctl;
use nix::unistd::{Gid, Uid, execve, geteuid, initgroups, setresgid, setresuid};

fn main() -> ExitCode {
    let parsed = match parse_argv(std::env::args_os()) {
        Ok(p) => p,
        Err(e) => return die(&format!("argv: {e}"), 64),
    };

    if !geteuid().is_root() {
        return die("not running as root (EUID != 0); refusing to proceed", 1);
    }

    if let Err(e) = enforce_uid_floor(parsed.target_uid, parsed.target_gid) {
        return die(&format!("UID floor: {e}"), 1);
    }

    let env = sanitize_env(std::env::vars_os());
    let target_uid = Uid::from_raw(parsed.target_uid);
    let target_gid = Gid::from_raw(parsed.target_gid);

    // Build the execve ref-vectors BEFORE privilege drop so the privileged
    // window between setresgid and execve contains zero allocations (matches
    // the spec comment in the header block; audit-grade clarity).
    let argv: Vec<&CString> = parsed.command.iter().collect();
    let env_refs: Vec<&CString> = env.iter().collect();
    let path = &parsed.command[0];

    // ── Privilege drop. Straight-line; no allocations and no syscalls
    //    touch user-controlled state between here and execve. ──────────
    if let Err(e) = setresgid(target_gid, target_gid, target_gid) {
        return die(&format!("setresgid: {e}"), 1);
    }
    if let Err(e) = initgroups(parsed.target_user.as_c_str(), target_gid) {
        return die(&format!("initgroups: {e}"), 1);
    }
    if let Err(e) = setresuid(target_uid, target_uid, target_uid) {
        return die(&format!("setresuid: {e}"), 1);
    }
    if let Err(e) = prctl::set_no_new_privs() {
        return die(&format!("PR_SET_NO_NEW_PRIVS: {e}"), 1);
    }

    match execve(path, &argv, &env_refs) {
        Ok(_) => unreachable!("execve returns Infallible on success"),
        Err(e) => die(&format!("execve: {e}"), 127),
    }
}

fn die(msg: &str, code: u8) -> ExitCode {
    eprintln!("halmasuit-spawn: {msg}");
    ExitCode::from(code)
}
