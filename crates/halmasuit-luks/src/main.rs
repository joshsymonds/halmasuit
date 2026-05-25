// halmasuit-luks — Phase B systemd password-agent Wayland client.
//
// Watches `/run/systemd/ask-password/` for outstanding LUKS unlock
// requests, prompts the user via a fullscreen `xdg_toplevel`, captures
// keystrokes via `wl_keyboard`, and writes the response to the agent
// socket named in each request file. Replaceable: any other
// implementation of the systemd password-agent protocol can be
// substituted by being placed at the path halmasuit invokes.
//
// Spawned by halmasuit when it starts from initramfs (the Phase B
// boot-from-initrd deployment). Exits when `/etc/initrd-release`
// disappears (= `switch_root` complete; rootfs systemd's own
// password-agents take over from here).
//
// MVP scope: fullscreen solid-color surface (no text rendering yet),
// captures keystrokes into a `Zeroizing<Vec<u8>>`, submits on Enter,
// cancels on ESC. Visual polish (text rendering, theming, masked
// asterisks) is later-task work. The test surface this gates is the
// SURVIVAL + SUBMIT mechanism, not UI fidelity.

#![deny(unsafe_code)]
// reason: sctk's idiomatic State struct uses the *_state field names;
// renaming would diverge from every sctk downstream example.
#![allow(clippy::struct_field_names)]
// reason: shm buffer dims are u32 on the wire but i32 in sctk's
// create_buffer API; our sizes are bounded well below i32::MAX.
#![allow(clippy::cast_possible_wrap)]

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::Context;
use calloop::EventLoop;
use calloop::timer::{TimeoutAction, Timer};
use calloop_wayland_source::WaylandSource;
use smithay_client_toolkit::compositor::{CompositorHandler, CompositorState};
use smithay_client_toolkit::output::{OutputHandler, OutputState};
use smithay_client_toolkit::reexports::client::globals::registry_queue_init;
use smithay_client_toolkit::reexports::client::protocol::wl_keyboard::WlKeyboard;
use smithay_client_toolkit::reexports::client::protocol::wl_seat::WlSeat;
use smithay_client_toolkit::reexports::client::protocol::wl_shm::Format;
use smithay_client_toolkit::reexports::client::protocol::{wl_output, wl_surface};
use smithay_client_toolkit::reexports::client::{Connection, QueueHandle};
use smithay_client_toolkit::registry::{ProvidesRegistryState, RegistryState};
use smithay_client_toolkit::registry_handlers;
use smithay_client_toolkit::seat::keyboard::{
    KeyEvent, KeyboardHandler, Keysym, Modifiers, RawModifiers, RepeatInfo,
};
use smithay_client_toolkit::seat::{Capability, SeatHandler, SeatState};
use smithay_client_toolkit::shell::WaylandSurface;
use smithay_client_toolkit::shell::xdg::XdgShell;
use smithay_client_toolkit::shell::xdg::window::{
    Window, WindowConfigure, WindowDecorations, WindowHandler,
};
use smithay_client_toolkit::shm::slot::SlotPool;
use smithay_client_toolkit::shm::{Shm, ShmHandler};
use smithay_client_toolkit::{
    delegate_compositor, delegate_keyboard, delegate_output, delegate_registry, delegate_seat,
    delegate_shm, delegate_xdg_shell, delegate_xdg_window,
};
use tracing_subscriber::EnvFilter;
use zeroize::Zeroizing;

mod agent;

use agent::{AskFile, outstanding_requests};

/// Solid background color for the prompt surface — a deep teal that's
/// distinct from halmasuit's brand clear (`#0a0014`) and from any
/// likely greeter colour. Frame audit identifies this colour as the
/// "halmasuit-luks is up" sentinel.
///
/// ARGB8888-LE byte order: `[B, G, R, A]`.
const PROMPT_BGRA: [u8; 4] = [0x66, 0x4C, 0x0A, 0xFF];

