//! Phase 2 存量项 headless 验收出图（T3/T5）。
//!
//! 用 headless 渲染证明选区高亮（P2-2）、回滚指示（P2-3）、干净 shell 帧（T5）
//! 走的是与窗口路径同一个 `Renderer`。比 xdotool 驱动 GUI 更确定、可复现。
//! 产物写入指定目录（默认 scratchpad）。

use std::path::{Path, PathBuf};

use alacritty_terminal::grid::Scroll;
use alacritty_terminal::index::{Column, Line, Point, Side};
use alacritty_terminal::selection::{Selection, SelectionType};

use vlt::config::FontSpec;
use vlt::font::{FontEngine, RasterTuning};
use vlt::gpu::Gpu;
use vlt::headless::{write_png, Headless};
use vlt::render::Renderer;
use vlt::snapshot::GridSnapshot;
use vlt::terminal::{term_from_ansi, TermSize};
use vlt::theme::Palette;

/// 用内嵌字体渲染一个（可被回调修改的）Term 为 PNG。
fn render_term<F: FnOnce(&mut alacritty_terminal::term::Term<vlt::terminal::EventProxy>)>(
    gpu: &Gpu,
    ppem: f32,
    cols: usize,
    lines: usize,
    ansi: &[u8],
    mutate: F,
    out: &Path,
) {
    let mut font = FontEngine::from_spec_tuned(
        ppem,
        &FontSpec::default(),
        0.0,
        1.0,
        RasterTuning::default(),
    );
    let cw = font.metrics.width;
    let ch = font.metrics.height;
    let width = cw * cols as u32;
    let height = ch * lines as u32;

    let palette = Palette::default();
    let size = TermSize {
        columns: cols,
        screen_lines: lines,
    };
    let mut term = term_from_ansi(size, ansi);
    mutate(&mut term);
    let snap = GridSnapshot::capture(&term, &palette);

    let mut renderer = Renderer::new(&gpu.device, &font);
    let headless = Headless::new(&gpu.device, width, height);
    let rgba = headless.render_to_rgba(gpu, &mut renderer, &snap, &mut font, true);
    write_png(out, width, height, &rgba).expect("写 PNG 失败");
    println!("  {} ({}x{})", out.display(), width, height);
}

fn main() {
    env_logger::init();
    let out_dir: PathBuf = std::env::args()
        .nth(1)
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));
    std::fs::create_dir_all(&out_dir).ok();

    let gpu = Gpu::new(None);
    println!("{}", gpu.describe());
    let ppem = 30.0;

    // ---- P2-2 选区高亮：拖选一段文字，验证淡蓝纸感底 + 墨色文字不反白 ----
    println!("== P2-2 选区高亮 ==");
    let sel_text =
        "The quick brown fox jumps\r\nover the lazy dog. 选中这段\r\nlet x = foo->bar(baz);";
    render_term(
        &gpu,
        ppem,
        30,
        4,
        sel_text.as_bytes(),
        |term| {
            // 模拟从 (line1,col0) 拖到 (line1,col16) 的简单选区。
            let start = Point::new(Line(1), Column(0));
            let mut sel = Selection::new(SelectionType::Simple, start, Side::Left);
            sel.update(Point::new(Line(1), Column(16)), Side::Right);
            term.selection = Some(sel);
        },
        &out_dir.join("verify_selection.png"),
    );

    // ---- P2-3 回滚指示：制造滚动缓冲后上滚，验证右上角浅灰指示条 ----
    println!("== P2-3 回滚指示条 ==");
    let mut scroll_ansi = String::new();
    for i in 1..=40 {
        scroll_ansi.push_str(&format!("line {i:02}: 滚动缓冲内容 scrollback row\r\n"));
    }
    render_term(
        &gpu,
        ppem,
        40,
        8,
        scroll_ansi.as_bytes(),
        |term| {
            term.scroll_display(Scroll::Delta(15)); // 上滚 15 行看历史
        },
        &out_dir.join("verify_scrollback.png"),
    );

    // ---- T5 干净 shell 帧：ls --color=always + vim 一帧 ----
    println!("== T5 干净 shell 帧 ==");
    let mut shell = String::new();
    shell.push_str("$ ls --color=always\r\n");
    shell.push_str("\x1b[1;34msrc\x1b[0m  \x1b[1;34massets\x1b[0m  \x1b[1;34mtests\x1b[0m  ");
    shell.push_str("\x1b[0mCargo.toml\x1b[0m  \x1b[0mdesign.md\x1b[0m  \x1b[1;32mcheck_deps.sh\x1b[0m\r\n");
    shell.push_str("$ vim src/font.rs\r\n");
    shell.push_str("\x1b[7m  src/font.rs                              rust  utf-8  12,1  \x1b[0m\r\n");
    shell.push_str("\x1b[38;5;240m  1\x1b[0m \x1b[38;5;170mpub fn\x1b[0m \x1b[38;5;39mrasterize\x1b[0m(ch: \x1b[38;5;39mchar\x1b[0m) {\r\n");
    shell.push_str("\x1b[38;5;240m  2\x1b[0m     \x1b[38;5;107m// 字形按目标字号 CPU 光栅化\x1b[0m\r\n");
    shell.push_str("\x1b[38;5;240m  3\x1b[0m     \x1b[38;5;170mlet\x1b[0m cells = ch.width();\r\n");
    shell.push_str("\x1b[38;5;240m  4\x1b[0m }\r\n");
    shell.push_str("\x1b[7m-- NORMAL --\x1b[0m\r\n");
    render_term(
        &gpu,
        ppem,
        62,
        10,
        shell.as_bytes(),
        |_| {},
        &out_dir.join("verify_shell_frame.png"),
    );

    println!("完成，见 {}", out_dir.display());
}
