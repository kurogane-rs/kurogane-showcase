//! Records what steers the scene, and plays it back.
//!
//! `--record=<name>` keeps every change the loop sees, from the panes or the
//! keyboard, with its time since the first frame, in `perf/<name>.input.json`.
//! A benchmark with `--replay=<name>` makes the same changes at the same
//! times, whatever the controls do meanwhile.

use std::error::Error;
use std::fs;
use std::time::Instant;

use serde::{Deserialize, Serialize};

use crate::galaxy::Params;
use crate::perf;

/// One change to the scene.
#[derive(Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Input {
    /// The sliders, all of them
    Params(Params),
    Burst,
    /// Pull apart, or dock again
    Explode,
}

#[derive(Serialize, Deserialize)]
pub struct Event {
    /// Seconds since the first frame
    pub at: f64,
    pub input: Input,
}

fn file(name: &str) -> std::path::PathBuf {
    perf::path(name, "input.json")
}

pub struct Recorder {
    name: String,
    started: Option<Instant>,
    /// The sliders as last recorded
    last: Option<Params>,
    events: Vec<Event>,
}

impl Recorder {
    pub fn new(name: String) -> Self {
        Self {
            name,
            started: None,
            last: None,
            events: Vec::new(),
        }
    }

    pub fn file(&self) -> std::path::PathBuf {
        file(&self.name)
    }

    /// Each frame: keeps the sliders when they have changed, and the first
    /// frame's as they start.
    pub fn params(&mut self, now: Instant, params: Params) {
        if self.last != Some(params) {
            self.last = Some(params);
            self.push(now, Input::Params(params));
        }
    }

    pub fn push(&mut self, now: Instant, input: Input) {
        let at = (now - *self.started.get_or_insert(now)).as_secs_f64();
        self.events.push(Event { at, input });
    }

    /// Writes the recording, when the app is done.
    pub fn save(&self) {
        let path = self.file();
        let written = serde_json::to_string_pretty(&self.events)
            .map_err(Box::<dyn Error>::from)
            .and_then(|json| {
                if let Some(dir) = path.parent() {
                    fs::create_dir_all(dir)?;
                }
                fs::write(&path, json).map_err(Into::into)
            });
        match written {
            Ok(()) => println!(
                "Recorded {} inputs over {:.1} s to {}",
                self.events.len(),
                self.events.last().map_or(0.0, |event| event.at),
                path.display()
            ),
            Err(e) => eprintln!("the recording could not be written to {}: {e}", path.display()),
        }
    }
}

pub struct Player {
    pub name: String,
    events: Vec<Event>,
    /// The first event not yet played
    next: usize,
}

impl Player {
    pub fn load(name: &str) -> Result<Self, Box<dyn Error>> {
        let path = file(name);
        let json = fs::read_to_string(&path).map_err(|e| format!("{}: {e}", path.display()))?;
        Ok(Self {
            name: name.to_owned(),
            events: serde_json::from_str(&json)?,
            next: 0,
        })
    }

    /// The events due by `elapsed` seconds since the first frame.
    pub fn due(&mut self, elapsed: f64) -> &[Event] {
        let from = self.next;
        while self.events.get(self.next).is_some_and(|event| event.at <= elapsed) {
            self.next += 1;
        }
        &self.events[from..self.next]
    }

    /// When the last event happens, in seconds since the first frame.
    pub fn length(&self) -> f64 {
        self.events.last().map_or(0.0, |event| event.at)
    }

    pub fn played(&self) -> bool {
        self.next == self.events.len()
    }
}
