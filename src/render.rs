//! 第 4 层：wgpu 渲染层。
//!
//! 设计：与「窗口/离屏」无关的纯渲染器 —— 输入一个 `GridSnapshot` + 一个目标
//! `TextureView`，产出一帧。窗口路径与 headless 截图路径共用同一个 `Renderer`，
//! 保证逐像素一致（headless 测出的即窗口渲染的）。
//!
//! 三个 pass：背景色 → 装饰线（下划线/删除线）→ 灰度字形。
//! gamma 校正在字形 pass 的 fragment shader 内做（见 shader.wgsl）。

use bytemuck::{Pod, Zeroable};
use wgpu::util::DeviceExt;

use crate::font::FontEngine;
use crate::snapshot::{GridSnapshot, SnapCursorShape};
use crate::theme::Rgb;

/// 目标 surface 使用的像素格式。选 **非 sRGB** 的 `Rgba8Unorm`：
/// 我们在 shader 里手动做线性/非线性转换，若再让硬件 sRGB 写入会二次转换。
pub const TARGET_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Rgba8Unorm;

#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
struct Globals {
    screen_size: [f32; 2],
    gamma_correct: f32,
    _pad: f32,
}

#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
struct BgInstance {
    pos: [f32; 2],
    size: [f32; 2],
    color: [f32; 3],
}

#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
struct GlyphInstance {
    pos: [f32; 2],
    size: [f32; 2],
    uv_min: [f32; 2],
    uv_max: [f32; 2],
    fg: [f32; 3],
    bg: [f32; 3],
}

pub struct Renderer {
    globals_buf: wgpu::Buffer,
    bind_group_layout: wgpu::BindGroupLayout,
    bg_pipeline: wgpu::RenderPipeline,
    glyph_pipeline: wgpu::RenderPipeline,

    // atlas 纹理（R8）与其上传状态。
    atlas_tex: wgpu::Texture,
    atlas_view: wgpu::TextureView,
    sampler: wgpu::Sampler,
    atlas_dim: (u32, u32),
    /// atlas 纹理是否需要重传（首帧或尺寸变化后置真）。
    atlas_needs_upload: bool,

    bind_group: wgpu::BindGroup,
}

