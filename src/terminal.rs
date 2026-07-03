//! 第 1 层封装：alacritty_terminal 的 Term + PTY + 事件循环包装。
//!
//! 铁律 2：不手写 VT 解析器/PTY，整层复用 alacritty_terminal。
//! 本模块只做「胶水」：spawn PTY、起读写线程、把 Grid 变更通知转给渲染循环。

use std::sync::{Arc, Mutex};

use alacritty_terminal::event::{Event, EventListener, WindowSize};
use alacritty_terminal::event_loop::{EventLoop, EventLoopSender, Msg};
use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::sync::FairMutex;
use alacritty_terminal::term::{Config, Term};
use alacritty_terminal::tty::{self, Options as PtyOptions};
use alacritty_terminal::vte::ansi::{Processor, StdSyncHandler};

/// 实现 `Dimensions` 的尺寸描述，用于 `Term::new` 与 resize。
#[derive(Clone, Copy, Debug)]
pub struct TermSize {
    pub columns: usize,
    pub screen_lines: usize,
}

impl Dimensions for TermSize {
    fn total_lines(&self) -> usize {
        self.screen_lines
    }
    fn screen_lines(&self) -> usize {
        self.screen_lines
    }
    fn columns(&self) -> usize {
        self.columns
    }
}

/// 事件监听器：把 alacritty 的 `Event`（尤其 `Wakeup`）转发出去以触发损伤驱动重绘。
#[derive(Clone)]
pub struct EventProxy {
    /// 是否有内容变更需要重绘。
    dirty: Arc<std::sync::atomic::AtomicBool>,
    /// 子进程是否已退出。
    exited: Arc<std::sync::atomic::AtomicBool>,
    /// winit 事件循环唤醒器（窗口路径用；headless 为 None）。
    waker: Arc<Mutex<Option<Box<dyn Fn() + Send>>>>,
}

impl EventProxy {
    pub fn new() -> Self {
        EventProxy {
            dirty: Arc::new(std::sync::atomic::AtomicBool::new(true)),
            exited: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            waker: Arc::new(Mutex::new(None)),
        }
    }

    /// 设置窗口唤醒回调（winit `EventLoopProxy::wake_up`）。
    pub fn set_waker(&self, f: Box<dyn Fn() + Send>) {
        *self.waker.lock().unwrap() = Some(f);
    }

    /// 取出并清除 dirty 标志。
    pub fn take_dirty(&self) -> bool {
        self.dirty.swap(false, std::sync::atomic::Ordering::SeqCst)
    }

    pub fn mark_dirty(&self) {
        self.dirty.store(true, std::sync::atomic::Ordering::SeqCst);
    }

    pub fn has_exited(&self) -> bool {
        self.exited.load(std::sync::atomic::Ordering::SeqCst)
    }
}

impl Default for EventProxy {
    fn default() -> Self {
        Self::new()
    }
}

impl EventListener for EventProxy {
    fn send_event(&self, event: Event) {
        match event {
            Event::Wakeup | Event::MouseCursorDirty | Event::CursorBlinkingChange => {
                self.dirty.store(true, std::sync::atomic::Ordering::SeqCst);
                if let Some(w) = self.waker.lock().unwrap().as_ref() {
                    w();
                }
            }
            Event::Exit | Event::ChildExit(_) => {
                self.exited.store(true, std::sync::atomic::Ordering::SeqCst);
                if let Some(w) = self.waker.lock().unwrap().as_ref() {
                    w();
                }
            }
            _ => {}
        }
    }
}

/// 运行中的终端：持有 Term 与 PTY 写入通道。
pub struct Terminal {
    pub term: Arc<FairMutex<Term<EventProxy>>>,
    pub proxy: EventProxy,
    sender: EventLoopSender,
    _io_thread: std::thread::JoinHandle<(EventLoop<tty::Pty, EventProxy>, alacritty_terminal::event_loop::State)>,
}

impl Terminal {
    /// spawn 一个真实 shell 的 PTY 并起 IO 线程。
    pub fn spawn(size: TermSize, cell_px: (u16, u16)) -> std::io::Result<Self> {
        let proxy = EventProxy::new();

        let config = Config {
            scrolling_history: 10_000,
            ..Config::default()
        };

        let term = Term::new(config, &size, proxy.clone());
        let term = Arc::new(FairMutex::new(term));

        let window_size = WindowSize {
            num_lines: size.screen_lines as u16,
            num_cols: size.columns as u16,
            cell_width: cell_px.0,
            cell_height: cell_px.1,
        };

        // P2-0：终端能力环境变量。让 shell/程序识别 256 色 + 真彩色能力，
        // 否则 bash 提示符不上色、ls 目录不上蓝。
        // TODO(Phase 3): 评估自带 terminfo；本期借用 xterm-256color。
        let mut env = std::collections::HashMap::new();
        env.insert("TERM".to_string(), "xterm-256color".to_string());
        env.insert("COLORTERM".to_string(), "truecolor".to_string());

        let pty_options = PtyOptions {
            shell: None, // 默认 shell（$SHELL）
            working_directory: None,
            drain_on_exit: true,
            env,
        };

        let pty = tty::new(&pty_options, window_size, 0)?;

        let event_loop = EventLoop::new(term.clone(), proxy.clone(), pty, false, false)?;
        let sender = event_loop.channel();
        let io_thread = event_loop.spawn();

        Ok(Terminal {
            term,
            proxy,
            sender,
            _io_thread: io_thread,
        })
    }

    /// 向 PTY 写入输入字节（键盘输入）。
    pub fn write(&self, bytes: Vec<u8>) {
        let _ = self.sender.send(Msg::Input(std::borrow::Cow::Owned(bytes)));
    }

    /// 通知 PTY 与 Term 尺寸变化。
    pub fn resize(&self, size: TermSize, cell_px: (u16, u16)) {
        self.term.lock().resize(size);
        let window_size = WindowSize {
            num_lines: size.screen_lines as u16,
            num_cols: size.columns as u16,
            cell_width: cell_px.0,
            cell_height: cell_px.1,
        };
        let _ = self.sender.send(Msg::Resize(window_size));
    }
}

impl Drop for Terminal {
    fn drop(&mut self) {
        let _ = self.sender.send(Msg::Shutdown);
    }
}

/// 用于 headless 测试：不 spawn 真实 shell，直接把一段 ANSI 字节流喂进 Term。
///
/// 复用 alacritty 的 VTE `Processor` 驱动同一个 Term 状态机，
/// 故渲染出的帧与真实 shell 输出等价（验收清单 3 的静态帧路线）。
pub fn term_from_ansi(size: TermSize, ansi: &[u8]) -> Term<EventProxy> {
    let proxy = EventProxy::new();
    let config = Config::default();
    let mut term = Term::new(config, &size, proxy);

    let mut processor: Processor<StdSyncHandler> = Processor::new();
    processor.advance(&mut term, ansi);
    term
}
