//! vlt 窗口入口：winit 窗口 + wgpu surface + 真实 shell PTY。
//!
//! - 损伤驱动重绘：仅在 Grid 变更/窗口事件/光标闪烁沿时请求重绘，
//!   空闲时 `ControlFlow::Wait` 挂起，不跑帧循环（CPU/GPU 空闲 ≈ 0，P2-8）。
//! - 键盘输入经 `keymap` 编码后写入 PTY。
//! - 渲染复用 headless 同一个 `Renderer`，保证窗口所见即截图所测。
//!
//! Phase 2 已接入：角色制字体（config）/ 窗口内边距 / 鼠标选区 + 复制粘贴 /
//! 滚动缓冲翻页 / 字号热调整 / IME 事件预留（不绘制）。

use std::sync::Arc;
use std::time::{Duration, Instant};

use winit::application::ApplicationHandler;
use winit::event::{ElementState, Ime, MouseButton, MouseScrollDelta, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop};
use winit::keyboard::{Key, ModifiersState, NamedKey};
use winit::window::{Window, WindowId};

use alacritty_terminal::grid::Scroll;
use alacritty_terminal::index::{Column, Point, Side};
use alacritty_terminal::selection::{Selection, SelectionType};
use alacritty_terminal::term::viewport_to_point;

use vlt::config::Config as VltConfig;
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

/// 光标闪烁半周期（≤2Hz，即整周期 ≥500ms；这里 530ms 半周期 ≈0.94Hz，P2-8）。
const CURSOR_BLINK_INTERVAL: Duration = Duration::from_millis(530);

/// 双击判定的时间窗口。
const MULTI_CLICK_INTERVAL: Duration = Duration::from_millis(350);

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
    cfg: VltConfig,
    palette: Palette,
    /// 当前逻辑字号（px @ DPR=1），字号热调整会改它。
    font_size: f32,
    /// 当前窗口 DPR。
    scale_factor: f32,
    mods: ModifiersState,
    proxy: winit::event_loop::EventLoopProxy<Wakeup>,

    // ---- 光标闪烁（P2-8：失焦停闪，≤2Hz）----
    focused: bool,
    cursor_visible_phase: bool,
    last_blink: Instant,

    // ---- 鼠标选区（P2-2）----
    /// 最近一次鼠标物理像素位置。
    mouse_pos: (f64, f64),
    /// 是否正在拖拽选区。
    selecting: bool,
    /// 连击计数与时间戳（用于双击选词/三击选行）。
    click_count: u32,
    last_click: Option<Instant>,
    last_click_cell: Option<(usize, usize)>,
}

impl App {
    fn new(proxy: winit::event_loop::EventLoopProxy<Wakeup>, cfg: VltConfig) -> Self {
        let palette = cfg.palette.clone();
        let font_size = cfg.font_size;
        App {
            gpu: None,
            window: None,
            surface: None,
            surface_config: None,
            renderer: None,
            font: None,
            terminal: None,
            cfg,
            palette,
            font_size,
            scale_factor: 1.0,
            mods: ModifiersState::empty(),
            proxy,
            focused: true,
            cursor_visible_phase: true,
            last_blink: Instant::now(),
            mouse_pos: (0.0, 0.0),
            selecting: false,
            click_count: 0,
            last_click: None,
            last_click_cell: None,
        }
    }

    /// 窗口内边距（物理像素）= 逻辑 padding × DPR。
    fn padding_px(&self) -> (u32, u32) {
        let (px, py) = self.cfg.padding;
        (
            (px as f32 * self.scale_factor).round() as u32,
            (py as f32 * self.scale_factor).round() as u32,
        )
    }

    /// 由物理像素尺寸计算网格行列（扣除内边距后按 cell 取整，保证 1:1 对齐）。
    fn grid_size(&self, phys_w: u32, phys_h: u32) -> TermSize {
        let font = self.font.as_ref().unwrap();
        let (ox, oy) = self.padding_px();
        let usable_w = phys_w.saturating_sub(ox * 2);
        let usable_h = phys_h.saturating_sub(oy * 2);
        let cols = (usable_w / font.metrics.width).max(1) as usize;
        let lines = (usable_h / font.metrics.height).max(1) as usize;
        TermSize {
            columns: cols,
            screen_lines: lines,
        }
    }

