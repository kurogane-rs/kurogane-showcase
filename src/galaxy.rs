//! A simple GPU demo with stars orbiting a central mass.
//!
//! The stars are simulated and rendered on the GPU each frame. They live in
//! buffers of `CHUNK` stars, added as the count grows until GPU memory runs
//! out.

use std::sync::Arc;
use std::sync::atomic::{AtomicU8, Ordering};

use bytemuck::{Pod, Zeroable};
use glam::{Mat4, Vec3};
use serde::{Deserialize, Serialize};

/// Histogram bins per histogram.
pub const BINS: usize = 64;
/// The most stars the controls can ask for; GPU memory runs out first.
pub const MAX_STARS: u32 = 1 << 30;
/// Stars per buffer: 128 MiB, wgpu's default limit for one storage binding,
/// and 16,384 workgroups, within the limit for one dispatch.
const CHUNK: u32 = 1 << 22;
const STAR_BYTES: u64 = 32;
const WORKGROUP: u32 = 256;
/// GPU memory the stars leave free for everything else, since the budget
/// they fill moves with what other processes use: twice 256 MiB, wgpu's
/// default limit for one buffer.
const HEADROOM: u64 = 256 << 20;
/// Mappable memory left free for what every frame allocates: staging for
/// uniform writes and egui's buffers. Its budget shrinks as the stars grow.
const STAGING_HEADROOM: u64 = 32 << 20;

/// What the controls pane steers.
#[derive(Clone, Copy, Debug, PartialEq, Deserialize, Serialize)]
pub struct Params {
    pub count: u32,
    pub gravity: f32,
    pub swirl: f32,
    pub turbulence: f32,
    pub hue: f32,
    pub spin: f32,
    pub size: f32,
}

impl Default for Params {
    fn default() -> Self {
        Self {
            count: 1 << 20,
            gravity: 1.0,
            swirl: 0.12,
            turbulence: 0.04,
            hue: 0.64,
            spin: 0.06,
            size: 1.5,
        }
    }
}

#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
struct SimUniform {
    dt: f32,
    gravity: f32,
    swirl: f32,
    turbulence: f32,
    time: f32,
    burst: f32,
    seed: u32,
    _pad: u32,
}

/// Which of the galaxy's stars a buffer holds.
#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
struct ChunkUniform {
    first: u32,
    count: u32,
    _pad: [u32; 2],
}

#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
struct DrawUniform {
    view_proj: [[f32; 4]; 4],
    viewport: [f32; 2],
    size: f32,
    hue: f32,
}

/// The stars' histograms, as fractions of the stars counted.
#[derive(Clone)]
pub struct Histograms {
    pub radius: [f32; BINS],
    pub speed: [f32; BINS],
}

impl Default for Histograms {
    fn default() -> Self {
        Self {
            radius: [0.0; BINS],
            speed: [0.0; BINS],
        }
    }
}

/// A buffer of stars and the bindings that reach it.
struct Chunk {
    /// The galaxy-wide index of its first star
    first: u32,
    uniform: wgpu::Buffer,
    sim_group: wgpu::BindGroup,
    draw_group: wgpu::BindGroup,
    /// The star count last written to `uniform`
    written: Option<u32>,
    seeded: bool,
}

const IDLE: u8 = 0;
const COPIED: u8 = 1;
const MAPPING: u8 = 2;
const MAPPED: u8 = 3;

pub struct Galaxy {
    sim_uniform: wgpu::Buffer,
    draw_uniform: wgpu::Buffer,
    bins: wgpu::Buffer,
    readback: wgpu::Buffer,
    readback_state: Arc<AtomicU8>,
    sim_layout: wgpu::BindGroupLayout,
    draw_layout: wgpu::BindGroupLayout,
    seed: wgpu::ComputePipeline,
    step: wgpu::ComputePipeline,
    draw: wgpu::RenderPipeline,
    chunks: Vec<Chunk>,
    /// The stars stepped and drawn: as many as asked for that fit
    stars: u32,
    /// GPU memory ran out, so no more buffers are tried
    full: bool,
    /// The stars counted into the histograms on their way back
    counted: u32,
    time: f32,
    orbit: f32,
    /// Frames stepped; varies where respawned stars appear
    frame: u32,
    pub histograms: Histograms,
}

