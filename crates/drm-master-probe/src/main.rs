// drm-master-probe — research probe for halmasuit v2.
//
// This crate is the artifact described in RESEARCH.md at the repo root.
// It is NOT production code: it is not gated by `just check`, it has
// not been through formal review, and its assertions mirror the
// architectural questions that motivated it rather than the contracts
// halmasuit v2 production code (`halmasuit-*` crates) will need.
//
// The probe exists as a runnable proof, so when doubt about the
// project's premise arises in the future, anyone — including
// future-me — can run `just test-drm-probe` and `just test-drm-probe-phase1`
// and re-establish ground truth in seconds.
//
// Phase 0 (rootfs-direct): if /etc/initrd-release is absent at startup,
// the probe opens /dev/dri/card0, takes DRM master, paints solid red via
// dumb buffer + SETCRTC, and heartbeats forever. Answers: can a userspace
// process hold DRM master from rootfs boot through multi-user.target?
//
// Phase 1 (initramfs-survival): if /etc/initrd-release is present at
// startup, the probe sets argv[0][0] = '@' (systemd's switch_root
// survival convention from <https://systemd.io/ROOT_STORAGE_DAEMONS/>),
// takes DRM master, paints, and polls for /etc/initrd-release to
// disappear (signal that switch_root completed). On disappearance, it
// drops privileges via setresuid to a non-root UID (default 1000,
// configurable via PROBE_DROP_UID), verifies master is still held by
// re-issuing SETCRTC, and continues heartbeating. Answers: can the same
// process span the initramfs→rootfs boundary with DRM master and drop
// privileges cleanly?
//
// See RESEARCH.md for results, ARCHITECTURE.md "The architectural
// commitment" for how the probe's findings feed v2's design. Production
// DRM ownership lives in `halmasuit-kms` (v2), written from scratch
// against the patterns this probe validated — not lifted from it.

#![deny(unsafe_code)]

use std::os::fd::{AsFd, AsRawFd, BorrowedFd};
use std::path::Path;
use std::sync::atomic::{AtomicI32, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use drm::Device;
use drm::buffer::DrmFourcc;
use drm::control::{Device as ControlDevice, Mode, connector, crtc, framebuffer};

/// Minimal `Device` newtype wrapping a DRM device file.
///
/// Matches the pattern from the upstream `drm-rs` `legacy_modeset` example:
/// implement `AsFd`, then the marker traits `Device` + `ControlDevice`.
struct Card(std::fs::File);

impl AsFd for Card {
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.0.as_fd()
    }
}

impl Device for Card {}
impl ControlDevice for Card {}

/// State produced by `setup_drm_and_paint`. The handles + mode are
/// retained so Phase 1 can re-issue `set_crtc` post-setresuid as the
/// master-still-held verification.
struct ActiveModeset {
    card: Card,
    crtc_handle: crtc::Handle,
    fb_handle: framebuffer::Handle,
    connector_handle: connector::Handle,
    mode: Mode,
}

/// Set `argv[0][0] = '@'` so systemd's `switch_root` excludes us from
/// the killing spree at the initramfs→rootfs boundary. See
/// <https://systemd.io/ROOT_STORAGE_DAEMONS/>.
///
/// Plymouth and other storage daemons use this exact technique in
/// production. The mechanism is byte-level mutation of the argv strings
/// region in the process's main-thread stack memory.
#[expect(
    unsafe_code,
    reason = "argv[0] mutation is the systemd @-survival convention per ROOT_STORAGE_DAEMONS; no safe API exposes this"
)]
fn set_argv0_marker() {
    // glibc exports `__progname_full` as a public data symbol pointing
    // at argv[0] in the argv strings region of the process's stack
    // (set up by __libc_init_first at startup). `nm -D libc.so.6 |
    // grep progname` confirms it's available on NixOS's glibc. Writing
    // to *__progname_full mutates argv[0] memory, which /proc/self/cmdline
    // reflects via mm_struct's arg_start.
    unsafe extern "C" {
        static __progname_full: *mut std::os::raw::c_char;
    }

    // SAFETY: glibc populates __progname_full at startup with a pointer
    // to argv[0] in the argv strings region of the process's main-thread
    // stack. That memory is writable per Linux's process layout. Writing
    // one byte to argv[0][0] is the documented mechanism for systemd's
    // @-survival convention per <https://systemd.io/ROOT_STORAGE_DAEMONS/>.
    // Plymouth and other storage daemons use this exact technique.
    unsafe {
        let argv0_ptr: *mut std::os::raw::c_char = __progname_full;
        *argv0_ptr = b'@'.cast_signed();
    }
}

