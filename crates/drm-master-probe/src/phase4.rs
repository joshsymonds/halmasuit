//! Phase 4 — libseat/seatd session survival across `setresuid`.
//!
//! Phases 0–3 validated a SELF-acquired DRM master (drm-rs
//! `SET_MASTER`) surviving privilege boundaries. Epic layer E adopts
//! libseat: `Session::open()` REPLACES self-`SET_MASTER` — seatd (the
//! root daemon) brokers the DRM + input fds and owns the
//! master/session. This phase empirically answers the question that
//! gates the #11 production rewire: does a seatd-brokered libseat
//! session (DRM master + libinput + session-active) survive
//! halmasuit's `setresuid` drop?
//!
//! The drop here is a BARE `setresuid` to a non-root uid (zero
//! retained capabilities) — strictly STRICTER than halmasuit's actual
//! drop, which retains `CAP_KILL`. So a pass here is a-fortiori valid
//! for halmasuit's posture. Selected by `PROBE_PHASE=seatd`; entirely
//! behind the `phase4` cargo feature so Phases 0–3 keep their lean
//! DRM-only closure.

use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use drm::control::Device as ControlDevice;
use smithay::backend::input::{InputEvent, KeyboardKeyEvent};
use smithay::backend::libinput::{LibinputInputBackend, LibinputSessionInterface};
use smithay::backend::session::Session;
use smithay::backend::session::libseat::LibSeatSession;
use smithay::reexports::calloop::EventLoop;
use smithay::reexports::input::Libinput;
use smithay::reexports::rustix::fs::OFlags;

use crate::Card;

struct PhaseState {
    dropped: bool,
    got_key: bool,
}

/// The handles a modeset establishes, enough to re-issue the
/// master-only `set_crtc` after the privilege drop.
struct Modeset {
    crtc: drm::control::crtc::Handle,
    fb: drm::control::framebuffer::Handle,
    connector: drm::control::connector::Handle,
    mode: drm::control::Mode,
}

/// Phase 4 entry. Returns `Err` (or `bail!`s) on any failure so the
/// systemd unit exits non-zero and the VM test fails loudly with the
/// precise finding.
pub fn run() -> Result<()> {
    let target_uid: u32 = std::env::var("PROBE_DROP_UID")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(1000);
    let drm_path =
        std::env::var("PROBE_DRM_DEVICE").unwrap_or_else(|_| "/dev/dri/card0".to_owned());

    // 1. libseat session via seatd (NOT self-acquired master).
    let (mut session, notifier) = LibSeatSession::new()
        .context("LibSeatSession::new() — is seatd running and the socket reachable?")?;
    let seat = session.seat();
    eprintln!(
        "drm-master-probe: phase4 libseat session seat={seat} active={}",
        session.is_active()
    );

    // 2. DRM fd brokered by seatd.
    let drm_fd = session
        .open(
            Path::new(&drm_path),
            OFlags::RDWR | OFlags::CLOEXEC | OFlags::NONBLOCK,
        )
        .map_err(|e| anyhow!("session.open({drm_path}): {e}"))?;
    let card = Card(std::fs::File::from(drm_fd));

    // 3. Pre-drop modeset. `set_crtc` is master-only — it succeeds
    //    here iff seatd actually brokered DRM master to us.
    let modeset = modeset(&card).context("phase4 pre-drop modeset (seatd brokered master?)")?;
    eprintln!(
        "drm-master-probe: phase4 pre-drop master=OK (set_crtc) active={}",
        session.is_active()
    );

    // 4. libinput, fed device fds through the same seatd session.
    let mut libinput = Libinput::new_with_udev(LibinputSessionInterface::from(session.clone()));
    libinput
        .udev_assign_seat(&seat)
        .map_err(|()| anyhow!("libinput udev_assign_seat({seat})"))?;
    let backend = LibinputInputBackend::new(libinput);

    let mut event_loop: EventLoop<PhaseState> =
        EventLoop::try_new().context("phase4 calloop EventLoop")?;
    let handle = event_loop.handle();
    handle
        .insert_source(notifier, |_event, (), _state| {})
        .map_err(|e| anyhow!("insert libseat notifier: {e}"))?;
    handle
        .insert_source(backend, |event, (), state: &mut PhaseState| {
            if let InputEvent::Keyboard { event } = event
                && state.dropped
            {
                state.got_key = true;
                let code = event.key_code();
                eprintln!("drm-master-probe: phase4 post-drop input event received (key {code:?})");
            }
        })
        .map_err(|e| anyhow!("insert libinput backend: {e}"))?;

    eprintln!(
        "drm-master-probe: phase4 pre-drop: master=OK active={} seat={seat}",
        session.is_active()
    );

    // 5. BARE setresuid to a non-root uid (zero retained caps —
    //    stricter than halmasuit's CAP_KILL-retaining drop).
    let uid = nix::unistd::Uid::from_raw(target_uid);
    nix::unistd::setresuid(uid, uid, uid).map_err(|e| anyhow!("setresuid(->{target_uid}): {e}"))?;
    eprintln!("drm-master-probe: phase4 setresuid(->{target_uid}) ok");

    let mut state = PhaseState {
        dropped: true,
        got_key: false,
    };

    // 6. Post-drop master re-assert: re-issue the master-only
    //    `set_crtc`. Fails (EACCES/EPERM) iff master was lost.
    card.set_crtc(
        modeset.crtc,
        Some(modeset.fb),
        (0, 0),
        &[modeset.connector],
        Some(modeset.mode),
    )
    .context("phase4 post-drop set_crtc — DRM master lost across setresuid")?;
    let active = session.is_active();
    eprintln!("drm-master-probe: phase4 post-drop master=OK active={active}");

    // 7. Prove libinput still delivers AFTER the drop: the VM test
    //    injects a keystroke now. Budget: 120 × 500ms ≈ 60s.
    for _ in 0..120 {
        if state.got_key {
            break;
        }
        event_loop
            .dispatch(Some(Duration::from_millis(500)), &mut state)
            .context("phase4 calloop dispatch (post-drop)")?;
    }
    if !state.got_key {
        bail!("phase4: no input event within ~60s after setresuid (libinput dead post-drop?)");
    }

    // The single line the VM test asserts on.
    eprintln!("drm-master-probe: phase4 post-drop: master=OK input=OK active={active}");

    // Hold so the driver can read the journal, then shut the VM down.
    loop {
        let _ = event_loop.dispatch(Some(Duration::from_secs(1)), &mut state);
    }
}

