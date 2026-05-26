//! `BrokerEpisode` — the compositor's per-greeter-connection episode
//! object (Epic R3 / Amendments A4/A5/A6/A7).
//!
//! This is `halmasuit-pam`'s successor on the live path: instead of
//! running PAM in-process, the compositor relays the greetd auth
//! conversation to the privileged `halmasuit-session` broker over a
//! `SOCK_SEQPACKET` channel speaking the frozen `halmasuit-session-ipc`
//! contract. The pure translation brain is
//! [`crate::broker_relay::BrokerRelay`] (#27); the sans-IO greetd
//! protocol machine is `halmasuit_greetd::server::Connection` (#30a);
//! this file binds them to the broker channel.
//!
//! # Single owner, sans-IO (A6 + A7)
//!
//! [`BrokerEpisode`] OWNS the [`SeqpacketChannel`] for the whole
//! episode (A6 — no dup/Rc/Arc; the auth state never owns the fd). The
//! drive methods perform NO blocking I/O (A7): every channel `send`/
//! `recv` is `MSG_DONTWAIT`. The calloop layer multiplexes the greeter
//! fd and the broker fd as two non-blocking sources and calls
//! [`BrokerEpisode::on_greeter_bytes`] / [`BrokerEpisode::on_broker_readable`]
//! on readiness; the compositor render/calloop thread never blocks on
//! a privileged-peer round-trip. Broker death is a readable→EOF event
//! ([`WireError::Closed`]) → greetd fail-closed auth failure (A7.4).
//!
//! The compositor links no libpam and never depends on the
//! `halmasuit-session` crate (R2/R3/R14); only the pure
//! `halmasuit-session-ipc` codec is shared (the SEQPACKET syscall
//! wrapper is reimplemented locally).

use std::fmt;
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, FromRawFd, OwnedFd, RawFd};
use std::path::Path;

use halmasuit_greetd::server::{Connection, Demand, SpawnRequest};
use halmasuit_session_ipc::{
    BrokerToCompositor, CodecError, CompositorToBroker, MAX_MESSAGE_SIZE, SessionOutcome, encode,
    try_decode,
};
use nix::sys::socket::{ControlMessageOwned, MsgFlags, recvmsg, send};

use crate::broker_relay::{BrokerRelay, RelayError, RelayEvent};

/// System-controlled PATH used for resolving relative session
/// commands sent by the greeter (R8 PATH-resolution hardening).
///
/// Greeters like DMS DankGreeter read `.desktop` files whose `Exec=`
/// lines are often plain command names (`niri-session`) — the XDG
/// Desktop Entry spec allows both relative and absolute Exec entries
/// and the greeter forwards what it found verbatim. The broker's
/// `halmasuit-session::session_leader::SessionSpec::validate` REFUSES
/// relative commands (CVE-2019-style argv-injection / PATH-attack
/// hardening — the privileged process must never PATH-resolve from a
/// peer-controlled string).
///
/// halmasuit-greetd's relay runs in the UNPRIVILEGED compositor's
/// address space, so PATH resolution here is not a privilege boundary
/// — we resolve against this fixed system path (NixOS + FHS
/// conventions), substitute the absolute path into the SpawnRequest,
/// then hand the broker the absolute-path form it requires. If the
/// command is already absolute, it passes through untouched. If
/// resolution fails, the episode fails closed.
///
/// The PATH is HARDCODED, not read from `$PATH` — the greeter never
/// controls it. This is the same posture used by the OpenSSH server,
/// systemd-pam, and other privilege-boundary daemons.
///
/// **Ordering is security-significant: first hit wins.** Earlier
/// directories shadow later ones, so `/run/wrappers/bin` (NixOS
/// setuid wrappers) takes precedence over `/run/current-system/sw/bin`
/// (the activation-package profile), which takes precedence over the
/// FHS dirs. This matches how `execvp(3)` walks a PATH, and matches
/// what a NixOS user expects (wrappers are the system-blessed
/// versions).
const SYSTEM_PATH: &[&str] = &[
    "/run/wrappers/bin",
    "/run/current-system/sw/bin",
    "/usr/local/bin",
    "/usr/bin",
    "/bin",
];

/// Resolve `cmd[0]` against [`SYSTEM_PATH`] if it does not start
/// with `/`. Returns the resolved command vector with `cmd[0]`
/// substituted, or `None` for any input the broker would not accept:
///
/// - empty vector (broker rejects with `SpecError::EmptyCommand`),
/// - `cmd[0]` containing an interior `\0` (broker would reject at
///   `CString::new` in the worker; we fail closed earlier),
/// - `cmd[0]` containing `/` but not absolute (path-traversal-shaped
///   strings — greeter has no business sending these),
/// - `cmd[0]` plain name but not found on any [`SYSTEM_PATH`] entry.
///
/// Absolute commands pass through unchanged.
///
/// **Thread:** runs on the compositor's calloop thread inside the
/// `Demand::Spawn` handler, performing up to `SYSTEM_PATH.len()`
/// synchronous `stat(2)` syscalls (`Path::is_file`). This is
/// intentional — the alternative (suspend `Demand::Spawn` for an
/// async resolve) would add greetd-state-machine surface to spare a
/// handful of local-FS stats that fire **once per login transition**,
/// not per frame or input event. The A7 rule ("the compositor never
/// blocks the render/calloop thread on broker IPC") is specifically
/// about broker IPC; bounded local-FS stats are explicitly fine.
#[must_use]
pub fn resolve_command_path(cmd: Vec<String>) -> Option<Vec<String>> {
    let first = cmd.first()?;
    // Reject interior NUL bytes explicitly. The broker would reject
    // these too (worker's `CString::new` returns NulError) but
    // catching them here means a future refactor that swaps the
    // `is_file` lookup for an in-memory cache can't silently regress
    // NUL-rejection.
    if first.contains('\0') {
        return None;
    }
    if first.starts_with('/') {
        return Some(cmd);
    }
    // Reject `cmd[0]` containing path separators that aren't an
    // absolute path (e.g. `../niri`): these are explicit relative
    // path traversals the greeter has no business sending.
    if first.contains('/') {
        return None;
    }
    for dir in SYSTEM_PATH {
        let candidate = std::path::PathBuf::from(dir).join(first);
        if candidate.is_file() {
            let mut resolved = cmd;
            resolved[0] = candidate.to_string_lossy().into_owned();
            return Some(resolved);
        }
    }
    None
}

