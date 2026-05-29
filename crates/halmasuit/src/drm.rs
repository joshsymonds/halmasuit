// halmasuit/src/drm.rs — DRM backend: GLES + GBM + DrmCompositor.
//
// Production renderer wiring. halmasuit owns the DRM device, runs a
// GLES renderer through smithay's `DrmCompositor`, and composites
// the wallpaper plane as the bottom-most element on every frame —
// including frame 0, before any wl_client connects. There is no
// pre-client solid phase (epic amendment G1/R3/R6): the GL clear
// color is a transient fully covered by the wallpaper on every
// frame. wlr-layer-shell surfaces, the foreground toplevel, and (in
// halmasuit-debug) frame_audit layer on top of this same pipeline.
//
// The wallpaper plane is owned by the [`WallpaperEngine`](crate::wallpaper::WallpaperEngine);
// three pluggable backends (image / shader / video) share a single
// trait surface and synchronously commit their first renderable
// state before halmasuit's first composite. Phase-A wires the image
// backend; shader and video are typed stubs the wallpaper-engine
// epic fills in.
//
// Pattern lifted from niri's `src/backend/tty.rs` + smithay's anvil
// example at the pinned `ff5fa7df` rev, simplified to:
//
//   * Single GPU, single output, single CRTC (no MultiRenderer, no
//     udev hot-plug, no multi-monitor)
//   * Render loop driven by `DrmEvent::VBlank` from the
//     `DrmDeviceNotifier` calloop source
//   * The wallpaper plane is the bottom-most render element; the
//     front-to-back element list composites every surface over it

use std::io;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use thiserror::Error;

use smithay::backend::allocator::Fourcc;
use smithay::backend::allocator::gbm::{GbmAllocator, GbmBufferFlags, GbmDevice};
use smithay::backend::drm::compositor::{DrmCompositor, FrameFlags};
use smithay::backend::drm::exporter::gbm::GbmFramebufferExporter;
use smithay::backend::drm::{DrmDevice, DrmDeviceFd, DrmEvent, DrmSurface};
use smithay::backend::egl::{EGLContext, EGLDisplay};
use smithay::backend::renderer::Color32F;
use smithay::backend::renderer::element::memory::{
    MemoryRenderBuffer, MemoryRenderBufferRenderElement,
};
use smithay::backend::renderer::element::render_elements;
use smithay::backend::renderer::element::surface::WaylandSurfaceRenderElement;
use smithay::backend::renderer::element::texture::TextureRenderElement;
use smithay::backend::renderer::gles::{GlesRenderer, GlesTexture};
use smithay::output::{Mode as OutputMode, Output, OutputModeSource, PhysicalProperties, Subpixel};
use smithay::reexports::drm::Device as BaseDrmDevice;
use smithay::reexports::drm::control::Device as ControlDevice;
use smithay::reexports::drm::control::{connector, crtc};
use smithay::utils::DeviceFd;

use crate::wallpaper::{
    ImageBackend, ShaderBackend, VideoBackend, WallpaperBackend, WallpaperConfig, WallpaperEngine,
    wallpaper_slot,
};

/// The transient GL clear color, as the RGB bytes of an `#0a0014`
/// pixel: `red=0x0A, green=0x00, blue=0x14`. Under epic amendment
/// G1/R6 there is no observable pre-client solid phase — the
/// wallpaper plane covers the entire output on every frame including
/// frame 0, so this clear is never scanned out. It is retained as
/// THE single source of truth shared by the renderer clear
/// (`HALMASUIT_BRAND_CLEAR` = `xrgb_le(CLEAR_RGB[0..3])`) and
/// `frame_audit`/`offscreen`: the no-flash audit treats a pixel
/// byte-equal to this as the uncovered sentinel ("wallpaper not
/// covering"), so renderer and audit must never disagree about its
/// value. This module is compiled into BOTH `halmasuit` and
/// `halmasuit-debug` (not `frame_audit`-gated), the only place all
/// consumers can share it. Keep the value stable: the audit math
/// keys on it as the sentinel even though it is never visible.
pub const CLEAR_RGB: [u8; 3] = [0x0A, 0x00, 0x14];

/// Color formats we'll accept for scanout. ARGB2101010 first (preferred
/// 10-bit), then 8-bit ARGB8888 / ABGR8888 as fallbacks. Matches anvil's
/// list and is widely supported across virtio-gpu and real GPUs.
const SUPPORTED_COLOR_FORMATS: &[Fourcc] = &[
    Fourcc::Abgr2101010,
    Fourcc::Argb2101010,
    Fourcc::Abgr8888,
    Fourcc::Argb8888,
];

/// Encode `#RRGGBB` as XRGB8888 little-endian for filling a dumb
/// buffer (`[B, G, R, X]`). Retained from the B.1 slice so existing
/// callers / unit tests continue to compile. The GLES render path
/// uses [`xrgb_le_to_color32f`] instead; the dumb-buffer path is no
/// longer used by halmasuit's production scanout.
///
/// `const fn` so callers can build constants at compile time. Pinned
/// by `xrgb_le_pins_byte_order` in tests below — any channel
/// transpose trips a fast unit test.
#[must_use]
pub const fn xrgb_le(r: u8, g: u8, b: u8) -> [u8; 4] {
    [b, g, r, 0]
}

/// Convert an XRGB8888 little-endian byte array (the storage form
/// from [`xrgb_le`]) into a `Color32F` for smithay's `render_frame`.
/// Reads back the R/G/B channels by index, normalizes to [0.0, 1.0].
#[must_use]
pub fn xrgb_le_to_color32f(bytes: [u8; 4]) -> Color32F {
    Color32F::new(
        f32::from(bytes[2]) / 255.0,
        f32::from(bytes[1]) / 255.0,
        f32::from(bytes[0]) / 255.0,
        1.0,
    )
}

// ─── DRM device selection ────────────────────────────────────────────
//
// Halmasuit historically opened `/dev/dri/card0` directly, which works
// on single-DRM-device hosts (every test VM) but fails on real hardware
// where the kernel may register multiple DRM devices in non-deterministic
// order (e.g., gnomon: simpledrm + chipset-side DRM + NVIDIA — card0 ends
// up being the wrong device). `DrmDeviceSpec` + `resolve_drm_device`
// replace that hardcode with a three-mode selector:
//
//   * `Auto`  — iterate `/dev/dri/card*`, pick the first with at least
//                one `Connection::Connected` connector (libdrm probe).
//   * `Path`  — open the supplied path as-is.
//   * `Pci`   — match by PCI BDF via `/sys/class/drm/cardN/device`.
//
// All three modes wrap a deadline-bounded retry: udev may not have
// finished creating the device node when halmasuit's initramfs unit
// fires (`systemd-modules-load` deactivates BEFORE udev drains its event
// queue), so the resolver polls every 100ms up to `deadline`, retrying
// on `ErrorKind::NotFound` and propagating any other error immediately.

/// How the caller specifies which DRM device halmasuit should open.
///
/// Parsed from the `HALMASUIT_DRM_DEVICE` env var:
///   * empty / unset → [`DrmDeviceSpec::Auto`]
///   * starts with `pci:` → [`DrmDeviceSpec::Pci`] with the parsed BDF
///   * anything else → [`DrmDeviceSpec::Path`]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DrmDeviceSpec {
    /// Iterate `/dev/dri/card*` and pick the first card with at least
    /// one connector reporting `Connection::Connected`.
    Auto,
    /// Use the exact path as-is. The caller knows which `cardN` it
    /// wants (e.g., explicit gnomon-side override during early bringup
    /// before the operator has measured the PCI BDF).
    Path(PathBuf),
    /// Look up the matching `/dev/dri/cardN` by reading
    /// `/sys/class/drm/cardN/device` and comparing against the BDF.
    /// Stable across reboots regardless of kernel probe order.
    Pci(PciBdf),
}

/// A validated PCI BDF in `DDDD:BB:DD.F` format (domain `[0-9a-f]{4}`,
/// bus `[0-9a-f]{2}`, device `[0-9a-f]{2}`, function `[0-7]`).
///
/// Stored normalized to lowercase so the comparison against
/// `/sys/class/drm/cardN/device` symlinks (which Linux emits in
/// lowercase) is a simple string match.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PciBdf(String);

/// Parser-rejection reasons for [`PciBdf::parse`].
///
/// The raw input is deliberately NOT included in the `Display` impl.
/// `HALMASUIT_DRM_DEVICE` is set from a trusted source (the NixOS unit
/// `environment =` block) today, but if the source ever changes (e.g.,
/// consumed from a less-trusted env pass-through), echoing the raw
/// input into journald would leak attacker-controlled bytes into logs.
/// Callers wanting the raw value back must capture it themselves
/// before calling `parse`.
#[derive(Debug, Error)]
pub enum BdfParseError {
    /// The string doesn't match the `DDDD:BB:DD.F` shape.
    #[error("PCI BDF must be 'DDDD:BB:DD.F'")]
    BadFormat,
    /// The function digit is outside `0..=7`.
    #[error("PCI BDF function digit must be 0-7")]
    BadFunction,
}

impl PciBdf {
    /// Parse a `DDDD:BB:DD.F` string. Accepts both hex cases; stores
    /// normalized to lowercase.
    ///
    /// # Errors
    ///
    /// Returns [`BdfParseError::BadFormat`] if the structural shape is
    /// wrong (missing separators, wrong digit count, non-hex digits).
    /// Returns [`BdfParseError::BadFunction`] if the function digit is
    /// `>= 8` (PCI functions are 3 bits).
    pub fn parse(s: &str) -> Result<Self, BdfParseError> {
        // Shape: 4 hex : 2 hex : 2 hex . 1 hex
        let (domain_bus_dev, function) = s.rsplit_once('.').ok_or(BdfParseError::BadFormat)?;
        if function.len() != 1 {
            return Err(BdfParseError::BadFormat);
        }
        let function_digit =
            u8::from_str_radix(function, 16).map_err(|_| BdfParseError::BadFormat)?;
        if function_digit > 7 {
            return Err(BdfParseError::BadFunction);
        }

        let parts: Vec<&str> = domain_bus_dev.split(':').collect();
        if parts.len() != 3 {
            return Err(BdfParseError::BadFormat);
        }
        let [domain, bus, device] = [parts[0], parts[1], parts[2]];
        if domain.len() != 4 || bus.len() != 2 || device.len() != 2 {
            return Err(BdfParseError::BadFormat);
        }
        // Validate each segment is pure hex.
        u32::from_str_radix(domain, 16).map_err(|_| BdfParseError::BadFormat)?;
        u8::from_str_radix(bus, 16).map_err(|_| BdfParseError::BadFormat)?;
        u8::from_str_radix(device, 16).map_err(|_| BdfParseError::BadFormat)?;

        Ok(Self(s.to_ascii_lowercase()))
    }

