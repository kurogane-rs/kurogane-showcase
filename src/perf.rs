//! What the loop measures, and the switches that write it down.
//!
//! Each stage of a frame is a tracing span, so a trace shows which stage
//! Kurogane's own events happened in, and the stage's milliseconds go into
//! the frame's record. With a trace open, every frame's record is one JSON
//! line in `perf/<name>.jsonl`.

use std::path::PathBuf;

/// Where traces and benchmark summaries go.
const PERF_DIR: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/perf");

/// The build the numbers come from: debug numbers are mostly unoptimised Rust.
pub const BUILD: &str = if cfg!(debug_assertions) {
    "debug"
} else {
    "release"
};

/// The showcase's switches, given after `--`:
/// `kurogane run --release -- --bench=baseline`.
///
/// Chromium ignores switches it does not know and passes none of these on to
/// its helper processes.
#[derive(Default)]
pub struct Options {
    /// `--trace=<name>`: every frame's timings in `perf/<name>.jsonl`
    pub trace: Option<String>,
    /// `--bench=<name>`: the benchmark, traced, summarised in
    /// `perf/<name>.json`; the window closes when it is done
    pub bench: Option<String>,
    /// `--replay=<recording>`: the benchmark replays a recording instead of
    /// stepping through the star counts
    pub replay: Option<String>,
    /// `--record=<name>`: the controls' changes, kept in
    /// `perf/<name>.input.json` for a benchmark to replay
    pub record: Option<String>,
    /// `--against=<name>`: the benchmark to compare with; by default
    /// `baseline`, or `baseline-<recording>` for a replay
    pub against: Option<String>,
    /// `--compare=<a>,<b>`: compares two benchmarks without opening a window
    pub compare: Option<(String, String)>,
}

impl Options {
    pub fn from_args() -> Self {
        let mut options = Self::default();
        for arg in std::env::args_os().skip(1) {
            let arg = arg.to_string_lossy();
            let Some((key, value)) = arg.strip_prefix("--").and_then(|a| a.split_once('=')) else {
                continue;
            };
            let value = value.to_owned();
            match key {
                "trace" => options.trace = Some(value),
                "bench" => options.bench = Some(value),
                "replay" => options.replay = Some(value),
                "record" => options.record = Some(value),
                "against" => options.against = Some(value),
                "compare" => {
                    options.compare = value
                        .split_once(',')
                        .map(|(a, b)| (a.to_owned(), b.to_owned()));
                }
                _ => {}
            }
        }
        options
    }

    /// The trace file, for a trace or a benchmark.
    pub fn trace_path(&self) -> Option<PathBuf> {
        self.bench
            .as_ref()
            .or(self.trace.as_ref())
            .map(|name| path(name, "jsonl"))
    }
}

/// A run's file in `perf/`.
pub fn path(name: &str, extension: &str) -> PathBuf {
    PathBuf::from(PERF_DIR).join(format!("{name}.{extension}"))
}

/// Runs `$body` inside the tracing span `$name` and adds its milliseconds
/// to `$slot`.
macro_rules! stage {
    ($slot:expr, $name:literal, $body:expr) => {{
        let _span = tracing::info_span!($name).entered();
        let started = std::time::Instant::now();
        let out = $body;
        $slot += started.elapsed().as_secs_f64() * 1000.0;
        out
    }};
}
pub(crate) use stage;

/// One frame's time on the CPU, stage by stage, in milliseconds.
#[derive(Clone, Copy, Default)]
pub struct CpuTimes {
    /// From the previous frame to this one
    pub interval: f64,
    /// All of this frame's work on the loop, the stages below and the rest
    pub frame: f64,
    /// In Chromium's pumps since the previous frame
    pub pump: f64,
    /// Springs stepped and panes moved
    pub layout: f64,
    /// Waiting for the surface's next texture: long when the GPU is behind
    pub acquire: f64,
    /// Star buffers added for a higher star count
    pub grow: f64,
    /// egui's chrome laid out and tessellated
    pub ui: f64,
    /// The compute and render passes recorded
    pub encode: f64,
    pub submit: f64,
    pub present: f64,
    /// Histograms and GPU timings taken from mapped buffers
    pub readback: f64,
    /// The charts' stream sent
    pub telemetry: f64,
}

impl CpuTimes {
    pub fn trace(&self, frame: u64, stars: u32) {
        tracing::info!(
            target: "perf",
            frame,
            stars,
            interval_ms = self.interval,
            frame_ms = self.frame,
            pump_ms = self.pump,
            layout_ms = self.layout,
            acquire_ms = self.acquire,
            grow_ms = self.grow,
            ui_ms = self.ui,
            encode_ms = self.encode,
            submit_ms = self.submit,
            present_ms = self.present,
            readback_ms = self.readback,
            telemetry_ms = self.telemetry,
            "frame"
        );
    }
}