/// Phase 1 diagnostic: fd to /run/drm-master-probe-events.log, written
/// to from signal handlers before the process dies. -1 means uninitialized;
/// signal handler bails to default behavior in that case.
static EVENT_LOG_FD: AtomicI32 = AtomicI32::new(-1);

/// Async-signal-safe handler that records which signal arrived and
/// **continues running** (does not exit). This lets us learn what
/// arrives without dying from it; we want SIGTERM ignored so we can see
/// what kills us next (e.g., a follow-up SIGKILL would still terminate).
///
/// SIGABRT specifically: Rust panic machinery uses abort(), which
/// raises SIGABRT. We do NOT want to ignore SIGABRT in production code,
/// but for this probe's diagnostic, returning lets us see whether the
/// kill is signal-based or something else entirely.
#[expect(
    unsafe_code,
    reason = "signal handlers must use async-signal-safe libc primitives only"
)]
extern "C" fn diagnostic_signal_handler(sig: libc::c_int) {
    let fd = EVENT_LOG_FD.load(Ordering::Relaxed);
    let msg: &[u8] = match sig {
        libc::SIGTERM => b"SIGTERM (caught, ignored)\n",
        libc::SIGHUP => b"SIGHUP (caught, ignored)\n",
        libc::SIGPIPE => b"SIGPIPE (caught, ignored)\n",
        libc::SIGINT => b"SIGINT (caught, ignored)\n",
        libc::SIGABRT => b"SIGABRT (caught, ignored)\n",
        libc::SIGQUIT => b"SIGQUIT (caught, ignored)\n",
        _ => b"OTHER (caught, ignored)\n",
    };
    // SAFETY: write() is async-signal-safe per POSIX. Returning from the
    // handler resumes the interrupted instruction; the signal is consumed.
    unsafe {
        if fd >= 0 {
            libc::write(fd, msg.as_ptr().cast(), msg.len());
        }
    }
}

/// Phase 1 diagnostics:
///   - dup2 stderr (fd 2) onto a file in /run so eprintln writes survive
///     switch_root (journald-in-initrd dies, its stderr pipe breaks)
///   - install signal handlers that log which signal killed us before
///     exiting (caught signals only — SIGKILL can't be caught, but the
///     absence of any logged signal IS itself a finding: probably a
///     SIGKILL from cgroup.kill or similar)
#[expect(
    unsafe_code,
    reason = "libc dup2 + signal for diagnostic setup; documented operations"
)]
fn setup_phase1_diagnostics() -> Result<()> {
    use std::os::unix::io::IntoRawFd;

    let log = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open("/run/drm-master-probe.log")
        .context("open /run/drm-master-probe.log")?;
    let events = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open("/run/drm-master-probe-events.log")
        .context("open /run/drm-master-probe-events.log")?;

    // SAFETY: dup2 redirects fd 2 (stderr) onto our log file fd. After
    // this, every eprintln write lands in /run/drm-master-probe.log,
    // which persists across switch_root (it's tmpfs at /run).
    unsafe {
        libc::dup2(log.into_raw_fd(), 2);
    }
    EVENT_LOG_FD.store(events.into_raw_fd(), Ordering::Relaxed);

    let signals: &[libc::c_int] = &[
        libc::SIGTERM,
        libc::SIGHUP,
        libc::SIGPIPE,
        libc::SIGINT,
        libc::SIGABRT,
        libc::SIGQUIT,
    ];
    // SAFETY: signal() installs a global signal handler. Legal for a
    // single-threaded process (which ours is).
    unsafe {
        for &sig in signals {
            libc::signal(
                sig,
                diagnostic_signal_handler as *const () as libc::sighandler_t,
            );
        }
    }
    Ok(())
}

/// Log /proc/self/cgroup contents — if the probe dies because of cgroup
/// manipulation by rootfs systemd, we'll see the cgroup change in the
/// log right before death.
fn log_cgroup(label: &str) {
    let cgroup = std::fs::read_to_string("/proc/self/cgroup")
        .unwrap_or_else(|e| format!("(unreadable: {e})"));
    eprintln!("drm-master-probe: cgroup at {label}: {}", cgroup.trim());
}