    /// The BDF as a lowercase string (e.g., `0000:01:00.0`).
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl DrmDeviceSpec {
    /// Parse a `HALMASUIT_DRM_DEVICE` env value into a spec.
    ///
    /// Empty input → [`DrmDeviceSpec::Auto`]. A `pci:`-prefixed value
    /// is parsed via [`PciBdf::parse`]. Anything else is treated as a
    /// path and must look like `/dev/dri/cardN` (a literal DRM device
    /// path) — the parser rejects other shapes with [`PathShapeError`]
    /// as defense-in-depth against environments where the env var
    /// might come from a less-trusted source than the unit's static
    /// `environment =` block.
    ///
    /// # Errors
    ///
    /// Returns [`BdfParseError`] if a `pci:`-prefixed value has an
    /// unparsable BDF, or [`PathShapeError`] (boxed via
    /// [`DrmDeviceSpecParseError`]) if a path-shaped value doesn't
    /// match `/dev/dri/card[0-9]+`.
    pub fn from_env_value(s: &str) -> Result<Self, DrmDeviceSpecParseError> {
        if s.is_empty() {
            Ok(Self::Auto)
        } else if let Some(rest) = s.strip_prefix("pci:") {
            Ok(Self::Pci(PciBdf::parse(rest)?))
        } else if is_dev_dri_card_path(s) {
            Ok(Self::Path(PathBuf::from(s)))
        } else {
            Err(DrmDeviceSpecParseError::BadPath(PathShapeError))
        }
    }
}

/// Reject anything that isn't structurally `/dev/dri/card[0-9]+`.
///
/// Implementing without regex to avoid a dep just for this; the shape
/// is fixed and trivial.
fn is_dev_dri_card_path(s: &str) -> bool {
    let Some(rest) = s.strip_prefix("/dev/dri/card") else {
        return false;
    };
    !rest.is_empty() && rest.bytes().all(|b| b.is_ascii_digit())
}

/// Errors from [`DrmDeviceSpec::from_env_value`].
///
/// The raw input string is deliberately NOT included in the message —
/// `HALMASUIT_DRM_DEVICE` is set from a trusted source today, but if
/// the source ever changes (e.g., consumed from a less-trusted env
/// pass-through), logging the raw input could surface attacker-
/// controlled bytes into journald.
#[derive(Debug, Error)]
pub enum DrmDeviceSpecParseError {
    /// The `pci:`-prefixed value failed BDF validation.
    #[error("HALMASUIT_DRM_DEVICE pci: value did not parse as a BDF")]
    BadBdf(#[from] BdfParseError),
    /// The path-shaped value wasn't `/dev/dri/card[0-9]+`.
    #[error("HALMASUIT_DRM_DEVICE path must match /dev/dri/card[0-9]+")]
    BadPath(#[from] PathShapeError),
}

/// Marker error type — see [`DrmDeviceSpecParseError::BadPath`].
#[derive(Debug, Error)]
#[error("path shape rejected")]
pub struct PathShapeError;

/// How often the retry loop polls when the device isn't there yet.
const RESOLVE_POLL_INTERVAL: Duration = Duration::from_millis(100);

/// Resolve a [`DrmDeviceSpec`] to a concrete `/dev/dri/cardN` path,
/// retrying on `NotFound`-class errors until either resolution
/// succeeds or `deadline` elapses (polls every
/// [`RESOLVE_POLL_INTERVAL`]).
///
/// `Auto` iterates `/dev/dri/card*`, opens each, and picks the first
/// card with at least one connector reporting `Connection::Connected`.
/// `Path` returns the supplied path once it exists. `Pci` looks up the
/// matching card via `/sys/class/drm/cardN/device`.
///
/// Non-`NotFound` errors (e.g., `EACCES` reading `/dev/dri/`) propagate
/// immediately without retry — the caller will not recover by polling.
///
/// # Errors
///
/// Returns [`io::Error`] of kind `NotFound` if the deadline elapses
/// without resolution; propagates any other I/O error from the first
/// failing call.
pub fn resolve_drm_device(spec: &DrmDeviceSpec, deadline: Duration) -> io::Result<PathBuf> {
    let start = Instant::now();
    // First probe is unconditional — when the device is already there
    // (the common case), avoid any sleep before resolution.
    if let Some(p) = try_resolve_once(spec)? {
        return Ok(p);
    }
    // Subsequent probes: sleep, then check deadline BEFORE probing.
    // This keeps the probe count bounded by ceil(deadline / interval)
    // rather than doing one extra probe AFTER the deadline trips.
    loop {
        std::thread::sleep(RESOLVE_POLL_INTERVAL);
        if start.elapsed() >= deadline {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                format!("no DRM device satisfies {spec:?} within {deadline:?}"),
            ));
        }
        if let Some(p) = try_resolve_once(spec)? {
            return Ok(p);
        }
    }
}

/// Single pass at resolution. `Ok(None)` means "not yet — retry";
/// `Err(_)` propagates immediately to the caller.
fn try_resolve_once(spec: &DrmDeviceSpec) -> io::Result<Option<PathBuf>> {
    match spec {
        DrmDeviceSpec::Auto => find_auto(),
        DrmDeviceSpec::Path(p) => {
            // `symlink_metadata` (vs. `Path::exists` → `metadata`)
            // does not traverse symlinks. We're addressing a literal
            // DRM device node; a symlink in `/dev/dri/` (whatever its
            // target) is rejected. Defense-in-depth against a future
            // wiring that might let a less-trusted source set the env.
            match std::fs::symlink_metadata(p) {
                Ok(md) if md.file_type().is_symlink() => Ok(None),
                Ok(_) => Ok(Some(p.clone())),
                Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
                Err(e) => Err(e),
            }
        }
        DrmDeviceSpec::Pci(bdf) => find_by_bdf(bdf),
    }
}

/// Iterate `/dev/dri/card*` and return the first card with a
/// connected connector. Opens each card read-write (libdrm's
/// connection probe needs that). On error opening a card, skip it
/// rather than fail — a card that's busy or otherwise unopenable
/// shouldn't block discovery of a usable card.
fn find_auto() -> io::Result<Option<PathBuf>> {
    // Honor the resolver contract: NotFound is "not yet — caller
    // retries"; any other error (EACCES, EIO, EMFILE…) propagates so
    // the operator sees the real failure instead of a misleading
    // deadline-exhausted NotFound at the end of the 10s wait. Matches
    // `find_by_bdf`'s shape below.
    let read_dir = match std::fs::read_dir("/dev/dri") {
        Ok(d) => d,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e),
    };

    let mut cards: Vec<PathBuf> = Vec::new();
    for entry in read_dir {
        // Same posture for per-entry I/O errors: propagate rather
        // than silently skip.
        let entry = entry?;
        let name = entry.file_name();
        let Some(name_str) = name.to_str() else {
            continue;
        };
        if !name_str.starts_with("card") {
            continue;
        }
        if name_str[4..].parse::<u32>().is_err() {
            continue;
        }
        cards.push(entry.path());
    }
    cards.sort();

    // Trace the discovered card list so the multi-DRM VM test can
    // assert Auto-mode actually iterated the directory (rather than
    // a regression that hardcoded /dev/dri/card0 returning the same
    // card without probing anything).
    tracing::info!(?cards, "DRM auto-discover: enumerated card candidates");

    for card in cards {
        if card_has_connected_connector(&card)? {
            return Ok(Some(card));
        }
    }
    Ok(None)
}

/// Open `path` and check whether any connector reports
/// `Connection::Connected`. Returns `Ok(false)` for "no connected
/// connector"; opens that fail with `NotFound` translate to
/// `Ok(false)` (treated as "skip this card, try the next"). Other
/// errors propagate.
fn card_has_connected_connector(path: &Path) -> io::Result<bool> {
    use std::os::fd::OwnedFd;

    let file = match std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)
    {
        Ok(f) => f,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(e) if e.kind() == io::ErrorKind::PermissionDenied => return Ok(false),
        Err(e) => return Err(e),
    };
    let owned_fd: OwnedFd = file.into();
    let device_fd = DrmDeviceFd::new(DeviceFd::from(owned_fd));

    // ControlDevice's `resource_handles` returns the connector list;
    // `get_connector` reads each connector's current state.
    let Ok(handles) = ControlDevice::resource_handles(&device_fd) else {
        return Ok(false);
    };
    for &handle in handles.connectors() {
        if let Ok(info) = ControlDevice::get_connector(&device_fd, handle, false)
            && info.state() == connector::State::Connected
        {
            return Ok(true);
        }
    }
    Ok(false)
}

/// Walk `/sys/class/drm/card*/device` and return the `/dev/dri/cardN`
/// whose symlink target's basename matches `bdf`. `Ok(None)` means
/// "no match yet" (caller retries — udev may not have populated the
/// sysfs link yet).
fn find_by_bdf(bdf: &PciBdf) -> io::Result<Option<PathBuf>> {
    let read_dir = match std::fs::read_dir("/sys/class/drm") {
        Ok(d) => d,
        // /sys/class/drm hasn't been populated yet — caller retries.
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e),
    };
    let target_bdf = bdf.as_str();

    for entry in read_dir {
        // Propagate I/O errors reading the directory (e.g., EACCES)
        // instead of swallowing them — the caller cannot recover by
        // polling.
        let entry = entry?;
        let name_os = entry.file_name();
        let Some(name) = name_os.to_str() else {
            continue;
        };
        if !name.starts_with("card") || name[4..].parse::<u32>().is_err() {
            continue;
        }

        // The `device` symlink may legitimately be absent on partial
        // udev population — treat NotFound as "skip this card, try the
        // next" and propagate other errors.
        let device_symlink = entry.path().join("device");
        let target = match std::fs::read_link(&device_symlink) {
            Ok(t) => t,
            Err(e) if e.kind() == io::ErrorKind::NotFound => continue,
            Err(e) => return Err(e),
        };
        let Some(basename_os) = target.file_name() else {
            continue;
        };
        let Some(basename) = basename_os.to_str() else {
            continue;
        };
        if basename.eq_ignore_ascii_case(target_bdf) {
            return Ok(Some(PathBuf::from(format!("/dev/dri/{name}"))));
        }
    }
    Ok(None)
}

render_elements! {
    /// One frame's render elements. smithay element lists are
    /// front-to-back (index 0 = topmost, drawn last). `Surface` wraps
    /// a committed wl_client subtree (including client-attached
    /// cursor surface trees set via `wl_pointer.set_cursor` — they
    /// share the variant; smithay's `Kind::Cursor` marker on the
    /// inner element handles cursor-specific damage tracking).
    /// `Wallpaper` / `WallpaperShader` are halmasuit's internal
    /// full-output background plane (image-backed and shader-backed
    /// respectively), always the LAST element so every surface
    /// composites over it (epic G1/R3/R6). Exactly one wallpaper
    /// variant is produced per frame — the engine's active backend
    /// picks which. `Memory` is a memory-buffer-backed element — used
    /// for BOTH the named-cursor pixmap from the loaded xcursor theme
    /// (R8b-render; prepended at INDEX 0, topmost) AND the Epic #71
    /// diagnostic overlay (R3.2/R-honest.6): a full-output buffer with
    /// a semitransparent dark backdrop + the diagnostic text (phase,
    /// broker, window list, journal tail) rasterized via the
    /// console-font blitter, composited above every surface but BELOW
    /// the cursor. The two share one variant because they're the same
    /// inner type (the `render_elements!` macro requires distinct
    /// per-variant types); z-order is set by position in the element
    /// list, not the variant.
    pub SceneElement<=GlesRenderer>;
    Surface         = WaylandSurfaceRenderElement<GlesRenderer>,
    Wallpaper       = TextureRenderElement<GlesTexture>,
    WallpaperShader = smithay::backend::renderer::gles::element::PixelShaderElement,
    Memory          = MemoryRenderBufferRenderElement<GlesRenderer>,
}

