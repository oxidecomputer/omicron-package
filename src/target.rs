// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use crate::package::Package;
use std::collections::BTreeMap;

/// A target describes what platform and configuration we're trying
/// to deploy on.
#[derive(Clone, Debug, Default)]
pub struct Target(pub BTreeMap<String, String>);

impl Target {
    // Returns true if this target should include the package.
    pub(crate) fn includes_package(&self, pkg: &Package) -> bool {
        let valid_targets = if let Some(targets) = &pkg.only_for_targets {
            // If targets are specified for the packages, filter them.
            targets
        } else {
            // If no targets are specified, assume the package should be
            // included by default.
            return true;
        };

        // For each of the targets permitted by the package, check if
        // the current target matches.
        for (k, v) in valid_targets {
            let target_value = if let Some(target_value) = self.0.get(k) {
                target_value
            } else {
                return false;
            };

            if target_value != v {
                return false;
            };
        }
        true
    }
}

impl std::fmt::Display for Target {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        for (key, value) in &self.0 {
            write!(f, "{}={} ", key, value)?;
        }
        Ok(())
    }
}

#[derive(thiserror::Error, Debug)]
pub enum TargetParseError {
    #[error("Cannot parse key-value pair out of '{0}'")]
    MissingEquals(String),
}

impl std::str::FromStr for Target {
    type Err = TargetParseError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let kvs = s
            .split_whitespace()
            .map(|kv| {
                kv.split_once('=')
                    .ok_or_else(|| TargetParseError::MissingEquals(kv.to_string()))
                    .map(|(k, v)| (k.to_string(), v.to_string()))
            })
            .collect::<Result<BTreeMap<String, String>, _>>()?;
        Ok(Target(kvs))
    }
}
