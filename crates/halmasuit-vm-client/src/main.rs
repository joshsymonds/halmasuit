//! `halmasuit-vm-client` — a small CLI that speaks halmasuit-greetd's
//! wire protocol from the client side.
//!
//! Purpose: drive halmasuit's `/run/halmasuit/greetd.sock` from VM
//! tests (and ad-hoc debugging sessions). Uses
//! [`halmasuit_greetd`]'s own wire types and codec, so the client
//! can't drift from the daemon — a protocol change forces both to
//! update in lockstep.
//!
//! Subcommands:
//! - `wait-for-socket SOCKET [--timeout SECONDS]` — poll until the
//!   socket file exists. Exit 0 on success, nonzero on timeout.
//! - `full-auth SOCKET USER --password-file FILE [...] [--cmd CMD
//!   [--cmd-arg ARG]...] [--env KEY=VAL]...` — full happy-path
//!   round-trip: send CreateSession, respond to each AuthMessage,
//!   send StartSession on Success.
//!
//! See `parse_argv` for the full grammar and exit-code conventions.

#![forbid(unsafe_code)]

use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::{Duration, Instant};

use halmasuit_greetd::{
    AuthMessageType, CodecError, ErrorType, MAX_MESSAGE_SIZE, Request, Response, encode, try_decode,
};
use thiserror::Error;

const DEFAULT_TIMEOUT_SECS: u64 = 30;
const POLL_INTERVAL: Duration = Duration::from_millis(50);

#[derive(Debug, Error)]
enum ClientError {
    #[error("usage: {0}")]
    Usage(String),
    #[error("I/O: {0}")]
    Io(#[from] std::io::Error),
    #[error("codec: {0}")]
    Codec(#[from] CodecError),
    #[error("auth failed: {0}")]
    AuthFailed(String),
    #[error("protocol error: {0}")]
    Protocol(String),
    #[error("timeout waiting for {what}")]
    Timeout { what: &'static str },
}

#[derive(Debug, PartialEq, Eq)]
enum Subcommand {
    WaitForSocket {
        socket: PathBuf,
        timeout: Duration,
    },
    FullAuth {
        socket: PathBuf,
        user: String,
        responses: Vec<String>,
        cmd: Vec<String>,
        env: Vec<String>,
        timeout: Duration,
    },
}

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(ClientError::AuthFailed(msg)) => {
            eprintln!("auth_failed: {msg}");
            ExitCode::from(2)
        }
        Err(ClientError::Protocol(msg)) => {
            eprintln!("protocol_error: {msg}");
            ExitCode::from(3)
        }
        Err(ClientError::Usage(msg)) => {
            eprintln!("{msg}");
            ExitCode::from(64) // EX_USAGE
        }
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::from(1)
        }
    }
}

fn run() -> Result<(), ClientError> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let cmd = parse_argv(&args)?;
    match cmd {
        Subcommand::WaitForSocket { socket, timeout } => wait_for_socket(&socket, timeout),
        Subcommand::FullAuth {
            socket,
            user,
            responses,
            cmd,
            env,
            timeout,
        } => full_auth(&socket, &user, responses, cmd, env, timeout),
    }
}

