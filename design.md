# 架构设计：Vellum Terminal (vlt) v2

### 纯 Rust 原生终端 —— PDF 式自主字体光栅化 + wgpu GPU 渲染

> 本文档是 v1（Tauri + Canvas/WebGL/MSDF 构想）的彻底改写版。
> v1 的核心理念保留：**像 PDF 一样，把字体当作纯矢量几何路径在进程内部自主光栅化，100% 绕开宿主系统的文本渲染链路（FreeType / Fontconfig / Xft），使渲染结果只取决于 vlt 自身，与用户系统字体配置完全无关。**
> v1 的实现路线全部推翻：不再使用浏览器/Tauri/Canvas/WASM/MSDF，改为纯 Rust 原生实现。决策依据见 §3。

---

## 1. 项目定位

**一句话：在任何 Linux 机器上——无论其 Fontconfig 配置多么混乱——vlt 呈现的文字都逐像素一致、干净、不发虚，如同 PDF 阅读器打开同一份文件。**

- **目标用户**：Linux（尤其 HiDPI 屏）开发者；被系统字体渲染差异折磨过的用户；CJK 用户。
- **核心卖点**：渲染自主性（视觉净土）+ 跨发行版逐像素一致 + GPU 低延迟。
- **定位诚实性约束**：vlt 的优势主张是"**一致性与可控性**"，而非笼统的"更清晰"。在低分屏（~96 DPI）小字号下，无 hinting 的纯灰度光栅化不保证胜过精调的 FreeType；在 HiDPI 屏上二者差距消失、自主光栅化的一致性优势凸显。宣传与验收口径都以此为准。

## 2. 与既有终端的本质差异

| 维度 | Alacritty / Kitty | WezTerm | Hyper / VS Code (xterm.js) | **vlt** |
| :--- | :--- | :--- | :--- | :--- |
| GPU 渲染 | 有 | 有 | 有（WebGL 贴图） | 有（wgpu） |
| 字形光栅化来源 | 系统 FreeType | **捆绑的** FreeType/HarfBuzz | 浏览器 `ctx.fillText` → 系统 FreeType | **纯 Rust 光栅化器（swash），零 C 依赖** |
| 受系统 Fontconfig 配置影响 | 是 | 字体**发现**受影响，光栅化行为基本不受 | 完全受影响 | **完全隔离**（字体发现与光栅化均自主） |
| 跨发行版逐像素一致 | 否 | 接近 | 否 | **是（硬性验收标准）** |

> 注意：WezTerm 是最接近的先行者（捆绑 FreeType 已实现大半"净土"目标）。vlt 的增量价值在于：① 光栅化器也换成纯 Rust，供应链上彻底去掉 C 图形库；② 字体发现不走 Fontconfig（自带默认字体 + fontdb 目录扫描）；③ 以"逐像素一致"作为可自动化验证的硬指标，而非顺带效果。

## 3. 关键技术决策（ADR）

### ADR-1：纯 Rust 原生（winit + wgpu），放弃 Tauri/Web 技术栈 —— 【已定】

**理由**：核心诉求是"自主光栅化 + 一致性 + 低延迟"，浏览器对此没有任何贡献，反而引入三大额外成本：
1. Canvas 上无原生文本框，**中文输入法（IME）** 需手工处理 composition 事件并叠加 DOM，是 Web 终端公认最痛的部分；原生路线由 winit 的 `Ime` 事件直接覆盖（X11/Wayland、fcitx5/ibus 均可用）。
2. 选区/复制/无障碍需要"透明 DOM 同步层"这种脆弱结构；原生路线自己就是绘制方，选区就是普通的高亮绘制 + `arboard` 剪贴板。
3. WASM 边界、双进程 IPC、Chromium 内存开销，均与"极速轻量（Velocity Light Terminal）"命名相悖。

Web 栈唯一的真实优势（CSS 写 UI、标签页/分屏可定制）不在 MVP 目标内，放弃。

### ADR-2：按字号光栅化 + Glyph Atlas，砍掉 MSDF —— 【已定】

**理由**：MSDF 的强项是任意连续缩放（游戏/3D 场景文字），弱项恰恰是终端主战场——12~16px 小字号，在笔画交叉、尖角处会产生瑕疵、细笔画丢失。终端字号变化是**离散事件**（Ctrl +/-、DPR 变化），触发时整体重建 atlas 即可，代价一次性且极小。Alacritty / Kitty / WezTerm / Ghostty 全部采用"按字号光栅化 + atlas"，无一使用 SDF。**MSDF 从设计中彻底移除，不作为实验分支保留。**

### ADR-3：复用 `alacritty_terminal` 作为 VT 状态机与 PTY 层，绝不手写 —— 【已定】

