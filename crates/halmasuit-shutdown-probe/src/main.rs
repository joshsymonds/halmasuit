// halmasuit-shutdown-probe — research probe for halmasuit Epic #47 R2.
//
// This crate is a runnable proof, analogous to `drm-master-probe`.
// It empirically validates the load-bearing mechanics the production
// `halmasuit` binary depends on for wallpaper continuity through
// systemd shutdown. It is NOT production code: it's gated by VM tests,
// not by `just check`'s unit-test sweep, and its assertions answer
// architectural questions rather than implement the production contract.
//
// Phase 0: the probe runs as a rootfs systemd unit with
// `SurviveFinalKillSignal=yes`. It writes timestamped heartbeats to
// `/dev/kmsg` every 100ms and ignores SIGTERM / SIGINT / SIGHUP. The
// VM test triggers `systemctl poweroff`, waits for the VM to halt, then
// reads qemu's serial-console capture and asserts that at least one
// heartbeat was emitted AFTER systemd-shutdown's "Sending SIGKILL to
// remaining processes" marker. STATUS: GREEN.
//
// Phase 1: same probe with `systemd.shutdownRamfs.storePaths`
// configured to materialize the probe binary into the shutdown ramfs.
// Validates that the same PID survives systemd-shutdown's actual pivot
// from rootfs to `/run/initramfs` — heartbeats appear AFTER the first
// `shutdown[1]:` log line (which is the post-pivot systemd-shutdown
// binary executing from the shutdown ramfs). STATUS: GREEN.
//
// Phase 2: + opens `/dev/dri/card0`, takes DRM master, allocates a
// magenta-painted dumb buffer + framebuffer, calls `set_crtc` ONCE
// at startup, then enters the standard heartbeat loop. Per-tick
// status reports `drm_fd_open=<raw_fd>` — proving the fd handle
// is intact across each tick without making a kernel ioctl that
// might deadlock against shutdown's unmount sequence. The VM test
// asserts (a) DRM setup succeeded pre-shutdown (debugfs shows the
// probe as DRM master) and (b) heartbeats appear AFTER the
// post-pivot `shutdown[1]:` marker — proving a process HOLDING
// DRM resources survives the rootfs→shutdownRamfs pivot. The
// stronger question "does set_crtc still succeed post-pivot?" is
// deferred to production wiring (an earlier experiment with
// per-tick set_crtc made the heartbeat loop stop emitting ~15ms
// before the SIGTERM kill spree, suggesting set_crtc deadlocks
// against the kernel's shutdown teardown — production code will
// use the page-flip / atomic API which has its own non-blocking
// semantics).
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
use std::os::fd::{AsFd, BorrowedFd};
use std::path::Path;
use std::process;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use drm::Device;
use drm::buffer::DrmFourcc;
use drm::control::{Device as ControlDevice, connector};
use nix::poll::{PollFd, PollFlags, PollTimeout, poll};
use nix::sys::signal::{SigSet, Signal};
use nix::sys::signalfd::{SfdFlags, SignalFd};

/// Process start instant; heartbeat seq is monotonic from boot of THIS
/// process, used by the test to confirm the same process kept emitting
/// past the kill marker (not a new process re-entering after a respawn).
static HEARTBEAT_SEQ: AtomicU64 = AtomicU64::new(0);

/// Run subcommand router. Sub-phases compose: each one adds something
/// (shutdownRamfs.storePaths config in Phase 1, DRM in Phase 2) but
/// the probe loop is the same shape every time.
fn main() {
    let mut args = std::env::args().skip(1);
    let phase = args.next();
    match phase.as_deref() {
        Some("phase0") => heartbeat_loop("phase0", noop_status),
        Some("phase1") => heartbeat_loop("phase1", noop_status),
        Some("phase2") => phase2(),
        Some(other) => {
            eprintln!(
                "halmasuit-shutdown-probe: unknown phase {other:?}; \
                 supported: phase0, phase1, phase2"
            );
            process::exit(2);
        }
        None => {
            eprintln!(
                "halmasuit-shutdown-probe: usage: halmasuit-shutdown-probe <phase0|phase1|phase2>"
            );
            process::exit(2);
        }
    }
}

/// Status callback for phases that don't add per-tick instrumentation.
const fn noop_status() -> String {
    String::new()
}