fn parse_argv(args: &[String]) -> Result<Subcommand, ClientError> {
    let mut it = args.iter();
    let sub = it.next().ok_or_else(|| ClientError::Usage(USAGE.into()))?;
    match sub.as_str() {
        "wait-for-socket" => {
            let socket = PathBuf::from(
                it.next()
                    .ok_or_else(|| ClientError::Usage("wait-for-socket: missing SOCKET".into()))?,
            );
            let mut timeout = Duration::from_secs(DEFAULT_TIMEOUT_SECS);
            while let Some(flag) = it.next() {
                match flag.as_str() {
                    "--timeout" => {
                        let v = it.next().ok_or_else(|| {
                            ClientError::Usage("--timeout: missing SECONDS".into())
                        })?;
                        timeout = parse_timeout(v)?;
                    }
                    other => return Err(ClientError::Usage(format!("unknown flag: {other}"))),
                }
            }
            Ok(Subcommand::WaitForSocket { socket, timeout })
        }
        "full-auth" => {
            let socket = PathBuf::from(
                it.next()
                    .ok_or_else(|| ClientError::Usage("full-auth: missing SOCKET".into()))?,
            );
            let user = it
                .next()
                .ok_or_else(|| ClientError::Usage("full-auth: missing USER".into()))?
                .clone();
            let mut responses: Vec<String> = Vec::new();
            let mut cmd: Vec<String> = Vec::new();
            let mut env: Vec<String> = Vec::new();
            let mut timeout = Duration::from_secs(DEFAULT_TIMEOUT_SECS);
            while let Some(flag) = it.next() {
                match flag.as_str() {
                    "--password-file" => {
                        let path = it.next().ok_or_else(|| {
                            ClientError::Usage("--password-file: missing PATH".into())
                        })?;
                        // Zeroize the initial-read buffer on drop —
                        // it holds the password plus any trailing
                        // newline. The trimmed copy pushed below is a
                        // separate small heap allocation; it's
                        // dropped at the end of `full_auth` when
                        // `responses` goes out of scope. Consistent
                        // with the broker's CLAUDE.md zeroize rule
                        // even though this is test-only.
                        let pw: zeroize::Zeroizing<String> = zeroize::Zeroizing::new(
                            std::fs::read_to_string(path).map_err(ClientError::Io)?,
                        );
                        responses.push(pw.trim_end_matches('\n').to_owned());
                    }
                    "--response" => {
                        let v = it.next().ok_or_else(|| {
                            ClientError::Usage("--response: missing VALUE".into())
                        })?;
                        responses.push(v.clone());
                    }
                    "--cmd" => {
                        let v = it
                            .next()
                            .ok_or_else(|| ClientError::Usage("--cmd: missing CMD".into()))?;
                        cmd.push(v.clone());
                    }
                    "--cmd-arg" => {
                        let v = it
                            .next()
                            .ok_or_else(|| ClientError::Usage("--cmd-arg: missing ARG".into()))?;
                        cmd.push(v.clone());
                    }
                    "--env" => {
                        let v = it
                            .next()
                            .ok_or_else(|| ClientError::Usage("--env: missing KEY=VAL".into()))?;
                        env.push(v.clone());
                    }
                    "--timeout" => {
                        let v = it.next().ok_or_else(|| {
                            ClientError::Usage("--timeout: missing SECONDS".into())
                        })?;
                        timeout = parse_timeout(v)?;
                    }
                    other => return Err(ClientError::Usage(format!("unknown flag: {other}"))),
                }
            }
            Ok(Subcommand::FullAuth {
                socket,
                user,
                responses,
                cmd,
                env,
                timeout,
            })
        }
        other => Err(ClientError::Usage(format!("unknown subcommand: {other}"))),
    }
}

fn parse_timeout(s: &str) -> Result<Duration, ClientError> {
    s.parse::<u64>()
        .map(Duration::from_secs)
        .map_err(|e| ClientError::Usage(format!("--timeout: {e}")))
}

fn wait_for_socket(socket: &Path, timeout: Duration) -> Result<(), ClientError> {
    let deadline = Instant::now() + timeout;
    loop {
        if socket.exists() {
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err(ClientError::Timeout {
                what: "socket file existence",
            });
        }
        std::thread::sleep(POLL_INTERVAL);
    }
}

/// Connect to a Unix-socket path or an abstract Linux socket. Paths
/// beginning with `@` are bound in the kernel's net-ns-scoped
/// abstract namespace (Phase B fromInitrd deployment).
fn connect_socket(socket: &Path) -> Result<UnixStream, ClientError> {
    use std::os::linux::net::SocketAddrExt;
    use std::os::unix::net::SocketAddr;

    let path_str = socket.to_string_lossy();
    if let Some(abstract_name) = path_str.strip_prefix('@') {
        let addr = SocketAddr::from_abstract_name(abstract_name.as_bytes())?;
        return UnixStream::connect_addr(&addr).map_err(ClientError::Io);
    }
    UnixStream::connect(socket).map_err(ClientError::Io)
}

