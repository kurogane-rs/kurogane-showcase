//! The benchmark: the scene steered the same way on every run, so a run
//! before a change and one after it can be compared.
//!
//! By default it sweeps the Stars slider's range: from 2^16 stars it steps
//! up by √2, lets each count settle for a second and measures it for three,
//! until GPU memory is full or a count runs below 5 frames a second. With
//! `--replay=<recording>` it plays back a recorded session instead, drags
//! and all, and measures every frame.
//!
//! Either way the controls are ignored while it runs. It writes
//! `perf/<name>.json`, prints its numbers per star count, and compares them
//! with another run's: `--against=<name>`, or by default `baseline` for a
//! sweep and `baseline-<recording>` for a replay, when there is one.

use std::error::Error;
use std::fs;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use crate::galaxy::{MAX_STARS, Params};
use crate::gpu_timer::GpuTimes;
use crate::input::{Input, Player};
use crate::perf::{self, BUILD, CpuTimes};
use crate::ui::group_thousands;

/// For the panes to load before anything is measured.
const WARMUP: Duration = Duration::from_secs(3);
/// After each new count: its buffers are made and seeded.
const SETTLE: Duration = Duration::from_secs(1);
const MEASURE: Duration = Duration::from_secs(3);
/// The sweep's first count, 2^16 stars, the slider's least.
const FIRST_POWER: f64 = 16.0;
/// The sweep ends after a count slower than this.
const SLOWEST_FPS: f64 = 5.0;
/// After a replay's last input, for its effects to play out.
const TAIL: f64 = 2.0;
/// A count is reported when the run stayed at it this long; a replay's
/// drags pass through many counts for a frame or two.
const DWELL_SECONDS: f64 = 1.0;
/// A frame is on time when it comes within this many refresh periods.
const LATE: f64 = 1.25;
/// A count is kept up with when this share of its frames is on time.
const KEPT_UP: f64 = 0.95;

enum Script {
    Sweep,
    Replay(Player),
}

#[derive(Clone, Copy, PartialEq)]
enum Phase {
    Warmup,
    Settle,
    Measure,
    Playing,
    Done,
}

/// What the loop does next for the benchmark.
pub enum Next {
    /// The sweep moved on to this many stars
    Stars(u32),
    Finish,
}

/// What the scene runs on this frame, from the benchmark.
pub struct Steer {
    pub params: Params,
    pub burst: bool,
    pub explode: bool,
}

/// A frame measured.
#[derive(Clone, Copy)]
struct Frame {
    stars: u32,
    /// The stars the frame asked for: more when GPU memory was full
    asked: u32,
    cpu: CpuTimes,
}

pub struct Bench {
    name: String,
    against: Option<String>,
    script: Script,
    phase: Phase,
    started: Option<Instant>,
    /// When the phase began
    since: Option<Instant>,
    params: Params,
    /// The first frame measured
    measured_from: u64,
    frames: Vec<Frame>,
    gpu: Vec<GpuTimes>,
    steps: Vec<Step>,
    run: Option<Step>,
}

/// Where the benchmark ran.
pub struct Setup {
    pub adapter: String,
    pub present_mode: String,
    pub refresh_hz: f64,
    pub surface: [u32; 2],
}

/// A benchmark run, as `perf/<name>.json` keeps it.
#[derive(Serialize, Deserialize)]
pub struct Summary {
    pub name: String,
    pub build: String,
    /// `sweep`, or the recording replayed
    #[serde(default)]
    pub script: String,
    pub adapter: String,
    pub present_mode: String,
    pub refresh_hz: f64,
    pub surface: [u32; 2],
    pub steps: Vec<Step>,
    /// A replay's every frame, all its star counts together
    #[serde(default)]
    pub run: Option<Step>,
}