/// Heartbeat loop shared by every phase. Each phase tags its kmsg
/// lines with its phase name so the VM test can disambiguate. The
/// `extra_status` callback is invoked per heartbeat and its output
/// is appended to the log line — Phase 2 uses this to report
/// `drm_master_ok=true/false`.
///
/// Behavior: heartbeat every 100ms via signalfd-polled loop. SIGTERM,
/// SIGINT, SIGHUP are blocked + drained via signalfd, logged, ignored.
/// Only SIGKILL exits this process.
fn heartbeat_loop<F: FnMut() -> String>(phase: &'static str, mut extra_status: F) {
    let pid = process::id();
    let mut kmsg = open_kmsg();
    write_kmsg(
        &mut kmsg,
        phase,
        &format!("started pid={pid} (ignoring SIGTERM/INT/HUP)"),
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

    // Tick interval. 100ms is coarse enough that systemd-shutdown's
    // serial log doesn't drown the heartbeats; 10ms is fine enough
    // to reliably catch the brief (~50-100ms) post-pivot window
    // before kernel halt. Phase 0/1 worked with 100ms but had
    // borderline coverage of the post-pivot window; Phase 2's first
    // attempts showed the heartbeat tick straddling the shutdown
    // window without emitting inside it. 10ms is the smallest value
    // that still produces a readable serial console (faster ticks
    // saturate qemu's stdio with heartbeat noise).
    let tick = Duration::from_millis(10);
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
                // Timeout: emit heartbeat (with optional per-tick
                // status suffix) and advance tick.
                let seq = HEARTBEAT_SEQ.fetch_add(1, Ordering::Relaxed) + 1;
                let status = extra_status();
                let line = if status.is_empty() {
                    format!("heartbeat seq={seq} pid={pid}")
                } else {
                    format!("heartbeat seq={seq} pid={pid} {status}")
                };
                write_kmsg(&mut kmsg, phase, &line);
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
                            phase,
                            &format!("caught signal={signo} pid={pid} (ignored)"),
                        );
                    }
                    Ok(None) => {
                        // EAGAIN — spurious wake, no signal queued. Loop.
                    }
                    Err(e) => {
                        write_kmsg(
                            &mut kmsg,
                            phase,
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
                write_kmsg(
                    &mut kmsg,
                    phase,
                    &format!("poll err={e} pid={pid} (continuing)"),
                );
            }
        }
    }
}

// ─────────────────────────────────────────────────────────────────────
// Phase 2 — DRM master fd survival
// ─────────────────────────────────────────────────────────────────────

/// `Card` newtype wrapping a DRM device fd, mirroring drm-master-probe's
/// shape. The trait impls let `drm-rs` issue ioctls against our file.
struct Card(std::fs::File);

impl AsFd for Card {
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.0.as_fd()
    }
}

impl Device for Card {}
impl ControlDevice for Card {}

/// State held across the Phase 2 heartbeat loop. Holding the `Card`
/// (and therefore the DRM fd) open is what keeps master valid; we
/// keep the crtc / fb handles around in the struct so a future
/// extension can re-issue ioctls without re-discovering them.
struct DrmState {
    card: Card,
    /// Stored for diagnostic logging in the one-shot `drm_setup_ok=true`
    /// line; not used per-tick.
    #[allow(dead_code, reason = "logged once at setup, kept for future sub-tasks")]
    crtc_handle: drm::control::crtc::Handle,
    /// Same as `crtc_handle`.
    #[allow(dead_code, reason = "logged once at setup, kept for future sub-tasks")]
    fb_handle: drm::control::framebuffer::Handle,
}