fn full_auth(
    socket: &Path,
    user: &str,
    responses: Vec<String>,
    cmd: Vec<String>,
    env: Vec<String>,
    timeout: Duration,
) -> Result<(), ClientError> {
    let stream = connect_socket(socket)?;
    stream.set_read_timeout(Some(timeout))?;
    stream.set_write_timeout(Some(timeout))?;

    let mut client = Client::new(stream);
    let mut responses = responses.into_iter();

    client.send(&Request::CreateSession {
        username: user.to_owned(),
    })?;

    loop {
        let resp = client.recv()?;
        match resp {
            Response::AuthMessage {
                auth_message_type,
                auth_message: _,
            } => {
                let reply = match auth_message_type {
                    AuthMessageType::Secret => {
                        // Scripted response, or empty if exhausted.
                        Some(responses.next().unwrap_or_default())
                    }
                    AuthMessageType::Visible => {
                        // Scripted response, or fall back to the
                        // username (common case for "Login:" prompts).
                        Some(responses.next().unwrap_or_else(|| user.to_owned()))
                    }
                    AuthMessageType::Info | AuthMessageType::Error => None,
                };
                client.send(&Request::PostAuthMessageResponse { response: reply })?;
            }
            Response::Success => break,
            Response::Error {
                error_type,
                description,
            } => match error_type {
                ErrorType::AuthError => return Err(ClientError::AuthFailed(description)),
                ErrorType::Error => return Err(ClientError::Protocol(description)),
            },
        }
    }

    client.send(&Request::StartSession { cmd, env })?;
    match client.recv()? {
        Response::Success => Ok(()),
        Response::Error {
            error_type: ErrorType::AuthError,
            description,
        } => Err(ClientError::AuthFailed(description)),
        Response::Error {
            error_type: ErrorType::Error,
            description,
        } => Err(ClientError::Protocol(description)),
        Response::AuthMessage { .. } => Err(ClientError::Protocol(
            "unexpected AuthMessage after StartSession".into(),
        )),
    }
}

/// Tiny request/response framing helper around a UnixStream. Reads
/// into a growing buffer and feeds bytes to `try_decode` until a
/// complete message lands.
struct Client {
    stream: UnixStream,
    read_buf: Vec<u8>,
}

impl Client {
    const fn new(stream: UnixStream) -> Self {
        Self {
            stream,
            read_buf: Vec::new(),
        }
    }

    fn send(&mut self, request: &Request) -> Result<(), ClientError> {
        let bytes = encode(request)?;
        emit_io_log("tx", request);
        self.stream.write_all(&bytes)?;
        Ok(())
    }

    fn recv(&mut self) -> Result<Response, ClientError> {
        let mut buf = [0u8; 4096];
        loop {
            if let Some((msg, n)) = try_decode::<Response>(&self.read_buf)? {
                self.read_buf.drain(..n);
                emit_io_log("rx", &msg);
                return Ok(msg);
            }
            if self.read_buf.len() > (MAX_MESSAGE_SIZE as usize + 4) {
                return Err(ClientError::Protocol("server overran buffer".into()));
            }
            let n = self.stream.read(&mut buf)?;
            if n == 0 {
                return Err(ClientError::Protocol("server closed mid-message".into()));
            }
            self.read_buf.extend_from_slice(&buf[..n]);
        }
    }
}

fn emit_io_log<T: serde::Serialize>(direction: &str, msg: &T) {
    if let Ok(serialized) = serde_json::to_string(msg) {
        println!("{{\"direction\":\"{direction}\",\"msg\":{serialized}}}");
    }
}

const USAGE: &str = "usage:\n  \
    halmasuit-vm-client wait-for-socket SOCKET [--timeout SECONDS]\n  \
    halmasuit-vm-client full-auth SOCKET USER [--password-file FILE | --response VALUE]... \
    [--cmd CMD [--cmd-arg ARG]...] [--env KEY=VAL]... [--timeout SECONDS]";

