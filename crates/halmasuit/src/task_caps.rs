//! Post-drop audit of every thread's credentials.
//!
//! Capabilities, the bounding set, and securebits are per-thread
//! attributes (capabilities(7)); only `setgroups`, `setresgid`, and
//! `setresuid` reach every thread of the process (glibc's setxid
//! broadcast). A thread alive at `drop_privileges` time therefore keeps
//! whatever bounding set it was created with. `main` empties the
//! bounding set before the first thread exists so every later thread
//! inherits an empty one; this module confirms the result from
//! `/proc/self/task/*/status` so a regression fails the drop instead of
//! silently leaving a thread able to gain capabilities through `execve`
//! (ARCHITECTURE.md threat model row 12). The same audit confirms the
//! uid and gid components and that no supplementary group is held.

use std::io;

/// `CAP_KILL` = 5 per `uapi/linux/capability.h`.
pub const CAP_KILL_MASK: u64 = 1 << 5;

/// The credential fields of one `/proc/<pid>/task/<tid>/status`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskCreds {
    /// Real, effective, saved, and filesystem uid.
    pub uids: [u32; 4],
    /// Real, effective, saved, and filesystem gid.
    pub gids: [u32; 4],
    /// Supplementary gids (the `Groups:` line).
    pub groups: Vec<u32>,
    pub cap_inh: u64,
    pub cap_prm: u64,
    pub cap_eff: u64,
    pub cap_bnd: u64,
    pub cap_amb: u64,
}

/// Parse a `Uid:` or `Gid:` value: exactly `N` space-separated ids.
fn id_slots<const N: usize>(name: &str, raw: &str) -> Result<[u32; N], String> {
    let parsed: Vec<u32> = raw
        .split_whitespace()
        .map(str::parse)
        .collect::<Result<_, _>>()
        .map_err(|e| format!("{name}: {raw:?}: {e}"))?;
    let len = parsed.len();
    parsed
        .try_into()
        .map_err(|_| format!("{name}: expected {N} values, got {len}"))
}

/// Parse the `Uid:`, `Gid:`, `Groups:`, and `Cap*:` lines of a status
/// file.
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
    let uids: [u32; 4] = id_slots("Uid", field("Uid")?)?;
    let gids: [u32; 4] = id_slots("Gid", field("Gid")?)?;
    let groups_raw = field("Groups")?;
    let groups: Vec<u32> = groups_raw
        .split_whitespace()
        .map(str::parse)
        .collect::<Result<_, _>>()
        .map_err(|e| format!("Groups: {groups_raw:?}: {e}"))?;
    Ok(TaskCreds {
        uids,
        gids,
        groups,
        cap_inh: cap("CapInh")?,
        cap_prm: cap("CapPrm")?,
        cap_eff: cap("CapEff")?,
        cap_bnd: cap("CapBnd")?,
        cap_amb: cap("CapAmb")?,
    })
}

