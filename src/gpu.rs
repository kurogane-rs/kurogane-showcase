//! The window's GPU surface.

use std::error::Error;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use winit::event_loop::OwnedDisplayHandle;
use winit::window::Window;

pub struct Gpu {
    pub surface: wgpu::Surface<'static>,
    pub device: wgpu::Device,
    pub queue: wgpu::Queue,
    pub config: wgpu::SurfaceConfiguration,
    /// The GPU and the API wgpu drives it with
    pub adapter: String,
}

impl Gpu {
    pub fn new(window: Arc<Window>, display: OwnedDisplayHandle) -> Result<Self, Box<dyn Error>> {
        let instance = wgpu::Instance::new(wgpu::InstanceDescriptor {
            // Star buffers stop short of the GPU's memory budget rather than
            // spill into system memory
            memory_budget_thresholds: wgpu::MemoryBudgetThresholds {
                for_resource_creation: Some(90),
                for_device_loss: None,
            },
            ..wgpu::InstanceDescriptor::new_with_display_handle(Box::new(display))
        });
        let surface = instance.create_surface(window.clone())?;
        let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
            power_preference: wgpu::PowerPreference::HighPerformance,
            compatible_surface: Some(&surface),
            ..Default::default()
        }))?;
        let (device, queue) = pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor {
            // Timing the passes, where the GPU can
            required_features: adapter.features() & wgpu::Features::TIMESTAMP_QUERY,
            ..Default::default()
        }))?;
        // Running out of memory costs a write or a frame, not the app; any
        // other error is a bug and panics, as by default
        let told = AtomicBool::new(false);
        device.on_uncaptured_error(Arc::new(move |error| match error {
            wgpu::Error::OutOfMemory { .. } => {
                if !told.swap(true, Ordering::Relaxed) {
                    tracing::error!("the GPU ran out of memory outside the star buffers");
                }
            }
            error => panic!("wgpu error: {error}"),
        }));

        let size = window.inner_size();
        let mut config = surface
            .get_default_config(&adapter, size.width.max(1), size.height.max(1))
            .ok_or("the window's surface does not suit this GPU")?;
        // Keep the render loop moving without waiting for the display refresh
        let modes = surface.get_capabilities(&adapter).present_modes;
        config.present_mode = [wgpu::PresentMode::Mailbox, wgpu::PresentMode::Immediate]
            .into_iter()
            .find(|mode| modes.contains(mode))
            .unwrap_or(wgpu::PresentMode::Fifo);
        surface.configure(&device, &config);

        let info = adapter.get_info();
        Ok(Self {
            surface,
            device,
            queue,
            config,
            adapter: format!("{} on {:?}", info.name, info.backend),
        })
    }

    pub fn resize(&mut self, width: u32, height: u32) {
        if width == 0 || height == 0 {
            return;
        }
        self.config.width = width;
        self.config.height = height;
        self.surface.configure(&self.device, &self.config);
    }
}