// ── Tests ───────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::net::UnixListener;
    use std::sync::mpsc::{Receiver, channel};
    use std::thread;
    use tempfile::TempDir;

    // ── argv parsing ────────────────────────────────────────────────────

    #[test]
    fn parse_wait_for_socket_with_defaults() {
        let args = vec!["wait-for-socket".to_owned(), "/tmp/sock".to_owned()];
        let parsed = parse_argv(&args).unwrap();
        assert_eq!(
            parsed,
            Subcommand::WaitForSocket {
                socket: PathBuf::from("/tmp/sock"),
                timeout: Duration::from_secs(DEFAULT_TIMEOUT_SECS),
            }
        );
    }

    #[test]
    fn parse_wait_for_socket_with_timeout() {
        let args = vec![
            "wait-for-socket".to_owned(),
            "/tmp/sock".to_owned(),
            "--timeout".to_owned(),
            "5".to_owned(),
        ];
        let parsed = parse_argv(&args).unwrap();
        match parsed {
            Subcommand::WaitForSocket { timeout, .. } => {
                assert_eq!(timeout, Duration::from_secs(5));
            }
            other @ Subcommand::FullAuth { .. } => panic!("expected WaitForSocket, got {other:?}"),
        }
    }

    #[test]
    fn parse_full_auth_with_password_file_and_cmd() {
        let dir = TempDir::new().unwrap();
        let pw_file = dir.path().join("pw");
        std::fs::write(&pw_file, "hunter2\n").unwrap();
        let args = vec![
            "full-auth".to_owned(),
            "/tmp/sock".to_owned(),
            "alice".to_owned(),
            "--password-file".to_owned(),
            pw_file.to_str().unwrap().to_owned(),
            "--cmd".to_owned(),
            "niri".to_owned(),
            "--env".to_owned(),
            "XDG_SESSION_TYPE=wayland".to_owned(),
        ];
        let parsed = parse_argv(&args).unwrap();
        match parsed {
            Subcommand::FullAuth {
                socket,
                user,
                responses,
                cmd,
                env,
                ..
            } => {
                assert_eq!(socket, PathBuf::from("/tmp/sock"));
                assert_eq!(user, "alice");
                // password-file content with trailing newline stripped
                assert_eq!(responses, vec!["hunter2".to_owned()]);
                assert_eq!(cmd, vec!["niri".to_owned()]);
                assert_eq!(env, vec!["XDG_SESSION_TYPE=wayland".to_owned()]);
            }
            other @ Subcommand::WaitForSocket { .. } => panic!("expected FullAuth, got {other:?}"),
        }
    }

    #[test]
    fn parse_full_auth_supports_multiple_responses() {
        let args = vec![
            "full-auth".to_owned(),
            "/tmp/sock".to_owned(),
            "alice".to_owned(),
            "--response".to_owned(),
            "first".to_owned(),
            "--response".to_owned(),
            "second".to_owned(),
        ];
        let parsed = parse_argv(&args).unwrap();
        match parsed {
            Subcommand::FullAuth { responses, .. } => {
                assert_eq!(responses, vec!["first".to_owned(), "second".to_owned()]);
            }
            other @ Subcommand::WaitForSocket { .. } => panic!("expected FullAuth, got {other:?}"),
        }
    }

    #[test]
    fn parse_rejects_unknown_subcommand() {
        let args = vec!["weird".to_owned()];
        let err = parse_argv(&args).unwrap_err();
        assert!(matches!(err, ClientError::Usage(_)));
    }

    #[test]
    fn parse_rejects_unknown_flag() {
        let args = vec![
            "full-auth".to_owned(),
            "/tmp/sock".to_owned(),
            "alice".to_owned(),
            "--bogus".to_owned(),
        ];
        let err = parse_argv(&args).unwrap_err();
        assert!(matches!(err, ClientError::Usage(_)));
    }

    // ── mock-server round trip ──────────────────────────────────────────

    /// Spawn a one-shot mock greetd server on the given tempdir
    /// socket. The thread accepts one connection, plays the
    /// `script` of responses in order (one per received Request),
    /// closes after the script is exhausted, and returns the list of
    /// requests via the returned channel.
    fn spawn_mock_server(socket: &Path, script: Vec<Response>) -> Receiver<Vec<Request>> {
        let (ready_tx, ready_rx) = channel();
        let (done_tx, done_rx) = channel();
        let listener = UnixListener::bind(socket).unwrap();
        thread::spawn(move || {
            ready_tx.send(()).unwrap();
            let (mut stream, _) = listener.accept().unwrap();
            let mut received: Vec<Request> = Vec::new();
            let mut read_buf: Vec<u8> = Vec::new();
            let mut buf = [0u8; 4096];
            let mut script_iter = script.into_iter();
            'outer: loop {
                while let Some((req, n)) = try_decode::<Request>(&read_buf).expect("server decode")
                {
                    read_buf.drain(..n);
                    received.push(req);
                    if let Some(resp) = script_iter.next() {
                        let bytes = encode(&resp).expect("server encode");
                        stream.write_all(&bytes).expect("server write");
                    } else {
                        break 'outer;
                    }
                }
                match stream.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => read_buf.extend_from_slice(&buf[..n]),
                }
            }
            done_tx.send(received).unwrap();
        });
        ready_rx.recv().unwrap();
        done_rx
    }

    #[test]
    fn full_auth_happy_path_sends_and_receives_expected_messages() {
        let dir = TempDir::new().unwrap();
        let sock = dir.path().join("greetd.sock");

        let script = vec![
            Response::AuthMessage {
                auth_message_type: AuthMessageType::Secret,
                auth_message: "password:".into(),
            },
            Response::Success,
            Response::Success,
        ];
        let done = spawn_mock_server(&sock, script);

        full_auth(
            &sock,
            "alice",
            vec!["hunter2".to_owned()],
            vec!["niri".to_owned()],
            vec![],
            Duration::from_secs(5),
        )
        .expect("full_auth happy path");

        let received = done.recv_timeout(Duration::from_secs(5)).unwrap();
        assert_eq!(
            received,
            vec![
                Request::CreateSession {
                    username: "alice".into()
                },
                Request::PostAuthMessageResponse {
                    response: Some("hunter2".into())
                },
                Request::StartSession {
                    cmd: vec!["niri".into()],
                    env: vec![]
                },
            ]
        );
    }

    #[test]
    fn full_auth_wrong_password_yields_auth_failed_error() {
        let dir = TempDir::new().unwrap();
        let sock = dir.path().join("greetd.sock");

        let script = vec![
            Response::AuthMessage {
                auth_message_type: AuthMessageType::Secret,
                auth_message: "password:".into(),
            },
            Response::Error {
                error_type: ErrorType::AuthError,
                description: "bad password".into(),
            },
        ];
        let _done = spawn_mock_server(&sock, script);

        let r = full_auth(
            &sock,
            "alice",
            vec!["wrong".to_owned()],
            vec!["niri".to_owned()],
            vec![],
            Duration::from_secs(5),
        );
        match r {
            Err(ClientError::AuthFailed(desc)) => {
                assert_eq!(desc, "bad password");
            }
            other => panic!("expected AuthFailed, got {other:?}"),
        }
    }

    #[test]
    fn full_auth_protocol_error_classifies_correctly() {
        let dir = TempDir::new().unwrap();
        let sock = dir.path().join("greetd.sock");

        // Server sends Error{Error} (not AuthError) — protocol-level
        // misuse, distinct from a wrong-password rejection.
        let script = vec![Response::Error {
            error_type: ErrorType::Error,
            description: "protocol violation".into(),
        }];
        let _done = spawn_mock_server(&sock, script);

        let r = full_auth(
            &sock,
            "alice",
            vec!["whatever".to_owned()],
            vec!["niri".to_owned()],
            vec![],
            Duration::from_secs(5),
        );
        match r {
            Err(ClientError::Protocol(desc)) => {
                assert_eq!(desc, "protocol violation");
            }
            other => panic!("expected Protocol, got {other:?}"),
        }
    }

    #[test]
    fn wait_for_socket_returns_when_file_appears() {
        let dir = TempDir::new().unwrap();
        let sock = dir.path().join("appears.sock");
        let sock_for_thread = sock.clone();
        thread::spawn(move || {
            thread::sleep(Duration::from_millis(150));
            let _l = UnixListener::bind(&sock_for_thread).unwrap();
            // listener dropped at end of thread; socket file may stick
            // around briefly but that's fine — we only assert existence.
            thread::sleep(Duration::from_secs(2));
        });
        wait_for_socket(&sock, Duration::from_secs(2)).expect("wait_for_socket should succeed");
    }

    #[test]
    fn wait_for_socket_times_out_when_file_never_appears() {
        let dir = TempDir::new().unwrap();
        let sock = dir.path().join("never.sock");
        let r = wait_for_socket(&sock, Duration::from_millis(100));
        assert!(matches!(r, Err(ClientError::Timeout { .. })));
    }
}
