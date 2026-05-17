//! Unix-socket server building blocks for the greetd protocol.
//!
//! Two layered pieces:
//!
//! - [`Listener`] — binds a Unix socket with a chosen mode, exposes
//!   peer credentials (`SO_PEERCRED`) on accept so the caller can
//!   authorize the connecting greeter. Removes the socket file on
//!   drop so a restart can re-bind.
//! - [`Connection`] — per-connection driver. Owns a buffered read
//!   buffer and a [`SessionState`]. It is **fully sans-IO**
//!   (Amendment A7): it never calls PAM and never touches the broker
//!   socket. The compositor episode loop feeds it greeter bytes via
//!   [`Connection::feed_greeter`]; when a PAM round is required the
//!   call returns [`Demand::Pam`] and the connection SUSPENDS. The
//!   episode loop runs one broker round-trip and RESUMES the
//!   connection via [`Connection::resume_pam`]. Broker EOF is fed in
//!   via [`Connection::broker_closed`] (fail-closed).
//!
//! Neither layer touches an event loop. The compositor wires these
//! into calloop, multiplexing the greeter fd and the privileged broker
//! fd as two non-blocking sources (Amendment A7) so the render loop
//! never blocks on a privileged-peer round-trip.

use crate::{
    Action, CodecError, MAX_MESSAGE_SIZE, PamStep, Request, Response, SessionState, encode,
    try_decode,
};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use zeroize::Zeroizing;

/// Hard cap on `Connection::read_buf` length. A full message body is
/// at most [`MAX_MESSAGE_SIZE`]; the 4-byte length prefix is the only
/// other thing that can be in-flight. Anything above this cap means a
/// peer is feeding garbage that the framing layer has already rejected
/// or refused to complete — close the connection.
const MAX_READ_BUF: usize = MAX_MESSAGE_SIZE as usize + 4;

// greetd is sans-IO and owns no PAM session: it builds nothing and
// rate-limits nothing. System-wide auth-churn bounding is the
// privileged broker's `AuthSlot` concern (Epic R5/R10), not greetd's.

// ── Peer credentials ────────────────────────────────────────────────────

/// Credentials of the connecting peer, as reported by `SO_PEERCRED`.
/// The compositor uses `uid` to verify the connection came from the
/// configured greeter user; `pid` is mostly diagnostic.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PeerCredentials {
    pub pid: u32,
    pub uid: u32,
    pub gid: u32,
}

// ── Listener ────────────────────────────────────────────────────────────

/// Bound greetd Unix socket. The bound path is recorded so `Drop`
/// can clean it up; this matters in dev/test where the daemon may
/// restart without systemd unlinking for us.
pub struct Listener {
    inner: UnixListener,
    path: PathBuf,
}

impl Listener {
    /// Bind `path` and chmod it to `mode` (typically `0o660`).
    ///
    /// The bind itself only sets the socket's create mode (via the
    /// process umask); we chmod afterwards so the resulting file
    /// matches exactly. Ownership (chown) is left to the caller —
    /// production runs from systemd which sets up `RuntimeDirectory`
    /// with the right owner/group.
    ///
    /// # Security
    ///
    /// Defense-in-depth: `mode` is rejected if it grants any
    /// world-access bits (`mode & 0o007 != 0`). The compositor's
    /// greetd socket has no business being world-accessible.
    ///
    /// # Bind/chmod TOCTOU window
    ///
    /// The kernel creates the bound socket file with permissions
    /// `0o666 & !umask` *before* this function's `set_permissions`
    /// runs. In the brief window between the two syscalls, a process
    /// with traverse permission on the parent directory could
    /// `connect()` to the socket. This is acceptable in production
    /// because the production socket lives under
    /// `RuntimeDirectory=halmasuit` (typically mode `0o755`, owned
    /// by `compositor:greeter`) so the parent-directory ACL bounds
    /// the attack surface to root and the compositor itself. If a
    /// future caller binds the socket in a world-traversable
    /// directory, this assumption breaks — make sure the parent
    /// directory is restricted before relying on the socket mode
    /// alone.
    ///
    /// # Errors
    ///
    /// `io::ErrorKind::InvalidInput` if `mode` grants world access.
    /// Otherwise any `io::Error` from `UnixListener::bind` or
    /// `set_permissions`.
    pub fn bind(path: &Path, mode: u32) -> std::io::Result<Self> {
        let inner = bind_socket(path, mode)?;
        Ok(Self {
            inner,
            path: path.to_owned(),
        })
    }