/// SEQPACKET framing error. Internal — every variant is turned into a
/// fail-closed greeter auth failure by the episode; it is never
/// surfaced to the greeter as anything but an auth error.
#[derive(Debug)]
pub enum WireError {
    /// `send`/`recv` syscall failed.
    Io(nix::Error),
    /// Body failed framing/JSON decode (incl. an oversized prefix).
    Codec(CodecError),
    /// Peer closed the socket (zero-length read).
    Closed,
    /// A datagram arrived that was not exactly one framed message. On
    /// SEQPACKET there is no "more to come" — this is malformed.
    Malformed,
}

impl fmt::Display for WireError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(e) => write!(f, "broker channel syscall failed: {e}"),
            Self::Codec(e) => write!(f, "broker channel codec error: {e}"),
            Self::Closed => f.write_str("broker closed the connection"),
            Self::Malformed => f.write_str("broker sent a malformed datagram"),
        }
    }
}

impl std::error::Error for WireError {}

impl From<nix::Error> for WireError {
    fn from(e: nix::Error) -> Self {
        Self::Io(e)
    }
}

impl From<CodecError> for WireError {
    fn from(e: CodecError) -> Self {
        Self::Codec(e)
    }
}

/// One end of a connected `SOCK_SEQPACKET` socket carrying
/// `halmasuit-session-ipc` messages, one datagram per logical message.
///
/// The codec ([`encode`]/[`try_decode`]) is shared with the broker via
/// the pure contract crate; only this thin syscall wrapper is local
/// (the compositor must not depend on the libpam-linking
/// `halmasuit-session` crate — R14). Both `send` and `recv` are
/// `MSG_DONTWAIT` (A7: the compositor never blocks on broker IPC).
pub struct SeqpacketChannel {
    fd: OwnedFd,
    /// Reusable receive scratch, allocated ONCE at the max framed size
    /// and reused across every `recv_with_fd` — which runs on the
    /// compositor render/calloop thread on EVERY broker-source
    /// readiness. Avoids a ~1 MiB `vec![0u8; …]` alloc+memset per
    /// datagram on the load-bearing no-flash thread. The episode owns
    /// exactly one channel for its whole lifetime (A6/A8 single owner)
    /// ⇒ never aliased; `RefCell` keeps `recv_with_fd`'s `&self`
    /// signature so the calloop borrow shape is unchanged.
    recv_buf: std::cell::RefCell<Vec<u8>>,
}

impl SeqpacketChannel {
    #[must_use]
    pub fn new(fd: OwnedFd) -> Self {
        Self {
            fd,
            recv_buf: std::cell::RefCell::new(vec![
                0u8;
                std::mem::size_of::<u32>()
                    + MAX_MESSAGE_SIZE as usize
            ]),
        }
    }

    /// Encode `msg` and write it as exactly one datagram, non-blocking.
    ///
    /// # Errors
    /// [`WireError::Codec`] on encode overflow; [`WireError::Io`] on
    /// `send` (incl. `EAGAIN` — a wedged privileged peer is fail-closed,
    /// A7.4); [`WireError::Malformed`] if the kernel accepted only part
    /// of the datagram (a SEQPACKET message cannot be continued).
    pub fn send(&self, msg: &CompositorToBroker) -> Result<(), WireError> {
        let bytes = encode(msg)?;
        let n = send(self.fd.as_raw_fd(), &bytes, MsgFlags::MSG_DONTWAIT)?;
        if n == bytes.len() {
            Ok(())
        } else {
            Err(WireError::Malformed)
        }
    }