impl Renderer {
    pub fn new(device: &wgpu::Device, font: &FontEngine) -> Self {
        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("vlt-shader"),
            source: wgpu::ShaderSource::Wgsl(include_str!("shader.wgsl").into()),
        });

        let globals_buf = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("globals"),
            size: std::mem::size_of::<Globals>() as u64,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });

        // atlas 纹理。
        let (aw, ah) = (font.atlas.width, font.atlas.height);
        let atlas_tex = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("glyph-atlas"),
            size: wgpu::Extent3d {
                width: aw,
                height: ah,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::R8Unorm,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        });
        let atlas_view = atlas_tex.create_view(&wgpu::TextureViewDescriptor::default());

        let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("atlas-sampler"),
            // 物理像素 1:1 对齐，最近邻避免半像素采样发虚。
            mag_filter: wgpu::FilterMode::Nearest,
            min_filter: wgpu::FilterMode::Nearest,
            mipmap_filter: wgpu::FilterMode::Nearest,
            ..Default::default()
        });

        let bind_group_layout =
            device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
                label: Some("vlt-bgl"),
                entries: &[
                    wgpu::BindGroupLayoutEntry {
                        binding: 0,
                        visibility: wgpu::ShaderStages::VERTEX_FRAGMENT,
                        ty: wgpu::BindingType::Buffer {
                            ty: wgpu::BufferBindingType::Uniform,
                            has_dynamic_offset: false,
                            min_binding_size: None,
                        },
                        count: None,
                    },
                    wgpu::BindGroupLayoutEntry {
                        binding: 1,
                        visibility: wgpu::ShaderStages::FRAGMENT,
                        ty: wgpu::BindingType::Texture {
                            sample_type: wgpu::TextureSampleType::Float { filterable: true },
                            view_dimension: wgpu::TextureViewDimension::D2,
                            multisampled: false,
                        },
                        count: None,
                    },
                    wgpu::BindGroupLayoutEntry {
                        binding: 2,
                        visibility: wgpu::ShaderStages::FRAGMENT,
                        ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                        count: None,
                    },
                ],
            });

        let bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("vlt-bg"),
            layout: &bind_group_layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: globals_buf.as_entire_binding(),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::TextureView(&atlas_view),
                },
                wgpu::BindGroupEntry {
                    binding: 2,
                    resource: wgpu::BindingResource::Sampler(&sampler),
                },
            ],
        });

        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("vlt-pl"),
            bind_group_layouts: &[&bind_group_layout],
            push_constant_ranges: &[],
        });

        // 背景实例布局。
        let bg_layout = wgpu::VertexBufferLayout {
            array_stride: std::mem::size_of::<BgInstance>() as u64,
            step_mode: wgpu::VertexStepMode::Instance,
            attributes: &wgpu::vertex_attr_array![0 => Float32x2, 1 => Float32x2, 2 => Float32x3],
        };

        let bg_pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("bg-pipeline"),
            layout: Some(&pipeline_layout),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: "vs_bg",
                buffers: &[bg_layout.clone()],
                compilation_options: Default::default(),
            },
            fragment: Some(wgpu::FragmentState {
                module: &shader,
                entry_point: "fs_bg",
                targets: &[Some(wgpu::ColorTargetState {
                    format: TARGET_FORMAT,
                    blend: None,
                    write_mask: wgpu::ColorWrites::ALL,
                })],
                compilation_options: Default::default(),
            }),
            primitive: wgpu::PrimitiveState::default(),
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            multiview: None,
            cache: None,
        });

        let glyph_layout = wgpu::VertexBufferLayout {
            array_stride: std::mem::size_of::<GlyphInstance>() as u64,
            step_mode: wgpu::VertexStepMode::Instance,
            attributes: &wgpu::vertex_attr_array![
                0 => Float32x2, 1 => Float32x2, 2 => Float32x2,
                3 => Float32x2, 4 => Float32x3, 5 => Float32x3
            ],
        };

        let glyph_pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("glyph-pipeline"),
            layout: Some(&pipeline_layout),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: "vs_glyph",
                buffers: &[glyph_layout],
                compilation_options: Default::default(),
            },
            fragment: Some(wgpu::FragmentState {
                module: &shader,
                entry_point: "fs_glyph",
                targets: &[Some(wgpu::ColorTargetState {
                    format: TARGET_FORMAT,
                    // 字形已在 shader 内与背景混合完成，输出不透明，无需硬件混合。
                    blend: None,
                    write_mask: wgpu::ColorWrites::ALL,
                })],
                compilation_options: Default::default(),
            }),
            primitive: wgpu::PrimitiveState::default(),
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            multiview: None,
            cache: None,
        });

        Renderer {
            globals_buf,
            bind_group_layout,
            bg_pipeline,
            glyph_pipeline,
            atlas_tex,
            atlas_view,
            sampler,
            atlas_dim: (aw, ah),
            atlas_needs_upload: true, // 强制首次上传
            bind_group,
        }
    }

    /// 若 atlas 有新字形（dirty）或纹理待重传则整块传到 GPU。
    fn sync_atlas(&mut self, queue: &wgpu::Queue, font: &mut FontEngine) {
        if !font.dirty && !self.atlas_needs_upload {
            return;
        }
        queue.write_texture(
            wgpu::ImageCopyTexture {
                texture: &self.atlas_tex,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            &font.atlas.data,
            wgpu::ImageDataLayout {
                offset: 0,
                bytes_per_row: Some(self.atlas_dim.0), // R8：每像素 1 字节
                rows_per_image: Some(self.atlas_dim.1),
            },
            wgpu::Extent3d {
                width: self.atlas_dim.0,
                height: self.atlas_dim.1,
                depth_or_array_layers: 1,
            },
        );
        font.dirty = false;
        self.atlas_needs_upload = false;
    }

    /// 渲染一帧到给定的 `view`。
    ///
    /// - `snap`：网格快照。
    /// - `font`：字体引擎（会即时光栅化尚未缓存的字形）。
    /// - `screen_size`：目标物理像素尺寸。
    /// - `gamma_correct`：true=正确混合，false=naive（用于对比截图）。
    #[allow(clippy::too_many_arguments)]
    pub fn render(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        view: &wgpu::TextureView,
        snap: &GridSnapshot,
        font: &mut FontEngine,
        screen_size: (u32, u32),
        gamma_correct: bool,
    ) {
        let m = font.metrics;
        let cw = m.width as f32;
        let ch = m.height as f32;

        // ---- 构建实例数据 ----
        let mut bg_instances: Vec<BgInstance> = Vec::with_capacity(snap.cells.len() + 8);
        let mut deco_instances: Vec<BgInstance> = Vec::new();
        let mut glyph_instances: Vec<GlyphInstance> = Vec::with_capacity(snap.cells.len());

        // 光标底：作为背景之一（block 光标画墨块，其上文字反白）。
        let cursor = snap.cursor;

        // 先算出光标所在单元格，用于文字反白。
        let cursor_cell = if cursor.visible && cursor.shape == SnapCursorShape::Block {
            Some((cursor.col, cursor.line))
        } else {
            None
        };

        for cell in &snap.cells {
            let x = cell.col as f32 * cw;
            let y = cell.line as f32 * ch;

            let mut fg = cell.fg;
            let bg = cell.bg;

            let is_cursor_cell = cursor_cell == Some((cell.col, cell.line));
            if is_cursor_cell {
                // block 光标：单元格底填墨色，文字用纸白反白。
                let cur_col = Rgb::new(0x1A, 0x1A, 0x1A);
                // 背景实例改画光标墨块。
                bg_instances.push(BgInstance {
                    pos: [x, y],
                    size: [cw, ch],
                    color: cur_col.to_srgb_f32(),
                });
                fg = snap.default_bg; // 文字反白为纸白
            } else if bg != snap.default_bg {
                // 非默认背景才画背景矩形（默认纸白由清屏统一处理，省实例）。
                bg_instances.push(BgInstance {
                    pos: [x, y],
                    size: [cw, ch],
                    color: bg.to_srgb_f32(),
                });
            }

            if cell.wide_spacer {
                continue; // 占位半格不画字形。
            }

            // 装饰线：下划线/删除线。
            if cell.underline {
                let uy = y + m.underline_y as f32;
                deco_instances.push(BgInstance {
                    pos: [x, uy],
                    size: [cw, m.underline_thickness as f32],
                    color: fg.to_srgb_f32(),
                });
            }
            if cell.strikeout {
                let sy = y + (m.ascent as f32) * 0.65;
                deco_instances.push(BgInstance {
                    pos: [x, sy],
                    size: [cw, m.underline_thickness as f32],
                    color: fg.to_srgb_f32(),
                });
            }

            // 字形。空格及不可见字符跳过。
            if cell.ch == ' ' || cell.ch == '\0' {
                continue;
            }
            if let Some(entry) = font.glyph(cell.ch) {
                // 字形位图原点：基线在 y + ascent；placement.top 为位图顶到基线的高度。
                let gx = x + entry.left as f32;
                let gy = y + m.ascent as f32 - entry.top as f32;
                let uv_min = [
                    entry.atlas_x as f32 / self.atlas_dim.0 as f32,
                    entry.atlas_y as f32 / self.atlas_dim.1 as f32,
                ];
                let uv_max = [
                    (entry.atlas_x + entry.width) as f32 / self.atlas_dim.0 as f32,
                    (entry.atlas_y + entry.height) as f32 / self.atlas_dim.1 as f32,
                ];
                glyph_instances.push(GlyphInstance {
                    pos: [gx, gy],
                    size: [entry.width as f32, entry.height as f32],
                    uv_min,
                    uv_max,
                    fg: fg.to_srgb_f32(),
                    // 字形与其单元格背景做 gamma 混合：
                    // 若该格是光标格，背景是墨块色；否则是该格背景。
                    bg: if is_cursor_cell {
                        Rgb::new(0x1A, 0x1A, 0x1A).to_srgb_f32()
                    } else {
                        cell.bg.to_srgb_f32()
                    },
                });
            }
        }

        // 非 block 光标（beam/underline/hollow）作为装饰线画。
        if cursor.visible {
            let x = cursor.col as f32 * cw;
            let y = cursor.line as f32 * ch;
            let cur_col = Rgb::new(0x1A, 0x1A, 0x1A).to_srgb_f32();
            match cursor.shape {
                SnapCursorShape::Beam => deco_instances.push(BgInstance {
                    pos: [x, y],
                    size: [(cw * 0.15).max(1.0), ch],
                    color: cur_col,
                }),
                SnapCursorShape::Underline => deco_instances.push(BgInstance {
                    pos: [x, y + ch - m.underline_thickness as f32 * 2.0],
                    size: [cw, m.underline_thickness as f32 * 2.0],
                    color: cur_col,
                }),
                SnapCursorShape::HollowBlock => {
                    // 四条边框。
                    let t = 1.0f32;
                    deco_instances.push(BgInstance { pos: [x, y], size: [cw, t], color: cur_col });
                    deco_instances.push(BgInstance { pos: [x, y + ch - t], size: [cw, t], color: cur_col });
                    deco_instances.push(BgInstance { pos: [x, y], size: [t, ch], color: cur_col });
                    deco_instances.push(BgInstance { pos: [x + cw - t, y], size: [t, ch], color: cur_col });
                }
                _ => {}
            }
        }

        // ---- 同步 atlas（字形光栅化在上面 font.glyph 里已发生）----
        self.sync_atlas(queue, font);

        // ---- 上传实例缓冲 ----
        // 损伤驱动下每帧只在有变更时才渲染，故直接每帧新建缓冲，简单正确。
        // wgpu::Buffer 非 Clone，且我们只在本函数内使用，无需跨帧复用。
        let bg_buf = upload_owned(device, &bg_instances, "bg-inst");
        let deco_buf = upload_owned(device, &deco_instances, "deco-inst");
        let glyph_buf = upload_owned(device, &glyph_instances, "glyph-inst");

        // ---- globals ----
        let globals = Globals {
            screen_size: [screen_size.0 as f32, screen_size.1 as f32],
            gamma_correct: if gamma_correct { 1.0 } else { 0.0 },
            _pad: 0.0,
        };
        queue.write_buffer(&self.globals_buf, 0, bytemuck::bytes_of(&globals));

        // ---- 录制并提交 ----
        let mut encoder =
            device.create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some("frame") });
        {
            let bg = snap.default_bg;
            let clear = wgpu::Color {
                r: bg.r as f64 / 255.0,
                g: bg.g as f64 / 255.0,
                b: bg.b as f64 / 255.0,
                a: 1.0,
            };
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("main-pass"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(clear),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
            });

            pass.set_bind_group(0, &self.bind_group, &[]);

            // 1) 背景色（含光标墨块）。
            if !bg_instances.is_empty() {
                pass.set_pipeline(&self.bg_pipeline);
                pass.set_vertex_buffer(0, bg_buf.slice(..));
                pass.draw(0..6, 0..bg_instances.len() as u32);
            }

            // 2) 装饰线（下划线/删除线/非 block 光标）——复用 bg pipeline。
            if !deco_instances.is_empty() {
                pass.set_pipeline(&self.bg_pipeline);
                pass.set_vertex_buffer(0, deco_buf.slice(..));
                pass.draw(0..6, 0..deco_instances.len() as u32);
            }

            // 3) 灰度字形（gamma 正确混合）。
            if !glyph_instances.is_empty() {
                pass.set_pipeline(&self.glyph_pipeline);
                pass.set_vertex_buffer(0, glyph_buf.slice(..));
                pass.draw(0..6, 0..glyph_instances.len() as u32);
            }
        }

        queue.submit(Some(encoder.finish()));
    }

    /// 供窗口 resize / DPR 变化后重建 atlas 纹理（尺寸变化时）。
    pub fn rebuild_atlas_texture(&mut self, device: &wgpu::Device, font: &FontEngine) {
        let (aw, ah) = (font.atlas.width, font.atlas.height);
        if (aw, ah) == self.atlas_dim {
            self.atlas_needs_upload = true; // 强制重传
            return;
        }
        self.atlas_tex = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("glyph-atlas"),
            size: wgpu::Extent3d {
                width: aw,
                height: ah,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::R8Unorm,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        });
        self.atlas_view = self.atlas_tex.create_view(&wgpu::TextureViewDescriptor::default());
        self.atlas_dim = (aw, ah);
        self.atlas_needs_upload = true;

        // 重建 bind group。
        self.bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("vlt-bg"),
            layout: &self.bind_group_layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: self.globals_buf.as_entire_binding(),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::TextureView(&self.atlas_view),
                },
                wgpu::BindGroupEntry {
                    binding: 2,
                    resource: wgpu::BindingResource::Sampler(&self.sampler),
                },
            ],
        });
    }
}

/// 为一帧实例数据新建 vertex 缓冲（空数据也返回一个占位缓冲）。
fn upload_owned<T: Pod>(device: &wgpu::Device, data: &[T], label: &str) -> wgpu::Buffer {
    if data.is_empty() {
        return device.create_buffer(&wgpu::BufferDescriptor {
            label: Some(label),
            size: std::mem::size_of::<T>() as u64,
            usage: wgpu::BufferUsages::VERTEX,
            mapped_at_creation: false,
        });
    }
    device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
        label: Some(label),
        contents: bytemuck::cast_slice(data),
        usage: wgpu::BufferUsages::VERTEX,
    })
}
