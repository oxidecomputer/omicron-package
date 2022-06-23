// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Configuration for a package.

use crate::package::Package;
use serde_derive::Deserialize;
use std::collections::BTreeMap;
use std::path::Path;
use thiserror::Error;

/// Describes the origin of an externally-built package.
#[derive(Deserialize, Debug)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum ExternalPackageSource {
    /// Downloads the package from the following URL:
    ///
    /// <https://buildomat.eng.oxide.computer/public/file/oxidecomputer/REPO/image/COMMIT/PACKAGE>
    Prebuilt {
        repo: String,
        commit: String,
        sha256: String,
    },
    /// Expects that a package will be manually built and placed into the output
    /// directory.
    Manual,
}

/// Describes a package which originates from outside this repo.
#[derive(Deserialize, Debug)]
pub struct ExternalPackage {
    #[serde(flatten)]
    pub package: Package,

    pub source: ExternalPackageSource,
}

/// Describes the configuration for a set of packages.
#[derive(Deserialize, Debug)]
pub struct Config {
    /// Packages to be built and installed.
    #[serde(default, rename = "package")]
    pub packages: BTreeMap<String, Package>,

    /// Packages to be installed, but which have been created outside this
    /// repository.
    #[serde(default, rename = "external_package")]
    pub external_packages: BTreeMap<String, ExternalPackage>,
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