    fn cell_px(&self) -> (u16, u16) {
        let m = self.font.as_ref().unwrap().metrics;
        (m.width as u16, m.height as u16)
    }

    /// 把鼠标物理像素坐标映射到可见网格 (col, line)，并 clamp 到边界内。
    fn mouse_to_cell(&self, phys_x: f64, phys_y: f64) -> (usize, usize) {
        let font = self.font.as_ref().unwrap();
        let (ox, oy) = self.padding_px();
        let cw = font.metrics.width as f64;
        let ch = font.metrics.height as f64;
        let term = self.terminal.as_ref().unwrap();
        let (cols, lines) = {
            let t = term.term.lock();
            use alacritty_terminal::grid::Dimensions;
            (t.columns(), t.screen_lines())
        };
        let rel_x = (phys_x - ox as f64).max(0.0);
        let rel_y = (phys_y - oy as f64).max(0.0);
        let col = ((rel_x / cw) as usize).min(cols.saturating_sub(1));
        let line = ((rel_y / ch) as usize).min(lines.saturating_sub(1));
        (col, line)
    }

    /// 鼠标在单元格的哪一侧（左/右半格），用于选区锚点精细化。
    fn mouse_side(&self, phys_x: f64) -> Side {
        let font = self.font.as_ref().unwrap();
        let (ox, _) = self.padding_px();
        let cw = font.metrics.width as f64;
        let rel_x = (phys_x - ox as f64).max(0.0);
        if (rel_x % cw) < cw / 2.0 {
            Side::Left
        } else {
            Side::Right
        }
    }

    /// 把可见网格坐标转成 alacritty 的 grid Point（考虑回滚偏移）。
    fn viewport_point(&self, col: usize, line: usize) -> Point {
        let term = self.terminal.as_ref().unwrap();
        let display_offset = term.term.lock().grid().display_offset();
        viewport_to_point(display_offset, Point::new(line, Column(col)))
    }

    /// 请求重绘（内容/交互变更时调用）。
    fn wake(&self) {
        if let Some(w) = self.window.as_ref() {
            w.request_redraw();
        }
    }

    fn redraw(&mut self) {
        let (ox, oy) = self.padding_px();
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

        // 从 Term 抓快照并渲染。光标闪烁：非可见相位时隐藏光标（在 snapshot 后覆盖）。
        let mut snap = {
            let term = terminal.term.lock();
            GridSnapshot::capture(&term, &self.palette)
        };
        if !self.cursor_visible_phase || !self.focused {
            snap.cursor.visible = false;
        }

        renderer.render(
            &gpu.device,
            &gpu.queue,
            &view,
            &snap,
            font,
            (config.width, config.height),
            (ox, oy),
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

        // 启用 IME（P2-6 预留：仅路由事件到空 handler，不绘制候选框）。
        window.set_ime_allowed(true);

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
            present_mode: wgpu::PresentMode::Fifo, // vsync，损伤驱动下不空转（P2-8）
            desired_maximum_frame_latency: 2,
            alpha_mode: caps.alpha_modes[0],
            view_formats: vec![],
        };
        surface.configure(&gpu.device, &config);

        // 字体引擎：物理 ppem = 逻辑字号 × DPR；角色制从 config 装配。
        let scale = window.scale_factor() as f32;
        self.scale_factor = scale;
        let font = FontEngine::from_spec_tuned(
            self.font_size * scale,
            &self.cfg.font,
            self.cfg.letter_spacing * scale, // 逻辑像素 → 物理像素
            self.cfg.line_height,
            self.cfg.tuning,
        );
        log::info!("已加载字体角色链：{:?}", font.loaded_families());

        // 用选中的 surface 格式重建渲染器（其 pipeline 需匹配该格式）。
        let renderer = Renderer::new_with_format(&gpu.device, &font, format);

        // 计算初始网格（扣内边距）并 spawn shell。
        self.font = Some(font);
        let size = self.grid_size(width, height);
        let cell_px = self.cell_px();
        let terminal =
            Terminal::spawn(size, cell_px, self.cfg.scrolling_history).expect("spawn PTY 失败");

        // 让 PTY 线程的内容变更唤醒 winit（损伤驱动）。
        let proxy = self.proxy.clone();
        terminal.proxy.set_waker(Box::new(move || {
            let _ = proxy.send_event(Wakeup);
        }));

        println!("{}", gpu.describe());
        let m = self.font.as_ref().unwrap().metrics;
        println!(
            "窗口 {}x{} 物理像素 | 网格 {}x{} | cell {}x{}px | padding {:?} | DPR {}",
            width, height, size.columns, size.screen_lines, m.width, m.height, self.padding_px(), scale
        );

        self.gpu = Some(gpu);
        self.surface = Some(surface);
        self.surface_config = Some(config);
        self.renderer = Some(renderer);
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
        self.wake();
    }

