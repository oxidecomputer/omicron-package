// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Describes utilities for relaying progress to end-users.

use once_cell::sync::OnceCell;
use slog::Logger;
use std::borrow::Cow;

/// Trait for propagating progress information while constructing the package.
pub trait Progress {
    /// Updates the message displayed regarding progress constructing
    /// the package.
    fn set_message(&self, _msg: Cow<'static, str>) {}

    /// Returns the debug logger
    fn get_log(&self) -> &Logger;

    /// Increments the number of things which need to be completed
    fn increment_total(&self, _delta: u64) {}

    /// Increments the number of things which have completed.
    fn increment_completed(&self, _delta: u64) {}

    /// Returns a new [`Progress`] which will report progress for a sub task.
    fn sub_progress(&self, _total: u64) -> Box<dyn Progress> {
        Box::new(NoProgress::new())
    }
}

/// Implements [`Progress`] as a no-op.
pub struct NoProgress {
    log: OnceCell<slog::Logger>,
}

impl NoProgress {
    pub const fn new() -> Self {
        Self {
            log: OnceCell::new(),
        }
    }
}

impl Default for NoProgress {
    fn default() -> Self {
        Self::new()
    }
}

impl Progress for NoProgress {
    fn get_log(&self) -> &Logger {
        self.log
            .get_or_init(|| slog::Logger::root(slog::Discard, slog::o!()))
    }
}
