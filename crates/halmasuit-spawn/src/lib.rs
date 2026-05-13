//! Pure-logic primitives for the halmasuit-spawn setuid helper.
//!
//! Split out of `main.rs` so the argv parser, UID-floor validator, and
//! env allowlist can be exercised by unit + property tests without
//! requiring root privileges. `main.rs` composes these with the
//! privileged syscall sequence; the privileged path itself is exercised
//! by the NixOS VM test.

#![forbid(unsafe_code)]

use std::ffi::{CString, OsString};
use std::os::unix::ffi::OsStringExt;

/// Minimum acceptable target UID/GID.
///
/// Refusing anything below this is the load-bearing security property
/// documented in ARCHITECTURE.md threat model row 11 — without it, a
/// compromised halmasuit could spawn as root and the privilege split is
/// theater.
pub const UID_MIN: u32 = 1000;

/// Environment variable names that pass through the privilege drop.
///
/// Anything outside this list (plus a `LC_*` prefix match) is stripped.
/// `LD_PRELOAD` / `LD_LIBRARY_PATH` / `MALLOC_CONF` etc. are deliberately
/// absent — those are the standard setuid escalation vectors.
pub const ENV_ALLOWLIST: &[&[u8]] = &[
    b"XDG_RUNTIME_DIR",
    b"PATH",
    b"LANG",
    b"HOME",
    b"USER",
    b"LOGNAME",
    b"SHELL",
];

#[derive(Debug, PartialEq, Eq)]
pub struct ParsedArgs {
    pub target_uid: u32,
    pub target_gid: u32,
    pub target_user: CString,
    /// First element is the path passed to execve; the whole vector is
    /// argv for the target process.
    pub command: Vec<CString>,
}

#[derive(Debug, PartialEq, Eq)]
pub enum SpawnError {
    /// argv shape wrong: count, missing `--`, non-numeric uid/gid, empty
    /// command, etc.
    Argv(&'static str),
    /// target_uid or target_gid below `UID_MIN`. The offending value is
    /// carried so the operator can see what was refused.
    UidFloor(u32),
    /// A path/argument string contained an interior NUL byte and so cannot
    /// be passed to `execve` via a `CString`.
    InvalidString,
}

impl std::fmt::Display for SpawnError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Argv(s) => f.write_str(s),
            Self::UidFloor(v) => write!(f, "uid/gid {v} is below UID_MIN ({UID_MIN})"),
            Self::InvalidString => f.write_str("argument contained NUL byte"),
        }
    }
}

impl std::error::Error for SpawnError {}

/// Parse the canonical argv schema:
///
/// ```text
/// halmasuit-spawn <uid> <gid> <user> -- <command> [args...]
/// ```
///
/// argv[0] is consumed but not validated (the caller of execve(2) chose it).
pub fn parse_argv<I>(args: I) -> Result<ParsedArgs, SpawnError>
where
    I: IntoIterator<Item = OsString>,
{
    let mut it = args.into_iter();
    it.next().ok_or(SpawnError::Argv("missing argv[0]"))?;
    let uid_arg = it.next().ok_or(SpawnError::Argv("missing <uid>"))?;
    let gid_arg = it.next().ok_or(SpawnError::Argv("missing <gid>"))?;
    let user_arg = it.next().ok_or(SpawnError::Argv("missing <user>"))?;
    let sep = it.next().ok_or(SpawnError::Argv("missing -- separator"))?;
    if sep.as_encoded_bytes() != b"--" {
        return Err(SpawnError::Argv("expected -- separator"));
    }

    let target_uid = parse_u32_decimal(uid_arg.as_encoded_bytes())?;
    let target_gid = parse_u32_decimal(gid_arg.as_encoded_bytes())?;
    let target_user = osstring_to_cstring(user_arg)?;

    let command: Vec<CString> = it.map(osstring_to_cstring).collect::<Result<_, _>>()?;
    if command.is_empty() {
        return Err(SpawnError::Argv("missing command after --"));
    }

    Ok(ParsedArgs {
        target_uid,
        target_gid,
        target_user,
        command,
    })
}

/// Refuse any uid/gid below `UID_MIN`.
///
/// This must NEVER be relaxed — see ARCHITECTURE.md threat model row 11.
pub const fn enforce_uid_floor(uid: u32, gid: u32) -> Result<(), SpawnError> {
    if uid < UID_MIN {
        Err(SpawnError::UidFloor(uid))
    } else if gid < UID_MIN {
        Err(SpawnError::UidFloor(gid))
    } else {
        Ok(())
    }
}

