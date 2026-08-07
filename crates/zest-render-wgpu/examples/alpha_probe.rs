//! Transparency capability probe.
//!
//! # Why this exists
//!
//! Whether the surface can accept **premultiplied per-pixel alpha** decides
//! whether every shader in this crate premultiplies, and that is not something
//! you want to discover after the pipelines are written.
//!
//! On Windows the answer is genuinely uncertain up front:
//! `IDXGIFactory2::CreateSwapChainForHwnd` only permits `DXGI_ALPHA_MODE_IGNORE`.
//! Per-pixel alpha to DWM normally requires `CreateSwapChainForComposition`
//! bound to a DirectComposition visual. But winit's `with_transparent(true)`
//! calls `DwmEnableBlurBehindWindow` with an empty blur region, which makes DWM
//! honor per-pixel alpha for some presentation paths. Vulkan on Windows almost
//! always reports only `Opaque`.
//!
//! So: enumerate, don't assume.
//!
//! Run with `cargo run -p zest-render-wgpu --example alpha_probe`.
//! Add `--` `--visual` to keep a window open showing a half-transparent
//! checkerboard, so the result can be confirmed by eye rather than by API
//! report alone -- an adapter can advertise `PreMultiplied` and still composite
//! wrong.

use std::sync::Arc;

use wgpu::CompositeAlphaMode;
use winit::application::ApplicationHandler;
use winit::event::WindowEvent;
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop};
use winit::window::{Window, WindowId};

/// What the probe concluded, mapped onto the fallback ladder in the plan.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Path {
    /// Native premultiplied alpha. Nothing extra to build.
    A,
    /// Only post-multiplied. Usable, but we must NOT premultiply in-shader;
    /// the compositor divides. Worth knowing, rarely what we want.
    APostMultiplied,
    /// Opaque only. Per-pixel alpha needs a DirectComposition presenter.
    B,
}

struct Probe {
    window: Option<Arc<Window>>,
    visual: bool,
    reported: bool,
}

impl ApplicationHandler for Probe {
    fn resumed(&mut self, el: &ActiveEventLoop) {
        if self.window.is_some() {
            return;
        }
        let attrs = Window::default_attributes()
            .with_title("zesterm alpha probe")
            .with_transparent(true)
            .with_decorations(!self.visual)
            .with_inner_size(winit::dpi::LogicalSize::new(560.0, 360.0));

        let window = Arc::new(el.create_window(attrs).expect("create window"));

        #[cfg(windows)]
        try_mica(&window);

        self.window = Some(window);
    }

    fn window_event(&mut self, el: &ActiveEventLoop, _id: WindowId, event: WindowEvent) {
        match event {
            WindowEvent::CloseRequested => el.exit(),
            WindowEvent::RedrawRequested => {
                if !self.reported {
                    self.reported = true;
                    let window = self.window.clone().expect("window exists by redraw");
                    let path = pollster::block_on(probe(window, self.visual));
                    report(path);
                    if !self.visual {
                        el.exit();
                    }
                }
            }
            _ => {}
        }
    }
}

/// Ask every available backend what it supports, then configure the preferred one.
async fn probe(window: Arc<Window>, visual: bool) -> Path {
    // Enumerate ALL backends, not just the default, so the report says what the
    // machine can do rather than what wgpu happened to pick.
    let instance = wgpu::Instance::new(wgpu::InstanceDescriptor {
        backends: wgpu::Backends::all(),
        ..wgpu::InstanceDescriptor::new_without_display_handle()
    });

    let surface = instance.create_surface(window.clone()).expect("create surface");

    println!("\n=== adapters ===");
    let mut best: Option<(wgpu::Adapter, Vec<CompositeAlphaMode>)> = None;

    for adapter in instance.enumerate_adapters(wgpu::Backends::all()).await {
        let info = adapter.get_info();
        if !adapter.is_surface_supported(&surface) {
            println!("  {:?} / {:<28} -- surface not supported", info.backend, info.name);
            continue;
        }
        let caps = surface.get_capabilities(&adapter);
        println!(
            "  {:?} / {:<28} alpha_modes={:?}",
            info.backend, info.name, caps.alpha_modes
        );
        println!("      formats={:?}", &caps.formats);
        println!("      present_modes={:?}", &caps.present_modes);

        let supports_premul = caps.alpha_modes.contains(&CompositeAlphaMode::PreMultiplied);
        let better = match &best {
            None => true,
            Some((_, modes)) => {
                supports_premul && !modes.contains(&CompositeAlphaMode::PreMultiplied)
            }
        };
        if better {
            best = Some((adapter, caps.alpha_modes.clone()));
        }
    }

    let Some((adapter, alpha_modes)) = best else {
        println!("\nNo adapter supports this surface at all.");
        return Path::B;
    };

    let info = adapter.get_info();
    println!("\nchosen: {:?} / {}", info.backend, info.name);

    let path = if alpha_modes.contains(&CompositeAlphaMode::PreMultiplied) {
        Path::A
    } else if alpha_modes.contains(&CompositeAlphaMode::PostMultiplied) {
        Path::APostMultiplied
    } else {
        Path::B
    };

    if visual {
        paint(&adapter, &surface, &window, path).await;
    }

    path
}