    /// Read at most one datagram, decode it, and return any ONE
    /// `SCM_RIGHTS` fd the broker attached (the Amendment-A5.6
    /// poll-only leader pidfd, sent with `SessionOpened`).
    /// Non-blocking — always `recvmsg`-with-cmsg-space (never a bare
    /// `recv`, which would truncate the ancillary data, set
    /// `MSG_CTRUNC`, and leak the kernel-dup'd fd).
    ///
    /// `Ok(None)` means "no datagram ready" (`EAGAIN`/`EWOULDBLOCK`) —
    /// a spurious calloop wakeup; the caller does nothing. The fd is a
    /// fresh [`OwnedFd`] kernel-dup'd into this process; the compositor
    /// treats it STRICTLY poll-only (never
    /// waitid/reap/pidfd_send_signal — the broker is the sole reaper,
    /// R9/A5).
    ///
    /// Fail-closed: received fds are adopted into `OwnedFd` BEFORE the
    /// fallible decode so no error path leaks a privilege-crossing
    /// kernel fd; >1 fd (a protocol the broker never speaks) closes
    /// them all and errors.
    ///
    /// # Errors
    /// [`WireError::Closed`] on peer hangup; [`WireError::Codec`] on a
    /// bad/oversized body; [`WireError::Malformed`] if the datagram is
    /// not exactly one complete message or carried >1 fd;
    /// [`WireError::Io`] on a `recvmsg` error other than would-block.
    pub fn recv_with_fd(&self) -> Result<Option<(BrokerToCompositor, Option<OwnedFd>)>, WireError> {
        // Reuse the per-channel scratch (episode owns one channel for
        // its lifetime, A6/A8 ⇒ never aliased) — no ~1 MiB alloc+memset
        // per datagram on the compositor render/calloop thread.
        let mut buf = self.recv_buf.borrow_mut();
        let mut iov = [std::io::IoSliceMut::new(&mut buf)];
        let mut cmsg = nix::cmsg_space!(RawFd);
        let r = match recvmsg::<()>(
            self.fd.as_raw_fd(),
            &mut iov,
            Some(&mut cmsg),
            MsgFlags::MSG_DONTWAIT,
        ) {
            Ok(r) => r,
            Err(nix::errno::Errno::EAGAIN) => return Ok(None),
            Err(e) => return Err(WireError::Io(e)),
        };
        // Adopt every received fd into an OwnedFd IMMEDIATELY (before
        // the fallible decode) so no error path leaks a kernel fd.
        let mut fds: Vec<OwnedFd> = Vec::new();
        for c in r.cmsgs().map_err(WireError::Io)? {
            if let ControlMessageOwned::ScmRights(raws) = c {
                for raw in raws {
                    // SAFETY: `raw` was just produced by
                    // recvmsg(SCM_RIGHTS) in THIS process; nothing else
                    // owns it. Sole ownership → closed on drop on every
                    // path (no privilege-crossing-fd leak).
                    #[expect(
                        unsafe_code,
                        reason = "adopt a kernel-fresh SCM_RIGHTS fd \
                                  into OwnedFd so it is closed on every \
                                  error path (A5.6 leader pidfd)"
                    )]
                    fds.push(unsafe { OwnedFd::from_raw_fd(raw) });
                }
            }
        }
        let n = r.bytes;
        if n == 0 {
            return Err(WireError::Closed);
        }
        if fds.len() > 1 {
            return Err(WireError::Malformed); // extra fds drop/close here
        }
        let fd = fds.into_iter().next();
        match try_decode::<BrokerToCompositor>(&buf[..n])? {
            Some((msg, consumed)) if consumed == n => Ok(Some((msg, fd))),
            // `fd` drops (closes) here — no leak on the malformed path.
            _ => Err(WireError::Malformed),
        }
    }
}

impl AsFd for SeqpacketChannel {
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.fd.as_fd()
    }
}

/// Connect a client `SOCK_SEQPACKET` socket to the broker.
///
/// The compositor's per-greeter-connection [`BrokerEpisode`] calls
/// this ONCE on greeter-accept and OWNS the returned channel for the
/// whole episode (Amendment A6 single-owner).
///
/// # Errors
/// [`WireError::Io`] if `socket`/`connect` fails.
pub fn connect_broker(sock_path: &Path) -> Result<SeqpacketChannel, WireError> {
    use nix::sys::socket::{AddressFamily, SockFlag, SockType, UnixAddr, connect, socket};
    let fd = socket(
        AddressFamily::Unix,
        SockType::SeqPacket,
        SockFlag::empty(),
        None,
    )?;
    // Path-or-abstract: `@name` selects the kernel's net-namespace-
    // scoped abstract socket. Used in the Phase B fromInitrd
    // deployment to bypass the cross-pivot mount-namespace
    // divergence.
    let path_str = sock_path.to_string_lossy();
    let addr = if let Some(abstract_name) = path_str.strip_prefix('@') {
        UnixAddr::new_abstract(abstract_name.as_bytes()).map_err(WireError::Io)?
    } else {
        UnixAddr::new(sock_path).map_err(WireError::Io)?
    };
    connect(fd.as_raw_fd(), &addr)?;
    Ok(SeqpacketChannel::new(fd))
}