/// Filter an env map through the allowlist + `LC_*` prefix.
///
/// Produces `KEY=VALUE` CStrings ready for `execve`. Keys with interior
/// NULs (which no real OS hands us, but we won't panic on them) are
/// silently dropped.
pub fn sanitize_env<I>(env: I) -> Vec<CString>
where
    I: IntoIterator<Item = (OsString, OsString)>,
{
    env.into_iter()
        .filter(|(k, _)| is_env_allowed(k.as_encoded_bytes()))
        .filter_map(|(k, v)| {
            let mut bytes = k.into_vec();
            bytes.push(b'=');
            bytes.extend_from_slice(&v.into_vec());
            CString::new(bytes).ok()
        })
        .collect()
}

fn is_env_allowed(key: &[u8]) -> bool {
    ENV_ALLOWLIST.contains(&key) || key.starts_with(b"LC_")
}

fn parse_u32_decimal(bytes: &[u8]) -> Result<u32, SpawnError> {
    if bytes.is_empty() || !bytes.iter().all(u8::is_ascii_digit) {
        return Err(SpawnError::Argv("uid/gid must be ASCII digits"));
    }
    // Safe because we verified ASCII digits above.
    let s = std::str::from_utf8(bytes).expect("ascii digits are utf-8");
    s.parse::<u32>()
        .map_err(|_| SpawnError::Argv("uid/gid out of range"))
}

