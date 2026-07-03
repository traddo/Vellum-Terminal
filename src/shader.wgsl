// Vellum Terminal 渲染 shader。
//
// 视觉硬要求：gamma 校正的 alpha 混合。
// 深色文字叠白底时，若在 sRGB 非线性空间直接按 alpha 线性插值（naive），
// 笔画会显得过重/发糊。正确做法：把前景/背景先转到「线性光」空间，
// 在线性空间按覆盖率 alpha 混合，再转回 sRGB 显示。
//
// 本 shader 有一个 uniform 开关 `gamma_correct`，用于导出「开/关」对比截图。

struct Globals {
    // 视口物理像素尺寸。
    screen_size: vec2<f32>,
    // 1.0 = 开启 gamma 校正；0.0 = naive sRGB 线性混合（仅用于对比截图）。
    gamma_correct: f32,
    _pad: f32,
};

@group(0) @binding(0) var<uniform> globals: Globals;
@group(0) @binding(1) var atlas_tex: texture_2d<f32>;
@group(0) @binding(2) var atlas_samp: sampler;

// ---- sRGB <-> 线性光 转换（IEC 61966-2-1 标准分段函数）----

fn srgb_to_linear(c: vec3<f32>) -> vec3<f32> {
    let cutoff = vec3<f32>(0.04045);
    let lo = c / 12.92;
    let hi = pow((c + vec3<f32>(0.055)) / 1.055, vec3<f32>(2.4));
    return select(hi, lo, c <= cutoff);
}

fn linear_to_srgb(c: vec3<f32>) -> vec3<f32> {
    let cutoff = vec3<f32>(0.0031308);
    let lo = c * 12.92;
    let hi = 1.055 * pow(c, vec3<f32>(1.0 / 2.4)) - vec3<f32>(0.055);
    return select(hi, lo, c <= cutoff);
}

// ============================================================
// 背景 pass：每单元格一个实例，填充纯色矩形（含选区/光标底）。
// ============================================================

struct BgInstance {
    @location(0) pos: vec2<f32>,   // 左上角（物理像素）
    @location(1) size: vec2<f32>,  // 宽高（物理像素）
    @location(2) color: vec3<f32>, // sRGB 非线性
};

struct BgVsOut {
    @builtin(position) clip: vec4<f32>,
    @location(0) color: vec3<f32>,
};

// 单位四边形的 6 个顶点（两个三角形）。
fn quad_corner(vi: u32) -> vec2<f32> {
    var corners = array<vec2<f32>, 6>(
        vec2<f32>(0.0, 0.0), vec2<f32>(1.0, 0.0), vec2<f32>(0.0, 1.0),
        vec2<f32>(0.0, 1.0), vec2<f32>(1.0, 0.0), vec2<f32>(1.0, 1.0),
    );
    return corners[vi];
}

// 物理像素坐标 -> NDC（clip space）。
fn px_to_clip(px: vec2<f32>) -> vec4<f32> {
    let ndc = vec2<f32>(
        px.x / globals.screen_size.x * 2.0 - 1.0,
        1.0 - px.y / globals.screen_size.y * 2.0,
    );
    return vec4<f32>(ndc, 0.0, 1.0);
}

@vertex
fn vs_bg(@builtin(vertex_index) vi: u32, inst: BgInstance) -> BgVsOut {
    let corner = quad_corner(vi);
    let px = inst.pos + corner * inst.size;
    var out: BgVsOut;
    out.clip = px_to_clip(px);
    out.color = inst.color;
    return out;
}

@fragment
fn fs_bg(in: BgVsOut) -> @location(0) vec4<f32> {
    // 背景不透明。surface 为非 sRGB 格式，故这里直接输出 sRGB 非线性值。
    return vec4<f32>(in.color, 1.0);
}

// ============================================================
// 字形 pass：每字形一个实例，采样 atlas 的 alpha 覆盖率，
// 与「其单元格背景」做 gamma 正确混合。
// ============================================================

struct GlyphInstance {
    @location(0) pos: vec2<f32>,      // 字形位图左上角（物理像素）
    @location(1) size: vec2<f32>,     // 字形位图宽高（物理像素）
    @location(2) uv_min: vec2<f32>,   // atlas 归一化 UV 左上
    @location(3) uv_max: vec2<f32>,   // atlas 归一化 UV 右下
    @location(4) fg: vec3<f32>,       // 前景色 sRGB 非线性
    @location(5) bg: vec3<f32>,       // 该单元格背景 sRGB 非线性（用于线性混合）
};

struct GlyphVsOut {
    @builtin(position) clip: vec4<f32>,
    @location(0) uv: vec2<f32>,
    @location(1) fg: vec3<f32>,
    @location(2) bg: vec3<f32>,
};

@vertex
fn vs_glyph(@builtin(vertex_index) vi: u32, inst: GlyphInstance) -> GlyphVsOut {
    let corner = quad_corner(vi);
    let px = inst.pos + corner * inst.size;
    let uv = mix(inst.uv_min, inst.uv_max, corner);
    var out: GlyphVsOut;
    out.clip = px_to_clip(px);
    out.uv = uv;
    out.fg = inst.fg;
    out.bg = inst.bg;
    return out;
}

@fragment
fn fs_glyph(in: GlyphVsOut) -> @location(0) vec4<f32> {
    // atlas 是 RGBA8「每通道覆盖率」：
    // 灰度 AA 时 R=G=B（三通道同值 → 等价于旧的单覆盖率路径）；
    // 亚像素 AA 时 R/G/B 为三个子像素各自覆盖率（逐通道独立混合，防彩边靠 CPU 端 LCD filter）。
    let cov = textureSample(atlas_tex, atlas_samp, in.uv).rgb;

    if (globals.gamma_correct > 0.5) {
        // 正确路径：在线性光空间按「逐通道覆盖率」混合，再转回 sRGB。
        let fg_lin = srgb_to_linear(in.fg);
        let bg_lin = srgb_to_linear(in.bg);
        let mixed_lin = mix(bg_lin, fg_lin, cov); // cov 为 vec3，逐通道 mix
        let out_srgb = linear_to_srgb(mixed_lin);
        return vec4<f32>(out_srgb, 1.0);
    } else {
        // 对照路径：直接在 sRGB 非线性空间混合（naive，笔画偏重发糊）。
        let mixed = mix(in.bg, in.fg, cov);
        return vec4<f32>(mixed, 1.0);
    }
}

// ============================================================
// 装饰线 pass（下划线/删除线）：复用背景实例格式，纯色矩形。
// 与 vs_bg/fs_bg 共用即可，无需单独入口。
// ============================================================