    /// Accept a connection and the peer's credentials.
    ///
    /// The caller is responsible for authorizing `creds.uid` against
    /// the configured greeter UID before doing anything with the
    /// stream — we don't enforce that here so the same code can be
    /// driven from tests where the test process's UID is the peer.
    /// For the common "only accept connections from a specific UID"
    /// case, use [`Self::accept_authorized`] which makes the check
    /// the default path.
    ///
    /// # Blocking
    ///
    /// This call BLOCKS the current thread until a connection
    /// arrives. Event-loop integrations (calloop, etc.) must put the
    /// underlying `UnixListener` in non-blocking mode before
    /// integrating; otherwise `accept` stalls the loop until the
    /// next greeter connects.
    ///
    /// # Errors
    ///
    /// `io::Error` from `accept`, or from the `getsockopt(SO_PEERCRED)`
    /// wrapped by nix.
    pub fn accept_with_creds(&self) -> std::io::Result<(UnixStream, PeerCredentials)> {
        let (stream, _addr) = self.inner.accept()?;
        let creds = peer_creds(&stream)?;
        Ok((stream, creds))
    }

    /// Accept a connection from the configured `allowed_uid` only.
    /// Connections from any other UID are immediately closed (the
    /// stream is dropped on the floor) and the function loops back to
    /// `accept`, so the caller gets a stream they know is authorized.
    ///
    /// # Blocking
    ///
    /// Same as [`Self::accept_with_creds`] — blocks until an
    /// authorized peer connects.
    ///
    /// # Errors
    ///
    /// Same as [`Self::accept_with_creds`].
    pub fn accept_authorized(
        &self,
        allowed_uid: u32,
    ) -> std::io::Result<(UnixStream, PeerCredentials)> {
        loop {
            let (stream, creds) = self.accept_with_creds()?;
            if creds.uid == allowed_uid {
                return Ok((stream, creds));
            }
            // Drop the unauthorized stream and try again. Logging
            // the rejection is the I/O layer's job — this crate
            // doesn't depend on tracing.
            drop(stream);
        }
    }

    /// The bound path. Useful for tests and for logging.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Toggle non-blocking mode on the inner socket. Required before
    /// integrating with an event loop (calloop, mio, etc.) — see the
    /// `Blocking` section of [`Self::accept_with_creds`].
    ///
    /// # Errors
    ///
    /// Any `io::Error` from the underlying `setsockopt`.
    pub fn set_nonblocking(&self, nonblocking: bool) -> std::io::Result<()> {
        self.inner.set_nonblocking(nonblocking)
    }
}

/// Bind a greetd Unix socket and return the raw [`UnixListener`].
///
/// Performs the same validation as [`Listener::bind`] (rejects mode
/// bits granting world access, chmod's to the requested mode after
/// bind) but does NOT wrap the result in the [`Listener`] auto-unlink
/// guard. Production callers (the halmasuit binary integrating with
/// calloop) use this; tests use [`Listener::bind`] to get the
/// auto-unlink-on-drop guard.
///
/// The caller is responsible for the bound path's lifecycle —
/// typically, production runs under systemd's
/// `RuntimeDirectory=halmasuit` which handles cleanup on unit stop.
///
/// # Errors
///
/// `io::ErrorKind::InvalidInput` if `mode` grants world access
/// (`mode & 0o007 != 0`). Otherwise any `io::Error` from
/// `UnixListener::bind` or `set_permissions`.
pub fn bind_socket(path: &Path, mode: u32) -> std::io::Result<UnixListener> {
    if mode & 0o007 != 0 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("bind_socket refuses world-accessible mode {mode:#o}"),
        ));
    }
    let sock = UnixListener::bind(path)?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))?;
    Ok(sock)
}

/// Look up the peer's credentials on an accepted [`UnixStream`].
///
/// Exposed for callers that drive the accept loop themselves
/// (e.g. the halmasuit binary's calloop integration) and therefore
/// can't use [`Listener::accept_with_creds`] / `accept_authorized`.
///
/// # Errors
///
/// Any `io::Error` from `getsockopt(SO_PEERCRED)`.
pub fn peer_credentials(stream: &UnixStream) -> std::io::Result<PeerCredentials> {
    peer_creds(stream)
}

