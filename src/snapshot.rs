//! 网格快照：把 alacritty_terminal 的 `renderable_content()` 转成渲染层直接消费的
//! 扁平结构（每单元格字符 + 已解析前景/背景 RGB + 样式 + 光标信息）。
//!
//! 这是第 1 层（VT/Grid）与第 4 层（wgpu）之间的边界。渲染层不依赖 alacritty 类型。

use alacritty_terminal::term::cell::Flags;
use alacritty_terminal::term::{Term, TermMode};
use alacritty_terminal::event::EventListener;
use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::vte::ansi::CursorShape;

use crate::color_resolve::{resolve_bg, resolve_fg};
use crate::theme::{Palette, Rgb};

/// 单个可见单元格的渲染信息。
#[derive(Clone, Copy, Debug)]
pub struct SnapCell {
    /// 网格列（0 基，可见区）。
    pub col: usize,
    /// 网格行（0 基，可见区，顶部为 0）。
    pub line: usize,
    pub ch: char,
    pub fg: Rgb,
    pub bg: Rgb,
    pub bold: bool,
    pub italic: bool,
    pub underline: bool,
    pub strikeout: bool,
    /// 宽字符的后半格占位（不绘制字形，仅继承背景）。
    pub wide_spacer: bool,
}

/// 光标快照。
#[derive(Clone, Copy, Debug)]
pub struct SnapCursor {
    pub col: usize,
    pub line: usize,
    pub visible: bool,
    pub shape: SnapCursorShape,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SnapCursorShape {
    Block,
    Beam,
    Underline,
    HollowBlock,
    Hidden,
}

/// 整帧快照。
pub struct GridSnapshot {
    pub cols: usize,
    pub lines: usize,
    pub cells: Vec<SnapCell>,
    pub cursor: SnapCursor,
    /// 默认背景（纸白），用于清屏与空单元格。
    pub default_bg: Rgb,
    /// 回滚偏移（>0 表示正在查看历史，渲染层据此画回滚指示，P2-3）。
    pub display_offset: usize,
}

impl GridSnapshot {
    /// 从终端状态与调色板生成快照。
    pub fn capture<T: EventListener>(term: &Term<T>, palette: &Palette) -> Self {
        let cols = term.columns();
        let lines = term.screen_lines();

        let content = term.renderable_content();
        let display_offset = content.display_offset as i32;
        let selection = content.selection;

        let cursor_point = content.cursor.point;
        let cursor_shape = content.cursor.shape;
        let mode = content.mode;

        let mut cells = Vec::with_capacity(cols * lines);

        for indexed in content.display_iter {
            let point = indexed.point;
            let cell = indexed.cell;

            // 把带 display_offset 的 line 映射回可见区 0..lines。
            let visible_line = point.line.0 + display_offset;
            if visible_line < 0 || visible_line as usize >= lines {
                continue;
            }
            let line = visible_line as usize;
            let col = point.column.0;

            let flags = cell.flags;

            // 宽字符占位符（后半格）：不画字形，只保留背景。
            let wide_spacer = flags.contains(Flags::WIDE_CHAR_SPACER)
                || flags.contains(Flags::LEADING_WIDE_CHAR_SPACER);

            let mut fg = resolve_fg(cell.fg, palette);
            let mut bg = resolve_bg(cell.bg, palette);

            // INVERSE：前后景对调（反显）。
            if flags.contains(Flags::INVERSE) {
                std::mem::swap(&mut fg, &mut bg);
            }

            // HIDDEN：前景设为背景色（不可见）。
            if flags.contains(Flags::HIDDEN) {
                fg = bg;
            }

            // P2-2 选区高亮：淡蓝纸感底色，文字保持墨色不反白。
            // （宽字符尾随占位格也纳入判定，避免选中 CJK 时后半格漏色。）
            if let Some(range) = &selection {
                let in_selection = range.contains(point)
                    || (flags.contains(Flags::WIDE_CHAR)
                        && range.contains(alacritty_terminal::index::Point::new(
                            point.line,
                            point.column + 1,
                        )));
                if in_selection {
                    bg = palette.selection;
                }
            }

            let ch = cell.c;

            cells.push(SnapCell {
                col,
                line,
                ch,
                fg,
                bg,
                bold: flags.contains(Flags::BOLD),
                italic: flags.contains(Flags::ITALIC),
                underline: flags.intersects(Flags::ALL_UNDERLINES),
                strikeout: flags.contains(Flags::STRIKEOUT),
                wide_spacer,
            });
        }

        // 光标：映射到可见区。
        let cursor_visible = mode.contains(TermMode::SHOW_CURSOR)
            && cursor_shape != CursorShape::Hidden;
        let cursor_visible_line = cursor_point.line.0 + display_offset;
        let cursor = SnapCursor {
            col: cursor_point.column.0,
            line: cursor_visible_line.max(0) as usize,
            visible: cursor_visible
                && cursor_visible_line >= 0
                && (cursor_visible_line as usize) < lines,
            shape: match cursor_shape {
                CursorShape::Block => SnapCursorShape::Block,
                CursorShape::Beam => SnapCursorShape::Beam,
                CursorShape::Underline => SnapCursorShape::Underline,
                CursorShape::HollowBlock => SnapCursorShape::HollowBlock,
                CursorShape::Hidden => SnapCursorShape::Hidden,
            },
        };

        GridSnapshot {
            cols,
            lines,
            cells,
            cursor,
            default_bg: palette.background,
            display_offset: display_offset.max(0) as usize,
        }
    }
}
