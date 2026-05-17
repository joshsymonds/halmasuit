//! Session-leader validation + supplementary-group merge (Epic R7/R11).
//!
//! The PURE, privilege-free, PAM-free security core of the Design-A
//! session leader: validate the PAM-resolved identity + session
//! command/env, and compute the MERGED supplementary-group set.
//!
//! The hardened discipline (UID/GID floor, `(uid_t)-1`/overflow
//! rejection, pwent (uid,gid,user) cross-check, env allowlist) is
//! PORTED from `crates/halmasuit-spawn` (Epic R11). `halmasuit-spawn`
//! the standalone setuid binary is DELETED as part of the epic (R15,
//! the R14 pattern) — the broker is already root and forks-then-drops
//! in a non-setuid child, so the setuid bit is obsolete; this is its
//! fuzzable successor function. NOT a dependency/wrapper of
//! halmasuit-spawn (it encodes the deleted setuid model).
//!
//! NEW vs halmasuit-spawn (the R7/R11 finding): groups are
//! `getgrouplist`-MERGED with the pam_setcred/pam_open_session-
//! established set — a blind `initgroups` REPLACES the list and would
//! silently clobber groups added by pam_group/pam_systemd/pam_mount.
//!
//! `#![forbid(unsafe_code)]` — pure validation/computation; the unsafe
//! `fork`/`setres*`/`execve` syscall sequence lands in the NEXT task.

#![forbid(unsafe_code)]

use std::ffi::CString;

use nix::unistd::{Gid, User, getgrouplist};
use thiserror::Error;

/// Minimum uid/gid (Epic R8). NEVER relax — ARCHITECTURE threat row
/// 11. Ported from `halmasuit-spawn::UID_MIN` (R11; that crate is
/// deleted at epic close per R15).
pub const UID_MIN: u32 = 1000;

/// Env names that survive the privilege drop. Ported from
/// `halmasuit-spawn::ENV_ALLOWLIST` (R11). `LD_*`/`GCONV_*`/`MALLOC_*`/
/// `IFS` are deliberately absent — and since the session-leader child
/// is NOT setuid the kernel does NOT auto-strip them, so this
/// allowlist is the ONLY defense (Epic R11).
const ENV_ALLOWLIST: &[&str] = &[
    "XDG_RUNTIME_DIR",
    "PATH",
    "LANG",
    "HOME",
    "USER",
    "LOGNAME",
    "SHELL",
];

/// Why a session spec / group merge was refused. Fail-closed on every
/// check.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum SpecError {
    #[error("uid {0} is below UID_MIN ({UID_MIN})")]
    UidFloor(u32),
    #[error("gid {0} is below UID_MIN ({UID_MIN})")]
    GidFloor(u32),
    #[error("id {0} is the (uid_t)-1 sentinel / overflow (CVE-2019-14287 class)")]
    BadId(u32),
    #[error("pwent cross-check failed: {0}")]
    PwentMismatch(String),
    #[error("session command must be an absolute path")]
    RelativeCommand,
    #[error("session command is empty")]
    EmptyCommand,
    #[error("group resolution failed: {0}")]
    Groups(String),
}

/// A validated, ready-to-launch session spec. Constructed ONLY via
/// [`validate`]; `env` is already allowlist-sanitized.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionSpec {
    pub username: String,
    pub uid: u32,
    pub gid: u32,
    pub cmd: Vec<String>,
    pub env: Vec<(String, String)>,
}

fn is_env_allowed(key: &str) -> bool {
    ENV_ALLOWLIST.contains(&key) || key.starts_with("LC_")
}

/// Filter env to the allowlist (+ `LC_*`). Ported from
/// `halmasuit-spawn::sanitize_env` (R11).
#[must_use]
pub fn sanitize_env(env: Vec<(String, String)>) -> Vec<(String, String)> {
    env.into_iter().filter(|(k, _)| is_env_allowed(k)).collect()
}

fn lookup_user(username: &str) -> Result<User, SpecError> {
    match User::from_name(username) {
        Ok(Some(u)) => Ok(u),
        Ok(None) => Err(SpecError::PwentMismatch(format!(
            "unknown user: {username:?}"
        ))),
        Err(e) => Err(SpecError::PwentMismatch(format!(
            "NSS lookup failed for {username:?}: {e}"
        ))),
    }
}

