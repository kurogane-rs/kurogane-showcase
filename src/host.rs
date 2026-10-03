//! The main window loop: handles events, updates the layout,
//! renders the GPU scene and keeps Chromium responsive.
//!
//! Each frame updates the panes, draws the scene and UI,
//! sends telemetry to the charts and pumps Chromium.

use std::error::Error;
use std::sync::Arc;
use std::time::{Duration, Instant};

use kurogane::{AppInstance, BrowserBounds, BrowserHandle};
use winit::application::ApplicationHandler;
use winit::dpi::LogicalSize;
use winit::event::{ElementState, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop};
use winit::keyboard::{Key, NamedKey};
use winit::window::{Window, WindowId};

use crate::bench::{Bench, Next, Setup};
use crate::galaxy::{BINS, Galaxy};
use crate::gpu::Gpu;
use crate::gpu_timer::{GpuTimer, GpuTimes, Pass};
use crate::input::{Input, Recorder};
use crate::layout::{Layout, Rect, Tile};
use crate::perf::{BUILD, CpuTimes, stage};
use crate::shared::{Shared, UserEvent};
use crate::ui::{self, Budget, FrameCost, Hud, Wire, Wires, group_thousands};

/// cefclient's longest wait between pumps.
const MAX_PUMP_DELAY: Duration = Duration::from_millis(1000 / 30);
/// How long a burst pushes the stars out.
const BURST_SECONDS: f32 = 0.35;

const PANES: [(Tile, &str); 3] = [
    (Tile::Controls, "app://app/controls.html"),
    (Tile::Charts, "app://app/charts.html"),
    (Tile::Console, "app://app/console.html"),
];

struct Egui {
    ctx: egui::Context,
    state: egui_winit::State,
    renderer: egui_wgpu::Renderer,
}

struct Pane {
    tile: Tile,
    browser: BrowserHandle,
    /// Where it was last put, so an unmoved pane is left alone
    placed: Option<(i32, i32, i32, i32)>,
}

/// What the loop counts over a second, for the console and the wires.
#[derive(Default)]
struct Second {
    frames: u32,
    pumps: u32,
    pump_ms: f32,
    commands: u32,
    messages: u32,
    bytes: u32,
    lines: u32,
}

struct Host {
    instance: AppInstance,
    shared: Arc<Shared>,
    window: Option<Arc<Window>>,
    gpu: Option<Gpu>,
    egui: Option<Egui>,
    galaxy: Option<Galaxy>,
    /// None when the GPU cannot time its passes
    timer: Option<GpuTimer>,
    layout: Option<Layout>,
    panes: Vec<Pane>,
    adapter: String,
    budget: Budget,
    /// The GPU's recent frames, for the HUD
    gpu_times: Option<GpuTimes>,
    wires: Wires,
    /// Steers the scene while it runs, instead of the controls
    bench: Option<Bench>,
    /// Keeps the controls' changes, for a benchmark to replay
    recorder: Option<Recorder>,
    next_pump: Instant,
    /// One frame per refresh of the window's display
    frame_period: Duration,
    next_frame: Instant,
    last_frame: Instant,
    /// Frames drawn, for the trace
    frames: u64,
    /// Spent in Chromium's pump since the last frame
    pump_ms: f64,
    burst_left: f32,
    /// 0 docked, 1 pulled apart
    apart: f32,
    /// Told the console GPU memory is full
    told_full: bool,
    second_started: Instant,
    second: Second,
}

