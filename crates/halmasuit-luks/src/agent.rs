//! systemd password-agent protocol.
//!
//! Reference: <https://systemd.io/PASSWORD_AGENTS/>
//!
//! systemd-cryptsetup (and other agents that need passwords from a
//! user) writes a request file at `/run/systemd/ask-password/ask.<RANDOM>`
//! with an INI-like body:
//!
//! ```text
//! [Ask]
//! PID=1234
//! Socket=/run/systemd/ask-password/sck.<RANDOM>
//! AcceptCached=0
//! Echo=0
//! NotAfter=0
//! Message=Please enter passphrase for disk root:
//! ```
//!
//! The password agent responds by sending a Unix datagram to the
//! `Socket=` path containing:
//!
//! - `+<passphrase>` for a single-password success response, OR
//! - `+<passphrase>\0<passphrase>\0...` for multiple (rare; LUKS is single)
//! - `-` for cancel
//!
//! No trailing newline.
//!
//! The kernel guarantees `SOCK_DGRAM` `sendto` is atomic; one syscall
//! delivers one message.

use std::io;
use std::os::unix::net::UnixDatagram;
use std::path::{Path, PathBuf};

use zeroize::Zeroizing;

/// A parsed ask-password request file.
#[derive(Debug)]
pub struct AskFile {
    /// Path to the response socket (`Socket=` field).
    pub response_socket: PathBuf,
    /// User-facing prompt text (`Message=` field). May be empty.
    #[cfg_attr(
        not(test),
        expect(
            dead_code,
            reason = "consumed by the future text-rendering follow-up (MVP shows solid-color surface, no rendered prompt text)"
        )
    )]
    pub message: String,
    /// Whether to echo characters as typed (`Echo=` field). Always
    /// `false` for LUKS; tracked so future non-secret prompts (e.g.,
    /// confirmation) display correctly.
    #[cfg_attr(
        not(test),
        expect(
            dead_code,
            reason = "consumed by the future text-rendering follow-up (LUKS path is always non-echo)"
        )
    )]
    pub echo: bool,
}

impl AskFile {
    /// Parse the INI body of an ask-password request.
    ///
    /// The format is a single `[Ask]` section with `Key=Value` pairs.
    /// Unknown keys are ignored (forward-compatibility with future
    /// systemd extensions).
    pub fn parse(body: &str) -> io::Result<Self> {
        let mut socket: Option<PathBuf> = None;
        let mut message = String::new();
        let mut echo = false;
        for line in body.lines() {
            let line = line.trim();
            // Tolerate leading section header and blank lines.
            if line.is_empty() || line.starts_with('[') || line.starts_with('#') {
                continue;
            }
            let Some((key, value)) = line.split_once('=') else {
                continue;
            };
            match key.trim() {
                "Socket" => socket = Some(PathBuf::from(value.trim())),
                "Message" => value.trim().clone_into(&mut message),
                "Echo" => echo = matches!(value.trim(), "1" | "yes" | "true"),
                _ => {}
            }
        }
        let response_socket = socket.ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "ask-password file missing required `Socket=` field",
            )
        })?;
        Ok(Self {
            response_socket,
            message,
            echo,
        })
    }

    /// Read and parse an ask-password file from disk.
    pub fn read(path: &Path) -> io::Result<Self> {
        let body = std::fs::read_to_string(path)?;
        Self::parse(&body)
    }

    /// Send a successful passphrase response to the request's socket.
    ///
    /// The passphrase is zeroized after the send (whether successful or
    /// not). The caller's `Zeroizing<Vec<u8>>` continues to manage the
    /// original buffer's lifetime, but the bytes we construct here for
    /// the wire frame are wiped immediately.
    pub fn send_passphrase(&self, passphrase: &[u8]) -> io::Result<()> {
        let mut frame = Zeroizing::new(Vec::with_capacity(passphrase.len() + 1));
        frame.push(b'+');
        frame.extend_from_slice(passphrase);
        send_datagram(&self.response_socket, &frame)
    }

    /// Send a cancel response (`-`) to the request's socket. The user
    /// pressed ESC or otherwise abandoned the prompt; the agent should
    /// proceed to the next attempt or fail the unlock.
    pub fn send_cancel(&self) -> io::Result<()> {
        send_datagram(&self.response_socket, b"-")
    }
}

fn send_datagram(socket_path: &Path, bytes: &[u8]) -> io::Result<()> {
    let socket = UnixDatagram::unbound()?;
    socket.send_to(bytes, socket_path)?;
    Ok(())
}

