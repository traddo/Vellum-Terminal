//! Vellum Paper 主题 —— 唯一主题，PDF 白纸风格。
//!
//! 设计约束（硬验收）：
//! - 背景纯白 #FFFFFF，正文墨黑 #1A1A1A。
//! - ANSI 16 色针对白底重调：normal 8 色对白底对比度 ≥ 4.5:1，
//!   杜绝白底亮黄/亮青这种不可读组合。
//! - 所有颜色以 sRGB 8-bit 存储；线性化与 gamma 校正在 shader 里做。

/// sRGB 颜色（8-bit 分量），渲染前会被 shader 线性化。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Rgb {
    pub r: u8,
    pub g: u8,
    pub b: u8,
}

impl Rgb {
    pub const fn new(r: u8, g: u8, b: u8) -> Self {
        Rgb { r, g, b }
    }

    /// 归一化到 [0,1] 的 sRGB（非线性）分量，供顶点/实例数据上传。
    pub fn to_srgb_f32(self) -> [f32; 3] {
        [
            self.r as f32 / 255.0,
            self.g as f32 / 255.0,
            self.b as f32 / 255.0,
        ]
    }
}

/// Vellum Paper 调色板。
#[derive(Clone, Debug)]
pub struct Palette {
    /// 纸面背景（纯白）。
    pub background: Rgb,
    /// 正文前景（墨黑）。
    pub foreground: Rgb,
    /// 光标颜色（墨色 block）。
    pub cursor: Rgb,
    /// 选区高亮底色（淡蓝纸感，文字保持墨色不反白，P2-2）。
    pub selection: Rgb,
    /// ANSI normal 8 色（index 0..=7）。
    pub normal: [Rgb; 8],
    /// ANSI bright 8 色（index 8..=15）。
    pub bright: [Rgb; 8],
}

impl Default for Palette {
    fn default() -> Self {
        // 锚点参考来自项目视觉要求，针对白底重调，normal 8 色对白底对比度均 ≥ 4.5:1。
        Palette {
            background: Rgb::new(0xFF, 0xFF, 0xFF),
            foreground: Rgb::new(0x1A, 0x1A, 0x1A),
            cursor: Rgb::new(0x1A, 0x1A, 0x1A),
            selection: Rgb::new(0xB4, 0xD5, 0xFE), // 淡蓝纸感，非饱和蓝
            normal: [
                Rgb::new(0x2B, 0x2B, 0x2B), // black  -> 深灰墨（对比度极高）
                Rgb::new(0xC0, 0x39, 0x2B), // red
                Rgb::new(0x1E, 0x7D, 0x32), // green
                Rgb::new(0x9A, 0x67, 0x00), // yellow -> 暗金而非亮黄
                Rgb::new(0x1A, 0x5F, 0xB4), // blue
                Rgb::new(0x82, 0x50, 0xDF), // magenta
                Rgb::new(0x0E, 0x74, 0x90), // cyan   -> 暗青
                Rgb::new(0x5C, 0x5C, 0x5C), // white  -> 中灰（白底上的“亮白”实为浅墨）
            ],
            bright: [
                Rgb::new(0x55, 0x55, 0x55), // bright black -> 灰
                Rgb::new(0xA8, 0x2C, 0x1F), // bright red   -> 加深
                Rgb::new(0x18, 0x66, 0x28), // bright green -> 加深
                Rgb::new(0x8A, 0x5A, 0x00), // bright yellow-> 更暗金
                Rgb::new(0x15, 0x50, 0x99), // bright blue  -> 加深
                Rgb::new(0x6F, 0x3F, 0xC4), // bright magenta
                Rgb::new(0x0B, 0x60, 0x78), // bright cyan  -> 加深
                Rgb::new(0x1A, 0x1A, 0x1A), // bright white -> 墨黑（白底上最“亮”即最深的正文色）
            ],
        }
    }
}

impl Palette {
    /// 解析 ANSI 16 色索引（0..16）到 RGB。
    pub fn ansi16(&self, idx: u8) -> Rgb {
        if idx < 8 {
            self.normal[idx as usize]
        } else if idx < 16 {
            self.bright[(idx - 8) as usize]
        } else {
            self.foreground
        }
    }

    /// 解析 256 色调色板索引到 RGB。
    ///
    /// - 0..16：ANSI 16 色（走白底重调后的表）。
    /// - 16..232：6×6×6 色立方。
    /// - 232..256：灰阶。
    ///
    /// 注意：白底风格下不对 16..256 做逐色重映射（那是 Phase 2+ 的主题工程），
    /// 但色立方与灰阶按标准 xterm 公式生成，保证程序输出的 256 色可辨识。
    pub fn ansi256(&self, idx: u8) -> Rgb {
        match idx {
            0..=15 => self.ansi16(idx),
            16..=231 => {
                let i = idx - 16;
                let r = i / 36;
                let g = (i % 36) / 6;
                let b = i % 6;
                // xterm 标准：0 -> 0，其余 -> 55 + n*40。
                let conv = |n: u8| -> u8 {
                    if n == 0 {
                        0
                    } else {
                        55 + n * 40
                    }
                };
                Rgb::new(conv(r), conv(g), conv(b))
            }
            232..=255 => {
                let level = 8 + (idx - 232) * 10;
                Rgb::new(level, level, level)
            }
        }
    }
}