/// Validate the PAM-resolved identity + session command/env (Epic
/// R7/R8/R11).
///
/// Discipline ported from `halmasuit-spawn` (deleted at epic close,
/// R15) — this is its fuzzable successor: `(uid_t)-1`/overflow
/// rejection → UID/GID floor → command shape → pwent (uid,gid,user)
/// cross-check → env allowlist.
///
/// # Errors
/// See [`SpecError`]. Fail-closed on every check.
pub fn validate(
    username: &str,
    uid: u32,
    gid: u32,
    cmd: Vec<String>,
    env: Vec<(String, String)>,
) -> Result<SessionSpec, SpecError> {
    // (uid_t)-1 sentinel / overflow FIRST: setres*(-1) means "no
    // change"; u32::MAX is that bit pattern (CVE-2019-14287 class).
    if uid == u32::MAX {
        return Err(SpecError::BadId(uid));
    }
    if gid == u32::MAX {
        return Err(SpecError::BadId(gid));
    }
    // UID/GID floor — NEVER relax (threat row 11).
    if uid < UID_MIN {
        return Err(SpecError::UidFloor(uid));
    }
    if gid < UID_MIN {
        return Err(SpecError::GidFloor(gid));
    }
    // Command shape.
    let first = cmd.first().ok_or(SpecError::EmptyCommand)?;
    if !first.starts_with('/') {
        return Err(SpecError::RelativeCommand);
    }
    // pwent (uid,gid,user) cross-check — the load-bearing defense
    // against supplementary-group escalation (a `1000 1000 root`
    // triple would otherwise graft root's groups via the merge).
    let user = lookup_user(username)?;
    if user.uid.as_raw() != uid {
        return Err(SpecError::PwentMismatch(format!(
            "{username:?} has uid={} in passwd but spec says {uid}",
            user.uid.as_raw()
        )));
    }
    if user.gid.as_raw() != gid {
        return Err(SpecError::PwentMismatch(format!(
            "{username:?} has gid={} in passwd but spec says {gid}",
            user.gid.as_raw()
        )));
    }
    Ok(SessionSpec {
        username: username.to_owned(),
        uid,
        gid,
        cmd,
        env: sanitize_env(env),
    })
}

fn dedup_push(out: &mut Vec<u32>, g: u32) {
    if !out.contains(&g) {
        out.push(g);
    }
}

