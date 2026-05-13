// drm-master-probe — research probe for halmasuit v2.
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
// This is research scaffolding. Production DRM ownership lives in
// halmasuit-kms (v2). See ARCHITECTURE.md "The architectural commitment".

#![deny(unsafe_code)]

use std::os::fd::{AsFd, AsRawFd, BorrowedFd};
use std::path::Path;
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
    let pid = std::process::id();
    eprintln!("drm-master-probe: phase=initramfs pid={pid} argv0_marker=@ set");

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

    // Phase 1c: post-switchroot heartbeat. Same `start`, monotonic tick.
    loop {
        std::thread::sleep(Duration::from_millis(100));
        let elapsed = start.elapsed().as_secs();
        if elapsed > last_tick {
            last_tick = elapsed;
            eprintln!("drm-master-probe: tick t={elapsed}s phase=post-switchroot");
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