/// Log /proc/self/cmdline contents — verifies whether the argv[0]='@'
/// mutation is still visible to the kernel (and thus to systemd's
/// @-survival check). Note: cmdline is NUL-separated; we display
/// substituting NUL with '|' for readability.
fn log_cmdline(label: &str) {
    let cmdline = std::fs::read("/proc/self/cmdline").map_or_else(
        |e| format!("(unreadable: {e})"),
        |bytes| {
            let displayable: Vec<u8> = bytes
                .iter()
                .map(|&b| if b == 0 { b'|' } else { b })
                .collect();
            String::from_utf8_lossy(&displayable).into_owned()
        },
    );
    eprintln!("drm-master-probe: cmdline at {label}: {cmdline}");
}

/// Phase 0's DRM pipeline: open device, take master, paint solid red,
/// SETCRTC. Returns the modeset state so Phase 1 can re-verify master
/// after setresuid. The three Phase 0 log lines are emitted unchanged.
fn setup_drm_and_paint() -> Result<ActiveModeset> {
    // step 1: open /dev/dri/card0
    let card = Card(
        std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open("/dev/dri/card0")
            .context("step 1: open /dev/dri/card0")?,
    );
    let fd = card.0.as_raw_fd();

    // step 2: become DRM master
    card.acquire_master_lock()
        .context("step 2: SET_MASTER (acquire_master_lock)")?;
    let pid = std::process::id();
    eprintln!("drm-master-probe: SET_MASTER ok pid={pid} fd={fd}");

    // step 3: enumerate DRM resources
    let res = card
        .resource_handles()
        .context("step 3: resource_handles")?;

    // step 4: find a connected connector
    let Some(con) = res
        .connectors()
        .iter()
        .filter_map(|&h| card.get_connector(h, true).ok())
        .find(|info| info.state() == connector::State::Connected)
    else {
        eprintln!("drm-master-probe: error: no connected connector");
        std::process::exit(2);
    };

    // step 5: pick the connector's preferred mode (DRM convention: first)
    let Some(&mode) = con.modes().first() else {
        eprintln!("drm-master-probe: error: connector has no modes");
        std::process::exit(2);
    };
    let (w, h) = mode.size();
    let refresh = mode.vrefresh();
    eprintln!("drm-master-probe: mode {w}x{h}@{refresh}");

    // step 6: pick a CRTC (first available; single-monitor VM substrate)
    let &crtc_handle = res.crtcs().first().context("step 6: no CRTCs available")?;
    eprintln!("drm-master-probe: crtc {crtc_handle:?}");

    // step 7: create dumb buffer at the connector's preferred resolution
    let mut db = card
        .create_dumb_buffer((u32::from(w), u32::from(h)), DrmFourcc::Xrgb8888, 32)
        .context("step 7: create_dumb_buffer")?;

    // step 8: map + fill with solid red
    //
    // XRGB8888 little-endian byte order is [B, G, R, X] per pixel.
    // `chunks_exact_mut(4)` guarantees each chunk is exactly 4 bytes, so
    // `copy_from_slice` of a 4-byte literal is bounds-safe and cleaner
    // than indexed assignment.
    {
        let mut map = card
            .map_dumb_buffer(&mut db)
            .context("step 8: map_dumb_buffer")?;
        for pixel in map.as_mut().chunks_exact_mut(4) {
            pixel.copy_from_slice(&[0x00, 0x00, 0xFF, 0x00]);
        }
    }

    // step 9: wrap dumb buffer as a framebuffer
    let fb_handle = card
        .add_framebuffer(&db, 24, 32)
        .context("step 9: add_framebuffer")?;

    // step 10: drive the connector with our framebuffer at the chosen mode
    let connector_handle = con.handle();
    card.set_crtc(
        crtc_handle,
        Some(fb_handle),
        (0, 0),
        &[connector_handle],
        Some(mode),
    )
    .context("step 10: set_crtc")?;
    eprintln!("drm-master-probe: SETCRTC ok");

    Ok(ActiveModeset {
        card,
        crtc_handle,
        fb_handle,
        connector_handle,
        mode,
    })
}

/// Phase 0 entry: rootfs-direct heartbeat. Holds master forever; SIGTERM
/// is the only exit path. Tick log format is byte-identical to the
/// pre-Phase-1 probe (no `phase=` tag) — `tests/drm-master-probe.nix`
/// keeps passing unchanged.
#[expect(
    clippy::infinite_loop,
    reason = "the probe is intended to hold DRM master forever and only exit via SIGTERM"
)]
fn run_rootfs_direct_phase() -> Result<()> {
    let _state = setup_drm_and_paint()?;

    // step 11: heartbeat — hold master forever
    let start = Instant::now();
    loop {
        std::thread::sleep(Duration::from_secs(1));
        let t = start.elapsed().as_secs();
        eprintln!("drm-master-probe: tick t={t}s");
    }
}