/// Lighter teal painted while keystrokes are being captured — gives a
/// minimal visual indication that input is being received. Same brand
/// family, just brighter.
const ACTIVE_BGRA: [u8; 4] = [0xAA, 0x88, 0x22, 0xFF];

/// Polling cadence for the ask-password directory and the
/// `/etc/initrd-release` pivot marker. systemd-cryptsetup writes the
/// ask file once and waits; 200ms is fast enough that the prompt
/// appears within human-perception latency of the unlock attempt.
const POLL_INTERVAL: Duration = Duration::from_millis(200);

/// Canonical path of the systemd ask-password request directory.
const ASK_DIR: &str = "/run/systemd/ask-password";

/// Canonical INITRD_INTERFACE marker; its absence signals
/// `switch_root` complete.
const INITRD_RELEASE: &str = "/etc/initrd-release";

/// Argv shape: at most one optional flag, `--passphrase-from PATH`.
/// Anything else is rejected loudly.
struct Args {
    /// If `Some`, run in non-interactive mode: skip Wayland connect
    /// entirely, read the passphrase from this file once at startup,
    /// and respond to every ask-password request with that passphrase
    /// until `/etc/initrd-release` disappears. Real production use is
    /// unattended-boot setups where the passphrase is materialised by
    /// an earlier initramfs step (TPM-derived, USB key, etc.); the
    /// LUKS VM gate uses it as well. The file is read once and the
    /// in-memory copy is zeroized after each submit.
    passphrase_from: Option<PathBuf>,
}

fn parse_args() -> anyhow::Result<Args> {
    let mut argv = std::env::args().skip(1);
    let mut passphrase_from: Option<PathBuf> = None;
    while let Some(arg) = argv.next() {
        match arg.as_str() {
            "--passphrase-from" => {
                let path = argv
                    .next()
                    .context("--passphrase-from requires a PATH argument")?;
                passphrase_from = Some(PathBuf::from(path));
            }
            other => anyhow::bail!("unknown argument: {other}"),
        }
    }
    Ok(Args { passphrase_from })
}

fn main() -> anyhow::Result<()> {
    let env_filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    tracing_subscriber::fmt()
        .json()
        .with_writer(std::io::stderr)
        .with_env_filter(env_filter)
        .init();

    let args = parse_args()?;

    tracing::info!(
        version = env!("CARGO_PKG_VERSION"),
        non_interactive = args.passphrase_from.is_some(),
        "halmasuit-luks starting"
    );

    if let Some(path) = args.passphrase_from {
        return run_noninteractive(
            &path,
            &PathBuf::from(ASK_DIR),
            &PathBuf::from(INITRD_RELEASE),
        );
    }

    let conn =
        Connection::connect_to_env().context("connect to wayland (is WAYLAND_DISPLAY set?)")?;
    let (globals, event_queue) = registry_queue_init(&conn)?;
    let qh = event_queue.handle();

    let compositor = CompositorState::bind(&globals, &qh)?;
    let xdg_shell = XdgShell::bind(&globals, &qh)?;
    let shm = Shm::bind(&globals, &qh)?;
    let seat_state = SeatState::new(&globals, &qh);

    let surface = compositor.create_surface(&qh);
    let window = xdg_shell.create_window(surface, WindowDecorations::RequestServer, &qh);
    window.set_title("halmasuit-luks");
    window.set_app_id("halmasuit.luks");
    window.commit();

    // 1080p worst-case buffer pool. Real output is read from
    // configure(); the pool is sized once for the lifetime of the
    // process. SCTK's SlotPool grows on demand if we exceed this, but
    // we never should in the boot-time prompt path.
    let pool = SlotPool::new(1920 * 1080 * 4, &shm)?;

    let mut state = State {
        registry_state: RegistryState::new(&globals),
        output_state: OutputState::new(&globals, &qh),
        seat_state,
        shm,
        _compositor_state: compositor,
        _xdg_shell: xdg_shell,
        window,
        pool,
        keyboard: None,
        width: 0,
        height: 0,
        configured: false,
        passphrase: Zeroizing::new(Vec::with_capacity(128)),
        current_request: None,
        active: false,
        ask_dir: PathBuf::from(ASK_DIR),
        initrd_release: PathBuf::from(INITRD_RELEASE),
        should_exit: false,
    };

    // calloop event loop: drives both the Wayland source AND a 200ms
    // polling timer that scans the ask-password dir and checks for the
    // post-pivot exit condition. Wayland-only would miss the pivot;
    // poll-only would lose keyboard responsiveness.
    let mut event_loop: EventLoop<'static, State> = EventLoop::try_new()?;
    let loop_handle = event_loop.handle();

    WaylandSource::new(conn, event_queue)
        .insert(loop_handle.clone())
        .map_err(|e| anyhow::anyhow!("insert WaylandSource into calloop: {e}"))?;

    loop_handle
        .insert_source(
            Timer::from_duration(POLL_INTERVAL),
            |_deadline, (): &mut (), state: &mut State| {
                state.poll_external_state();
                if state.should_exit {
                    return TimeoutAction::Drop;
                }
                TimeoutAction::ToDuration(POLL_INTERVAL)
            },
        )
        .map_err(|e| anyhow::anyhow!("insert poll timer: {e}"))?;

    while !state.should_exit {
        event_loop.dispatch(Some(POLL_INTERVAL), &mut state)?;
    }

    tracing::info!("halmasuit-luks exiting (pivot complete)");
    Ok(())
}

