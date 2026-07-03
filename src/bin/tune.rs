//! 视觉调优截图工具（T1 CJK scale 阶梯 + T2 锐度包对比）。
//!
//! 与入库回归的 `snapshot` bin 不同：本工具**运行时磁盘加载商业字体**
//! （Pragmata + Microsoft YaHei），仅用于人工比对选档，产物写 scratchpad，
//! 绝不入库、绝不 embed（许可红线）。
//!
//! 用法：`cargo run --release --bin tune -- <out_dir>`。
//! 全部按 DPR=1（用户 96 DPI 双 1080p）实际字号出图。

use std::path::{Path, PathBuf};

use vlt::config::{FontRole, FontSpec};
use vlt::font::{AaMode, FontEngine, RasterTuning};
use vlt::gpu::Gpu;
use vlt::headless::{write_png, Headless};
use vlt::render::Renderer;
use vlt::snapshot::GridSnapshot;
use vlt::terminal::{term_from_ansi, TermSize};
use vlt::theme::Palette;

/// 本机字体角色（磁盘加载）：Pragmata 主字体 + 微软雅黑 CJK + Sarasa 兜底。
fn spec(cjk_scale: f32) -> FontSpec {
    FontSpec {
        latin: Some(FontRole {
            source: "Pragmata".to_string(),
            scale: 1.0,
        }),
        cjk: Some(FontRole {
            source: "Microsoft YaHei".to_string(),
            scale: cjk_scale,
        }),
        symbols: None,
        fallback: vec!["Sarasa Mono SC".to_string()],
    }
}

/// 渲染一行样文到 PNG（可选 N× 整数最近邻放大，便于观察子像素/笔画）。
#[allow(clippy::too_many_arguments)]
fn render_line(
    gpu: &Gpu,
    ppem: f32,
    text: &str,
    spec: &FontSpec,
    tuning: RasterTuning,
    cols: usize,
    zoom: u32,
    out: &Path,
) {
    let mut font = FontEngine::from_spec_tuned(ppem, spec, 0.0, 1.0, tuning);
    let cw = font.metrics.width;
    let ch = font.metrics.height;
    let lines = 1usize;
    let width = cw * cols as u32;
    let height = ch * lines as u32;

    let palette = Palette::default();
    let size = TermSize {
        columns: cols,
        screen_lines: lines,
    };
    // 单行样文：不追加换行（screen_lines=1 时换行会把内容滚进历史，可见区变空白）。
    let term = term_from_ansi(size, text.as_bytes());
    let mut snap = GridSnapshot::capture(&term, &palette);
    snap.cursor.visible = false; // 静态比对不画光标

    let mut renderer = Renderer::new(&gpu.device, &font);
    let headless = Headless::new(&gpu.device, width, height);
    let rgba = headless.render_to_rgba(gpu, &mut renderer, &snap, &mut font, true);

    let (ow, oh, data) = if zoom <= 1 {
        (width, height, rgba)
    } else {
        nearest_zoom(&rgba, width, height, zoom)
    };
    write_png(out, ow, oh, &data).expect("写 PNG 失败");
    println!("  {} ({}x{}, cell {}x{}px, zoom {}x)", out.display(), ow, oh, cw, ch, zoom);
}

/// 最近邻整数放大（无插值，忠实展示每个物理像素）。
fn nearest_zoom(rgba: &[u8], w: u32, h: u32, z: u32) -> (u32, u32, Vec<u8>) {
    let (ow, oh) = (w * z, h * z);
    let mut out = vec![0u8; (ow * oh * 4) as usize];
    for y in 0..oh {
        let sy = y / z;
        for x in 0..ow {
            let sx = x / z;
            let s = ((sy * w + sx) * 4) as usize;
            let d = ((y * ow + x) * 4) as usize;
            out[d..d + 4].copy_from_slice(&rgba[s..s + 4]);
        }
    }
    (ow, oh, out)
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

    // 用户 96 DPI DPR=1、默认字号 15 → 物理 ppem=15。
    let ppem = 15.0;
    // T1 样文：中英混排（含易辨识别 x-height 的字母与常用汉字）。
    let cjk_text = "他说：Hello, vlt! 永东国酬爱郁灵鹰袋 0123 iIlL";
    let cols = 52;

    // ---- T1：CJK scale 阶梯（0.88 / 0.92 / 0.96 / 1.00），默认锐度 ----
    println!("== T1 CJK scale 阶梯 ==");
    let default_tuning = RasterTuning::default();
    for (tag, s) in [("088", 0.88f32), ("092", 0.92), ("096", 0.96), ("100", 1.00)] {
        render_line(
            &gpu,
            ppem,
            cjk_text,
            &spec(s),
            default_tuning,
            cols,
            2,
            &out_dir.join(format!("cjk_scale_{tag}.png")),
        );
    }

    // ---- T2.2：text_contrast 开/关对比（选定 scale=0.92，短样文高倍放大）----
    println!("== T2.2 stem darkening 对比 ==");
    // 细笔画敏感样文：i/l/1/f 竖干 + 逗号句点 + 中文细横。
    let sharp_text = "Illifl 1il 永东爱 abg";
    let ncols = 24;
    let off = RasterTuning { contrast: 0.0, aa: AaMode::Grayscale };
    let on = RasterTuning { contrast: 0.30, aa: AaMode::Grayscale };
    render_line(&gpu, ppem, sharp_text, &spec(0.92), off, ncols, 6, &out_dir.join("contrast_off.png"));
    render_line(&gpu, ppem, sharp_text, &spec(0.92), on, ncols, 6, &out_dir.join("contrast_on.png"));

    // ---- T2.3：grayscale vs subpixel-rgb 放大对比 ----
    println!("== T2.3 grayscale vs subpixel-rgb ==");
    let gray = RasterTuning { contrast: 0.30, aa: AaMode::Grayscale };
    let sub = RasterTuning { contrast: 0.30, aa: AaMode::SubpixelRgb };
    render_line(&gpu, ppem, sharp_text, &spec(0.92), gray, ncols, 6, &out_dir.join("aa_grayscale.png"));
    render_line(&gpu, ppem, sharp_text, &spec(0.92), sub, ncols, 6, &out_dir.join("aa_subpixel_rgb.png"));

    println!("完成，产物见 {}", out_dir.display());
}
