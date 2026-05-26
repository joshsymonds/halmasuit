// halmasuit-shutdown-probe — research probe for halmasuit Epic #47 R2.
//
// This crate is a runnable proof, analogous to `drm-master-probe`.
// It empirically validates the load-bearing mechanics the production
// `halmasuit` binary depends on for wallpaper continuity through
// systemd shutdown. It is NOT production code: it's gated by VM tests,
// not by `just check`'s unit-test sweep, and its assertions answer
// architectural questions rather than implement the production contract.
//
// Phase 0 (THIS file): the probe runs as a rootfs systemd unit with
// `SurviveFinalKillSignal=yes`. It writes timestamped heartbeats to
// `/dev/kmsg` every 100ms and ignores SIGTERM / SIGINT / SIGHUP. The
// VM test triggers `systemctl poweroff`, waits for the VM to halt, then
// reads qemu's serial-console capture and asserts that at least one
// heartbeat was emitted AFTER systemd-shutdown's "Sending SIGKILL to
// remaining processes" marker. That marker is the last-line-of-defense
// kill before halt; heartbeats past it prove the directive worked on
// the SHUTDOWN-direction kill spree (drm-master-probe Phase 2 already
// proved it for the BOOT-direction one).
//
// Future sub-phases (separate tasks in Epic #47):
//   * Phase 1: + `boot.initrd.systemd.shutdownRamfs.storePaths` includes
//     this binary so the rootfs unmount doesn't pull mmap'd code out.
//     Asserts the SAME pid keeps emitting heartbeats post-pivot.
//   * Phase 2: + opens `/dev/dri/card0`, takes DRM master, paints a
//     solid color via dumb buffer. Asserts the SAME DRM master fd
//     remains valid post-pivot and a paint after pivot lands on screen.
//
// The probe uses /dev/kmsg because it bypasses journald (journald itself
// is shutting down during the kill spree). /dev/kmsg writes go to the
// kernel ring buffer, which surfaces on the serial console (qemu's
// `-serial stdio`) until the kernel halts.
//
// Production wallpaper-continuity code lives in `halmasuit` (production
// crate), written against the patterns these phases validate — not
// lifted from this probe.

#![deny(unsafe_code)]

use std::io::Write;
use std::os::fd::AsFd;
use std::path::Path;
use std::process;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use nix::poll::{PollFd, PollFlags, PollTimeout, poll};
use nix::sys::signal::{SigSet, Signal};
use nix::sys::signalfd::{SfdFlags, SignalFd};

/// Process start instant; heartbeat seq is monotonic from boot of THIS
/// process, used by the test to confirm the same process kept emitting
/// past the kill marker (not a new process re-entering after a respawn).
static HEARTBEAT_SEQ: AtomicU64 = AtomicU64::new(0);

/// Run subcommand router. Phase 0 is the only sub-phase implemented;
/// `phase1` / `phase2` panic until those sub-tasks land. Keeping the
/// router in place documents the planned shape.
fn main() {
    let mut args = std::env::args().skip(1);
    let phase = args.next();
    match phase.as_deref() {
        Some("phase0") => phase0(),
        Some(other) => {
            eprintln!(
                "halmasuit-shutdown-probe: unknown phase {other:?}; \
                 only `phase0` is implemented in this build"
            );
            process::exit(2);
        }
        None => {
            eprintln!("halmasuit-shutdown-probe: usage: halmasuit-shutdown-probe <phase0>");
            process::exit(2);
        }
    }
}

