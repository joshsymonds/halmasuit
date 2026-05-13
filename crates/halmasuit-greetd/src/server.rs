//! Unix-socket server building blocks for the greetd protocol.
//!
//! Two layered pieces:
//!
//! - [`Listener`] — binds a Unix socket with a chosen mode, exposes
//!   peer credentials (`SO_PEERCRED`) on accept so the caller can
//!   authorize the connecting greeter. Removes the socket file on
//!   drop so a restart can re-bind.
//! - [`Connection`] — per-connection driver. Owns a buffered read
//!   buffer, a [`SessionState`], and an optional in-flight
//!   `Box<dyn PamSession + Send>` built on demand via a
//!   [`PamSessionFactory`]. Feed bytes via [`Connection::process`];
//!   it decodes complete messages, advances the state machine, and
//!   returns reply bytes plus (on the terminal `Spawning` transition)
//!   a [`SpawnRequest`] for halmasuit-spawn.
//!
//! Neither layer touches an event loop. The halmasuit binary wires
//! these into calloop in a separate task.

use crate::{CodecError, PamSession, Request, Response, SessionState, encode, try_decode};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::Arc;

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
    /// The bind itself only sets the socket's create mode; we
    /// chmod afterwards so the resulting file matches exactly.
    /// Ownership (chown) is left to the caller — production runs
    /// from systemd which sets up `RuntimeDirectory` with the
    /// right owner/group.
    ///
    /// # Errors
    ///
    /// Any `io::Error` from `UnixListener::bind` or `set_permissions`.
    pub fn bind(path: &Path, mode: u32) -> std::io::Result<Self> {
        let inner = UnixListener::bind(path)?;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))?;
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

    /// The bound path. Useful for tests and for logging.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for Listener {
    fn drop(&mut self) {
        // Best-effort unlink — if the path is gone (e.g. cleaned up
        // by tempdir teardown) this errors silently.
        let _ = std::fs::remove_file(&self.path);
    }
}

fn peer_creds(stream: &UnixStream) -> std::io::Result<PeerCredentials> {
    use nix::sys::socket::{getsockopt, sockopt::PeerCredentials as Opt};
    let raw = getsockopt(stream, Opt).map_err(|e| std::io::Error::from_raw_os_error(e as i32))?;
    // PID is i32 in ucred; in practice it's never negative.
    Ok(PeerCredentials {
        pid: raw.pid().cast_unsigned(),
        uid: raw.uid(),
        gid: raw.gid(),
    })
}

// ── PamSessionFactory + Connection ──────────────────────────────────────

/// Builds a fresh PAM session for each `CreateSession` request.
///
/// Production: a factory closes over the PAM service name and
/// returns a fresh `halmasuit_pam::PamThread`. Tests: a factory
/// returns a scripted mock.
pub trait PamSessionFactory: Send + Sync {
    /// Build a new session for `username`. The returned object is
    /// consumed by [`Connection`] for one auth round; if the greeter
    /// cancels and creates again, a fresh session is built.
    fn build(&self, username: &str) -> Box<dyn PamSession + Send>;
}

/// Once a session reaches `SessionState::Spawning`, the I/O layer
/// hands this off to halmasuit-spawn (after closing the connection).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpawnRequest {
    pub username: String,
    pub uid: u32,
    pub gid: u32,
    pub cmd: Vec<String>,
    pub env: Vec<String>,
}

/// Result of feeding bytes into [`Connection::process`].
#[derive(Debug, Default)]
pub struct ProcessOutput {
    /// Bytes to write back to the peer (concatenated for all complete
    /// messages parsed this call; empty if nothing parsed yet).
    pub reply: Vec<u8>,
    /// Populated when the state machine reached `Spawning` during
    /// this call. The caller should write `reply`, close the
    /// connection, and invoke halmasuit-spawn with these arguments.
    pub spawn: Option<SpawnRequest>,
    /// Caller should close the connection after writing `reply`.
    /// Set on `Spawning` (auth complete) and on terminal codec
    /// errors handled by the caller.
    pub close: bool,
}

/// Per-connection driver.
pub struct Connection {
    state: SessionState,
    read_buf: Vec<u8>,
    factory: Arc<dyn PamSessionFactory>,
    current_session: Option<Box<dyn PamSession + Send>>,
}

impl Connection {
    /// Build a fresh connection driver. No I/O is performed here;
    /// the connection is `Idle` and ready to receive its first
    /// `CreateSession`.
    #[must_use]
    pub fn new(factory: Arc<dyn PamSessionFactory>) -> Self {
        Self {
            state: SessionState::default(),
            read_buf: Vec::new(),
            factory,
            current_session: None,
        }
    }

