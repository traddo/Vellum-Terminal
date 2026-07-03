# Vellum Terminal (vlt) — 项目指令

纯 Rust 原生终端模拟器。核心命题：**像 PDF 一样在进程内自主光栅化字体，100% 绕开系统文本渲染链路（FreeType/Fontconfig/Xft），实现跨发行版逐像素一致的渲染。**

完整架构与决策依据见 [design.md](design.md)。开始任何实现前先通读该文档，本文件是其不可违背的原则摘要。

## 铁律（违背任何一条前必须停下来向用户确认）

1. **渲染路径零系统依赖**：字形光栅化只允许走 `swash`（纯 Rust）。禁止引入 `freetype-rs`、`fontconfig` 绑定、`font-kit`、`harfbuzz` C 绑定、`cosmic-text`、任何 Electron/Tauri/Web 组件。最终二进制 `ldd` 不得出现 `libfreetype` / `libfontconfig`——这是 CI 级验收项。
2. **不手写 VT 解析器和 PTY 层**：整层复用 `alacritty_terminal`（解析器、Grid、滚动缓冲、选区模型）。发现其能力不够时，先扩展/包装，不重写。
3. **不做 MSDF/SDF**：字形按目标字号 CPU 光栅化进 glyph atlas；字号或 DPR 变化时整体重建 atlas（简单正确优先，不做增量迁移）。此决策已定案（design.md ADR-2），不再重新评估。
4. **网格宽度唯一权威**：单元格宽度只由主字体 cell 尺寸与 `unicode-width` 决定；回退字体（CJK/Emoji/图标）的自身 advance 一律不参与排版，字形在其 1/2 格内居中缩放。宽度判定必须与 `alacritty_terminal` 内部使用的 unicode-width 版本同源。
5. **字体发现不走 Fontconfig**：内嵌默认字体 + `fontdb` 目录扫描；回退链在 TOML 配置中显式声明、顺序确定。

## 技术栈（已定，不再选型）

`winit`（窗口/事件/IME）+ `wgpu`（Vulkan 优先）+ `alacritty_terminal` + `swash` + `rustybuzz` + `fontdb` + `arboard` + `serde`/`toml`。

## 开发顺序

严格按 design.md §8 的三个 Phase 推进，**当前从 Phase 1 (MVP) 开始**。Phase 1 只做：ASCII/Latin + 内嵌 JetBrains Mono + 真彩色 + 光标 + 滚动。CJK、IME、Emoji、连字全部属于 Phase 2/3，MVP 阶段遇到相关诱惑时写 TODO 跳过，不提前实现。

## 验收纪律

- Phase 1 的核心验证是**逐像素一致性**：同一 commit 在不同 fontconfig 配置/不同发行版下截图 diff == 0。实现过程中尽早搭建"渲染一帧 → 导出 PNG"的 headless 截图测试能力，作为回归基线。
- 每个 Phase 的验收标准以 design.md §8 为准，完成后逐条核对并如实报告，未达标项不得标记完成。
- 性能对标基线是 Alacritty（`vtebench`），Phase 3 才关注，前期不做性能优化。

## 沟通

- 始终用简体中文交流（含代码注释）；代码标识符、crate 名、命令、路径保持英文。
- 定位口径注意：vlt 的卖点是"一致性与可控性"，不承诺"低分屏上比精调 FreeType 更清晰"（design.md §1 定位诚实性约束）。
