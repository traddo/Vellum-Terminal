//! headless 截图工具：渲染固定测试内容为 PNG。
//!
//! 用途：逐像素一致性回归基线 + gamma 校正开/关对比 + vim/htop/ls 静态帧验收。
//! 用法：`cargo run --release --bin snapshot`，产物写入 tests/snapshots/。

use std::path::Path;

use vlt::font::FontEngine;
use vlt::gpu::Gpu;
use vlt::headless::{write_png, Headless};
use vlt::render::Renderer;
use vlt::snapshot::GridSnapshot;
use vlt::terminal::{term_from_ansi, TermSize};
use vlt::theme::Palette;

/// 渲染给定 ANSI 流为一帧 PNG。
#[allow(clippy::too_many_arguments)]
fn render_case(
    gpu: &Gpu,
    ppem: f32,
    cols: usize,
    lines: usize,
    ansi: &[u8],
    gamma: bool,
    out: &Path,
) {
    let mut font = FontEngine::new(ppem);
    let cw = font.metrics.width;
    let ch = font.metrics.height;
    let width = cw * cols as u32;
    let height = ch * lines as u32;

    let palette = Palette::default();
    let size = TermSize {
        columns: cols,
        screen_lines: lines,
    };
    let term = term_from_ansi(size, ansi);
    let snap = GridSnapshot::capture(&term, &palette);

    let mut renderer = Renderer::new(&gpu.device, &font);
    let headless = Headless::new(&gpu.device, width, height);
    let rgba = headless.render_to_rgba(gpu, &mut renderer, &snap, &mut font, gamma);
    write_png(out, width, height, &rgba).expect("写 PNG 失败");
    println!("  写出 {} ({}x{})", out.display(), width, height);
}