/// What the calloop layer must do after driving the episode.
///
/// `greeter_reply` is always written to the greeter fd first. The two
/// terminal flags model the two fds' distinct lifetimes (A6: the
/// episode owns the broker socket the WHOLE episode — past the
/// greeter's greetd connection, which ends at `Spawning`).
#[derive(Debug, Default, PartialEq, Eq)]
pub struct EpisodeOutcome {
    /// Bytes to write to the greeter fd.
    pub greeter_reply: Vec<u8>,
    /// Set once, when greetd reached `Spawning` and `StartSession` was
    /// forwarded to the broker. Carries the broker's PAM-resolved
    /// identity (R8 — never the client hint) for the compositor's
    /// session bookkeeping (`session_uid`); identity bookkeeping ONLY —
    /// the greeter teardown + foreground flip are gated by the A5
    /// two-key `SwapGate` (`session_opened` + the session client's
    /// first non-empty frame), never on `spawned` alone.
    pub spawned: Option<SpawnRequest>,
    /// Amendment A5 key 1: the broker sent `SessionOpened` (leader
    /// forked+dropped, `pam_open_session` ok). Set once. The compositor
    /// feeds this to its two-key [`crate::swap_gate::SwapGate`] — it
    /// does NOT make the session visible by itself (that needs the
    /// session client's first frame too; swapping on this alone is the
    /// flash).
    pub session_opened: bool,
    /// Amendment A5.5 revert trigger: the broker sent `SessionEnded`,
    /// carrying the leader's crash-vs-clean outcome (A5.2, NOT
    /// collapsed). Accompanied by `terminate` (the broker channel is
    /// done); the compositor reverts the foreground before tearing the
    /// `ConnState` down.
    pub session_ended: Option<SessionOutcome>,
    /// The whole episode is over (greetd auth-fail close, broker
    /// EOF/fail-closed, `SessionEnded`, or a fatal protocol error).
    /// The calloop layer drains `greeter_reply` then removes BOTH the
    /// greeter and broker sources.
    pub terminate: bool,
}

/// Per-greeter-connection episode. Owns the broker
/// [`SeqpacketChannel`], the lazily-built [`BrokerRelay`] phase
/// machine, and the sans-IO greetd [`Connection`] for the whole
/// episode (Amendments A6/A7).
///
/// The relay is built lazily on the first [`Demand::Pam`] because the
/// broker `BeginAuth` hint needs the client-supplied `CreateSession`
/// username, which the sans-IO greetd machine only surfaces (via
/// [`Connection::pending_username`]) once it has parsed `CreateSession`.
/// Per Epic R8 that name is ONLY a `pam_start` hint; the authoritative
/// identity is the broker's PAM-resolved `Success`, relayed verbatim.
///
/// Fail-closed: any transport/relay error `poison`s the relay and
/// drives greetd's `broker_closed()` so the greeter sees an auth
/// failure and the episode terminates — never a panic, never a partial
/// success (A7.4).
pub struct BrokerEpisode {
    chan: SeqpacketChannel,
    service: String,
    relay: Option<BrokerRelay>,
    conn: Connection,
    /// Amendment A5.6: the poll-only leader pidfd the broker attached
    /// to `SessionOpened` via SCM_RIGHTS, stashed here (not in
    /// `EpisodeOutcome`, which is `PartialEq`) until the calloop layer
    /// `take_leader_pidfd()`s it to arm a poll-only liveness backstop.
    /// The episode is the sole owner until taken (A6); the compositor
    /// NEVER waitid/reap/signals it (the broker is the sole reaper —
    /// R9/A5).
    leader_pidfd: Option<OwnedFd>,
}

impl BrokerEpisode {
    /// `chan` — the broker channel (owned for the whole episode).
    /// `service` — the PAM service (`/etc/pam.d/<service>`); the
    /// `BeginAuth` hint's service field.
    #[must_use]
    pub fn new(chan: SeqpacketChannel, service: String) -> Self {
        Self {
            chan,
            service,
            relay: None,
            conn: Connection::new(),
            leader_pidfd: None,
        }
    }

    /// Take the Amendment-A5.6 leader pidfd received with
    /// `SessionOpened` (poll-only liveness backstop). `Some` exactly
    /// once, on the call after the `EpisodeOutcome` whose
    /// `session_opened` was set; `None` otherwise (incl. the
    /// fd-less / older path — the backstop is an accelerator, not the
    /// authoritative `SessionEnded` signal).
    pub const fn take_leader_pidfd(&mut self) -> Option<OwnedFd> {
        self.leader_pidfd.take()
    }

