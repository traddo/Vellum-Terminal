//! Headless 离屏渲染：把一个 `GridSnapshot` 渲染到离屏纹理并读回为 RGBA 像素/PNG。
//!
//! 这是逐像素一致性验收的基础设施：不开窗口，`cargo run --bin snapshot` 或
//! `cargo test` 即可产出确定性 PNG。窗口路径与此路径共用 `Renderer`，
//! 因此这里测出的即是窗口里看到的。

use std::path::Path;

use crate::font::FontEngine;
use crate::gpu::Gpu;
use crate::render::{Renderer, TARGET_FORMAT};
use crate::snapshot::GridSnapshot;

/// 离屏渲染目标（颜色纹理 + 读回缓冲）。
pub struct Headless {
    pub width: u32,
    pub height: u32,
    color_tex: wgpu::Texture,
    color_view: wgpu::TextureView,
    readback: wgpu::Buffer,
    padded_bytes_per_row: u32,
}

impl Headless {
    pub fn new(device: &wgpu::Device, width: u32, height: u32) -> Self {
        let color_tex = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("headless-color"),
            size: wgpu::Extent3d {
                width,
                height,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: TARGET_FORMAT,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
            view_formats: &[],
        });
        let color_view = color_tex.create_view(&wgpu::TextureViewDescriptor::default());

        // 读回缓冲每行须对齐到 256 字节（wgpu 要求）。
        let unpadded = width * 4;
        let align = wgpu::COPY_BYTES_PER_ROW_ALIGNMENT;
        let padded_bytes_per_row = ((unpadded + align - 1) / align) * align;

        let readback = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("headless-readback"),
            size: (padded_bytes_per_row * height) as u64,
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
            mapped_at_creation: false,
        });

        Headless {
            width,
            height,
            color_tex,
            color_view,
            readback,
            padded_bytes_per_row,
        }
    }

    /// 渲染快照并读回为紧凑 RGBA8 像素（去掉行填充）。
    pub fn render_to_rgba(
        &self,
        gpu: &Gpu,
        renderer: &mut Renderer,
        snap: &GridSnapshot,
        font: &mut FontEngine,
        gamma_correct: bool,
    ) -> Vec<u8> {
        // 渲染到离屏 view（headless 不加 padding，保证回归基线稳定）。
        renderer.render(
            &gpu.device,
            &gpu.queue,
            &self.color_view,
            snap,
            font,
            (self.width, self.height),
            (0, 0),
            gamma_correct,
        );

        // 把颜色纹理拷到读回缓冲。
        let mut encoder = gpu
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("readback-copy"),
            });
        encoder.copy_texture_to_buffer(
            wgpu::ImageCopyTexture {
                texture: &self.color_tex,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            wgpu::ImageCopyBuffer {
                buffer: &self.readback,
                layout: wgpu::ImageDataLayout {
                    offset: 0,
                    bytes_per_row: Some(self.padded_bytes_per_row),
                    rows_per_image: Some(self.height),
                },
            },
            wgpu::Extent3d {
                width: self.width,
                height: self.height,
                depth_or_array_layers: 1,
            },
        );
        gpu.queue.submit(Some(encoder.finish()));

        // 映射读回。
        let slice = self.readback.slice(..);
        let (tx, rx) = std::sync::mpsc::channel();
        slice.map_async(wgpu::MapMode::Read, move |r| {
            let _ = tx.send(r);
        });
        gpu.device.poll(wgpu::Maintain::Wait);
        rx.recv().unwrap().expect("map_async 失败");

        let data = slice.get_mapped_range();
        let mut out = Vec::with_capacity((self.width * self.height * 4) as usize);
        for row in 0..self.height {
            let start = (row * self.padded_bytes_per_row) as usize;
            let end = start + (self.width * 4) as usize;
            out.extend_from_slice(&data[start..end]);
        }
        drop(data);
        self.readback.unmap();
        out
    }
}

/// 把紧凑 RGBA8 写入 PNG 文件。
pub fn write_png(path: &Path, width: u32, height: u32, rgba: &[u8]) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let file = std::fs::File::create(path)?;
    let w = std::io::BufWriter::new(file);
    let mut encoder = png::Encoder::new(w, width, height);
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    let mut writer = encoder.write_header()?;
    writer.write_image_data(rgba).map_err(std::io::Error::other)?;
    Ok(())
}
