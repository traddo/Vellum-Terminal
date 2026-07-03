//! 颜色解析：把 alacritty_terminal（vte）的 `Color` 解析为 Vellum Paper 调色板下的 RGB。
//!
//! 铁律相关：alacritty 的 cell.fg/bg 是「未解析」的 `Color`（Named/Spec/Indexed），
//! 必须在 vlt 侧按我们自己的白底调色板解析，而非用系统/默认色。

use alacritty_terminal::vte::ansi::{Color, NamedColor};

use crate::theme::{Palette, Rgb};

/// 把 vte 的 `NamedColor` 归一到 0..16 的 ANSI 索引（bright 段映射到 8..16）。
/// 返回 None 表示是前景/背景/光标/dim 等「特殊」名，交由调用方按语义处理。
fn named_to_ansi16(n: NamedColor) -> Option<u8> {
    use NamedColor::*;
    Some(match n {
        Black => 0,
        Red => 1,
        Green => 2,
        Yellow => 3,
        Blue => 4,
        Magenta => 5,
        Cyan => 6,
        White => 7,
        BrightBlack => 8,
        BrightRed => 9,
        BrightGreen => 10,
        BrightYellow => 11,
        BrightBlue => 12,
        BrightMagenta => 13,
        BrightCyan => 14,
        BrightWhite => 15,
        // 特殊语义色，另行处理。
        _ => return None,
    })
}

/// 解析前景色。`is_fg = true`。
pub fn resolve_fg(color: Color, palette: &Palette) -> Rgb {
    resolve(color, palette, true)
}

/// 解析背景色。`is_fg = false`。
pub fn resolve_bg(color: Color, palette: &Palette) -> Rgb {
    resolve(color, palette, false)
}

fn resolve(color: Color, palette: &Palette, is_fg: bool) -> Rgb {
    match color {
        // 真彩色：直接采用（真彩色是应用显式指定，尊重之）。
        Color::Spec(rgb) => Rgb::new(rgb.r, rgb.g, rgb.b),
        // 调色板索引：走白底重调后的 256 色表。
        Color::Indexed(i) => palette.ansi256(i),
        Color::Named(n) => {
            if let Some(idx) = named_to_ansi16(n) {
                palette.ansi16(idx)
            } else {
                // 特殊名：前景/背景/光标/dim。
                match n {
                    NamedColor::Foreground | NamedColor::BrightForeground => palette.foreground,
                    NamedColor::Background => palette.background,
                    NamedColor::Cursor => palette.cursor,
                    // Dim 前景在白底上仍用正文墨色（避免发灰不可读）。
                    NamedColor::DimForeground => palette.foreground,
                    // Dim 具体色：退回对应 normal 色（白底下不再压暗，保证可读）。
                    NamedColor::DimBlack => palette.normal[0],
                    NamedColor::DimRed => palette.normal[1],
                    NamedColor::DimGreen => palette.normal[2],
                    NamedColor::DimYellow => palette.normal[3],
                    NamedColor::DimBlue => palette.normal[4],
                    NamedColor::DimMagenta => palette.normal[5],
                    NamedColor::DimCyan => palette.normal[6],
                    NamedColor::DimWhite => palette.normal[7],
                    // 兜底。
                    _ => {
                        if is_fg {
                            palette.foreground
                        } else {
                            palette.background
                        }
                    }
                }
            }
        }
    }
}
