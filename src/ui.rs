//! Native UI drawn with egui alongside the galaxy.
//!
//! It shows the title, frame info, and tile layout around the Chromium panes.

use std::collections::VecDeque;

use egui::{Align2, Color32, FontId, Painter, Pos2, Stroke, StrokeKind, pos2, vec2};

use crate::layout::{Layout, Rect, TILES, Tile};

const INK: Color32 = Color32::from_rgb(222, 226, 235);
const MUTED: Color32 = Color32::from_rgb(122, 130, 148);
const FRAME: Color32 = Color32::from_rgb(38, 43, 58);
/// Chromium's colour in the chrome: its share of the frame, its labels
pub const CHROMIUM: Color32 = Color32::from_rgb(242, 178, 76);
/// The GPU's colour
pub const GPU: Color32 = Color32::from_rgb(139, 123, 255);

/// What one frame cost the loop, in milliseconds.
#[derive(Clone, Copy, Default)]
pub struct FrameCost {
    /// From the previous frame to this one
    pub interval: f32,
    /// Inside Chromium's pump since the previous frame
    pub pump: f32,
    /// Building and submitting this frame on the CPU
    pub work: f32,
}

pub struct Budget {
    frames: VecDeque<FrameCost>,
}

const BUDGET_FRAMES: usize = 180;

impl Budget {
    pub fn new() -> Self {
        Self {
            frames: VecDeque::with_capacity(BUDGET_FRAMES),
        }
    }

    pub fn push(&mut self, cost: FrameCost) {
        if self.frames.len() == BUDGET_FRAMES {
            self.frames.pop_front();
        }
        self.frames.push_back(cost);
    }

    fn average(&self, part: impl Fn(&FrameCost) -> f32) -> f32 {
        if self.frames.is_empty() {
            return 0.0;
        }
        self.frames.iter().map(part).sum::<f32>() / self.frames.len() as f32
    }

    pub fn fps(&self) -> f32 {
        let interval = self.average(|f| f.interval);
        if interval > 0.0 {
            1000.0 / interval
        } else {
            0.0
        }
    }

    pub fn pump(&self) -> f32 {
        self.average(|f| f.pump)
    }
}

/// What the chrome shows besides the layout.
pub struct Hud<'a> {
    pub budget: &'a Budget,
    pub stars: u32,
    pub adapter: &'a str,
    /// 0 docked, 1 pulled apart, in between while moving
    pub apart: f32,
}

pub fn draw(painter: &Painter, layout: &Layout, hud: &Hud, wires: &Wires) {
    title(painter, hud);
    for tile in TILES {
        frame(painter, layout.rect(tile));
    }
    scene_overlay(painter, layout.rect(Tile::Scene), hud);
    if hud.apart > 0.01 {
        for tile in TILES {
            label(painter, tile, layout.rect(tile), hud.apart);
        }
        draw_wires(painter, layout, wires, hud.apart);
    }
}

/// The three ways the loop and the panes talk, drawn between the tiles
/// when the window is pulled apart.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Wire {
    /// The controls' commands, Chromium to Rust
    Commands,
    /// The telemetry stream, Rust to Chromium
    Stream,
    /// The console's events, Rust to Chromium
    Events,
}

const WIRES: [Wire; 3] = [Wire::Commands, Wire::Stream, Wire::Events];
/// Seconds a dot takes along its wire.
const TRAVEL: f32 = 0.55;
/// The fewest seconds between two dots on one wire; the caption carries the
/// real rate.
const SPACING: f32 = 1.0 / 20.0;

/// Messages in flight along the wires, one dot per message, and the rates
/// of the last full second.
pub struct Wires {
    dots: Vec<(Wire, f32)>,
    since: [f32; 3],
    pub rates: [u32; 3],
    pub stream_bytes: u32,
}

impl Wires {
    pub fn new() -> Self {
        Self {
            dots: Vec::new(),
            since: [SPACING; 3],
            rates: [0; 3],
            stream_bytes: 0,
        }
    }

    /// `messages` went along `wire` this frame.
    pub fn send(&mut self, wire: Wire, messages: u32) {
        let i = wire as usize;
        if messages > 0 && self.since[i] >= SPACING {
            self.dots.push((wire, 0.0));
            self.since[i] = 0.0;
        }
    }

    pub fn step(&mut self, dt: f32) {
        for (_, t) in &mut self.dots {
            *t += dt / TRAVEL;
        }
        self.dots.retain(|(_, t)| *t < 1.0);
        for since in &mut self.since {
            *since += dt;
        }
    }
}