/// R8b-render cursor state. Lives on `DrmBackend` (next to the
/// renderer and the wallpaper engine) because cursor pixmap upload
/// uses the same renderer; main.rs is sans-Renderer and forwards
/// state changes via `set_cursor_status` / `set_pointer_location`.
struct CursorRenderState {
    theming: crate::cursor::CursorTheming,
    status: smithay::input::pointer::CursorImageStatus,
    /// Current pointer position in PHYSICAL pixels (= LOGICAL for
    /// halmasuit's 1:1 scale single output). `(0, 0)` until the first
    /// pointer-motion event arrives.
    pointer_loc: smithay::utils::Point<i32, smithay::utils::Physical>,
    /// `true` once a pointer event has arrived. The cursor render
    /// path no-ops while this is `false` — a system compositor at
    /// idle (greeter waiting, no user input yet) shows no cursor,
    /// matching the UX of every production display server and
    /// keeping deterministic boot-frame goldens cursor-free.
    has_moved: bool,
    /// Cached `(name, hotspot, buffer)` for the currently displayed
    /// Named cursor. Re-baked from the xcursor theme when `status`
    /// changes to a different name.
    cached: Option<CachedNamed>,
    started_at: std::time::Instant,
}

struct CachedNamed {
    name: String,
    hotspot: (i32, i32),
    buffer: MemoryRenderBuffer,
}

/// One physical output: its smithay `Output` (logical — owns the
/// per-output `LayerMap`, the current mode, and the position in the
/// global compositor space), the CRTC scanning it out (the vblank
/// routing key), and the `DrmCompositor` driving that CRTC's surface.
///
/// NVIDIA's open kernel module exposes one CRTC ("head") per connector
/// and never sets `possible_clones`, so binding two connectors to one
/// CRTC is rejected by the kernel at the first atomic commit (inside
/// `DrmCompositor::new`'s test commit). Per-CRTC — one of these per
/// connector — is the only viable multi-output path, and it is what
/// niri and cosmic-comp do. A single-connector substrate (the headless
/// VM test matrix, single-monitor laptops) degrades to a Vec of one,
/// byte-identical to the prior single-output behavior.
struct OutputSurface {
    /// Logical output. Cloned into `HalmasuitState.outputs` (Arc bump)
    /// so the layer-shell / frame-callback / global-advertisement paths
    /// share the same underlying output (LayerMap is keyed by the
    /// output's internal id, shared across clones).
    output: Output,
    /// The CRTC this output scans out on. `DrmEvent::VBlank(crtc)` is
    /// routed back to this surface by matching this handle.
    crtc: crtc::Handle,
    /// The smithay `DrmCompositor` driving this CRTC's surface. Owns the
    /// `DrmSurface`, a per-output GBM allocator + framebuffer exporter,
    /// and its own swapchain. Dropping it releases the surface (and thus
    /// the CRTC).
    compositor: DrmCompositor<
        GbmAllocator<DrmDeviceFd>,
        GbmFramebufferExporter<DrmDeviceFd>,
        (),
        DrmDeviceFd,
    >,
}

/// The full GLES + GBM stack plus one `DrmCompositor` per connected
/// connector (each on its own dedicated CRTC). Pinned for the process
/// lifetime in `HalmasuitState`. Dropping this value releases master,
/// tears down EGL, and lets the kernel reset every CRTC.
pub struct DrmBackend {
    /// One per connected connector — extended multi-output. The first
    /// entry is the primary (leftmost, at x=0; honors
    /// `services.halmasuit.rendering.primaryOutput`). All share the
    /// `renderer` below and the same GBM device (each holds its own
    /// allocator/exporter clone of it). Always non-empty (`setup`
    /// errors if zero outputs initialize).
    outputs: Vec<OutputSurface>,
    /// GLES renderer bound to the GBM device's EGL display, SHARED
    /// across every output's compositor. Used by `render_frame` to
    /// clear + composite each output's scene every vblank.
    pub renderer: GlesRenderer,
    /// The wallpaper engine — owns the active backend (image / shader
    /// / video) and builds the bottom-most render element every frame
    /// (epic G1/R3/R6). When no backend is configured the engine
    /// produces no element — the legacy clear-only path for
    /// non-visual integration tests; production/visual deployments
    /// always configure one.
    wallpaper: WallpaperEngine,
    /// R8b-render cursor state. `theming` is the loaded xcursor theme
    /// (or procedural fallback); `status` is the latest client-
    /// requested CursorImageStatus; `pointer_loc` is the current
    /// pointer position in physical pixels; `cached` is the
    /// MemoryRenderBuffer for the currently-displayed Named icon
    /// (re-baked when `status` changes to a different name).
    /// `started_at` anchors animation timing — `Instant`-based
    /// elapsed time chooses the active animation frame.
    cursor: CursorRenderState,
    /// Monotonic frame counter, incremented on every successful render
    /// path through `render_one_frame` / `render_layer_elements` /
    /// the wallpaper-engine tick. Always present (not feature-gated):
    /// production halmasuit exposes it via the shutdown-liveness line
    /// so the pivot-survival test can assert the render loop is
    /// actually advancing the counter post-pivot (not just dispatching
    /// the calloop liveness timer). `halmasuit-debug` also surfaces it
    /// as `Event::FrameRendered.frame_id` for the `frame_audit` stream.
    ///
    /// Epic #71 R-honest.1: this is an `Arc<AtomicU64>` — the SAME
    /// counter the Compositor1 DBus surface (`GetFrameCounter`) and
    /// the diagnostic overlay read. `main` swaps in the shared
    /// `CompositorObservability::frame_counter` clone via
    /// `set_frame_counter` before the render loop starts, so there is
    /// exactly ONE counter (epic anti-pattern: no second copy).
    frame_counter: std::sync::Arc<std::sync::atomic::AtomicU64>,
    /// Epic #71 R3.2: whether to composite the diagnostic overlay
    /// element this frame. Toggled by `set_overlay_visible` from
    /// `HalmasuitState::handle_chord_action` in response to
    /// `Ctrl+Alt+Shift+Esc` / `Esc`-when-open.
    overlay_visible: bool,
    /// Epic #71 R-honest.6: the diagnostic text the overlay renders
    /// (phase / broker / windows / journal tail), composed on the
    /// calloop thread from the shared observability store + the
    /// off-thread journal fetch. Set via `set_overlay_text`.
    overlay_text: String,
    /// Cached rasterized overlay buffer (semitransparent dark backdrop
    /// plus the text blitted via the console font). A value of `None`
    /// means the cache is invalid and the next composite rebuilds it;
    /// `set_overlay_text` resets it to `None` on every content change.
    /// The rebuild happens only while the overlay is open, so a closed
    /// overlay costs nothing.
    overlay_buffer: Option<MemoryRenderBuffer>,

    /// Latest composited frame, published every audited frame for the
    /// D-Bus `Snapshot()` method to read. Only exists in
    /// `halmasuit-debug`.
    #[cfg(feature = "frame_audit")]
    snapshot_buf: crate::dbus::SnapshotBuffer,
    /// Latest wallpaper-plane-only frame, captured by re-rendering just
    /// the wallpaper element to an offscreen GLES texture. Published
    /// alongside `snapshot_buf` so the test driver can request a
    /// variant-distinct golden uncoupled from any layer-shell /
    /// xdg-toplevel overlay. Only exists in `halmasuit-debug`.
    #[cfg(feature = "frame_audit")]
    wallpaper_only_buf: crate::dbus::SnapshotBuffer,
}

impl DrmBackend {
    /// Clones of the shared snapshot slots (current composition +
    /// wallpaper-plane-only), to hand to the D-Bus server. The render
    /// loop publishes into both; `Snapshot(path, scene)` reads from one
    /// based on the `scene` arg.
    #[cfg(feature = "frame_audit")]
    #[must_use]
    pub fn snapshot_handle(&self) -> crate::dbus::SnapshotHandles {
        crate::dbus::SnapshotHandles {
            current: self.snapshot_buf.clone(),
            wallpaper_only: self.wallpaper_only_buf.clone(),
        }
    }

    /// Periodic tick that drives the wallpaper backend's
    /// render-loop-independent polling AND the fallback-swap
    /// check. Called from a calloop timer registered in
    /// [`setup_drm_backend`] for `WallpaperConfig::Video`
    /// configurations. For non-video backends this is a no-op.
    ///
    /// Returns the wallpaper-tick decision: drives the per-tick render
    /// AND the fallback-swap state machine in a single call. The
    /// `main.rs` wallpaper-tick callback consumes this and renders if
    /// the action is non-`Idle`.
    pub fn tick_wallpaper(&mut self) -> WallpaperTickAction {
        let fallback_swapped = self.wallpaper.tick(&mut self.renderer);
        let wants_continuous = self.wallpaper.wants_continuous_render();
        match (fallback_swapped, wants_continuous) {
            (true, _) => WallpaperTickAction::RenderAndSwapped,
            (false, true) => WallpaperTickAction::RenderContinuous,
            (false, false) => WallpaperTickAction::Idle,
        }
    }

    /// Forward a fired system event to the wallpaper backend; returns
    /// the GLSL uniform names updated (empty if the backend has no
    /// binding for `event_name`). The continuous wallpaper tick picks
    /// up the mutated uniform values on its next fire — no explicit
    /// redraw needed. See [`WallpaperEngine::notify_event`].
    pub fn notify_wallpaper_event(&mut self, event_name: &str, value: f32) -> Vec<String> {
        self.wallpaper.notify_event(event_name, value)
    }
}

/// Outcome of `DrmBackend::tick_wallpaper` — encodes both the
/// fallback-swap state-machine step AND whether the active backend
/// needs a per-tick render. `main.rs`'s wallpaper-tick callback
/// matches on this to decide whether to call `render_one_frame`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WallpaperTickAction {
    /// No render needed this tick. Static-wallpaper config (image)
    /// with no fallback swap pending — the kernel keeps scanning out
    /// the last-flipped framebuffer.
    Idle,
    /// Render this tick because the active backend wants continuous
    /// renders (shader, video — `wants_continuous_render() == true`),
    /// not because a swap fired.
    RenderContinuous,
    /// A fallback swap fired this tick (e.g. video relay died); the
    /// freshly-installed fallback must reach the screen, so the
    /// caller queues a render.
    RenderAndSwapped,
}

impl WallpaperTickAction {
    /// `true` iff this action wants the caller to invoke `render_one_frame`.
    #[must_use]
    pub const fn wants_render(self) -> bool {
        !matches!(self, Self::Idle)
    }
}

/// Map a drm-rs `Interface` enum variant to the canonical Wayland
/// short-name prefix that compositors surface to users ("DP" for
/// DisplayPort, "HDMI-A" for HDMI Type-A, "eDP" for embedded
/// DisplayPort, etc.). Lives in a pure helper so unit tests can pin
/// the mapping without needing a real `connector::Info` (which is
/// opaque / kernel-supplied).
///
/// Follows libdrm's `drmModeGetConnectorName` convention — matches
/// `wlr-randr`, `niri msg outputs`, and the major compositors.
const fn interface_prefix(
    iface: smithay::reexports::drm::control::connector::Interface,
) -> &'static str {
    use smithay::reexports::drm::control::connector::Interface;
    match iface {
        Interface::DisplayPort => "DP",
        Interface::HDMIA => "HDMI-A",
        Interface::HDMIB => "HDMI-B",
        Interface::DVII => "DVI-I",
        Interface::DVID => "DVI-D",
        Interface::DVIA => "DVI-A",
        Interface::VGA => "VGA",
        Interface::EmbeddedDisplayPort => "eDP",
        Interface::Composite => "Composite",
        Interface::SVideo => "SVIDEO",
        Interface::LVDS => "LVDS",
        Interface::Component => "Component",
        Interface::NinePinDIN => "DIN",
        Interface::DSI => "DSI",
        Interface::DPI => "DPI",
        Interface::Writeback => "Writeback",
        Interface::SPI => "SPI",
        Interface::USB => "USB",
        Interface::TV => "TV",
        Interface::Virtual => "Virtual",
        // `Interface::Unknown` AND any future `#[non_exhaustive]`
        // additions (DisplayPort-MST, VirtIO, etc.) collapse to
        // "Unknown" — a new drm-rs variant won't break the build,
        // and users see the actual name via the journal's
        // `available` list when matching fails.
        _ => "Unknown",
    }
}