/// Scan `/run/systemd/ask-password/` for outstanding `ask.*` request
/// files. Returns them in directory-iteration order (typically the
/// kernel's underlying file-creation order on a tmpfs). systemd
/// guarantees the dir exists when the agent system is in use; we
/// tolerate ENOENT as "no requests pending" since the dir may not
/// exist before the first request lands.
pub fn outstanding_requests(ask_dir: &Path) -> io::Result<Vec<PathBuf>> {
    let mut out = Vec::new();
    let read_dir = match std::fs::read_dir(ask_dir) {
        Ok(rd) => rd,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(out),
        Err(e) => return Err(e),
    };
    for entry in read_dir {
        let entry = entry?;
        let name = entry.file_name();
        if name.to_string_lossy().starts_with("ask.") {
            out.push(entry.path());
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::net::UnixDatagram as TestUnixDatagram;

    #[test]
    fn parse_valid_ask_extracts_required_fields() {
        let body = "[Ask]\n\
                    PID=1234\n\
                    Socket=/run/systemd/ask-password/sck.ABCDEF\n\
                    AcceptCached=0\n\
                    Echo=0\n\
                    NotAfter=0\n\
                    Message=Please enter passphrase for disk root:\n";
        let parsed = AskFile::parse(body).expect("parse should succeed");
        assert_eq!(
            parsed.response_socket,
            PathBuf::from("/run/systemd/ask-password/sck.ABCDEF")
        );
        assert_eq!(parsed.message, "Please enter passphrase for disk root:");
        assert!(!parsed.echo);
    }

    #[test]
    fn parse_echo_true_variants_set_echo_flag() {
        for echo_value in ["1", "yes", "true"] {
            let body = format!("[Ask]\nSocket=/x\nEcho={echo_value}\n");
            let parsed = AskFile::parse(&body).expect("parse");
            assert!(parsed.echo, "Echo={echo_value} must set echo=true");
        }
    }

    #[test]
    fn parse_missing_socket_field_is_an_error() {
        let body = "[Ask]\nMessage=no socket\n";
        let err = AskFile::parse(body).expect_err("parse must reject");
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn parse_tolerates_unknown_keys_for_forward_compat() {
        let body = "[Ask]\nSocket=/x\nFutureSystemdKey=ignored\n";
        let parsed = AskFile::parse(body).expect("parse must accept unknown keys");
        assert_eq!(parsed.response_socket, PathBuf::from("/x"));
    }

    #[test]
    fn parse_handles_comments_and_blank_lines() {
        let body = "\n# a comment\n[Ask]\n\nSocket=/x\nMessage=hi\n";
        let parsed = AskFile::parse(body).expect("parse must accept comments");
        assert_eq!(parsed.message, "hi");
    }

    #[test]
    fn outstanding_requests_returns_only_ask_prefixed_files() {
        let tmp = tempfile::tempdir().expect("tempdir");
        // ask.* matches, sck.* does not, neither does a stray .txt.
        std::fs::write(tmp.path().join("ask.AAA"), "").expect("write ask file");
        std::fs::write(tmp.path().join("ask.BBB"), "").expect("write ask file");
        std::fs::write(tmp.path().join("sck.CCC"), "").expect("write sck file");
        std::fs::write(tmp.path().join("stray.txt"), "").expect("write stray");
        let mut found = outstanding_requests(tmp.path()).expect("scan");
        found.sort();
        assert_eq!(found.len(), 2);
        assert_eq!(found[0].file_name().unwrap(), "ask.AAA");
        assert_eq!(found[1].file_name().unwrap(), "ask.BBB");
    }

    #[test]
    fn outstanding_requests_returns_empty_when_dir_missing() {
        let path = std::path::Path::new("/this/dir/does/not/exist/ever");
        let found = outstanding_requests(path).expect("missing dir must be Ok(empty)");
        assert!(found.is_empty());
    }

    #[test]
    fn send_passphrase_writes_plus_prefix_and_passphrase() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let sck_path = tmp.path().join("sck.TEST");
        let listener = TestUnixDatagram::bind(&sck_path).expect("bind response socket");

        let ask = AskFile {
            response_socket: sck_path,
            message: String::new(),
            echo: false,
        };
        ask.send_passphrase(b"hunter2").expect("send_passphrase");

        let mut buf = [0u8; 64];
        let (n, _) = listener.recv_from(&mut buf).expect("recv");
        assert_eq!(&buf[..n], b"+hunter2");
    }

    #[test]
    fn send_cancel_writes_minus_byte() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let sck_path = tmp.path().join("sck.TEST");
        let listener = TestUnixDatagram::bind(&sck_path).expect("bind response socket");

        let ask = AskFile {
            response_socket: sck_path,
            message: String::new(),
            echo: false,
        };
        ask.send_cancel().expect("send_cancel");

        let mut buf = [0u8; 4];
        let (n, _) = listener.recv_from(&mut buf).expect("recv");
        assert_eq!(&buf[..n], b"-");
    }

    #[test]
    fn read_from_disk_round_trips_through_parse() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let ask_path = tmp.path().join("ask.XYZ");
        std::fs::write(
            &ask_path,
            "[Ask]\nSocket=/run/systemd/ask-password/sck.XYZ\nMessage=Test\n",
        )
        .expect("write ask file");
        let parsed = AskFile::read(&ask_path).expect("read+parse");
        assert_eq!(parsed.message, "Test");
    }
}
