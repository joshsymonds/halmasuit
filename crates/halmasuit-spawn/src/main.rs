// halmasuit-spawn — setuid-root privilege-drop helper.
//
// This binary is the ONLY privileged code path in halmasuit. It must
// stay microscopic and audited. The unsafe-code forbid is non-negotiable.
//
// ## Expected argv schema (v2)
//
//   halmasuit-spawn <uid> <gid> <user> -- <command> [args...]
//
// - <uid>: target real/effective/saved UID (parsed as integer; must
//   match <user>'s pwent.pw_uid).
// - <gid>: target real/effective/saved GID (parsed as integer).
// - <user>: target username (used for XDG_RUNTIME_DIR derivation and
//   supplementary groups via initgroups()).
// - --: required separator.
// - <command> [args...]: the program to exec after privilege drop.
//
// ## Sequence (v2)
//
// 1. Validate argv shape (exactly the schema above; reject otherwise).
// 2. Confirm current EUID == 0 (we should be setuid-root; abort if not).
// 3. Sanitize envp: drop everything except an explicit allowlist
//    (XDG_RUNTIME_DIR, PATH, LANG, LC_*, HOME, USER, LOGNAME, SHELL).
// 4. setresgid(target_gid, target_gid, target_gid).
// 5. initgroups(target_user) — or setgroups(target_supplementary_groups).
// 6. setresuid(target_uid, target_uid, target_uid).
// 7. prctl(PR_SET_NO_NEW_PRIVS).
// 8. execve(command, sanitized_argv, sanitized_envp).
//
// No intervening syscalls touch user-controlled state between drop and exec.

#![forbid(unsafe_code)]

fn main() {
    // v1: this binary is not yet wired into the system. If anything
    // invokes it (e.g., a misconfigured NixOS module flipping the setuid
    // bit before v2 lands), fail closed with a precise message rather
    // than a generic `unimplemented!()` panic.
    eprintln!("halmasuit-spawn: v2 not implemented; this binary must not be invoked yet.");
    eprintln!(
        "If you are seeing this, audit any NixOS module that wired a setuid bit on this binary."
    );
    std::process::exit(1);
}
