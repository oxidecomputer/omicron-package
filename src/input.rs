// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::collections::BTreeSet;

/// A directory that should be added to the target archive
#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Serialize, Deserialize)]
pub struct TargetDirectory(pub PathBuf);

/// A package that should be added to the target archive
#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Serialize, Deserialize)]
pub struct TargetPackage(pub PathBuf);

/// A pair of paths, mapping from a file or directory on the host to the target
#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Serialize, Deserialize)]
pub struct MappedPath {
    /// Source path.
    pub from: PathBuf,
    /// Destination path.
    pub to: PathBuf,
}

/// All possible inputs which are used to construct Omicron packages
#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Serialize, Deserialize)]
pub enum BuildInput {
    /// Adds a single file, which is stored in-memory.
    ///
    /// This is mostly used as a way to cache metadata.
    AddInMemoryFile {
        dst_path: PathBuf,
        contents: String,
    },

    /// Add a single directory to the target archive.
    ///
    /// This directory doesn't need to exist on the build host.
    AddDirectory(TargetDirectory),

    /// Add a file directly from source to target.
    AddFile(MappedPath),

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
    pub fn input_path(&self) -> Option<&Path> {
        match self {
            // This file is stored in-memory, it isn't cached.
            BuildInput::AddInMemoryFile { .. } => None,
            // This path doesn't need to exist on the host, it's just fabricated
            // on the target.
            BuildInput::AddDirectory(_target) => None,
            BuildInput::AddFile(mapped_path) => Some(&mapped_path.from),
            BuildInput::AddBlob { path, .. } => Some(&path.from),
            BuildInput::AddPackage(target_package) => Some(&target_package.0),
        }
    }
}

/// A orderd, unique collection of build inputs.
///
/// When referring to multiple inputs, it's important to preserve the properties
/// of uniqueness and order for build determinism.
pub struct BuildInputs(pub BTreeSet<BuildInput>);

impl BuildInputs {
    pub fn new() -> Self {
        Self(BTreeSet::new())
    }
}