/// Non-interactive mode: no Wayland UI. Poll `ask_dir` for outstanding
/// ask-password requests, respond to each with the contents of
/// `passphrase_path`, exit when `initrd_release` disappears. The
/// passphrase file is read once at startup into a `Zeroizing` buffer
/// that's reused for every request; the buffer is zeroed on drop. The
/// file on disk is not touched (the caller's job is to wipe it if
/// needed — typically a tmpfs-backed file consumed once).
fn run_noninteractive(
    passphrase_path: &Path,
    ask_dir: &Path,
    initrd_release: &Path,
) -> anyhow::Result<()> {
    let passphrase: Zeroizing<Vec<u8>> = Zeroizing::new(
        std::fs::read(passphrase_path)
            .with_context(|| format!("read passphrase file {}", passphrase_path.display()))?,
    );
    tracing::info!(
        passphrase_bytes = passphrase.len(),
        ask_dir = %ask_dir.display(),
        "non-interactive responder ready"
    );

    loop {
        if !initrd_release.exists() {
            tracing::info!("pivot complete (/etc/initrd-release gone); exiting");
            return Ok(());
        }
        match outstanding_requests(ask_dir) {
            Ok(requests) => {
                for path in requests {
                    match AskFile::read(&path) {
                        Ok(ask) => match ask.send_passphrase(&passphrase) {
                            Ok(()) => tracing::info!(
                                request = %path.display(),
                                socket = %ask.response_socket.display(),
                                "responded to ask-password request ({} bytes)",
                                passphrase.len()
                            ),
                            Err(e) => tracing::warn!(
                                request = %path.display(),
                                error = %e,
                                "send_passphrase failed"
                            ),
                        },
                        Err(e) => tracing::warn!(
                            request = %path.display(),
                            error = %e,
                            "parse ask-password file"
                        ),
                    }
                }
            }
            Err(e) => tracing::warn!(error = %e, "scan ask-password dir"),
        }
        std::thread::sleep(POLL_INTERVAL);
    }
}

