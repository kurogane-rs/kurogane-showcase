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

use crate::galaxy::{BINS, Galaxy};
use crate::gpu::Gpu;
use crate::layout::{Layout, Rect, Tile};
use crate::shared::{Shared, UserEvent};
use crate::ui::{self, Budget, FrameCost, Hud, Wire, Wires};

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
    layout: Option<Layout>,
    panes: Vec<Pane>,
    adapter: String,
    budget: Budget,
    wires: Wires,
    next_pump: Instant,
    /// One frame per refresh of the window's display
    frame_period: Duration,
    next_frame: Instant,
    last_frame: Instant,
    /// Spent in Chromium's pump since the last frame
    pump_ms: f32,
    burst_left: f32,
    /// 0 docked, 1 pulled apart
    apart: f32,
    second_started: Instant,
    second: Second,
}

pub fn run(instance: AppInstance, shared: Arc<Shared>) -> Result<(), Box<dyn Error>> {
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
        layout: None,
        panes: Vec::new(),
        adapter: String::new(),
        budget: Budget::new(),
        wires: Wires::new(),
        // At once: requests made before the proxy was set went nowhere
        next_pump: now,
        frame_period: Duration::from_secs(1) / 60,
        next_frame: now,
        last_frame: now,
        pump_ms: 0.0,
        burst_left: 0.0,
        apart: 0.0,
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
            UserEvent::Explode => self.toggle_layout(),
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
                self.toggle_layout();
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
            self.instance.pump();
            let ms = started.elapsed().as_secs_f32() * 1000.0;
            self.pump_ms += ms;
            self.second.pumps += 1;
            self.second.pump_ms += ms;
        }

        if self.instance.should_shutdown() {
            self.panes.clear();
            self.egui = None;
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
        Ok(())
    }

    fn logical_size(&self) -> (f32, f32) {
        let Some(window) = &self.window else {
            return (1.0, 1.0);
        };
        let size = window.inner_size().to_logical::<f32>(window.scale_factor());
        (size.width, size.height)
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

        let logical = self.logical_size();
        if let Some(layout) = &mut self.layout {
            layout.step(dt, logical);
            let target = if layout.exploded() { 1.0 } else { 0.0 };
            self.apart += (target - self.apart) * (dt * 6.0).min(1.0);
        }
        self.place_panes();
        self.wires.step(dt);

        let params = self.shared.params();
        if self.shared.take_burst() {
            self.burst_left = BURST_SECONDS;
        }
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

        let (surface, stale) = match gpu.surface.get_current_texture() {
            wgpu::CurrentSurfaceTexture::Success(surface) => (surface, false),
            wgpu::CurrentSurfaceTexture::Suboptimal(surface) => (surface, true),
            wgpu::CurrentSurfaceTexture::Outdated | wgpu::CurrentSurfaceTexture::Lost => {
                gpu.surface.configure(&gpu.device, &gpu.config);
                return;
            }
            // Minimised, covered or late: skip this frame
            _ => return,
        };
        let work_started = Instant::now();
        let view = surface
            .texture
            .create_view(&wgpu::TextureViewDescriptor::default());

        let hud = Hud {
            budget: &self.budget,
            stars: params.count,
            adapter: &self.adapter,
            apart: self.apart,
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

        let mut encoder = gpu
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor::default());
        galaxy.step(&gpu.queue, &mut encoder, &params, dt, burst);
        let mut commands =
            egui.renderer
                .update_buffers(&gpu.device, &gpu.queue, &mut encoder, &jobs, &screen);
        {
            let mut pass = encoder
                .begin_render_pass(&wgpu::RenderPassDescriptor {
                    label: Some("frame"),
                    color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                        view: &view,
                        resolve_target: None,
                        depth_slice: None,
                        ops: wgpu::Operations {
                            load: wgpu::LoadOp::Clear(wgpu::Color {
                                r: 0.0016,
                                g: 0.0019,
                                b: 0.0032,
                                a: 1.0,
                            }),
                            store: wgpu::StoreOp::Store,
                        },
                    })],
                    ..Default::default()
                })
                .forget_lifetime();
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
            // egui covers the whole surface again
            pass.set_viewport(
                0.0,
                0.0,
                gpu.config.width as f32,
                gpu.config.height as f32,
                0.0,
                1.0,
            );
            pass.set_scissor_rect(0, 0, gpu.config.width, gpu.config.height);
            egui.renderer.render(&mut pass, &jobs, &screen);
        }
        commands.push(encoder.finish());
        gpu.queue.submit(commands);
        let work = work_started.elapsed().as_secs_f32() * 1000.0;
        window.pre_present_notify();
        gpu.queue.present(surface);
        if stale {
            gpu.surface.configure(&gpu.device, &gpu.config);
        }
        for id in textures.free.drain() {
            egui.renderer.free_texture(&id);
        }
        galaxy.collect(&gpu.device, params.count);

        let cost = FrameCost {
            interval: interval.as_secs_f32() * 1000.0,
            pump: self.pump_ms,
            work,
        };
        self.pump_ms = 0.0;
        self.budget.push(cost);
        self.send_telemetry(cost, params.count);

        let (commands, lines) = self.shared.take_traffic();
        self.wires.send(Wire::Commands, commands);
        self.wires.send(Wire::Events, lines);
        self.second.commands += commands;
        self.second.lines += lines;
        self.report_second();
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