    /// Append `new_bytes` to the read buffer and process every
    /// complete message that's now decodable.
    ///
    /// # Errors
    ///
    /// [`CodecError`] on framing or JSON errors. The caller should
    /// close the connection on any error.
    pub fn process(&mut self, new_bytes: &[u8]) -> Result<ProcessOutput, CodecError> {
        self.read_buf.extend_from_slice(new_bytes);
        let mut out = ProcessOutput::default();
        while let Some((req, consumed)) = try_decode::<Request>(&self.read_buf)? {
            self.handle_request(req, &mut out);
            self.read_buf.drain(..consumed);
            if out.close {
                break;
            }
        }
        Ok(out)
    }

    fn handle_request(&mut self, req: Request, out: &mut ProcessOutput) {
        // CreateSession (re)builds the per-session PamSession. Any
        // earlier in-flight session is dropped — the state machine
        // refuses double-create via DoubleCreate, but we still need
        // the factory's output to give the state machine something
        // to call step() on for the new username.
        if let Request::CreateSession { username } = &req {
            // Only rebuild when the state machine will actually accept
            // CreateSession (i.e. when we're in Idle). In other states
            // the state machine returns DoubleCreate without touching
            // pam, so the current_session is preserved.
            if matches!(self.state, SessionState::Idle) {
                self.current_session = Some(self.factory.build(username));
            }
        }

        // We need *some* PamSession to satisfy the state machine's
        // signature even though several (state, request) pairs return
        // an error without invoking it. Use a tiny NullPam for those
        // cases; the state machine guarantees it's never called.
        let response = match self.current_session.as_mut() {
            Some(session) => self.state.handle(req, session.as_mut()),
            None => self.state.handle(req, &mut NullPam),
        };

        match response {
            Ok(resp) => append_encoded(&resp, out),
            Err(sm_err) => append_encoded(&sm_err.to_response(), out),
        }

        // After handling, inspect the state. Spawning is terminal for
        // this connection: surface the spawn parameters, drop any
        // PAM session, close.
        if let SessionState::Spawning {
            username,
            uid,
            gid,
            cmd,
            env,
        } = &self.state
        {
            out.spawn = Some(SpawnRequest {
                username: username.clone(),
                uid: *uid,
                gid: *gid,
                cmd: cmd.clone(),
                env: env.clone(),
            });
            self.current_session = None;
            out.close = true;
        } else if matches!(self.state, SessionState::Idle) {
            // CancelSession or auth-failure dropped us back to Idle —
            // discard the now-finished PAM session.
            self.current_session = None;
        }
    }
}

fn append_encoded(resp: &Response, out: &mut ProcessOutput) {
    // encode failure here would be a serde_json bug; surface it by
    // writing nothing (the caller's CodecError will fire on the next
    // read). In practice serde_json can only fail on non-string Map
    // keys, which our Response type doesn't have.
    if let Ok(bytes) = encode(resp) {
        out.reply.extend(bytes);
    }
}

// ── NullPam ─────────────────────────────────────────────────────────────
//
// Placeholder PAM session used when the state machine refuses a
// request before it would call step(). The state machine's
// (Idle, StartSession) → StartBeforeAuth path, for example, never
// touches pam — so passing this NullPam is safe. If it ever IS
// called the state machine has a bug; we return Failure rather than
// panic to keep the protocol responsive.

struct NullPam;

impl PamSession for NullPam {
    fn step(&mut self, _response: Option<String>) -> crate::PamStep {
        crate::PamStep::Failure {
            reason: "no PAM session active (state machine bug)".into(),
        }
    }
}

