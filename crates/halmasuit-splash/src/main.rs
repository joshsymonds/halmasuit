// halmasuit-splash — halmasuit's system background wl_client.
//
// Connects to halmasuit's `wayland-0`, binds wlr-layer-shell with role
// BACKGROUND anchored to all edges (exclusive zone 0), and renders the
// PNG named by `HALMASUIT_SPLASH_IMAGE` fullscreen via wgpu: one
// fragment shader sampling the image as a texture on a fullscreen
// triangle. Holds until SIGTERM.
//
// v1 is deliberately boring (Epic #1 anti-patterns): one shader, one
// PNG, one fullscreen quad, fill (no aspect preservation), no
// animation / video / multi-scene state. "Fancy splash" is a future
// epic.
//
// In the software-Mesa VM (LIBGL_ALWAYS_SOFTWARE + llvmpipe, wired by
// nix/module.nix) wgpu's GL backend drives Mesa's swrast wayland-EGL
// platform, which allocates `wl_shm` buffers — exactly what
// halmasuit's existing shm import composites. We force `Backends::GL`
// to match that substrate.

// reason: the only `unsafe` is the single `create_surface_unsafe`
// FFI bridge handing wayland's wl_display/wl_surface pointers to
// wgpu; every use is annotated with a SAFETY note at the call site.
#![allow(
    unsafe_code,
    reason = "single wgpu raw-surface FFI bridge; see SAFETY at call site"
)]

use std::ptr::NonNull;

use anyhow::{Context, Result, anyhow};
use raw_window_handle::{
    RawDisplayHandle, RawWindowHandle, WaylandDisplayHandle, WaylandWindowHandle,
};
use smithay_client_toolkit::compositor::{CompositorHandler, CompositorState};
use smithay_client_toolkit::output::{OutputHandler, OutputState};
use smithay_client_toolkit::reexports::client::globals::registry_queue_init;
use smithay_client_toolkit::reexports::client::protocol::{wl_output, wl_surface};
use smithay_client_toolkit::reexports::client::{Connection, QueueHandle};
use smithay_client_toolkit::registry::{ProvidesRegistryState, RegistryState};
use smithay_client_toolkit::registry_handlers;
use smithay_client_toolkit::shell::WaylandSurface;
use smithay_client_toolkit::shell::wlr_layer::{
    Anchor, KeyboardInteractivity, Layer, LayerShell, LayerShellHandler, LayerSurface,
    LayerSurfaceConfigure,
};
use smithay_client_toolkit::{
    delegate_compositor, delegate_layer, delegate_output, delegate_registry,
};
// `Proxy` brings `.id()` into scope; with the `dlopen` feature the
// resulting `ObjectId`/`Backend` are the system-backend types that
// expose the libwayland pointers.
use wayland_client::Proxy;

/// Decoded splash image: tightly-packed RGBA8 plus dimensions.
struct SplashImage {
    rgba: Vec<u8>,
    width: u32,
    height: u32,
}

fn load_splash_image() -> Result<SplashImage> {
    let path = std::env::var_os("HALMASUIT_SPLASH_IMAGE")
        .ok_or_else(|| anyhow!("HALMASUIT_SPLASH_IMAGE is not set"))?;
    let bytes = std::fs::read(&path)
        .with_context(|| format!("reading splash image {}", path.to_string_lossy()))?;
    let img = image::load_from_memory(&bytes)
        .with_context(|| format!("decoding splash image {}", path.to_string_lossy()))?
        .to_rgba8();
    let (width, height) = img.dimensions();
    Ok(SplashImage {
        rgba: img.into_raw(),
        width,
        height,
    })
}

/// Live wgpu state, built lazily once the first layer-shell configure
/// gives us a concrete output size.
struct Gpu {
    device: wgpu::Device,
    queue: wgpu::Queue,
    surface: wgpu::Surface<'static>,
    config: wgpu::SurfaceConfiguration,
    pipeline: wgpu::RenderPipeline,
    bind_group: wgpu::BindGroup,
}