fn main() {
    env_logger::init();
    let gpu = Gpu::new(None);
    println!("{}", gpu.describe());

    let ppem = 30.0; // 15px @ DPR 2，模拟 HiDPI 物理像素
    let out_dir = Path::new("tests/snapshots");

    // ---- 用例 1：ASCII/Latin 基线 + 真彩色 + ANSI 色 ----
    let mut basic = String::new();
    basic.push_str("Vellum Terminal (vlt) — PDF-style rendering\r\n");
    basic.push_str("The quick brown fox jumps over the lazy dog.\r\n");
    basic.push_str("0123456789 !@#$%^&*()_+-=[]{}|;:,.<>?/`~\r\n");
    basic.push_str("Ligature-ish: -> => != == >= <= |> <| ::\r\n");
    // ANSI 16 色前景。
    for i in 0..8 {
        basic.push_str(&format!("\x1b[3{}mnormal{}\x1b[0m ", i, i));
    }
    basic.push_str("\r\n");
    for i in 0..8 {
        basic.push_str(&format!("\x1b[9{}mbright{}\x1b[0m ", i, i));
    }
    basic.push_str("\r\n");
    // 真彩色渐变。
    basic.push_str("Truecolor: ");
    for k in 0..16 {
        let r = (k * 16) as u8;
        basic.push_str(&format!("\x1b[38;2;{};80;{}m█\x1b[0m", r, 200 - r));
    }
    basic.push_str("\r\n");
    // 样式：粗体/斜体/下划线/反显。
    basic.push_str("\x1b[1mBold\x1b[0m \x1b[3mItalic\x1b[0m \x1b[4mUnderline\x1b[0m \x1b[7mInverse\x1b[0m \x1b[9mStrike\x1b[0m\r\n");

    render_case(&gpu, ppem, 60, 12, basic.as_bytes(), true, &out_dir.join("basic.png"));

    // ---- 用例 2：gamma 校正 对比（同内容，深色文字叠白底）----
    let gamma_text = concat!(
        "Gamma test - ink text on paper-white, stroke weight\r\n",
        "AAAA BBBB CCCC gggg oooo eeee wwww mmmm nnnn\r\n",
        "if (x == 0) { return foo->bar(baz); } // comment\r\n",
        "The quick brown fox jumps over the lazy dog 1234567890\r\n",
        "|||||||||||||||||||||||||||||||||||||||||||||||||||||||\r\n",
    );
    render_case(&gpu, ppem, 56, 6, gamma_text.as_bytes(), true, &out_dir.join("gamma_on.png"));
    render_case(&gpu, ppem, 56, 6, gamma_text.as_bytes(), false, &out_dir.join("gamma_off.png"));

    // ---- 用例 3：ls --color 风格输出 ----
    let mut ls = String::new();
    ls.push_str("$ ls --color\r\n");
    ls.push_str("\x1b[1;34mdir_blue\x1b[0m  ");
    ls.push_str("\x1b[1;32mexecutable\x1b[0m  ");
    ls.push_str("\x1b[1;36msymlink\x1b[0m  ");
    ls.push_str("\x1b[0mregular.txt\x1b[0m\r\n");
    ls.push_str("\x1b[1;31marchive.tar.gz\x1b[0m  ");
    ls.push_str("\x1b[1;35mimage.png\x1b[0m  ");
    ls.push_str("\x1b[33mconfig.toml\x1b[0m\r\n");
    render_case(&gpu, ppem, 60, 4, ls.as_bytes(), true, &out_dir.join("ls_color.png"));

    // ---- 用例 4：vim 风格（行号栏 + 语法高亮 + 状态栏反显）----
    let mut vim = String::new();
    // 反显状态行在顶部。
    vim.push_str("\x1b[7m  main.rs                          rust  utf-8  \x1b[0m\r\n");
    vim.push_str("\x1b[38;5;240m  1\x1b[0m \x1b[38;5;170mfn\x1b[0m \x1b[38;5;39mmain\x1b[0m() {\r\n");
    vim.push_str("\x1b[38;5;240m  2\x1b[0m     \x1b[38;5;39mprintln!\x1b[0m(\x1b[38;5;107m\"hello, vlt\"\x1b[0m);\r\n");
    vim.push_str("\x1b[38;5;240m  3\x1b[0m }\r\n");
    vim.push_str("\x1b[38;5;240m  4\x1b[0m \x1b[38;5;240m// 光标在下一行\x1b[0m\r\n");
    vim.push_str("\x1b[7m-- INSERT --\x1b[0m\r\n");
    render_case(&gpu, ppem, 52, 7, vim.as_bytes(), true, &out_dir.join("vim.png"));

    // ---- 用例 5：htop 风格（进度条 + 彩色）----
    let mut htop = String::new();
    htop.push_str("  CPU[\x1b[32m||||||\x1b[33m|||\x1b[31m||\x1b[0m       35%]\r\n");
    htop.push_str("  Mem[\x1b[32m|||||||||||\x1b[0m           2.1G/16G]\r\n");
    htop.push_str("  Swp[\x1b[0m                        0K/2G]\r\n");
    htop.push_str("\r\n");
    htop.push_str("\x1b[30;42m  PID USER      CPU% MEM%  COMMAND          \x1b[0m\r\n");
    htop.push_str(" 1234 steven    12.3  4.5  \x1b[36mvlt\x1b[0m\r\n");
    htop.push_str(" 5678 steven     0.7  1.2  \x1b[36mbash\x1b[0m\r\n");
    render_case(&gpu, ppem, 52, 8, htop.as_bytes(), true, &out_dir.join("htop.png"));

    // ---- 用例 6：光标（block，文字反白）----
    let mut cur = String::new();
    cur.push_str("Cursor block over text: ready_");
    // 光标默认在输出末尾；这里让它落在字符上。
    render_case(&gpu, ppem, 40, 2, cur.as_bytes(), true, &out_dir.join("cursor.png"));

    println!("全部截图完成，见 tests/snapshots/");
}