/// One star count's numbers. Times are milliseconds.
#[derive(Serialize, Deserialize)]
pub struct Step {
    pub stars: u32,
    /// Fewer stars than asked for: GPU memory was full
    pub memory_full: bool,
    pub frames: usize,
    /// How long the run stayed at this count
    #[serde(default)]
    pub seconds: f64,
    pub fps: f64,
    /// The share of frames on time
    pub on_time: f64,
    pub interval: Stats,
    pub frame: Stats,
    /// The frame's work without the wait for the surface's next texture
    #[serde(default)]
    pub busy: Stats,
    pub pump: Stats,
    pub acquire: Stats,
    #[serde(default)]
    pub grow: Stats,
    pub ui: Stats,
    pub encode: Stats,
    pub present: Stats,
    /// None when the GPU cannot time its passes
    pub gpu_total: Option<Stats>,
    pub gpu_compute: Option<Stats>,
    pub gpu_draw: Option<Stats>,
    pub gpu_chrome: Option<Stats>,
}

#[derive(Clone, Copy, Default, Serialize, Deserialize)]
pub struct Stats {
    pub mean: f64,
    pub p50: f64,
    pub p95: f64,
    pub p99: f64,
    pub max: f64,
}

impl Stats {
    fn of(values: impl Iterator<Item = f64>) -> Option<Self> {
        let mut sorted: Vec<f64> = values.collect();
        if sorted.is_empty() {
            return None;
        }
        sorted.sort_by(f64::total_cmp);
        let n = sorted.len();
        let rank = |p: f64| sorted[((p * n as f64).ceil() as usize).clamp(1, n) - 1];
        Some(Self {
            mean: sorted.iter().sum::<f64>() / n as f64,
            p50: rank(0.5),
            p95: rank(0.95),
            p99: rank(0.99),
            max: sorted[n - 1],
        })
    }
}

impl Step {
    fn of(stars: u32, frames: &[Frame], gpu: &[GpuTimes], period: Duration) -> Self {
        let cpu = |part: fn(&CpuTimes) -> f64| {
            Stats::of(frames.iter().map(|f| part(&f.cpu))).unwrap_or_default()
        };
        let gpu = |part: fn(&GpuTimes) -> f64| Stats::of(gpu.iter().map(part));
        let interval = cpu(|c| c.interval);
        let late = period.as_secs_f64() * 1000.0 * LATE;
        let on_time = frames.iter().filter(|f| f.cpu.interval <= late).count();
        Step {
            stars,
            memory_full: frames.iter().any(|f| f.stars < f.asked),
            frames: frames.len(),
            seconds: frames.iter().map(|f| f.cpu.interval).sum::<f64>() / 1000.0,
            fps: 1000.0 / interval.mean.max(f64::EPSILON),
            on_time: on_time as f64 / frames.len().max(1) as f64,
            interval,
            frame: cpu(|c| c.frame),
            busy: cpu(|c| c.frame - c.acquire),
            pump: cpu(|c| c.pump),
            acquire: cpu(|c| c.acquire),
            grow: cpu(|c| c.grow),
            ui: cpu(|c| c.ui),
            encode: cpu(|c| c.encode),
            present: cpu(|c| c.present),
            gpu_total: gpu(|g| g.total),
            gpu_compute: gpu(|g| g.compute),
            gpu_draw: gpu(|g| g.draw),
            gpu_chrome: gpu(|g| g.chrome),
        }
    }

    fn trace(&self, message: &'static str) {
        tracing::info!(
            target: "perf",
            stars = self.stars,
            fps = self.fps,
            on_time = self.on_time,
            interval_p95_ms = self.interval.p95,
            interval_max_ms = self.interval.max,
            frame_ms = self.frame.mean,
            gpu_ms = self.gpu_total.map(|s| s.mean),
            "{message}"
        );
    }
}

impl Bench {
    /// A sweep, or a replay of `replay`.
    pub fn new(name: String, against: Option<String>, replay: Option<Player>) -> Self {
        Self {
            name,
            against,
            script: replay.map_or(Script::Sweep, Script::Replay),
            phase: Phase::Warmup,
            started: None,
            since: None,
            params: Params::default(),
            measured_from: 0,
            frames: Vec::new(),
            gpu: Vec::new(),
            steps: Vec::new(),
            run: None,
        }
    }