impl Gpu {
    fn init(
        conn: &Connection,
        layer: &LayerSurface,
        img: &SplashImage,
        w: u32,
        h: u32,
    ) -> Result<Self> {
        let instance = wgpu::Instance::new(wgpu::InstanceDescriptor {
            backends: wgpu::Backends::GL,
            ..Default::default()
        });

        // SAFETY: `conn` and `layer` (and its wl_surface) are owned by
        // the caller's `Splash` state and outlive this `Gpu` and the
        // returned `Surface`. `display_ptr()`/`id().as_ptr()` are the
        // libwayland pointers wgpu's wayland WSI expects; both are
        // non-null for a live connection/surface.
        // libwayland pointers (system backend, enabled via the
        // `wayland-client/dlopen` feature). `display_ptr()` is the
        // raw `*mut wl_display`; `wl_surface.id().as_ptr()` is the
        // `*mut wl_proxy` which, for a wl_surface, is the
        // `wl_surface*` wgpu's wayland WSI expects.
        let display = NonNull::new(conn.backend().display_ptr().cast::<std::ffi::c_void>())
            .ok_or_else(|| anyhow!("null wl_display pointer"))?;
        let wl_surface = NonNull::new(layer.wl_surface().id().as_ptr().cast::<std::ffi::c_void>())
            .ok_or_else(|| anyhow!("null wl_surface pointer"))?;
        let rdh = RawDisplayHandle::Wayland(WaylandDisplayHandle::new(display));
        let rwh = RawWindowHandle::Wayland(WaylandWindowHandle::new(wl_surface));
        // SAFETY: `conn` and `layer` (held by the caller's `Splash`
        // state) outlive this `Gpu` and the returned `Surface`, so the
        // display/surface pointers stay valid for the surface's whole
        // lifetime. The handles are constructed from live, non-null
        // libwayland objects.
        let surface = unsafe {
            instance.create_surface_unsafe(wgpu::SurfaceTargetUnsafe::RawHandle {
                raw_display_handle: rdh,
                raw_window_handle: rwh,
            })?
        };

        let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
            power_preference: wgpu::PowerPreference::LowPower,
            force_fallback_adapter: false,
            compatible_surface: Some(&surface),
        }))
        .ok_or_else(|| anyhow!("no wgpu GL adapter (is Mesa/llvmpipe available?)"))?;

        let (device, queue) = pollster::block_on(adapter.request_device(
            &wgpu::DeviceDescriptor {
                label: Some("halmasuit-splash"),
                required_features: wgpu::Features::empty(),
                required_limits: wgpu::Limits::downlevel_defaults(),
                memory_hints: wgpu::MemoryHints::default(),
            },
            None,
        ))
        .context("wgpu request_device")?;

        let caps = surface.get_capabilities(&adapter);
        let format = caps
            .formats
            .iter()
            .copied()
            .find(wgpu::TextureFormat::is_srgb)
            .unwrap_or(caps.formats[0]);
        let config = wgpu::SurfaceConfiguration {
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            format,
            width: w,
            height: h,
            present_mode: wgpu::PresentMode::Fifo,
            desired_maximum_frame_latency: 2,
            alpha_mode: caps.alpha_modes[0],
            view_formats: vec![],
        };
        surface.configure(&device, &config);

        let bind_group = build_texture_bind_group(&device, &queue, img);
        let pipeline = build_pipeline(&device, format, &bind_group_layout(&device));

        Ok(Self {
            device,
            queue,
            surface,
            config,
            pipeline,
            bind_group,
        })
    }

    fn resize(&mut self, w: u32, h: u32) {
        if w == 0 || h == 0 || (w == self.config.width && h == self.config.height) {
            return;
        }
        self.config.width = w;
        self.config.height = h;
        self.surface.configure(&self.device, &self.config);
    }

    fn render(&self) -> Result<()> {
        let frame = self
            .surface
            .get_current_texture()
            .context("wgpu get_current_texture")?;
        let view = frame
            .texture
            .create_view(&wgpu::TextureViewDescriptor::default());
        let mut encoder = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("halmasuit-splash"),
            });
        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("halmasuit-splash"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &view,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color::BLACK),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
            });
            pass.set_pipeline(&self.pipeline);
            pass.set_bind_group(0, &self.bind_group, &[]);
            pass.draw(0..3, 0..1);
        }
        self.queue.submit([encoder.finish()]);
        frame.present();
        Ok(())
    }
}

