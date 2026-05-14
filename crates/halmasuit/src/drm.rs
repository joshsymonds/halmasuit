// halmasuit/src/drm.rs — DRM backend (B.2 slice: GLES + GBM + DrmCompositor).
//
// Production renderer wiring. Halmasuit owns the DRM device, runs a GLES
// renderer through smithay's `DrmCompositor`, and scans out a brand-clear
// `#0a0014` first frame (the same color the B.1 dumb-buffer path painted)
// before any wl_client has connected. Subsequent subtasks layer
// wlr-layer-shell + buffer import + frame_audit on top of this same
// pipeline.
//
// Pattern lifted from niri's `src/backend/tty.rs` + smithay's anvil
// example at the pinned `ff5fa7df` rev, simplified to:
//
//   * Single GPU, single output, single CRTC (no MultiRenderer, no udev
//     hot-plug, no multi-monitor)
//   * Render loop driven by `DrmEvent::VBlank` from the
//     `DrmDeviceNotifier` calloop source
//   * Empty element set — the entire scene is the clear color until B.3
//     adds wlr-layer-shell

use std::fs::OpenOptions;
use std::io;
use std::os::fd::OwnedFd;
use std::path::Path;

use smithay::backend::allocator::Fourcc;
use smithay::backend::allocator::gbm::{GbmAllocator, GbmBufferFlags, GbmDevice};
use smithay::backend::drm::compositor::{DrmCompositor, FrameFlags};
use smithay::backend::drm::exporter::gbm::GbmFramebufferExporter;
use smithay::backend::drm::{DrmDevice, DrmDeviceFd, DrmEvent, DrmSurface};
use smithay::backend::egl::{EGLContext, EGLDisplay};
use smithay::backend::renderer::Color32F;
use smithay::backend::renderer::gles::GlesRenderer;
use smithay::output::{Mode as OutputMode, Output, OutputModeSource, PhysicalProperties, Subpixel};
use smithay::reexports::drm::Device as DrmDeviceTrait;
use smithay::reexports::drm::control::Device as ControlDevice;
use smithay::reexports::drm::control::connector;
use smithay::utils::DeviceFd;

/// Transient newtype used only by [`open_and_set_master`] to call
/// `acquire_master_lock` via the drm-rs typed wrapper. The kernel-side
/// master designation lives on the file descriptor, not on this
/// handle, so callers receive the raw `OwnedFd` after master has been
/// taken — `MasterCard` itself is dropped immediately.
struct MasterCard(std::fs::File);

impl std::os::fd::AsFd for MasterCard {
    fn as_fd(&self) -> std::os::fd::BorrowedFd<'_> {
        self.0.as_fd()
    }
}
impl DrmDeviceTrait for MasterCard {}
impl ControlDevice for MasterCard {}

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

/// Open `path` for DRM access and acquire master via drm-rs's typed
/// wrapper. Returns the device's `OwnedFd` so the caller can hand it
/// to both smithay's `DrmDeviceFd::new` (which retries master
/// acquisition idempotently — the second acquire fails with EBUSY,
/// which smithay handles cleanly) and `GbmDevice::new` (via clone).
///
/// drm-master-probe Phases 0–3 validated that the master designation
/// lives on the file descriptor and survives `setresuid` to the
/// compositor user (halmasuit's privilege drop).
///
/// # Errors
///
/// Bubbles `open(2)` or `DRM_IOCTL_SET_MASTER` failures with context.
pub fn open_and_set_master(path: &Path) -> io::Result<OwnedFd> {
    let dev = OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)
        .map_err(|e| io::Error::other(format!("open({}): {e}", path.display())))?;

    let master = MasterCard(dev);
    master
        .acquire_master_lock()
        .map_err(|e| io::Error::other(format!("DRM SET_MASTER on {}: {e}", path.display())))?;
    Ok(master.0.into())
}

/// The full GLES + GBM + DrmCompositor stack wrapped around a single
/// DRM device + connector + CRTC. Pinned for the process lifetime in
/// `HalmasuitState`. Dropping this value releases the master, tears
/// down EGL, and lets the kernel reset the CRTC.
pub struct DrmBackend {
    /// The smithay `DrmCompositor` driving our single CRTC. Owns the
    /// `DrmSurface` (and through it the `crtc::Handle`), the GBM
    /// allocator, the framebuffer exporter, and the swapchain. Pinned
    /// for the process lifetime; dropping it releases the surface
    /// (which releases the CRTC) and unrefs the GBM device.
    pub compositor: DrmCompositor<
        GbmAllocator<DrmDeviceFd>,
        GbmFramebufferExporter<DrmDeviceFd>,
        (),
        DrmDeviceFd,
    >,
    /// GLES renderer bound to the GBM device's EGL display. Used by
    /// `render_frame` every vblank to clear + composite.
    pub renderer: GlesRenderer,
}

