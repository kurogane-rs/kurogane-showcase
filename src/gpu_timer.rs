//! Times the frame's GPU passes with timestamp queries.
//!
//! Like the galaxy's histograms, the timings come back through mapped
//! buffers a frame or two later, so the frame never waits for the GPU.

use std::sync::Arc;
use std::sync::atomic::{AtomicU8, Ordering};

/// A pass the timer times.
#[derive(Clone, Copy)]
pub enum Pass {
    /// The galaxy's compute step
    Compute,
    /// The stars drawn
    Draw,
    /// egui's chrome drawn over them
    Chrome,
}

const PASSES: u32 = 3;
const BYTES: u64 = PASSES as u64 * 2 * 8;
/// Frames whose timings can be on their way back at once.
const SLOTS: usize = 4;

const IDLE: u8 = 0;
const COPIED: u8 = 1;
const MAPPING: u8 = 2;
const MAPPED: u8 = 3;

/// One frame's time on the GPU, in milliseconds.
#[derive(Clone, Copy, Default)]
pub struct GpuTimes {
    pub frame: u64,
    pub stars: u32,
    /// From the start of the first pass to the end of the last
    pub total: f64,
    pub compute: f64,
    pub draw: f64,
    pub chrome: f64,
}

impl GpuTimes {
    pub fn trace(&self) {
        tracing::info!(
            target: "perf",
            frame = self.frame,
            stars = self.stars,
            total_ms = self.total,
            compute_ms = self.compute,
            draw_ms = self.draw,
            chrome_ms = self.chrome,
            "gpu"
        );
    }

    /// Moves a share `k` of the way to `next`: a steady figure for the HUD.
    pub fn ease(&mut self, next: &GpuTimes, k: f64) {
        self.total += (next.total - self.total) * k;
        self.compute += (next.compute - self.compute) * k;
        self.draw += (next.draw - self.draw) * k;
        self.chrome += (next.chrome - self.chrome) * k;
    }
}

struct Slot {
    buffer: wgpu::Buffer,
    state: Arc<AtomicU8>,
    frame: u64,
    stars: u32,
}

pub struct GpuTimer {
    queries: wgpu::QuerySet,
    resolved: wgpu::Buffer,
    slots: Vec<Slot>,
    /// Nanoseconds per timestamp tick
    period: f64,
    /// Where this frame's timings go, when a slot was free
    current: Option<usize>,
}

impl GpuTimer {
    /// A timer, when the device can write timestamps.
    pub fn new(device: &wgpu::Device, queue: &wgpu::Queue) -> Option<Self> {
        if !device.features().contains(wgpu::Features::TIMESTAMP_QUERY) {
            return None;
        }
        let queries = device.create_query_set(&wgpu::QuerySetDescriptor {
            label: Some("pass times"),
            ty: wgpu::QueryType::Timestamp,
            count: PASSES * 2,
        });
        let resolved = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("pass times"),
            size: BYTES,
            usage: wgpu::BufferUsages::QUERY_RESOLVE | wgpu::BufferUsages::COPY_SRC,
            mapped_at_creation: false,
        });
        let slots = (0..SLOTS)
            .map(|_| Slot {
                buffer: device.create_buffer(&wgpu::BufferDescriptor {
                    label: Some("pass times readback"),
                    size: BYTES,
                    usage: wgpu::BufferUsages::MAP_READ | wgpu::BufferUsages::COPY_DST,
                    mapped_at_creation: false,
                }),
                state: Arc::new(AtomicU8::new(IDLE)),
                frame: 0,
                stars: 0,
            })
            .collect();
        Some(Self {
            queries,
            resolved,
            slots,
            period: queue.get_timestamp_period() as f64,
            current: None,
        })
    }

    /// Before a frame's passes: the frame is timed when a slot is free.
    pub fn begin(&mut self, frame: u64, stars: u32) {
        self.current = self
            .slots
            .iter()
            .position(|slot| slot.state.load(Ordering::Acquire) == IDLE);
        if let Some(i) = self.current {
            self.slots[i].frame = frame;
            self.slots[i].stars = stars;
        }
    }

    pub fn compute_pass(&self) -> Option<wgpu::ComputePassTimestampWrites<'_>> {
        self.current?;
        let (begin, end) = indices(Pass::Compute);
        Some(wgpu::ComputePassTimestampWrites {
            query_set: &self.queries,
            beginning_of_pass_write_index: Some(begin),
            end_of_pass_write_index: Some(end),
        })
    }

    pub fn render_pass(&self, pass: Pass) -> Option<wgpu::RenderPassTimestampWrites<'_>> {
        self.current?;
        let (begin, end) = indices(pass);
        Some(wgpu::RenderPassTimestampWrites {
            query_set: &self.queries,
            beginning_of_pass_write_index: Some(begin),
            end_of_pass_write_index: Some(end),
        })
    }

    /// After the frame's passes: copies their timestamps out.
    pub fn resolve(&self, encoder: &mut wgpu::CommandEncoder) {
        let Some(i) = self.current else { return };
        encoder.resolve_query_set(&self.queries, 0..PASSES * 2, &self.resolved, 0);
        encoder.copy_buffer_to_buffer(&self.resolved, 0, &self.slots[i].buffer, 0, None);
        self.slots[i].state.store(COPIED, Ordering::Release);
    }

    /// After the frame's commands are submitted: asks for the timings
    /// copied this frame, and returns the ones that have arrived.
    pub fn collect(&mut self, device: &wgpu::Device) -> Vec<GpuTimes> {
        for slot in &self.slots {
            if slot.state.load(Ordering::Acquire) == COPIED {
                slot.state.store(MAPPING, Ordering::Release);
                let state = slot.state.clone();
                slot.buffer
                    .map_async(wgpu::MapMode::Read, .., move |result| {
                        state.store(
                            if result.is_ok() { MAPPED } else { IDLE },
                            Ordering::Release,
                        );
                    });
            }
        }
        let _ = device.poll(wgpu::PollType::Poll);

        let mut arrived = Vec::new();
        for slot in &self.slots {
            if slot.state.load(Ordering::Acquire) != MAPPED {
                continue;
            }
            if let Ok(data) = slot.buffer.get_mapped_range(..) {
                let ticks: Vec<u64> = data
                    .as_chunks::<8>()
                    .0
                    .iter()
                    .map(|bytes| u64::from_le_bytes(*bytes))
                    .collect();
                // A tick count that goes backwards is no timing at all
                let span = |begin: u32, end: u32| {
                    let (begin, end) = (ticks[begin as usize], ticks[end as usize]);
                    (end > begin).then(|| (end - begin) as f64 * self.period / 1e6)
                };
                let part = |pass| {
                    let (begin, end) = indices(pass);
                    span(begin, end)
                };
                if let (Some(total), Some(compute), Some(draw), Some(chrome)) = (
                    span(0, PASSES * 2 - 1),
                    part(Pass::Compute),
                    part(Pass::Draw),
                    part(Pass::Chrome),
                ) {
                    arrived.push(GpuTimes {
                        frame: slot.frame,
                        stars: slot.stars,
                        total,
                        compute,
                        draw,
                        chrome,
                    });
                }
            }
            slot.buffer.unmap();
            slot.state.store(IDLE, Ordering::Release);
        }
        arrived
    }
}

/// The queries a pass writes at its start and its end.
fn indices(pass: Pass) -> (u32, u32) {
    let first = pass as u32 * 2;
    (first, first + 1)
}
