//! 第 3 层：Vellum 字体引擎。
//!
//! 铁律 1：字形光栅化只走 swash（纯 Rust），零系统依赖。
//! 铁律 3：按目标字号 CPU 光栅化进 glyph atlas；字号/DPR 变化整体重建 atlas。
//!
//! Phase 1 仅内嵌 JetBrains Mono Regular，只渲染 ASCII/Latin 的灰度 alpha 图。
//! CJK / Emoji / 连字 / 回退链 / 亚像素偏移变体全部属于 Phase 2/3，此处不实现。

use std::collections::HashMap;

use swash::scale::{Render, ScaleContext, Source, StrikeWith};
use swash::scale::image::Content;
use swash::zeno::{Format, Vector};
use swash::FontRef;

/// 内嵌默认字体（铁律 5：不走 Fontconfig，直接内嵌）。
pub const JETBRAINS_MONO_REGULAR: &[u8] =
    include_bytes!("../assets/fonts/JetBrainsMono-Regular.ttf");

/// 单元格度量（全部为物理像素，且已取整，保证物理像素 1:1 对齐）。
#[derive(Clone, Copy, Debug)]
pub struct CellMetrics {
    /// 单元格宽度（物理像素，整数）。
    pub width: u32,
    /// 单元格高度（物理像素，整数）。
    pub height: u32,
    /// 基线相对单元格顶部的偏移（物理像素）。
    pub ascent: u32,
    /// 下划线相对单元格顶部的位置（物理像素）。
    pub underline_y: u32,
    /// 下划线粗细（物理像素，至少 1）。
    pub underline_thickness: u32,
}

/// atlas 中单个字形的记录。
#[derive(Clone, Copy, Debug)]
pub struct GlyphEntry {
    /// 在 atlas 纹理中的像素坐标与尺寸。
    pub atlas_x: u32,
    pub atlas_y: u32,
    pub width: u32,
    pub height: u32,
    /// 字形位图相对“字形绘制原点（基线上的笔尖）”的偏移。
    /// left = placement.left，top = placement.top（top 为正表示位图顶部在基线之上）。
    pub left: i32,
    pub top: i32,
}

/// 字形键：Phase 1 只按字符区分（单字体、单样式、无亚像素变体）。
/// TODO(Phase 2): 加入 (font_id, bold, italic, subpixel_offset) 维度。
type GlyphKey = char;

/// 灰度字形 atlas（单张纹理，shelf packing）。
pub struct Atlas {
    pub width: u32,
    pub height: u32,
    /// R8 单通道 alpha 数据，行主序。
    pub data: Vec<u8>,
    // shelf packing 游标。
    shelf_x: u32,
    shelf_y: u32,
    shelf_height: u32,
}

impl Atlas {
    fn new(width: u32, height: u32) -> Self {
        Atlas {
            width,
            height,
            data: vec![0u8; (width * height) as usize],
            shelf_x: 0,
            shelf_y: 0,
            shelf_height: 0,
        }
    }

    /// 在 atlas 中分配一块 (w × h) 区域并写入 alpha 数据，返回左上角坐标。
    /// 1px 间距避免采样时相邻字形串色。
    fn insert(&mut self, w: u32, h: u32, src: &[u8]) -> Option<(u32, u32)> {
        if w == 0 || h == 0 {
            return Some((0, 0));
        }
        const PAD: u32 = 1;
        if self.shelf_x + w + PAD > self.width {
            // 换行到新 shelf。
            self.shelf_x = 0;
            self.shelf_y += self.shelf_height + PAD;
            self.shelf_height = 0;
        }
        if self.shelf_y + h > self.height {
            return None; // atlas 满（Phase 1 固定尺寸够用，满了直接放弃该字形）。
        }
        let (x, y) = (self.shelf_x, self.shelf_y);
        for row in 0..h {
            let dst_off = ((y + row) * self.width + x) as usize;
            let src_off = (row * w) as usize;
            self.data[dst_off..dst_off + w as usize]
                .copy_from_slice(&src[src_off..src_off + w as usize]);
        }
        self.shelf_x += w + PAD;
        self.shelf_height = self.shelf_height.max(h);
        Some((x, y))
    }
}

/// 字体引擎：持有字体数据、scaler 上下文、cell 度量与 glyph atlas。
pub struct FontEngine {
    font_data: &'static [u8],
    context: ScaleContext,
    ppem: f32,
    pub metrics: CellMetrics,
    pub atlas: Atlas,
    cache: HashMap<GlyphKey, Option<GlyphEntry>>,
    /// atlas 是否自上次上传后有新增字形。
    pub dirty: bool,
}