/// Set up the full DRM/GBM/EGL/GLES/DrmCompositor stack on the device
/// at `path`. Acquires master, picks the first connected connector +
/// its preferred mode + first CRTC, builds a GBM allocator + EGL
/// display + GLES renderer + DrmCompositor wrapping that surface,
/// registers the DRM event source with calloop for vblank
/// notifications, and returns the retained backend plus a smithay
/// `Output` for the caller to register as a global.
///
/// The caller is responsible for calling `output.create_global::<S>(&display_handle)`
/// after this returns — we don't do it here because that call requires
/// `S: GlobalDispatch<WlOutput, …>` which is implemented at the
/// caller's site, not in this module.
///
/// `drm_event_handler` is invoked from inside the calloop callback
/// when `DrmEvent::VBlank` fires.
///
/// # Errors
///
/// Bubbles any DRM ioctl, GBM allocation, EGL initialization, or
/// calloop registration failure with context.
pub fn setup_drm_backend<S, F>(
    path: &Path,
    loop_handle: &smithay::reexports::calloop::LoopHandle<'static, S>,
    drm_event_handler: F,
) -> io::Result<(
    DrmBackend,
    smithay::reexports::calloop::RegistrationToken,
    Output,
)>
where
    S: 'static,
    F: FnMut(DrmEvent, &mut Option<smithay::backend::drm::DrmEventMetadata>, &mut S) + 'static,
{
    let owned_fd = open_and_set_master(path)?;
    let device_fd = DrmDeviceFd::new(DeviceFd::from(owned_fd));

    // DrmDevice + its event notifier. `drm` must be `mut` so we can
    // call `create_surface` on it below.
    let (mut drm, notifier) = DrmDevice::new(device_fd.clone(), true)
        .map_err(|e| io::Error::other(format!("DrmDevice::new: {e}")))?;

    // Pick connector + mode + CRTC. Same shape as the B.1 slice; the
    // resource-handle enumeration goes through drm-rs (re-exposed via
    // smithay's `DrmDevice` deref).
    let res = drm
        .resource_handles()
        .map_err(|e| io::Error::other(format!("resource_handles: {e}")))?;

    let connector_info = res
        .connectors()
        .iter()
        .filter_map(|&h| drm.get_connector(h, true).ok())
        .find(|info| info.state() == connector::State::Connected)
        .ok_or_else(|| io::Error::other("no connected DRM connector"))?;

    let mode = *connector_info
        .modes()
        .first()
        .ok_or_else(|| io::Error::other("connected DRM connector has no modes"))?;
    let (w, h) = mode.size();

    let &crtc_handle = res
        .crtcs()
        .first()
        .ok_or_else(|| io::Error::other("no DRM CRTCs available"))?;

    // Create the DRM surface — smithay's higher-level wrapper above
    // `drmModeSetCrtc`.
    let surface: DrmSurface = drm
        .create_surface(crtc_handle, mode, &[connector_info.handle()])
        .map_err(|e| io::Error::other(format!("DrmDevice::create_surface: {e}")))?;

    // GBM device on the same fd. Allocator pulls SCANOUT-capable
    // buffers from this device; the framebuffer exporter wraps the
    // resulting BOs as DRM framebuffer handles.
    let gbm =
        GbmDevice::new(device_fd).map_err(|e| io::Error::other(format!("GbmDevice::new: {e}")))?;

    // EGL display + context + GLES renderer. The two `unsafe`s are
    // smithay's API contracts: EGLDisplay::new takes a native display
    // pointer and trusts the caller it's a valid GBM device, and
    // GlesRenderer::new requires the context not be active on another
    // thread (it isn't — this is the main thread).
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
    let renderer = unsafe { GlesRenderer::new(egl_context) }
        .map_err(|e| io::Error::other(format!("GlesRenderer::new: {e}")))?;

    // Allocator + framebuffer exporter, both wrapping the same GBM
    // device. SCANOUT is non-optional (the buffer must be scannable
    // out by the CRTC); RENDERING marks it as a render target.
    let allocator = GbmAllocator::new(
        gbm.clone(),
        GbmBufferFlags::RENDERING | GbmBufferFlags::SCANOUT,
    );
    // `NodeFilter::None` means "use the GBM device for all framebuffer
    // exports regardless of source DrmNode" — fine for single-GPU.
    let exporter = GbmFramebufferExporter::new(
        gbm.clone(),
        smithay::backend::drm::exporter::gbm::NodeFilter::None,
    );

    // Smithay output backed by the real DRM mode (no more synthesized
    // 1920×1080 placeholder).
    let output_mode = OutputMode {
        size: (i32::from(w), i32::from(h)).into(),
        // smithay's `Mode::refresh` is in mHz; the DRM `vrefresh` is
        // in Hz. Convert.
        refresh: i32::try_from(mode.vrefresh()).unwrap_or(60_000) * 1000,
    };
    let physical = PhysicalProperties {
        // (0, 0) signals "unknown" per the wl_output spec — virtio-gpu
        // doesn't report physical dimensions.
        size: (0, 0).into(),
        subpixel: Subpixel::Unknown,
        make: "halmasuit".to_owned(),
        model: format!("drm-{w}x{h}"),
        serial_number: String::new(),
    };
    // Output global creation requires `S: GlobalDispatch<WlOutput,
    // WlOutputData>`. Rather than propagate that bound up through
    // every caller of `setup_drm_backend`, we hand the Output back to
    // the caller and let it register the global from a context where
    // the concrete state type's `GlobalDispatch` impl is visible.
    let output = Output::new("output-0".to_owned(), physical);
    output.change_current_state(Some(output_mode), None, None, Some((0, 0).into()));
    output.set_preferred(output_mode);

    // DrmCompositor: the workhorse. Drives the surface, manages the
    // GBM-backed swapchain, queues page-flips, owns plane assignment.
    // The `OutputModeSource::Auto(output.downgrade())` ties the
    // compositor's working size + scale to the smithay `Output` we
    // just registered — `change_current_state` updates flow through
    // automatically.
    let render_formats = renderer
        .egl_context()
        .dmabuf_render_formats()
        .iter()
        .copied()
        .collect::<Vec<_>>();
    let cursor_size = drm.cursor_size();
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
        render_formats,
        cursor_size,
        Some(gbm),
    )
    .map_err(|e| io::Error::other(format!("DrmCompositor::new: {e}")))?;

    // Register the DRM event notifier with calloop. The notifier
    // emits `DrmEvent::VBlank(crtc)` on every page-flip ack and
    // `DrmEvent::Error(e)` on async DRM errors. The caller-provided
    // handler is what drives our render loop.
    let registration_token = loop_handle
        .insert_source(notifier, drm_event_handler)
        .map_err(|e| io::Error::other(format!("calloop register DrmDeviceNotifier: {e}")))?;

    Ok((
        DrmBackend {
            compositor,
            renderer,
        },
        registration_token,
        output,
    ))
}

