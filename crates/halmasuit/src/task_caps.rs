//! Post-drop audit of every thread's credentials.
//!
//! Capabilities, the bounding set, and securebits are per-thread
//! attributes (capabilities(7)); only `setresuid`/`setresgid` reach
//! every thread of the process (glibc's setxid broadcast). A thread
//! alive at `drop_privileges` time therefore keeps whatever bounding
//! set it was created with. `main` empties the bounding set before the
//! first thread exists so every later thread inherits an empty one;
//! this module confirms the result from `/proc/self/task/*/status` so a
//! regression fails the drop instead of silently leaving a thread able
//! to gain capabilities through `execve` (ARCHITECTURE.md threat model
//! row 12).

use std::io;

/// `CAP_KILL` = 5 per `uapi/linux/capability.h`.
pub const CAP_KILL_MASK: u64 = 1 << 5;

/// The credential fields of one `/proc/<pid>/task/<tid>/status`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskCreds {
    /// Real, effective, saved, and filesystem uid.
    pub uids: [u32; 4],
    pub cap_inh: u64,
    pub cap_prm: u64,
    pub cap_eff: u64,
    pub cap_bnd: u64,
    pub cap_amb: u64,
}

/// Parse the `Uid:` and `Cap*:` lines of a status file.
///
/// # Errors
/// A required line is missing or malformed.
pub fn parse_status(text: &str) -> Result<TaskCreds, String> {
    let field = |name: &str| {
        text.lines()
            .find_map(|line| line.strip_prefix(name)?.strip_prefix(':'))
            .map(str::trim)
            .ok_or_else(|| format!("missing {name}: line"))
    };
    let cap = |name: &str| {
        let raw = field(name)?;
        u64::from_str_radix(raw, 16).map_err(|e| format!("{name}: {raw:?}: {e}"))
    };
    let uid_raw = field("Uid")?;
    let parsed: Vec<u32> = uid_raw
        .split_whitespace()
        .map(str::parse)
        .collect::<Result<_, _>>()
        .map_err(|e| format!("Uid: {uid_raw:?}: {e}"))?;
    let uids: [u32; 4] = parsed
        .try_into()
        .map_err(|v: Vec<u32>| format!("Uid: expected 4 values, got {}", v.len()))?;
    Ok(TaskCreds {
        uids,
        cap_inh: cap("CapInh")?,
        cap_prm: cap("CapPrm")?,
        cap_eff: cap("CapEff")?,
        cap_bnd: cap("CapBnd")?,
        cap_amb: cap("CapAmb")?,
    })
}

/// Check one thread against the post-drop invariant: every uid
/// component is `uid`, the bounding, inheritable, and ambient sets are
/// empty, and permitted/effective hold nothing beyond `CAP_KILL`.
///
/// # Errors
/// A description of every violated clause.
pub fn check(creds: &TaskCreds, uid: u32) -> Result<(), String> {
    let mut violations = Vec::new();
    if creds.uids != [uid; 4] {
        violations.push(format!("Uid {:?} != {uid}", creds.uids));
    }
    for (name, value) in [
        ("CapBnd", creds.cap_bnd),
        ("CapInh", creds.cap_inh),
        ("CapAmb", creds.cap_amb),
    ] {
        if value != 0 {
            violations.push(format!("{name} {value:016x} != 0"));
        }
    }
    for (name, value) in [("CapPrm", creds.cap_prm), ("CapEff", creds.cap_eff)] {
        if value & !CAP_KILL_MASK != 0 {
            violations.push(format!("{name} {value:016x} exceeds {{CAP_KILL}}"));
        }
    }
    if violations.is_empty() {
        Ok(())
    } else {
        Err(violations.join(", "))
    }
}

