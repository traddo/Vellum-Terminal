//! 第 3 层：Vellum 字体引擎（角色制）。
//!
//! 铁律 1：字形光栅化只走 swash（纯 Rust），零系统依赖。
//! 铁律 3：按目标字号 CPU 光栅化进 glyph atlas；字号/DPR 变化整体重建 atlas。
//! 铁律 4：网格宽度只由主字体 cell 尺寸与 unicode-width 决定；
//!         回退/CJK 字体 advance 一律不参与排版，字形在其 1/2 格内居中缩放。
//! 铁律 5：字体发现不走 Fontconfig——内嵌默认字体 + fontdb 目录扫描
//!         （显式禁用 fontdb 的 fontconfig XML 解析特性，纯目录扫描）。
//!
//! 角色制（VSCode 式）：latin（主字体，cell 权威）/ cjk / symbols（预留）/
//! fallback 链 / 内嵌 JetBrains Mono 终极兜底 / tofu。
//! 许可红线：商业字体只运行时从磁盘加载，绝不内嵌、绝不入库。
//!
//! TODO(Phase 3): 合成粗体/斜体（Pragmata 等单字面家族）、彩色 Emoji、连字、
//!                亚像素偏移变体。当前 bold/italic 用同一字面渲染。

use std::collections::HashMap;
use std::sync::Arc;

use swash::scale::image::Content;
use swash::scale::{Render, ScaleContext, Source, StrikeWith};
use swash::zeno::{Format, Vector};
use swash::FontRef;
use unicode_width::UnicodeWidthChar;

use crate::config::FontSpec;

/// 内嵌默认字体（OFL 许可，铁律 5：不走 Fontconfig，直接内嵌）。
pub const JETBRAINS_MONO_REGULAR: &[u8] =
    include_bytes!("../assets/fonts/JetBrainsMono-Regular.ttf");

/// 光栅化调优参数（锐度包 T2）。全部为「跨机确定」的纯 CPU 后处理，
/// 同一取值在任何机器上产出逐字节一致的 alpha/RGBA mask（不破逐像素一致性铁律）。
#[derive(Clone, Copy, Debug)]
pub struct RasterTuning {
    /// 笔画对比度增强（stem darkening）。0.0 = 关闭（忠实覆盖率）。
    /// 无 hinting 的细笔画在白底显「洗白」，此项对 alpha 覆盖率做幂律加深：
    /// `cov' = cov^(1/(1+contrast))`，contrast>0 时抬升中间覆盖率、笔画更扎实。
    /// 纯查表/幂运算，确定性；不引入任何系统依赖。
    pub contrast: f32,
    /// 亚像素抗锯齿模式（决定 atlas 通道与光栅路径）。
    pub aa: AaMode,
}

impl Default for RasterTuning {
    fn default() -> Self {
        // 96 DPI 白底默认：轻度 stem darkening 让细笔画「扎实不糊」，
        // 但不过冲（>0.5 会让 'o'/'e' 等闭合字腔糊死）。灰度 AA 为默认。
        RasterTuning {
            contrast: 0.30,
            aa: AaMode::Grayscale,
        }
    }
}

/// 抗锯齿模式（text_aa 配置）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AaMode {
    /// 灰度覆盖率（默认，跨机最稳，无彩边）。
    Grayscale,
    /// 水平亚像素 RGB（标准 LCD 条带排列）。
    SubpixelRgb,
    /// 水平亚像素 BGR（部分面板条带反序）。
    SubpixelBgr,
}

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
    /// 字形位图相对「该字符首格左边缘 + 主字体基线」的最终偏移。
    /// 非主字体的居中已在光栅化时折算进 left，渲染层无需感知字体来源。
    pub left: i32,
    pub top: i32,
}

