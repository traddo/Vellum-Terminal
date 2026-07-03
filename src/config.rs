//! P2-4：TOML 配置（~/.config/vlt/vlt.toml）。
//!
//! 原则：
//! - 全部字段有缺省值，文件不存在时零配置可用（并生成一份带注释的默认配置）。
//! - 解析失败：清晰报错到 stderr + 回退默认值，绝不 panic。
//! - 字体按「角色」声明（VSCode 式）：latin / cjk / symbols / fallback 链；
//!   latin 是 cell 尺寸权威（铁律 4），最终兜底永远是内嵌 JetBrains Mono（OFL）。
//! - 许可红线：商业字体（Pragmata/PragmataPro / 微软雅黑）只允许运行时从磁盘加载，
//!   绝不 embed 进二进制、绝不 commit 进仓库。

use serde::Deserialize;

use crate::theme::{Palette, Rgb};

/// 一个字体角色：家族名或文件路径 + 可选缩放系数。
#[derive(Clone, Debug)]
pub struct FontRole {
    /// 家族名（fontdb 目录扫描解析）或字体文件绝对路径。
    pub source: String,
    /// 字形光栅缩放系数（乘在自动适配 ppem 上）。
    /// 只影响字形视觉大小，不影响网格 advance（铁律 4 不破）。
    pub scale: f32,
}

/// 字体角色配置汇总。
#[derive(Clone, Debug)]
pub struct FontSpec {
    /// 主字体（Latin），cell 尺寸权威。None = 内嵌 JetBrains Mono。
    pub latin: Option<FontRole>,
    /// CJK 码点专用角色。None = 走 fallback 链。
    pub cjk: Option<FontRole>,
    /// 符号/图标角色（Phase 3 Nerd Font 用，预留）。
    pub symbols: Option<FontRole>,
    /// 显式兜底链（家族名，顺序即优先级）。
    pub fallback: Vec<String>,
}

impl Default for FontSpec {
    fn default() -> Self {
        FontSpec {
            latin: None,
            cjk: None,
            symbols: None,
            fallback: vec!["Sarasa Mono SC".to_string()],
        }
    }
}

/// 运行时配置（已解析、已填缺省）。
#[derive(Clone, Debug)]
pub struct Config {
    pub font: FontSpec,
    /// 逻辑字号（px @ DPR=1）。
    pub font_size: f32,
    /// 窗口内边距（逻辑像素，DPR 换算后生效），(x, y)。
    pub padding: (u32, u32),
    /// 滚动缓冲行数。
    pub scrolling_history: usize,
    /// 主题调色板（可被 [colors] 覆盖）。
    pub palette: Palette,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            font: FontSpec::default(),
            font_size: 15.0,
            padding: (10, 10),
            scrolling_history: 10_000,
            palette: Palette::default(),
        }
    }
}

// ---------- TOML 原始结构（全部 Option，逐字段填缺省） ----------

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct RawConfig {
    font: Option<RawFont>,
    window: Option<RawWindow>,
    colors: Option<RawColors>,
    scrollback: Option<RawScrollback>,
}

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct RawFont {
    latin: Option<String>,
    cjk: Option<String>,
    symbols: Option<String>,
    fallback: Option<Vec<String>>,
    size: Option<f32>,
    /// 各角色缩放（可选表）：latin_scale / cjk_scale / symbols_scale。
    latin_scale: Option<f32>,
    cjk_scale: Option<f32>,
    symbols_scale: Option<f32>,
}

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct RawWindow {
    padding_x: Option<u32>,
    padding_y: Option<u32>,
}

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct RawColors {
    foreground: Option<String>,
    background: Option<String>,
    cursor: Option<String>,
    normal: Option<Vec<String>>,
    bright: Option<Vec<String>>,
}

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct RawScrollback {
    lines: Option<usize>,
}

/// 配置文件路径：~/.config/vlt/vlt.toml。
pub fn config_path() -> Option<std::path::PathBuf> {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(std::path::PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| std::path::PathBuf::from(h).join(".config")))?;
    Some(base.join("vlt").join("vlt.toml"))
}

/// 加载配置。文件不存在 → 生成默认配置文件并返回缺省值；
/// 解析失败 → stderr 报错 + 缺省值（不 panic）。
pub fn load() -> Config {
    let Some(path) = config_path() else {
        return Config::default();
    };

    if !path.exists() {
        // 首次启动：生成带注释的默认配置。
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        if let Err(e) = std::fs::write(&path, DEFAULT_CONFIG_TOML) {
            eprintln!("vlt: 无法写入默认配置 {}: {}", path.display(), e);
        } else {
            eprintln!("vlt: 已生成默认配置 {}", path.display());
        }
        // 生成后按同一路径解析（保证「所写即所读」）。
    }

    let text = match std::fs::read_to_string(&path) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("vlt: 读取配置失败 {}: {}（使用缺省值）", path.display(), e);
            return Config::default();
        }
    };

    match toml::from_str::<RawConfig>(&text) {
        Ok(raw) => resolve(raw),
        Err(e) => {
            eprintln!(
                "vlt: 配置解析失败 {}:\n  {}\n（使用缺省值）",
                path.display(),
                e
            );
            Config::default()
        }
    }
}

