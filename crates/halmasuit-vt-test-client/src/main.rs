//! Epic #71 R1.4 — VT-switching VM test client.
//!
//! Connects to the live `halmasuit-session` broker, drives the
//! `RequestVtSwitch` dance end-to-end, logs a single `VERDICT:` line.
//! Test-only; not deployed in production.
//!
//! ## Why a separate crate, not `use halmasuit::vt_switch`
//!
//! `halmasuit` is a binary crate. Exposing its private `vt_switch`
//! module to another crate would require a `lib.rs` refactor for
//! marginal value: R1.3's unit tests already pin the dance's
//! protocol shape against a real socketpair.
//!
//! This client reimplements the dance inline using `halmasuit-session-ipc`
//! (the frozen wire crate) and `nix` socket primitives. An independent
//! implementation on the other side of the wire is a STRONGER
//! protocol test — a shared driver could hide a wire-format bug on
//! both sides. Same pattern as `halmasuit-vm-client` (which drives
//! the greetd socket using `halmasuit-greetd`'s wire types
//! independently, not via the compositor's protocol machine).
//!
//! ## Modes
//!
//! - `--mode happy`: full dance. Send `RequestVtSwitch{N}` → receive
//!   `VtSwitchPrepare(fd)` → setsid + TIOCSCTTY + VT_SETMODE on the
//!   inherited fd → send `VtSwitchMasterDropped` → receive
//!   `VtSwitchActivated`. Exit 0; logs `VERDICT: ACTIVATED`.
//!
//! - `--mode timeout`: receives the Prepare(fd) but never sends the
//!   `VtSwitchMasterDropped` ack. Expects the broker's 5s watchdog to
//!   emit `VtSwitchRejected { reason: MasterDropTimeout }`. This is
//!   the systemd #21388 invariant: broker MUST FAIL, MUST NOT fire
//!   `VT_ACTIVATE`. The test script verifies this at the kernel-state
//!   level by checking `/sys/class/tty/tty0/active` did not change.

use std::ffi::c_int;
use std::io::{IoSlice, IoSliceMut, Write};
use std::os::fd::{AsFd, AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::path::PathBuf;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use halmasuit_session_ipc::{
    BrokerToCompositor, CompositorToBroker, VtSwitchRejectReason, encode, try_decode,
};
use nix::poll::{PollFd, PollFlags, PollTimeout, poll};
use nix::sys::socket::{
    AddressFamily, ControlMessage, ControlMessageOwned, MsgFlags, SockFlag, SockType, UnixAddr,
    connect, recvmsg, sendmsg, socket,
};

const TIOCSCTTY: u64 = 0x540E;
const VT_SETMODE: u64 = 0x5602;
const VT_PROCESS: u8 = 0x01;

#[repr(C)]
struct VtMode {
    mode: u8,
    waitv: u8,
    relsig: u16,
    acqsig: u16,
    frsig: u16,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    Happy,
    Timeout,
}

struct Args {
    broker_socket: PathBuf,
    target_vt: u8,
    mode: Mode,
    log: PathBuf,
}

fn parse_args() -> Result<Args> {
    let mut broker_socket: Option<PathBuf> = None;
    let mut target_vt: Option<u8> = None;
    let mut mode: Option<Mode> = None;
    let mut log: Option<PathBuf> = None;

    let mut argv = std::env::args().skip(1);
    while let Some(arg) = argv.next() {
        match arg.as_str() {
            "--broker-socket" => {
                broker_socket = Some(PathBuf::from(
                    argv.next().context("--broker-socket needs a value")?,
                ));
            }
            "--target-vt" => {
                target_vt = Some(
                    argv.next()
                        .context("--target-vt needs a value")?
                        .parse()
                        .context("--target-vt must be a u8")?,
                );
            }
            "--mode" => {
                mode = Some(
                    match argv.next().context("--mode needs a value")?.as_str() {
                        "happy" => Mode::Happy,
                        "timeout" => Mode::Timeout,
                        other => bail!("--mode must be 'happy' or 'timeout', got {other}"),
                    },
                );
            }
            "--log" => {
                log = Some(PathBuf::from(argv.next().context("--log needs a value")?));
            }
            other => bail!("unknown arg: {other}"),
        }
    }

    Ok(Args {
        broker_socket: broker_socket.context("--broker-socket required")?,
        target_vt: target_vt.context("--target-vt required")?,
        mode: mode.context("--mode required")?,
        log: log.context("--log required")?,
    })
}

/// Append-on-write log: each call writes one line plus newline,
/// fsyncs, drops the handle. The test script tails this file via
/// `wait_until_succeeds("grep VERDICT …")` so we want every
/// intermediate event durable on disk immediately.
struct Log {
    path: PathBuf,
}

impl Log {
    fn new(path: PathBuf) -> Result<Self> {
        // Truncate at startup so repeated test runs don't pollute the
        // previous run's log.
        std::fs::OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .open(&path)
            .with_context(|| format!("truncate log {}", path.display()))?;
        Ok(Self { path })
    }

    fn line(&self, msg: &str) {
        let r: Result<()> = (|| {
            let mut f = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&self.path)?;
            writeln!(f, "{msg}")?;
            f.sync_data()?;
            Ok(())
        })();
        if let Err(e) = r {
            eprintln!("log write failed: {e}");
        }
        eprintln!("{msg}");
    }
}