/// 字体数据来源：内嵌静态 或 磁盘加载。
enum FontData {
    Static(&'static [u8]),
    Owned(Arc<Vec<u8>>),
}

impl FontData {
    fn as_slice(&self) -> &[u8] {
        match self {
            FontData::Static(b) => b,
            FontData::Owned(v) => v.as_slice(),
        }
    }
}

/// 一个已加载的字体槽位。
struct FontSlot {
    data: FontData,
    /// TTC 内的 face 索引（msyh.ttc 等集合格式）。
    index: u32,
    /// 该字体的光栅化 ppem（自动适配 × 角色 scale，铁律 4：只缩字形不动网格）。
    ppem: f32,
    /// 家族名/来源描述（诊断用）。
    family: String,
}

/// 槽位角色索引表：把「角色」映射到 slots 下标。
#[derive(Default)]
struct RoleIndex {
    /// 主字体（永远是 slots[0]）。
    // main = 0
    cjk: Option<usize>,
    symbols: Option<usize>,
    /// fallback 链（顺序即优先级）。
    fallback: Vec<usize>,
    /// 内嵌 JetBrains Mono 兜底（若主字体即内嵌则为 None，避免重复）。
    embedded: Option<usize>,
}

/// 字形 atlas（单张 RGBA8 纹理，shelf packing）。
///
/// 统一用 RGBA8 承载「每通道覆盖率」：灰度 AA 时 R=G=B=覆盖率；
/// 亚像素 AA 时 R/G/B 为三个子像素各自覆盖率。A 通道恒为 255（不用）。
/// 单一格式让灰度/亚像素共用同一纹理与同一 shader 路径（per-channel mix）。
pub struct Atlas {
    pub width: u32,
    pub height: u32,
    /// RGBA8 数据，行主序，每像素 4 字节。
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
            data: vec![0u8; (width * height * 4) as usize],
            shelf_x: 0,
            shelf_y: 0,
            shelf_height: 0,
        }
    }

    /// 在 atlas 中分配一块 (w × h) 区域并写入 RGBA 数据（`src` 长度须为 w*h*4），
    /// 返回左上角坐标。1px 间距避免采样时相邻字形串色。
    fn insert(&mut self, w: u32, h: u32, src: &[u8]) -> Option<(u32, u32)> {
        if w == 0 || h == 0 {
            return Some((0, 0));
        }
        const PAD: u32 = 1;
        if self.shelf_x + w + PAD > self.width {
            self.shelf_x = 0;
            self.shelf_y += self.shelf_height + PAD;
            self.shelf_height = 0;
        }
        if self.shelf_y + h > self.height {
            return None; // atlas 满（CJK 常用字规模下 2048² 足够；满了放弃该字形）。
        }
        let (x, y) = (self.shelf_x, self.shelf_y);
        for row in 0..h {
            let dst_off = (((y + row) * self.width + x) * 4) as usize;
            let src_off = (row * w * 4) as usize;
            let n = (w * 4) as usize;
            self.data[dst_off..dst_off + n].copy_from_slice(&src[src_off..src_off + n]);
        }
        self.shelf_x += w + PAD;
        self.shelf_height = self.shelf_height.max(h);
        Some((x, y))
    }
}

/// 字体引擎：持有角色化字体槽位、scaler 上下文、cell 度量与 glyph atlas。
pub struct FontEngine {
    slots: Vec<FontSlot>,
    roles: RoleIndex,
    context: ScaleContext,
    pub metrics: CellMetrics,
    pub atlas: Atlas,
    cache: HashMap<char, Option<GlyphEntry>>,
    /// 光栅化调优（stem darkening / 亚像素 AA，T2）。
    tuning: RasterTuning,
    /// atlas 是否自上次上传后有新增字形。
    pub dirty: bool,
}

impl FontEngine {
    /// 零配置构造：内嵌 JetBrains Mono 主字体 + 默认 fallback（Sarasa Mono SC）。
    pub fn new(ppem: f32) -> Self {
        Self::from_spec_tuned(ppem, &FontSpec::default(), 0.0, 1.0, RasterTuning::default())
    }

    /// 按角色配置构造（默认字距/行高/调优）。
    pub fn from_spec(ppem: f32, spec: &FontSpec) -> Self {
        Self::from_spec_tuned(ppem, spec, 0.0, 1.0, RasterTuning::default())
    }