/// 把原始 TOML 结构解析成运行时配置（填缺省 + 颜色解析）。
fn resolve(raw: RawConfig) -> Config {
    let mut cfg = Config::default();

    if let Some(f) = raw.font {
        let role = |src: Option<String>, scale: Option<f32>| -> Option<FontRole> {
            let s = src?;
            if s.trim().is_empty() {
                return None;
            }
            Some(FontRole {
                source: s,
                scale: scale.unwrap_or(1.0).clamp(0.5, 2.0),
            })
        };
        cfg.font.latin = role(f.latin, f.latin_scale);
        cfg.font.cjk = role(f.cjk, f.cjk_scale);
        cfg.font.symbols = role(f.symbols, f.symbols_scale);
        if let Some(fb) = f.fallback {
            cfg.font.fallback = fb;
        }
        if let Some(s) = f.size {
            cfg.font_size = s.clamp(6.0, 72.0);
        }
    }

    if let Some(w) = raw.window {
        cfg.padding = (
            w.padding_x.unwrap_or(10).min(200),
            w.padding_y.unwrap_or(10).min(200),
        );
    }

    if let Some(s) = raw.scrollback {
        if let Some(n) = s.lines {
            cfg.scrolling_history = n.min(1_000_000);
        }
    }

    if let Some(c) = raw.colors {
        let parse = |s: &Option<String>, dst: &mut Rgb, what: &str| {
            if let Some(hex) = s {
                match parse_hex(hex) {
                    Some(rgb) => *dst = rgb,
                    None => eprintln!("vlt: colors.{} 非法颜色 {:?}（忽略）", what, hex),
                }
            }
        };
        parse(&c.foreground, &mut cfg.palette.foreground, "foreground");
        parse(&c.background, &mut cfg.palette.background, "background");
        parse(&c.cursor, &mut cfg.palette.cursor, "cursor");
        let parse_arr = |arr: &Option<Vec<String>>, dst: &mut [Rgb; 8], what: &str| {
            if let Some(list) = arr {
                for (i, hex) in list.iter().take(8).enumerate() {
                    match parse_hex(hex) {
                        Some(rgb) => dst[i] = rgb,
                        None => eprintln!("vlt: colors.{}[{}] 非法颜色 {:?}（忽略）", what, i, hex),
                    }
                }
            }
        };
        parse_arr(&c.normal, &mut cfg.palette.normal, "normal");
        parse_arr(&c.bright, &mut cfg.palette.bright, "bright");
    }

    cfg
}

/// 解析 `#RRGGBB` / `RRGGBB` 十六进制颜色。
fn parse_hex(s: &str) -> Option<Rgb> {
    let h = s.trim().trim_start_matches('#');
    if h.len() != 6 || !h.chars().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    let r = u8::from_str_radix(&h[0..2], 16).ok()?;
    let g = u8::from_str_radix(&h[2..4], 16).ok()?;
    let b = u8::from_str_radix(&h[4..6], 16).ok()?;
    Some(Rgb::new(r, g, b))
}

/// 首次启动生成的默认配置（本机默认：Pragmata + 微软雅黑 + Sarasa 兜底）。
///
/// 注意：这些商业字体只在运行时按家族名从磁盘解析；任何角色未命中时
/// 自动退到 fallback 链，最终兜底是内嵌 JetBrains Mono，零配置也可用。
const DEFAULT_CONFIG_TOML: &str = r##"# Vellum Terminal 配置
# 全部字段可省略；省略时用内置缺省值（内嵌 JetBrains Mono + Sarasa 兜底）。

[font]
# 各角色可填「家族名」（按字体目录扫描解析，不走 fontconfig）或字体文件绝对路径。
# 家族名按精确匹配（"Pragmata" 不会误匹配 "PragmataPro"）。
# 注意：Pragmata 仅有 Medium 一个字面，bold/italic 暂用同一字面渲染。
latin = "Pragmata"             # 主字体：cell 尺寸权威
cjk = "Microsoft YaHei"        # CJK 码点路由到此
symbols = ""                    # 图标/符号（Phase 3 预留，空 = 跳过）
fallback = ["Sarasa Mono SC"]  # 角色未命中后的显式兜底链（顺序即优先级）
size = 15                       # 逻辑字号（px @ DPR=1）

# 各角色字形缩放（只缩字形视觉大小，不改网格）：
# cjk_scale：雅黑在 PragmataPro 窄格下的视觉补偿，1.0 = 纯自动适配。
cjk_scale = 1.1

[window]
padding_x = 10                  # 窗口内边距（逻辑像素）
padding_y = 10

[scrollback]
lines = 10000

# [colors]                      # 主题色覆盖（可选，#RRGGBB）
# foreground = "#1A1A1A"
# background = "#FFFFFF"
# cursor = "#1A1A1A"
# normal = ["#2B2B2B", "#C0392B", "#1E7D32", "#9A6700", "#1A5FB4", "#8250DF", "#0E7490", "#5C5C5C"]
# bright = ["#555555", "#A82C1F", "#186628", "#8A5A00", "#155099", "#6F3FC4", "#0B6078", "#1A1A1A"]
"##;