impl Galaxy {
    pub fn new(device: &wgpu::Device, format: wgpu::TextureFormat) -> Self {
        let sim_uniform = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("sim"),
            size: size_of::<SimUniform>() as u64,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let draw_uniform = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("draw"),
            size: size_of::<DrawUniform>() as u64,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let bins_size = (2 * BINS * 4) as u64;
        let bins = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("bins"),
            size: bins_size,
            usage: wgpu::BufferUsages::STORAGE
                | wgpu::BufferUsages::COPY_SRC
                | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let readback = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("bins readback"),
            size: bins_size,
            usage: wgpu::BufferUsages::MAP_READ | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });

        let storage = |binding, read_only, visibility| wgpu::BindGroupLayoutEntry {
            binding,
            visibility,
            ty: wgpu::BindingType::Buffer {
                ty: wgpu::BufferBindingType::Storage { read_only },
                has_dynamic_offset: false,
                min_binding_size: None,
            },
            count: None,
        };
        let uniform = |binding, visibility| wgpu::BindGroupLayoutEntry {
            binding,
            visibility,
            ty: wgpu::BindingType::Buffer {
                ty: wgpu::BufferBindingType::Uniform,
                has_dynamic_offset: false,
                min_binding_size: None,
            },
            count: None,
        };

        let sim_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("sim"),
            entries: &[
                uniform(0, wgpu::ShaderStages::COMPUTE),
                storage(1, false, wgpu::ShaderStages::COMPUTE),
                storage(2, false, wgpu::ShaderStages::COMPUTE),
                uniform(3, wgpu::ShaderStages::COMPUTE),
            ],
        });
        let draw_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("draw"),
            entries: &[
                uniform(0, wgpu::ShaderStages::VERTEX),
                storage(1, true, wgpu::ShaderStages::VERTEX),
            ],
        });

        let step_module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("step"),
            source: wgpu::ShaderSource::Wgsl(include_str!("step.wgsl").into()),
        });
        let draw_module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("draw"),
            source: wgpu::ShaderSource::Wgsl(include_str!("draw.wgsl").into()),
        });

        let sim_pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("sim"),
            bind_group_layouts: &[Some(&sim_layout)],
            immediate_size: 0,
        });
        let compute = |entry: &str| {
            device.create_compute_pipeline(&wgpu::ComputePipelineDescriptor {
                label: Some(entry),
                layout: Some(&sim_pipeline_layout),
                module: &step_module,
                entry_point: Some(entry),
                compilation_options: Default::default(),
                cache: None,
            })
        };
        let seed = compute("seed");
        let step = compute("step");

        let draw_pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("draw"),
            bind_group_layouts: &[Some(&draw_layout)],
            immediate_size: 0,
        });
        // Light adds up: dense regions glow
        let additive = wgpu::BlendComponent {
            src_factor: wgpu::BlendFactor::One,
            dst_factor: wgpu::BlendFactor::One,
            operation: wgpu::BlendOperation::Add,
        };
        let draw = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("stars"),
            layout: Some(&draw_pipeline_layout),
            vertex: wgpu::VertexState {
                module: &draw_module,
                entry_point: Some("vs"),
                compilation_options: Default::default(),
                buffers: &[],
            },
            primitive: wgpu::PrimitiveState::default(),
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            fragment: Some(wgpu::FragmentState {
                module: &draw_module,
                entry_point: Some("fs"),
                compilation_options: Default::default(),
                targets: &[Some(wgpu::ColorTargetState {
                    format,
                    blend: Some(wgpu::BlendState {
                        color: additive,
                        alpha: additive,
                    }),
                    write_mask: wgpu::ColorWrites::ALL,
                })],
            }),
            multiview_mask: None,
            cache: None,
        });

        Self {
            sim_uniform,
            draw_uniform,
            bins,
            readback,
            readback_state: Arc::new(AtomicU8::new(IDLE)),
            sim_layout,
            draw_layout,
            seed,
            step,
            draw,
            chunks: Vec::new(),
            stars: 0,
            full: false,
            counted: 0,
            time: 0.0,
            orbit: 0.6,
            frame: 0,
            histograms: Histograms::default(),
        }
    }

    /// Asks for `count` stars and returns how many there are: buffers are
    /// added as needed, until GPU memory runs out.
    pub fn reserve(&mut self, device: &wgpu::Device, count: u32) -> u32 {
        let count = count.min(MAX_STARS);
        let had = self.chunks.len();
        let was_full = self.full;
        while self.capacity() < count && !self.full {
            if !self.add_chunk(device) {
                self.full = true;
            }
        }
        // New buffers go back until there is room to spare; older ones may
        // be in use on the GPU
        if self.chunks.len() > had {
            while self.chunks.len() > had && !has_room(device) {
                self.chunks.pop();
                self.full = true;
            }
        }
        if self.full && !was_full {
            tracing::warn!(stars = self.capacity(), "GPU memory is full");
        }
        self.stars = count.min(self.capacity());
        self.stars
    }

    /// The most stars there is room for.
    pub fn capacity(&self) -> u32 {
        self.chunks.len() as u32 * CHUNK
    }

    /// Whether GPU memory ran out.
    pub fn full(&self) -> bool {
        self.full
    }

    /// Adds a buffer of stars; false when GPU memory is full.
    fn add_chunk(&mut self, device: &wgpu::Device) -> bool {
        let scope = device.push_error_scope(wgpu::ErrorFilter::OutOfMemory);
        let stars = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("stars"),
            size: CHUNK as u64 * STAR_BYTES,
            usage: wgpu::BufferUsages::STORAGE,
            mapped_at_creation: false,
        });
        let uniform = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("chunk"),
            size: size_of::<ChunkUniform>() as u64,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        if pollster::block_on(scope.pop()).is_some() {
            return false;
        }

        let sim_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("sim"),
            layout: &self.sim_layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: self.sim_uniform.as_entire_binding(),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: stars.as_entire_binding(),
                },
                wgpu::BindGroupEntry {
                    binding: 2,
                    resource: self.bins.as_entire_binding(),
                },
                wgpu::BindGroupEntry {
                    binding: 3,
                    resource: uniform.as_entire_binding(),
                },
            ],
        });
        let draw_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("draw"),
            layout: &self.draw_layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: self.draw_uniform.as_entire_binding(),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: stars.as_entire_binding(),
                },
            ],
        });
        self.chunks.push(Chunk {
            first: self.capacity(),
            uniform,
            sim_group,
            draw_group,
            written: None,
            seeded: false,
        });
        true
    }

    /// Each buffer with stars in use, and how many.
    fn in_use(&self) -> impl Iterator<Item = (&Chunk, u32)> {
        let stars = self.stars;
        self.chunks
            .iter()
            .map(move |chunk| (chunk, stars.saturating_sub(chunk.first).min(CHUNK)))
            .take_while(|&(_, count)| count > 0)
    }

    /// Moves the galaxy on by `dt` seconds and counts it.
    pub fn step(
        &mut self,
        queue: &wgpu::Queue,
        encoder: &mut wgpu::CommandEncoder,
        params: &Params,
        dt: f32,
        burst: f32,
        timestamps: Option<wgpu::ComputePassTimestampWrites<'_>>,
    ) {
        self.time += dt;
        self.orbit += dt * params.spin;
        self.frame = self.frame.wrapping_add(1);
        let uniform = SimUniform {
            // Twice real time: the inner orbits take a couple of seconds
            dt: dt * 2.0,
            gravity: params.gravity,
            swirl: params.swirl,
            turbulence: params.turbulence,
            time: self.time,
            burst,
            seed: self.frame,
            _pad: 0,
        };
        queue.write_buffer(&self.sim_uniform, 0, bytemuck::bytes_of(&uniform));
        // Each buffer's share of the stars
        for chunk in &mut self.chunks {
            let count = self.stars.saturating_sub(chunk.first).min(CHUNK);
            if chunk.written != Some(count) {
                chunk.written = Some(count);
                let uniform = ChunkUniform {
                    first: chunk.first,
                    count,
                    _pad: [0; 2],
                };
                queue.write_buffer(&chunk.uniform, 0, bytemuck::bytes_of(&uniform));
            }
        }

        let collect = self.readback_state.load(Ordering::Acquire) == IDLE;
        encoder.clear_buffer(&self.bins, 0, None);
        {
            let mut pass = encoder.begin_compute_pass(&wgpu::ComputePassDescriptor {
                label: Some("galaxy"),
                timestamp_writes: timestamps,
            });
            // A new buffer starts as a spiral
            pass.set_pipeline(&self.seed);
            for chunk in self.chunks.iter_mut().filter(|chunk| !chunk.seeded) {
                pass.set_bind_group(0, &chunk.sim_group, &[]);
                pass.dispatch_workgroups(CHUNK / WORKGROUP, 1, 1);
                chunk.seeded = true;
            }
            pass.set_pipeline(&self.step);
            for (chunk, count) in self.in_use() {
                pass.set_bind_group(0, &chunk.sim_group, &[]);
                pass.dispatch_workgroups(count.div_ceil(WORKGROUP), 1, 1);
            }
        }
        if collect {
            encoder.copy_buffer_to_buffer(&self.bins, 0, &self.readback, 0, None);
            self.counted = self.stars;
            self.readback_state.store(COPIED, Ordering::Release);
        }
    }

    /// Draws the galaxy into `viewport`, in the surface's pixels, on a
    /// surface `surface` pixels in size.
    pub fn draw(
        &self,
        queue: &wgpu::Queue,
        pass: &mut wgpu::RenderPass<'_>,
        params: &Params,
        viewport: [f32; 4],
        surface: [u32; 2],
    ) {
        let [x, y, w, h] = viewport;
        // The scissor must lie inside the surface
        let left = x.max(0.0) as u32;
        let top = y.max(0.0) as u32;
        let right = ((x + w).max(0.0) as u32).min(surface[0]);
        let bottom = ((y + h).max(0.0) as u32).min(surface[1]);
        if w < 1.0 || h < 1.0 || right <= left || bottom <= top {
            return;
        }
        let eye = Vec3::new(self.orbit.cos() * 8.5, 4.2, self.orbit.sin() * 8.5);
        let view = Mat4::look_at_rh(eye, Vec3::ZERO, Vec3::Y);
        let proj = Mat4::perspective_rh(40f32.to_radians(), w / h, 0.1, 100.0);
        let uniform = DrawUniform {
            view_proj: (proj * view).to_cols_array_2d(),
            viewport: [w, h],
            size: params.size,
            hue: params.hue,
        };
        queue.write_buffer(&self.draw_uniform, 0, bytemuck::bytes_of(&uniform));

        pass.set_viewport(x, y, w, h, 0.0, 1.0);
        pass.set_scissor_rect(left, top, right - left, bottom - top);
        pass.set_pipeline(&self.draw);
        for (chunk, count) in self.in_use() {
            pass.set_bind_group(0, &chunk.draw_group, &[]);
            pass.draw(0..count * 6, 0..1);
        }
    }

    /// After the frame's commands are submitted: asks for the histograms
    /// copied this frame, and takes the ones that have arrived.
    pub fn collect(&mut self, device: &wgpu::Device) {
        if self.readback_state.load(Ordering::Acquire) == COPIED {
            self.readback_state.store(MAPPING, Ordering::Release);
            let state = self.readback_state.clone();
            self.readback
                .map_async(wgpu::MapMode::Read, .., move |result| {
                    state.store(
                        if result.is_ok() { MAPPED } else { IDLE },
                        Ordering::Release,
                    );
                });
        }
        let _ = device.poll(wgpu::PollType::Poll);
        if self.readback_state.load(Ordering::Acquire) != MAPPED {
            return;
        }
        if let Ok(data) = self.readback.get_mapped_range(..) {
            let counts: &[u32] = bytemuck::cast_slice(&data);
            let total = self.counted.max(1) as f32;
            for i in 0..BINS {
                self.histograms.radius[i] = counts[i] as f32 / total;
                self.histograms.speed[i] = counts[BINS + i] as f32 / total;
            }
        }
        self.readback.unmap();
        self.readback_state.store(IDLE, Ordering::Release);
    }
}

/// Whether GPU memory has room to spare, for buffers like the stars' and
/// for staging: buffers that big are made and dropped at once.
fn has_room(device: &wgpu::Device) -> bool {
    let scope = device.push_error_scope(wgpu::ErrorFilter::OutOfMemory);
    let _probes = [
        (HEADROOM, wgpu::BufferUsages::STORAGE),
        (HEADROOM, wgpu::BufferUsages::STORAGE),
        (
            STAGING_HEADROOM,
            wgpu::BufferUsages::MAP_WRITE | wgpu::BufferUsages::COPY_SRC,
        ),
    ]
    .map(|(size, usage)| {
        device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("headroom"),
            size,
            usage,
            mapped_at_creation: false,
        })
    });
    pollster::block_on(scope.pop()).is_none()
}
