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
use std::path::Path;

use smithay::backend::allocator::Fourcc;
use smithay::backend::allocator::gbm::{GbmAllocator, GbmBufferFlags, GbmDevice};
use smithay::backend::drm::compositor::{DrmCompositor, FrameFlags};
use smithay::backend::drm::exporter::gbm::GbmFramebufferExporter;
use smithay::backend::drm::{DrmDevice, DrmDeviceFd, DrmEvent, DrmSurface};
use smithay::backend::egl::{EGLContext, EGLDisplay};
use smithay::backend::renderer::Color32F;
use smithay::backend::renderer::element::render_elements;
use smithay::backend::renderer::element::surface::WaylandSurfaceRenderElement;
use smithay::backend::renderer::element::texture::TextureRenderElement;
use smithay::backend::renderer::gles::{GlesRenderer, GlesTexture};
use smithay::backend::session::Session;
use smithay::backend::session::libseat::LibSeatSession;
use smithay::output::{Mode as OutputMode, Output, OutputModeSource, PhysicalProperties, Subpixel};
use smithay::reexports::drm::control::Device as ControlDevice;
use smithay::reexports::drm::control::connector;
use smithay::reexports::rustix::fs::OFlags;
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

render_elements! {
    /// One frame's render elements. smithay element lists are
    /// front-to-back (index 0 = topmost, drawn last). `Surface` wraps
    /// a committed wl_client subtree; `Wallpaper` /
    /// `WallpaperShader` are halmasuit's internal full-output
    /// background plane (image-backed and shader-backed
    /// respectively), always the LAST element so every surface
    /// composites over it (epic G1/R3/R6). Exactly one wallpaper
    /// variant is produced per frame — the engine's active backend
    /// picks which.
    pub SceneElement<=GlesRenderer>;
    Surface = WaylandSurfaceRenderElement<GlesRenderer>,
    Wallpaper = TextureRenderElement<GlesTexture>,
    WallpaperShader = smithay::backend::renderer::gles::element::PixelShaderElement,
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
    /// The wallpaper engine — owns the active backend (image / shader
    /// / video) and builds the bottom-most render element every frame
    /// (epic G1/R3/R6). When no backend is configured the engine
    /// produces no element — the legacy clear-only path for
    /// non-visual integration tests; production/visual deployments
    /// always configure one.
    wallpaper: WallpaperEngine,
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

    /// Periodic tick that drives the wallpaper backend's
    /// render-loop-independent polling AND the fallback-swap
    /// check. Called from a calloop timer registered in
    /// [`setup_drm_backend`] for `WallpaperConfig::Video`
    /// configurations. For non-video backends this is a no-op.
    pub fn tick_wallpaper(&mut self) {
        self.wallpaper.tick(&mut self.renderer);
    }
}

/// Set up the full DRM/GBM/EGL/GLES/DrmCompositor stack on the device
/// at `path`. The DRM fd is opened through the libseat `session`
/// (seatd brokers it and owns DRM master — halmasuit never issues
/// `SET_MASTER`; the improved privilege posture validated by
/// drm-master-probe Phase 4). Picks the first connected connector +
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
    session: &mut LibSeatSession,
    path: &Path,
    loop_handle: &smithay::reexports::calloop::LoopHandle<'static, S>,
    drm_event_handler: F,
    wallpaper_config: Option<WallpaperConfig>,
) -> io::Result<(
    DrmBackend,
    smithay::reexports::calloop::RegistrationToken,
    Output,
)>
where
    S: 'static,
    F: FnMut(DrmEvent, &mut Option<smithay::backend::drm::DrmEventMetadata>, &mut S) + 'static,
{
    // seatd brokers the DRM fd (it holds master; we never SET_MASTER).
    // O_CLOEXEC|O_NONBLOCK|O_RDWR matches the anvil udev pattern.
    let owned_fd = session
        .open(path, OFlags::RDWR | OFlags::CLOEXEC | OFlags::NONBLOCK)
        .map_err(|e| io::Error::other(format!("libseat session.open({}): {e}", path.display())))?;
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
    let mut renderer = unsafe { GlesRenderer::new(egl_context) }
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

    // Build the wallpaper engine. Each backend's constructor decodes
    // / compiles synchronously, so the engine is frame-0 ready when
    // this returns (epic G1/R3/R6 — every frame the renderer
    // composites after this is wallpaper-covered). `renderer` is no
    // longer borrowed (the immutable `render_formats` borrow above
    // has ended) and is moved into `DrmBackend` just below.
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
            compositor,
            renderer,
            wallpaper,
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
        Ok(elements)
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
    pub fn render_one_frame(
        &mut self,
        output: &smithay::output::Output,
        clear_color: [u8; 4],
    ) -> io::Result<bool> {
        let elements = self.scene_elements(output, None)?;
        self.render_with_elements_inner(output, &elements, clear_color)
    }

    /// Render a frame composed of the mapped layer-shell surfaces and
    /// the optional foreground toplevel over the wallpaper plane.
    /// See [`scene_elements`](Self::scene_elements) for z-order.
    ///
    /// # Errors
    ///
    /// Returns an error if `scene_elements`, `render_frame`, or
    /// `queue_frame` fail.
    pub fn render_layer_elements(
        &mut self,
        output: &smithay::output::Output,
        foreground: Option<&smithay::reexports::wayland_server::protocol::wl_surface::WlSurface>,
        clear_color: [u8; 4],
    ) -> io::Result<bool> {
        let elements = self.scene_elements(output, foreground)?;
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
            pixel_count: stats.pixel_count,
            clear_pixel_count: stats.clear_pixel_count,
            black_pixel_count: stats.black_pixel_count,
            degenerate: stats.degenerate,
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
    ///
    /// Thin delegate to [`crate::offscreen::read_frame_rgba`] — the
    /// offscreen GLES render-target mechanism lives in its own cohesive
    /// `frame_audit`-gated module so production never compiles it.
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
        let color = xrgb_le_to_color32f(clear_color);
        crate::offscreen::read_frame_rgba(&mut self.renderer, output, elements, color)
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

    // The wallpaper z-order contract is unit-tested by
    // `crate::wallpaper::tests::wallpaper_is_the_bottom_most_element`
    // — the `wallpaper_slot` helper now lives there alongside the
    // engine that consumes it.
}