fn main() -> Result<()> {
    let args = parse_args()?;
    let log = Log::new(args.log.clone())?;

    log.line(&format!(
        "START broker_socket={} target_vt={} mode={:?}",
        args.broker_socket.display(),
        args.target_vt,
        args.mode,
    ));

    match run(&args, &log) {
        Ok(()) => Ok(()),
        Err(e) => {
            // Ensure SOMETHING gets logged as VERDICT on every failure
            // path so the test script's grep can distinguish "client
            // bailed" from "client hung". This is in addition to the
            // mode-specific VERDICT lines logged on the success path.
            log.line(&format!("VERDICT: ERROR {e:#}"));
            Err(e)
        }
    }
}

fn run(args: &Args, log: &Log) -> Result<()> {
    let chan = connect_broker(&args.broker_socket)
        .with_context(|| format!("connect broker socket {}", args.broker_socket.display()))?;
    log.line("CONNECTED to broker");

    send_frame(
        &chan,
        &CompositorToBroker::RequestVtSwitch {
            target_vt: args.target_vt,
        },
        None,
    )
    .context("send RequestVtSwitch")?;
    log.line(&format!(
        "SENT RequestVtSwitch target_vt={}",
        args.target_vt
    ));

    // Wait for the broker's first reply. Could be VtSwitchPrepare(fd)
    // or VtSwitchRejected.
    let (frame, prepare_fd) = recv_frame(&chan, Duration::from_secs(10))
        .context("recv first reply (Prepare or Rejected)")?;

    match frame {
        BrokerToCompositor::VtSwitchPrepare => {
            let prepare_fd = prepare_fd
                .ok_or_else(|| anyhow!("broker sent VtSwitchPrepare without SCM_RIGHTS fd"))?;
            log.line(&format!(
                "RECV VtSwitchPrepare fd={}",
                prepare_fd.as_raw_fd()
            ));

            match args.mode {
                Mode::Happy => happy_path(&chan, prepare_fd, log),
                Mode::Timeout => timeout_path(&chan, prepare_fd, log),
            }
        }
        BrokerToCompositor::VtSwitchRejected { reason } => {
            log.line(&format!("RECV early VtSwitchRejected reason={reason:?}"));
            log.line(&format!("VERDICT: REJECTED reason={reason:?}"));
            // Early rejection is a failure for both modes — we expected
            // the broker to send Prepare first.
            bail!("broker rejected before Prepare: {reason:?}");
        }
        other => bail!("broker sent unexpected first frame: {other:?}"),
    }
}