    /// 按角色配置 + cell 微调 + 光栅调优构造。
    ///
    /// - `letter_spacing_px`：物理像素，加到主字体 cell 宽度（唯一权威）上。
    ///   全网格统一变化；CJK 仍占 2×cell、回退字形随之居中，不破铁律 4。
    /// - `line_height`：行高倍数（1.0 = 字体自身度量）。
    /// - `tuning`：stem darkening 对比度 + 亚像素 AA 模式（T2）。
    ///
    /// 任何角色未命中都记日志并优雅降级，最终兜底内嵌字体。
    pub fn from_spec_tuned(
        ppem: f32,
        spec: &FontSpec,
        letter_spacing_px: f32,
        line_height: f32,
        tuning: RasterTuning,
    ) -> Self {
        let db = build_font_db();
        let mut slots: Vec<FontSlot> = Vec::new();
        let mut roles = RoleIndex::default();

        // ---- 主字体（latin 角色）：cell 尺寸权威 ----
        let mut main_is_embedded = true;
        if let Some(role) = &spec.latin {
            if let Some((data, index)) = load_source(&db, &role.source) {
                slots.push(FontSlot {
                    data: FontData::Owned(data),
                    index,
                    ppem: ppem * role.scale,
                    family: role.source.clone(),
                });
                main_is_embedded = false;
                log::info!("latin 主字体：{}", role.source);
            } else {
                log::warn!("latin 字体未找到：{}（退回内嵌 JetBrains Mono）", role.source);
            }
        }
        if slots.is_empty() {
            slots.push(FontSlot {
                data: FontData::Static(JETBRAINS_MONO_REGULAR),
                index: 0,
                ppem,
                family: "JetBrains Mono (embedded)".to_string(),
            });
        }

        // cell 度量来自主字体（铁律 4）；letter_spacing/line_height 在此并入。
        let metrics = {
            let s = &slots[0];
            let font =
                FontRef::from_index(s.data.as_slice(), s.index as usize).expect("主字体解析失败");
            compute_cell_metrics(&font, s.ppem, letter_spacing_px, line_height)
        };

        // ---- cjk 角色 ----
        if let Some(role) = &spec.cjk {
            if let Some((data, index)) = load_source(&db, &role.source) {
                let fit = fit_fallback_ppem(data.as_slice(), index, ppem, &metrics);
                let final_ppem = fit * role.scale;
                log::info!(
                    "cjk 字体：{}（自动适配 ppem {:.1} × scale {:.2} = {:.1}）",
                    role.source, fit, role.scale, final_ppem
                );
                slots.push(FontSlot {
                    data: FontData::Owned(data),
                    index,
                    ppem: final_ppem,
                    family: role.source.clone(),
                });
                roles.cjk = Some(slots.len() - 1);
            } else {
                log::warn!("cjk 字体未找到：{}（CJK 走 fallback 链）", role.source);
            }
        }

        // ---- symbols 角色（Phase 3 预留） ----
        if let Some(role) = &spec.symbols {
            if let Some((data, index)) = load_source(&db, &role.source) {
                let fit = fit_fallback_ppem(data.as_slice(), index, ppem, &metrics);
                slots.push(FontSlot {
                    data: FontData::Owned(data),
                    index,
                    ppem: fit * role.scale,
                    family: role.source.clone(),
                });
                roles.symbols = Some(slots.len() - 1);
            } else {
                log::warn!("symbols 字体未找到：{}", role.source);
            }
        }

        // ---- fallback 链 ----
        for family in &spec.fallback {
            if let Some((data, index)) = load_source(&db, family) {
                let fit = fit_fallback_ppem(data.as_slice(), index, ppem, &metrics);
                slots.push(FontSlot {
                    data: FontData::Owned(data),
                    index,
                    ppem: fit,
                    family: family.clone(),
                });
                roles.fallback.push(slots.len() - 1);
            } else {
                log::warn!("fallback 字体未找到（跳过）：{}", family);
            }
        }

        // ---- 内嵌兜底（主字体非内嵌时追加） ----
        if !main_is_embedded {
            slots.push(FontSlot {
                data: FontData::Static(JETBRAINS_MONO_REGULAR),
                index: 0,
                ppem,
                family: "JetBrains Mono (embedded)".to_string(),
            });
            roles.embedded = Some(slots.len() - 1);
        }

        FontEngine {
            slots,
            roles,
            context: ScaleContext::new(),
            metrics,
            atlas: Atlas::new(2048, 2048),
            cache: HashMap::new(),
            tuning,
            dirty: true,
        }
    }