    fn window_event(
        &mut self,
        event_loop: &ActiveEventLoop,
        _id: WindowId,
        event: WindowEvent,
    ) {
        match event {
            WindowEvent::CloseRequested => event_loop.exit(),

            WindowEvent::Focused(f) => {
                self.focused = f;
                // 失焦停闪（P2-8）：重置相位为可见，避免复焦时闪烁不同步。
                self.cursor_visible_phase = true;
                self.last_blink = Instant::now();
                self.wake();
            }

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
                self.wake();
            }

            WindowEvent::ScaleFactorChanged { scale_factor, .. } => {
                self.scale_factor = scale_factor as f32;
                self.rebuild_font(event_loop);
            }

            WindowEvent::KeyboardInput { event, .. } => {
                if event.state == ElementState::Pressed {
                    if self.handle_key(&event) {
                        return; // 已被快捷键消费
                    }
                    let bytes = encode_key(&event.logical_key, event.text.as_deref(), self.mods);
                    if !bytes.is_empty() {
                        // 有输入 → 退出回滚查看（回到底部），并清选区。
                        if let Some(t) = self.terminal.as_ref() {
                            {
                                let mut term = t.term.lock();
                                term.scroll_display(Scroll::Bottom);
                                term.selection = None;
                            }
                            t.write(bytes);
                        }
                    }
                }
            }

            WindowEvent::ModifiersChanged(m) => {
                self.mods = m.state();
            }

            // ---- 鼠标选区（P2-2）----
            WindowEvent::CursorMoved { position, .. } => {
                self.mouse_pos = (position.x, position.y);
                if self.selecting {
                    self.update_selection();
                    self.wake();
                }
            }

            WindowEvent::MouseInput { state, button, .. } => {
                self.handle_mouse(state, button);
            }

            // ---- 滚动缓冲（P2-3）----
            WindowEvent::MouseWheel { delta, .. } => {
                let lines = match delta {
                    MouseScrollDelta::LineDelta(_, y) => y.round() as i32,
                    MouseScrollDelta::PixelDelta(p) => {
                        let ch = self.font.as_ref().map(|f| f.metrics.height).unwrap_or(1) as f64;
                        (p.y / ch).round() as i32
                    }
                };
                if lines != 0 {
                    if let Some(t) = self.terminal.as_ref() {
                        // 向上滚（正 delta）看历史 → Scroll::Delta 正数。
                        t.term.lock().scroll_display(Scroll::Delta(lines));
                    }
                    self.wake();
                }
            }

            // ---- IME（P2-6 预留：仅路由，不绘制候选框，不提交）----
            WindowEvent::Ime(ime) => {
                self.handle_ime(ime);
            }

            WindowEvent::RedrawRequested => {
                self.redraw();
            }

            _ => {}
        }
    }

    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        // 光标闪烁：仅在聚焦时按固定半周期翻转相位，用定时唤醒驱动（≤2Hz）。
        // 失焦时不设定时器，事件循环彻底挂起（CPU/GPU 空闲 ≈ 0，P2-8）。
        if self.focused {
            let now = Instant::now();
            let elapsed = now.duration_since(self.last_blink);
            if elapsed >= CURSOR_BLINK_INTERVAL {
                self.cursor_visible_phase = !self.cursor_visible_phase;
                self.last_blink = now;
                self.wake();
                event_loop.set_control_flow(ControlFlow::WaitUntil(now + CURSOR_BLINK_INTERVAL));
            } else {
                event_loop.set_control_flow(ControlFlow::WaitUntil(
                    self.last_blink + CURSOR_BLINK_INTERVAL,
                ));
            }
        } else {
            event_loop.set_control_flow(ControlFlow::Wait);
        }
    }
}

