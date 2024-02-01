// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use anyhow::Context;
use camino::{Utf8Path, Utf8PathBuf};
use serde::{Deserialize, Serialize};

/// A directory that should be added to the target archive
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct TargetDirectory(pub Utf8PathBuf);

/// A package that should be added to the target archive
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct TargetPackage(pub Utf8PathBuf);

/// A pair of paths, mapping from a file or directory on the host to the target
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct MappedPath {
    /// Source path.
    pub from: Utf8PathBuf,
    /// Destination path.
    pub to: Utf8PathBuf,
}

/// All possible inputs which are used to construct Omicron packages
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum BuildInput {
    /// Adds a single file, which is stored in-memory.
    ///
    /// This is mostly used as a way to cache metadata.
    AddInMemoryFile {
        dst_path: Utf8PathBuf,
        contents: String,
    },

    /// Add a single directory to the target archive.
    ///
    /// This directory doesn't need to exist on the build host.
    AddDirectory(TargetDirectory),

    /// Add a file directly from source to target.
    AddFile {
        /// Describes the files being added.
        mapped_path: MappedPath,

        /// The length of the file.
        ///
        /// Q: Is this necessary? Aren't we already storing the file itself,
        /// making this field redundant?
        ///
        /// A: We use it to help caching, on the "known cache miss" case. In
        /// *most* circumstances where a file has been edited, the length
        /// changes too. Comparing u64s is significantly faster than hashing,
        /// in this situation.
        len: u64,
    },

    /// Add a dowloaded file from source to target.
    ///
    /// This is similar to "AddFile", though it may require downloading an input
    /// first.
    AddBlob {
        path: MappedPath,
        blob: crate::blob::Source,
    },

    /// Add a package from source to target.
    ///
    /// This is similar to "AddFile", though it requires unpacking the package
    /// and re-packaging it into the target.
    AddPackage(TargetPackage),
}

impl BuildInput {
    /// If the input has a path on the host machine, return it.
    pub fn input_path(&self) -> Option<&Utf8Path> {
        match self {
            // This file is stored in-memory, it isn't cached.
            BuildInput::AddInMemoryFile { .. } => None,
            // This path doesn't need to exist on the host, it's just fabricated
            // on the target.
            BuildInput::AddDirectory(_target) => None,
            BuildInput::AddFile { mapped_path, .. } => Some(&mapped_path.from),
            BuildInput::AddBlob { path, .. } => Some(&path.from),
            BuildInput::AddPackage(target_package) => Some(&target_package.0),
        }
    }

    pub fn add_file(mapped_path: MappedPath) -> anyhow::Result<Self> {
        let src = &mapped_path.from;
        let len = src
            .metadata()
            .with_context(|| format!("Failed to get length of {src}"))?
            .len();

        Ok(Self::AddFile { mapped_path, len })
    }
}

/// A ordered collection of build inputs.
pub struct BuildInputs(pub Vec<BuildInput>);

impl BuildInputs {
    pub fn new() -> Self {
        Self(vec![])
    }
}

impl Default for BuildInputs {
    fn default() -> Self {
        Self::new()
    }
}