/// Render a DRM connector's `(interface, interface_id)` as the
/// canonical Wayland short-name ("DP-3", "HDMI-A-1", "eDP-1", etc.).
/// Used to match against `services.halmasuit.rendering.primaryOutput`
/// (env `HALMASUIT_PRIMARY_OUTPUT`) when selecting which output is the
/// primary (leftmost, at x=0 in the global compositor space).
fn connector_short_name(info: &connector::Info) -> String {
    format!(
        "{}-{}",
        interface_prefix(info.interface()),
        info.interface_id()
    )
}

/// Set up the full DRM/GBM/EGL/GLES/DrmCompositor stack by opening the
/// DRM device at `path` directly and issuing `DRM_IOCTL_SET_MASTER`
/// ourselves. halmasuit is a system compositor: it owns master for its
/// entire process lifetime, in both the rootfs and the initramfs
/// deployment paths. No libseat / no seatd anywhere in the runtime
/// closure (R2.3) — the kernel's "first opener wins" rule grants us
/// master implicitly on open, and the subsequent `acquire_master_lock`
/// is idempotent. Probes drm-master-probe-phase{1,2} validated that
/// the master designation survives both `setresuid` (privilege drop)
/// and `switch_root` (initramfs→rootfs pivot) when paired with
/// `SurviveFinalKillSignal=yes`.
///
/// Enumerates every connected connector, builds a shared GBM + EGL +
/// GLES stack, then gives each connector its own dedicated CRTC +
/// `DrmSurface` + `DrmCompositor` (extended multi-output, laid out
/// left-to-right with the `primaryOutput` at x=0), registers the DRM
/// event source with calloop for vblank notifications, and returns the
/// retained backend plus one smithay `Output` per connector for the
/// caller to register as globals.
///
/// The caller is responsible for calling
/// `output.create_global::<S>(&display_handle)` for EACH returned
/// `Output` after this returns — that call requires
/// `S: GlobalDispatch<WlOutput, …>` which is implemented at the
/// caller's site, not in this module.
///
/// `drm_event_handler` is invoked from inside the calloop callback
/// when `DrmEvent::VBlank(crtc)` fires; `crtc` routes back to the
/// originating output.
///
/// # Errors
///
/// Bubbles any open / SET_MASTER ioctl / DRM / GBM / EGL / GLES /
/// calloop failure with context. Errors if zero outputs initialize.
pub fn setup_drm_direct<S, F>(
    path: &Path,
    loop_handle: &smithay::reexports::calloop::LoopHandle<'static, S>,
    drm_event_handler: F,
    wallpaper_config: Option<WallpaperConfig>,
) -> io::Result<(
    DrmBackend,
    smithay::reexports::calloop::RegistrationToken,
    Vec<Output>,
)>
where
    S: 'static,
    F: FnMut(DrmEvent, &mut Option<smithay::backend::drm::DrmEventMetadata>, &mut S) + 'static,
{
    use std::os::fd::OwnedFd;
    use std::os::unix::fs::OpenOptionsExt;
    // O_RDWR | O_NONBLOCK. O_CLOEXEC is set by std's OpenOptions by
    // default since Rust 1.20.
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .custom_flags(libc::O_NONBLOCK)
        .open(path)
        .map_err(|e| io::Error::other(format!("open {}: {e}", path.display())))?;
    let owned_fd: OwnedFd = file.into();
    let device_fd = DrmDeviceFd::new(DeviceFd::from(owned_fd));
    build_drm_pipeline(device_fd, loop_handle, drm_event_handler, wallpaper_config)
}

// reason: a single linear DRM→GBM→EGL→GLES→DrmCompositor→calloop
// init sequence used by `setup_drm_direct`. The ordering is
// load-bearing (master before GBM, EGL before GLES, surface before
// compositor); splitting it into more helpers scatters that ordering
// across the module for no readability or testability gain.
#[allow(
    clippy::too_many_lines,
    reason = "linear hardware-init sequence; ordering is load-bearing"
)]
fn build_drm_pipeline<S, F>(
    device_fd: DrmDeviceFd,
    loop_handle: &smithay::reexports::calloop::LoopHandle<'static, S>,
    drm_event_handler: F,
    wallpaper_config: Option<WallpaperConfig>,
) -> io::Result<(
    DrmBackend,
    smithay::reexports::calloop::RegistrationToken,
    Vec<Output>,
)>
where
    S: 'static,
    F: FnMut(DrmEvent, &mut Option<smithay::backend::drm::DrmEventMetadata>, &mut S) + 'static,
{
    // DrmDevice + its event notifier. `drm` must be `mut` so we can
    // call `create_surface` on it below.
    let (mut drm, notifier) = DrmDevice::new(device_fd.clone(), true)
        .map_err(|e| io::Error::other(format!("DrmDevice::new: {e}")))?;

    // SET_MASTER. drm-rs's `ControlDevice::acquire_master_lock` issues
    // `DRM_IOCTL_SET_MASTER`; idempotent on an fd that already has
    // master (which it should, courtesy of the kernel's "first opener
    // wins" rule — but call this explicitly so the master designation
    // is recorded against this fd before we touch CRTCs).
    drm.acquire_master_lock()
        .map_err(|e| io::Error::other(format!("DRM SET_MASTER: {e}")))?;

    // Enumerate connected connectors and give EACH its own dedicated
    // CRTC + DrmSurface + DrmCompositor (extended multi-output). This
    // is the idiomatic smithay path (niri / cosmic-comp): NVIDIA's
    // open kernel module exposes one CRTC per connector and never sets
    // `possible_clones`, so binding multiple connectors to one CRTC is
    // rejected at the first atomic commit (the prior kernel-clone
    // approach crash-looped here on real hardware). Single-connector
    // substrates (the headless VM test matrix, single-monitor laptops)
    // produce a Vec of one — byte-identical to the prior single-output
    // behavior, so the login-flash gate and the visual suite are
    // unaffected.
    let res = drm
        .resource_handles()
        .map_err(|e| io::Error::other(format!("resource_handles: {e}")))?;

    let mut connected: Vec<_> = res
        .connectors()
        .iter()
        .filter_map(|&h| drm.get_connector(h, true).ok())
        .filter(|info| info.state() == connector::State::Connected)
        .collect();

    // Honor `services.halmasuit.rendering.primaryOutput` (env
    // HALMASUIT_PRIMARY_OUTPUT). When set to a connector short-name
    // like "DP-3", reorder `connected` so the matching connector is
    // first — it becomes the primary (leftmost, at x=0 in the global
    // compositor space) and the index-0 output used for frame-audit.
    // When the env is unset or doesn't match any connected connector,
    // fall back to enumeration order.
    if let Ok(want) = std::env::var("HALMASUIT_PRIMARY_OUTPUT") {
        if let Some(idx) = connected
            .iter()
            .position(|info| connector_short_name(info) == want)
        {
            if idx != 0 {
                connected.swap(0, idx);
            }
            tracing::info!(
                target: "halmasuit",
                primary = %want,
                "primary output pinned"
            );
        } else {
            let available: Vec<_> = connected.iter().map(connector_short_name).collect();
            tracing::warn!(
                target: "halmasuit",
                requested = %want,
                ?available,
                "HALMASUIT_PRIMARY_OUTPUT not found among connected connectors; using enumeration order"
            );
        }
    }

    if connected.is_empty() {
        return Err(io::Error::other("no connected DRM connector"));
    }

    // GBM device on the same fd, SHARED by every output's allocator,
    // exporter, and compositor (each takes its own clone — GbmDevice is
    // ref-counted). Allocators pull SCANOUT-capable buffers from this
    // device; the framebuffer exporters wrap the resulting BOs as DRM
    // framebuffer handles.
    let gbm =
        GbmDevice::new(device_fd).map_err(|e| io::Error::other(format!("GbmDevice::new: {e}")))?;

    // EGL display + context + GLES renderer, SHARED across all outputs.
    // The two `unsafe`s are smithay's API contracts: EGLDisplay::new
    // takes a native display pointer and trusts the caller it's a valid
    // GBM device, and GlesRenderer::new requires the context not be
    // active on another thread (it isn't — this is the main thread).
    #[expect(
        unsafe_code,
        reason = "EGLDisplay::new is unsafe by smithay API contract; gbm is freshly constructed above and not handed to any other thread"
    )]
    let egl_display = unsafe { EGLDisplay::new(gbm.clone()) }
        .map_err(|e| io::Error::other(format!("EGLDisplay::new: {e}")))?;
    let egl_context = EGLContext::new(&egl_display)
        .map_err(|e| io::Error::other(format!("EGLContext::new: {e}")))?;
    #[expect(
        unsafe_code,
        reason = "GlesRenderer::new is unsafe by smithay API contract; the EGLContext is owned and not made current on another thread"
    )]
    let mut renderer = unsafe { GlesRenderer::new(egl_context) }
        .map_err(|e| io::Error::other(format!("GlesRenderer::new: {e}")))?;

    let render_formats = renderer
        .egl_context()
        .dmabuf_render_formats()
        .iter()
        .copied()
        .collect::<Vec<_>>();
    let cursor_size = drm.cursor_size();

    // One CRTC + surface + DrmCompositor + Output per connected
    // connector. Outputs are laid out left-to-right starting at x=0
    // with the primary (front of `connected`) first.
    let mut output_surfaces: Vec<OutputSurface> = Vec::with_capacity(connected.len());
    let mut outputs: Vec<Output> = Vec::with_capacity(connected.len());
    let mut used_crtcs: Vec<crtc::Handle> = Vec::new();
    let mut x_offset: i32 = 0;

    for info in &connected {
        let name = connector_short_name(info);
        let Some(mode) = info.modes().first().copied() else {
            tracing::warn!(
                target: "halmasuit",
                connector = %name,
                "connected connector reports no modes; skipping"
            );
            continue;
        };

        // Allocate a free CRTC reachable from one of this connector's
        // encoders (the `possible_crtcs` mask), not already claimed by
        // an earlier output. On NVIDIA each connector has exactly one
        // candidate head, so this is effectively a 1:1 assignment.
        let crtc_handle = info
            .encoders()
            .iter()
            .filter_map(|&eh| drm.get_encoder(eh).ok())
            .flat_map(|enc| res.filter_crtcs(enc.possible_crtcs()))
            .find(|c| !used_crtcs.contains(c));
        let Some(crtc_handle) = crtc_handle else {
            tracing::warn!(
                target: "halmasuit",
                connector = %name,
                "no free CRTC reachable from connector; skipping"
            );
            continue;
        };

        // Single-element connector slice — one connector per CRTC. This
        // is the only shape NVIDIA accepts (see OutputSurface docs).
        let surface: DrmSurface = match drm.create_surface(crtc_handle, mode, &[info.handle()]) {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!(
                    target: "halmasuit",
                    connector = %name,
                    error = %e,
                    "create_surface failed; skipping connector"
                );
                continue;
            }
        };
        used_crtcs.push(crtc_handle);

        let (w, h) = mode.size();
        let output_mode = OutputMode {
            size: (i32::from(w), i32::from(h)).into(),
            // smithay's `Mode::refresh` is in mHz; DRM `vrefresh` is Hz.
            refresh: i32::try_from(mode.vrefresh()).unwrap_or(60_000) * 1000,
        };
        let physical = PhysicalProperties {
            // (0, 0) signals "unknown" per the wl_output spec.
            size: (0, 0).into(),
            subpixel: Subpixel::Unknown,
            make: "halmasuit".to_owned(),
            model: format!("drm-{w}x{h}"),
            serial_number: String::new(),
        };
        // Name the Output after the connector short-name ("DP-3") so
        // the journal + the client-visible wl_output name identify the
        // physical monitor. Position it at the running x-offset; the
        // caller registers a wl_output global per returned Output.
        let output = Output::new(name.clone(), physical);
        output.change_current_state(Some(output_mode), None, None, Some((x_offset, 0).into()));
        output.set_preferred(output_mode);

        // Per-output allocator + framebuffer exporter, each wrapping a
        // clone of the shared GBM device. `NodeFilter::None` = "use this
        // GBM device for all framebuffer exports" (single-GPU).
        let allocator = GbmAllocator::new(
            gbm.clone(),
            GbmBufferFlags::RENDERING | GbmBufferFlags::SCANOUT,
        );
        let exporter = GbmFramebufferExporter::new(
            gbm.clone(),
            smithay::backend::drm::exporter::gbm::NodeFilter::None,
        );

        // DrmCompositor: drives this CRTC's surface + swapchain.
        // `OutputModeSource::Auto(output.downgrade())` ties its working
        // size/scale to this output. `DrmCompositor::new` issues a TEST
        // atomic commit — the layer the prior kernel-clone approach was
        // rejected at; per-CRTC it passes.
        let compositor = DrmCompositor::<
            GbmAllocator<DrmDeviceFd>,
            GbmFramebufferExporter<DrmDeviceFd>,
            (),
            DrmDeviceFd,
        >::new(
            OutputModeSource::Auto(output.downgrade()),
            surface,
            None,
            allocator,
            exporter,
            SUPPORTED_COLOR_FORMATS.iter().copied(),
            render_formats.clone(),
            cursor_size,
            Some(gbm.clone()),
        )
        .map_err(|e| io::Error::other(format!("DrmCompositor::new ({name}): {e}")))?;

        tracing::info!(
            target: "halmasuit",
            connector = %name,
            crtc = ?crtc_handle,
            x = x_offset,
            mode_w = w,
            mode_h = h,
            mode_hz = mode.vrefresh(),
            "DRM output: connector bound to dedicated CRTC"
        );

        outputs.push(output.clone());
        output_surfaces.push(OutputSurface {
            output,
            crtc: crtc_handle,
            compositor,
        });
        x_offset += i32::from(w);
    }

    if output_surfaces.is_empty() {
        return Err(io::Error::other(
            "no DRM outputs could be initialized (every connected connector failed CRTC allocation or create_surface)",
        ));
    }

    tracing::info!(
        target: "halmasuit",
        outputs = output_surfaces.len(),
        "DRM scanout: per-CRTC multi-output initialized"
    );

    // Build the wallpaper engine ONCE (shared across outputs; each
    // output's scene renders the wallpaper element at its own size).
    // Each backend's constructor decodes / compiles synchronously, so
    // the engine is frame-0 ready when this returns (epic G1/R3/R6).
    // `renderer` is no longer borrowed by the compositors (they took
    // `render_formats.clone()`) and is moved into `DrmBackend` below.
    let wallpaper = match wallpaper_config {
        Some(cfg) => {
            let backend: Box<dyn WallpaperBackend> = match cfg {
                WallpaperConfig::Image { source } => {
                    Box::new(ImageBackend::new(&mut renderer, &source)?)
                }
                WallpaperConfig::Shader { source, uniforms } => {
                    Box::new(ShaderBackend::new(&mut renderer, &source, uniforms)?)
                }
                WallpaperConfig::Video {
                    source,
                    loop_playback,
                    fallback,
                } => Box::new(VideoBackend::new(
                    &mut renderer,
                    &source,
                    loop_playback,
                    fallback,
                )?),
            };
            WallpaperEngine::with_backend(backend)
        }
        None => WallpaperEngine::empty(),
    };

    // Register the DRM event notifier with calloop. The notifier
    // emits `DrmEvent::VBlank(crtc)` on every page-flip ack and
    // `DrmEvent::Error(e)` on async DRM errors. The caller-provided
    // handler is what drives our render loop.
    let registration_token = loop_handle
        .insert_source(notifier, drm_event_handler)
        .map_err(|e| io::Error::other(format!("calloop register DrmDeviceNotifier: {e}")))?;

    Ok((
        DrmBackend {
            outputs: output_surfaces,
            renderer,
            wallpaper,
            cursor: CursorRenderState {
                theming: crate::cursor::CursorTheming::load(),
                status: smithay::input::pointer::CursorImageStatus::default_named(),
                pointer_loc: (0, 0).into(),
                has_moved: false,
                cached: None,
                started_at: std::time::Instant::now(),
            },
            // Own counter until `main` swaps in the shared
            // CompositorObservability clone via `set_frame_counter`
            // (before the render loop starts, so this initial Arc is
            // never incremented).
            frame_counter: std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0)),
            // Epic #71 R3.2: overlay starts hidden; chord toggles it.
            overlay_visible: false,
            overlay_text: String::new(),
            overlay_buffer: None,
            #[cfg(feature = "frame_audit")]
            snapshot_buf: crate::dbus::new_buffer(),
            #[cfg(feature = "frame_audit")]
            wallpaper_only_buf: crate::dbus::new_buffer(),
        },
        registration_token,
        outputs,
    ))
}

