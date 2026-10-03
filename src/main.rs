//! Kurogane Showcase
//!
//! A Rust app that drives a GPU scene and three Chromium panes.

mod bench;
mod galaxy;
mod gpu;
mod gpu_timer;
mod host;
mod input;
mod layout;
mod perf;
mod shared;
mod ui;

use std::error::Error;
use std::fs::{self, File};
use std::path::Path;
use std::sync::Arc;
use std::time::Instant;

use kurogane::{
    App, AppHandle, IpcError, Key, KeyDecision, PumpRequest, StreamHandler, StreamResponder,
};
use serde_json::Value;
use tracing_appender::non_blocking::{NonBlockingBuilder, WorkerGuard};
use tracing_subscriber::filter::LevelFilter;
use tracing_subscriber::prelude::*;
use tracing_subscriber::{EnvFilter, fmt};

use bench::Bench;
use galaxy::Params;
use input::{Player, Recorder};
use perf::Options;
use shared::Shared;

const PANES_DIR: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/panes");

fn main() -> Result<(), Box<dyn Error>> {
    let options = Options::from_args();
    if let Some((a, b)) = &options.compare {
        return bench::compare(a, b);
    }
    if options.replay.is_some() && options.bench.is_none() {
        return Err("--replay=<recording> plays back in a benchmark: add --bench=<name>".into());
    }
    let replay = options.replay.as_deref().map(Player::load).transpose()?;
    // Flushes the trace when the app is done
    let _trace = logging(options.trace_path().as_deref());
    let bench = options
        .bench
        .map(|name| Bench::new(name, options.against, replay));
    // A benchmark ignores the controls, so there is nothing to record
    let recorder = options.record.filter(|_| bench.is_none()).map(Recorder::new);

    let shared = Arc::new(Shared::default());
    let instance = App::new(PANES_DIR)
        .scheduler({
            let shared = shared.clone();
            move |request: PumpRequest| shared.pump_by(request.deadline(Instant::now()))
        })
        // The controls' commands; each one also lights the wire it travels
        .command("sim.get", {
            let shared = shared.clone();
            move |_: Value, _: &AppHandle| {
                shared.count_command();
                Ok(shared.params())
            }
        })
        .command("sim.set", {
            let shared = shared.clone();
            move |params: Params, _: &AppHandle| {
                shared.count_command();
                shared.set_params(params);
                Ok(Value::Null)
            }
        })
        .command("sim.burst", {
            let shared = shared.clone();
            move |_: Value, _: &AppHandle| {
                shared.count_command();
                shared.burst();
                Ok(Value::Null)
            }
        })
        .command("view.explode", {
            let shared = shared.clone();
            move |_: Value, _: &AppHandle| {
                shared.count_command();
                shared.explode();
                Ok(Value::Null)
            }
        })
        .command("console.backlog", {
            let shared = shared.clone();
            move |_: Value, _: &AppHandle| Ok(shared.backlog())
        })
        .stream("telemetry", {
            let shared = shared.clone();
            move || Telemetry {
                shared: shared.clone(),
            }
        })
        // E pulls the window apart wherever the keyboard is, a pane included
        .on_key({
            let shared = shared.clone();
            move |press, _| {
                if press.key() == Key::Char('E')
                    && press.modifiers().none()
                    && !press.in_editable_field()
                {
                    shared.explode();
                    KeyDecision::Consume
                } else {
                    KeyDecision::Default
                }
            }
        })
        .start_embedded()?;
    host::run(instance, shared, bench, recorder)
}

/// A charts pane's telemetry stream: the loop sends on it every frame.
struct Telemetry {
    shared: Arc<Shared>,
}

impl StreamHandler for Telemetry {
    fn on_opened(&mut self, responder: &StreamResponder) -> Result<(), IpcError> {
        self.shared.watch(responder.clone());
        Ok(())
    }

    fn on_chunk(&mut self, _: &[u8], _: &StreamResponder) -> Result<(), IpcError> {
        Ok(())
    }
}

/// Kurogane's warnings and errors; `RUST_LOG` for more. With a trace, the
/// trace file also gets everything at info and above, every frame's timings
/// among it, as JSON lines.
fn logging(trace: Option<&Path>) -> Option<WorkerGuard> {
    let terminal = fmt::layer()
        .without_time()
        .with_target(false)
        .log_internal_errors(false)
        .with_filter(
            EnvFilter::builder()
                .with_default_directive(LevelFilter::WARN.into())
                .from_env_lossy()
                // Every frame's timings belong in the trace, not the terminal
                .add_directive("perf=off".parse().expect("a valid directive")),
        );

    // Helper processes run this too; only the browser process traces
    let file = trace
        .filter(|_| kurogane::is_browser_process())
        .and_then(|path| match open_trace(path) {
            Ok(file) => {
                println!("Tracing to {}", path.display());
                Some(file)
            }
            Err(e) => {
                eprintln!("the trace could not be opened at {}: {e}", path.display());
                None
            }
        });
    // Written on a thread of its own, so the loop never waits on the disk
    let (writer, guard) = match file {
        Some(file) => {
            let (writer, guard) = NonBlockingBuilder::default().lossy(false).finish(file);
            (Some(writer), Some(guard))
        }
        None => (None, None),
    };
    let traced = writer.map(|writer| {
        fmt::layer()
            .json()
            .flatten_event(true)
            .with_span_list(false)
            .with_writer(writer)
            .with_filter(
                EnvFilter::builder()
                    .with_default_directive(LevelFilter::INFO.into())
                    .from_env_lossy()
                    .add_directive("perf=info".parse().expect("a valid directive")),
            )
    });

    tracing_subscriber::registry()
        .with(terminal)
        .with(traced)
        .init();
    guard
}

fn open_trace(path: &Path) -> std::io::Result<File> {
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir)?;
    }
    File::create(path)
}