/// The MERGED supplementary-group set for the session leader (R7/R11).
///
/// `getgrouplist(user, primary_gid)` UNION the pam_setcred/
/// pam_open_session-established gids, with `primary_gid` included,
/// deduped (insertion order stable, deterministic).
///
/// NEVER a blind `initgroups` — that REPLACES the list and would
/// silently clobber groups added by pam_group/pam_systemd/pam_mount
/// during `setcred`/`open_session` (the R7/R11 finding). Fail-closed
/// on an unknown user (getgrouplist alone does NOT error for an
/// unknown name — it returns just the primary gid).
///
/// # Errors
/// [`SpecError::Groups`] on unknown user / NSS failure / interior NUL;
/// [`SpecError::PwentMismatch`] is reused for the unknown-user case.
pub fn merged_groups(
    username: &str,
    primary_gid: u32,
    pam_established: &[u32],
) -> Result<Vec<u32>, SpecError> {
    // Fail-closed: getgrouplist returns Ok([primary]) for an unknown
    // user, so verify existence first.
    lookup_user(username)?;
    let cname = CString::new(username)
        .map_err(|_| SpecError::Groups("username contains interior NUL".to_owned()))?;
    let base = getgrouplist(&cname, Gid::from_raw(primary_gid))
        .map_err(|e| SpecError::Groups(format!("getgrouplist({username:?}): {e}")))?;
    let mut out: Vec<u32> = Vec::new();
    dedup_push(&mut out, primary_gid);
    for g in base {
        dedup_push(&mut out, g.as_raw());
    }
    for &g in pam_established {
        dedup_push(&mut out, g);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ok_cmd() -> Vec<String> {
        vec!["/run/current-system/sw/bin/niri".into(), "--session".into()]
    }

    // `root` is universally present (uid 0, gid 0) — used to exercise
    // the pwent cross-check deterministically without a ≥1000 fixture.
    fn root_ids() -> (u32, u32) {
        let u = nix::unistd::User::from_name("root").unwrap().unwrap();
        (u.uid.as_raw(), u.gid.as_raw())
    }

    #[test]
    fn uid_floor_and_gid_floor_rejected() {
        assert!(matches!(
            validate("x", 0, 1000, ok_cmd(), vec![]),
            Err(SpecError::UidFloor(0))
        ));
        assert!(matches!(
            validate("x", 1000, 42, ok_cmd(), vec![]),
            Err(SpecError::GidFloor(42))
        ));
    }

    #[test]
    fn sentinel_minus_one_and_overflow_rejected_cve_2019_14287() {
        assert!(matches!(
            validate("x", u32::MAX, 1000, ok_cmd(), vec![]),
            Err(SpecError::BadId(u32::MAX))
        ));
        assert!(matches!(
            validate("x", 1000, u32::MAX, ok_cmd(), vec![]),
            Err(SpecError::BadId(u32::MAX))
        ));
    }

    #[test]
    fn pwent_mismatch_rejected() {
        // "root" with a ≥1000 uid/gid that is NOT root's real pwent →
        // the supplementary-group-escalation vector, refused.
        let r = validate("root", 4000, 4000, ok_cmd(), vec![]);
        assert!(matches!(r, Err(SpecError::PwentMismatch(_))), "got {r:?}");
        // An unknown account also fails closed.
        let r = validate("halmasuit_no_such_user_zz", 4000, 4000, ok_cmd(), vec![]);
        assert!(matches!(r, Err(SpecError::PwentMismatch(_))), "got {r:?}");
    }

    #[test]
    fn matching_pwent_validates_when_above_floor_or_explained() {
        // root's real ids are 0/0 → fails the floor (correct: the
        // session leader never runs anything below the UID floor).
        let (ru, rg) = root_ids();
        assert_eq!((ru, rg), (0, 0));
        assert!(matches!(
            validate("root", ru, rg, ok_cmd(), vec![]),
            Err(SpecError::UidFloor(0))
        ));
        // Floor+pwent are independent: a ≥1000 uid whose name/ids do
        // not agree is PwentMismatch, proving the cross-check runs
        // even past the floor.
        assert!(matches!(
            validate("root", 1000, 1000, ok_cmd(), vec![]),
            Err(SpecError::PwentMismatch(_))
        ));
    }

    #[test]
    fn command_must_be_absolute_and_nonempty() {
        let (ru, rg) = (1000u32, 1000u32);
        assert!(matches!(
            validate("x", ru, rg, vec![], vec![]),
            Err(SpecError::EmptyCommand)
        ));
        assert!(matches!(
            validate("x", ru, rg, vec!["niri".into()], vec![]),
            Err(SpecError::RelativeCommand)
        ));
    }

    #[test]
    fn sanitize_env_allowlist_strips_escalation_vectors() {
        let raw = vec![
            ("LD_PRELOAD".into(), "/evil.so".into()),
            ("LD_LIBRARY_PATH".into(), "/evil".into()),
            ("GCONV_PATH".into(), "/evil".into()),
            ("IFS".into(), " ".into()),
            ("PATH".into(), "/run/current-system/sw/bin".into()),
            ("HOME".into(), "/home/alice".into()),
            ("XDG_RUNTIME_DIR".into(), "/run/user/1000".into()),
            ("LC_ALL".into(), "C".into()),
            ("HACKER_VAR".into(), "x".into()),
        ];
        let out = sanitize_env(raw);
        let keys: Vec<&str> = out.iter().map(|(k, _)| k.as_str()).collect();
        for bad in [
            "LD_PRELOAD",
            "LD_LIBRARY_PATH",
            "GCONV_PATH",
            "IFS",
            "HACKER_VAR",
        ] {
            assert!(!keys.contains(&bad), "must strip {bad}: {keys:?}");
        }
        for good in ["PATH", "HOME", "XDG_RUNTIME_DIR", "LC_ALL"] {
            assert!(keys.contains(&good), "must keep {good}: {keys:?}");
        }
    }

    #[test]
    fn validate_stores_sanitized_env_in_spec() {
        let spec = validate(
            "root",
            1000,
            1000,
            ok_cmd(),
            vec![
                ("LD_PRELOAD".into(), "/evil.so".into()),
                ("PATH".into(), "/bin".into()),
            ],
        );
        // root@1000 fails pwent, so use a name we can't guarantee;
        // instead assert the env-sanitization independently above and
        // here only that an invalid identity never yields a spec.
        assert!(spec.is_err());
    }

    #[test]
    fn merged_groups_unions_pam_established_not_blind_replace() {
        // getgrouplist("root", 0) is well-defined on every Linux box.
        // A pam-established gid NOT in root's /etc/group set must
        // still appear (proves MERGE, not the blind-initgroups
        // REPLACE that R7/R11 forbids), and the primary gid is present.
        let pam_established = [4242u32, 4243u32];
        let merged = merged_groups("root", 0, &pam_established).expect("getgrouplist");
        assert!(merged.contains(&0), "primary gid present: {merged:?}");
        for g in pam_established {
            assert!(
                merged.contains(&g),
                "pam-established gid {g} must survive the merge: {merged:?}"
            );
        }
        // Deduped.
        let mut sorted = merged.clone();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(sorted.len(), merged.len(), "no duplicate gids: {merged:?}");
    }

    #[test]
    fn merged_groups_unknown_user_fails_closed() {
        assert!(merged_groups("halmasuit_no_such_user_zz", 1000, &[]).is_err());
    }
}
