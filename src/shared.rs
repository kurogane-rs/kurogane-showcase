//! Shared state between the panes, Chromium and the main loop.
//!
//! Commands and scheduler events wake the loop which handles the work
//! on its own thread.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Mutex, OnceLock, PoisonError};
use std::time::Instant;

use kurogane::{AppHandle, StreamResponder};
use winit::event_loop::EventLoopProxy;

use crate::galaxy::Params;

pub enum UserEvent {
    /// Chromium asked to be pumped by then
    Pump(Instant),
    /// Switch between the docked and the exploded layout
    Explode,
}

// Shared state used by the panes and the main loop
#[derive(Default)]
pub struct Shared {
    wake: OnceLock<EventLoopProxy<UserEvent>>,
    params: Mutex<Params>,
    burst: AtomicBool,
    /// The charts panes' open telemetry streams
    telemetry: Mutex<Vec<StreamResponder>>,
    /// How often Chromium asked to be pumped since the loop last looked
    pump_requests: AtomicU32,
    /// The console's recent lines for a console that opens late
    backlog: Mutex<VecDeque<String>>,
    /// Commands and console lines since the last loop update
    commands: AtomicU32,
    lines: AtomicU32,
}

const BACKLOG: usize = 200;

impl Shared {
    pub fn connect(&self, proxy: EventLoopProxy<UserEvent>) {
        let _ = self.wake.set(proxy);
    }

    fn send(&self, event: UserEvent) {
        if let Some(proxy) = self.wake.get() {
            let _ = proxy.send_event(event);
        }
    }

    /// Chromium scheduler pumps by its deadline.
    pub fn pump_by(&self, deadline: Instant) {
        self.pump_requests.fetch_add(1, Ordering::Relaxed);
        self.send(UserEvent::Pump(deadline));
    }

    pub fn take_pump_requests(&self) -> u32 {
        self.pump_requests.swap(0, Ordering::Relaxed)
    }

    pub fn explode(&self) {
        self.send(UserEvent::Explode);
    }

    pub fn params(&self) -> Params {
        *self.params.lock().unwrap_or_else(PoisonError::into_inner)
    }

    pub fn set_params(&self, params: Params) {
        *self.params.lock().unwrap_or_else(PoisonError::into_inner) = params;
    }

    pub fn burst(&self) {
        self.burst.store(true, Ordering::Relaxed);
    }

    pub fn take_burst(&self) -> bool {
        self.burst.swap(false, Ordering::Relaxed)
    }

    /// A charts pane opened the telemetry stream.
    pub fn watch(&self, responder: StreamResponder) {
        self.telemetry
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(responder);
    }

    /// A pane's command reached Rust.
    pub fn count_command(&self) {
        self.commands.fetch_add(1, Ordering::Relaxed);
    }

    /// Commands and console lines since the last call.
    pub fn take_traffic(&self) -> (u32, u32) {
        (
            self.commands.swap(0, Ordering::Relaxed),
            self.lines.swap(0, Ordering::Relaxed),
        )
    }

    /// Tells the console something, now and to a console that opens later.
    pub fn log(&self, app: &AppHandle, line: String) {
        app.broadcast_json("console.line", &line);
        self.lines.fetch_add(1, Ordering::Relaxed);
        let mut backlog = self.backlog.lock().unwrap_or_else(PoisonError::into_inner);
        if backlog.len() == BACKLOG {
            backlog.pop_front();
        }
        backlog.push_back(line);
    }

    pub fn backlog(&self) -> Vec<String> {
        self.backlog
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .iter()
            .cloned()
            .collect()
    }

    /// Sends telemetry to open streams and removes closed ones.
    pub fn send_telemetry(&self, bytes: &[u8]) -> usize {
        let mut streams = self
            .telemetry
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        streams.retain(|stream| stream.send_data(bytes).is_ok());
        streams.len()
    }
}
