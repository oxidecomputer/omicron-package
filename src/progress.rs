// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Describes utilities for relaying progress to end-users.

use std::borrow::Cow;

/// Trait for propagating progress information while constructing the package.
pub trait Progress {
    /// Updates the message displayed regarding progress constructing
    /// the package.
    fn set_message(&self, msg: Cow<'static, str>);

    /// Increments the number of things which have completed.
    fn increment(&self, delta: u64);

    /// Returns a new [`Progress`] which will report progress for a sub task.
    fn sub_progress(&self, _total: u64) -> Box<dyn Progress> {
        Box::new(NoProgress)
    }
}

/// Implements [`Progress`] as a no-op.
pub struct NoProgress;
impl Progress for NoProgress {
    fn set_message(&self, _msg: Cow<'static, str>) {}
    fn increment(&self, _delta: u64) {}
}