impl App {
    /// 处理快捷键（复制/粘贴/字号热调整/翻页）。返回 true 表示已消费。
    fn handle_key(&mut self, event: &winit::event::KeyEvent) -> bool {
        let ctrl = self.mods.control_key();
        let shift = self.mods.shift_key();

        // Ctrl+Shift+C：复制选区。
        if ctrl && shift {
            if let Key::Character(s) = &event.logical_key {
                match s.as_str() {
                    "c" | "C" => {
                        self.copy_selection();
                        return true;
                    }
                    "v" | "V" => {
                        self.paste_clipboard();
                        return true;
                    }
                    _ => {}
                }
            }
        }

        // 字号热调整（P2-5）：Ctrl+= / Ctrl+- / Ctrl+0。
        if ctrl && !shift {
            if let Key::Character(s) = &event.logical_key {
                match s.as_str() {
                    "=" | "+" => {
                        self.adjust_font_size(1.0);
                        return true;
                    }
                    "-" | "_" => {
                        self.adjust_font_size(-1.0);
                        return true;
                    }
                    "0" => {
                        self.reset_font_size();
                        return true;
                    }
                    _ => {}
                }
            }
        }

        // Shift+PageUp / Shift+PageDown：翻页滚动缓冲（P2-3）。
        if shift {
            if let Key::Named(named) = &event.logical_key {
                match named {
                    NamedKey::PageUp => {
                        if let Some(t) = self.terminal.as_ref() {
                            t.term.lock().scroll_display(Scroll::PageUp);
                        }
                        self.wake();
                        return true;
                    }
                    NamedKey::PageDown => {
                        if let Some(t) = self.terminal.as_ref() {
                            t.term.lock().scroll_display(Scroll::PageDown);
                        }
                        self.wake();
                        return true;
                    }
                    _ => {}
                }
            }
        }

        false
    }

    /// 处理鼠标按下/释放：左键起选、中键粘贴、连击选词/选行。
    fn handle_mouse(&mut self, state: ElementState, button: MouseButton) {
        match (state, button) {
            (ElementState::Pressed, MouseButton::Left) => {
                let (col, line) = self.mouse_to_cell(self.mouse_pos.0, self.mouse_pos.1);
                let side = self.mouse_side(self.mouse_pos.0);

                // 连击计数：同格且在时间窗内则递增。
                let now = Instant::now();
                let same_cell = self.last_click_cell == Some((col, line));
                let in_window = self
                    .last_click
                    .map(|t| now.duration_since(t) < MULTI_CLICK_INTERVAL)
                    .unwrap_or(false);
                if same_cell && in_window {
                    self.click_count = (self.click_count % 3) + 1;
                } else {
                    self.click_count = 1;
                }
                self.last_click = Some(now);
                self.last_click_cell = Some((col, line));

                let ty = match self.click_count {
                    2 => SelectionType::Semantic, // 双击选词
                    3 => SelectionType::Lines,    // 三击选行
                    _ => SelectionType::Simple,   // 单击拖选
                };

                let point = self.viewport_point(col, line);
                if let Some(t) = self.terminal.as_ref() {
                    let sel = Selection::new(ty, point, side);
                    t.term.lock().selection = Some(sel);
                }
                self.selecting = self.click_count == 1;
                self.wake();
            }
            (ElementState::Released, MouseButton::Left) => {
                self.selecting = false;
            }
            (ElementState::Pressed, MouseButton::Middle) => {
                // 中键粘贴主选区（这里简化为粘贴系统剪贴板，X11 惯例可接受）。
                self.paste_clipboard();
            }
            _ => {}
        }
    }

    /// 拖拽中更新选区终点。
    fn update_selection(&mut self) {
        let (col, line) = self.mouse_to_cell(self.mouse_pos.0, self.mouse_pos.1);
        let side = self.mouse_side(self.mouse_pos.0);
        let point = self.viewport_point(col, line);
        if let Some(t) = self.terminal.as_ref() {
            let mut term = t.term.lock();
            if let Some(sel) = term.selection.as_mut() {
                sel.update(point, side);
            }
        }
    }

