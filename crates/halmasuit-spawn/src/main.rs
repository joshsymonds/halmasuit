// halmasuit-spawn — setuid-root privilege-drop helper.
//
// This binary is the only privileged code path in halmasuit. It must stay
// microscopic and audited. The unsafe-code forbid is non-negotiable.
//
// v1 placeholder. v2 implements:
//   setresgid(target_gid, target_gid, target_gid)
//   setgroups(supplementary_groups)
//   setresuid(target_uid, target_uid, target_uid)
//   prctl(PR_SET_NO_NEW_PRIVS)
//   execve(cmd, sanitized_argv, sanitized_envp)
// with no intervening user-controlled-state syscalls.

#![forbid(unsafe_code)]

fn main() {
    unimplemented!("v2: setuid privilege-drop helper");
}