    /// 已加载的字体清单（诊断/报告用）。
    pub fn loaded_families(&self) -> Vec<&str> {
        self.slots.iter().map(|s| s.family.as_str()).collect()
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
        // 铁律 4：宽度判定唯一来源是 unicode-width（与 alacritty_terminal 同版本同源）。
        let cells = match ch.width() {
            Some(0) | None => return None, // 零宽/控制字符不绘制。TODO(Phase 3): 组合字符。
            Some(w) => w.min(2) as u32,
        };
        let span_w = (self.metrics.width * cells) as i32;

        // 按角色确定查找顺序。
        let mut order: Vec<usize> = Vec::with_capacity(self.slots.len());
        if is_cjk(ch) {
            // CJK 码点：cjk 角色 → fallback 链 → 内嵌 → 主字体（最后试，一般没有）。
            if let Some(i) = self.roles.cjk {
                order.push(i);
            }
            order.extend(&self.roles.fallback);
            if let Some(i) = self.roles.embedded {
                order.push(i);
            }
            order.push(0);
        } else {
            // 其余：主字体 → symbols → fallback 链 → 内嵌。
            order.push(0);
            if let Some(i) = self.roles.symbols {
                order.push(i);
            }
            order.extend(&self.roles.fallback);
            if let Some(i) = self.roles.embedded {
                order.push(i);
            }
        }

        for fi in order {
            let glyph_id = {
                let slot = &self.slots[fi];
                match FontRef::from_index(slot.data.as_slice(), slot.index as usize) {
                    Some(font) => font.charmap().map(ch),
                    None => 0,
                }
            };
            if glyph_id == 0 {
                continue;
            }
            if let Some(entry) = self.rasterize_from_slot(fi, glyph_id, span_w) {
                return Some(entry);
            }
            // 光栅成功但空白（如空格）→ 直接返回“无像素”。
            return None;
        }

        // 整条链都没有 → tofu（空心矩形占位）。
        self.make_tofu(span_w)
    }

    /// 用指定槽位光栅化字形；非主字体按 advance 在其 1/2 格内水平居中。
    /// 输出统一为 RGBA（每通道覆盖率）；亚像素模式下横向 3× 过采样 + LCD 滤波。
    fn rasterize_from_slot(&mut self, fi: usize, glyph_id: u16, span_w: i32) -> Option<GlyphEntry> {
        let slot = &self.slots[fi];
        let ppem = slot.ppem;
        let font = FontRef::from_index(slot.data.as_slice(), slot.index as usize)?;

        // 非主字体的水平居中偏移：按该字形自身 advance 计算。
        // advance 只用于居中，绝不参与排版（铁律 4）。
        let center_dx = if fi == 0 {
            0
        } else {
            let adv = font.glyph_metrics(&[]).scale(ppem).advance_width(glyph_id);
            (((span_w as f32) - adv) / 2.0).round() as i32
        };

        let subpixel = matches!(self.tuning.aa, AaMode::SubpixelRgb | AaMode::SubpixelBgr);

        // 亚像素：横向 3× 过采样光栅化，再逐 3 子像素合成一个像素的 RGB。
        // 灰度：正常 1× 光栅化。x 方向的缩放通过 transform 矩阵实现（design.md 亚像素路线）。
        let x_scale = if subpixel { 3.0 } else { 1.0 };

        // 无 hinting：一致性 > 低分屏微调（design.md §5.1）。
        let mut scaler = self.context.builder(font).size(ppem).hint(false).build();

        // 亚像素时用 Render transform 在 x 方向放大 3×（placement.left/width 随之 3×）。
        let mut render = Render::new(&[
            Source::ColorOutline(0),
            Source::ColorBitmap(StrikeWith::BestFit),
            Source::Outline,
        ]);
        render.format(Format::Alpha).offset(Vector::new(0.0, 0.0));
        if subpixel {
            use swash::zeno::Transform;
            render.transform(Some(Transform::scale(x_scale, 1.0)));
        }
        let image = render.render(&mut scaler, glyph_id)?;

        // 灰度 mask 之外的内容（彩色 Emoji 等）暂不支持。TODO(Phase 3)。
        if image.content != Content::Mask {
            return None;
        }

        let sw = image.placement.width; // 过采样后的位图宽（亚像素时 ≈3×）
        let h = image.placement.height;
        if sw == 0 || h == 0 {
            return None; // 空白字形（空格等）。
        }

        let (rgba, w, left_px) = if subpixel {
            self.compose_subpixel(&image.data, sw, h, image.placement.left)
        } else {
            (self.compose_grayscale(&image.data, sw, h), sw, image.placement.left)
        };

        let (ax, ay) = self.atlas.insert(w, h, &rgba)?;
        Some(GlyphEntry {
            atlas_x: ax,
            atlas_y: ay,
            width: w,
            height: h,
            left: left_px + center_dx,
            top: image.placement.top,
        })
    }