impl DrmBackend {
    /// Render one frame and queue it on the surface. Returns `Ok(true)`
    /// if a frame was actually queued (non-empty damage), `Ok(false)`
    /// if no damage and no flip was queued. The clear color is always
    /// the halmasuit brand `#0a0014` — by B.2 there are still no
    /// wl_clients to composite, so the clear IS the entire scene.
    ///
    /// # Errors
    ///
    /// Returns an error if `render_frame` or `queue_frame` fail.
    pub fn render_one_frame(&mut self, clear_color: [u8; 4]) -> io::Result<bool> {
        // No wl_clients yet; element type only needs to satisfy
        // `RenderElement<GlesRenderer>` for the call to type-check. We
        // pick a concrete type from smithay and pass an empty slice.
        // (Texture render elements are the simplest concrete shape;
        // B.3 will replace this with a layer-shell-aware element enum.)
        type EmptyElement = smithay::backend::renderer::element::texture::TextureRenderElement<
            smithay::backend::renderer::gles::GlesTexture,
        >;
        let elements: &[EmptyElement] = &[];

        let color = xrgb_le_to_color32f(clear_color);
        let render_res = self
            .compositor
            .render_frame::<_, EmptyElement>(
                &mut self.renderer,
                elements,
                color,
                FrameFlags::DEFAULT,
            )
            .map_err(|e| io::Error::other(format!("render_frame: {e}")))?;

        if render_res.is_empty {
            return Ok(false);
        }

        self.compositor
            .queue_frame(())
            .map_err(|e| io::Error::other(format!("queue_frame: {e}")))?;
        Ok(true)
    }

    /// Acknowledge a page-flip completion. Called from the
    /// `DrmEvent::VBlank` callback in calloop. Releases the previous
    /// front buffer for reuse and emits any presentation-time
    /// feedback (none configured in this slice).
    ///
    /// # Errors
    ///
    /// Returns an error if smithay's `frame_submitted` reports an
    /// underlying DRM failure.
    pub fn frame_submitted(&mut self) -> io::Result<()> {
        self.compositor
            .frame_submitted()
            .map_err(|e| io::Error::other(format!("frame_submitted: {e}")))?;
        Ok(())
    }
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
}