impl Drop for Listener {
    /// Best-effort unlink of the bound socket path.
    ///
    /// # Caveat
    ///
    /// This is `remove_file(&self.path)` without verifying the inode
    /// is still ours — if another process re-created the socket at
    /// the same path between our `bind` and now (which would require
    /// substantial privilege escalation given typical
    /// `RuntimeDirectory` setup), our `Drop` would unlink the new
    /// socket. Production deployments under systemd don't see this
    /// because the unit owns the path exclusively for its lifetime.
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

fn peer_creds(stream: &UnixStream) -> std::io::Result<PeerCredentials> {
    use nix::sys::socket::{getsockopt, sockopt::PeerCredentials as Opt};
    let raw = getsockopt(stream, Opt).map_err(std::io::Error::from)?;
    // PID is i32 in ucred; in practice it's never negative.
    Ok(PeerCredentials {
        pid: raw.pid().cast_unsigned(),
        uid: raw.uid(),
        gid: raw.gid(),
    })
}

// ── Connection ──────────────────────────────────────────────────────────

/// Surfaced via [`Demand::Spawn`] when the state machine reaches
/// `SessionState::Spawning`.
///
/// The episode/IO layer (which owns the privileged broker channel —
/// Amendment A6) forwards it to the broker as
/// `CompositorToBroker::StartSession`; the broker forks-then-drops the
/// session leader (Epic R7). greetd itself never spawns anything and
/// never holds the broker socket.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpawnRequest {
    pub username: String,
    pub uid: u32,
    pub gid: u32,
    pub cmd: Vec<String>,
    pub env: Vec<String>,
}

/// What the I/O layer must do after feeding input into a
/// [`Connection`]. The `reply` bytes in the accompanying [`Outcome`]
/// are always written to the greeter first, regardless of the demand.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Demand {
    /// Nothing more is needed this turn. Keep reading the greeter fd;
    /// no broker action is pending.
    Continue,
    /// SUSPENDED: a PAM round is required. The I/O layer must run
    /// exactly one broker round-trip with this conversation `response`
    /// (`None` for the initial round) and feed the resulting
    /// [`PamStep`] back via [`Connection::resume_pam`]. Until then it
    /// must NOT feed more greeter bytes — the greetd protocol is
    /// strict request/response, so a well-behaved greeter sends
    /// nothing while suspended.
    Pam { response: Option<String> },
    /// Terminal: PAM completed and the greeter asked to start the
    /// session. Forward this as `StartSession` to the broker, then
    /// close the greeter connection.
    Spawn(SpawnRequest),
    /// Terminal: close the connection after writing `reply`.
    Close,
}

/// The result of feeding input into a [`Connection`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Outcome {
    /// Bytes to write back to the greeter (concatenated for every
    /// complete message processed this call; empty if none).
    pub reply: Vec<u8>,
    /// What the I/O layer must do next. See [`Demand`].
    pub demand: Demand,
}

/// Per-connection driver. **Fully sans-IO** (Amendment A7).
///
/// # Buffer bound
///
/// `read_buf` is capped at [`MAX_READ_BUF`] (`MAX_MESSAGE_SIZE + 4`).
/// If a peer pushes more than that without producing a decodable
/// message, [`Self::feed_greeter`] returns
/// [`CodecError::OversizedMessage`] and the caller should close the
/// connection. The buffer's backing memory is wrapped in [`Zeroizing`]
/// so plaintext credentials are wiped when `Connection` drops (greetd
/// `PostAuthMessageResponse` bodies carry passwords).
///
/// # Suspend/resume contract
///
/// After a call returns [`Demand::Pam`] the connection is SUSPENDED:
/// the driver must run exactly one broker round-trip and call
/// [`Self::resume_pam`] before feeding any more greeter bytes. Feeding
/// greeter bytes while suspended is a driver/peer-contract violation
/// and fails the connection closed.
///
/// # Idle-timeout responsibility
///
/// `Connection` has no internal timer. A peer can `connect()` and stop
/// sending; the connection sits idle forever. The I/O layer (calloop
/// integration in the compositor) MUST close connections that go
/// silent mid-message — `halmasuit-greetd` imposes no deadline.
pub struct Connection {
    state: SessionState,
    // Zeroizing wipes the backing memory on drop, covering credential
    // residue in PostAuthMessageResponse bodies that haven't been
    // consumed yet (the consumed bytes are deserialized into the
    // Request enum and relayed onward, where they meet the broker
    // side's own Zeroizing).
    read_buf: Zeroizing<Vec<u8>>,
    /// `true` between emitting [`Demand::Pam`] and the matching
    /// [`Self::resume_pam`]. While set, no greeter bytes are processed.
    awaiting_pam: bool,
    /// Terminal latch. Once set every entry point returns
    /// [`Demand::Close`] with no further state changes.
    closed: bool,
}

impl Default for Connection {
    fn default() -> Self {
        Self::new()
    }
}