/// Phase 1 entry: mark argv[0] for systemd survival, take master, wait
/// for `switch_root` (signaled by `/etc/initrd-release` disappearance),
/// then drop privileges via setresuid and continue heartbeating with a
/// `phase=post-switchroot` tag. Tick numbers are monotonically
/// continuous across the phase boundary (same `start` instant).
#[expect(
    clippy::infinite_loop,
    reason = "the probe is intended to hold DRM master forever and only exit via SIGTERM"
)]
fn run_initramfs_phase() -> Result<()> {
    set_argv0_marker();
    setup_phase1_diagnostics()?;
    let pid = std::process::id();
    eprintln!("drm-master-probe: phase=initramfs pid={pid} argv0_marker=@ set");
    log_cgroup("phase=initramfs start");
    log_cmdline("phase=initramfs start");

    let state = setup_drm_and_paint()?;

    let start = Instant::now();
    let initrd_release = Path::new("/etc/initrd-release");
    let mut last_tick: u64 = 0;

    // Phase 1a: initramfs heartbeat + poll for switch_root completion.
    // Poll every ~100ms; tick once per second; transition when
    // /etc/initrd-release disappears.
    loop {
        std::thread::sleep(Duration::from_millis(100));
        let elapsed = start.elapsed().as_secs();
        if elapsed > last_tick {
            last_tick = elapsed;
            eprintln!("drm-master-probe: tick t={elapsed}s phase=initramfs");
        }
        if !initrd_release.exists() {
            break;
        }
    }

    log_cgroup("switch_root detected, pre-setresuid");
    log_cmdline("switch_root detected, pre-setresuid");
    // Empirical finding from this probe: rootfs systemd discovers our
    // orphan unit ~1s post-switch_root and sends SIGTERM intending to
    // reap it (the unit's name lives only in the initramfs systemd; the
    // rootfs systemd's "Unit drm-master-probe.service not found" status
    // means it tries to stop what it doesn't recognize). With the
    // diagnostic SIGTERM handler in setup_phase1_diagnostics catching
    // and ignoring it, we survive indefinitely. Rootfs systemd then
    // sits in "stop-sigterm" wait. v2 production halmasuit would
    // either sd_notify into rootfs systemd to be tracked as a unit,
    // handle SIGTERM with graceful release, or detach explicitly —
    // none of those is an architectural blocker; this is standard
    // daemon engineering.

    // Phase 1b: switch_root has completed. Drop privileges.
    let from_uid = nix::unistd::Uid::current().as_raw();
    let target_uid: u32 = std::env::var("PROBE_DROP_UID")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(1000);
    let target = nix::unistd::Uid::from_raw(target_uid);

    if let Err(e) = nix::unistd::setresuid(target, target, target) {
        eprintln!("drm-master-probe: error: setresuid({from_uid}→{target_uid}) failed: {e}");
        std::process::exit(3);
    }

    // Verify master is still held by re-issuing set_crtc with the same
    // parameters as the original SETCRTC. set_crtc requires the file
    // descriptor to be master; if we lost master across setresuid this
    // ioctl returns an error. The kernel does a delta check on the
    // modeset parameters and skips actual hardware programming when
    // nothing has changed, so this is a no-op on the visible scanout.
    if let Err(e) = state.card.set_crtc(
        state.crtc_handle,
        Some(state.fb_handle),
        (0, 0),
        &[state.connector_handle],
        Some(state.mode),
    ) {
        eprintln!("drm-master-probe: error: master verification failed after setresuid: {e}");
        std::process::exit(4);
    }

    eprintln!(
        "drm-master-probe: phase=post-switchroot setresuid({from_uid}→{target_uid}) ok, master still held"
    );
    log_cgroup("phase=post-switchroot start");

    // Phase 1c: post-switchroot heartbeat. Same `start`, monotonic tick.
    // Re-log cgroup state every 5 ticks so we can see if rootfs systemd
    // manipulates our cgroup membership over time.
    loop {
        std::thread::sleep(Duration::from_millis(100));
        let elapsed = start.elapsed().as_secs();
        if elapsed > last_tick {
            last_tick = elapsed;
            eprintln!("drm-master-probe: tick t={elapsed}s phase=post-switchroot");
            if elapsed.is_multiple_of(5) {
                log_cgroup(&format!("phase=post-switchroot t={elapsed}s"));
            }
        }
    }
}

fn main() -> Result<()> {
    if Path::new("/etc/initrd-release").exists() {
        run_initramfs_phase()
    } else {
        run_rootfs_direct_phase()
    }
}