fn bind_group_layout(device: &wgpu::Device) -> wgpu::BindGroupLayout {
    device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
        label: Some("halmasuit-splash"),
        entries: &[
            wgpu::BindGroupLayoutEntry {
                binding: 0,
                visibility: wgpu::ShaderStages::FRAGMENT,
                ty: wgpu::BindingType::Texture {
                    sample_type: wgpu::TextureSampleType::Float { filterable: true },
                    view_dimension: wgpu::TextureViewDimension::D2,
                    multisampled: false,
                },
                count: None,
            },
            wgpu::BindGroupLayoutEntry {
                binding: 1,
                visibility: wgpu::ShaderStages::FRAGMENT,
                ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                count: None,
            },
        ],
    })
}

fn build_texture_bind_group(
    device: &wgpu::Device,
    queue: &wgpu::Queue,
    img: &SplashImage,
) -> wgpu::BindGroup {
    let size = wgpu::Extent3d {
        width: img.width,
        height: img.height,
        depth_or_array_layers: 1,
    };
    let texture = device.create_texture(&wgpu::TextureDescriptor {
        label: Some("halmasuit-splash"),
        size,
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: wgpu::TextureFormat::Rgba8UnormSrgb,
        usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
        view_formats: &[],
    });
    queue.write_texture(
        wgpu::ImageCopyTexture {
            texture: &texture,
            mip_level: 0,
            origin: wgpu::Origin3d::ZERO,
            aspect: wgpu::TextureAspect::All,
        },
        &img.rgba,
        wgpu::ImageDataLayout {
            offset: 0,
            bytes_per_row: Some(4 * img.width),
            rows_per_image: Some(img.height),
        },
        size,
    );
    let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
    let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
        label: Some("halmasuit-splash"),
        mag_filter: wgpu::FilterMode::Linear,
        min_filter: wgpu::FilterMode::Linear,
        ..Default::default()
    });
    device.create_bind_group(&wgpu::BindGroupDescriptor {
        label: Some("halmasuit-splash"),
        layout: &bind_group_layout(device),
        entries: &[
            wgpu::BindGroupEntry {
                binding: 0,
                resource: wgpu::BindingResource::TextureView(&view),
            },
            wgpu::BindGroupEntry {
                binding: 1,
                resource: wgpu::BindingResource::Sampler(&sampler),
            },
        ],
    })
}

fn build_pipeline(
    device: &wgpu::Device,
    format: wgpu::TextureFormat,
    bgl: &wgpu::BindGroupLayout,
) -> wgpu::RenderPipeline {
    let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
        label: Some("halmasuit-splash"),
        source: wgpu::ShaderSource::Wgsl(include_str!("shader.wgsl").into()),
    });
    let layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
        label: Some("halmasuit-splash"),
        bind_group_layouts: &[bgl],
        push_constant_ranges: &[],
    });
    device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
        label: Some("halmasuit-splash"),
        layout: Some(&layout),
        vertex: wgpu::VertexState {
            module: &shader,
            entry_point: "vs_main",
            buffers: &[],
            compilation_options: wgpu::PipelineCompilationOptions::default(),
        },
        fragment: Some(wgpu::FragmentState {
            module: &shader,
            entry_point: "fs_main",
            targets: &[Some(wgpu::ColorTargetState {
                format,
                blend: Some(wgpu::BlendState::REPLACE),
                write_mask: wgpu::ColorWrites::ALL,
            })],
            compilation_options: wgpu::PipelineCompilationOptions::default(),
        }),
        primitive: wgpu::PrimitiveState::default(),
        depth_stencil: None,
        multisample: wgpu::MultisampleState::default(),
        multiview: None,
        cache: None,
    })
}

struct Splash {
    registry_state: RegistryState,
    output_state: OutputState,
    // Held for the surface's lifetime / pointer validity.
    _compositor_state: CompositorState,
    _layer_shell: LayerShell,
    layer: LayerSurface,
    conn: Connection,
    image: SplashImage,
    gpu: Option<Gpu>,
}

impl LayerShellHandler for Splash {
    fn closed(&mut self, _c: &Connection, _qh: &QueueHandle<Self>, _l: &LayerSurface) {
        std::process::exit(0);
    }