/// Minimal modeset on a seatd-brokered `Card`: connected connector →
/// preferred mode → first CRTC → dumb buffer → framebuffer →
/// `set_crtc`. `set_crtc` is the master-only op whose success proves
/// seatd brokered DRM master to us. Self-contained (does NOT touch
/// the Phase 0–3 code path).
fn modeset(card: &Card) -> Result<Modeset> {
    use drm::control::connector;

    let res = card.resource_handles().context("phase4 resource_handles")?;
    let con = res
        .connectors()
        .iter()
        .filter_map(|&h| card.get_connector(h, true).ok())
        .find(|i| i.state() == connector::State::Connected)
        .ok_or_else(|| anyhow!("phase4: no connected connector"))?;
    let &mode = con
        .modes()
        .first()
        .ok_or_else(|| anyhow!("phase4: connector has no modes"))?;
    let (w, h) = mode.size();
    let &crtc_handle = res
        .crtcs()
        .first()
        .ok_or_else(|| anyhow!("phase4: no CRTCs"))?;
    let mut db = card
        .create_dumb_buffer(
            (u32::from(w), u32::from(h)),
            drm::buffer::DrmFourcc::Xrgb8888,
            32,
        )
        .context("phase4 create_dumb_buffer")?;
    {
        let mut map = card
            .map_dumb_buffer(&mut db)
            .context("phase4 map_dumb_buffer")?;
        for px in map.as_mut().chunks_exact_mut(4) {
            px.copy_from_slice(&[0x14, 0x00, 0x0a, 0x00]); // brand-ish, XRGB8888 LE
        }
    }
    let fb = card
        .add_framebuffer(&db, 24, 32)
        .context("phase4 add_framebuffer")?;
    let connector = con.handle();
    card.set_crtc(crtc_handle, Some(fb), (0, 0), &[connector], Some(mode))
        .context("phase4 set_crtc (master-only — seatd brokered master?)")?;
    Ok(Modeset {
        crtc: crtc_handle,
        fb,
        connector,
        mode,
    })
}