fn osstring_to_cstring(s: OsString) -> Result<CString, SpawnError> {
    CString::new(s.into_vec()).map_err(|_| SpawnError::InvalidString)
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    fn os(s: &str) -> OsString {
        OsString::from(s)
    }

    fn argv(xs: &[&str]) -> Vec<OsString> {
        xs.iter().copied().map(os).collect()
    }

    // ── parse_argv ────────────────────────────────────────────────────

    #[test]
    fn parse_argv_happy_path() {
        let p = parse_argv(argv(&[
            "halmasuit-spawn",
            "1000",
            "100",
            "alice",
            "--",
            "/usr/bin/id",
            "-u",
        ]))
        .expect("happy path should parse");
        assert_eq!(p.target_uid, 1000);
        assert_eq!(p.target_gid, 100);
        assert_eq!(p.target_user.to_bytes(), b"alice");
        assert_eq!(p.command.len(), 2);
        assert_eq!(p.command[0].to_bytes(), b"/usr/bin/id");
        assert_eq!(p.command[1].to_bytes(), b"-u");
    }

    #[test]
    fn parse_argv_rejects_missing_separator() {
        let err = parse_argv(argv(&[
            "halmasuit-spawn",
            "1000",
            "100",
            "alice",
            "/usr/bin/id",
        ]))
        .unwrap_err();
        assert!(matches!(err, SpawnError::Argv(_)), "{err:?}");
    }

    #[test]
    fn parse_argv_rejects_non_numeric_uid() {
        let err = parse_argv(argv(&[
            "halmasuit-spawn",
            "alice",
            "100",
            "alice",
            "--",
            "/usr/bin/id",
        ]))
        .unwrap_err();
        assert!(matches!(err, SpawnError::Argv(_)), "{err:?}");
    }

    #[test]
    fn parse_argv_rejects_negative_uid() {
        let err = parse_argv(argv(&[
            "halmasuit-spawn",
            "-1",
            "100",
            "alice",
            "--",
            "/usr/bin/id",
        ]))
        .unwrap_err();
        assert!(matches!(err, SpawnError::Argv(_)), "{err:?}");
    }

    #[test]
    fn parse_argv_rejects_empty_command() {
        let err = parse_argv(argv(&["halmasuit-spawn", "1000", "100", "alice", "--"])).unwrap_err();
        assert!(matches!(err, SpawnError::Argv(_)), "{err:?}");
    }

    #[test]
    fn parse_argv_rejects_too_few_args() {
        let err = parse_argv(argv(&["halmasuit-spawn", "1000"])).unwrap_err();
        assert!(matches!(err, SpawnError::Argv(_)), "{err:?}");
    }

    proptest! {
        #[test]
        fn parse_argv_round_trip(
            uid in 1000u32..u32::MAX,
            gid in 1000u32..u32::MAX,
            user in "[a-z][a-z0-9_-]{0,15}",
            cmd in r"/[a-zA-Z0-9/_.-]+",
        ) {
            let v = argv(&[
                "halmasuit-spawn",
                &uid.to_string(),
                &gid.to_string(),
                &user,
                "--",
                &cmd,
            ]);
            let p = parse_argv(v).expect("valid input should parse");
            prop_assert_eq!(p.target_uid, uid);
            prop_assert_eq!(p.target_gid, gid);
            prop_assert_eq!(p.target_user.to_bytes(), user.as_bytes());
            prop_assert_eq!(p.command[0].to_bytes(), cmd.as_bytes());
        }

        #[test]
        fn parse_argv_garbage_does_not_panic(garbage in proptest::collection::vec(".*", 0..8)) {
            // Any vec of arbitrary strings must return Ok or a SpawnError
            // — never panic. The function is parsing untrusted input.
            let v: Vec<OsString> = garbage.into_iter().map(OsString::from).collect();
            let _ = parse_argv(v);
        }
    }

    // ── enforce_uid_floor ─────────────────────────────────────────────

    #[test]
    fn floor_accepts_minimum() {
        assert_eq!(enforce_uid_floor(UID_MIN, UID_MIN), Ok(()));
    }

    #[test]
    fn floor_accepts_above_minimum() {
        assert_eq!(enforce_uid_floor(2000, 1500), Ok(()));
    }

    #[test]
    fn floor_refuses_root_uid() {
        assert_eq!(enforce_uid_floor(0, 1000), Err(SpawnError::UidFloor(0)));
    }

    #[test]
    fn floor_refuses_root_gid() {
        assert_eq!(enforce_uid_floor(1000, 0), Err(SpawnError::UidFloor(0)));
    }

    #[test]
    fn floor_refuses_just_below_threshold() {
        assert_eq!(enforce_uid_floor(999, 1000), Err(SpawnError::UidFloor(999)));
    }

    proptest! {
        #[test]
        fn floor_refuses_every_system_uid(uid in 0u32..UID_MIN) {
            prop_assert!(enforce_uid_floor(uid, 1000).is_err());
            prop_assert!(enforce_uid_floor(1000, uid).is_err());
        }

        #[test]
        fn floor_accepts_every_user_uid(uid in UID_MIN..u32::MAX, gid in UID_MIN..u32::MAX) {
            prop_assert_eq!(enforce_uid_floor(uid, gid), Ok(()));
        }
    }

    // ── sanitize_env ──────────────────────────────────────────────────

    fn env(pairs: &[(&str, &str)]) -> Vec<(OsString, OsString)> {
        pairs
            .iter()
            .map(|(k, v)| (OsString::from(k), OsString::from(v)))
            .collect()
    }

    fn contains_key(env: &[CString], key: &str) -> bool {
        let needle = key.as_bytes();
        env.iter().any(|kv| {
            let bytes = kv.to_bytes();
            bytes.starts_with(needle) && bytes.get(needle.len()) == Some(&b'=')
        })
    }

    #[test]
    fn sanitize_keeps_allowlisted() {
        let out = sanitize_env(env(&[
            ("PATH", "/usr/bin"),
            ("XDG_RUNTIME_DIR", "/run/user/1000"),
            ("LANG", "en_US.UTF-8"),
            ("HOME", "/home/alice"),
            ("USER", "alice"),
            ("LOGNAME", "alice"),
            ("SHELL", "/bin/bash"),
        ]));
        for key in [
            "PATH",
            "XDG_RUNTIME_DIR",
            "LANG",
            "HOME",
            "USER",
            "LOGNAME",
            "SHELL",
        ] {
            assert!(contains_key(&out, key), "missing {key} in {out:?}");
        }
    }

    #[test]
    fn sanitize_strips_ld_preload() {
        let out = sanitize_env(env(&[("LD_PRELOAD", "/tmp/evil.so")]));
        assert!(out.is_empty(), "LD_PRELOAD must be stripped, got: {out:?}");
    }

    #[test]
    fn sanitize_strips_ld_library_path() {
        let out = sanitize_env(env(&[("LD_LIBRARY_PATH", "/tmp/evil")]));
        assert!(
            out.is_empty(),
            "LD_LIBRARY_PATH must be stripped, got: {out:?}"
        );
    }

    #[test]
    fn sanitize_strips_malloc_conf() {
        let out = sanitize_env(env(&[("MALLOC_CONF", "junk:true")]));
        assert!(out.is_empty(), "MALLOC_CONF must be stripped, got: {out:?}");
    }

    #[test]
    fn sanitize_strips_random_keys() {
        let out = sanitize_env(env(&[
            ("RUST_LOG", "debug"),
            ("EDITOR", "vim"),
            ("DBUS_SESSION_BUS_ADDRESS", "unix:path=/tmp/..."),
        ]));
        assert!(
            out.is_empty(),
            "non-allowlisted keys must be stripped: {out:?}"
        );
    }

    #[test]
    fn sanitize_keeps_lc_prefix() {
        let out = sanitize_env(env(&[
            ("LC_TIME", "C"),
            ("LC_MESSAGES", "en_US.UTF-8"),
            ("LC_ALL", "C.UTF-8"),
        ]));
        assert!(contains_key(&out, "LC_TIME"));
        assert!(contains_key(&out, "LC_MESSAGES"));
        assert!(contains_key(&out, "LC_ALL"));
    }

    #[test]
    fn sanitize_formats_as_key_equals_value() {
        let out = sanitize_env(env(&[("PATH", "/usr/bin:/bin")]));
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].to_bytes(), b"PATH=/usr/bin:/bin");
    }
}