#[allow(
    clippy::needless_pass_by_value,
    reason = "the OwnedFd must be held alive by THIS function for the duration of the ioctls + final ack — passing by value gives drop-at-function-end semantics, which is what we want"
)]
fn happy_path(chan: &OwnedFd, prepare_fd: OwnedFd, log: &Log) -> Result<()> {
    // 0. Block SIGUSR1/SIGUSR2 BEFORE VT_SETMODE. Once VT_SETMODE
    //    PROCESS is set on tty2 with relsig=SIGUSR1/acqsig=SIGUSR2,
    //    the kernel can deliver those signals at any moment (e.g.
    //    immediately when VT_ACTIVATE runs on the broker side and
    //    the kernel switches to tty2 → SIGUSR2 to us). Without a
    //    handler the default action is termination — Phase 0's
    //    probe documents the same precondition. Production R2.1
    //    install a signalfd source here; this test doesn't need to
    //    drain the signals (the kernel switch completes regardless
    //    of whether we VT_RELDISP-ack acqsig), so blocking is
    //    sufficient to survive.
    block_vt_signals(log).context("block SIGUSR1/SIGUSR2")?;

    // 1. setsid + TIOCSCTTY + VT_SETMODE on the inherited fd. Per
    //    Phase 0's verdict, no CAP_SYS_TTY_CONFIG required: TIOCSCTTY
    //    makes the fd the controlling TTY, satisfying the kernel's
    //    perm check for subsequent VT ioctls.
    setsid_best_effort(log);
    tiocsctty(prepare_fd.as_raw_fd()).context("TIOCSCTTY")?;
    log.line("TIOCSCTTY: ok");
    vt_setmode_process(prepare_fd.as_raw_fd(), libc::SIGUSR1, libc::SIGUSR2)
        .context("VT_SETMODE PROCESS")?;
    log.line("VT_SETMODE PROCESS: ok");

    // 2. Send MasterDropped ack.
    send_frame(chan, &CompositorToBroker::VtSwitchMasterDropped, None)
        .context("send VtSwitchMasterDropped")?;
    log.line("SENT VtSwitchMasterDropped");

    // 3. Wait for Activated or Rejected.
    let (frame, _fd) = recv_frame(chan, Duration::from_secs(10))
        .context("recv final reply (Activated or Rejected)")?;
    match frame {
        BrokerToCompositor::VtSwitchActivated => {
            log.line("RECV VtSwitchActivated");
            log.line("VERDICT: ACTIVATED");
            Ok(())
        }
        BrokerToCompositor::VtSwitchRejected { reason } => {
            log.line(&format!("RECV final VtSwitchRejected reason={reason:?}"));
            log.line(&format!("VERDICT: REJECTED reason={reason:?}"));
            bail!("broker rejected post-MasterDropped: {reason:?}");
        }
        other => bail!("broker sent unexpected final frame: {other:?}"),
    }
}

#[allow(
    clippy::needless_pass_by_value,
    reason = "the OwnedFd must be held alive by THIS function for the duration of the broker's timeout window — passing by value gives drop-at-function-end semantics"
)]
fn timeout_path(chan: &OwnedFd, prepare_fd: OwnedFd, log: &Log) -> Result<()> {
    // Block SIGUSR1/SIGUSR2 defensively in case the broker
    // (incorrectly) fires VT_ACTIVATE despite the timeout — without
    // this, an inverted bug would also kill our process via SIGUSR2.
    // Blocking ensures we live long enough to OBSERVE the bug and
    // report it via VERDICT rather than just dying.
    block_vt_signals(log).context("block SIGUSR1/SIGUSR2")?;

    log.line(
        "TIMEOUT-MODE: deliberately NOT sending VtSwitchMasterDropped; \
         waiting for broker watchdog to fire VtSwitchRejected{MasterDropTimeout}",
    );

    // Don't call TIOCSCTTY / VT_SETMODE either — if the broker were
    // (incorrectly) to fire VT_ACTIVATE despite the timeout, doing
    // those ioctls would mask the bug because the kernel would
    // accept the switch. Holding the fd open without ioctls is the
    // strictest test.
    let _keep_fd = prepare_fd;

    // The broker's VT_MASTER_DROP_TIMEOUT is 5s. Wait up to 12s for
    // its Rejected reply — gives plenty of margin without making
    // the test crawl.
    let started = Instant::now();
    let (frame, _fd) = recv_frame(chan, Duration::from_secs(12))
        .context("recv timeout-path reply (expected Rejected{MasterDropTimeout})")?;
    let elapsed = started.elapsed();

    match frame {
        BrokerToCompositor::VtSwitchRejected {
            reason: VtSwitchRejectReason::MasterDropTimeout,
        } => {
            log.line(&format!(
                "RECV VtSwitchRejected reason=MasterDropTimeout (after {}ms)",
                elapsed.as_millis()
            ));
            // The broker's timeout is 5s. Sanity-check that the reply
            // arrived at roughly that interval (between 4s and 12s).
            // Tighter bound risks flakiness; looser bound lets a
            // truly-broken broker that emits Rejected for the wrong
            // reason pass.
            if elapsed < Duration::from_secs(4) {
                bail!(
                    "broker emitted MasterDropTimeout suspiciously early ({}ms < 4000ms); \
                     this suggests the broker is NOT actually waiting on the master-drop ack",
                    elapsed.as_millis()
                );
            }
            log.line("VERDICT: REJECTED reason=MasterDropTimeout");
            Ok(())
        }
        BrokerToCompositor::VtSwitchRejected { reason } => {
            log.line(&format!(
                "VERDICT: REJECTED reason={reason:?} (UNEXPECTED — wanted MasterDropTimeout)"
            ));
            bail!("broker rejected with wrong reason: {reason:?} (wanted MasterDropTimeout)");
        }
        BrokerToCompositor::VtSwitchActivated => {
            log.line("VERDICT: ACTIVATED (UNEXPECTED — broker fired VT_ACTIVATE on timeout!)");
            bail!(
                "load-bearing failure: broker emitted VtSwitchActivated AFTER timing out on \
                 master-drop ack. This is the systemd #21388 bug class — broker MUST FAIL on \
                 timeout, never bypass safety to 'make the request go through.'"
            );
        }
        other => bail!("broker sent unexpected frame on timeout path: {other:?}"),
    }
}