struct State {
    registry_state: RegistryState,
    output_state: OutputState,
    seat_state: SeatState,
    shm: Shm,
    _compositor_state: CompositorState,
    _xdg_shell: XdgShell,
    window: Window,
    pool: SlotPool,
    /// `wl_keyboard` for the bound seat. Set when the seat advertises
    /// the Keyboard capability; cleared if it goes away.
    keyboard: Option<WlKeyboard>,
    width: u32,
    height: u32,
    /// `true` once we have received the first `configure` event; the
    /// surface is committable from this point on.
    configured: bool,
    /// Typed passphrase, zeroed on drop / clear. Submitted to the
    /// agent socket on Enter, cleared after submit.
    passphrase: Zeroizing<Vec<u8>>,
    /// The ask-password request currently being prompted, if any.
    /// `None` means we're idle — no outstanding request, splash-only.
    current_request: Option<CurrentRequest>,
    /// `true` while at least one character has been typed since the
    /// current prompt opened. Toggles the surface tint so the user
    /// gets a minimal "input is being received" signal even without
    /// text rendering.
    active: bool,
    /// `/run/systemd/ask-password/` (override-friendly for unit
    /// tests; defaults to the canonical path).
    ask_dir: PathBuf,
    /// `/etc/initrd-release` (override-friendly for unit tests).
    initrd_release: PathBuf,
    /// Set by the polling timer when the pivot has completed; the
    /// main loop observes this and tears the event loop down.
    should_exit: bool,
}

struct CurrentRequest {
    path: PathBuf,
    ask: AskFile,
}

impl State {
    /// Scan for outstanding ask-password requests. Called from the
    /// 200ms polling timer.
    ///
    /// State machine:
    /// - Idle + new request → enter prompt state, paint active tint.
    /// - Prompt + request file still present → unchanged.
    /// - Prompt + request file gone → idle (someone else answered,
    ///   or systemd cancelled the request); clear passphrase.
    /// - Always: if `/etc/initrd-release` disappears, send cancel for
    ///   any in-flight request, request loop exit.
    fn poll_external_state(&mut self) {
        if !self.initrd_release.exists() {
            tracing::info!("pivot complete (/etc/initrd-release gone); exiting");
            if let Some(ref req) = self.current_request
                && let Err(e) = req.ask.send_cancel()
            {
                tracing::warn!(error = %e, "send_cancel during pivot-exit");
            }
            self.should_exit = true;
            return;
        }

        if let Some(ref req) = self.current_request
            && !req.path.exists()
        {
            tracing::info!(
                request = %req.path.display(),
                "ask-password file disappeared (cancelled or answered elsewhere)"
            );
            self.current_request = None;
            self.active = false;
            self.passphrase.clear();
            if self.configured {
                self.paint();
            }
            return;
        }

        if self.current_request.is_none() {
            match outstanding_requests(&self.ask_dir) {
                Ok(requests) => {
                    if let Some(path) = requests.into_iter().next() {
                        match AskFile::read(&path) {
                            Ok(ask) => {
                                tracing::info!(
                                    request = %path.display(),
                                    socket = %ask.response_socket.display(),
                                    "new ask-password request"
                                );
                                self.current_request = Some(CurrentRequest { path, ask });
                                self.passphrase.clear();
                                self.active = false;
                                if self.configured {
                                    self.paint();
                                }
                            }
                            Err(e) => {
                                tracing::warn!(
                                    request = %path.display(),
                                    error = %e,
                                    "failed to parse ask-password request"
                                );
                            }
                        }
                    }
                }
                Err(e) => {
                    tracing::warn!(error = %e, "scan ask-password dir");
                }
            }
        }
    }

    /// Append a typed printable character to the passphrase buffer.
    fn type_char(&mut self, b: u8) {
        if self.current_request.is_none() {
            return;
        }
        self.passphrase.push(b);
        if !self.active {
            self.active = true;
            if self.configured {
                self.paint();
            }
        }
    }

    /// Backspace.
    fn backspace(&mut self) {
        if self.current_request.is_none() {
            return;
        }
        self.passphrase.pop();
    }