**理由**：终端转义序列兼容性是数百条规则的长尾泥潭（真彩色、括号粘贴、鼠标协议、备用屏幕、滚动区……），`alacritty_terminal` crate 提供 PTY 管理、ANSI/VT 解析器、屏幕网格（Grid）、滚动缓冲、选区模型，经过海量真实使用验证。vlt 的全部创新集中在**第 3、4 层（字体引擎与渲染）**，第 1、2 层整层复用。

### ADR-4：字体发现不走 Fontconfig；内嵌默认字体 + fontdb 目录扫描 —— 【已定】

**理由**：隔离必须彻底——不仅光栅化不走系统，**字体选择/回退也不受系统配置影响**。方案：
- 二进制内嵌一套默认字体（候选：JetBrains Mono / Sarasa Term SC 或 Noto Sans Mono CJK SC / Noto Color Emoji / Symbols Nerd Font），开箱即得完整覆盖。
- 用户指定字体时接受**文件路径或家族名**，家族名解析走 `fontdb`（直接扫描字体目录，不调用 fontconfig 库）。
- 回退链在 vlt 配置中显式声明、顺序确定，不做系统级"智能"匹配。

## 4. 系统架构

```
+----------------------------------------------------------------------+
| 第 1 层  PTY 与 VT 状态机          [复用 alacritty_terminal]           |
|  - PTY 生成与读写、ANSI/VT 解析、Grid（行列/样式/滚动缓冲/选区模型）      |
+----------------------------------------------------------------------+
              | 网格快照：(单元格字符, 前景/背景色, 样式标志, 宽度)
              v
+----------------------------------------------------------------------+
| 第 2 层  整形与回退层（vlt 自研核心之一）                                |
|  - 按"同字体同样式连续段"切 run → rustybuzz 整形（连字/组合字符）        |
|  - 显式回退链：主字体 → CJK → Emoji → Nerd Symbols → 缺字符 (tofu)      |
|  - CJK 双宽与网格对齐（与第 1 层的 unicode-width 判定保持同源一致）       |
+----------------------------------------------------------------------+
              | (glyph_id, 字体引用, 网格坐标, 亚像素偏移档位)
              v
+----------------------------------------------------------------------+
| 第 3 层  Vellum 字体引擎（vlt 自研核心之二）                             |
|  - swash：读取 TTF/OTF 贝塞尔路径 → 按目标字号 CPU 光栅化 alpha 灰度图    |
|  - Emoji：swash 解码 COLR / 位图彩色字形，走独立 RGBA 通道               |
|  - Glyph Atlas 缓存（shelf packing；字号/DPR 变更时整体重建）            |
|  - 全程零系统调用：无 FreeType、无 Fontconfig、无 Xft                    |
+----------------------------------------------------------------------+
              | atlas 纹理 + 每单元格实例数据
              v
+----------------------------------------------------------------------+
| 第 4 层  wgpu 渲染层                                                   |
|  - 实例化四边形批量绘制：背景色 pass → 灰度字形 pass → 彩色 Emoji pass    |
|  - 光标、选区高亮、下划线/删除线为独立图元                                |
|  - 物理像素 1:1 对齐；DPR（含 Wayland 分数缩放）变化触发 atlas 重建       |
+----------------------------------------------------------------------+

窗口/事件：winit（键盘、鼠标、IME composition、DPI 变更）
剪贴板：arboard        配置：TOML（字体路径、回退链、字号、主题）
```

## 5. 字体引擎设计要点（第 2、3 层）

1. **光栅化**：`swash`（内部基于 `ttf-parser` 读取轮廓、`zeno` 填充），输出 8-bit alpha 位图。无 hinting——这是有意为之的取舍（一致性 > 低分屏微调），见 §1 定位诚实性约束。
2. **亚像素定位**：等宽网格下字形原点按单元格对齐，但为斜体/连字/回退字体度量差异保留 4 档水平亚像素偏移的 atlas 变体（业界通行做法，成本可控）。
3. **整形（shaping）**：`rustybuzz`。终端场景整形范围有限（同一 run 内的连字如 `->` `=>`、组合变音符），按行内连续同样式段切 run，不做段落级布局——**不引入 `cosmic-text`**，其段落布局模型与终端网格模型不匹配，只取其思路不取其依赖。
4. **回退链度量协调**：回退字体（尤其 CJK）的 advance 不参与排版计算——**网格宽度只由主字体的 cell 尺寸与 unicode-width 决定**，回退字形在其占据的 1/2 格内居中缩放。这是保证 tmux/vim 下 CJK 不错位的关键，实现时以此为铁律。
5. **Emoji**：swash 解码 COLR（Noto Color Emoji 的 COLR 版）为分层矢量或直接取位图表，进独立 RGBA atlas，与灰度文字管线分开渲染。CBDT/sbix 位图表按最近字号取图后缩放。
6. **Atlas 管理**：单张或少量 2048² 纹理，shelf packing；字号/DPR 改变→丢弃全部重建（简单正确优先，不做增量迁移）。