    /// 灰度合成：单通道覆盖率 → RGBA（R=G=B=cov'，A=255），并应用 stem darkening。
    fn compose_grayscale(&self, mask: &[u8], w: u32, h: u32) -> Vec<u8> {
        let mut out = vec![0u8; (w * h * 4) as usize];
        for i in 0..(w * h) as usize {
            let c = apply_contrast(mask[i], self.tuning.contrast);
            out[i * 4] = c;
            out[i * 4 + 1] = c;
            out[i * 4 + 2] = c;
            out[i * 4 + 3] = 255;
        }
        out
    }

    /// 亚像素合成：横向 3× 过采样 mask → 逐像素 RGB 三元组。
    ///
    /// 路线（design.md T2）：源位图每 3 个横向子样本对应一个目标像素的 R/G/B，
    /// 再用相邻通道 [1/9,2/9,3/9,2/9,1/9] 归一化 5 抽头 LCD filter 压制彩边（FIR 低通）。
    /// BGR 面板在最后交换 R/B。stem darkening 在子像素域先施加。
    /// 返回 (rgba, 目标宽度像素, 目标 left 像素)。
    fn compose_subpixel(
        &self,
        mask: &[u8],
        sw: u32,
        h: u32,
        src_left: i32,
    ) -> (Vec<u8>, u32, i32) {
        // 目标宽度：过采样宽度 /3 向上取整；左右各留 1 像素余量吸收 filter 拖尾。
        let dst_w = sw.div_ceil(3) + 2;
        let dst_left = src_left.div_euclid(3) - 1;
        // 子像素起点在目标位图内的偏移（源 left 相对 dst_left*3 的位置）。
        let sub_origin = src_left - dst_left * 3;

        // LCD 5 抽头滤波器（归一化，和为 1）。
        const TAP: [i32; 5] = [1, 2, 3, 2, 1];
        const TAP_SUM: i32 = 9;

        let bgr = self.tuning.aa == AaMode::SubpixelBgr;
        let mut out = vec![0u8; (dst_w * h * 4) as usize];

        for y in 0..h as i32 {
            for x in 0..dst_w as i32 {
                // 该目标像素三个子像素在过采样源中的中心索引。
                let base = x * 3 + sub_origin;
                let mut chan = [0u8; 3];
                for (ci, c) in chan.iter_mut().enumerate() {
                    // 子像素中心 = base + ci；对其 ±2 邻域做 5 抽头卷积。
                    let center = base + ci as i32;
                    let mut acc = 0i32;
                    for (t, &wgt) in TAP.iter().enumerate() {
                        let sx = center + t as i32 - 2;
                        let v = if sx >= 0 && sx < sw as i32 {
                            mask[(y as u32 * sw + sx as u32) as usize] as i32
                        } else {
                            0
                        };
                        acc += v * wgt;
                    }
                    let cov = (acc / TAP_SUM).clamp(0, 255) as u8;
                    *c = apply_contrast(cov, self.tuning.contrast);
                }
                let (r, g, b) = if bgr {
                    (chan[2], chan[1], chan[0])
                } else {
                    (chan[0], chan[1], chan[2])
                };
                let off = ((y as u32 * dst_w + x as u32) * 4) as usize;
                out[off] = r;
                out[off + 1] = g;
                out[off + 2] = b;
                out[off + 3] = 255;
            }
        }
        (out, dst_w, dst_left)
    }