impl Connection {
    /// Build a fresh connection driver. No I/O, no PAM session — the
    /// connection is `Idle` and ready to receive its first
    /// `CreateSession`.
    #[must_use]
    pub fn new() -> Self {
        Self {
            state: SessionState::default(),
            read_buf: Zeroizing::new(Vec::new()),
            awaiting_pam: false,
            closed: false,
        }
    }

    /// Append `new_bytes` to the read buffer and process every
    /// complete greeter message that's now decodable, advancing the
    /// state machine with NO I/O.
    ///
    /// Returns the bytes to write back to the greeter plus a
    /// [`Demand`] telling the I/O layer what to do next (suspend for a
    /// PAM round, forward a spawn, close, or keep reading).
    ///
    /// # Errors
    ///
    /// [`CodecError::OversizedMessage`] if the buffer exceeds
    /// [`MAX_READ_BUF`] (the framing layer should have rejected this
    /// already; the explicit cap is defense-in-depth). Other
    /// [`CodecError`] variants on framing or JSON errors. The caller
    /// should close the connection on any error.
    pub fn feed_greeter(&mut self, new_bytes: &[u8]) -> Result<Outcome, CodecError> {
        if self.closed {
            return Ok(Outcome {
                reply: Vec::new(),
                demand: Demand::Close,
            });
        }
        if self.awaiting_pam {
            // A PAM round is in flight; the greeter must wait for the
            // Response it will produce. The greetd protocol is strict
            // request/response, so unsolicited bytes here mean a
            // buggy/hostile greeter — fail closed.
            self.closed = true;
            let reply = encode(&Response::Error {
                error_type: crate::ErrorType::Error,
                description: "greeter sent data while a PAM round was in flight".into(),
            })?;
            return Ok(Outcome {
                reply,
                demand: Demand::Close,
            });
        }
        self.read_buf.extend_from_slice(new_bytes);
        if self.read_buf.len() > MAX_READ_BUF {
            return Err(CodecError::OversizedMessage(
                u32::try_from(self.read_buf.len()).unwrap_or(u32::MAX),
                MAX_MESSAGE_SIZE,
            ));
        }
        self.drive()
    }

    /// Resume a suspended connection with the broker's PAM result.
    ///
    /// Call this exactly once after a [`Demand::Pam`], with the
    /// [`PamStep`] obtained from one broker round-trip. The state
    /// machine resumes, the greeter sees the resulting `Response`, and
    /// any greeter bytes that were already buffered are then drained.
    ///
    /// # Errors
    ///
    /// [`CodecError`] if encoding the resulting `Response` fails or a
    /// buffered message can't be decoded. Calling this without an
    /// outstanding PAM round fails the connection closed.
    pub fn resume_pam(&mut self, step: PamStep) -> Result<Outcome, CodecError> {
        if self.closed {
            return Ok(Outcome {
                reply: Vec::new(),
                demand: Demand::Close,
            });
        }
        if !self.awaiting_pam {
            self.closed = true;
            let reply = encode(&Response::Error {
                error_type: crate::ErrorType::Error,
                description: "resume_pam called without an outstanding PAM round".into(),
            })?;
            return Ok(Outcome {
                reply,
                demand: Demand::Close,
            });
        }
        self.awaiting_pam = false;
        let resp = self.state.on_pam_result(step);
        let mut reply = encode(&resp)?;
        // Auth failure is NOT terminal for the connection: the state
        // machine is back in Idle and a well-behaved greeter starts
        // over with a fresh CreateSession on the same connection
        // (greetd protocol). Success/AuthMessage are likewise
        // non-terminal. So in every case continue draining whatever
        // the greeter has already buffered.
        let mut out = self.drive()?;
        let mut all = std::mem::take(&mut reply);
        all.extend(out.reply);
        out.reply = all;
        Ok(out)
    }

    /// Feed in broker-side EOF / connection loss. Fail closed: the
    /// privileged peer is SIGKILL-able by design (Epic R5 / Amendment
    /// A7.4), so its disappearance is an ordinary source event, not a
    /// crash. Tell the greeter auth failed and close. Infallible — a
    /// fixed short `Response` always encodes.
    pub fn broker_closed(&mut self) -> Outcome {
        if self.closed {
            return Outcome {
                reply: Vec::new(),
                demand: Demand::Close,
            };
        }
        self.closed = true;
        self.awaiting_pam = false;
        let reply = if matches!(self.state, SessionState::Spawning { .. }) {
            // Already past auth: the session leader is launched and the
            // greeter conversation is over — nothing to tell it.
            Vec::new()
        } else {
            self.state = SessionState::Idle;
            encode(&Response::Error {
                error_type: crate::ErrorType::AuthError,
                description: "broker connection closed".into(),
            })
            .unwrap_or_default()
        };
        Outcome {
            reply,
            demand: Demand::Close,
        }
    }