    /// 复制当前选区到系统剪贴板。
    fn copy_selection(&mut self) {
        let text = self
            .terminal
            .as_ref()
            .and_then(|t| t.term.lock().selection_to_string());
        if let Some(text) = text {
            if text.is_empty() {
                return;
            }
            match arboard::Clipboard::new() {
                Ok(mut cb) => {
                    if let Err(e) = cb.set_text(text) {
                        eprintln!("vlt: 写剪贴板失败：{}", e);
                    }
                }
                Err(e) => eprintln!("vlt: 打开剪贴板失败：{}", e),
            }
        }
    }

    /// 从系统剪贴板粘贴到 PTY（含括号粘贴由 alacritty 处理，这里直接写字节）。
    fn paste_clipboard(&mut self) {
        let text = match arboard::Clipboard::new().and_then(|mut cb| cb.get_text()) {
            Ok(t) => t,
            Err(_) => return,
        };
        if text.is_empty() {
            return;
        }
        if let Some(t) = self.terminal.as_ref() {
            {
                let mut term = t.term.lock();
                term.scroll_display(Scroll::Bottom);
            }
            t.write(text.into_bytes());
        }
    }

    /// 字号热调整：改逻辑字号后整体重建字体引擎与 atlas（铁律 3）。
    fn adjust_font_size(&mut self, delta: f32) {
        let new_size = (self.font_size + delta).clamp(6.0, 72.0);
        if (new_size - self.font_size).abs() < f32::EPSILON {
            return;
        }
        self.font_size = new_size;
        self.rebuild_font_only();
    }

    fn reset_font_size(&mut self) {
        if (self.font_size - self.cfg.font_size).abs() < f32::EPSILON {
            return;
        }
        self.font_size = self.cfg.font_size;
        self.rebuild_font_only();
    }

    /// 用当前 font_size × DPR 重建字体引擎、atlas 纹理，并按新 cell 重算网格与 padding。
    fn rebuild_font_only(&mut self) {
        let (Some(gpu), Some(renderer)) = (self.gpu.as_ref(), self.renderer.as_mut()) else {
            return;
        };
        let font = FontEngine::from_spec_tuned(
            self.font_size * self.scale_factor,
            &self.cfg.font,
            self.cfg.letter_spacing * self.scale_factor,
            self.cfg.line_height,
            self.cfg.tuning,
        );
        renderer.rebuild_atlas_texture(&gpu.device, &font);
        self.font = Some(font);
        if let (Some(config), Some(t)) = (self.surface_config.as_ref(), self.terminal.as_ref()) {
            let ts = self.grid_size(config.width, config.height);
            let cell_px = self.cell_px();
            t.resize(ts, cell_px);
        }
        self.wake();
    }

    /// DPR 变化时重建字体引擎（与字号热调整同路径，区别仅在触发来源）。
    fn rebuild_font(&mut self, _event_loop: &ActiveEventLoop) {
        self.rebuild_font_only();
    }

    /// IME 事件路由（P2-6 预留）：Phase 2 不绘制候选框、不处理 preedit。
    /// Commit 仍直接写入 PTY，保证输入法直接上屏可用（拉丁/中文兜底行为）。
    /// TODO(Phase 2 完整版): preedit 下划线绘制 + set_ime_cursor_area 候选框定位。
    fn handle_ime(&mut self, ime: Ime) {
        match ime {
            Ime::Commit(text) => {
                if !text.is_empty() {
                    if let Some(t) = self.terminal.as_ref() {
                        {
                            let mut term = t.term.lock();
                            term.scroll_display(Scroll::Bottom);
                        }
                        t.write(text.into_bytes());
                    }
                }
            }
            // Enabled / Preedit / Disabled：预留，不绘制（TODO Phase 2 完整版）。
            Ime::Enabled | Ime::Preedit(_, _) | Ime::Disabled => {}
        }
    }
}

fn main() {
    env_logger::init();

    // 加载配置（不存在则生成默认文件；解析失败回退缺省，不 panic）。
    let cfg = vlt::config::load();

    let event_loop = EventLoop::<Wakeup>::with_user_event()
        .build()
        .expect("创建 winit 事件循环失败");
    event_loop.set_control_flow(ControlFlow::Wait);
    let proxy = event_loop.create_proxy();
    let mut app = App::new(proxy, cfg);
    event_loop.run_app(&mut app).expect("事件循环退出异常");
}
