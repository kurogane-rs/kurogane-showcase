//! Kurogane Showcase
//!
//! A Rust app that drives a GPU scene and three Chromium panes.

mod galaxy;
mod gpu;
mod host;
mod layout;
mod shared;
mod ui;

use std::error::Error;
use std::sync::Arc;
use std::time::Instant;

use kurogane::{
    App, AppHandle, IpcError, Key, KeyDecision, PumpRequest, StreamHandler, StreamResponder,
};
use serde_json::Value;
use tracing_subscriber::EnvFilter;
use tracing_subscriber::filter::LevelFilter;

use galaxy::Params;
use shared::Shared;

const PANES_DIR: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/panes");

fn main() -> Result<(), Box<dyn Error>> {
    logging();

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
    host::run(instance, shared)
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

/// Kurogane's warnings and errors; `RUST_LOG` for more.
fn logging() {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::builder()
                .with_default_directive(LevelFilter::WARN.into())
                .from_env_lossy(),
        )
        .without_time()
        .with_target(false)
        .log_internal_errors(false)
        .init();
}