/// Draw a half-transparent checkerboard so the result can be judged by eye.
///
/// The API report is necessary but not sufficient -- an adapter can advertise
/// `PreMultiplied` and still composite incorrectly, and only looking at it
/// tells you that.
async fn paint(
    adapter: &wgpu::Adapter,
    surface: &wgpu::Surface<'static>,
    window: &Arc<Window>,
    path: Path,
) {
    let (device, queue) = adapter
        .request_device(&wgpu::DeviceDescriptor {
            label: Some("alpha probe"),
            required_features: wgpu::Features::empty(),
            required_limits: wgpu::Limits::downlevel_defaults(),
            ..Default::default()
        })
        .await
        .expect("request device");

    let caps = surface.get_capabilities(adapter);
    let alpha_mode = match path {
        Path::A => CompositeAlphaMode::PreMultiplied,
        Path::APostMultiplied => CompositeAlphaMode::PostMultiplied,
        Path::B => CompositeAlphaMode::Opaque,
    };
    // Prefer an sRGB format: the renderer blends in linear and lets the
    // hardware encode, which is the same discipline the real pipeline uses.
    let format = caps
        .formats
        .iter()
        .copied()
        .find(|f| f.is_srgb())
        .unwrap_or(caps.formats[0]);

    let size = window.inner_size();
    surface.configure(
        &device,
        &wgpu::SurfaceConfiguration {
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            format,
            // `Auto` resolves to Srgb for an sRGB format, which is what we
            // want: blend linear, let the hardware encode.
            color_space: wgpu::SurfaceColorSpace::Auto,
            width: size.width.max(1),
            height: size.height.max(1),
            present_mode: wgpu::PresentMode::Fifo,
            alpha_mode,
            view_formats: vec![],
            desired_maximum_frame_latency: 2,
        },
    );
    println!("configured with alpha_mode={alpha_mode:?}, format={format:?}");

    let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
        label: Some("checker"),
        source: wgpu::ShaderSource::Wgsl(include_str!("alpha_probe.wgsl").into()),
    });
    let layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
        label: None,
        bind_group_layouts: &[],
        immediate_size: 0,
    });
    let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
        label: Some("checker"),
        layout: Some(&layout),
        vertex: wgpu::VertexState {
            module: &shader,
            entry_point: Some("vs"),
            buffers: &[],
            compilation_options: Default::default(),
        },
        fragment: Some(wgpu::FragmentState {
            module: &shader,
            entry_point: Some("fs"),
            targets: &[Some(wgpu::ColorTargetState {
                format,
                // Premultiplied source over destination -- the blend every
                // pipeline in this crate will use.
                blend: Some(wgpu::BlendState {
                    color: wgpu::BlendComponent {
                        src_factor: wgpu::BlendFactor::One,
                        dst_factor: wgpu::BlendFactor::OneMinusSrcAlpha,
                        operation: wgpu::BlendOperation::Add,
                    },
                    alpha: wgpu::BlendComponent {
                        src_factor: wgpu::BlendFactor::One,
                        dst_factor: wgpu::BlendFactor::OneMinusSrcAlpha,
                        operation: wgpu::BlendOperation::Add,
                    },
                }),
                write_mask: wgpu::ColorWrites::ALL,
            })],
            compilation_options: Default::default(),
        }),
        primitive: Default::default(),
        depth_stencil: None,
        multisample: Default::default(),
        multiview_mask: None,
        cache: None,
    });

    let frame = match surface.get_current_texture() {
        wgpu::CurrentSurfaceTexture::Success(t) | wgpu::CurrentSurfaceTexture::Suboptimal(t) => t,
        other => {
            println!("could not acquire a surface texture: {other:?}");
            return;
        }
    };
    let view = frame.texture.create_view(&Default::default());
    let mut enc = device.create_command_encoder(&Default::default());
    {
        let mut pass = enc.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("checker"),
            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                view: &view,
                resolve_target: None,
                ops: wgpu::Operations {
                    // Fully transparent clear. If the window shows the desktop
                    // through the cleared region, per-pixel alpha works.
                    load: wgpu::LoadOp::Clear(wgpu::Color::TRANSPARENT),
                    store: wgpu::StoreOp::Store,
                },
                depth_slice: None,
            })],
            ..Default::default()
        });
        pass.set_pipeline(&pipeline);
        pass.draw(0..3, 0..1);
    }
    queue.submit([enc.finish()]);
    queue.present(frame);

    println!(
        "\nLook at the window. Expect: hard-edged opaque squares, translucent\n\
         squares showing the desktop, and fully clear gaps between them.\n\
         If the 'clear' regions are black, per-pixel alpha is NOT working\n\
         regardless of what alpha_modes reported."
    );
}

