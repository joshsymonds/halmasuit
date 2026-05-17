//! The socket-activated privileged broker binary (Epic R6).
//!
//! PID 1 (systemd) creates+binds+listens the `SOCK_SEQPACKET` socket
//! and passes it as `LISTEN_FDS` fd 3; this process never binds
//! anything itself. It then serially accepts ONE connection at a time
//! (Epic R5 global single slot) and runs the full one-handle lifecycle
//! per connection via `halmasuit_session::handle_connection`, with a
//! single long-lived `AuthSlot`. When the listening socket goes away
//! the process exits and the unit deactivates — there is NO standing
//! root process when idle (Epic R6).
//!
//! `#![forbid(unsafe_code)]`: pure composition. The inherited-fd
//! adoption (the only `unsafe` socket activation needs) is quarantined
//! behind `halmasuit_session::accept_seqpacket` in the crate's
//! `worker` module, the same `#[expect(unsafe_code, reason=…)]`
//! boundary as the rest of the crate's process control.
#![forbid(unsafe_code)]

use std::os::fd::RawFd;

use halmasuit_session::{AuthSlot, accept_seqpacket, handle_connection};

/// `SD_LISTEN_FDS_START` — systemd passes the first activation fd here.
const SD_LISTEN_FDS_START: RawFd = 3;

/// Why socket activation could not yield the listening fd.
#[derive(Debug, PartialEq, Eq)]
enum ActivationError {
    /// `LISTEN_PID`/`LISTEN_FDS` absent or non-numeric — not
    /// socket-activated.
    NotActivated,
    /// `LISTEN_PID` names another process: the fds are not ours
    /// (inherited past the intended target — fail closed, do NOT use
    /// them).
    WrongPid { listen_pid: i32, my_pid: i32 },
    /// We require EXACTLY one passed socket (the single broker
    /// listener); anything else is a misconfigured unit.
    UnexpectedCount(i32),
}

/// Resolve the activation listening fd from the environment.
///
/// `var` is the env accessor (injected so this is unit-testable
/// without mutating the real process environment); `my_pid` is
/// `getpid()`. Returns [`SD_LISTEN_FDS_START`] iff `LISTEN_PID ==
/// my_pid` and `LISTEN_FDS == 1`.
fn listen_fd(var: impl Fn(&str) -> Option<String>, my_pid: i32) -> Result<RawFd, ActivationError> {
    let pid: i32 = var("LISTEN_PID")
        .and_then(|s| s.parse().ok())
        .ok_or(ActivationError::NotActivated)?;
    let count: i32 = var("LISTEN_FDS")
        .and_then(|s| s.parse().ok())
        .ok_or(ActivationError::NotActivated)?;
    if pid != my_pid {
        return Err(ActivationError::WrongPid {
            listen_pid: pid,
            my_pid,
        });
    }
    if count != 1 {
        return Err(ActivationError::UnexpectedCount(count));
    }
    Ok(SD_LISTEN_FDS_START)
}

/// Greeter uid the SO_PEERCRED gate authorizes (Epic R5/R8). Same env
/// key the compositor module already sets.
fn greeter_uid(var: impl Fn(&str) -> Option<String>) -> Result<u32, String> {
    var("HALMASUIT_GREETER_UID")
        .ok_or_else(|| "HALMASUIT_GREETER_UID is unset".to_owned())?
        .parse::<u32>()
        .map_err(|e| format!("HALMASUIT_GREETER_UID is not a u32: {e}"))
}

fn main() -> std::process::ExitCode {
    // pid_t directly (i32) — matches systemd's LISTEN_PID; no lossy
    // u32→i32 cast.
    let my_pid = nix::unistd::getpid().as_raw();
    let lfd = match listen_fd(|k| std::env::var(k).ok(), my_pid) {
        Ok(fd) => fd,
        Err(e) => {
            eprintln!("halmasuit-session: not socket-activated: {e:?}");
            return std::process::ExitCode::FAILURE;
        }
    };
    let guid = match greeter_uid(|k| std::env::var(k).ok()) {
        Ok(u) => u,
        Err(e) => {
            eprintln!("halmasuit-session: {e}");
            return std::process::ExitCode::FAILURE;
        }
    };

    // ONE slot for the whole process lifetime (Epic R5 global single
    // slot). Serial accept: one connection's full lifecycle before the
    // next is accepted.
    let mut slot = AuthSlot::with_defaults(guid);
    loop {
        let chan = match accept_seqpacket(lfd) {
            Ok(c) => c,
            Err(e) => {
                eprintln!("halmasuit-session: accept failed, exiting: {e}");
                // Listener gone ⇒ unit deactivates; no standing root.
                return std::process::ExitCode::SUCCESS;
            }
        };
        match handle_connection(&mut slot, &chan) {
            Ok(disp) => eprintln!("halmasuit-session: connection ended: {disp:?}"),
            Err(e) => eprintln!("halmasuit-session: connection error: {e}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env_of<'a>(pairs: &'a [(&'a str, &'a str)]) -> impl Fn(&str) -> Option<String> + 'a {
        move |k| {
            pairs
                .iter()
                .find(|(kk, _)| *kk == k)
                .map(|(_, v)| (*v).to_owned())
        }
    }

    #[test]
    fn listen_fd_ok_when_pid_matches_and_one_fd() {
        let env = env_of(&[("LISTEN_PID", "42"), ("LISTEN_FDS", "1")]);
        assert_eq!(listen_fd(env, 42), Ok(SD_LISTEN_FDS_START));
    }

    #[test]
    fn listen_fd_not_activated_when_env_absent_or_garbage() {
        assert_eq!(
            listen_fd(env_of(&[]), 42),
            Err(ActivationError::NotActivated)
        );
        assert_eq!(
            listen_fd(env_of(&[("LISTEN_PID", "x"), ("LISTEN_FDS", "1")]), 42),
            Err(ActivationError::NotActivated)
        );
        assert_eq!(
            listen_fd(env_of(&[("LISTEN_PID", "42")]), 42),
            Err(ActivationError::NotActivated)
        );
    }

    #[test]
    fn listen_fd_wrong_pid_fails_closed() {
        let env = env_of(&[("LISTEN_PID", "999"), ("LISTEN_FDS", "1")]);
        assert_eq!(
            listen_fd(env, 42),
            Err(ActivationError::WrongPid {
                listen_pid: 999,
                my_pid: 42
            })
        );
    }

    #[test]
    fn listen_fd_rejects_not_exactly_one_socket() {
        let env = env_of(&[("LISTEN_PID", "42"), ("LISTEN_FDS", "2")]);
        assert_eq!(listen_fd(env, 42), Err(ActivationError::UnexpectedCount(2)));
        let env0 = env_of(&[("LISTEN_PID", "42"), ("LISTEN_FDS", "0")]);
        assert_eq!(
            listen_fd(env0, 42),
            Err(ActivationError::UnexpectedCount(0))
        );
    }

    #[test]
    fn greeter_uid_parsed_or_explained() {
        assert_eq!(
            greeter_uid(env_of(&[("HALMASUIT_GREETER_UID", "1000")])),
            Ok(1000)
        );
        assert!(greeter_uid(env_of(&[])).is_err());
        assert!(greeter_uid(env_of(&[("HALMASUIT_GREETER_UID", "-1")])).is_err());
    }
}
