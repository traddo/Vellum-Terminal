//! vlt 窗口入口：winit 窗口 + wgpu surface + 真实 shell PTY。
//!
//! - 损伤驱动重绘：仅在 Grid 变更/窗口事件时请求重绘，空闲不跑帧循环（`ControlFlow::Wait`）。
//! - 键盘输入经 `keymap` 编码后写入 PTY。
//! - 渲染复用 headless 同一个 `Renderer`，保证窗口所见即截图所测。
//!
//! IME/选区/滚动缓冲翻页属于 Phase 2，此处不实现（TODO）。

use std::sync::Arc;

use winit::application::ApplicationHandler;
use winit::event::{ElementState, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop};
use winit::keyboard::ModifiersState;
use winit::window::{Window, WindowId};

use vlt::font::FontEngine;
use vlt::gpu::Gpu;
use vlt::keymap::encode_key;
use vlt::render::{Renderer, TARGET_FORMAT};
use vlt::snapshot::GridSnapshot;
use vlt::terminal::{Terminal, TermSize};
use vlt::theme::Palette;

/// 窗口初始逻辑尺寸（DPR=1 时的像素）。
const INIT_W: u32 = 960;
const INIT_H: u32 = 600;

/// user event：仅用于把 PTY 线程的“内容已变更”唤醒送进 winit 事件循环。
#[derive(Debug, Clone, Copy)]
struct Wakeup;

struct App {
    gpu: Option<Gpu>,
    window: Option<Arc<Window>>,
    surface: Option<wgpu::Surface<'static>>,
    surface_config: Option<wgpu::SurfaceConfiguration>,
    renderer: Option<Renderer>,
    font: Option<FontEngine>,
    terminal: Option<Terminal>,
    palette: Palette,
    ppem: f32,
    mods: ModifiersState,
    proxy: winit::event_loop::EventLoopProxy<Wakeup>,
}

impl App {
    fn new(proxy: winit::event_loop::EventLoopProxy<Wakeup>) -> Self {
        App {
            gpu: None,
            window: None,
            surface: None,
            surface_config: None,
            renderer: None,
            font: None,
            terminal: None,
            palette: Palette::default(),
            ppem: vlt::DEFAULT_FONT_SIZE,
            mods: ModifiersState::empty(),
            proxy,
        }
    }

    /// 由物理像素尺寸计算网格行列（cell 尺寸取整，保证 1:1 对齐）。
    fn grid_size(&self, phys_w: u32, phys_h: u32) -> TermSize {
        let font = self.font.as_ref().unwrap();
        let cols = (phys_w / font.metrics.width).max(1) as usize;
        let lines = (phys_h / font.metrics.height).max(1) as usize;
        TermSize {
            columns: cols,
            screen_lines: lines,
        }
    }

    fn cell_px(&self) -> (u16, u16) {
        let m = self.font.as_ref().unwrap().metrics;
        (m.width as u16, m.height as u16)
    }

    fn redraw(&mut self) {
        let (Some(gpu), Some(surface), Some(config), Some(renderer), Some(font), Some(terminal)) = (
            self.gpu.as_ref(),
            self.surface.as_ref(),
            self.surface_config.as_ref(),
            self.renderer.as_mut(),
            self.font.as_mut(),
            self.terminal.as_ref(),
        ) else {
            return;
        };

        // 取当前帧。
        let frame = match surface.get_current_texture() {
            Ok(f) => f,
            Err(wgpu::SurfaceError::Outdated) | Err(wgpu::SurfaceError::Lost) => {
                surface.configure(&gpu.device, config);
                return;
            }
            Err(_) => return,
        };
        let view = frame
            .texture
            .create_view(&wgpu::TextureViewDescriptor::default());

        // 从 Term 抓快照并渲染。
        let snap = {
            let term = terminal.term.lock();
            GridSnapshot::capture(&term, &self.palette)
        };

        renderer.render(
            &gpu.device,
            &gpu.queue,
            &view,
            &snap,
            font,
            (config.width, config.height),
            true, // 窗口路径始终 gamma 校正
        );

        frame.present();
    }
}

impl ApplicationHandler<Wakeup> for App {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.window.is_some() {
            return;
        }

        let attrs = Window::default_attributes()
            .with_title("Vellum Terminal")
            .with_inner_size(winit::dpi::LogicalSize::new(INIT_W, INIT_H));
        let window = Arc::new(event_loop.create_window(attrs).expect("创建窗口失败"));

        // GPU + surface。
        let gpu = Gpu::new(None);
        let surface = gpu
            .instance
            .create_surface(window.clone())
            .expect("创建 surface 失败");

        // 选一个非 sRGB 的 8-bit unorm 格式（shader 自己做 sRGB 编码）。
        let caps = surface.get_capabilities(&gpu.adapter);
        let format = caps
            .formats
            .iter()
            .copied()
            .find(|f| *f == TARGET_FORMAT)
            .or_else(|| {
                caps.formats
                    .iter()
                    .copied()
                    .find(|f| *f == wgpu::TextureFormat::Bgra8Unorm)
            })
            .unwrap_or(caps.formats[0]);

        if format != TARGET_FORMAT && format != wgpu::TextureFormat::Bgra8Unorm {
            eprintln!(
                "警告：surface 无非 sRGB unorm 格式，颜色可能偏差（得到 {:?}）",
                format
            );
        }