impl DrmBackend {
    /// Epic #71 R-honest.7: release DRM master so another VT (text
    /// console fbcon, or a getty) can take over the display while
    /// halmasuit is backgrounded. Called from
    /// `HalmasuitState::handle_vt_relsig` on the kernel's relsig BEFORE
    /// it `VT_RELDISP`s the switch away.
    ///
    /// Idempotent (the underlying DROP_MASTER ioctl is no-op if we
    /// already don't hold master). Does NOT tear down the
    /// `DrmCompositor` — its CRTC / framebuffer state is preserved
    /// in our address space; only the kernel-side master designation
    /// is released. `resume()` reacquires.
    ///
    /// Note: pageflip attempts that race the master drop will EACCES.
    /// The render loop's existing DRM-error path swallows transient
    /// EACCES; the wallpaper-tick timer keeps polling so the next
    /// successful frame after `resume()` re-paints.
    ///
    /// # Errors
    /// Bubbles DROP_MASTER ioctl errno. The caller logs and continues —
    /// the VT switch proceeds regardless.
    pub fn pause(&self) -> io::Result<()> {
        // DRM master is per-DEVICE: every per-CRTC `OutputSurface`
        // shares the one /dev/dri/card0 fd, so releasing once via any
        // surface drops the device's master designation.
        let Some(first) = self.outputs.first() else {
            return Ok(());
        };
        first
            .compositor
            .surface()
            .device_fd()
            .release_master_lock()
            .map_err(|e| io::Error::other(format!("DRM DROP_MASTER: {e}")))?;
        Ok(())
    }

    /// Epic #71 R-honest.7: re-acquire DRM master after the kernel
    /// switches back to halmasuit's home VT. Idempotent. Called from
    /// `HalmasuitState::handle_vt_acqsig` on the kernel's acqsig before
    /// it `VT_RELDISP(ackacq)`s.
    ///
    /// # Errors
    /// Bubbles SET_MASTER ioctl errno. If this fails the display
    /// stays blank — the caller (`handle_vt_acqsig`) logs loudly. The
    /// user can switch away + back to retry.
    pub fn resume(&self) -> io::Result<()> {
        // Per-device master (see `pause`): one acquire via any surface
        // reacquires for the whole device / all CRTCs.
        let Some(first) = self.outputs.first() else {
            return Ok(());
        };
        first
            .compositor
            .surface()
            .device_fd()
            .acquire_master_lock()
            .map_err(|e| io::Error::other(format!("DRM SET_MASTER: {e}")))?;
        Ok(())
    }

    /// Epic #71 R3.2: toggle the diagnostic-overlay composition
    /// element. Called from `HalmasuitState::handle_chord_action`
    /// on `Ctrl+Alt+Shift+Esc` / `Esc`-when-open. Cheap state-only
    /// flip; the visible effect lands on the next render tick.
    pub const fn set_overlay_visible(&mut self, visible: bool) {
        self.overlay_visible = visible;
    }

    /// Epic #71 R-honest.6: set the diagnostic text the overlay
    /// renders and invalidate the cached buffer so the next composite
    /// rebuilds the panel with the fresh content. Called from the
    /// calloop thread (chord-open recompose + the off-thread journal
    /// fetch's calloop-channel handler).
    pub fn set_overlay_text(&mut self, text: String) {
        if self.overlay_text != text {
            self.overlay_text = text;
            self.overlay_buffer = None; // force rebuild on next composite
        }
    }

    /// R8b-render: install the latest `CursorImageStatus` from the
    /// focused client (or halmasuit's own default). When the status
    /// transitions to a new Named icon, drop the cached buffer so
    /// the next render bakes the new pixmap from the xcursor theme.
    pub fn set_cursor_status(&mut self, status: smithay::input::pointer::CursorImageStatus) {
        let name_changed = match (&self.cursor.status, &status) {
            (
                smithay::input::pointer::CursorImageStatus::Named(a),
                smithay::input::pointer::CursorImageStatus::Named(b),
            ) => a != b,
            _ => true,
        };
        if name_changed {
            self.cursor.cached = None;
        }
        self.cursor.status = status;
    }