/// A wire's curve: from the tile that sends to the one that receives.
fn path(wire: Wire, layout: &Layout) -> [Pos2; 4] {
    let scene = to_egui(layout.rect(Tile::Scene));
    let across = |a: Pos2, b: Pos2| {
        let bend = (b.x - a.x) * 0.5;
        [a, a + vec2(bend, 0.0), b - vec2(bend, 0.0), b]
    };
    match wire {
        Wire::Commands => across(
            to_egui(layout.rect(Tile::Controls)).right_center(),
            scene.left_center(),
        ),
        Wire::Stream => across(
            scene.right_center(),
            to_egui(layout.rect(Tile::Charts)).left_center(),
        ),
        Wire::Events => {
            let from = scene.center_bottom();
            let to = pos2(from.x, to_egui(layout.rect(Tile::Console)).top());
            let bend = (to.y - from.y) * 0.5;
            [from, from + vec2(0.0, bend), to - vec2(0.0, bend), to]
        }
    }
}

fn bezier([a, b, c, d]: [Pos2; 4], t: f32) -> Pos2 {
    let u = 1.0 - t;
    let p = a.to_vec2() * (u * u * u)
        + b.to_vec2() * (3.0 * u * u * t)
        + c.to_vec2() * (3.0 * u * t * t)
        + d.to_vec2() * (t * t * t);
    p.to_pos2()
}

fn draw_wires(painter: &Painter, layout: &Layout, wires: &Wires, apart: f32) {
    let fade = |c: Color32, a: f32| {
        Color32::from_rgba_unmultiplied(c.r(), c.g(), c.b(), (a * apart * 255.0) as u8)
    };
    for wire in WIRES {
        let points = path(wire, layout);
        painter.add(egui::epaint::CubicBezierShape::from_points_stroke(
            points,
            false,
            Color32::TRANSPARENT,
            Stroke::new(1.5, fade(Color32::from_rgb(70, 77, 98), 1.0)),
        ));
        let (name, rate) = match wire {
            Wire::Commands => ("commands", format!("{} a second", wires.rates[0])),
            Wire::Stream => (
                "binary stream",
                format!(
                    "{} a second, {:.0} KB/s",
                    wires.rates[1],
                    wires.stream_bytes as f32 / 1000.0
                ),
            ),
            Wire::Events => ("events", format!("{} a second", wires.rates[2])),
        };
        let middle = bezier(points, 0.5);
        let (at, align) = match wire {
            Wire::Events => (middle + vec2(12.0, 0.0), Align2::LEFT_CENTER),
            _ => (middle - vec2(0.0, 30.0), Align2::CENTER_CENTER),
        };
        painter.text(
            at - vec2(0.0, 8.0),
            align,
            name,
            FontId::proportional(13.0),
            fade(CHROMIUM, 1.0),
        );
        painter.text(
            at + vec2(0.0, 8.0),
            align,
            rate,
            FontId::proportional(11.5),
            fade(MUTED, 1.0),
        );
    }
    for (wire, t) in &wires.dots {
        let at = bezier(path(*wire, layout), *t);
        painter.circle_filled(at, 7.0, fade(CHROMIUM, 0.18));
        painter.circle_filled(at, 2.8, fade(CHROMIUM, 1.0));
    }
}

fn to_egui(r: Rect) -> egui::Rect {
    egui::Rect::from_min_size(pos2(r.x, r.y), vec2(r.w, r.h))
}

fn title(painter: &Painter, hud: &Hud) {
    painter.text(
        pos2(16.0, 13.0),
        Align2::LEFT_TOP,
        "Kurogane Showcase",
        FontId::proportional(22.0),
        INK,
    );
    painter.text(
        pos2(16.0, 40.0),
        Align2::LEFT_TOP,
        "One Rust event loop. One GPU scene. Three live Chromium panes. Kurogane lets you compose Chromium. Rust stays in control.",
        FontId::proportional(12.5),
        MUTED,
    );

    let right = painter.clip_rect().right() - 16.0;
    painter.text(
        pos2(right, 12.0),
        Align2::RIGHT_TOP,
        format!("{:.0} fps", hud.budget.fps()),
        FontId::proportional(22.0),
        INK,
    );
    painter.text(
        pos2(right, 40.0),
        Align2::RIGHT_TOP,
        format!("Chromium takes {:.2} ms of each frame", hud.budget.pump()),
        FontId::proportional(12.5),
        CHROMIUM,
    );
}

