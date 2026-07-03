//! headless 渲染的回归测试。
//!
//! - `determinism`：同一内容渲染两次逐字节一致（逐像素一致性的进程内基线）。
//! - `gamma_correction_lightens`：gamma 校正开启时，深色文字叠白底的总墨量
//!   应少于 naive 混合（校正让笔画更轻、不发糊）。

use vlt::font::FontEngine;
use vlt::gpu::Gpu;
use vlt::headless::Headless;
use vlt::render::Renderer;
use vlt::snapshot::GridSnapshot;
use vlt::terminal::{term_from_ansi, TermSize};
use vlt::theme::Palette;

fn render(ansi: &[u8], cols: usize, lines: usize, gamma: bool) -> (u32, u32, Vec<u8>) {
    let ppem = 30.0;
    let mut font = FontEngine::new(ppem);
    let cw = font.metrics.width;
    let ch = font.metrics.height;
    let (w, h) = (cw * cols as u32, ch * lines as u32);

    let gpu = Gpu::new(None);
    let palette = Palette::default();
    let size = TermSize {
        columns: cols,
        screen_lines: lines,
    };
    let term = term_from_ansi(size, ansi);
    let snap = GridSnapshot::capture(&term, &palette);

    let mut renderer = Renderer::new(&gpu.device, &font);
    let headless = Headless::new(&gpu.device, w, h);
    let rgba = headless.render_to_rgba(&gpu, &mut renderer, &snap, &mut font, gamma);
    (w, h, rgba)
}

#[test]
fn determinism() {
    let text = b"Hello, Vellum! if (x==0) { foo->bar(); } 0123456789";
    let (_, _, a) = render(text, 52, 2, true);
    let (_, _, b) = render(text, 52, 2, true);
    assert_eq!(a, b, "同内容两次渲染必须逐字节一致");
}

#[test]
fn gamma_correction_lightens() {
    // 纯黑文字叠纯白底，统计总“墨量”（255 - 亮度 的和）。
    let text = b"AAAA BBBB gggg mmmm wwww 0000 The quick brown fox";
    let (_, _, on) = render(text, 52, 2, true);
    let (_, _, off) = render(text, 52, 2, false);

    let ink = |rgba: &[u8]| -> u64 {
        let mut sum = 0u64;
        for px in rgba.chunks_exact(4) {
            // 用绿通道近似亮度即可。
            sum += (255 - px[1]) as u64;
        }
        sum
    };
    let ink_on = ink(&on);
    let ink_off = ink(&off);

    // 两者必须不同（说明 gamma 开关确实生效）。
    assert_ne!(ink_on, ink_off, "gamma 开/关应产生不同像素");
    // gamma 校正后墨量更少（笔画更轻）。
    assert!(
        ink_on < ink_off,
        "gamma 校正应让墨量更少：on={ink_on} off={ink_off}"
    );
}