    /// Submit the typed passphrase to the agent and clear.
    fn submit(&mut self) {
        let Some(req) = self.current_request.take() else {
            return;
        };
        match req.ask.send_passphrase(&self.passphrase) {
            Ok(()) => tracing::info!(
                request = %req.path.display(),
                "passphrase submitted ({} bytes)",
                self.passphrase.len(),
            ),
            Err(e) => tracing::warn!(
                request = %req.path.display(),
                error = %e,
                "send_passphrase failed; the request file may be rewritten by systemd"
            ),
        }
        self.passphrase.clear();
        self.active = false;
        if self.configured {
            self.paint();
        }
    }

    /// Cancel the current prompt (user pressed ESC).
    fn cancel(&mut self) {
        let Some(req) = self.current_request.take() else {
            return;
        };
        if let Err(e) = req.ask.send_cancel() {
            tracing::warn!(
                request = %req.path.display(),
                error = %e,
                "send_cancel failed"
            );
        }
        self.passphrase.clear();
        self.active = false;
        if self.configured {
            self.paint();
        }
    }

    fn paint(&mut self) {
        if self.width == 0 || self.height == 0 {
            return;
        }
        let w = self.width as i32;
        let h = self.height as i32;
        let stride = w * 4;
        let Ok((buffer, canvas)) = self.pool.create_buffer(w, h, stride, Format::Argb8888) else {
            tracing::warn!("create_buffer failed; skipping paint");
            return;
        };
        let color = if self.active {
            ACTIVE_BGRA
        } else {
            PROMPT_BGRA
        };
        for px in canvas.chunks_exact_mut(4) {
            px.copy_from_slice(&color);
        }
        let surface = self.window.wl_surface();
        surface.damage_buffer(0, 0, w, h);
        if let Err(e) = buffer.attach_to(surface) {
            tracing::warn!(error = %e, "attach_to failed");
            return;
        }
        self.window.commit();
    }
}

// ─── Wayland handler impls ──────────────────────────────────────────

impl WindowHandler for State {
    fn request_close(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &Window) {
        // halmasuit doesn't send close requests in v1 (no decorations,
        // no client-driven close). If one ever arrives, treat it as
        // ESC: cancel current prompt and exit.
        self.cancel();
        self.should_exit = true;
    }

    fn configure(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _window: &Window,
        configure: WindowConfigure,
        _serial: u32,
    ) {
        let w = configure.new_size.0.map_or(1280, std::num::NonZeroU32::get);
        let h = configure.new_size.1.map_or(800, std::num::NonZeroU32::get);
        self.width = w;
        self.height = h;
        self.configured = true;
        self.paint();
    }
}

impl CompositorHandler for State {
    fn scale_factor_changed(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_surface::WlSurface,
        _: i32,
    ) {
    }
    fn transform_changed(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_surface::WlSurface,
        _: wl_output::Transform,
    ) {
    }
    fn frame(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_surface::WlSurface, _: u32) {}
    fn surface_enter(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_surface::WlSurface,
        _: &wl_output::WlOutput,
    ) {
    }
    fn surface_leave(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_surface::WlSurface,
        _: &wl_output::WlOutput,
    ) {
    }
}