    /// 合成 tofu（空心矩形）：链上所有字体都缺字时的占位。
    fn make_tofu(&mut self, span_w: i32) -> Option<GlyphEntry> {
        let m = self.metrics;
        // 矩形尺寸：占格宽约 70%，高约为 ascent 的 76%，底边落在基线。
        let w = ((span_w as f32) * 0.70).round().max(3.0) as u32;
        let h = ((m.ascent as f32) * 0.76).round().max(3.0) as u32;
        let stroke = ((m.height as f32) / 14.0).round().max(1.0) as u32;

        let mut bitmap = vec![0u8; (w * h * 4) as usize];
        for y in 0..h {
            for x in 0..w {
                let edge = x < stroke || y < stroke || x >= w - stroke || y >= h - stroke;
                if edge {
                    let off = ((y * w + x) * 4) as usize;
                    // 略淡于正文，不喧宾夺主；RGBA（灰度写三通道）。
                    bitmap[off] = 0xB0;
                    bitmap[off + 1] = 0xB0;
                    bitmap[off + 2] = 0xB0;
                    bitmap[off + 3] = 255;
                }
            }
        }

        let (ax, ay) = self.atlas.insert(w, h, &bitmap)?;
        Some(GlyphEntry {
            atlas_x: ax,
            atlas_y: ay,
            width: w,
            height: h,
            left: (span_w as u32).saturating_sub(w) as i32 / 2,
            top: h as i32, // 底边落在基线上
        })
    }
}

/// Stem darkening：对 alpha 覆盖率做幂律加深。
/// `cov' = 255 * (cov/255)^(1/(1+contrast))`，contrast=0 时恒等（不改一个字节）。
/// 纯标量运算，跨机确定；抬升 0<cov<255 的中间覆盖率，让无 hinting 的细笔画更扎实。
#[inline]
fn apply_contrast(cov: u8, contrast: f32) -> u8 {
    if contrast <= 0.0 || cov == 0 || cov == 255 {
        return cov;
    }
    let x = cov as f32 / 255.0;
    let y = x.powf(1.0 / (1.0 + contrast));
    (y * 255.0 + 0.5).clamp(0.0, 255.0) as u8
}

/// CJK 码点判定（决定路由到 cjk 角色）。
/// 覆盖：CJK 统一表意及扩展、部首、注音、假名、谚文、CJK 标点、全角半角形。
fn is_cjk(ch: char) -> bool {
    matches!(u32::from(ch),
        0x1100..=0x11FF     // Hangul Jamo
        | 0x2E80..=0x303F   // CJK 部首/康熙/注音/CJK 标点
        | 0x3040..=0x30FF   // 平/片假名
        | 0x3130..=0x318F   // Hangul 兼容 Jamo
        | 0x31C0..=0x9FFF   // 笔画/扩展 A/统一表意
        | 0xA960..=0xA97F   // Hangul Jamo 扩展 A
        | 0xAC00..=0xD7FF   // Hangul 音节及 Jamo 扩展 B
        | 0xF900..=0xFAFF   // CJK 兼容表意
        | 0xFE30..=0xFE4F   // CJK 兼容形
        | 0xFF00..=0xFFEF   // 全角/半角形
        | 0x20000..=0x3FFFF // 扩展 B..H
    )
}

/// 构建 fontdb（固定顺序的显式目录扫描，不读 fontconfig 配置）。
fn build_font_db() -> fontdb::Database {
    let mut db = fontdb::Database::new();
    // 顺序确定：用户目录优先，再系统目录。
    if let Some(home) = std::env::var_os("HOME") {
        let home = std::path::PathBuf::from(home);
        db.load_fonts_dir(home.join(".local/share/fonts"));
        db.load_fonts_dir(home.join(".fonts"));
    }
    db.load_fonts_dir("/usr/local/share/fonts");
    db.load_fonts_dir("/usr/share/fonts");
    db
}