/// Check one thread against the post-drop invariant: every uid
/// component is `uid`, every gid component is the matching entry of
/// `gids`, no supplementary group is held, the bounding, inheritable,
/// and ambient sets are empty, and permitted/effective hold nothing
/// beyond `CAP_KILL`.
///
/// # Errors
/// A description of every violated clause.
pub fn check(creds: &TaskCreds, uid: u32, gids: [u32; 4]) -> Result<(), String> {
    let mut violations = Vec::new();
    if creds.uids != [uid; 4] {
        violations.push(format!("Uid {:?} != {uid}", creds.uids));
    }
    if creds.gids != gids {
        violations.push(format!("Gid {:?} != {gids:?}", creds.gids));
    }
    if !creds.groups.is_empty() {
        violations.push(format!("Groups {:?} != []", creds.groups));
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
pub fn audit_all_tasks(uid: u32, gids: [u32; 4]) -> io::Result<()> {
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
        if let Err(v) = check(&creds, uid, gids) {
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

    /// `groups` is the kernel's `Groups:` field verbatim: space-separated
    /// gids, possibly empty. `gid` is the `Gid:` field verbatim: real,
    /// effective, saved, and filesystem gid.
    fn status(uid: &str, gid: &str, groups: &str, prm: &str, eff: &str, bnd: &str) -> String {
        format!(
            "Name:\thalmasuit\nUid:\t{uid}\nGid:\t{gid}\n\
             Groups:\t{groups}\n\
             CapInh:\t0000000000000000\nCapPrm:\t{prm}\nCapEff:\t{eff}\n\
             CapBnd:\t{bnd}\nCapAmb:\t0000000000000000\n"
        )
    }

    const KILL: &str = "0000000000000020";
    const NONE: &str = "0000000000000000";
    const FULL: &str = "000001ffffffffff";
    const UID: &str = "998\t998\t998\t998";
    /// Compositor gid 998 as real/effective/fs, the `input` gid 990 as the
    /// saved set-gid: the post-drop shape with `HALMASUIT_INPUT_GID` set.
    const GID: &str = "998\t998\t990\t998";
    const EXPECTED_GIDS: [u32; 4] = [998, 998, 990, 998];

    #[test]
    fn parses_uid_gid_group_and_capability_lines() {
        let creds = parse_status(&status(UID, GID, "", KILL, KILL, NONE)).unwrap();
        assert_eq!(
            creds,
            TaskCreds {
                uids: [998; 4],
                gids: [998, 998, 990, 998],
                groups: Vec::new(),
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
        let creds = parse_status(&status(UID, GID, "", KILL, KILL, NONE)).unwrap();
        assert_eq!(check(&creds, 998, EXPECTED_GIDS), Ok(()));
    }

    #[test]
    fn worker_thread_with_no_capabilities_passes() {
        let creds = parse_status(&status(UID, GID, "", NONE, NONE, NONE)).unwrap();
        assert_eq!(check(&creds, 998, EXPECTED_GIDS), Ok(()));
    }

    /// No `HALMASUIT_INPUT_GID`: the saved gid is the compositor gid.
    #[test]
    fn saved_gid_equal_to_compositor_gid_passes_without_input_group() {
        let creds = parse_status(&status(UID, "998\t998\t998\t998", "", NONE, NONE, NONE)).unwrap();
        assert_eq!(check(&creds, 998, [998; 4]), Ok(()));
    }

    /// Supplementary `shadow` (gid 42) inherited from the unit would let
    /// the compositor read /etc/shadow.
    #[test]
    fn supplementary_group_shadow_fails() {
        let creds = parse_status(&status(UID, GID, "42", NONE, NONE, NONE)).unwrap();
        let err = check(&creds, 998, EXPECTED_GIDS).unwrap_err();
        assert!(err.contains("Groups"), "{err}");
    }

    #[test]
    fn supplementary_input_group_fails() {
        // The input gid must be the saved set-gid, never a supplementary
        // group: an exec'd child would inherit it.
        let creds = parse_status(&status(UID, GID, "990", NONE, NONE, NONE)).unwrap();
        let err = check(&creds, 998, EXPECTED_GIDS).unwrap_err();
        assert!(err.contains("Groups"), "{err}");
    }

    #[test]
    fn each_wrong_gid_slot_fails() {
        let slots: [(&str, [u32; 4]); 4] = [
            ("real", [0, 998, 990, 998]),
            ("effective", [998, 0, 990, 998]),
            ("saved", [998, 998, 0, 998]),
            ("fs", [998, 998, 990, 0]),
        ];
        for (name, gids) in slots {
            let text = status(
                UID,
                &format!("{}\t{}\t{}\t{}", gids[0], gids[1], gids[2], gids[3]),
                "",
                NONE,
                NONE,
                NONE,
            );
            let creds = parse_status(&text).unwrap();
            let err = check(&creds, 998, EXPECTED_GIDS).unwrap_err();
            assert!(err.contains("Gid"), "{name} slot: {err}");
        }
    }

    /// The bug this module exists for: a thread spawned before the drop
    /// keeps a full bounding set after `setresuid`.
    #[test]
    fn full_bounding_set_on_any_thread_fails() {
        let creds = parse_status(&status(UID, GID, "", NONE, NONE, FULL)).unwrap();
        let err = check(&creds, 998, EXPECTED_GIDS).unwrap_err();
        assert!(err.contains("CapBnd"), "{err}");
    }

    #[test]
    fn capability_beyond_cap_kill_fails() {
        // CAP_KILL plus CAP_SYS_ADMIN (21).
        let creds = parse_status(&status(UID, GID, "", "0000000000200020", KILL, NONE)).unwrap();
        let err = check(&creds, 998, EXPECTED_GIDS).unwrap_err();
        assert!(err.contains("CapPrm"), "{err}");
    }

    #[test]
    fn uid_component_still_root_fails() {
        let creds = parse_status(&status("998\t998\t0\t998", GID, "", KILL, KILL, NONE)).unwrap();
        let err = check(&creds, 998, EXPECTED_GIDS).unwrap_err();
        assert!(err.contains("Uid"), "{err}");
    }

    #[test]
    fn missing_gid_line_is_a_parse_error() {
        let text = "Uid:\t998\t998\t998\t998\nGroups:\t\nCapInh:\t0\nCapPrm:\t0\n\
                    CapEff:\t0\nCapBnd:\t0\nCapAmb:\t0\n";
        let err = parse_status(text).unwrap_err();
        assert!(err.contains("Gid"), "{err}");
    }

    #[test]
    fn gid_line_with_three_values_is_a_parse_error() {
        let err = parse_status(&status(UID, "998\t998\t990", "", NONE, NONE, NONE)).unwrap_err();
        assert!(err.contains("Gid"), "{err}");
    }

    #[test]
    fn non_numeric_gid_is_a_parse_error() {
        let err =
            parse_status(&status(UID, "998\t998\tinput\t998", "", NONE, NONE, NONE)).unwrap_err();
        assert!(err.contains("Gid"), "{err}");
    }

    #[test]
    fn missing_groups_line_is_a_parse_error() {
        let text = "Uid:\t998\t998\t998\t998\nGid:\t998\t998\t998\t998\nCapInh:\t0\n\
                    CapPrm:\t0\nCapEff:\t0\nCapBnd:\t0\nCapAmb:\t0\n";
        let err = parse_status(text).unwrap_err();
        assert!(err.contains("Groups"), "{err}");
    }

    #[test]
    fn missing_capability_line_is_a_parse_error() {
        let text = "Uid:\t998\t998\t998\t998\nGid:\t998\t998\t990\t998\nGroups:\t\n\
                    CapPrm:\t0000000000000020\n";
        let err = parse_status(text).unwrap_err();
        assert!(err.contains("CapInh"), "{err}");
    }
}