## 6. 关键挑战与对策

| 挑战 | 对策 | 难度 |
| :--- | :--- | :--- |
| 中文输入法（IME） | winit `Ime::Enabled/Preedit/Commit` 事件；preedit 文本以下划线样式绘制在光标处；用 `set_ime_cursor_area` 通知候选框位置。X11(fcitx5/ibus) 与 Wayland(text-input-v3) 分别实测 | 高（原生路线下从"极痛"降为"中等"，但仍是验收重点） |
| CJK 双宽对齐 | §5.4 铁律：网格宽度唯一权威是 unicode-width，且与 alacritty_terminal 内部判定同源，避免双方对宽度意见不一 | 中 |
| Emoji | §5.5 独立彩色管线；MVP 阶段可先渲染为 tofu 占位，Phase 2 补齐 | 中 |
| 连字 | rustybuzz 按 run 整形；连字字形跨多格时按 alacritty 同款"首格绘制、后续格空白"策略 | 中 |
| 选区/复制 | 复用 alacritty_terminal 选区模型 + 自绘高亮 + arboard；无需任何 DOM 技巧 | 低 |
| Wayland 分数缩放 | wp_fractional_scale 下按真实物理 DPR 光栅化，禁止先渲染后拉伸 | 中 |
| 无障碍 | 非目标（见 §7）；远期可评估 AccessKit | — |

## 7. 明确不做（Non-Goals）

- **不做** MSDF / SDF 渲染（ADR-2）。
- **不做** 浏览器/Electron/Tauri 壳（ADR-1）。
- **不手写** VT 解析器 / PTY 层（ADR-3）。
- **不做** 标签页、分屏、复用器功能——交给 tmux/zellij；vlt 专注单窗口渲染品质。
- **不做** Windows/macOS 支持（Phase 3 之前）；主战场是 Linux X11 + Wayland。
- **不做** Sixel/Kitty 图形协议、屏幕阅读器无障碍（远期再议）。
- **不追求** 低分屏上胜过精调 FreeType 的显示效果（定位诚实性约束）。

## 8. 分阶段路线图与验收标准

### Phase 1 — MVP（验证核心命题）
- 内容：winit 窗口 + wgpu 管线 + alacritty_terminal 接入 + swash 光栅化 + atlas + ASCII/Latin 渲染 + 16/256/真彩色 + 光标 + 滚动。字体仅内嵌 JetBrains Mono。
- **验收（核心命题验证）**：
  1. 在一台故意写坏 `fontconfig` 配置（如全局强 hinting + BGR 亚像素）的 Linux 机器上，vlt 渲染结果与配置正常机器**逐像素一致**（截图 diff == 0）。
  2. 同一 commit 在两个不同发行版（如 Arch 与 Ubuntu）上截图 diff == 0。
  3. `vim`、`htop`、`ls --color` 显示正常，无网格错位。
  4. `ldd` 输出中不出现 `libfreetype` / `libfontconfig`。

### Phase 2 — 可日用
- 内容：CJK 双宽 + 显式回退链 + 选区/复制粘贴 + IME（fcitx5 @ X11 与 Wayland 实测）+ 连字 + 滚动缓冲翻页 + TOML 配置 + 字号热调整（atlas 重建）。
- 验收:`tmux` + `vim` 中文编辑无错位；fcitx5 输入中文全流程可用；Fira Code 连字正确。

### Phase 3 — 完整产品
- 内容：彩色 Emoji 管线 + Nerd Font 图标 + 超链接（OSC 8）+ 主题系统 + 性能打磨（`vtebench` 对标 Alacritty）+ Wayland 分数缩放实测。

## 9. 依赖清单（供实现阶段锁定版本）

| 用途 | Crate | 备注 |
| :--- | :--- | :--- |
| VT 状态机 + PTY + Grid + 选区 | `alacritty_terminal` | 第 1 层整层复用 |
| 窗口 / 事件 / IME | `winit` | 确认所用版本的 Wayland text-input 支持状况 |
| GPU | `wgpu` | Vulkan 优先，GL 回退 |
| 字形光栅化 | `swash` | 含 ttf-parser + zeno；COLR 支持 |
| 整形 | `rustybuzz` | 纯 Rust HarfBuzz 移植 |
| 字体家族名解析 | `fontdb` | 仅目录扫描，不链接 fontconfig |
| 字符宽度 | `unicode-width` | 须与 alacritty_terminal 内部版本一致 |
| 剪贴板 | `arboard` | X11/Wayland |
| 配置 | `serde` + `toml` | |

**禁止引入**：`freetype-rs`、`fontconfig` 及任何绑定、`font-kit`（默认走系统库）、`harfbuzz` C 绑定、`cosmic-text`（模型不匹配，见 §5.3）、任何 Electron/Web 组件。
