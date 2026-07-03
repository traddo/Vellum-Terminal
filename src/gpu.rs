//! wgpu 设备/队列初始化。Vulkan 优先（design.md ADR-1 技术栈）。
//!
//! 同一份初始化逻辑供窗口路径与 headless 截图路径共用，保证二者渲染结果一致。

/// 持有 wgpu 的 instance/adapter/device/queue。
pub struct Gpu {
    pub instance: wgpu::Instance,
    pub adapter: wgpu::Adapter,
    pub device: wgpu::Device,
    pub queue: wgpu::Queue,
}

impl Gpu {
    /// 创建一个 Vulkan 优先的 GPU 上下文。
    ///
    /// `compatible_surface` 用于窗口路径挑选能呈现到该 surface 的 adapter；
    /// headless 路径传 None。
    pub fn new(compatible_surface: Option<&wgpu::Surface<'_>>) -> Self {
        // Vulkan 优先；若不可用回退到其它后端（design.md：GL 回退）。
        let instance = wgpu::Instance::new(wgpu::InstanceDescriptor {
            backends: wgpu::Backends::VULKAN | wgpu::Backends::GL,
            ..Default::default()
        });

        let adapter = pollster::block_on(instance.request_adapter(
            &wgpu::RequestAdapterOptions {
                power_preference: wgpu::PowerPreference::HighPerformance,
                force_fallback_adapter: false,
                compatible_surface,
            },
        ))
        .expect("找不到可用的 GPU adapter（Vulkan/GL 均不可用）");

        let (device, queue) = pollster::block_on(adapter.request_device(
            &wgpu::DeviceDescriptor {
                label: Some("vlt-device"),
                required_features: wgpu::Features::empty(),
                required_limits: wgpu::Limits::default(),
                memory_hints: wgpu::MemoryHints::Performance,
            },
            None,
        ))
        .expect("创建 wgpu device 失败");

        Gpu {
            instance,
            adapter,
            device,
            queue,
        }
    }

    /// 打印选中的 adapter 信息（后端/设备名），用于确认走了 Vulkan。
    pub fn describe(&self) -> String {
        let info = self.adapter.get_info();
        format!(
            "GPU adapter: {} | backend={:?} | type={:?}",
            info.name, info.backend, info.device_type
        )
    }
}