/// Phase 0: heartbeat to `/dev/kmsg` + ignore SIGTERM/SIGINT/SIGHUP.
/// Only SIGKILL exits this process.
fn phase0() {
    let pid = process::id();
    let mut kmsg = open_kmsg();
    write_kmsg(
        &mut kmsg,
        &format!("started pid={pid} (phase=phase0, ignoring SIGTERM/INT/HUP)"),
    );

    // Block the signals we want to observe via signalfd. The block must
    // happen BEFORE signalfd creation so the signals don't race the
    // default disposition between block + signalfd registration.
    let mut mask = SigSet::empty();
    mask.add(Signal::SIGTERM);
    mask.add(Signal::SIGINT);
    mask.add(Signal::SIGHUP);
    mask.thread_block().expect("sigprocmask block failed");

    let sfd = SignalFd::with_flags(&mask, SfdFlags::SFD_CLOEXEC | SfdFlags::SFD_NONBLOCK)
        .expect("signalfd create failed");

    // 100ms tick interval. Used by the test to count post-kill-spree
    // heartbeats. Coarse enough that systemd-shutdown's serial log
    // doesn't drown them.
    let tick = Duration::from_millis(100);
    let mut next_tick = Instant::now() + tick;

    loop {
        // poll(signalfd) with timeout = remaining time until next tick.
        // Returns early if a signal arrives; otherwise the tick fires.
        let now = Instant::now();
        let wait = next_tick.saturating_duration_since(now);
        let timeout_ms = i32::try_from(wait.as_millis().min(i32::MAX as u128)).unwrap_or(i32::MAX);
        let mut pollfds = [PollFd::new(sfd.as_fd(), PollFlags::POLLIN)];
        match poll(
            &mut pollfds,
            PollTimeout::from(u16::try_from(timeout_ms).unwrap_or(0)),
        ) {
            Ok(0) => {
                // Timeout: emit heartbeat and advance tick.
                let seq = HEARTBEAT_SEQ.fetch_add(1, Ordering::Relaxed) + 1;
                write_kmsg(&mut kmsg, &format!("heartbeat seq={seq} pid={pid}"));
                next_tick += tick;
                // If we've fallen behind by more than one tick (slow
                // I/O during shutdown), resync to "now + tick" so we
                // don't spin emitting catch-up heartbeats.
                let now2 = Instant::now();
                if next_tick < now2 {
                    next_tick = now2 + tick;
                }
            }
            Ok(_) => {
                // Signal pending — drain signalfd, log, ignore.
                match sfd.read_signal() {
                    Ok(Some(siginfo)) => {
                        let signo = siginfo.ssi_signo;
                        write_kmsg(
                            &mut kmsg,
                            &format!("caught signal={signo} pid={pid} (ignored)"),
                        );
                    }
                    Ok(None) => {
                        // EAGAIN — spurious wake, no signal queued. Loop.
                    }
                    Err(e) => {
                        write_kmsg(
                            &mut kmsg,
                            &format!("signalfd read err={e} pid={pid} (continuing)"),
                        );
                    }
                }
            }
            Err(nix::errno::Errno::EINTR) => {
                // poll interrupted by an unblocked signal (shouldn't
                // happen since we blocked the ones we care about, but
                // SIGCHLD / other signals can fire). Just loop.
            }
            Err(e) => {
                write_kmsg(&mut kmsg, &format!("poll err={e} pid={pid} (continuing)"));
            }
        }
    }
}

/// /dev/kmsg writer. Returns `None` if the device cannot be opened
/// (probe still runs — just writes to stderr in that case).
struct KmsgWriter {
    file: Option<std::fs::File>,
}

fn open_kmsg() -> KmsgWriter {
    let path = Path::new("/dev/kmsg");
    match std::fs::OpenOptions::new().append(true).open(path) {
        Ok(f) => KmsgWriter { file: Some(f) },
        Err(e) => {
            eprintln!(
                "halmasuit-shutdown-probe: cannot open /dev/kmsg ({e}); \
                 falling back to stderr"
            );
            KmsgWriter { file: None }
        }
    }
}

/// Write one tagged line. /dev/kmsg accepts a leading `<N>` priority
/// prefix; `<6>` is INFO which surfaces on the kernel serial console
/// at typical loglevel settings. The tag prefix matches what the VM
/// test regexes for.
fn write_kmsg(w: &mut KmsgWriter, msg: &str) {
    let line = format!("<6>halmasuit-shutdown-probe[phase0]: {msg}\n");
    if let Some(f) = w.file.as_mut() {
        // Best-effort: write failures during shutdown are expected as
        // the system unwinds. Don't escalate.
        let _ = f.write_all(line.as_bytes());
        let _ = f.flush();
    } else {
        let _ = std::io::stderr().write_all(line.as_bytes());
    }
}
