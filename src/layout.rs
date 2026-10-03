//! Defines the layout for the GPU scene and Chromium panes.
//!
//! Tiles smoothly animate to their new positions without overlapping.

/// A rectangle in the window's logical pixels.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Rect {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
}

impl Rect {
    pub fn new(x: f32, y: f32, w: f32, h: f32) -> Self {
        Self { x, y, w, h }
    }

    fn values(self) -> [f32; 4] {
        [self.x, self.y, self.w, self.h]
    }

    fn from_values([x, y, w, h]: [f32; 4]) -> Self {
        Self { x, y, w, h }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Tile {
    Scene,
    Controls,
    Charts,
    Console,
}

pub const TILES: [Tile; 4] = [Tile::Scene, Tile::Controls, Tile::Charts, Tile::Console];

/// Room above the tiles for the native title bar.
pub const TOP: f32 = 60.0;
const MARGIN: f32 = 14.0;
const GAP: f32 = 10.0;
const CONTROLS_W: f32 = 330.0;
const CHARTS_W: f32 = 370.0;
const CONSOLE_H: f32 = 190.0;
/// Room above each tile for its label when exploded.
const LABEL_H: f32 = 46.0;

/// A critically damped spring per value: fast, without wobble.
#[derive(Clone, Copy)]
struct Spring {
    value: f32,
    velocity: f32,
}

const STIFFNESS: f32 = 190.0;

impl Spring {
    fn step(&mut self, target: f32, dt: f32) {
        let damping = 2.0 * STIFFNESS.sqrt() * 0.92;
        let accel = STIFFNESS * (target - self.value) - damping * self.velocity;
        self.velocity += accel * dt;
        self.value += self.velocity * dt;
    }

    fn settled(&self, target: f32) -> bool {
        (target - self.value).abs() < 0.25 && self.velocity.abs() < 0.5
    }
}

struct Animated {
    springs: [Spring; 4],
    /// Seconds before this tile starts moving, so the tiles leave in turn
    delay: f32,
}

pub struct Layout {
    exploded: bool,
    tiles: Vec<(Tile, Animated)>,
}

impl Layout {
    pub fn new(window: (f32, f32)) -> Self {
        let mut layout = Self {
            exploded: false,
            tiles: Vec::new(),
        };
        for tile in TILES {
            let target = layout.target(tile, window).values();
            let springs = target.map(|value| Spring {
                value,
                velocity: 0.0,
            });
            layout.tiles.push((
                tile,
                Animated {
                    springs,
                    delay: 0.0,
                },
            ));
        }
        layout
    }

    pub fn exploded(&self) -> bool {
        self.exploded
    }

    /// Switches between docked and exploded; the tiles move in turn.
    pub fn toggle(&mut self) {
        self.exploded = !self.exploded;
        for (i, (_, animated)) in self.tiles.iter_mut().enumerate() {
            animated.delay = i as f32 * 0.045;
        }
    }

    /// Puts every tile at its target at once: a resized window must not
    /// leave the panes trailing behind its edges.
    pub fn snap(&mut self, window: (f32, f32)) {
        let targets: Vec<Rect> = self
            .tiles
            .iter()
            .map(|(tile, _)| self.target(*tile, window))
            .collect();
        for ((_, animated), target) in self.tiles.iter_mut().zip(targets) {
            for (spring, value) in animated.springs.iter_mut().zip(target.values()) {
                *spring = Spring {
                    value,
                    velocity: 0.0,
                };
            }
        }
    }

    /// Moves every tile on by `dt` seconds; true while any is still moving.
    pub fn step(&mut self, dt: f32, window: (f32, f32)) -> bool {
        let targets: Vec<Rect> = self
            .tiles
            .iter()
            .map(|(tile, _)| self.target(*tile, window))
            .collect();
        let mut moving = false;
        for ((_, animated), target) in self.tiles.iter_mut().zip(targets) {
            if animated.delay > 0.0 {
                animated.delay -= dt;
                moving = true;
                continue;
            }
            // Small steps keep the spring stable on a slow frame
            let steps = (dt / 0.004).ceil().max(1.0) as usize;
            for _ in 0..steps {
                for (spring, value) in animated.springs.iter_mut().zip(target.values()) {
                    spring.step(value, dt / steps as f32);
                }
            }
            moving |= !animated
                .springs
                .iter()
                .zip(target.values())
                .all(|(s, t)| s.settled(t));
        }
        moving
    }

    /// Where `tile` is this frame.
    pub fn rect(&self, tile: Tile) -> Rect {
        let (_, animated) = self
            .tiles
            .iter()
            .find(|(t, _)| *t == tile)
            .expect("every tile is laid out");
        Rect::from_values(animated.springs.map(|s| s.value))
    }

    fn target(&self, tile: Tile, (w, h): (f32, f32)) -> Rect {
        if self.exploded {
            exploded(tile, w, h)
        } else {
            docked(tile, w, h)
        }
    }
}

/// Controls on the left, charts on the right, the scene between them, the
/// console along the bottom.
fn docked(tile: Tile, w: f32, h: f32) -> Rect {
    let top_h = (h - TOP - MARGIN - GAP - CONSOLE_H).max(120.0);
    let scene_w = (w - 2.0 * MARGIN - 2.0 * GAP - CONTROLS_W - CHARTS_W).max(200.0);
    match tile {
        Tile::Controls => Rect::new(MARGIN, TOP, CONTROLS_W, top_h),
        Tile::Scene => Rect::new(MARGIN + CONTROLS_W + GAP, TOP, scene_w, top_h),
        Tile::Charts => Rect::new(w - MARGIN - CHARTS_W, TOP, CHARTS_W, top_h),
        Tile::Console => Rect::new(MARGIN, TOP + top_h + GAP, w - 2.0 * MARGIN, CONSOLE_H),
    }
}

/// Each tile where it is docked, shrunk about its own centre, so gaps open
/// between them for the labels and the wires. The panes shrink little
/// enough that their pages stay readable.
fn exploded(tile: Tile, w: f32, h: f32) -> Rect {
    let r = docked(tile, w, h);
    let (sx, sy) = match tile {
        Tile::Scene => (0.8, 0.78),
        Tile::Console => (0.86, 0.62),
        Tile::Controls | Tile::Charts => (0.86, 0.8),
    };
    let (nw, nh) = (r.w * sx, r.h * sy);
    // The top row drops below the room its labels need
    let lift = if tile == Tile::Console {
        12.0
    } else {
        LABEL_H * 0.6
    };
    Rect::new(
        r.x + (r.w - nw) / 2.0,
        r.y + (r.h - nh) / 2.0 + lift,
        nw,
        nh,
    )
}