    /// Decode and apply buffered greeter requests until the buffer is
    /// exhausted or the connection suspends/terminates. No I/O.
    fn drive(&mut self) -> Result<Outcome, CodecError> {
        let mut reply = Vec::new();
        loop {
            if self.closed {
                return Ok(Outcome {
                    reply,
                    demand: Demand::Close,
                });
            }
            let Some((req, consumed)) = try_decode::<Request>(&self.read_buf)? else {
                return Ok(Outcome {
                    reply,
                    demand: Demand::Continue,
                });
            };
            match self.state.on_request(req) {
                Ok(Action::Reply(r)) => {
                    append_encoded(&r, &mut reply)?;
                    self.read_buf.drain(..consumed);
                }
                Ok(Action::Pam { response }) => {
                    self.read_buf.drain(..consumed);
                    self.awaiting_pam = true;
                    return Ok(Outcome {
                        reply,
                        demand: Demand::Pam { response },
                    });
                }
                Ok(Action::Spawn(spawn)) => {
                    // greetd acks StartSession before the leader runs.
                    append_encoded(&Response::Success, &mut reply)?;
                    self.read_buf.drain(..consumed);
                    self.closed = true;
                    return Ok(Outcome {
                        reply,
                        demand: Demand::Spawn(spawn),
                    });
                }
                Err(sm_err) => {
                    // Protocol violation: surface as a wire error but
                    // keep the connection open (the greeter may retry).
                    append_encoded(&sm_err.to_response(), &mut reply)?;
                    self.read_buf.drain(..consumed);
                }
            }
        }
    }
}

fn append_encoded(resp: &Response, reply: &mut Vec<u8>) -> Result<(), CodecError> {
    // PamStep::Failure's reason has already been truncated by
    // on_pam_result to keep Response::Error::description below
    // MAX_MESSAGE_SIZE, so OversizedMessage shouldn't fire here. If
    // serde_json fails (it can't for our type — no non-string Map
    // keys), or a future change introduces a path that could produce
    // an oversize Response, we propagate the error so the I/O layer
    // closes the connection rather than silently dropping the reply.
    let bytes = encode(resp)?;
    reply.extend(bytes);
    Ok(())
}