    /// R8b-render: latest pointer location in physical pixels. Called
    /// from main.rs's pointer-motion / touch handlers after they
    /// route the event through smithay's `PointerHandle`.
    pub fn set_pointer_location(
        &mut self,
        loc: smithay::utils::Point<f64, smithay::utils::Logical>,
    ) {
        // Halmasuit's single output is 1:1 scale, so logical == physical;
        // round-to-nearest for pixel alignment.
        #[allow(
            clippy::cast_possible_truncation,
            reason = "cursor position bounded by output dimensions, well within i32"
        )]
        let (x, y) = (loc.x.round() as i32, loc.y.round() as i32);
        self.cursor.pointer_loc = (x, y).into();
        self.cursor.has_moved = true;
    }

    /// R8b-render: build the cursor render element(s) for the current
    /// status + location. Returns empty when Hidden, the cached or
    /// freshly-baked Named pixmap when Named, or surface-tree
    /// elements when Surface. Always at index 0 (topmost — drawn LAST
    /// in front-to-back order).
    fn cursor_elements(&mut self) -> Vec<SceneElement> {
        use smithay::backend::renderer::element::Kind;
        use smithay::input::pointer::CursorImageStatus;

        // Skip the default-named cursor at boot before the user
        // moves the mouse. A client-attached cursor surface
        // (`set_cursor(surface)`) overrides this — that's an
        // explicit client request to display its own cursor and
        // honored regardless of `has_moved`.
        if !self.cursor.has_moved && !matches!(self.cursor.status, CursorImageStatus::Surface(_)) {
            return Vec::new();
        }
        let scale = smithay::utils::Scale::from(1.0);
        match self.cursor.status.clone() {
            CursorImageStatus::Hidden => Vec::new(),
            CursorImageStatus::Named(name) => {
                let name_str = name.name();
                let elapsed = self.cursor.started_at.elapsed();
                let needs_bake = self
                    .cursor
                    .cached
                    .as_ref()
                    .is_none_or(|c| c.name != name_str);
                if needs_bake {
                    let icons = self.cursor.theming.load_named(name_str);
                    let frame = icons.current_frame(elapsed);
                    #[allow(
                        clippy::cast_possible_wrap,
                        reason = "xcursor image dimensions ≤ a sane cursor size, fit i32"
                    )]
                    let size = (frame.width as i32, frame.height as i32);
                    let buf = MemoryRenderBuffer::from_slice(
                        &frame.pixels_rgba,
                        Fourcc::Argb8888,
                        size,
                        1,
                        smithay::utils::Transform::Normal,
                        None,
                    );
                    #[allow(
                        clippy::cast_possible_wrap,
                        reason = "hotspot ≤ image size ≤ sane cursor dimension; fits i32"
                    )]
                    let hotspot = (frame.xhot as i32, frame.yhot as i32);
                    self.cursor.cached = Some(CachedNamed {
                        name: name_str.to_owned(),
                        hotspot,
                        buffer: buf,
                    });
                }
                let cached = self
                    .cursor
                    .cached
                    .as_ref()
                    .expect("just baked above if needs_bake was true");
                let loc = smithay::utils::Point::from((
                    self.cursor.pointer_loc.x - cached.hotspot.0,
                    self.cursor.pointer_loc.y - cached.hotspot.1,
                ));
                match MemoryRenderBufferRenderElement::from_buffer(
                    &mut self.renderer,
                    loc.to_f64(),
                    &cached.buffer,
                    None,
                    None,
                    None,
                    Kind::Cursor,
                ) {
                    Ok(el) => vec![SceneElement::Memory(el)],
                    Err(e) => {
                        tracing::warn!(error = %e, "cursor MemoryRenderBufferRenderElement::from_buffer");
                        Vec::new()
                    }
                }
            }
            CursorImageStatus::Surface(surface) => {
                // Client-attached cursor surface tree. Hotspot is on
                // the smithay-side `CursorImageSurfaceData` set by
                // `wl_pointer.set_cursor`'s hot_x/hot_y; smithay
                // tracks it via the surface's data map.
                let hotspot = smithay::wayland::compositor::with_states(&surface, |states| {
                    states
                        .data_map
                        .get::<std::sync::Mutex<smithay::input::pointer::CursorImageAttributes>>()
                        .map_or((0, 0), |d| {
                            let l = d.lock().unwrap();
                            (l.hotspot.x, l.hotspot.y)
                        })
                });
                let loc = smithay::utils::Point::from((
                    self.cursor.pointer_loc.x - hotspot.0,
                    self.cursor.pointer_loc.y - hotspot.1,
                ));
                let elements: Vec<WaylandSurfaceRenderElement<GlesRenderer>> =
                    smithay::backend::renderer::element::surface::render_elements_from_surface_tree(
                        &mut self.renderer,
                        &surface,
                        loc,
                        scale,
                        1.0,
                        Kind::Cursor,
                    );
                elements.into_iter().map(SceneElement::Surface).collect()
            }
        }
    }

    /// Build one frame's front-to-back render-element list (index 0 =
    /// topmost). Walks the output's LayerMap in scene z-order — top →
    /// bottom: Overlay, Top, the foreground xdg toplevel
    /// (greeter/session), Bottom, Background — wrapping each committed
    /// surface subtree as a `SceneElement::Surface`
    /// (`render_elements_from_surface_tree`; the renderer lazily
    /// imports committed `wl_shm` buffers as `GlesTexture`s during the
    /// draw). The wallpaper plane, when configured, is appended LAST
    /// as `SceneElement::Wallpaper` — the bottom-most element, so
    /// every surface composites over it and frame 0 (no surfaces) is
    /// already the wallpaper, never a solid clear (epic G1/R3/R6).
    /// The foreground toplevel sits above the wallpaper layers and
    /// below OSK/lock/notification overlays.
    ///
    /// # Errors
    ///
    /// Bubbles any wallpaper-backend render error (the image backend
    /// is infallible after construction; shader/video backends may
    /// fail per-frame once they land).
    fn scene_elements(
        &mut self,
        output: &smithay::output::Output,
        foreground: Option<&smithay::reexports::wayland_server::protocol::wl_surface::WlSurface>,
    ) -> io::Result<Vec<SceneElement>> {
        use smithay::backend::renderer::element::Kind;
        use smithay::backend::renderer::element::surface::render_elements_from_surface_tree;
        use smithay::desktop::layer_map_for_output;
        use smithay::utils::Scale;
        use smithay::wayland::shell::wlr_layer::Layer as WlrLayer;

        let scale = Scale::from(1.0);
        let map = layer_map_for_output(output);
        // R8b-render: cursor goes at INDEX 0 (topmost — drawn LAST in
        // front-to-back order). Build first so we can prepend at the
        // end without an extra Vec move.
        let cursor_elements = self.cursor_elements();
        let mut elements: Vec<SceneElement> = Vec::new();
        let mut push_surface_at =
            |renderer: &mut GlesRenderer,
             surface: &_,
             loc: smithay::utils::Point<i32, smithay::utils::Physical>| {
                let es: Vec<WaylandSurfaceRenderElement<GlesRenderer>> =
                    render_elements_from_surface_tree(
                        renderer,
                        surface,
                        loc,
                        scale,
                        1.0,
                        Kind::Unspecified,
                    );
                elements.extend(es.into_iter().map(SceneElement::Surface));
            };

        for which in [WlrLayer::Overlay, WlrLayer::Top] {
            for layer in map.layers_on(which) {
                let loc = map.layer_geometry(layer).map(|g| g.loc).unwrap_or_default();
                push_surface_at(
                    &mut self.renderer,
                    layer.wl_surface(),
                    (loc.x, loc.y).into(),
                );
            }
        }
        if let Some(top) = foreground {
            // Single fullscreen toplevel at the origin (v1: one
            // output, no window management).
            push_surface_at(&mut self.renderer, top, (0, 0).into());
        }
        for which in [WlrLayer::Bottom, WlrLayer::Background] {
            for layer in map.layers_on(which) {
                let loc = map.layer_geometry(layer).map(|g| g.loc).unwrap_or_default();
                push_surface_at(
                    &mut self.renderer,
                    layer.wl_surface(),
                    (loc.x, loc.y).into(),
                );
            }
        }
        drop(map);

        if let Some(slot) = wallpaper_slot(elements.len(), self.wallpaper.has_backend()) {
            debug_assert_eq!(slot, elements.len(), "wallpaper is the bottom-most element");
            let osize = output.current_mode().map(|m| m.size).unwrap_or_default();
            let dst =
                smithay::utils::Size::<i32, smithay::utils::Logical>::from((osize.w, osize.h));
            if let Some(element) = self.wallpaper.render_element(&mut self.renderer, dst)? {
                elements.push(element);
            }
        }
        // Epic #71 R3.2/R-honest.6: diagnostic overlay composites
        // above every surface (so it covers the focused app cleanly)
        // but BELOW the cursor (so the user can still see their
        // pointer). Insert at index 0 of `elements` now — after the
        // cursor logic below it'll move to index 1 (one below cursor).
        if self.overlay_visible
            && let Some(overlay) = self.build_overlay_element(output)
        {
            elements.insert(0, SceneElement::Memory(overlay));
        }
        // R8b-render: prepend cursor elements so they sit at index 0
        // (topmost). Smithay's render is front-to-back; the cursor
        // composites OVER all layers + foreground.
        if !cursor_elements.is_empty() {
            let mut combined = cursor_elements;
            combined.append(&mut elements);
            return Ok(combined);
        }
        Ok(elements)
    }

    /// Epic #71 R-honest.6: build the diagnostic-overlay render
    /// element — a full-output memory buffer holding a semitransparent
    /// dark backdrop with the diagnostic text (phase / broker /
    /// windows / journal tail) rasterized on top via the console-font
    /// blitter. The buffer is cached (`overlay_buffer`) and rebuilt
    /// only when `set_overlay_text` invalidates it, so a steady
    /// open overlay costs one upload, not one per frame. Returns
    /// `None` if the element can't be built (logged; the overlay just
    /// doesn't show that frame).
    fn build_overlay_element(
        &mut self,
        output: &smithay::output::Output,
    ) -> Option<MemoryRenderBufferRenderElement<GlesRenderer>> {
        use smithay::backend::renderer::element::Kind;

        if self.overlay_buffer.is_none() {
            self.overlay_buffer = Some(self.rasterize_overlay(output));
        }
        // Disjoint field borrows: `buffer` borrows `self.overlay_buffer`,
        // `from_buffer` borrows `self.renderer` — different fields, OK.
        let buffer = self.overlay_buffer.as_ref()?;
        match MemoryRenderBufferRenderElement::from_buffer(
            &mut self.renderer,
            smithay::utils::Point::from((0.0_f64, 0.0_f64)),
            buffer,
            None,
            None,
            None,
            Kind::Unspecified,
        ) {
            Ok(el) => Some(el),
            Err(e) => {
                tracing::warn!(error = %e, "diagnostic overlay from_buffer failed");
                None
            }
        }
    }

    /// Rasterize the diagnostic overlay into a full-output RGBA buffer:
    /// a semitransparent dark backdrop (so the wallpaper/app dims but
    /// stays faintly visible — the Force-Quit look) with `overlay_text`
    /// blitted in light console-font glyphs at the top-left. Pure
    /// CPU work; called only when the cached buffer is invalidated.
    fn rasterize_overlay(&self, output: &smithay::output::Output) -> MemoryRenderBuffer {
        /// Backdrop: black at ~60% alpha. Premultiplied-safe (black is
        /// 0 in every channel regardless of alpha convention).
        const OVERLAY_BG: [u8; 4] = [0, 0, 0, 153];
        /// Text: opaque light grey (alpha 255 → premult == straight).
        const OVERLAY_FG: [u8; 4] = [220, 220, 220, 255];

        let osize = output.current_mode().map(|m| m.size).unwrap_or_default();
        let w = osize.w.max(1);
        let h = osize.h.max(1);
        let wu = usize::try_from(w).expect("w >= 1");
        let hu = usize::try_from(h).expect("h >= 1");

        let mut px = vec![0_u8; wu * hu * 4];
        for chunk in px.chunks_exact_mut(4) {
            chunk.copy_from_slice(&OVERLAY_BG);
        }
        crate::console_font::blit_str(&mut px, wu, hu, 8, 8, &self.overlay_text, OVERLAY_FG);

        // Same Fourcc/byte-order convention as the cursor path:
        // [R,G,B,A] bytes with Fourcc::Argb8888.
        MemoryRenderBuffer::from_slice(
            &px,
            Fourcc::Argb8888,
            (w, h),
            1,
            smithay::utils::Transform::Normal,
            None,
        )
    }

    /// Render one frame with no foreground toplevel — the frame-0 /
    /// no-layers shape. The scene is the wallpaper plane (when
    /// configured); `clear_color` is the transient GL clear the
    /// wallpaper fully covers (epic G1/R6). With no wallpaper
    /// configured it is the legacy clear-only path used by
    /// non-visual integration tests. Returns `Ok(true)` if a frame
    /// was queued (non-empty damage), `Ok(false)` if nothing changed.
    ///
    /// # Errors
    ///
    /// Returns an error if `scene_elements`, `render_frame`, or
    /// `queue_frame` fail.
    pub fn render_one_frame(&mut self, clear_color: [u8; 4]) -> io::Result<bool> {
        self.render_all(None, clear_color)
    }

    /// Render a frame composed of the mapped layer-shell surfaces and
    /// the optional foreground toplevel over the wallpaper plane, on
    /// EVERY output (extended multi-output). Each output composites its
    /// own scene from its own `LayerMap` at its own size.
    /// See [`scene_elements`](Self::scene_elements) for z-order.
    ///
    /// # Errors
    ///
    /// Returns an error if `scene_elements`, `render_frame`, or
    /// `queue_frame` fail on any output.
    pub fn render_layer_elements(
        &mut self,
        foreground: Option<&smithay::reexports::wayland_server::protocol::wl_surface::WlSurface>,
        clear_color: [u8; 4],
    ) -> io::Result<bool> {
        self.render_all(foreground, clear_color)
    }

    /// Composite + queue a frame on every output. Returns `Ok(true)` if
    /// at least one output queued a frame (non-empty damage). The
    /// always-on `frame_counter` advances once per logical frame (not
    /// per output); frame-audit (halmasuit-debug only) runs on the
    /// primary output (index 0), matching the single-output test
    /// substrate.
    fn render_all(
        &mut self,
        foreground: Option<&smithay::reexports::wayland_server::protocol::wl_surface::WlSurface>,
        clear_color: [u8; 4],
    ) -> io::Result<bool> {
        let mut any = false;
        // Frame-audit needs the PRIMARY output's element set after the
        // loop; capture it (only built under the feature gate).
        #[cfg(feature = "frame_audit")]
        let mut primary_audit: Option<(Output, Vec<SceneElement>)> = None;

        for i in 0..self.outputs.len() {
            let output = self.outputs[i].output.clone();
            let elements = self.scene_elements(&output, foreground)?;
            // Split borrow: `self.outputs[i].compositor` and
            // `self.renderer` are disjoint fields.
            let queued = {
                let comp = &mut self.outputs[i].compositor;
                render_one_output(comp, &mut self.renderer, &elements, clear_color)?
            };
            any |= queued;
            #[cfg(feature = "frame_audit")]
            if i == 0 && queued {
                primary_audit = Some((output, elements));
            }
            #[cfg(not(feature = "frame_audit"))]
            let _ = output;
        }

        if any {
            // Advance the always-on render counter once per logical
            // frame. `audit_frame` reads (counter - 1) for
            // `Event::FrameRendered.frame_id` so the first emitted frame
            // is still id=0; production halmasuit exposes the
            // post-increment value via `frame_counter()` for the
            // shutdown-liveness line's `frames=N` field AND the
            // Compositor1 DBus `GetFrameCounter` (R-honest.1: same Arc).
            // `fetch_add` wraps on u64 overflow. Relaxed: one-way value
            // flow, no happens-before dependency on other fields.
            self.frame_counter
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);

            // Best-effort offscreen readback of the PRIMARY output's
            // scene. An audit failure must never take down the
            // compositor (Epic #1 req 6/7).
            #[cfg(feature = "frame_audit")]
            if let Some((output, elements)) = primary_audit
                && let Err(e) = self.audit_frame(&output, &elements, clear_color)
            {
                tracing::warn!(error = %e, "frame_audit readback failed");
            }
        }

        Ok(any)
    }

    /// Total frames the render path has queued for scanout. Monotonic,
    /// wraps on u64 overflow. Read by the shutdown-liveness timer to
    /// prove (via the `frames=N` field of the liveness line) that the
    /// render loop is actually advancing through shutdown — not just
    /// that the calloop event loop is firing the liveness timer.
    #[must_use]
    pub fn frame_counter(&self) -> u64 {
        self.frame_counter
            .load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Epic #71 R-honest.1: replace this backend's frame counter with
    /// the shared `CompositorObservability` counter so the render
    /// path, the shutdown-liveness line, `frame_audit`, and the
    /// Compositor1 DBus `GetFrameCounter` all observe ONE counter.
    /// Called by `main` after state construction and BEFORE the render
    /// loop starts, so the backend's initial own-counter is never
    /// incremented (no frames lost, no second copy).
    pub fn set_frame_counter(&mut self, shared: std::sync::Arc<std::sync::atomic::AtomicU64>) {
        self.frame_counter = shared;
    }

    /// Re-render `elements` (+ the clear color) into an offscreen
    /// texture, read the pixels back to the CPU, analyze them, and
    /// emit `Event::FrameRendered`. Test-only — see Epic #1 req 6/7.
    ///
    /// We render a second time into our own target rather than reading
    /// back the `DrmCompositor` swapchain buffer: the swapchain frame
    /// is page-flip-owned and not portably CPU-mappable, whereas a
    /// fresh `Offscreen<GlesTexture>` + `ExportMem` is the canonical
    /// smithay screenshot path and yields pixel-identical content for
    /// the same element set and clear. The cost (an extra render +
    /// GPU→CPU sync) is exactly why this is feature-gated.
    #[cfg(feature = "frame_audit")]
    fn audit_frame(
        &mut self,
        output: &smithay::output::Output,
        elements: &[SceneElement],
        clear_color: [u8; 4],
    ) -> io::Result<()> {
        let (rgba, wu, hu) = self.read_frame_rgba(output, elements, clear_color)?;
        let stats = crate::frame_audit::analyze(&rgba, wu, hu);
        // Publish the frame for the D-Bus `Snapshot()` reader. A
        // poisoned lock must not abort the render loop.
        if let Ok(mut slot) = self.snapshot_buf.lock() {
            *slot = Some(crate::dbus::FrameBuf {
                rgba,
                width: wu,
                height: hu,
            });
        }
        // The caller (`render_one_frame`) bumped `frame_counter`
        // after `queue_frame` succeeded; subtract one so the first
        // frame is still emitted as id=0 (matches every existing
        // visual test's frame-id assumptions).
        halmasuit_introspect::emit(&halmasuit_introspect::Event::FrameRendered {
            frame_id: self
                .frame_counter
                .load(std::sync::atomic::Ordering::Relaxed)
                .wrapping_sub(1),
            pixel_count: stats.pixel_count,
            clear_pixel_count: stats.clear_pixel_count,
            black_pixel_count: stats.black_pixel_count,
            degenerate: stats.degenerate,
            phash: stats.phash,
        });
        // Wallpaper-plane-only auxiliary capture (closes C-G1: the
        // session-scene golden has niri's opaque fullscreen toplevel
        // covering the wallpaper plane, so all six matrix cells'
        // session goldens otherwise end up byte-identical). Re-render
        // ONLY the wallpaper element to an offscreen texture and
        // publish it to a separate slot the D-Bus
        // `SnapshotScene(path, "wallpaper-only")` call can read.
        //
        // We REUSE the wallpaper element the live render already
        // built — do NOT call `self.wallpaper.render_element` again.
        // Re-calling would advance per-frame state in
        // `ShaderBackend` / `VideoBackend` (frame_counter, last_frame,
        // texture upload), so the live render's NEXT frame would see
        // mid-frame state drift. Best-effort: a failure logs and
        // skips the publish so the main snapshot path keeps working.
        if let Err(e) = self.audit_wallpaper_only(output, elements, clear_color) {
            tracing::warn!(error = %e, "frame_audit wallpaper-only readback failed");
        }
        Ok(())
    }

    /// Render JUST the wallpaper element from `elements` (no cursor /
    /// no layers / no foreground toplevel) into an offscreen texture
    /// and publish the bytes to `wallpaper_only_buf`.
    ///
    /// Reuses the already-built wallpaper element from the live
    /// composition's `elements` slice rather than calling
    /// `WallpaperBackend::render_element` again — the shader + video
    /// backends mutate per-frame timing state on every call and a
    /// second call would corrupt the live render's next frame. If no
    /// wallpaper element is in the list (legacy-clear path), returns
    /// Ok without publishing.
    #[cfg(feature = "frame_audit")]
    fn audit_wallpaper_only(
        &mut self,
        output: &smithay::output::Output,
        elements: &[SceneElement],
        clear_color: [u8; 4],
    ) -> io::Result<()> {
        // Wallpaper variants live in the SceneElement enum.
        // `scene_elements` pushes the wallpaper at the bottom of the
        // front-to-back list (its `debug_assert_eq!(slot,
        // elements.len(), "wallpaper is the bottom-most element")`
        // pins this invariant), and the cursor is prepended AT THE
        // FRONT — so after cursor prepend the wallpaper is at the
        // back, i.e. `elements.last()`. `O(1)`; falls through to
        // `Ok(())` on the legacy-clear path where no wallpaper
        // element was pushed (the bottom element is then a layer or
        // toplevel, which the variant filter rejects).
        let Some(wallpaper_elem) = elements.last().filter(|e| {
            matches!(
                e,
                SceneElement::Wallpaper(_) | SceneElement::WallpaperShader(_)
            )
        }) else {
            return Ok(());
        };
        let one = std::slice::from_ref(wallpaper_elem);
        let (rgba, wu, hu) = self.read_frame_rgba(output, one, clear_color)?;
        if let Ok(mut slot) = self.wallpaper_only_buf.lock() {
            *slot = Some(crate::dbus::FrameBuf {
                rgba,
                width: wu,
                height: hu,
            });
        }
        Ok(())
    }

    /// Render `elements` (+ the clear color) into an offscreen texture
    /// and read it back as tightly-packed RGBA8 (`[R, G, B, A]` per
    /// pixel). Shared by `audit_frame` (analysis + `FrameRendered`)
    /// and the D-Bus `Snapshot()` publisher so the GL plumbing exists
    /// once. Returns `(rgba, width_px, height_px)`.
    ///
    /// Thin delegate to [`crate::offscreen::read_frame_rgba`] — the
    /// offscreen GLES render-target mechanism lives in its own cohesive
    /// `frame_audit`-gated module so production never compiles it.
    #[cfg(feature = "frame_audit")]
    fn read_frame_rgba(
        &mut self,
        output: &smithay::output::Output,
        elements: &[SceneElement],
        clear_color: [u8; 4],
    ) -> io::Result<(Vec<u8>, usize, usize)> {
        let color = xrgb_le_to_color32f(clear_color);
        crate::offscreen::read_frame_rgba(&mut self.renderer, output, elements, color)
    }

    /// Acknowledge a page-flip completion for the output scanning out
    /// on `crtc`. Called from the `DrmEvent::VBlank(crtc)` callback in
    /// calloop. Releases that output's previous front buffer for reuse.
    /// Returns the flipped `Output` (cloned) so the caller can drive
    /// per-output frame-callbacks / presentation feedback, or `None`
    /// if no output owns that CRTC (should not happen).
    ///
    /// # Errors
    ///
    /// Returns an error if smithay's `frame_submitted` reports an
    /// underlying DRM failure.
    pub fn frame_submitted(&mut self, crtc: crtc::Handle) -> io::Result<Option<Output>> {
        let Some(surface) = self.outputs.iter_mut().find(|o| o.crtc == crtc) else {
            return Ok(None);
        };
        surface
            .compositor
            .frame_submitted()
            .map_err(|e| io::Error::other(format!("frame_submitted: {e}")))?;
        Ok(Some(surface.output.clone()))
    }
}