// ── Tests ───────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{AuthMessageType, ErrorType, PamStep};
    use std::collections::VecDeque;
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

    /// A test-only factory that hands out PamSessions returning
    /// scripted PamSteps.
    struct ScriptedFactory {
        scripts: std::sync::Mutex<VecDeque<VecDeque<PamStep>>>,
    }

    impl ScriptedFactory {
        fn new(scripts: Vec<Vec<PamStep>>) -> Self {
            Self {
                scripts: std::sync::Mutex::new(scripts.into_iter().map(VecDeque::from).collect()),
            }
        }
    }

    impl PamSessionFactory for ScriptedFactory {
        fn build(&self, _username: &str) -> Box<dyn PamSession + Send> {
            let next = self.scripts.lock().unwrap().pop_front().unwrap_or_default();
            Box::new(ScriptedPam { steps: next })
        }
    }

    struct ScriptedPam {
        steps: VecDeque<PamStep>,
    }

    impl PamSession for ScriptedPam {
        fn step(&mut self, _response: Option<String>) -> PamStep {
            self.steps.pop_front().unwrap_or_else(|| PamStep::Failure {
                reason: "scripted PAM exhausted".into(),
            })
        }
    }

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
    fn connection_happy_path_create_then_start() {
        let factory = Arc::new(ScriptedFactory::new(vec![vec![PamStep::Success {
            uid: 1000,
            gid: 1000,
        }]]));
        let mut conn = Connection::new(factory);

        // greeter → daemon: CreateSession
        let create = encode(&Request::CreateSession {
            username: "alice".into(),
        })
        .unwrap();
        let out1 = conn.process(&create).expect("process create");
        let r1 = drain_responses(&out1.reply);
        assert_eq!(r1, vec![Response::Success]);
        assert!(out1.spawn.is_none());
        assert!(!out1.close);

        // greeter → daemon: StartSession
        let start = encode(&Request::StartSession {
            cmd: vec!["niri".into()],
            env: vec!["XDG_SESSION_TYPE=wayland".into()],
        })
        .unwrap();
        let out2 = conn.process(&start).expect("process start");
        let r2 = drain_responses(&out2.reply);
        assert_eq!(r2, vec![Response::Success]);
        assert!(out2.close, "Spawning should set close=true");
        let spawn = out2.spawn.expect("spawn populated");
        assert_eq!(spawn.username, "alice");
        assert_eq!(spawn.uid, 1000);
        assert_eq!(spawn.gid, 1000);
        assert_eq!(spawn.cmd, vec!["niri".to_string()]);
        assert_eq!(spawn.env, vec!["XDG_SESSION_TYPE=wayland".to_string()]);
    }

    #[test]
    fn connection_rejects_start_in_idle() {
        let factory = Arc::new(ScriptedFactory::new(vec![]));
        let mut conn = Connection::new(factory);
        let start = encode(&Request::StartSession {
            cmd: vec!["niri".into()],
            env: vec![],
        })
        .unwrap();
        let out = conn.process(&start).expect("process");
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
        assert!(!out.close);
        assert!(out.spawn.is_none());
    }

    #[test]
    fn connection_handles_partial_messages() {
        // Feed CreateSession bytes one at a time. No reply until the
        // last byte arrives; then a single Success.
        let factory = Arc::new(ScriptedFactory::new(vec![vec![PamStep::Success {
            uid: 1000,
            gid: 1000,
        }]]));
        let mut conn = Connection::new(factory);

        let full = encode(&Request::CreateSession {
            username: "alice".into(),
        })
        .unwrap();
        let mut accumulated_reply = Vec::new();
        for byte in &full[..full.len() - 1] {
            let out = conn.process(&[*byte]).expect("partial process");
            accumulated_reply.extend(out.reply);
        }
        assert!(accumulated_reply.is_empty(), "no reply before last byte");

        let out = conn.process(&[full[full.len() - 1]]).expect("final byte");
        accumulated_reply.extend(out.reply);

        let resps = drain_responses(&accumulated_reply);
        assert_eq!(resps, vec![Response::Success]);
    }

    #[test]
    fn connection_drives_challenge_response_flow() {
        let factory = Arc::new(ScriptedFactory::new(vec![vec![
            PamStep::Challenge {
                kind: AuthMessageType::Secret,
                prompt: "password:".into(),
            },
            PamStep::Success {
                uid: 1000,
                gid: 1000,
            },
        ]]));
        let mut conn = Connection::new(factory);

        let create = encode(&Request::CreateSession {
            username: "alice".into(),
        })
        .unwrap();
        let out1 = conn.process(&create).unwrap();
        let r1 = drain_responses(&out1.reply);
        assert_eq!(
            r1,
            vec![Response::AuthMessage {
                auth_message_type: AuthMessageType::Secret,
                auth_message: "password:".into(),
            }]
        );

        let pmr = encode(&Request::PostAuthMessageResponse {
            response: Some("hunter2".into()),
        })
        .unwrap();
        let out2 = conn.process(&pmr).unwrap();
        let r2 = drain_responses(&out2.reply);
        assert_eq!(r2, vec![Response::Success]);
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

        let factory: Arc<dyn PamSessionFactory> = Arc::new(ScriptedFactory::new(vec![]));
        let mut conn = Connection::new(factory);

        // Read what the client sent, feed into Connection.
        let mut buf = [0u8; 256];
        let n = server_side.read(&mut buf).unwrap();
        let out = conn.process(&buf[..n]).unwrap();
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