// ── Tests ───────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{AuthMessageType, ErrorType};
    use std::io::{Read, Write};
    use std::os::unix::fs::FileTypeExt;
    use tempfile::TempDir;

    // Listener tests ─────────────────────────────────────────────────────

    #[test]
    fn listener_bind_creates_socket_with_mode() {
        let dir = TempDir::new().unwrap();
        let sock = dir.path().join("greetd.sock");
        let l = Listener::bind(&sock, 0o660).expect("bind");
        let meta = std::fs::metadata(&sock).expect("metadata");
        assert!(meta.file_type().is_socket(), "expected socket file");
        let perms = meta.permissions().mode() & 0o777;
        assert_eq!(perms, 0o660, "got mode {perms:o}");
        drop(l);
        // Drop should have unlinked it.
        assert!(
            !sock.exists(),
            "Listener::drop should remove the socket file"
        );
    }

    #[test]
    fn listener_accept_returns_peer_uid_matching_self() {
        let dir = TempDir::new().unwrap();
        let sock = dir.path().join("greetd.sock");
        let l = Listener::bind(&sock, 0o660).unwrap();

        let client =
            std::thread::spawn(move || UnixStream::connect(&sock).expect("client connect"));

        let (_server_side, creds) = l.accept_with_creds().expect("accept");
        let _ = client.join().unwrap();

        let self_uid = nix::unistd::getuid().as_raw();
        assert_eq!(
            creds.uid, self_uid,
            "peer should be us, got {} self {}",
            creds.uid, self_uid
        );
    }

    // Connection tests ───────────────────────────────────────────────────
    //
    // Amendment A7: `Connection` is fully sans-IO. The PAM result is
    // supplied directly as a `PamStep` (what the compositor's
    // `BrokerRelay` would yield after one broker round-trip); there is
    // no `PamSession` trait, no factory, no per-connection build cap
    // (R10 — the broker's AuthSlot churn throttle bounds churn
    // system-wide).

    fn drain_responses(bytes: &[u8]) -> Vec<Response> {
        let mut out = Vec::new();
        let mut cursor = bytes;
        while let Some((resp, n)) = try_decode::<Response>(cursor).expect("test decode") {
            out.push(resp);
            cursor = &cursor[n..];
        }
        out
    }

    #[test]
    fn connection_happy_path_create_resume_start() {
        // The client requests "alice"; PAM canonicalizes to
        // "alice.canonical". The SpawnRequest — and thus the broker's
        // initgroups(3) — must carry PAM's resolved name end-to-end,
        // not the pre-auth client string. (F1.)
        let mut conn = Connection::new();

        // greeter → daemon: CreateSession ⇒ suspend for a PAM round.
        let create = encode(&Request::CreateSession {
            username: "alice".into(),
        })
        .unwrap();
        let o1 = conn.feed_greeter(&create).expect("feed create");
        assert!(o1.reply.is_empty(), "no reply until PAM resolves");
        assert_eq!(o1.demand, Demand::Pam { response: None });

        // broker round-trip resolves immediately with Success.
        let o2 = conn
            .resume_pam(PamStep::Success {
                username: "alice.canonical".into(),
                uid: 1000,
                gid: 1000,
            })
            .expect("resume success");
        assert_eq!(drain_responses(&o2.reply), vec![Response::Success]);
        assert_eq!(o2.demand, Demand::Continue);

        // greeter → daemon: StartSession ⇒ spawn + close.
        let start = encode(&Request::StartSession {
            cmd: vec!["niri".into()],
            env: vec!["XDG_SESSION_TYPE=wayland".into()],
        })
        .unwrap();
        let o3 = conn.feed_greeter(&start).expect("feed start");
        assert_eq!(drain_responses(&o3.reply), vec![Response::Success]);
        match o3.demand {
            Demand::Spawn(spawn) => {
                assert_eq!(spawn.username, "alice.canonical");
                assert_eq!(spawn.uid, 1000);
                assert_eq!(spawn.gid, 1000);
                assert_eq!(spawn.cmd, vec!["niri".to_string()]);
                assert_eq!(spawn.env, vec!["XDG_SESSION_TYPE=wayland".to_string()]);
            }
            other => panic!("expected Spawn, got {other:?}"),
        }

        // Terminal: any further feed is Close.
        let o4 = conn.feed_greeter(&start).expect("post-spawn feed");
        assert_eq!(o4.demand, Demand::Close);
    }

    #[test]
    fn connection_drives_challenge_response_flow() {
        let mut conn = Connection::new();

        let create = encode(&Request::CreateSession {
            username: "alice".into(),
        })
        .unwrap();
        let o1 = conn.feed_greeter(&create).unwrap();
        assert_eq!(o1.demand, Demand::Pam { response: None });

        let o2 = conn
            .resume_pam(PamStep::Challenge {
                kind: AuthMessageType::Secret,
                prompt: "password:".into(),
            })
            .unwrap();
        assert_eq!(
            drain_responses(&o2.reply),
            vec![Response::AuthMessage {
                auth_message_type: AuthMessageType::Secret,
                auth_message: "password:".into(),
            }]
        );
        assert_eq!(o2.demand, Demand::Continue);

        let pmr = encode(&Request::PostAuthMessageResponse {
            response: Some("hunter2".into()),
        })
        .unwrap();
        let o3 = conn.feed_greeter(&pmr).unwrap();
        assert!(o3.reply.is_empty());
        assert_eq!(
            o3.demand,
            Demand::Pam {
                response: Some("hunter2".into())
            }
        );

        let o4 = conn
            .resume_pam(PamStep::Success {
                username: "alice".into(),
                uid: 1000,
                gid: 1000,
            })
            .unwrap();
        assert_eq!(drain_responses(&o4.reply), vec![Response::Success]);
        assert_eq!(o4.demand, Demand::Continue);
    }

    #[test]
    fn connection_rejects_start_in_idle() {
        let mut conn = Connection::new();
        let start = encode(&Request::StartSession {
            cmd: vec!["niri".into()],
            env: vec![],
        })
        .unwrap();
        let out = conn.feed_greeter(&start).expect("feed");
        let resps = drain_responses(&out.reply);
        assert_eq!(resps.len(), 1);
        match &resps[0] {
            Response::Error {
                error_type,
                description,
            } => {
                assert!(matches!(error_type, ErrorType::Error));
                assert!(description.contains("start_session"));
            }
            other => panic!("expected Error, got {other:?}"),
        }
        // Protocol error keeps the connection open for a retry.
        assert_eq!(out.demand, Demand::Continue);
    }

    #[test]
    fn connection_handles_partial_messages() {
        // Feed CreateSession bytes one at a time. No reply / no demand
        // change until the last byte arrives; then suspend for PAM.
        let mut conn = Connection::new();
        let full = encode(&Request::CreateSession {
            username: "alice".into(),
        })
        .unwrap();
        for byte in &full[..full.len() - 1] {
            let out = conn.feed_greeter(&[*byte]).expect("partial feed");
            assert!(out.reply.is_empty(), "no reply before last byte");
            assert_eq!(out.demand, Demand::Continue);
        }
        let out = conn
            .feed_greeter(&[full[full.len() - 1]])
            .expect("final byte");
        assert!(out.reply.is_empty());
        assert_eq!(out.demand, Demand::Pam { response: None });
    }

    #[test]
    fn connection_rejects_oversized_read_buf() {
        let mut conn = Connection::new();
        // Push past MAX_READ_BUF without ever completing a message.
        let oversize: Vec<u8> = vec![0; MAX_READ_BUF + 1];
        let r = conn.feed_greeter(&oversize);
        assert!(
            matches!(r, Err(CodecError::OversizedMessage(_, _))),
            "got: {r:?}"
        );
    }

    #[test]
    fn connection_broker_eof_fails_closed() {
        // CreateSession suspends for PAM; the broker dies before it
        // answers. greetd must fail the auth closed (Amendment A7.4).
        let mut conn = Connection::new();
        let create = encode(&Request::CreateSession {
            username: "alice".into(),
        })
        .unwrap();
        let o1 = conn.feed_greeter(&create).unwrap();
        assert_eq!(o1.demand, Demand::Pam { response: None });

        let o2 = conn.broker_closed();
        assert_eq!(o2.demand, Demand::Close);
        match &drain_responses(&o2.reply)[..] {
            [
                Response::Error {
                    error_type,
                    description,
                },
            ] => {
                assert!(matches!(error_type, ErrorType::AuthError));
                assert!(description.contains("broker"));
            }
            other => panic!("expected one AuthError, got {other:?}"),
        }
    }

    #[test]
    fn connection_feed_while_awaiting_pam_fails_closed() {
        // The greeter must wait for the Response a PAM round produces.
        // Unsolicited bytes while suspended fail the connection closed.
        let mut conn = Connection::new();
        let create = encode(&Request::CreateSession {
            username: "alice".into(),
        })
        .unwrap();
        assert_eq!(
            conn.feed_greeter(&create).unwrap().demand,
            Demand::Pam { response: None }
        );
        let out = conn.feed_greeter(&create).expect("second feed");
        assert_eq!(out.demand, Demand::Close);
        match &drain_responses(&out.reply)[..] {
            [Response::Error { error_type, .. }] => {
                assert!(matches!(error_type, ErrorType::Error));
            }
            other => panic!("expected one protocol Error, got {other:?}"),
        }
    }

    #[test]
    fn connection_resume_without_outstanding_round_fails_closed() {
        let mut conn = Connection::new();
        let out = conn
            .resume_pam(PamStep::Success {
                username: "root".into(),
                uid: 0,
                gid: 0,
            })
            .expect("resume");
        assert_eq!(out.demand, Demand::Close);
        match &drain_responses(&out.reply)[..] {
            [Response::Error { error_type, .. }] => {
                assert!(matches!(error_type, ErrorType::Error));
            }
            other => panic!("expected one protocol Error, got {other:?}"),
        }
    }

    #[test]
    fn connection_auth_failure_keeps_connection_open() {
        // A failed PAM round returns an AuthError but does NOT close —
        // the greeter may CreateSession again on the same connection.
        let mut conn = Connection::new();
        let create = encode(&Request::CreateSession {
            username: "alice".into(),
        })
        .unwrap();
        conn.feed_greeter(&create).unwrap();
        let o = conn
            .resume_pam(PamStep::Failure {
                reason: "bad password".into(),
            })
            .unwrap();
        assert_eq!(
            drain_responses(&o.reply),
            vec![Response::Error {
                error_type: ErrorType::AuthError,
                description: "bad password".into(),
            }]
        );
        assert_eq!(o.demand, Demand::Continue);

        // Retry works: a fresh CreateSession suspends for PAM again.
        let o2 = conn.feed_greeter(&create).unwrap();
        assert_eq!(o2.demand, Demand::Pam { response: None });
    }

    // ── New tests covering review-improvement behaviors ─────────────────

    #[test]
    fn listener_bind_rejects_world_accessible_mode() {
        let dir = TempDir::new().unwrap();
        let sock = dir.path().join("greetd.sock");
        let r = Listener::bind(&sock, 0o666);
        match r {
            Err(e) => assert_eq!(e.kind(), std::io::ErrorKind::InvalidInput),
            Ok(_) => panic!("expected InvalidInput for mode 0o666"),
        }
        // No socket should have been created.
        assert!(!sock.exists());
    }

    #[test]
    fn listener_accept_authorized_returns_when_uid_matches() {
        let dir = TempDir::new().unwrap();
        let sock = dir.path().join("greetd.sock");
        let l = Listener::bind(&sock, 0o660).unwrap();

        let client =
            std::thread::spawn(move || UnixStream::connect(&sock).expect("client connect"));

        // Test process IS the peer, so accept_authorized(self_uid) succeeds.
        let self_uid = nix::unistd::getuid().as_raw();
        let (_stream, creds) = l.accept_authorized(self_uid).expect("accept_authorized");
        assert_eq!(creds.uid, self_uid);
        let _ = client.join().unwrap();
    }

    #[test]
    fn listener_accept_authorized_drops_when_uid_does_not_match() {
        // Negative-path companion to the test above. The VM gate's
        // header in tests/halmasuit-vm.nix:18-20 defers wrong-UID
        // rejection coverage here; this test fulfills that deferral.
        let dir = TempDir::new().unwrap();
        let sock = dir.path().join("greetd.sock");
        let l = Listener::bind(&sock, 0o660).unwrap();
        // Non-blocking listener so the loop in accept_authorized
        // bubbles up `WouldBlock` instead of hanging once the queued
        // client has been rejected.
        l.set_nonblocking(true).unwrap();

        let self_uid = nix::unistd::getuid().as_raw();
        let disallowed = self_uid.wrapping_add(1);

        let mut client = UnixStream::connect(&sock).expect("client connect");

        // accept_authorized must drop the queued stream (peer uid
        // doesn't match) and then propagate the next `accept`'s
        // `WouldBlock` since no more clients are queued.
        let result = l.accept_authorized(disallowed);
        match &result {
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
            other => panic!("expected WouldBlock after rejection; got {other:?}"),
        }

        // The client must observe EOF — that's the visible evidence
        // halmasuit-greetd actually closed the unauthorized stream
        // (drop(stream) on the server side). A 2s read timeout
        // prevents the test from hanging if the close didn't fire.
        client
            .set_read_timeout(Some(std::time::Duration::from_secs(2)))
            .unwrap();
        let mut buf = [0u8; 1];
        let n = client.read(&mut buf).expect("client read");
        assert_eq!(
            n, 0,
            "client should observe EOF after server drops rejected stream"
        );
    }

    #[test]
    fn append_encoded_propagates_oversized_response() {
        let mut reply = Vec::new();
        let huge = "x".repeat(MAX_MESSAGE_SIZE as usize + 1);
        let resp = Response::Error {
            error_type: ErrorType::Error,
            description: huge,
        };
        let r = append_encoded(&resp, &mut reply);
        assert!(
            matches!(r, Err(CodecError::OversizedMessage(_, _))),
            "got: {r:?}"
        );
        assert!(reply.is_empty(), "no bytes should have been appended");
    }

    // Spot-check: a UnixStream pair can drive bytes both directions.
    #[test]
    fn listener_to_connection_smoke() {
        let dir = TempDir::new().unwrap();
        let sock = dir.path().join("greetd.sock");
        let l = Listener::bind(&sock, 0o660).unwrap();

        let client_thread = std::thread::spawn(move || {
            let mut stream = UnixStream::connect(&sock).unwrap();
            stream
                .write_all(&encode(&Request::CancelSession).unwrap())
                .unwrap();
            let mut buf = [0u8; 256];
            let n = stream.read(&mut buf).unwrap();
            buf[..n].to_vec()
        });

        let (mut server_side, _creds) = l.accept_with_creds().unwrap();

        let mut conn = Connection::new();

        // Read what the client sent, feed into Connection.
        let mut buf = [0u8; 256];
        let n = server_side.read(&mut buf).unwrap();
        let out = conn.feed_greeter(&buf[..n]).unwrap();
        server_side.write_all(&out.reply).unwrap();

        let echoed = client_thread.join().unwrap();
        let resps = drain_responses(&echoed);
        assert_eq!(resps.len(), 1);
        match &resps[0] {
            Response::Error { description, .. } => {
                assert!(description.contains("cancel_session"));
            }
            other => panic!("expected Error, got {other:?}"),
        }
    }
}