    /// What it does, for the console.
    pub fn describe(&self) -> String {
        match &self.script {
            Script::Sweep => "Benchmark: sweeping the star counts; the controls wait until it is done".to_owned(),
            Script::Replay(player) => format!(
                "Benchmark: replaying {} ({:.0} s); the controls wait until it is done",
                player.name,
                player.length()
            ),
        }
    }

    pub fn running(&self) -> bool {
        self.phase != Phase::Done
    }

    /// Before each frame: what the scene runs on while the benchmark runs,
    /// a replay's bursts and pull-aparts among it.
    pub fn steer(&mut self, now: Instant) -> Option<Steer> {
        if !self.running() {
            return None;
        }
        let elapsed = (now - *self.started.get_or_insert(now)).as_secs_f64();
        let mut steer = Steer {
            params: self.params,
            burst: false,
            explode: false,
        };
        if let Script::Replay(player) = &mut self.script {
            for event in player.due(elapsed) {
                match event.input {
                    Input::Params(params) => self.params = params,
                    Input::Burst => steer.burst = true,
                    // Twice in a frame is no change
                    Input::Explode => steer.explode = !steer.explode,
                }
            }
            steer.params = self.params;
        }
        Some(steer)
    }

    /// After each frame drawn, with the stars it drew.
    pub fn on_frame(
        &mut self,
        now: Instant,
        frame: u64,
        stars: u32,
        cpu: &CpuTimes,
        period: Duration,
    ) -> Option<Next> {
        let started = *self.started.get_or_insert(now);
        let in_phase = now - *self.since.get_or_insert(now);
        let measured = Frame {
            stars,
            asked: self.params.count,
            cpu: *cpu,
        };
        match self.phase {
            Phase::Warmup if now - started >= WARMUP => match self.script {
                Script::Sweep => Some(self.next_count(now)),
                Script::Replay(_) => {
                    self.phase = Phase::Playing;
                    self.measured_from = frame + 1;
                    None
                }
            },
            Phase::Settle if in_phase >= SETTLE => {
                self.phase = Phase::Measure;
                self.since = Some(now);
                self.measured_from = frame + 1;
                None
            }
            Phase::Measure => {
                self.frames.push(measured);
                if in_phase < MEASURE {
                    return None;
                }
                let step = Step::of(stars, &self.frames, &self.gpu, period);
                step.trace("step");
                let last = step.fps < SLOWEST_FPS
                    || step.memory_full
                    || self.next_stars() > MAX_STARS;
                self.steps.push(step);
                if last {
                    self.phase = Phase::Done;
                    Some(Next::Finish)
                } else {
                    Some(self.next_count(now))
                }
            }
            Phase::Playing => {
                self.frames.push(measured);
                let Script::Replay(player) = &self.script else {
                    return None;
                };
                let elapsed = (now - started).as_secs_f64();
                if !player.played() || elapsed < player.length() + TAIL {
                    return None;
                }
                self.summarise_replay(period);
                self.phase = Phase::Done;
                Some(Next::Finish)
            }
            _ => None,
        }
    }

    /// Each frame's GPU timings, as they arrive.
    pub fn on_gpu(&mut self, times: &GpuTimes) {
        if matches!(self.phase, Phase::Measure | Phase::Playing)
            && times.frame >= self.measured_from
        {
            self.gpu.push(*times);
        }
    }

