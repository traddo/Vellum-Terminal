//! Vellum Terminal (vlt) —— 纯 Rust 原生终端，PDF 式自主字体光栅化。
//!
//! 模块分层对应 design.md §4：
//! - `terminal`：第 1 层，alacritty_terminal 封装（PTY/VT/Grid）。
//! - `snapshot`：第 1↔4 层边界，把 Grid 快照成渲染数据。
//! - `font`：第 3 层，swash 光栅化 + glyph atlas。
//! - `render` / `gpu` / `headless`：第 4 层，wgpu 渲染与离屏截图。
//! - `theme` / `color_resolve`：Vellum Paper 调色板与颜色解析。

pub mod color_resolve;
pub mod font;
pub mod gpu;
pub mod headless;
pub mod keymap;
pub mod render;
pub mod snapshot;
pub mod terminal;
pub mod theme;

/// 逻辑字号（pt/px @ DPR=1）。物理 ppem = 该值 × DPR，取整后交给字体引擎。
pub const DEFAULT_FONT_SIZE: f32 = 15.0;
