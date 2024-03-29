// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! A timer to help track how long build phases take

use anyhow::{bail, Result};
use slog::Logger;
use std::borrow::Cow;
use tokio::time::{Duration, Instant};

type CowStr = Cow<'static, str>;

struct PhaseStart {
    name: CowStr,
    time: Instant,
}

struct PhaseEnd {
    name: Option<CowStr>,
    time: Instant,
}

impl PhaseStart {
    fn new(name: CowStr) -> Self {
        Self {
            name,
            time: Instant::now(),
        }
    }

    fn finish(self, name: Option<CowStr>) -> Phase {
        Phase {
            start: self,
            end: PhaseEnd {
                name,
                time: Instant::now(),
            },
        }
    }
}

/// Describes a single phase of time, with a start time, end time, and label.
pub struct Phase {
    start: PhaseStart,
    end: PhaseEnd,
}

impl Phase {
    pub fn name(&self) -> &str {
        &self.start.name
    }

    pub fn end_label(&self) -> Option<&str> {
        self.end.name.as_deref()
    }

    pub fn duration(&self) -> Duration {
        self.end.time.duration_since(self.start.time)
    }
}

/// A utility for tracking a series of related timers.
pub struct BuildTimer {
    current: Option<PhaseStart>,
    past: Vec<Phase>,
}

impl BuildTimer {
    pub fn new() -> Self {
        Self {
            current: None,
            past: vec![],
        }
    }

    /// Starts a new timer, ending a prior phase if one was in progress.
    pub fn start<S: Into<CowStr>>(&mut self, s: S) {
        // If a prior phase was ongoing, mark it completed
        if self.current.is_some() {
            let _ = self.finish();
        }
        self.current = Some(PhaseStart::new(s.into()));
    }

    /// Terminates the current phase with a label.
    pub fn finish_with_label<S: Into<CowStr>>(&mut self, label: S) -> Result<()> {
        self.finish_inner(Some(label.into()))
    }

    /// Terminates the current phase.
    pub fn finish(&mut self) -> Result<()> {
        self.finish_inner(Option::<CowStr>::None)
    }

    fn finish_inner(&mut self, label: Option<CowStr>) -> Result<()> {
        let Some(current) = self.current.take() else {
            bail!("No build phase in progress");
        };
        self.past.push(current.finish(label));
        Ok(())
    }

    /// Returns all previously completed phases.
    pub fn completed(&self) -> &Vec<Phase> {
        &self.past
    }

    /// A helper for logging all [Self::completed] phases.
    pub fn log_all(&self, log: &Logger) {
        for phase in self.completed() {
            let name = phase.name();
            let s = phase.duration().as_secs_f64();
            let label = if let Some(label) = phase.end_label() {
                format!(" -- {label}")
            } else {
                "".to_string()
            };
            slog::info!(log, "Phase {name} took {s:.6}s{label}");
        }
    }
}