/// 解析「家族名或文件路径」为 (整文件字节, face 索引)。
/// 家族名走 fontdb 精确匹配；路径直接读文件（face 0）。
fn load_source(db: &fontdb::Database, source: &str) -> Option<(Arc<Vec<u8>>, u32)> {
    let trimmed = source.trim();
    if trimmed.is_empty() {
        return None;
    }
    // 看起来像路径 → 直接加载。
    let path = std::path::Path::new(trimmed);
    if trimmed.contains('/') || path.is_absolute() {
        let bytes = std::fs::read(path).ok()?;
        return Some((Arc::new(bytes), 0));
    }

    // 家族名：fontdb 精确匹配（Family::Name 是全名匹配，不做前缀/模糊）。
    let query = fontdb::Query {
        families: &[fontdb::Family::Name(trimmed)],
        weight: fontdb::Weight::NORMAL,
        stretch: fontdb::Stretch::Normal,
        style: fontdb::Style::Normal,
    };
    let id = db.query(&query)?;
    let (source, index) = db.face_source(id)?;
    match source {
        fontdb::Source::File(path) => {
            let bytes = std::fs::read(&path).ok()?;
            Some((Arc::new(bytes), index))
        }
        fontdb::Source::Binary(data) | fontdb::Source::SharedFile(_, data) => {
            Some((Arc::new(data.as_ref().as_ref().to_vec()), index))
        }
    }
}

/// 计算非主字体的自动适配 ppem：
/// 以主字体 ppem 为起点，若其 CJK 全宽 advance 超出 2×cell_w、
/// 或行高超出 cell_h，则等比缩小——只缩字形，绝不动网格（铁律 4）。
fn fit_fallback_ppem(data: &[u8], index: u32, main_ppem: f32, cell: &CellMetrics) -> f32 {
    let Some(font) = FontRef::from_index(data, index as usize) else {
        return main_ppem;
    };
    // 用「水」作为 CJK 全宽代表字（常用、无争议的全宽汉字）。
    let gid = font.charmap().map('水');
    let mut scale = 1.0f32;
    if gid != 0 {
        let adv = font.glyph_metrics(&[]).scale(main_ppem).advance_width(gid);
        let max_w = (cell.width * 2) as f32;
        if adv > max_w {
            scale = scale.min(max_w / adv);
        }
    }
    let m = font.metrics(&[]).scale(main_ppem);
    let line_h = m.ascent + m.descent;
    if line_h > cell.height as f32 {
        scale = scale.min(cell.height as f32 / line_h);
    }
    main_ppem * scale
}

/// 由字体度量计算等宽单元格尺寸（全部取整到物理像素，保证 1:1 对齐）。
///
/// `letter_spacing_px`：物理像素，加到 cell 宽度上（可负）。
/// `line_height`：行高倍数（1.0 = 字体自身度量）。
///
/// 取整策略：cell 宽度用 **round（四舍五入）**，绝不用 ceil——ceil 会在小字号下
/// 每格偷偷加 0.x 像素，视觉上就是「字距发散」。宽度是等宽网格唯一权威（铁律 4），
/// 忠实主字体自身 advance 是默认行为，letter_spacing 才是用户显式微调入口。
fn compute_cell_metrics(
    font: &FontRef,
    ppem: f32,
    letter_spacing_px: f32,
    line_height: f32,
) -> CellMetrics {
    let m = font.metrics(&[]).scale(ppem);

    // 等宽字体：用 'M' 的 advance 作为 cell 宽度权威。
    let glyph_id = font.charmap().map('M');
    let advance = font.glyph_metrics(&[]).scale(ppem).advance_width(glyph_id);

    // 忠实 advance：round 而非 ceil；再叠加显式字距微调。
    let width = (advance + letter_spacing_px).round().max(1.0) as u32;

    // 行高 = (ascent + descent + leading) × line_height，向上取整保证不裁切。
    let ascent_f = m.ascent;
    let descent_f = m.descent;
    let leading_f = m.leading;
    let base_h = ascent_f + descent_f + leading_f;
    let height = (base_h * line_height).ceil().max(1.0) as u32;

    // 行高放大时，把字形垂直居中（基线相应下移半个增量），避免贴顶。
    let extra = ((base_h * line_height) - base_h).max(0.0) * 0.5;
    let ascent = (ascent_f + extra).round().max(1.0) as u32;

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