// ── Socket primitives ─────────────────────────────────────────────

/// Open a `SOCK_SEQPACKET` client socket and connect to the broker.
/// Path-or-abstract: `@name` selects the kernel's net-ns-scoped
/// abstract socket (matches the fromInitrd deployment shape).
fn connect_broker(sock_path: &std::path::Path) -> Result<OwnedFd> {
    let fd = socket(
        AddressFamily::Unix,
        SockType::SeqPacket,
        SockFlag::SOCK_CLOEXEC,
        None,
    )
    .context("socket(AF_UNIX, SOCK_SEQPACKET)")?;
    let path_str = sock_path.to_string_lossy();
    let addr = if let Some(abstract_name) = path_str.strip_prefix('@') {
        UnixAddr::new_abstract(abstract_name.as_bytes())
            .with_context(|| format!("abstract addr @{abstract_name}"))?
    } else {
        UnixAddr::new(sock_path).with_context(|| format!("addr {}", sock_path.display()))?
    };
    connect(fd.as_raw_fd(), &addr).context("connect")?;
    Ok(fd)
}

fn send_frame(chan: &OwnedFd, msg: &CompositorToBroker, fd: Option<RawFd>) -> Result<()> {
    let bytes = encode(msg).context("encode")?;
    let iov = [IoSlice::new(&bytes)];
    let raw_fd_storage = [fd.unwrap_or(-1)];
    let cmsgs: &[ControlMessage<'_>] = if fd.is_some() {
        &[ControlMessage::ScmRights(&raw_fd_storage)]
    } else {
        &[]
    };
    let n =
        sendmsg::<()>(chan.as_raw_fd(), &iov, cmsgs, MsgFlags::empty(), None).context("sendmsg")?;
    if n != bytes.len() {
        bail!("short sendmsg: wrote {n}, wanted {}", bytes.len());
    }
    Ok(())
}