/// Ask DWM for a Mica backdrop and rounded corners.
///
/// Both are Windows 11 22H2+; on older builds `DwmSetWindowAttribute` returns
/// an error we deliberately ignore, since the whole point is to find out.
#[cfg(windows)]
fn try_mica(window: &Window) {
    use raw_window_handle::{HasWindowHandle, RawWindowHandle};
    use windows_sys::Win32::Graphics::Dwm::DwmSetWindowAttribute;

    const DWMWA_WINDOW_CORNER_PREFERENCE: u32 = 33;
    const DWMWA_SYSTEMBACKDROP_TYPE: u32 = 38;
    const DWMWCP_ROUND: i32 = 2;
    const DWMSBT_MAINWINDOW: i32 = 2; // Mica

    let Ok(handle) = window.window_handle() else { return };
    let RawWindowHandle::Win32(win32) = handle.as_raw() else { return };
    let hwnd = win32.hwnd.get() as *mut core::ffi::c_void;

    unsafe {
        let backdrop = DWMSBT_MAINWINDOW;
        let r1 = DwmSetWindowAttribute(
            hwnd,
            DWMWA_SYSTEMBACKDROP_TYPE,
            std::ptr::addr_of!(backdrop).cast(),
            size_of::<i32>() as u32,
        );
        let corner = DWMWCP_ROUND;
        let r2 = DwmSetWindowAttribute(
            hwnd,
            DWMWA_WINDOW_CORNER_PREFERENCE,
            std::ptr::addr_of!(corner).cast(),
            size_of::<i32>() as u32,
        );
        println!("DWM: backdrop hr=0x{r1:08x}  corner hr=0x{r2:08x}  (0 = ok)");
    }
}

fn report(path: Path) {
    println!("\n=== VERDICT ===");
    match path {
        Path::A => println!(
            "PATH A -- native PreMultiplied alpha.\n\
             Configure the surface with CompositeAlphaMode::PreMultiplied and\n\
             premultiply in every shader. No DirectComposition presenter needed."
        ),
        Path::APostMultiplied => println!(
            "PATH A' -- only PostMultiplied available.\n\
             Usable, but the compositor divides by alpha, so shaders must NOT\n\
             premultiply their output. Decide deliberately: mixing the two\n\
             conventions is how dark halos around text appear."
        ),
        Path::B => println!(
            "PATH B -- Opaque only. Per-pixel alpha needs a manually-managed\n\
             DirectComposition swapchain (DCompositionCreateDevice ->\n\
             IDCompositionTarget -> CreateSwapChainForComposition with\n\
             DXGI_ALPHA_MODE_PREMULTIPLIED), reaching the D3D12 device via\n\
             wgpu's as_hal. Budget ~2 days and isolate it behind a Presenter\n\
             trait in zest-app/src/platform/windows/present.rs."
        ),
    }
}

fn main() {
    let visual = std::env::args().any(|a| a == "--visual");
    let el = EventLoop::new().expect("event loop");
    el.set_control_flow(ControlFlow::Wait);
    let mut probe = Probe { window: None, visual, reported: false };
    el.run_app(&mut probe).expect("run");
}