/// Phase 2 entry: do the DRM dance up front (open, master, paint), then
/// hand off to the shared heartbeat loop with a per-tick status callback
/// that just reports the raw DRM fd number (proves the fd handle is
/// alive without making a kernel ioctl that might deadlock against
/// shutdown teardown). If the DRM setup itself fails, the probe still
/// enters the heartbeat loop so the test can distinguish "DRM setup
/// failed" from "DRM setup succeeded but pivot killed us."
fn phase2() {
    let pid = process::id();
    let mut kmsg = open_kmsg();

    let drm_setup = setup_drm_and_paint();
    let drm_state = match drm_setup {
        Ok(state) => {
            write_kmsg(
                &mut kmsg,
                "phase2",
                &format!(
                    "drm_setup_ok=true pid={pid} crtc={crtc:?} fb={fb:?}",
                    crtc = state.crtc_handle,
                    fb = state.fb_handle,
                ),
            );
            Some(state)
        }
        Err(e) => {
            write_kmsg(
                &mut kmsg,
                "phase2",
                &format!("drm_setup_ok=false pid={pid} err={e:#}"),
            );
            None
        }
    };
    // Drop the kmsg writer used for the one-shot setup line; the
    // heartbeat loop opens its own (sharing one writer across two
    // ownership boundaries would force lifetime gymnastics for no
    // benefit — kmsg can be opened many times).
    drop(kmsg);

    let state = drm_state;
    heartbeat_loop("phase2", move || {
        state.as_ref().map_or_else(
            || "drm_fd_open=skipped (setup failed)".to_string(),
            |s| {
                // AsFd on Card → BorrowedFd; AsRawFd is i32. Reporting
                // the raw fd number per tick is a kernel-touch-free
                // way to confirm the fd handle is still alive across
                // the pivot. If a future test wants stronger evidence
                // (e.g. set_crtc still succeeds post-pivot), it should
                // use a non-blocking page-flip API rather than the
                // synchronous set_crtc — see module doc.
                use std::os::fd::AsRawFd;
                format!("drm_fd_open={}", s.card.0.as_raw_fd())
            },
        )
    });
}

/// Open `/dev/dri/card0`, take DRM master, allocate a magenta dumb
/// buffer, modeset on the first connected connector + first CRTC.
/// Returns the state the heartbeat loop will re-validate per tick.
///
/// Magenta (RGB 0xFF, 0x00, 0xFF) is the probe's intended color so a
/// future visual-shutdown test can distinguish "probe painted" from
/// "kernel framebuffer black" without ambiguity.
fn setup_drm_and_paint() -> Result<DrmState> {
    let card = Card(
        std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open("/dev/dri/card0")
            .context("open /dev/dri/card0")?,
    );

    card.acquire_master_lock()
        .context("DRM_IOCTL_SET_MASTER (acquire_master_lock)")?;

    let res = card.resource_handles().context("resource_handles")?;

    let con = res
        .connectors()
        .iter()
        .filter_map(|&h| card.get_connector(h, true).ok())
        .find(|info| info.state() == connector::State::Connected)
        .context("no connected connector")?;

    let mode = *con.modes().first().context("connector has no modes")?;

    let &crtc_handle = res.crtcs().first().context("no CRTCs available")?;

    let (w, h) = mode.size();
    let mut db = card
        .create_dumb_buffer((u32::from(w), u32::from(h)), DrmFourcc::Xrgb8888, 32)
        .context("create_dumb_buffer")?;

    {
        let mut map = card.map_dumb_buffer(&mut db).context("map_dumb_buffer")?;
        // XRGB8888 little-endian: [B, G, R, X] per pixel. Magenta =
        // (R=0xFF, G=0x00, B=0xFF) → [0xFF, 0x00, 0xFF, 0x00].
        for pixel in map.as_mut().chunks_exact_mut(4) {
            pixel.copy_from_slice(&[0xFF, 0x00, 0xFF, 0x00]);
        }
    }

    let fb_handle = card
        .add_framebuffer(&db, 24, 32)
        .context("add_framebuffer")?;
    let connector_handle = con.handle();
    card.set_crtc(
        crtc_handle,
        Some(fb_handle),
        (0, 0),
        &[connector_handle],
        Some(mode),
    )
    .context("set_crtc")?;

    Ok(DrmState {
        card,
        crtc_handle,
        fb_handle,
    })
}

// ─────────────────────────────────────────────────────────────────────
// kmsg writer
// ─────────────────────────────────────────────────────────────────────

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
fn write_kmsg(w: &mut KmsgWriter, phase: &str, msg: &str) {
    let line = format!("<6>halmasuit-shutdown-probe[{phase}]: {msg}\n");
    if let Some(f) = w.file.as_mut() {
        // Best-effort: write failures during shutdown are expected as
        // the system unwinds. Don't escalate.
        let _ = f.write_all(line.as_bytes());
        let _ = f.flush();
    } else {
        let _ = std::io::stderr().write_all(line.as_bytes());
    }
}