pub fn run(
    instance: AppInstance,
    shared: Arc<Shared>,
    bench: Option<Bench>,
    recorder: Option<Recorder>,
) -> Result<(), Box<dyn Error>> {
    // CEF parents a child browser to an X11 window on Linux
    #[cfg(target_os = "linux")]
    let event_loop = {
        use winit::platform::x11::EventLoopBuilderExtX11;
        EventLoop::<UserEvent>::with_user_event()
            .with_x11()
            .build()?
    };
    // Keep Kurogane's App, Edit and Window menus on macOS
    #[cfg(target_os = "macos")]
    let event_loop = {
        use winit::platform::macos::EventLoopBuilderExtMacOS;
        EventLoop::<UserEvent>::with_user_event()
            .with_default_menu(false)
            .build()?
    };
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    let event_loop = EventLoop::<UserEvent>::with_user_event().build()?;

    shared.connect(event_loop.create_proxy());

    let now = Instant::now();
    let mut host = Host {
        instance,
        shared,
        window: None,
        gpu: None,
        egui: None,
        galaxy: None,
        timer: None,
        layout: None,
        panes: Vec::new(),
        adapter: String::new(),
        budget: Budget::new(),
        gpu_times: None,
        wires: Wires::new(),
        bench,
        recorder,
        // At once: requests made before the proxy was set went nowhere
        next_pump: now,
        frame_period: Duration::from_secs(1) / 60,
        next_frame: now,
        last_frame: now,
        frames: 0,
        pump_ms: 0.0,
        burst_left: 0.0,
        apart: 0.0,
        told_full: false,
        second_started: now,
        second: Second::default(),
    };
    event_loop.run_app(&mut host)?;
    Ok(())
}

impl ApplicationHandler<UserEvent> for Host {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.window.is_some() {
            return;
        }
        if let Err(e) = self.open(event_loop) {
            eprintln!("the window could not be opened: {e}");
            event_loop.exit();
        }
    }

    fn user_event(&mut self, _: &ActiveEventLoop, event: UserEvent) {
        match event {
            // Keep the earliest: pumping early is harmless, late stalls Chromium
            UserEvent::Pump(deadline) => self.next_pump = self.next_pump.min(deadline),
            UserEvent::Explode => self.explode_asked(),
        }
    }

    fn window_event(&mut self, _: &ActiveEventLoop, _: WindowId, event: WindowEvent) {
        let (Some(window), Some(egui)) = (&self.window, &mut self.egui) else {
            return;
        };
        let _ = egui.state.on_window_event(window, &event);

        match event {
            WindowEvent::CloseRequested => {
                // The window stays until every browser has closed
                self.instance.handle().close_all_browsers(true);
            }
            WindowEvent::Resized(size) => {
                if let Some(gpu) = &mut self.gpu {
                    gpu.resize(size.width, size.height);
                }
                let logical = self.logical_size();
                if let Some(layout) = &mut self.layout {
                    layout.snap(logical);
                }
                self.place_panes();
            }
            // E with the keyboard on the scene; a pane's own E reaches the
            // loop through Kurogane's key hook
            WindowEvent::KeyboardInput { event, .. }
                if event.state == ElementState::Pressed
                    && !event.repeat
                    && matches!(&event.logical_key, Key::Character(c) if c.eq_ignore_ascii_case("e")) =>
            {
                self.explode_asked();
            }
            WindowEvent::KeyboardInput { event, .. }
                if event.state == ElementState::Pressed
                    && event.logical_key == Key::Named(NamedKey::Space) =>
            {
                self.shared.burst();
            }
            WindowEvent::RedrawRequested => self.frame(),
            _ => {}
        }
    }

    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        let now = Instant::now();
        if now >= self.next_pump {
            self.next_pump = now + MAX_PUMP_DELAY;
            let started = Instant::now();
            tracing::info_span!("pump").in_scope(|| self.instance.pump());
            let ms = started.elapsed().as_secs_f64() * 1000.0;
            self.pump_ms += ms;
            self.second.pumps += 1;
            self.second.pump_ms += ms as f32;
        }

        if self.instance.should_shutdown() {
            // Once: the loop can pass here again before it exits
            if let Some(recorder) = self.recorder.take() {
                recorder.save();
            }
            self.panes.clear();
            self.egui = None;
            self.timer = None;
            self.galaxy = None;
            self.gpu = None;
            self.window = None;
            self.instance.shutdown();
            event_loop.exit();
            return;
        }

        // A frame per refresh. Between frames the loop sleeps, and wakes at
        // once when Chromium asks for work: a page's command is answered
        // within a pump, not at the next frame
        let now = Instant::now();
        if now >= self.next_frame {
            if let Some(window) = &self.window {
                window.request_redraw();
            }
            self.next_frame = (self.next_frame + self.frame_period).max(now);
        }
        event_loop.set_control_flow(ControlFlow::WaitUntil(self.next_frame.min(self.next_pump)));
    }
}