    /// Writes the summary and prints it, against another run's when there
    /// is one.
    pub fn finish(&mut self, setup: Setup) {
        let summary = Summary {
            name: self.name.clone(),
            build: BUILD.to_owned(),
            script: match &self.script {
                Script::Sweep => "sweep".to_owned(),
                Script::Replay(player) => player.name.clone(),
            },
            adapter: setup.adapter,
            present_mode: setup.present_mode,
            refresh_hz: setup.refresh_hz,
            surface: setup.surface,
            steps: std::mem::take(&mut self.steps),
            run: self.run.take(),
        };
        let path = perf::path(&summary.name, "json");
        let written = serde_json::to_string_pretty(&summary)
            .map_err(Box::<dyn Error>::from)
            .and_then(|json| fs::write(&path, json).map_err(Into::into));
        match written {
            Ok(()) => println!("\nWrote {}", path.display()),
            Err(e) => eprintln!("the summary could not be written to {}: {e}", path.display()),
        }
        print_summary(&summary);

        // Unless told otherwise, a sweep is compared with `baseline` and a
        // replay of `<recording>` with `baseline-<recording>`, once they exist
        let (against, asked) = match (&self.against, &self.script) {
            (Some(name), _) => (name.clone(), true),
            (None, Script::Sweep) => ("baseline".to_owned(), false),
            (None, Script::Replay(player)) => (format!("baseline-{}", player.name), false),
        };
        if against == summary.name {
            return;
        }
        match read(&against) {
            Ok(base) => print_comparison(&base, &summary),
            Err(e) if asked => eprintln!("{against} could not be read: {e}"),
            Err(_) => {}
        }
    }

    fn next_stars(&self) -> u32 {
        2f64.powf(FIRST_POWER + self.steps.len() as f64 / 2.0).round() as u32
    }

    fn next_count(&mut self, now: Instant) -> Next {
        self.params = Params {
            count: self.next_stars(),
            ..Params::default()
        };
        self.phase = Phase::Settle;
        self.since = Some(now);
        self.frames.clear();
        self.gpu.clear();
        Next::Stars(self.params.count)
    }

    /// A replay's frames, star count by star count, and all together.
    fn summarise_replay(&mut self, period: Duration) {
        let mut counts: Vec<u32> = self.frames.iter().map(|f| f.stars).collect();
        counts.sort_unstable();
        counts.dedup();
        let steps = counts
            .iter()
            .map(|&stars| {
                let frames: Vec<Frame> =
                    self.frames.iter().filter(|f| f.stars == stars).copied().collect();
                let gpu: Vec<GpuTimes> =
                    self.gpu.iter().filter(|g| g.stars == stars).copied().collect();
                Step::of(stars, &frames, &gpu, period)
            })
            .collect();
        let most = counts.last().copied().unwrap_or(0);
        let run = Step::of(most, &self.frames, &self.gpu, period);
        run.trace("replay");
        self.steps = steps;
        self.run = Some(run);
    }
}

/// `--compare=<a>,<b>`: both runs, then how the second differs.
pub fn compare(a: &str, b: &str) -> Result<(), Box<dyn Error>> {
    let (a, b) = (read(a)?, read(b)?);
    print_summary(&a);
    print_summary(&b);
    print_comparison(&a, &b);
    Ok(())
}

fn read(name: &str) -> Result<Summary, Box<dyn Error>> {
    let path = perf::path(name, "json");
    let json = fs::read_to_string(&path).map_err(|e| format!("{}: {e}", path.display()))?;
    Ok(serde_json::from_str(&json)?)
}

/// The counts a run stayed at long enough to report.
fn dwelt(summary: &Summary) -> impl Iterator<Item = &Step> {
    summary
        .steps
        .iter()
        .filter(|step| step.seconds >= DWELL_SECONDS)
}

/// The most stars a run kept up with the display at.
fn most_on_time(summary: &Summary) -> Option<u32> {
    dwelt(summary)
        .filter(|step| step.on_time >= KEPT_UP)
        .map(|step| step.stars)
        .max()
}