    fn configure(
        &mut self,
        _c: &Connection,
        _qh: &QueueHandle<Self>,
        _l: &LayerSurface,
        configure: LayerSurfaceConfigure,
        _serial: u32,
    ) {
        let (mut w, mut h) = configure.new_size;
        if w == 0 || h == 0 {
            // halmasuit sends the output mode size; (0,0) only if it
            // defers — fall back to the VM's virtio-gpu-pci default.
            w = 1280;
            h = 800;
        }
        if self.gpu.is_none() {
            match Gpu::init(&self.conn, &self.layer, &self.image, w, h) {
                Ok(gpu) => self.gpu = Some(gpu),
                Err(e) => {
                    eprintln!("halmasuit-splash: wgpu init failed: {e:#}");
                    std::process::exit(1);
                }
            }
        } else if let Some(gpu) = self.gpu.as_mut() {
            gpu.resize(w, h);
        }
        if let Some(gpu) = self.gpu.as_ref() {
            if let Err(e) = gpu.render() {
                eprintln!("halmasuit-splash: render failed: {e:#}");
                std::process::exit(1);
            }
            eprintln!("halmasuit-splash: presented {w}x{h}");
        }
    }
}

impl CompositorHandler for Splash {
    fn scale_factor_changed(
        &mut self,
        _c: &Connection,
        _q: &QueueHandle<Self>,
        _s: &wl_surface::WlSurface,
        _f: i32,
    ) {
    }
    fn transform_changed(
        &mut self,
        _c: &Connection,
        _q: &QueueHandle<Self>,
        _s: &wl_surface::WlSurface,
        _t: wl_output::Transform,
    ) {
    }
    fn frame(
        &mut self,
        _c: &Connection,
        _q: &QueueHandle<Self>,
        _s: &wl_surface::WlSurface,
        _t: u32,
    ) {
    }
    fn surface_enter(
        &mut self,
        _c: &Connection,
        _q: &QueueHandle<Self>,
        _s: &wl_surface::WlSurface,
        _o: &wl_output::WlOutput,
    ) {
    }
    fn surface_leave(
        &mut self,
        _c: &Connection,
        _q: &QueueHandle<Self>,
        _s: &wl_surface::WlSurface,
        _o: &wl_output::WlOutput,
    ) {
    }
}

impl OutputHandler for Splash {
    fn output_state(&mut self) -> &mut OutputState {
        &mut self.output_state
    }
    fn new_output(&mut self, _c: &Connection, _q: &QueueHandle<Self>, _o: wl_output::WlOutput) {}
    fn update_output(&mut self, _c: &Connection, _q: &QueueHandle<Self>, _o: wl_output::WlOutput) {}
    fn output_destroyed(
        &mut self,
        _c: &Connection,
        _q: &QueueHandle<Self>,
        _o: wl_output::WlOutput,
    ) {
    }
}

impl ProvidesRegistryState for Splash {
    fn registry(&mut self) -> &mut RegistryState {
        &mut self.registry_state
    }
    registry_handlers![OutputState];
}

delegate_compositor!(Splash);
delegate_output!(Splash);
delegate_layer!(Splash);
delegate_registry!(Splash);

fn main() -> Result<()> {
    let image = load_splash_image()?;

    let conn = Connection::connect_to_env().context("connect to wayland")?;
    let (globals, mut event_queue) = registry_queue_init(&conn)?;
    let qh = event_queue.handle();

    let compositor_state = CompositorState::bind(&globals, &qh)?;
    let layer_shell = LayerShell::bind(&globals, &qh)?;

    let surface = compositor_state.create_surface(&qh);
    let layer = layer_shell.create_layer_surface(
        &qh,
        surface,
        Layer::Background,
        Some("halmasuit-splash"),
        None, // any output
    );
    layer.set_anchor(Anchor::all());
    layer.set_keyboard_interactivity(KeyboardInteractivity::None);
    layer.set_exclusive_zone(0);
    layer.commit();

    let mut state = Splash {
        registry_state: RegistryState::new(&globals),
        output_state: OutputState::new(&globals, &qh),
        _compositor_state: compositor_state,
        _layer_shell: layer_shell,
        layer,
        conn,
        image,
        gpu: None,
    };

    eprintln!("halmasuit-splash: bound, waiting for configure");
    loop {
        event_queue.blocking_dispatch(&mut state)?;
    }
}
