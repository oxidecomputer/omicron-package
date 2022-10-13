// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Configuration for a package.

use crate::package::{Package, PackageOutput};
use crate::target::Target;
use serde_derive::Deserialize;
use std::collections::BTreeMap;
use std::path::Path;
use thiserror::Error;

/// Describes the configuration for a set of packages.
#[derive(Deserialize, Debug)]
pub struct Config {
    /// Packages to be built and installed.
    #[serde(default, rename = "package")]
    pub packages: BTreeMap<String, Package>,
}

impl Config {
    /// Returns target packages to be assembled on the builder machine.
    pub fn packages_to_build(&self, target: &Target) -> BTreeMap<&String, &Package> {
        self.packages
            .iter()
            .filter(|(_, pkg)| target.includes_package(&pkg))
            .map(|(name, pkg)| (name, pkg))
            .collect()
    }

    /// Returns target packages which should execute on the deployment machine.
    pub fn packages_to_deploy(&self, target: &Target) -> BTreeMap<&String, &Package> {
        let all_packages = self.packages_to_build(target);
        all_packages
            .into_iter()
            .filter(|(_, pkg)| match pkg.output {
                PackageOutput::Zone { intermediate_only } => !intermediate_only,
                PackageOutput::Tarball => true,
            })
            .collect()
    }
}

/// Errors which may be returned when parsing the server configuration.
#[derive(Error, Debug)]
pub enum ParseError {
    #[error("Cannot parse toml: {0}")]
    Toml(#[from] toml::de::Error),
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
}

/// Parses a path in the filesystem into a package [`Config`].
pub fn parse<P: AsRef<Path>>(path: P) -> Result<Config, ParseError> {
    let contents = std::fs::read_to_string(path.as_ref())?;
    let cfg = toml::from_str::<Config>(&contents)?;
    Ok(cfg)
}
