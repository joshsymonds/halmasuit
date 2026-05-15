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
    /// Monotonic frame counter for the `frame_audit` `FrameRendered`
    /// stream. Only exists in `halmasuit-debug`.
    #[cfg(feature = "frame_audit")]
    frame_counter: u64,
    /// Latest composited frame, published every audited frame for the
    /// D-Bus `Snapshot()` method to read. Only exists in
    /// `halmasuit-debug`.
    #[cfg(feature = "frame_audit")]
    snapshot_buf: crate::dbus::SnapshotBuffer,
}

impl DrmBackend {
    /// A clone of the shared snapshot slot, to hand to the D-Bus
    /// server. The render loop publishes into it; `Snapshot()` reads.
    #[cfg(feature = "frame_audit")]
    #[must_use]
    pub fn snapshot_handle(&self) -> crate::dbus::SnapshotBuffer {
        self.snapshot_buf.clone()
    }
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
// reason: a single linear DRM→GBM→EGL→GLES→DrmCompositor→calloop
// init sequence. The ordering is load-bearing (master before GBM,
// EGL before GLES, surface before compositor); splitting it into
// helpers scatters that ordering across the module for no
// readability or testability gain.
#[allow(
    clippy::too_many_lines,
    reason = "linear hardware-init sequence; ordering is load-bearing"
)]
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
            #[cfg(feature = "frame_audit")]
            frame_counter: 0,
            #[cfg(feature = "frame_audit")]
            snapshot_buf: crate::dbus::new_buffer(),
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
    pub fn render_one_frame(
        &mut self,
        output: &smithay::output::Output,
        clear_color: [u8; 4],
    ) -> io::Result<bool> {
        // Equivalent to calling `render_layer_elements` with no layer
        // surfaces mapped — empty element list, just the clear color.
        // Kept as a thin alias so the B.2 initial-render call site in
        // main()'s startup still reads naturally.
        use smithay::backend::renderer::element::surface::WaylandSurfaceRenderElement;
        let elements: &[WaylandSurfaceRenderElement<GlesRenderer>] = &[];
        self.render_with_elements_inner(output, elements, clear_color)
    }

    /// Render a frame composed of layer-shell surfaces mapped onto
    /// `output`'s LayerMap, on top of the brand clear color. Walks
    /// the map in z-order (BACKGROUND, BOTTOM, TOP, OVERLAY) and
    /// builds a `WaylandSurfaceRenderElement<GlesRenderer>` per
    /// surface via `render_elements_from_surface_tree` — the renderer
    /// lazily imports committed `wl_shm` buffers as `GlesTexture`s
    /// during the draw.
    ///
    /// # Errors
    ///
    /// Returns an error if `render_frame` or `queue_frame` fail.
    pub fn render_layer_elements(
        &mut self,
        output: &smithay::output::Output,
        clear_color: [u8; 4],
    ) -> io::Result<bool> {
        use smithay::backend::renderer::element::Kind;
        use smithay::backend::renderer::element::surface::{
            WaylandSurfaceRenderElement, render_elements_from_surface_tree,
        };
        use smithay::desktop::layer_map_for_output;
        use smithay::utils::Scale;
        use smithay::wayland::shell::wlr_layer::Layer as WlrLayer;

        let map = layer_map_for_output(output);
        let mut elements: Vec<WaylandSurfaceRenderElement<GlesRenderer>> = Vec::new();
        // smithay render-element lists are FRONT-TO-BACK: the first
        // element is topmost (drawn last, over the rest). Walk the
        // wlr-layer stack from the top down — Overlay, Top, Bottom,
        // Background — so the persistent background ends up beneath
        // everything else rather than painted over it.
        for which in [
            WlrLayer::Overlay,
            WlrLayer::Top,
            WlrLayer::Bottom,
            WlrLayer::Background,
        ] {
            for layer in map.layers_on(which) {
                let surface = layer.wl_surface();
                // Honor the LayerMap-computed geometry — a centered or
                // anchored non-fullscreen layer must render at its
                // actual position, not (0,0). Scale 1.0, so logical
                // coords map 1:1 to physical.
                let loc = map.layer_geometry(layer).map(|g| g.loc).unwrap_or_default();
                let location: smithay::utils::Point<i32, smithay::utils::Physical> =
                    (loc.x, loc.y).into();
                let scale = Scale::from(1.0);
                let mut surface_elements: Vec<WaylandSurfaceRenderElement<GlesRenderer>> =
                    render_elements_from_surface_tree(
                        &mut self.renderer,
                        surface,
                        location,
                        scale,
                        1.0,
                        Kind::Unspecified,
                    );
                elements.append(&mut surface_elements);
            }
        }
        drop(map);

        self.render_with_elements_inner(output, &elements, clear_color)
    }

    fn render_with_elements_inner<E>(
        &mut self,
        output: &smithay::output::Output,
        elements: &[E],
        clear_color: [u8; 4],
    ) -> io::Result<bool>
    where
        E: smithay::backend::renderer::element::RenderElement<GlesRenderer>,
    {
        let color = xrgb_le_to_color32f(clear_color);
        let render_res = self
            .compositor
            .render_frame::<_, E>(&mut self.renderer, elements, color, FrameFlags::DEFAULT)
            .map_err(|e| io::Error::other(format!("render_frame: {e}")))?;

        if render_res.is_empty {
            return Ok(false);
        }

        self.compositor
            .queue_frame(())
            .map_err(|e| io::Error::other(format!("queue_frame: {e}")))?;

        // The frame is now queued for scanout. Under `frame_audit`
        // (halmasuit-debug only) re-render the identical element set
        // into an offscreen texture, read it back, analyze it, and
        // emit `FrameRendered`. This is the per-frame GPU readback the
        // production binary deliberately omits (Epic #1 req 6/7).
        #[cfg(feature = "frame_audit")]
        {
            // Best-effort: an audit failure must never take down the
            // compositor. Log and continue.
            if let Err(e) = self.audit_frame(output, elements, clear_color) {
                tracing::warn!(error = %e, "frame_audit readback failed");
            }
        }
        #[cfg(not(feature = "frame_audit"))]
        let _ = output;

        Ok(true)
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
    fn audit_frame<E>(
        &mut self,
        output: &smithay::output::Output,
        elements: &[E],
        clear_color: [u8; 4],
    ) -> io::Result<()>
    where
        E: smithay::backend::renderer::element::RenderElement<GlesRenderer>,
    {
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
        halmasuit_introspect::emit(&halmasuit_introspect::Event::FrameRendered {
            frame_id: self.frame_counter,
            mean_luminance: stats.mean_luminance,
            backdrop_coverage: stats.backdrop_coverage,
            phash: stats.phash,
        });
        self.frame_counter += 1;
        Ok(())
    }

    /// Render `elements` (+ the clear color) into an offscreen texture
    /// and read it back as tightly-packed RGBA8 (`[R, G, B, A]` per
    /// pixel). Shared by `audit_frame` (analysis + `FrameRendered`)
    /// and the D-Bus `Snapshot()` publisher so the GL plumbing exists
    /// once. Returns `(rgba, width_px, height_px)`.
    #[cfg(feature = "frame_audit")]
    fn read_frame_rgba<E>(
        &mut self,
        output: &smithay::output::Output,
        elements: &[E],
        clear_color: [u8; 4],
    ) -> io::Result<(Vec<u8>, usize, usize)>
    where
        E: smithay::backend::renderer::element::RenderElement<GlesRenderer>,
    {
        use smithay::backend::allocator::Fourcc;
        use smithay::backend::renderer::damage::OutputDamageTracker;
        use smithay::backend::renderer::gles::GlesTexture;
        use smithay::backend::renderer::{Bind, ExportMem, Offscreen};
        use smithay::utils::{Point, Rectangle, Size};

        let mode = output
            .current_mode()
            .ok_or_else(|| io::Error::other("frame_audit: output has no current mode"))?;
        let (w, h) = (mode.size.w, mode.size.h);
        let (wu, hu) = (
            usize::try_from(w).map_err(|_| io::Error::other("frame_audit: negative mode width"))?,
            usize::try_from(h)
                .map_err(|_| io::Error::other("frame_audit: negative mode height"))?,
        );
        if wu == 0 || hu == 0 {
            return Err(io::Error::other("frame_audit: zero mode size"));
        }
        let color = xrgb_le_to_color32f(clear_color);

        let mut tex: GlesTexture = Offscreen::<GlesTexture>::create_buffer(
            &mut self.renderer,
            Fourcc::Abgr8888,
            (w, h).into(),
        )
        .map_err(|e| io::Error::other(format!("frame_audit create_buffer: {e}")))?;
        let rgba = {
            let mut fb = Bind::bind(&mut self.renderer, &mut tex)
                .map_err(|e| io::Error::other(format!("frame_audit bind: {e}")))?;
            let mut dt = OutputDamageTracker::from_output(output);
            dt.render_output(&mut self.renderer, &mut fb, 0, elements, color)
                .map_err(|e| io::Error::other(format!("frame_audit render_output: {e:?}")))?;
            let region = Rectangle::new(Point::from((0, 0)), Size::from((w, h)));
            let mapping =
                ExportMem::copy_framebuffer(&mut self.renderer, &fb, region, Fourcc::Abgr8888)
                    .map_err(|e| io::Error::other(format!("frame_audit copy_framebuffer: {e}")))?;
            // `fb` (and its tex binding) is unused past the readback;
            // drop it before `map_texture` per significant_drop_tightening.
            drop(fb);
            let bytes = ExportMem::map_texture(&mut self.renderer, &mapping)
                .map_err(|e| io::Error::other(format!("frame_audit map_texture: {e}")))?;
            bytes.to_vec()
        };
        Ok((rgba, wu, hu))
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