fn print_summary(s: &Summary) {
    let script = if s.script.is_empty() || s.script == "sweep" {
        "sweep".to_owned()
    } else {
        format!("replay of {}", s.script)
    };
    println!(
        "\n{} ({script}): {} build, {}, {}x{} at {:.0} Hz, {}",
        s.name, s.build, s.adapter, s.surface[0], s.surface[1], s.refresh_hz, s.present_mode
    );
    println!(
        "{:>13} {:>6} {:>6} {:>8} {:>10} {:>7} {:>7} {:>7} {:>7} {:>7} {:>8} {:>7}",
        "stars", "secs", "fps", "on time", "frame p95", "worst", "cpu", "pump", "wait", "gpu", "compute", "draw"
    );
    for step in dwelt(s) {
        print_row(&group_thousands(step.stars), step);
    }
    let passed = s.steps.len() - dwelt(s).count();
    if passed > 0 {
        println!("{passed} more counts passed through for under {DWELL_SECONDS:.0} s each");
    }
    if let Some(run) = &s.run {
        print_row("every frame", run);
    }
    match most_on_time(s) {
        Some(stars) => println!("Most stars on time: {}", group_thousands(stars)),
        None => println!("Most stars on time: none of them"),
    }
    println!(
        "Milliseconds, means except frame p95 and worst. cpu is the loop's own work; wait is for the \
         surface's next texture, long when the GPU is behind."
    );
}

fn print_row(label: &str, step: &Step) {
    let gpu = |stats: &Option<Stats>| stats.map_or("-".to_owned(), |s| format!("{:.2}", s.mean));
    println!(
        "{:>13} {:>6.1} {:>6.1} {:>7.0}% {:>10.2} {:>7.1} {:>7.2} {:>7.2} {:>7.2} {:>7} {:>8} {:>7}{}",
        label,
        step.seconds,
        step.fps,
        step.on_time * 100.0,
        step.interval.p95,
        step.interval.max,
        step.busy.mean,
        step.pump.mean,
        step.acquire.mean,
        gpu(&step.gpu_total),
        gpu(&step.gpu_compute),
        gpu(&step.gpu_draw),
        if step.memory_full { "  GPU memory full" } else { "" },
    );
}

fn print_comparison(base: &Summary, run: &Summary) {
    println!("\n{} against {} (change in the mean)", run.name, base.name);
    let differs = [
        ("script", &base.script, &run.script),
        ("build", &base.build, &run.build),
        ("adapter", &base.adapter, &run.adapter),
        ("present mode", &base.present_mode, &run.present_mode),
    ];
    for (what, a, b) in differs {
        if a != b {
            println!("  Note: the {what} differs: {a} then {b}");
        }
    }
    if base.surface != run.surface {
        println!(
            "  Note: the window differs: {}x{} then {}x{}",
            base.surface[0], base.surface[1], run.surface[0], run.surface[1]
        );
    }
    println!(
        "{:>13} {:>7} {:>10} {:>7} {:>7} {:>7} {:>8} {:>7}",
        "stars", "fps", "frame p95", "worst", "cpu", "gpu", "compute", "draw"
    );
    let row = |label: &str, old: &Step, new: &Step| {
        println!(
            "{:>13} {:>7} {:>10} {:>7} {:>7} {:>7} {:>8} {:>7}",
            label,
            change(old.fps, new.fps),
            change(old.interval.p95, new.interval.p95),
            change(old.interval.max, new.interval.max),
            change(old.busy.mean, new.busy.mean),
            gpu_change(&old.gpu_total, &new.gpu_total),
            gpu_change(&old.gpu_compute, &new.gpu_compute),
            gpu_change(&old.gpu_draw, &new.gpu_draw),
        );
    };
    for step in dwelt(run) {
        if let Some(old) = dwelt(base).find(|old| old.stars == step.stars) {
            row(&group_thousands(step.stars), old, step);
        }
    }
    if let (Some(old), Some(new)) = (&base.run, &run.run) {
        row("every frame", old, new);
    }
    let most = |s: &Summary| most_on_time(s).map_or("none".to_owned(), group_thousands);
    println!("Most stars on time: {} then {}", most(base), most(run));
}

fn change(a: f64, b: f64) -> String {
    if a > 0.0 {
        format!("{:+.0}%", (b - a) / a * 100.0)
    } else {
        "-".to_owned()
    }
}

fn gpu_change(a: &Option<Stats>, b: &Option<Stats>) -> String {
    match (a, b) {
        (Some(a), Some(b)) => change(a.mean, b.mean),
        _ => "-".to_owned(),
    }
}