fn frame(painter: &Painter, r: Rect) {
    painter.rect_stroke(
        to_egui(r).expand(1.0),
        7.0,
        Stroke::new(1.0, FRAME),
        StrokeKind::Outside,
    );
}

fn scene_overlay(painter: &Painter, r: Rect, hud: &Hud) {
    let scene = to_egui(r);
    painter.text(
        scene.left_top() + vec2(14.0, 12.0),
        Align2::LEFT_TOP,
        format!("{} stars", group_thousands(hud.stars)),
        FontId::proportional(15.0),
        INK,
    );
    painter.text(
        scene.left_top() + vec2(14.0, 32.0),
        Align2::LEFT_TOP,
        format!("wgpu compute and draw, {}", hud.adapter),
        FontId::proportional(11.5),
        MUTED,
    );
    budget_chart(painter, scene.left_bottom() + vec2(14.0, -14.0), hud.budget);
}

/// The last three seconds of frames, one bar each: Chromium's pump at the
/// bottom, the loop's own work on top, the rest of the frame faint.
fn budget_chart(painter: &Painter, bottom_left: Pos2, budget: &Budget) {
    const HEIGHT: f32 = 54.0;
    const SCALE: f32 = HEIGHT / 20.0;
    let bar = 2.0;
    let width = BUDGET_FRAMES as f32 * bar;
    let area = egui::Rect::from_min_size(bottom_left - vec2(0.0, HEIGHT), vec2(width, HEIGHT));
    painter.rect_filled(
        area.expand(6.0),
        5.0,
        Color32::from_rgba_unmultiplied(6, 7, 11, 190),
    );

    for (i, cost) in budget.frames.iter().enumerate() {
        let x = area.left() + i as f32 * bar;
        let base = area.bottom();
        let mut top = base;
        let mut segment = |ms: f32, color: Color32| {
            let h = (ms * SCALE).min(top - area.top());
            if h > 0.0 {
                painter.rect_filled(
                    egui::Rect::from_min_max(pos2(x, top - h), pos2(x + bar - 0.5, top)),
                    0.0,
                    color,
                );
                top -= h;
            }
        };
        segment(cost.pump, CHROMIUM);
        segment(cost.work, GPU);
        segment(
            (cost.interval - cost.pump - cost.work).max(0.0),
            Color32::from_rgb(44, 49, 64),
        );
    }

    let sixty = area.bottom() - 16.7 * SCALE;
    painter.line_segment(
        [pos2(area.left(), sixty), pos2(area.right(), sixty)],
        Stroke::new(1.0, Color32::from_rgba_unmultiplied(222, 226, 235, 60)),
    );
    painter.text(
        pos2(area.right() + 4.0, sixty),
        Align2::LEFT_CENTER,
        "60 fps",
        FontId::proportional(10.0),
        MUTED,
    );
    painter.text(
        area.left_top() - vec2(0.0, 4.0),
        Align2::LEFT_BOTTOM,
        "Each frame: Chromium's pump, then the loop's own work",
        FontId::proportional(10.5),
        MUTED,
    );
}

/// What a tile is, above it, once the window is pulled apart.
fn label(painter: &Painter, tile: Tile, r: Rect, apart: f32) {
    let (kind, color, what) = match tile {
        Tile::Scene => (
            "GPU",
            GPU,
            "wgpu: a compute shader moves the stars, then draws them",
        ),
        Tile::Controls => (
            "Chromium",
            CHROMIUM,
            "controls.html steers the galaxy through Rust commands",
        ),
        Tile::Charts => (
            "Chromium",
            CHROMIUM,
            "charts.html plots a binary stream Rust sends every frame",
        ),
        Tile::Console => (
            "Chromium",
            CHROMIUM,
            "console.html shows what the Rust loop reports",
        ),
    };
    let alpha = (apart * 255.0) as u8;
    let fade = |c: Color32| Color32::from_rgba_unmultiplied(c.r(), c.g(), c.b(), alpha);
    let at = pos2(r.x, r.y - 8.0);
    painter.text(
        at - vec2(0.0, 18.0),
        Align2::LEFT_BOTTOM,
        kind,
        FontId::proportional(18.0),
        fade(color),
    );
    painter.text(
        at,
        Align2::LEFT_BOTTOM,
        what,
        FontId::proportional(12.5),
        fade(MUTED),
    );
}

fn group_thousands(n: u32) -> String {
    let digits = n.to_string();
    let mut out = String::new();
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}