/// Composite `elements` over `clear_color` onto one output's
/// `DrmCompositor` and queue the resulting frame for scanout. Free
/// function (not a method) so the caller can pass disjoint `&mut`
/// borrows of `self.outputs[i].compositor` and `self.renderer`.
/// Returns `Ok(true)` if a frame was queued, `Ok(false)` if the
/// composition had no damage (`render_frame` reported empty).
fn render_one_output(
    compositor: &mut DrmCompositor<
        GbmAllocator<DrmDeviceFd>,
        GbmFramebufferExporter<DrmDeviceFd>,
        (),
        DrmDeviceFd,
    >,
    renderer: &mut GlesRenderer,
    elements: &[SceneElement],
    clear_color: [u8; 4],
) -> io::Result<bool> {
    let color = xrgb_le_to_color32f(clear_color);
    let render_res = compositor
        .render_frame::<_, SceneElement>(renderer, elements, color, FrameFlags::DEFAULT)
        .map_err(|e| io::Error::other(format!("render_frame: {e}")))?;

    if render_res.is_empty {
        return Ok(false);
    }

    compositor
        .queue_frame(())
        .map_err(|e| io::Error::other(format!("queue_frame: {e}")))?;

    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Pin the `xrgb_le` byte ordering. The visual goldens depend on
    /// `[B, G, R, X]` little-endian; a refactor that transposes
    /// channels would trip this before any VM test runs.
    #[test]
    fn xrgb_le_pins_byte_order() {
        // #0a0014 → red=0x0a, green=0x00, blue=0x14
        // little-endian XRGB8888 → [B, G, R, X] = [0x14, 0x00, 0x0a, 0x00]
        assert_eq!(xrgb_le(0x0A, 0x00, 0x14), [0x14, 0x00, 0x0A, 0x00]);

        assert_eq!(xrgb_le(0xFF, 0x00, 0x00), [0x00, 0x00, 0xFF, 0x00]);
        assert_eq!(xrgb_le(0x00, 0xFF, 0x00), [0x00, 0xFF, 0x00, 0x00]);
        assert_eq!(xrgb_le(0x00, 0x00, 0xFF), [0xFF, 0x00, 0x00, 0x00]);
        assert_eq!(xrgb_le(0xFF, 0xFF, 0xFF), [0xFF, 0xFF, 0xFF, 0x00]);
    }

    /// Pin the `xrgb_le` → `Color32F` conversion. Reads the byte array
    /// back into floating-point RGB[1.0]A for smithay's `render_frame`.
    #[test]
    fn xrgb_le_to_color32f_round_trips_through_components() {
        let bytes = xrgb_le(0x0A, 0x00, 0x14);
        let color = xrgb_le_to_color32f(bytes);
        // Compare via f32 since Color32F's components are f32.
        let eps = 1.0 / 255.0 / 2.0;
        assert!((color.r() - f32::from(0x0A_u8) / 255.0).abs() < eps);
        assert!(color.g().abs() < eps);
        assert!((color.b() - f32::from(0x14_u8) / 255.0).abs() < eps);
        assert!((color.a() - 1.0).abs() < eps);
    }

    /// Pin the connector Interface → short-name prefix table used by
    /// `services.halmasuit.rendering.primaryOutput` matching. The
    /// exact spellings come from libdrm's drmModeGetConnectorName +
    /// what wlr-randr / niri surface; users will type these in their
    /// config so they must not drift.
    #[test]
    fn interface_prefix_pins_canonical_wayland_short_names() {
        use smithay::reexports::drm::control::connector::Interface;
        assert_eq!(interface_prefix(Interface::DisplayPort), "DP");
        assert_eq!(interface_prefix(Interface::HDMIA), "HDMI-A");
        assert_eq!(interface_prefix(Interface::HDMIB), "HDMI-B");
        assert_eq!(interface_prefix(Interface::EmbeddedDisplayPort), "eDP");
        assert_eq!(interface_prefix(Interface::DVII), "DVI-I");
        assert_eq!(interface_prefix(Interface::DVID), "DVI-D");
        assert_eq!(interface_prefix(Interface::VGA), "VGA");
        assert_eq!(interface_prefix(Interface::Virtual), "Virtual");
        // Unknown is the explicit fallback both for the named variant
        // and for any future #[non_exhaustive] additions.
        assert_eq!(interface_prefix(Interface::Unknown), "Unknown");
    }

    // The wallpaper z-order contract is unit-tested by
    // `crate::wallpaper::tests::wallpaper_is_the_bottom_most_element`
    // — the `wallpaper_slot` helper now lives there alongside the
    // engine that consumes it.

    // ── DRM device spec parsing + resolver tests ────────────────────
    // These tests cover [`DrmDeviceSpec::from_env_value`],
    // [`PciBdf::parse`], and the [`resolve_drm_device`] deadline
    // behavior for the Path arm. Auto and Pci runtime probing are
    // exercised by the multi-DRM VM test (DRM5).

    #[test]
    fn empty_env_parses_to_auto() {
        assert_eq!(
            DrmDeviceSpec::from_env_value("").unwrap(),
            DrmDeviceSpec::Auto
        );
    }

    #[test]
    fn path_env_parses_to_path() {
        match DrmDeviceSpec::from_env_value("/dev/dri/card1").unwrap() {
            DrmDeviceSpec::Path(p) => assert_eq!(p, PathBuf::from("/dev/dri/card1")),
            other => panic!("expected Path, got {other:?}"),
        }
    }

    #[test]
    fn pci_env_parses_to_pci_with_valid_bdf() {
        match DrmDeviceSpec::from_env_value("pci:0000:01:00.0").unwrap() {
            DrmDeviceSpec::Pci(b) => assert_eq!(b.as_str(), "0000:01:00.0"),
            other => panic!("expected Pci, got {other:?}"),
        }
    }

    #[test]
    fn pci_env_with_malformed_bdf_returns_error() {
        // Empty after the prefix.
        assert!(DrmDeviceSpec::from_env_value("pci:").is_err());
        // Not hex.
        assert!(DrmDeviceSpec::from_env_value("pci:bogus").is_err());
        // Wrong segment lengths.
        assert!(DrmDeviceSpec::from_env_value("pci:00:01:00.0").is_err());
        assert!(DrmDeviceSpec::from_env_value("pci:0000:1:00.0").is_err());
        assert!(DrmDeviceSpec::from_env_value("pci:0000:01:0.0").is_err());
        // Function digit > 7. Wrapped: BadBdf → BadFunction.
        assert!(matches!(
            DrmDeviceSpec::from_env_value("pci:0000:01:00.8"),
            Err(DrmDeviceSpecParseError::BadBdf(BdfParseError::BadFunction))
        ));
        // Missing function separator.
        assert!(DrmDeviceSpec::from_env_value("pci:0000:01:00").is_err());
    }

    #[test]
    fn non_dev_dri_card_path_rejected() {
        // Path shape validation (review S-1 hardening): the Path arm
        // must NOT accept arbitrary strings — only literal
        // `/dev/dri/cardN` paths. Defense-in-depth against future
        // wirings that might let a less-trusted source set the env.
        for bad in [
            "/etc/shadow",
            "/dev/null",
            "/dev/dri/renderD128", // valid DRM device but not a card
            "/dev/dri/card",       // missing the number
            "/dev/dri/card-foo",   // non-numeric suffix
            "/dev/dri/cardabc",    // hex isn't accepted; must be decimal digits
            "../../etc/passwd",
            "card0",
            "/dev/../etc/shadow",
        ] {
            assert!(
                matches!(
                    DrmDeviceSpec::from_env_value(bad),
                    Err(DrmDeviceSpecParseError::BadPath(_))
                ),
                "path {bad:?} should be rejected"
            );
        }
        // And the canonical shapes pass.
        for good in ["/dev/dri/card0", "/dev/dri/card1", "/dev/dri/card42"] {
            assert!(
                DrmDeviceSpec::from_env_value(good).is_ok(),
                "path {good:?} should be accepted"
            );
        }
    }

    #[test]
    fn parse_errors_do_not_echo_raw_input() {
        // Review S-3 hardening: BdfParseError's Display must NOT
        // include the offending input string. Defense-in-depth against
        // attacker-controlled bytes reaching journald via the error
        // path.
        let secret = "pci:0000:00:00.f"; // Bad function digit
        let err = DrmDeviceSpec::from_env_value(secret).unwrap_err();
        let msg = err.to_string();
        assert!(
            !msg.contains(secret) && !msg.contains("00.f"),
            "error message echoed raw input: {msg:?}"
        );
        // And a structural-format failure with attacker-shaped input.
        let secret2 = "pci:ATTACKER-CONTROLLED-STRING";
        let err2 = DrmDeviceSpec::from_env_value(secret2).unwrap_err();
        let msg2 = err2.to_string();
        assert!(
            !msg2.contains("ATTACKER"),
            "error message echoed raw input: {msg2:?}"
        );
    }

    #[test]
    fn pci_bdf_lowercase_and_uppercase_hex_both_accepted_normalized_to_lowercase() {
        assert_eq!(
            PciBdf::parse("0000:01:00.0").unwrap().as_str(),
            "0000:01:00.0"
        );
        assert_eq!(
            PciBdf::parse("ABCD:0F:00.0").unwrap().as_str(),
            "abcd:0f:00.0"
        );
        assert_eq!(
            PciBdf::parse("abcd:0f:00.0").unwrap().as_str(),
            "abcd:0f:00.0"
        );
    }

    #[test]
    fn pci_bdf_function_boundaries() {
        // 0..=7 must all parse.
        for f in 0..=7 {
            let s = format!("0000:01:00.{f:x}");
            assert!(PciBdf::parse(&s).is_ok(), "function {f} should parse");
        }
        // 8..=F must reject as BadFunction.
        for f in 8..=15 {
            let s = format!("0000:01:00.{f:x}");
            assert!(
                matches!(PciBdf::parse(&s), Err(BdfParseError::BadFunction)),
                "function {f:x} should be BadFunction"
            );
        }
    }

    #[test]
    fn resolve_path_mode_nonexistent_times_out_after_deadline() {
        let deadline = Duration::from_millis(300);
        let start = Instant::now();
        let r = resolve_drm_device(
            &DrmDeviceSpec::Path(PathBuf::from(
                "/dev/dri/halmasuit-test-definitely-does-not-exist",
            )),
            deadline,
        );
        let elapsed = start.elapsed();
        assert!(
            r.is_err(),
            "should fail for nonexistent path after deadline, got: {r:?}"
        );
        assert_eq!(r.unwrap_err().kind(), io::ErrorKind::NotFound);
        // Loop polls every 100ms; deadline 300ms gives ~3 iterations.
        // Total elapsed should be at least the deadline, and well under
        // 1s (no hang).
        assert!(
            elapsed >= deadline,
            "resolver returned before deadline: {elapsed:?}"
        );
        assert!(
            elapsed < Duration::from_millis(1500),
            "resolver hung past deadline: {elapsed:?}"
        );
    }

    #[test]
    fn resolve_path_mode_existing_returns_immediately() {
        // /dev/null exists on every test host; use it as a stand-in
        // for "an extant path the resolver should accept." (The
        // resolver doesn't validate the path is actually a DRM
        // device — that's `setup_drm_direct`'s job.)
        let start = Instant::now();
        let deadline = Duration::from_millis(500);
        let r = resolve_drm_device(&DrmDeviceSpec::Path(PathBuf::from("/dev/null")), deadline)
            .expect("/dev/null should resolve");
        let elapsed = start.elapsed();
        assert_eq!(r, PathBuf::from("/dev/null"));
        // Immediacy assertion: the resolver must NOT sleep when the
        // path already exists. A regression that always sleeps the
        // full deadline (or `RESOLVE_POLL_INTERVAL`) before checking
        // would still produce the right path; this assertion catches
        // that class.
        assert!(
            elapsed < RESOLVE_POLL_INTERVAL,
            "resolver should return without sleeping when path exists; \
             elapsed={elapsed:?} (poll interval={RESOLVE_POLL_INTERVAL:?})"
        );
    }
}