fn recv_frame(chan: &OwnedFd, timeout: Duration) -> Result<(BrokerToCompositor, Option<OwnedFd>)> {
    let timeout_ms =
        u16::try_from(timeout.as_millis().min(u128::from(u16::MAX))).expect("clamped to u16::MAX");
    let mut pollfd = [PollFd::new(chan.as_fd(), PollFlags::POLLIN)];
    let n = poll(&mut pollfd, PollTimeout::from(timeout_ms)).context("poll")?;
    if n == 0 {
        bail!("recv_frame: broker silent for {}ms", timeout.as_millis());
    }

    let mut buf = vec![0_u8; 4 + 4096];
    let mut iov = [IoSliceMut::new(&mut buf)];
    let mut cmsg = nix::cmsg_space!(RawFd);
    let r = recvmsg::<()>(
        chan.as_raw_fd(),
        &mut iov,
        Some(&mut cmsg),
        MsgFlags::empty(),
    )
    .context("recvmsg")?;
    let nread = r.bytes;
    if nread == 0 {
        bail!("broker closed connection");
    }

    // Adopt any received fd immediately (so an error past this point
    // doesn't leak a kernel-dup'd fd).
    let mut fds: Vec<OwnedFd> = Vec::new();
    for c in r.cmsgs().context("cmsgs")? {
        if let ControlMessageOwned::ScmRights(raws) = c {
            for raw in raws {
                // SAFETY: raw was just produced by recvmsg(SCM_RIGHTS)
                // in this process; nothing else owns it.
                #[expect(
                    unsafe_code,
                    reason = "adopt kernel-fresh SCM_RIGHTS fd into OwnedFd so it closes on every error path"
                )]
                let owned = unsafe { OwnedFd::from_raw_fd(raw) };
                fds.push(owned);
            }
        }
    }
    if fds.len() > 1 {
        bail!("broker sent {} fds, expected at most 1", fds.len());
    }
    let fd = fds.into_iter().next();

    let (msg, consumed) = try_decode::<BrokerToCompositor>(&buf[..nread])
        .context("decode")?
        .ok_or_else(|| anyhow!("partial frame: nread={nread}"))?;
    if consumed != nread {
        bail!("frame length mismatch: consumed={consumed}, nread={nread}");
    }
    Ok((msg, fd))
}

// ── ioctls ────────────────────────────────────────────────────────

fn block_vt_signals(log: &Log) -> Result<()> {
    use nix::sys::signal::{SigSet, Signal};
    let mut mask = SigSet::empty();
    mask.add(Signal::SIGUSR1);
    mask.add(Signal::SIGUSR2);
    mask.thread_block().context("sigprocmask block")?;
    log.line("SIGNALS: SIGUSR1+SIGUSR2 blocked");
    Ok(())
}

fn setsid_best_effort(log: &Log) {
    // EPERM means we're already a session leader (systemd typically
    // setsids each unit's main process), which is exactly what we
    // want. Anything else is a real error worth logging.
    match nix::unistd::setsid() {
        Ok(_) => log.line("SETSID: ok"),
        Err(nix::errno::Errno::EPERM) => log.line("SETSID: already-session-leader (EPERM)"),
        Err(e) => log.line(&format!("SETSID: warning {e}")),
    }
}

fn tiocsctty(fd: RawFd) -> Result<()> {
    // SAFETY: `ioctl(fd, TIOCSCTTY, 0)` reads only the request number
    // and an integer arg; no memory dereference. Errno is checked.
    #[expect(unsafe_code, reason = "raw TIOCSCTTY ioctl via libc")]
    let rc = unsafe { libc::ioctl(fd, TIOCSCTTY as _, 0_i32) };
    if rc < 0 {
        let errno = std::io::Error::last_os_error();
        bail!("TIOCSCTTY failed: {errno}");
    }
    Ok(())
}

fn vt_setmode_process(fd: RawFd, relsig: c_int, acqsig: c_int) -> Result<()> {
    let mode = VtMode {
        mode: VT_PROCESS,
        waitv: 0,
        relsig: u16::try_from(relsig).context("relsig fits u16")?,
        acqsig: u16::try_from(acqsig).context("acqsig fits u16")?,
        frsig: 0,
    };
    // SAFETY: ioctl reads the vt_mode struct by pointer; struct is a
    // valid stack allocation of the correct repr(C) layout for
    // `struct vt_mode` from `linux/vt.h`. Errno is checked.
    #[expect(unsafe_code, reason = "raw VT_SETMODE ioctl via libc")]
    let rc = unsafe {
        libc::ioctl(
            fd,
            VT_SETMODE as _,
            std::ptr::addr_of!(mode).cast::<libc::c_void>(),
        )
    };
    if rc < 0 {
        let errno = std::io::Error::last_os_error();
        bail!("VT_SETMODE failed: {errno}");
    }
    Ok(())
}