/// Audit every thread of this process against [`check`].
///
/// A thread that exits between the directory read and the status read
/// is skipped: it holds nothing any more.
///
/// # Errors
/// `/proc/self/task` is unreadable, a status file is malformed, or any
/// thread violates the invariant (all violations are reported).
pub fn audit_all_tasks(uid: u32) -> io::Result<()> {
    let mut violations = Vec::new();
    let mut audited = 0_usize;
    for entry in std::fs::read_dir("/proc/self/task")? {
        let entry = entry?;
        let tid = entry.file_name().to_string_lossy().into_owned();
        let text = match std::fs::read_to_string(entry.path().join("status")) {
            Ok(text) => text,
            Err(e) if e.kind() == io::ErrorKind::NotFound => continue,
            Err(e) => return Err(e),
        };
        let creds =
            parse_status(&text).map_err(|e| io::Error::other(format!("task {tid} status: {e}")))?;
        audited += 1;
        if let Err(v) = check(&creds, uid) {
            violations.push(format!("task {tid}: {v}"));
        }
    }
    if audited == 0 {
        return Err(io::Error::other("/proc/self/task listed no readable task"));
    }
    if violations.is_empty() {
        Ok(())
    } else {
        Err(io::Error::other(violations.join("; ")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn status(uid: &str, prm: &str, eff: &str, bnd: &str) -> String {
        format!(
            "Name:\thalmasuit\nUid:\t{uid}\nGid:\t999\t999\t999\t999\n\
             CapInh:\t0000000000000000\nCapPrm:\t{prm}\nCapEff:\t{eff}\n\
             CapBnd:\t{bnd}\nCapAmb:\t0000000000000000\n"
        )
    }

    const KILL: &str = "0000000000000020";
    const NONE: &str = "0000000000000000";
    const FULL: &str = "000001ffffffffff";

    #[test]
    fn parses_uid_and_capability_lines() {
        let creds = parse_status(&status("998\t998\t998\t998", KILL, KILL, NONE)).unwrap();
        assert_eq!(
            creds,
            TaskCreds {
                uids: [998; 4],
                cap_inh: 0,
                cap_prm: CAP_KILL_MASK,
                cap_eff: CAP_KILL_MASK,
                cap_bnd: 0,
                cap_amb: 0,
            }
        );
    }

    #[test]
    fn main_thread_with_cap_kill_passes() {
        let creds = parse_status(&status("998\t998\t998\t998", KILL, KILL, NONE)).unwrap();
        assert_eq!(check(&creds, 998), Ok(()));
    }

    #[test]
    fn worker_thread_with_no_capabilities_passes() {
        let creds = parse_status(&status("998\t998\t998\t998", NONE, NONE, NONE)).unwrap();
        assert_eq!(check(&creds, 998), Ok(()));
    }

    /// The bug this module exists for: a thread spawned before the drop
    /// keeps a full bounding set after `setresuid`.
    #[test]
    fn full_bounding_set_on_any_thread_fails() {
        let creds = parse_status(&status("998\t998\t998\t998", NONE, NONE, FULL)).unwrap();
        let err = check(&creds, 998).unwrap_err();
        assert!(err.contains("CapBnd"), "{err}");
    }

    #[test]
    fn capability_beyond_cap_kill_fails() {
        // CAP_KILL plus CAP_SYS_ADMIN (21).
        let creds = parse_status(&status(
            "998\t998\t998\t998",
            "0000000000200020",
            KILL,
            NONE,
        ))
        .unwrap();
        let err = check(&creds, 998).unwrap_err();
        assert!(err.contains("CapPrm"), "{err}");
    }

    #[test]
    fn uid_component_still_root_fails() {
        let creds = parse_status(&status("998\t998\t0\t998", KILL, KILL, NONE)).unwrap();
        let err = check(&creds, 998).unwrap_err();
        assert!(err.contains("Uid"), "{err}");
    }

    #[test]
    fn missing_capability_line_is_a_parse_error() {
        let text = "Uid:\t998\t998\t998\t998\nCapPrm:\t0000000000000020\n";
        let err = parse_status(text).unwrap_err();
        assert!(err.contains("CapInh"), "{err}");
    }
}