impl OutputHandler for State {
    fn output_state(&mut self) -> &mut OutputState {
        &mut self.output_state
    }
    fn new_output(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {}
    fn update_output(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {}
    fn output_destroyed(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {}
}

impl ShmHandler for State {
    fn shm_state(&mut self) -> &mut Shm {
        &mut self.shm
    }
}

impl SeatHandler for State {
    fn seat_state(&mut self) -> &mut SeatState {
        &mut self.seat_state
    }
    fn new_seat(&mut self, _: &Connection, _: &QueueHandle<Self>, _: WlSeat) {}
    fn new_capability(
        &mut self,
        _conn: &Connection,
        qh: &QueueHandle<Self>,
        seat: WlSeat,
        capability: Capability,
    ) {
        if capability == Capability::Keyboard && self.keyboard.is_none() {
            match self.seat_state.get_keyboard(qh, &seat, None) {
                Ok(kbd) => self.keyboard = Some(kbd),
                Err(e) => tracing::warn!(error = %e, "get_keyboard failed"),
            }
        }
    }
    fn remove_capability(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _seat: WlSeat,
        capability: Capability,
    ) {
        if capability == Capability::Keyboard
            && let Some(kbd) = self.keyboard.take()
        {
            kbd.release();
        }
    }
    fn remove_seat(&mut self, _: &Connection, _: &QueueHandle<Self>, _: WlSeat) {}
}

impl KeyboardHandler for State {
    fn enter(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &WlKeyboard,
        _: &wl_surface::WlSurface,
        _: u32,
        _: &[u32],
        _: &[Keysym],
    ) {
    }

    fn leave(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &WlKeyboard,
        _: &wl_surface::WlSurface,
        _: u32,
    ) {
    }

    fn press_key(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _kbd: &WlKeyboard,
        _serial: u32,
        event: KeyEvent,
    ) {
        let keysym = event.keysym;
        // xkbcommon constants. Avoid the dep by matching on the raw
        // values of the few keysyms we care about. The sctk
        // `seat::keyboard::Keysym` type wraps the raw u32; we read it
        // via .raw().
        match keysym.raw() {
            // XKB_KEY_Return = 0xff0d, XKB_KEY_KP_Enter = 0xff8d.
            0xff0d | 0xff8d => self.submit(),
            // XKB_KEY_Escape = 0xff1b.
            0xff1b => self.cancel(),
            // XKB_KEY_BackSpace = 0xff08.
            0xff08 => self.backspace(),
            _ => {
                // Use the UTF-8 character (already translated by
                // xkbcommon for us by sctk). Only accept printable
                // single-byte ASCII for the LUKS prompt — multi-byte
                // UTF-8 is fine to admit, but the LUKS keyfile is the
                // UTF-8 byte sequence directly, so passing bytes
                // through is correct.
                if let Some(utf8) = event.utf8 {
                    for &b in utf8.as_bytes() {
                        // Reject control chars (everything < 0x20 except
                        // what we've handled above); accept printable
                        // ASCII + UTF-8 continuation bytes.
                        if b >= 0x20 || b >= 0x80 {
                            self.type_char(b);
                        }
                    }
                }
            }
        }
    }

    fn release_key(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &WlKeyboard,
        _: u32,
        _: KeyEvent,
    ) {
    }

    fn repeat_key(
        &mut self,
        conn: &Connection,
        qh: &QueueHandle<Self>,
        kbd: &WlKeyboard,
        serial: u32,
        event: KeyEvent,
    ) {
        // xkbcommon's per-key auto-repeat: re-deliver the keystroke at
        // the seat's repeat rate. We treat repeats identically to
        // press_key — the same Backspace/printable-byte handling applies.
        self.press_key(conn, qh, kbd, serial, event);
    }

    fn update_modifiers(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &WlKeyboard,
        _: u32,
        _: Modifiers,
        _: RawModifiers,
        _: u32,
    ) {
    }

    fn update_keymap(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &WlKeyboard,
        _: smithay_client_toolkit::seat::keyboard::Keymap<'_>,
    ) {
    }

    fn update_repeat_info(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &WlKeyboard,
        _: RepeatInfo,
    ) {
    }
}

impl ProvidesRegistryState for State {
    fn registry(&mut self) -> &mut RegistryState {
        &mut self.registry_state
    }
    registry_handlers![OutputState, SeatState];
}

delegate_compositor!(State);
delegate_output!(State);
delegate_shm!(State);
delegate_seat!(State);
delegate_keyboard!(State);
delegate_xdg_shell!(State);
delegate_xdg_window!(State);
delegate_registry!(State);
