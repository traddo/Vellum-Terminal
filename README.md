# Vellum Terminal (vlt)

**纯 Rust 原生终端模拟器 —— 像 PDF 一样在进程内自主光栅化字体，100% 绕开系统文本渲染链路（FreeType / Fontconfig / Xft），实现跨发行版逐像素一致的渲染。**

在任何 Linux 机器上——无论其 Fontconfig 配置多么混乱——vlt 呈现的文字都逐像素一致、干净、不发虚，如同 PDF 阅读器打开同一份文件。

> 完整架构与技术决策见 [design.md](design.md)。

## 定位

- **核心卖点**：渲染自主性（视觉净土）+ 跨发行版逐像素一致 + GPU 低延迟。
- **技术栈**：`winit`（窗口/事件/IME）+ `wgpu`（Vulkan 优先）+ `alacritty_terminal`（VT 状态机/PTY/Grid/选区，整层复用）+ `swash`（纯 Rust 字形光栅化）+ `fontdb`（目录扫描，不走 Fontconfig）+ `arboard`（剪贴板）。
- **零系统文本渲染依赖**：最终二进制 `ldd` 不出现 `libfreetype` / `libfontconfig`（CI 级验收项，见 `scripts/check_deps.sh`）。
- **定位诚实性约束**：vlt 的优势是「**一致性与可控性**」，而非笼统的「更清晰」。在低分屏（~96 DPI）小字号下，无 hinting 的纯灰度光栅化不保证胜过精调的 FreeType；在 HiDPI 屏上二者差距消失，自主光栅化的一致性优势凸显。

## 构建与运行

需要 Rust 1.85+（edition 2021）。

```bash
# 构建
cargo build --release

# 运行（真实 shell 窗口）
./target/release/vlt

# 无系统字体渲染依赖验收（铁律 1）
bash scripts/check_deps.sh

# headless 快照测试（跨机逐像素一致的回归基线）
cargo test --release
```

首次启动会在 `~/.config/vlt/vlt.toml` 自动生成带中文注释的默认配置；无配置文件也可零配置运行（用内嵌 JetBrains Mono）。

启动时向 stderr 打印各字体角色的解析结果（角色 → 请求家族 → 实际命中文件 → ppem），便于确认字体是否按预期加载。

## 已实现能力（Phase 1 + Phase 2）

- ASCII / Latin / 真彩色（16/256/truecolor）、光标、滚动缓冲翻页
- **CJK 双宽**与网格对齐（宽度唯一权威 = 主字体 cell + `unicode-width`，回退字形在 1/2 格内居中，不参与排版）
- **角色制字体链**：latin（cell 尺寸权威）→ cjk → symbols → fallback → 内嵌兜底；家族名走 fontdb 精确匹配或字体文件绝对路径
- **选区 / 复制粘贴**：拖选、双击选词、三击选行；Ctrl+Shift+C/V、中键粘贴；淡蓝纸感高亮、文字保持墨色
- **TOML 配置**：字体、字号、中英视觉比例、字间距、行间距五类旋钮全可配（重启生效）
- **字号热调**：Ctrl+= / Ctrl+- / Ctrl+0（atlas 重建 + 网格/padding 重算）
- **锐度包**：stem darkening（`text_contrast`，抬升无 hinting 细笔画在白底的覆盖率）+ 亚像素抗锯齿（`text_aa`，横向 3× 过采样 + LCD filter，跨机确定不破逐像素一致性）
- **GPU 空闲**：损伤驱动重绘 + `PresentMode::Fifo`，静置增量 GPU 利用率 ≈ 0%，失焦停闪、彻底挂起
- **IME 预留**：winit 已启用 IME，Commit 直接上屏（拉丁/兜底可用）

`tests/snapshots/` 下有各场景的渲染快照（`basic` / `cjk` / `cursor` / `ls_color` / `vim` / `htop` / `gamma_on|off` / `window_live_shell`），既是回归基线也是效果展示。

## 配置文件说明

`~/.config/vlt/vlt.toml`（首启自动生成，全部字段可省略；解析出错时清晰报错并回退缺省，不崩溃）：

```toml
[font]
# 各角色填「家族名」（按字体目录扫描解析，不走 fontconfig）或字体文件【绝对路径】。
# 家族名精确匹配（"Pragmata" 不会误匹配 "PragmataPro"）。
latin = "Pragmata"             # 主字体：拉丁/数字，cell 尺寸唯一权威
cjk = "Microsoft YaHei"        # CJK 路由到此
symbols = ""                    # 图标/符号（预留，空 = 跳过）
fallback = ["Sarasa Mono SC"]  # 兜底链（顺序即优先级）

size = 15                       # 逻辑字号（px @ DPR=1）。建议 11~20
cjk_scale = 0.92                # CJK 相对拉丁的视觉比例。建议 0.88~1.05（0.92 中英最协调）
letter_spacing = 0              # 字间距（逻辑像素，可负）。建议 -1.0~2.0
line_height = 1.0               # 行高倍数。建议 1.0~1.4

text_contrast = 0.30            # stem darkening（0=关）。建议 0.2~0.4
text_aa = "grayscale"           # "grayscale"（默认，无彩边）/ "subpixel-rgb" / "subpixel-bgr"

[window]
padding_x = 10                  # 窗口内边距-左右（逻辑像素）
padding_y = 10                  # 窗口内边距-上下

[scrollback]
lines = 10000                   # 滚动缓冲行数

# [colors]                      # 主题色覆盖（可选，#RRGGBB），略
```

字体发现只扫描：`~/.local/share/fonts`、`~/.fonts`、`/usr/local/share/fonts`、`/usr/share/fonts`（固定顺序，不读 fontconfig 配置）。

## 已知限制

- **IME 未完整实现**：winit 的 IME 事件已启用、Commit 文本会直接写入 PTY（输入法上屏可用），但 preedit（候选串下划线绘制）与 `set_ime_cursor_area`（候选框定位）**尚未实现**，中文输入法的完整交互体验待补。
- **彩色 Emoji / Nerd Font 图标 / OSC 8 超链接 / 主题切换 / 连字**：属规划中的 Phase 3，当前未实现（缺字符渲染为 tofu 空心占位）。
- **Wayland 分数缩放**：未在真实 Wayland 分数缩放环境实测（开发环境为 X11）。
- **平台**：仅 Linux（X11 已实测，Wayland 路径存在但未充分验证）；不支持 Windows / macOS。
- **低分屏观感**：无 hinting 是有意取舍（一致性 > 低分屏微调），96 DPI 小字号不承诺胜过精调 FreeType，见上文定位诚实性约束。

## 许可

Apache-2.0。内嵌字体为 JetBrains Mono（OFL）。运行时磁盘加载的商业字体（如 Pragmata、微软雅黑）不随二进制分发，仅由用户本机提供。