impl FontEngine {
    /// 用内嵌 JetBrains Mono，按给定物理像素字号（ppem）构建引擎。
    ///
    /// `ppem` 应为逻辑字号 × DPR 后取整的物理像素值，保证 1:1 对齐。
    pub fn new(ppem: f32) -> Self {
        let font_data = JETBRAINS_MONO_REGULAR;
        let font = FontRef::from_index(font_data, 0).expect("内嵌字体解析失败");

        let metrics = compute_cell_metrics(&font, ppem);

        // 2048² 单张灰度 atlas，对 ASCII/Latin 足够。
        let atlas = Atlas::new(2048, 2048);

        FontEngine {
            font_data,
            context: ScaleContext::new(),
            ppem,
            metrics,
            atlas,
            cache: HashMap::new(),
            dirty: true,
        }
    }

    fn font(&self) -> FontRef<'static> {
        FontRef::from_index(self.font_data, 0).unwrap()
    }

    /// 取得字符对应的 atlas 记录，必要时即时光栅化并写入 atlas。
    /// 返回 None 表示该字形无可见像素（如空格）或分配失败。
    pub fn glyph(&mut self, ch: char) -> Option<GlyphEntry> {
        if let Some(entry) = self.cache.get(&ch) {
            return *entry;
        }
        let entry = self.rasterize(ch);
        self.cache.insert(ch, entry);
        if entry.is_some() {
            self.dirty = true;
        }
        entry
    }

    fn rasterize(&mut self, ch: char) -> Option<GlyphEntry> {
        let font = self.font();
        let glyph_id = font.charmap().map(ch);
        if glyph_id == 0 {
            // .notdef —— Phase 1 直接跳过（不画 tofu）。TODO(Phase 2): 回退链/tofu。
            return None;
        }

        let mut scaler = self
            .context
            .builder(font)
            .size(self.ppem)
            .hint(false) // 无 hinting：一致性 > 低分屏微调（design.md §5.1）。
            .build();

        // 只取轮廓源，渲染为 8-bit alpha 灰度。
        let image = Render::new(&[
            Source::ColorOutline(0),
            Source::ColorBitmap(StrikeWith::BestFit),
            Source::Outline,
        ])
        .format(Format::Alpha)
        .offset(Vector::new(0.0, 0.0))
        .render(&mut scaler, glyph_id)?;

        // Phase 1 只处理灰度 mask；彩色内容（Emoji）跳过。TODO(Phase 3)。
        if image.content != Content::Mask {
            return None;
        }

        let w = image.placement.width;
        let h = image.placement.height;
        if w == 0 || h == 0 {
            // 空白字形（空格等），缓存为“无像素”。
            return None;
        }

        let (ax, ay) = self.atlas.insert(w, h, &image.data)?;
        Some(GlyphEntry {
            atlas_x: ax,
            atlas_y: ay,
            width: w,
            height: h,
            left: image.placement.left,
            top: image.placement.top,
        })
    }
}

/// 由字体度量计算等宽单元格尺寸（全部取整到物理像素，保证 1:1 对齐）。
fn compute_cell_metrics(font: &FontRef, ppem: f32) -> CellMetrics {
    let m = font.metrics(&[]).scale(ppem);

    // 等宽字体：用 'M' 的 advance 作为 cell 宽度权威（JetBrains Mono 全字符等宽）。
    let glyph_id = font.charmap().map('M');
    let advance = font.glyph_metrics(&[]).scale(ppem).advance_width(glyph_id);

    let width = advance.round().max(1.0) as u32;

    // 行高 = ascent + descent + leading，向上取整保证不裁切。
    let ascent_f = m.ascent;
    let descent_f = m.descent;
    let leading_f = m.leading;
    let height = (ascent_f + descent_f + leading_f).ceil().max(1.0) as u32;

    let ascent = ascent_f.round().max(1.0) as u32;

    // 下划线：置于基线下方约 descent 的一半处。
    let underline_thickness = m.stroke_size.round().max(1.0) as u32;
    let underline_y = (ascent as f32 + (descent_f * 0.5)).round() as u32;

    CellMetrics {
        width,
        height,
        ascent,
        underline_y: underline_y.min(height.saturating_sub(underline_thickness)),
        underline_thickness,
    }
}