impl Host {
    fn open(&mut self, event_loop: &ActiveEventLoop) -> Result<(), Box<dyn Error>> {
        let attributes = Window::default_attributes()
            .with_title("Kurogane Showcase")
            .with_inner_size(LogicalSize::new(1560.0, 960.0))
            .with_min_inner_size(LogicalSize::new(1100.0, 700.0));
        let window = Arc::new(event_loop.create_window(attributes)?);
        let gpu = Gpu::new(window.clone(), event_loop.owned_display_handle())?;
        self.adapter = gpu.adapter.clone();
        if let Some(hz) = window
            .current_monitor()
            .and_then(|monitor| monitor.refresh_rate_millihertz())
            .filter(|&mhz| mhz >= 30_000)
        {
            self.frame_period = Duration::from_secs_f64(1000.0 / hz as f64);
        }

        let ctx = egui::Context::default();
        let state = egui_winit::State::new(
            ctx.clone(),
            egui::ViewportId::ROOT,
            &window,
            Some(window.scale_factor() as f32),
            None,
            None,
        );
        let renderer = egui_wgpu::Renderer::new(
            &gpu.device,
            gpu.config.format,
            egui_wgpu::RendererOptions::default(),
        );
        let galaxy = Galaxy::new(&gpu.device, gpu.config.format);
        self.timer = GpuTimer::new(&gpu.device, &gpu.queue);
        if self.timer.is_none() {
            tracing::warn!("this GPU cannot time its passes; GPU times are left out");
        }
        tracing::info!(
            target: "perf",
            adapter = %self.adapter,
            present_mode = ?gpu.config.present_mode,
            refresh_hz = 1.0 / self.frame_period.as_secs_f64(),
            build = BUILD,
            gpu_timing = self.timer.is_some(),
            "setup"
        );

        self.window = Some(window.clone());
        let layout = Layout::new(self.logical_size());

        let started = Instant::now();
        for (tile, url) in PANES {
            let bounds = pane_bounds(&window, layout.rect(tile));
            let browser = self.instance.create_child_browser(&window, bounds, url)?;
            self.panes.push(Pane {
                tile,
                browser,
                placed: None,
            });
        }
        let made = started.elapsed();

        self.gpu = Some(gpu);
        self.egui = Some(Egui {
            ctx,
            state,
            renderer,
        });
        self.galaxy = Some(galaxy);
        self.layout = Some(layout);
        self.place_panes();

        self.shared.log(
            self.instance.handle(),
            format!("GPU ready: {}", self.adapter),
        );
        self.shared.log(
            self.instance.handle(),
            format!(
                "Created {} Chromium panes in {} ms",
                PANES.len(),
                made.as_millis()
            ),
        );
        if let Some(bench) = &self.bench {
            self.shared.log(self.instance.handle(), bench.describe());
        }
        if let Some(recorder) = &self.recorder {
            self.shared.log(
                self.instance.handle(),
                format!("Recording the controls to {}", recorder.file().display()),
            );
        }
        Ok(())
    }

    fn logical_size(&self) -> (f32, f32) {
        let Some(window) = &self.window else {
            return (1.0, 1.0);
        };
        let size = window.inner_size().to_logical::<f32>(window.scale_factor());
        (size.width, size.height)
    }

    /// E or the Pull apart button: recorded when recording, and ignored
    /// while a benchmark steers the scene.
    fn explode_asked(&mut self) {
        if let Some(recorder) = &mut self.recorder {
            recorder.push(Instant::now(), Input::Explode);
        }
        if !self.bench.as_ref().is_some_and(Bench::running) {
            self.toggle_layout();
        }
    }