    /// The broker fd, for registering it as a calloop source.
    #[must_use]
    pub fn broker_fd(&self) -> BorrowedFd<'_> {
        self.chan.as_fd()
    }

    /// Greeter bytes arrived: drive the sans-IO greetd machine, then
    /// act on its [`Demand`] with NO blocking I/O.
    pub fn on_greeter_bytes(&mut self, bytes: &[u8]) -> EpisodeOutcome {
        let mut out = EpisodeOutcome::default();
        match self.conn.feed_greeter(bytes) {
            Ok(o) => self.act_on_demand(o.reply, o.demand, &mut out),
            Err(_codec) => {
                // The greeter is the untrusted nested client; a
                // malformed greetd frame ends the episode (greetd
                // would have closed the connection anyway). The broker
                // peer is not at fault, so no fail-closed reply is
                // owed — just tear the episode down.
                out.terminate = true;
            }
        }
        out
    }

    /// The broker fd is readable: read at most one frame, feed the
    /// relay, resume the suspended greetd machine. NO blocking I/O.
    pub fn on_broker_readable(&mut self) -> EpisodeOutcome {
        let mut out = EpisodeOutcome::default();
        let (frame, leader_pidfd) = match self.chan.recv_with_fd() {
            Ok(Some(pair)) => pair,
            // Spurious calloop wakeup (no datagram): do nothing.
            Ok(None) => return out,
            // Peer hangup or any transport error → fail closed (A7.4).
            Err(_e) => {
                self.fail_closed(&mut out);
                return out;
            }
        };
        if self.relay.is_none() {
            // A broker frame before any BeginAuth is out of sequence.
            self.fail_closed(&mut out);
            return out;
        }
        let ev = self.relay.as_mut().unwrap().on_broker_frame(frame);
        match ev {
            Ok(RelayEvent::Pam(step)) => match self.conn.resume_pam(step) {
                Ok(o) => self.act_on_demand(o.reply, o.demand, &mut out),
                Err(_codec) => self.fail_closed(&mut out),
            },
            Ok(RelayEvent::SessionOpened) => {
                // A5 key 1. Surface it to the compositor's two-key
                // SwapGate; the episode stays alive for SessionEnded.
                // Nothing to relay to the greeter (its greetd
                // connection ended at Spawning). NOT a visible swap by
                // itself — that needs the session client's first frame.
                out.session_opened = true;
                // A5.6: stash the SCM_RIGHTS leader pidfd (if any) for
                // the calloop layer to take and arm a poll-only
                // liveness backstop. Only SessionOpened carries one.
                self.leader_pidfd = leader_pidfd;
            }
            Ok(RelayEvent::SessionEnded(outcome)) => {
                // A5.5 revert trigger + episode end. The crash-vs-clean
                // outcome (A5.2) drives the compositor's revert; the
                // broker channel is done so the episode terminates.
                out.session_ended = Some(outcome);
                out.terminate = true;
            }
            Err(RelayError::OutOfPhase) => self.fail_closed(&mut out),
        }
        out
    }

    /// Apply a greetd outcome (`reply` bytes + [`Demand`]), performing
    /// the broker side-effects a `Pam`/`Spawn` demand requires. Shared
    /// by the greeter and broker entry points. NO blocking I/O.
    fn act_on_demand(&mut self, reply: Vec<u8>, demand: Demand, out: &mut EpisodeOutcome) {
        out.greeter_reply.extend(reply);
        match demand {
            Demand::Continue => {}
            Demand::Close => out.terminate = true,
            Demand::Pam { response } => {
                if self.relay.is_none() {
                    // First PAM round: build the relay now that greetd
                    // has parsed CreateSession and surfaces the client
                    // username (R8 — a pam_start hint only).
                    let user = self.conn.pending_username().unwrap_or_default().to_owned();
                    self.relay = Some(BrokerRelay::new(self.service.clone(), user));
                }
                let frame = self.relay.as_mut().unwrap().on_pam_step(response);
                match frame {
                    Ok(f) => {
                        if self.chan.send(&f).is_err() {
                            self.fail_closed(out);
                        }
                        // else: SUSPENDED — the broker reply arrives
                        // via `on_broker_readable` (A7 sans-IO).
                    }
                    Err(RelayError::OutOfPhase) => self.fail_closed(out),
                }
            }
            Demand::Spawn(mut spawn) => {
                // greetd already appended Response::Success to `reply`.
                if self.relay.is_none() {
                    self.fail_closed(out);
                    return;
                }
                // Greeters typically forward `Exec=` from a
                // .desktop file verbatim (e.g. DMS sends
                // ["niri-session"]). The broker requires absolute
                // paths. Resolve here, in the unprivileged
                // compositor's address space, against a fixed
                // system PATH. See `resolve_command_path` for the
                // security posture.
                if let Some(resolved) = resolve_command_path(spawn.cmd) {
                    spawn.cmd = resolved;
                } else {
                    tracing::warn!(
                        "session command not absolute and not found on system PATH; \
                         failing closed"
                    );
                    self.fail_closed(out);
                    return;
                }
                let frame = self
                    .relay
                    .as_mut()
                    .unwrap()
                    .start_session(spawn.cmd.clone(), &spawn.env);
                match frame {
                    Ok(f) => {
                        if self.chan.send(&f).is_err() {
                            self.fail_closed(out);
                        } else {
                            // Surface the broker-resolved identity for
                            // the compositor's session bookkeeping. The
                            // greeter's greetd connection is terminal;
                            // the episode continues on the broker
                            // channel for SessionOpened/SessionEnded.
                            out.spawned = Some(spawn);
                        }
                    }
                    Err(RelayError::OutOfPhase) => self.fail_closed(out),
                }
            }
        }
    }

    /// Poison the relay and drive greetd's `broker_closed()`
    /// fail-closed path, then terminate the episode (A7.4).
    fn fail_closed(&mut self, out: &mut EpisodeOutcome) {
        if let Some(r) = self.relay.as_mut() {
            r.poison();
        }
        let o = self.conn.broker_closed();
        out.greeter_reply.extend(o.reply);
        out.terminate = true;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use halmasuit_greetd::{
        AuthMessageType, Request, Response, encode as greetd_encode,
        try_decode as greetd_try_decode,
    };
    use halmasuit_session_ipc::{PromptStyle, SessionOutcome};
    use nix::sys::socket::{AddressFamily, SockFlag, SockType, recv, socketpair};

    /// Absolute commands pass through `resolve_command_path` unchanged.
    /// This is the production-recommended posture for greeter wire
    /// content; the resolver exists to be lenient about XDG `.desktop`
    /// Exec lines, not because anyone in the live path should rely on
    /// PATH lookups.
    #[test]
    fn resolve_command_path_passes_absolute_through() {
        let r = resolve_command_path(vec!["/usr/bin/niri".into(), "--session".into()]);
        assert_eq!(r, Some(vec!["/usr/bin/niri".into(), "--session".into()]));
    }

    /// A relative command not present on any directory in
    /// `SYSTEM_PATH` returns `None` — the episode then fails closed
    /// rather than handing the broker an unresolvable name.
    #[test]
    fn resolve_command_path_rejects_unknown_relative() {
        let r = resolve_command_path(vec!["definitely-not-a-real-binary".into()]);
        assert_eq!(r, None);
    }

    /// Relative paths containing `/` (e.g. `../niri`) are rejected
    /// outright — these are path traversals the greeter has no
    /// business sending and the fixed system PATH would not search
    /// them anyway.
    #[test]
    fn resolve_command_path_rejects_path_traversal() {
        assert_eq!(resolve_command_path(vec!["../niri".into()]), None);
        assert_eq!(resolve_command_path(vec!["bin/niri".into()]), None);
    }

    /// Empty command vector returns `None` (matches the broker's
    /// `SpecError::EmptyCommand` behavior on the other side).
    #[test]
    fn resolve_command_path_rejects_empty() {
        assert_eq!(resolve_command_path(vec![]), None);
    }

    /// Empty-string first element returns `None`. `Path::join("")`
    /// yields the directory itself, which `is_file()` returns false
    /// on — so today this is "safe by accident." Lock that property
    /// in here.
    #[test]
    fn resolve_command_path_rejects_empty_first_arg() {
        assert_eq!(resolve_command_path(vec![String::new()]), None);
    }

    /// Interior NUL in `cmd[0]` returns `None`. The broker would
    /// catch this at `CString::new`, but rejecting here keeps the
    /// invariant explicit and survives a future refactor that
    /// replaces `is_file()` with an in-memory directory cache (in
    /// which case `is_file` would no longer accidentally reject NUL
    /// names).
    #[test]
    fn resolve_command_path_rejects_interior_nul() {
        assert_eq!(resolve_command_path(vec!["niri\0bad".into()]), None);
        assert_eq!(
            resolve_command_path(vec!["a\0b".into(), "--session".into()]),
            None
        );
    }

    /// `connect_broker` accepts `@<name>` paths and connects via the
    /// kernel abstract namespace. Pins the Phase B fromInitrd
    /// cross-mount-ns connect path: halmasuit reaches the broker
    /// socket bound by rootfs systemd's `halmasuit-session.socket`
    /// via the abstract namespace, since the filesystem inode under
    /// /run/ isn't visible from initramfs's surviving process-root.
    #[test]
    fn connect_broker_abstract_round_trip() {
        use nix::sys::socket::{AddressFamily, SockFlag, SockType, UnixAddr, bind, listen};
        use std::os::fd::AsRawFd;

        let name = format!(
            "halmasuit-broker-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        );

        // Bind an abstract SEQPACKET listener and accept on a worker
        // thread; assert connect_broker reaches us.
        let server = nix::sys::socket::socket(
            AddressFamily::Unix,
            SockType::SeqPacket,
            SockFlag::empty(),
            None,
        )
        .expect("server socket");
        let addr = UnixAddr::new_abstract(name.as_bytes()).expect("abstract addr");
        bind(server.as_raw_fd(), &addr).expect("bind abstract");
        listen(&server, nix::sys::socket::Backlog::new(1).unwrap()).expect("listen");

        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let accepted = nix::sys::socket::accept(server.as_raw_fd()).expect("accept");
            tx.send(accepted).unwrap();
        });

        let abstract_path = std::path::PathBuf::from(format!("@{name}"));
        let _client = connect_broker(&abstract_path).expect("connect to @-prefixed broker");
        rx.recv_timeout(std::time::Duration::from_secs(2))
            .expect("server accepted abstract connection");
    }

    /// (compositor channel, broker end) connected SEQPACKET pair. The
    /// tests act as the broker SYNCHRONOUSLY on the broker end —
    /// single-threaded, deterministic (socketpair buffers; the episode
    /// drive methods are pure given buffered input).
    fn pair() -> (SeqpacketChannel, SeqpacketChannel) {
        let (a, b) = socketpair(
            AddressFamily::Unix,
            SockType::SeqPacket,
            None,
            SockFlag::empty(),
        )
        .expect("socketpair");
        (SeqpacketChannel::new(a), SeqpacketChannel::new(b))
    }

    fn broker_recv(end: &SeqpacketChannel) -> CompositorToBroker {
        let mut buf = vec![0u8; std::mem::size_of::<u32>() + MAX_MESSAGE_SIZE as usize];
        let n = recv(end.fd.as_raw_fd(), &mut buf, MsgFlags::empty()).expect("recv");
        assert!(n > 0, "compositor closed the broker channel");
        let (msg, consumed): (CompositorToBroker, usize) =
            try_decode(&buf[..n]).expect("decode").expect("complete");
        assert_eq!(consumed, n);
        msg
    }

    fn broker_send(end: &SeqpacketChannel, msg: &BrokerToCompositor) {
        let bytes = encode(msg).expect("encode");
        let nn = send(end.fd.as_raw_fd(), &bytes, MsgFlags::empty()).expect("send");
        assert_eq!(nn, bytes.len());
    }

    fn greeter_responses(bytes: &[u8]) -> Vec<Response> {
        let mut out = Vec::new();
        let mut cur = bytes;
        while let Some((r, n)) = greetd_try_decode::<Response>(cur).expect("decode") {
            out.push(r);
            cur = &cur[n..];
        }
        out
    }

    fn episode() -> (BrokerEpisode, SeqpacketChannel) {
        let (comp, broker) = pair();
        (BrokerEpisode::new(comp, "halmasuit".into()), broker)
    }

    fn create(username: &str) -> Vec<u8> {
        greetd_encode(&Request::CreateSession {
            username: username.into(),
        })
        .unwrap()
    }

    #[test]
    fn create_emits_begin_auth_with_client_hint_and_suspends() {
        let (mut ep, broker) = episode();
        let o = ep.on_greeter_bytes(&create("alice"));
        // Suspended for the PAM round: nothing for the greeter yet.
        assert!(o.greeter_reply.is_empty());
        assert!(!o.terminate);
        assert_eq!(o.spawned, None);
        // BeginAuth carries the client hint verbatim (R8 hint only).
        assert_eq!(
            broker_recv(&broker),
            CompositorToBroker::BeginAuth {
                service: "halmasuit".into(),
                username: "alice".into(),
            }
        );
    }

    #[test]
    fn challenge_response_success_then_start_session_passes_pam_identity() {
        let (mut ep, broker) = episode();
        ep.on_greeter_bytes(&create("alice"));
        assert!(matches!(
            broker_recv(&broker),
            CompositorToBroker::BeginAuth { .. }
        ));

        // Broker → challenge. Episode resumes greetd → AuthMessage.
        broker_send(
            &broker,
            &BrokerToCompositor::ConvPrompt {
                style: PromptStyle::Secret,
                message: "Password: ".into(),
            },
        );
        let o = ep.on_broker_readable();
        assert_eq!(
            greeter_responses(&o.greeter_reply),
            vec![Response::AuthMessage {
                auth_message_type: AuthMessageType::Secret,
                auth_message: "Password: ".into(),
            }]
        );
        assert!(!o.terminate);

        // Greeter answers → episode forwards ConvResponse.
        let pmr = greetd_encode(&Request::PostAuthMessageResponse {
            response: Some("hunter2".into()),
        })
        .unwrap();
        let o = ep.on_greeter_bytes(&pmr);
        assert!(o.greeter_reply.is_empty());
        match broker_recv(&broker) {
            CompositorToBroker::ConvResponse { response } => {
                assert_eq!(response.expose(), "hunter2");
            }
            other => panic!("expected ConvResponse, got {other:?}"),
        }

        // Broker → Success with the PAM-RESOLVED name (R8).
        broker_send(
            &broker,
            &BrokerToCompositor::Success {
                username: "alice.canonical".into(),
                uid: 1001,
                gid: 1001,
            },
        );
        let o = ep.on_broker_readable();
        assert_eq!(greeter_responses(&o.greeter_reply), vec![Response::Success]);
        assert_eq!(o.spawned, None);
        assert!(!o.terminate);

        // Greeter → StartSession. Episode forwards StartSession and
        // surfaces the broker-resolved identity (NOT "alice").
        let ss = greetd_encode(&Request::StartSession {
            // Absolute path — the broker requires absolute commands,
            // and `resolve_command_path` passes them through
            // unchanged. (A relative `"niri"` would trigger PATH
            // resolution which can fail on the test runner.)
            cmd: vec!["/usr/bin/niri".into()],
            env: vec!["XDG_SESSION_TYPE=wayland".into()],
        })
        .unwrap();
        let o = ep.on_greeter_bytes(&ss);
        assert_eq!(greeter_responses(&o.greeter_reply), vec![Response::Success]);
        let spawned = o.spawned.expect("StartSession surfaces spawn identity");
        assert_eq!(spawned.username, "alice.canonical");
        assert_eq!(spawned.uid, 1001);
        assert_eq!(spawned.gid, 1001);
        match broker_recv(&broker) {
            CompositorToBroker::StartSession { cmd, env } => {
                assert_eq!(cmd, vec!["/usr/bin/niri".to_string()]);
                assert_eq!(env, vec![("XDG_SESSION_TYPE".into(), "wayland".into())]);
            }
            other => panic!("expected StartSession, got {other:?}"),
        }

        // Lifecycle (Amendment A5): SessionOpened is key 1 — surfaced
        // to the compositor's two-key SwapGate, episode stays alive.
        // SessionEnded carries the crash-vs-clean outcome (A5.2) and
        // ends the episode.
        broker_send(&broker, &BrokerToCompositor::SessionOpened);
        let o = ep.on_broker_readable();
        assert!(o.session_opened, "SessionOpened surfaces A5 key 1");
        assert!(o.session_ended.is_none());
        assert!(!o.terminate, "SessionOpened must not end the episode");
        broker_send(
            &broker,
            &BrokerToCompositor::SessionEnded {
                outcome: SessionOutcome::Signaled { signal: 9 },
            },
        );
        let o = ep.on_broker_readable();
        assert_eq!(
            o.session_ended,
            Some(SessionOutcome::Signaled { signal: 9 }),
            "SessionEnded surfaces the crash-vs-clean outcome (A5.2)"
        );
        assert!(o.terminate, "SessionEnded ends the episode");
    }

    #[test]
    fn broker_failure_is_auth_error_and_keeps_greeter_open_for_retry() {
        let (mut ep, broker) = episode();
        ep.on_greeter_bytes(&create("alice"));
        assert!(matches!(
            broker_recv(&broker),
            CompositorToBroker::BeginAuth { .. }
        ));
        broker_send(
            &broker,
            &BrokerToCompositor::Failure {
                reason: "authentication failed".into(),
            },
        );
        let o = ep.on_broker_readable();
        assert_eq!(
            greeter_responses(&o.greeter_reply),
            vec![Response::Error {
                error_type: halmasuit_greetd::ErrorType::AuthError,
                description: "authentication failed".into(),
            }]
        );
        // greetd keeps the connection open after an auth failure — the
        // greeter may retry with a fresh CreateSession on a NEW broker
        // episode; the episode itself does not force-terminate here.
        assert!(!o.terminate);
    }

    #[test]
    fn broker_eof_mid_auth_fails_closed() {
        let (mut ep, broker) = episode();
        ep.on_greeter_bytes(&create("alice"));
        assert!(matches!(
            broker_recv(&broker),
            CompositorToBroker::BeginAuth { .. }
        ));
        // Broker dies before answering (it is SIGKILL-able by design,
        // Epic R5 / Amendment A7.4).
        drop(broker);
        let o = ep.on_broker_readable();
        assert!(o.terminate, "broker EOF must fail the episode closed");
        match &greeter_responses(&o.greeter_reply)[..] {
            [Response::Error { error_type, .. }] => {
                assert!(matches!(error_type, halmasuit_greetd::ErrorType::AuthError));
            }
            other => panic!("expected one AuthError, got {other:?}"),
        }
    }

    #[test]
    fn immediate_success_no_challenge_then_spawn() {
        let (mut ep, broker) = episode();
        ep.on_greeter_bytes(&create("bob"));
        assert!(matches!(
            broker_recv(&broker),
            CompositorToBroker::BeginAuth { .. }
        ));
        broker_send(
            &broker,
            &BrokerToCompositor::Success {
                username: "bob".into(),
                uid: 1000,
                gid: 1000,
            },
        );
        let o = ep.on_broker_readable();
        assert_eq!(greeter_responses(&o.greeter_reply), vec![Response::Success]);

        let ss = greetd_encode(&Request::StartSession {
            cmd: vec!["/usr/bin/sway".into()],
            env: vec![],
        })
        .unwrap();
        let o = ep.on_greeter_bytes(&ss);
        let spawned = o.spawned.expect("spawn surfaced");
        assert_eq!(spawned.username, "bob");
        assert!(matches!(
            broker_recv(&broker),
            CompositorToBroker::StartSession { .. }
        ));
    }

    /// Integration coverage for the `Demand::Spawn` ↔
    /// `resolve_command_path` boundary: a greeter that sends a
    /// command which fails resolution drives the episode through
    /// fail_closed, NOT through to the broker. Exercises the failure
    /// branch without filesystem state — we use a name that cannot
    /// exist on SYSTEM_PATH (interior NUL, rejected before any stat).
    #[test]
    fn unresolvable_session_command_fails_closed_before_broker() {
        let (mut ep, broker) = episode();
        ep.on_greeter_bytes(&create("bob"));
        assert!(matches!(
            broker_recv(&broker),
            CompositorToBroker::BeginAuth { .. }
        ));
        broker_send(
            &broker,
            &BrokerToCompositor::Success {
                username: "bob".into(),
                uid: 1000,
                gid: 1000,
            },
        );
        let _ = ep.on_broker_readable();

        // NUL in cmd[0] is rejected by resolve_command_path before any
        // filesystem stat — the broker never sees a StartSession frame.
        let ss = greetd_encode(&Request::StartSession {
            cmd: vec!["niri\0bad".into()],
            env: vec![],
        })
        .unwrap();
        let o = ep.on_greeter_bytes(&ss);

        // `spawned` is None — the resolver fail_closed'd before
        // surfacing the spawn identity. The broker never received a
        // StartSession frame (the resolver short-circuited).
        assert!(
            o.spawned.is_none(),
            "fail_closed must NOT surface a spawn identity"
        );
        // Episode terminates — greetd's broker_closed path drives
        // the connection down.
        assert!(o.terminate, "fail_closed sets terminate");
        // The greeter-reply contains greetd's pre-Demand::Spawn
        // Response::Success (appended by the state machine before
        // the relay-side resolver runs). This is the same shape as
        // the pre-existing `relay.is_none()` fail_closed branch:
        // the greeter sees Success then channel close. Documenting
        // it here as the current contract so a future change to
        // either greetd's response ordering OR resolve_command_path
        // would trip this assertion.
        assert_eq!(
            greeter_responses(&o.greeter_reply),
            vec![Response::Success],
            "fail_closed contract: greetd's Success was already \
             appended; episode terminates without retracting it"
        );
    }

    #[test]
    fn spurious_broker_wakeup_is_a_noop() {
        // calloop can wake us with no datagram (level edge / race).
        // recv → Ok(None) → the episode does nothing, stays alive.
        let (mut ep, broker) = episode();
        ep.on_greeter_bytes(&create("alice"));
        assert!(matches!(
            broker_recv(&broker),
            CompositorToBroker::BeginAuth { .. }
        ));
        let o = ep.on_broker_readable();
        assert_eq!(o, EpisodeOutcome::default());
        // Channel still usable afterwards.
        broker_send(
            &broker,
            &BrokerToCompositor::Failure {
                reason: "no".into(),
            },
        );
        let o = ep.on_broker_readable();
        assert!(!o.greeter_reply.is_empty());
    }
}
