//! 键盘输入 → PTY 字节编码。
//!
//! Phase 1 覆盖：可打印字符、Enter/Backspace/Tab/Esc、方向键、Home/End/PageUp/Down、
//! Delete、以及 Ctrl+字母（生成控制字符）。
//! IME/组合输入属于 Phase 2，此处不处理（winit `Ime` 事件暂忽略）。

use winit::keyboard::{Key, ModifiersState, NamedKey};

/// 把一次按键（logical_key + 修饰键 + text）编码为要写入 PTY 的字节。
/// 返回空表示该按键不产生输入。
pub fn encode_key(key: &Key, text: Option<&str>, mods: ModifiersState) -> Vec<u8> {
    let ctrl = mods.control_key();

    // Ctrl + 字母/符号 → 控制字符。
    if ctrl {
        if let Key::Character(s) = key {
            if let Some(c) = s.chars().next() {
                let lower = c.to_ascii_lowercase();
                if lower.is_ascii_lowercase() {
                    // Ctrl-A..Ctrl-Z → 0x01..0x1A
                    return vec![(lower as u8 - b'a') + 1];
                }
                match c {
                    '[' => return vec![0x1b],
                    '\\' => return vec![0x1c],
                    ']' => return vec![0x1d],
                    '^' => return vec![0x1e],
                    '_' => return vec![0x1f],
                    ' ' => return vec![0x00],
                    _ => {}
                }
            }
        }
    }

    match key {
        Key::Named(named) => match named {
            NamedKey::Enter => vec![b'\r'],
            NamedKey::Backspace => vec![0x7f],
            NamedKey::Tab => vec![b'\t'],
            NamedKey::Escape => vec![0x1b],
            NamedKey::Space => vec![b' '],
            // 方向键（DECCKM 正常模式的 CSI 序列）。
            NamedKey::ArrowUp => b"\x1b[A".to_vec(),
            NamedKey::ArrowDown => b"\x1b[B".to_vec(),
            NamedKey::ArrowRight => b"\x1b[C".to_vec(),
            NamedKey::ArrowLeft => b"\x1b[D".to_vec(),
            NamedKey::Home => b"\x1b[H".to_vec(),
            NamedKey::End => b"\x1b[F".to_vec(),
            NamedKey::PageUp => b"\x1b[5~".to_vec(),
            NamedKey::PageDown => b"\x1b[6~".to_vec(),
            NamedKey::Delete => b"\x1b[3~".to_vec(),
            NamedKey::Insert => b"\x1b[2~".to_vec(),
            _ => Vec::new(),
        },
        Key::Character(_) => {
            // 普通可打印字符：用 winit 给的 text（已应用 shift/layout）。
            if let Some(t) = text {
                t.as_bytes().to_vec()
            } else {
                Vec::new()
            }
        }
        _ => Vec::new(),
    }
}