        let phys = window.inner_size();
        let width = phys.width.max(1);
        let height = phys.height.max(1);

        let config = wgpu::SurfaceConfiguration {
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            format,
            width,
            height,
            present_mode: wgpu::PresentMode::Fifo, // vsync，损伤驱动下不空转
            desired_maximum_frame_latency: 2,
            alpha_mode: caps.alpha_modes[0],
            view_formats: vec![],
        };
        surface.configure(&gpu.device, &config);

        // 字体引擎：物理 ppem = 逻辑字号 × DPR。
        let scale = window.scale_factor() as f32;
        let font = FontEngine::new(self.ppem * scale);

        // 用选中的 surface 格式重建渲染器（其 pipeline 需匹配该格式）。
        let renderer = Renderer::new_with_format(&gpu.device, &font, format);

        // 计算初始网格并 spawn shell。
        let cols = (width / font.metrics.width).max(1) as usize;
        let lines = (height / font.metrics.height).max(1) as usize;
        let size = TermSize {
            columns: cols,
            screen_lines: lines,
        };
        let cell_px = (font.metrics.width as u16, font.metrics.height as u16);
        let terminal = Terminal::spawn(size, cell_px).expect("spawn PTY 失败");

        // 让 PTY 线程的内容变更唤醒 winit（损伤驱动）。
        let proxy = self.proxy.clone();
        terminal.proxy.set_waker(Box::new(move || {
            let _ = proxy.send_event(Wakeup);
        }));

        println!("{}", gpu.describe());
        println!(
            "窗口 {}x{} 物理像素 | 网格 {}x{} | cell {}x{}px | DPR {}",
            width, height, cols, lines, font.metrics.width, font.metrics.height, scale
        );

        self.gpu = Some(gpu);
        self.surface = Some(surface);
        self.surface_config = Some(config);
        self.renderer = Some(renderer);
        self.font = Some(font);
        self.terminal = Some(terminal);
        self.window = Some(window);
    }

    fn user_event(&mut self, event_loop: &ActiveEventLoop, _event: Wakeup) {
        // 内容变更或子进程退出 → 请求重绘。
        if let Some(t) = self.terminal.as_ref() {
            if t.proxy.has_exited() {
                event_loop.exit();
                return;
            }
        }
        if let Some(w) = self.window.as_ref() {
            w.request_redraw();
        }
    }

    fn window_event(
        &mut self,
        event_loop: &ActiveEventLoop,
        _id: WindowId,
        event: WindowEvent,
    ) {
        match event {
            WindowEvent::CloseRequested => event_loop.exit(),

            WindowEvent::Resized(size) => {
                let (w, h) = (size.width.max(1), size.height.max(1));
                if let (Some(gpu), Some(surface), Some(config)) = (
                    self.gpu.as_ref(),
                    self.surface.as_ref(),
                    self.surface_config.as_mut(),
                ) {
                    config.width = w;
                    config.height = h;
                    surface.configure(&gpu.device, config);
                }
                if self.font.is_some() {
                    let ts = self.grid_size(w, h);
                    let cell_px = self.cell_px();
                    if let Some(t) = self.terminal.as_ref() {
                        t.resize(ts, cell_px);
                    }
                }
                if let Some(win) = self.window.as_ref() {
                    win.request_redraw();
                }
            }

            WindowEvent::ScaleFactorChanged { scale_factor, .. } => {
                // DPR 变化：整体重建字体引擎与 atlas（铁律 3）。
                if let (Some(gpu), Some(renderer)) = (self.gpu.as_ref(), self.renderer.as_mut()) {
                    let font = FontEngine::new(self.ppem * scale_factor as f32);
                    renderer.rebuild_atlas_texture(&gpu.device, &font);
                    self.font = Some(font);
                    if let (Some(config), Some(t)) =
                        (self.surface_config.as_ref(), self.terminal.as_ref())
                    {
                        let ts = self.grid_size(config.width, config.height);
                        let cell_px = self.cell_px();
                        t.resize(ts, cell_px);
                    }
                }
                if let Some(win) = self.window.as_ref() {
                    win.request_redraw();
                }
            }

            WindowEvent::KeyboardInput { event, .. } => {
                if event.state == ElementState::Pressed {
                    let bytes = encode_key(&event.logical_key, event.text.as_deref(), self.mods);
                    if !bytes.is_empty() {
                        if let Some(t) = self.terminal.as_ref() {
                            t.write(bytes);
                        }
                    }
                }
            }

            WindowEvent::ModifiersChanged(m) => {
                self.mods = m.state();
            }

            WindowEvent::RedrawRequested => {
                self.redraw();
            }

            // TODO(Phase 2): Ime、鼠标选区、滚动。
            _ => {}
        }
    }

    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        // 损伤驱动：无事件时挂起等待，不空转。
        event_loop.set_control_flow(ControlFlow::Wait);
    }
}

fn main() {
    env_logger::init();
    let event_loop = EventLoop::<Wakeup>::with_user_event()
        .build()
        .expect("创建 winit 事件循环失败");
    event_loop.set_control_flow(ControlFlow::Wait);
    let proxy = event_loop.create_proxy();
    let mut app = App::new(proxy);
    event_loop.run_app(&mut app).expect("事件循环退出异常");
}