    fn toggle_layout(&mut self) {
        let Some(layout) = &mut self.layout else {
            return;
        };
        layout.toggle();
        let words = if layout.exploded() {
            "Pulled the window apart: one GPU scene, three Chromium panes"
        } else {
            "Docked the panes again"
        };
        self.shared.log(self.instance.handle(), words.to_owned());
    }

    /// Moves each pane to its tile, when its tile has moved.
    fn place_panes(&mut self) {
        let (Some(window), Some(layout)) = (&self.window, &self.layout) else {
            return;
        };
        for pane in &mut self.panes {
            let bounds = pane_bounds(window, layout.rect(pane.tile));
            let key = (bounds.x, bounds.y, bounds.width, bounds.height);
            if pane.placed != Some(key) {
                pane.browser.set_bounds(bounds);
                pane.placed = Some(key);
            }
        }
    }

    fn frame(&mut self) {
        let now = Instant::now();
        let interval = now - self.last_frame;
        self.last_frame = now;
        let dt = interval.as_secs_f32().min(1.0 / 20.0);
        self.frames += 1;
        let _frame = tracing::info_span!("frame", n = self.frames).entered();
        let mut cpu = CpuTimes {
            interval: interval.as_secs_f64() * 1000.0,
            pump: self.pump_ms,
            ..CpuTimes::default()
        };

        // A benchmark steers the scene while it runs; the controls otherwise
        let steer = self.bench.as_mut().and_then(|bench| bench.steer(now));
        let burst_asked = self.shared.take_burst();
        if let Some(recorder) = &mut self.recorder {
            recorder.params(now, self.shared.params());
            if burst_asked {
                recorder.push(now, Input::Burst);
            }
        }
        let (params, burst_now) = match &steer {
            Some(steer) => (steer.params, steer.burst),
            None => (self.shared.params(), burst_asked),
        };
        if burst_now {
            self.burst_left = BURST_SECONDS;
        }
        if steer.is_some_and(|steer| steer.explode) {
            self.toggle_layout();
        }

        let logical = self.logical_size();
        stage!(cpu.layout, "layout", {
            if let Some(layout) = &mut self.layout {
                layout.step(dt, logical);
                let target = if layout.exploded() { 1.0 } else { 0.0 };
                self.apart += (target - self.apart) * (dt * 6.0).min(1.0);
            }
            self.place_panes();
        });
        self.wires.step(dt);

        let burst = if self.burst_left > 0.0 {
            self.burst_left -= dt;
            7.0 * (self.burst_left / BURST_SECONDS).max(0.0)
        } else {
            0.0
        };

        let (Some(window), Some(gpu), Some(egui), Some(galaxy), Some(layout)) = (
            &self.window,
            &self.gpu,
            &mut self.egui,
            &mut self.galaxy,
            &self.layout,
        ) else {
            return;
        };

        let (surface, stale) = stage!(cpu.acquire, "acquire", {
            match gpu.surface.get_current_texture() {
                wgpu::CurrentSurfaceTexture::Success(surface) => (surface, false),
                wgpu::CurrentSurfaceTexture::Suboptimal(surface) => (surface, true),
                wgpu::CurrentSurfaceTexture::Outdated | wgpu::CurrentSurfaceTexture::Lost => {
                    gpu.surface.configure(&gpu.device, &gpu.config);
                    tracing::debug!(target: "perf", frame = self.frames, "skipped: surface reconfigured");
                    return;
                }
                // Minimised, covered or late: skip this frame
                _ => {
                    tracing::debug!(target: "perf", frame = self.frames, "skipped: no surface texture");
                    return;
                }
            }
        });

        let stars = stage!(cpu.grow, "grow", galaxy.reserve(&gpu.device, params.count));
        if galaxy.full() && !self.told_full {
            self.told_full = true;
            self.shared.log(
                self.instance.handle(),
                format!(
                    "GPU memory is full: {} stars fit",
                    group_thousands(galaxy.capacity())
                ),
            );
        }

        let work_started = Instant::now();
        let (jobs, screen, mut textures) = stage!(cpu.ui, "ui", {
            let hud = Hud {
                budget: &self.budget,
                stars,
                adapter: &self.adapter,
                apart: self.apart,
                gpu: self.gpu_times,
            };
            let input = egui.state.take_egui_input(window);
            let wires = &self.wires;
            let output = egui
                .ctx
                .run_ui(input, |root| ui::draw(root.painter(), layout, &hud, wires));
            egui.state
                .handle_platform_output(window, output.platform_output);
            let jobs = egui.ctx.tessellate(output.shapes, output.pixels_per_point);
            let screen = egui_wgpu::ScreenDescriptor {
                size_in_pixels: [gpu.config.width, gpu.config.height],
                pixels_per_point: output.pixels_per_point,
            };
            // egui insists every texture change is taken
            let mut textures = output.textures_delta;
            for (id, deltas) in textures.set.drain() {
                for delta in &deltas {
                    egui.renderer
                        .update_texture(&gpu.device, &gpu.queue, id, delta);
                }
            }
            (jobs, screen, textures)
        });

        if let Some(timer) = &mut self.timer {
            timer.begin(self.frames, stars);
        }
        let timer = self.timer.as_ref();
        let commands = stage!(cpu.encode, "encode", {
            let view = surface
                .texture
                .create_view(&wgpu::TextureViewDescriptor::default());
            let target = |load| {
                Some(wgpu::RenderPassColorAttachment {
                    view: &view,
                    resolve_target: None,
                    depth_slice: None,
                    ops: wgpu::Operations {
                        load,
                        store: wgpu::StoreOp::Store,
                    },
                })
            };
            let mut encoder = gpu
                .device
                .create_command_encoder(&wgpu::CommandEncoderDescriptor::default());
            galaxy.step(
                &gpu.queue,
                &mut encoder,
                &params,
                dt,
                burst,
                timer.and_then(GpuTimer::compute_pass),
            );
            let mut commands =
                egui.renderer
                    .update_buffers(&gpu.device, &gpu.queue, &mut encoder, &jobs, &screen);
            {
                let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                    label: Some("stars"),
                    color_attachments: &[target(wgpu::LoadOp::Clear(wgpu::Color {
                        r: 0.0016,
                        g: 0.0019,
                        b: 0.0032,
                        a: 1.0,
                    }))],
                    timestamp_writes: timer.and_then(|timer| timer.render_pass(Pass::Draw)),
                    ..Default::default()
                });
                let scale = window.scale_factor() as f32;
                let scene = layout.rect(Tile::Scene);
                galaxy.draw(
                    &gpu.queue,
                    &mut pass,
                    &params,
                    [
                        scene.x * scale,
                        scene.y * scale,
                        scene.w * scale,
                        scene.h * scale,
                    ],
                    [gpu.config.width, gpu.config.height],
                );
            }
            // egui over the whole surface, in a pass of its own so the GPU
            // times the stars and the chrome apart
            {
                let mut pass = encoder
                    .begin_render_pass(&wgpu::RenderPassDescriptor {
                        label: Some("chrome"),
                        color_attachments: &[target(wgpu::LoadOp::Load)],
                        timestamp_writes: timer.and_then(|timer| timer.render_pass(Pass::Chrome)),
                        ..Default::default()
                    })
                    .forget_lifetime();
                egui.renderer.render(&mut pass, &jobs, &screen);
            }
            if let Some(timer) = timer {
                timer.resolve(&mut encoder);
            }
            commands.push(encoder.finish());
            commands
        });
        stage!(cpu.submit, "submit", gpu.queue.submit(commands));
        let work = work_started.elapsed().as_secs_f32() * 1000.0;

        stage!(cpu.present, "present", {
            window.pre_present_notify();
            gpu.queue.present(surface);
            if stale {
                gpu.surface.configure(&gpu.device, &gpu.config);
            }
        });
        for id in textures.free.drain() {
            egui.renderer.free_texture(&id);
        }
        let arrived = stage!(cpu.readback, "readback", {
            galaxy.collect(&gpu.device);
            self.timer
                .as_mut()
                .map(|timer| timer.collect(&gpu.device))
                .unwrap_or_default()
        });

        let cost = FrameCost {
            interval: cpu.interval as f32,
            pump: self.pump_ms as f32,
            work,
        };
        self.pump_ms = 0.0;
        self.budget.push(cost);
        stage!(cpu.telemetry, "telemetry", self.send_telemetry(cost, stars));
        cpu.frame = now.elapsed().as_secs_f64() * 1000.0;

        cpu.trace(self.frames, stars);
        for times in &arrived {
            times.trace();
            match &mut self.gpu_times {
                Some(recent) => recent.ease(times, 0.1),
                None => self.gpu_times = Some(*times),
            }
            if let Some(bench) = &mut self.bench {
                bench.on_gpu(times);
            }
        }
        let next = self.bench.as_mut().and_then(|bench| {
            bench.on_frame(now, self.frames, stars, &cpu, self.frame_period)
        });
        match next {
            Some(Next::Stars(count)) => self.shared.log(
                self.instance.handle(),
                format!("Benchmark: {} stars", group_thousands(count)),
            ),
            Some(Next::Finish) => self.finish_bench(),
            None => {}
        }

        let (commands, lines) = self.shared.take_traffic();
        self.wires.send(Wire::Commands, commands);
        self.wires.send(Wire::Events, lines);
        self.second.commands += commands;
        self.second.lines += lines;
        self.report_second();
    }

    /// Writes the benchmark's summary and closes the window.
    fn finish_bench(&mut self) {
        let (Some(bench), Some(gpu)) = (&mut self.bench, &self.gpu) else {
            return;
        };
        bench.finish(Setup {
            adapter: self.adapter.clone(),
            present_mode: format!("{:?}", gpu.config.present_mode),
            refresh_hz: 1.0 / self.frame_period.as_secs_f64(),
            surface: [gpu.config.width, gpu.config.height],
        });
        self.instance.handle().close_all_browsers(true);
    }

    /// One frame's numbers for the charts pane, as little-endian f32s:
    /// interval, pump, work, stars, then the two histograms.
    fn send_telemetry(&mut self, cost: FrameCost, stars: u32) {
        let Some(galaxy) = &self.galaxy else { return };
        let mut values = Vec::with_capacity(4 + 2 * BINS);
        values.extend([cost.interval, cost.pump, cost.work, stars as f32]);
        values.extend(galaxy.histograms.radius);
        values.extend(galaxy.histograms.speed);
        let bytes: &[u8] = bytemuck::cast_slice(&values);
        let streams = self.shared.send_telemetry(bytes) as u32;
        self.wires.send(Wire::Stream, streams);
        self.second.messages += streams;
        self.second.bytes += streams * bytes.len() as u32;
    }

    /// Once a second, tells the console what the loop and Chromium did.
    fn report_second(&mut self) {
        self.second.frames += 1;
        if self.second_started.elapsed() < Duration::from_secs(1) {
            return;
        }
        let asked = self.shared.take_pump_requests();
        let second = std::mem::take(&mut self.second);
        self.second_started = Instant::now();
        self.wires.rates = [second.commands, second.messages, second.lines];
        self.wires.stream_bytes = second.bytes;
        self.shared.log(
            self.instance.handle(),
            format!(
                "{} frames; Chromium asked for work {} times, pumped {} times, {:.2} ms in all",
                second.frames, asked, second.pumps, second.pump_ms
            ),
        );
    }
}

/// Where a pane goes for a tile: pixels on Windows and X11, points on macOS.
fn pane_bounds(window: &Window, r: Rect) -> BrowserBounds {
    let scale = if cfg!(target_os = "macos") {
        1.0
    } else {
        window.scale_factor() as f32
    };
    let px = |v: f32| (v * scale).round() as i32;
    let (x, y) = (px(r.x), px(r.y));
    BrowserBounds {
        x,
        y,
        width: px(r.x + r.w) - x,
        height: px(r.y + r.h) - y,
    }
}
