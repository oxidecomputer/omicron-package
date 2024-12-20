// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Utility for bundling target binaries as tarfiles.

use crate::archive::{
    add_package_to_zone_archive, create_tarfile, open_tarfile, ArchiveBuilder, AsyncAppendFile,
    Encoder,
};
use crate::blob::{self, BLOB};
use crate::cache::{Cache, CacheError};
use crate::config::{PackageName, ServiceName};
use crate::input::{BuildInput, BuildInputs, MappedPath, TargetDirectory, TargetPackage};
use crate::progress::{NoProgress, Progress};
use crate::target::Target;
use crate::timer::BuildTimer;

use anyhow::{anyhow, bail, Context, Result};
use camino::{Utf8Path, Utf8PathBuf};
use flate2::write::GzEncoder;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::convert::TryFrom;
use std::fs::File;
use tar::Builder;
use tokio::io::{AsyncSeekExt, AsyncWriteExt};

// Returns the path as it should be placed within an archive, by
// prepending "root/".
//
// Example:
// - /opt/oxide -> root/opt/oxide
fn zone_archive_path(path: &Utf8Path) -> Result<Utf8PathBuf> {
    let leading_slash = std::path::MAIN_SEPARATOR.to_string();
    Ok(Utf8Path::new("root").join(path.strip_prefix(leading_slash)?))
}

// Adds all parent directories of a path to the archive.
//
// For example, if we wanted to insert the file into the archive:
//
// - /opt/oxide/foo/bar.txt
//
// We would add the following directories to the archive:
//
// - /root
// - /root/opt
// - /root/opt/oxide
// - /root/opt/oxide/foo
fn zone_get_all_parent_inputs(to: &Utf8Path) -> Result<Vec<TargetDirectory>> {
    let mut parents: Vec<&Utf8Path> = to.ancestors().collect::<Vec<&Utf8Path>>();
    parents.reverse();

    if to.is_relative() {
        bail!("Cannot add 'to = {to}'; absolute path required");
    }

    let mut outputs = vec![];
    for parent in parents {
        let dst = zone_archive_path(parent)?;
        outputs.push(TargetDirectory(dst))
    }
    Ok(outputs)
}

/// Describes a path to a Buildomat-generated artifact that should reside at
/// the following path:
///
/// <https://buildomat.eng.oxide.computer/public/file/oxidecomputer/REPO/SERIES/COMMIT/ARTIFACT>
#[derive(Clone, Serialize, Deserialize, Debug, PartialEq, Eq)]
pub struct PrebuiltBlob {
    pub repo: String,
    pub series: String,
    pub commit: String,
    pub artifact: String,
    pub sha256: String,
}

/// Describes the origin of an externally-built package.
#[derive(Clone, Deserialize, Debug, PartialEq)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum PackageSource {
    /// Describes a package which should be assembled locally.
    Local {
        /// A list of blobs from the Omicron build S3 bucket which should be placed
        /// within this package.
        blobs: Option<Vec<Utf8PathBuf>>,

        /// A list of Buildomat blobs that should be placed in this package.
        buildomat_blobs: Option<Vec<PrebuiltBlob>>,

        /// Configuration for packages containing Rust binaries.
        rust: Option<RustPackage>,

        /// A set of mapped paths which appear within the archive.
        #[serde(default)]
        paths: Vec<InterpolatedMappedPath>,
    },

    /// Downloads the package from the following URL:
    ///
    /// <https://buildomat.eng.oxide.computer/public/file/oxidecomputer/REPO/image/COMMIT/PACKAGE>
    Prebuilt {
        repo: String,
        commit: String,
        sha256: String,
    },

    /// A composite package, created by merging multiple tarballs into one.
    ///
    /// Currently, this package can only merge zone images.
    Composite { packages: Vec<String> },

    /// Expects that a package will be manually built and placed into the output
    /// directory.
    Manual,
}

impl PackageSource {
    fn rust_package(&self) -> Option<&RustPackage> {
        match self {
            PackageSource::Local {
                rust: Some(rust_pkg),
                ..
            } => Some(rust_pkg),
            _ => None,
        }
    }

    fn blobs(&self) -> Option<&[Utf8PathBuf]> {
        match self {
            PackageSource::Local {
                blobs: Some(blobs), ..
            } => Some(blobs),
            _ => None,
        }
    }

    fn buildomat_blobs(&self) -> Option<&[PrebuiltBlob]> {
        match self {
            PackageSource::Local {
                buildomat_blobs: Some(buildomat_blobs),
                ..
            } => Some(buildomat_blobs),
            _ => None,
        }
    }
}

/// Describes the output format of the package.
#[derive(Deserialize, Debug, Clone, PartialEq)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum PackageOutput {
    /// A complete zone image, ready to be deployed to the target.
    Zone {
        /// "true" if the package is only used to construct composite packages.
        ///
        /// This can be used to signal that the package should *not* be
        /// installed by itself.
        #[serde(default)]
        intermediate_only: bool,
    },
    /// A tarball, ready to be deployed to the target.
    Tarball,
}

/// A single package.
#[derive(Clone, Deserialize, Debug, PartialEq)]
pub struct Package {
    /// The name of the service name to be used on the target OS.
    pub service_name: ServiceName,

    /// Identifies from where the package originates.
    ///
    /// For example, do we need to assemble it ourselves, or pull it from
    /// somewhere else?
    pub source: PackageSource,

    /// Identifies what the output of the package should be.
    pub output: PackageOutput,

    /// Identifies the targets for which the package should be included.
    ///
    /// If ommitted, the package is assumed to be included for all targets.
    pub only_for_targets: Option<BTreeMap<String, String>>,

    /// A human-readable string with suggestions for setup if packaging fails.
    #[serde(default)]
    pub setup_hint: Option<String>,
}

// What version should we stamp on packages, before they have been stamped?
const DEFAULT_VERSION: semver::Version = semver::Version::new(0, 0, 0);

async fn new_zone_archive_builder(
    package_name: &PackageName,
    output_directory: &Utf8Path,
) -> Result<ArchiveBuilder<GzEncoder<File>>> {
    let tarfile = output_directory.join(format!("{}.tar.gz", package_name));
    crate::archive::new_compressed_archive_builder(&tarfile).await
}

/// Configuration that can modify how a package is built.
pub struct BuildConfig<'a> {
    /// Describes the [Target] to build the package for.
    pub target: &'a Target,

    /// Describes how progress will be communicated back to the caller.
    pub progress: &'a dyn Progress,

    /// If "true", disables all caching.
    pub cache_disabled: bool,
}

static DEFAULT_TARGET: Target = Target(BTreeMap::new());
static DEFAULT_PROGRESS: NoProgress = NoProgress::new();

impl Default for BuildConfig<'_> {
    fn default() -> Self {
        Self {
            target: &DEFAULT_TARGET,
            progress: &DEFAULT_PROGRESS,
            cache_disabled: false,
        }
    }
}

impl Package {
    /// The path of a package once it is built.
    pub fn get_output_path(&self, id: &PackageName, output_directory: &Utf8Path) -> Utf8PathBuf {
        output_directory.join(self.get_output_file(id))
    }

    /// The path of the service name with respect to the install directory.
    pub fn get_output_path_for_service(&self, install_directory: &Utf8Path) -> Utf8PathBuf {
        install_directory.join(self.get_output_file_for_service())
    }

    /// The path of a package after it has been "stamped" with a version.
    pub fn get_stamped_output_path(
        &self,
        name: &PackageName,
        output_directory: &Utf8Path,
    ) -> Utf8PathBuf {
        output_directory
            .join("versioned")
            .join(self.get_output_file(name))
    }

    /// The filename of a package once it is built.
    pub fn get_output_file(&self, name: &PackageName) -> String {
        match self.output {
            PackageOutput::Zone { .. } => format!("{}.tar.gz", name),
            PackageOutput::Tarball => format!("{}.tar", name),
        }
    }

    pub fn get_output_file_for_service(&self) -> String {
        match self.output {
            PackageOutput::Zone { .. } => format!("{}.tar.gz", self.service_name),
            PackageOutput::Tarball => format!("{}.tar", self.service_name),
        }
    }

    #[deprecated = "Use 'Package::create', which now takes a 'BuildConfig', and implements 'Default'"]
    pub async fn create_for_target(
        &self,
        target: &Target,
        name: &PackageName,
        output_directory: &Utf8Path,
    ) -> Result<File> {
        let build_config = BuildConfig {
            target,
            ..Default::default()
        };
        self.create_internal(name, output_directory, &build_config)
            .await
    }

    pub async fn create(
        &self,
        name: &PackageName,
        output_directory: &Utf8Path,
        build_config: &BuildConfig<'_>,
    ) -> Result<File> {
        self.create_internal(name, output_directory, build_config)
            .await
    }

    pub async fn stamp(
        &self,
        name: &PackageName,
        output_directory: &Utf8Path,
        version: &semver::Version,
    ) -> Result<Utf8PathBuf> {
        let stamp_path = self.get_stamped_output_path(name, output_directory);
        std::fs::create_dir_all(stamp_path.parent().unwrap())?;

        match self.output {
            PackageOutput::Zone { .. } => {
                let mut inputs = BuildInputs::new();
                inputs.0.push(self.get_version_input(name, Some(version)));
                inputs.0.push(BuildInput::AddPackage(TargetPackage(
                    self.get_output_path(name, output_directory),
                )));

                // Add the package to "itself", but as a stamped version.
                //
                // We jump through some hoops to avoid modifying the archive
                // in-place, which would complicate the ordering and determinism
                // in the build system.
                let mut archive =
                    new_zone_archive_builder(name, stamp_path.parent().unwrap()).await?;
                for input in inputs.0.iter() {
                    self.add_input_to_package(&NoProgress::new(), &mut archive, input)
                        .await
                        .with_context(|| format!("Adding input {input:?}"))?;
                }

                // Finalize the archive.
                archive
                    .builder
                    .into_inner()
                    .map_err(|err| anyhow!("Failed to finalize archive: {}", err))?
                    .finish()?;
            }
            PackageOutput::Tarball => {
                // Unpack the old tarball
                let original_file = self.get_output_path(name, output_directory);
                let mut reader = tar::Archive::new(open_tarfile(&original_file)?);
                let tmp = camino_tempfile::tempdir()?;
                reader.unpack(tmp.path())?;

                // Remove the placeholder version
                if let Err(err) = std::fs::remove_file(tmp.path().join("VERSION")) {
                    if err.kind() != std::io::ErrorKind::NotFound {
                        return Err(err.into());
                    }
                }

                // Create the new tarball
                let file = create_tarfile(&stamp_path)?;
                // TODO: We could add compression here, if we'd like?
                let mut archive = Builder::new(file);
                archive.mode(tar::HeaderMode::Deterministic);
                archive.append_dir_all_async(".", tmp.path()).await?;

                self.add_stamp_to_tarball_package(&mut archive, version)
                    .await?;

                // Finalize the archive.
                archive.finish()?;
            }
        }
        Ok(stamp_path)
    }

    /// Identical to [`Self::create`], but allows a caller to receive updates
    /// about progress while constructing the package.
    #[deprecated = "Use 'Package::create', which now takes a 'BuildConfig', and implements 'Default'"]
    pub async fn create_with_progress_for_target(
        &self,
        progress: &impl Progress,
        target: &Target,
        name: &PackageName,
        output_directory: &Utf8Path,
    ) -> Result<File> {
        let config = BuildConfig {
            target,
            progress,
            ..Default::default()
        };
        self.create_internal(name, output_directory, &config).await
    }

    async fn create_internal(
        &self,
        name: &PackageName,
        output_directory: &Utf8Path,
        config: &BuildConfig<'_>,
    ) -> Result<File> {
        let mut timer = BuildTimer::new();
        let output = match self.output {
            PackageOutput::Zone { .. } => {
                self.create_zone_package(&mut timer, name, output_directory, config)
                    .await?
            }
            PackageOutput::Tarball => {
                self.create_tarball_package(name, output_directory, config)
                    .await?
            }
        };

        timer.log_all(config.progress.get_log());
        Ok(output)
    }

    // Adds the version file to the archive
    fn get_version_input(
        &self,
        package_name: &PackageName,
        version: Option<&semver::Version>,
    ) -> BuildInput {
        match &self.output {
            PackageOutput::Zone { .. } => {
                // The first file in the archive must always be a JSON file
                // which identifies the format of the rest of the archive.
                //
                // See the OMICRON1(5) man page for more detail.
                let version = version.cloned().unwrap_or(DEFAULT_VERSION);
                let version = &version.to_string();

                let kvs = vec![
                    ("v", "1"),
                    ("t", "layer"),
                    ("pkg", package_name.as_ref()),
                    ("version", version),
                ];

                let contents = String::from("{")
                    + &kvs
                        .into_iter()
                        .map(|(k, v)| format!("\"{k}\":\"{v}\""))
                        .collect::<Vec<String>>()
                        .join(",")
                    + "}";

                BuildInput::AddInMemoryFile {
                    dst_path: "oxide.json".into(),
                    contents,
                }
            }
            PackageOutput::Tarball => {
                let version = version.cloned().unwrap_or(DEFAULT_VERSION);
                let contents = version.to_string();
                BuildInput::AddInMemoryFile {
                    dst_path: "VERSION".into(),
                    contents,
                }
            }
        }
    }

    fn get_paths_inputs(
        &self,
        target: &Target,
        paths: &Vec<InterpolatedMappedPath>,
    ) -> Result<BuildInputs> {
        let mut inputs = BuildInputs::new();

        for path in paths {
            let path = path.interpolate(target)?;
            let from = path.from;
            let to = path.to;

            match self.output {
                PackageOutput::Zone { .. } => {
                    // Zone images require all paths to have their parents before
                    // they may be unpacked.
                    inputs.0.extend(
                        zone_get_all_parent_inputs(to.parent().unwrap())?
                            .into_iter()
                            .map(BuildInput::AddDirectory),
                    );
                }
                PackageOutput::Tarball => {}
            }
            if !from.exists() {
                // Strictly speaking, this check is redundant, but it provides
                // a better error message.
                bail!(
                    "Cannot add path \"{}\" to package \"{}\" because it does not exist",
                    from,
                    self.service_name,
                );
            }

            let from_root = std::fs::canonicalize(&from)
                .map_err(|e| anyhow!("failed to canonicalize \"{}\": {}", from, e))?;
            let entries = walkdir::WalkDir::new(&from_root)
                // Pick up symlinked files.
                .follow_links(true)
                // Ensure the output tarball is deterministic.
                .sort_by_file_name();
            for entry in entries {
                let entry = entry?;
                let dst = if from.is_dir() {
                    // If copying a directory (and intermediates), strip out the
                    // source prefix when creating the target path.
                    to.join(<&Utf8Path>::try_from(
                        entry.path().strip_prefix(&from_root)?,
                    )?)
                } else {
                    // If copying a single file, it should be copied exactly.
                    assert_eq!(entry.path(), from_root.as_path());
                    to.clone()
                };

                let dst = match self.output {
                    PackageOutput::Zone { .. } => {
                        // Zone images must explicitly label all destination paths
                        // as within "root/".
                        zone_archive_path(&dst)?
                    }
                    PackageOutput::Tarball => dst,
                };

                if entry.file_type().is_dir() {
                    inputs
                        .0
                        .push(BuildInput::AddDirectory(TargetDirectory(dst)));
                } else if entry.file_type().is_file() {
                    let src = <&Utf8Path>::try_from(entry.path())?;
                    inputs.0.push(BuildInput::add_file(MappedPath {
                        from: src.to_path_buf(),
                        to: dst,
                    })?);
                } else {
                    panic!(
                        "Unsupported file type: {:?} for {:?}",
                        entry.file_type(),
                        entry
                    );
                }
            }
        }

        Ok(inputs)
    }

    fn get_all_inputs(
        &self,
        package_name: &PackageName,
        target: &Target,
        output_directory: &Utf8Path,
        zoned: bool,
        version: Option<&semver::Version>,
    ) -> Result<BuildInputs> {
        let mut all_paths = BuildInputs::new();

        // For all archive formats, the version comes first
        all_paths
            .0
            .push(self.get_version_input(package_name, version));

        match &self.source {
            PackageSource::Local { paths, .. } => {
                all_paths.0.extend(self.get_paths_inputs(target, paths)?.0);
                all_paths.0.extend(self.get_rust_inputs()?.0);
                all_paths
                    .0
                    .extend(self.get_blobs_inputs(output_directory, zoned)?.0);
            }
            PackageSource::Composite { packages } => {
                for component_package in packages {
                    all_paths.0.push(BuildInput::AddPackage(TargetPackage(
                        output_directory.join(component_package),
                    )));
                }
            }
            _ => {
                bail!(
                    "Cannot walk over a zone package with source: {:?}",
                    self.source
                );
            }
        }

        Ok(all_paths)
    }

    fn get_rust_inputs(&self) -> Result<BuildInputs> {
        let mut inputs = BuildInputs::new();
        if let Some(rust_pkg) = self.source.rust_package() {
            let dst_directory = match self.output {
                PackageOutput::Zone { .. } => {
                    let dst = Utf8Path::new("/opt/oxide")
                        .join(self.service_name.as_str())
                        .join("bin");
                    inputs.0.extend(
                        zone_get_all_parent_inputs(&dst)?
                            .into_iter()
                            .map(BuildInput::AddDirectory),
                    );

                    zone_archive_path(&dst)?
                }
                PackageOutput::Tarball => Utf8PathBuf::from(""),
            };

            for binary in &rust_pkg.binary_names {
                let from = RustPackage::local_binary_path(binary, rust_pkg.release);
                let to = dst_directory.join(binary);
                inputs
                    .0
                    .push(BuildInput::add_file(MappedPath { from, to })?);
            }
        }
        Ok(inputs)
    }

    fn get_blobs_inputs(&self, download_directory: &Utf8Path, zoned: bool) -> Result<BuildInputs> {
        let mut inputs = BuildInputs::new();

        let destination_path = if zoned {
            zone_archive_path(
                &Utf8Path::new("/opt/oxide")
                    .join(self.service_name.as_str())
                    .join(BLOB),
            )?
        } else {
            Utf8PathBuf::from(BLOB)
        };
        if let Some(s3_blobs) = self.source.blobs() {
            inputs.0.extend(s3_blobs.iter().map(|blob| {
                let from = download_directory
                    .join(self.service_name.as_str())
                    .join(blob);
                let to = destination_path.join(blob);
                BuildInput::AddBlob {
                    path: MappedPath { from, to },
                    blob: crate::blob::Source::S3(blob.clone()),
                }
            }))
        }
        if let Some(buildomat_blobs) = self.source.buildomat_blobs() {
            inputs.0.extend(buildomat_blobs.iter().map(|blob| {
                let from = download_directory
                    .join(self.service_name.as_str())
                    .join(&blob.artifact);
                let to = destination_path.join(&blob.artifact);
                BuildInput::AddBlob {
                    path: MappedPath { from, to },
                    blob: crate::blob::Source::Buildomat(blob.clone()),
                }
            }));
        }
        Ok(inputs)
    }

    async fn create_zone_package(
        &self,
        timer: &mut BuildTimer,
        name: &PackageName,
        output_directory: &Utf8Path,
        config: &BuildConfig<'_>,
    ) -> Result<File> {
        let target = &config.target;
        let progress = &config.progress;
        let mut cache = Cache::new(output_directory).await?;
        cache.set_disable(config.cache_disabled);
        timer.start("walking paths (identifying all inputs)");

        progress.set_message("Identifying inputs".into());
        let zoned = true;
        let inputs = self
            .get_all_inputs(name, target, output_directory, zoned, None)
            .context("Identifying all input paths")?;
        progress.increment_total(inputs.0.len() as u64);

        let output_file = self.get_output_file(&name);
        let output_path = output_directory.join(&output_file);

        // Decide whether or not to use a cached copy of the zone package
        timer.start("cache lookup");

        match cache.lookup(&inputs, &output_path).await {
            Ok(_) => {
                timer.finish_with_label("Cache hit")?;
                progress.set_message("Cache hit".into());
                return Ok(File::open(output_path)?);
            }
            Err(CacheError::CacheMiss { reason }) => {
                timer.finish_with_label(format!("Cache miss: {reason}"))?;
                progress.set_message("Cache miss".into());
            }
            Err(CacheError::Other(other)) => {
                return Err(other).context("Reading from package cache");
            }
        }

        // Actually build the package
        timer.start("add inputs to package");
        let mut archive = new_zone_archive_builder(name, output_directory).await?;

        for input in inputs.0.iter() {
            self.add_input_to_package(&**progress, &mut archive, input)
                .await
                .with_context(|| format!("Adding input {input:?}"))?;
        }
        timer.start("finalize archive");
        let file = archive.into_inner()?.finish()?;

        // Cache information about the built package
        timer.start("update cache manifest");
        progress.set_message("Updating cached copy".into());

        cache
            .update(&inputs, &output_path)
            .await
            .context("Updating package cache")?;

        timer.finish()?;
        Ok(file)
    }

    async fn add_stamp_to_tarball_package(
        &self,
        archive: &mut Builder<File>,
        version: &semver::Version,
    ) -> Result<()> {
        // Add the version file to the archive
        let mut version_file = tokio::fs::File::from_std(camino_tempfile::tempfile()?);
        version_file
            .write_all(version.to_string().as_bytes())
            .await?;
        version_file.seek(std::io::SeekFrom::Start(0)).await?;
        let version_filename = Utf8Path::new("VERSION");
        archive
            .append_file_async(version_filename, &mut version_file.into_std().await)
            .await?;
        Ok(())
    }

    async fn add_input_to_package<E: Encoder>(
        &self,
        progress: &dyn Progress,
        archive: &mut ArchiveBuilder<E>,
        input: &BuildInput,
    ) -> Result<()> {
        match &input {
            BuildInput::AddInMemoryFile { dst_path, contents } => {
                let mut src_file = tokio::fs::File::from_std(camino_tempfile::tempfile()?);
                src_file.write_all(contents.as_bytes()).await?;
                src_file.seek(std::io::SeekFrom::Start(0)).await?;
                archive
                    .builder
                    .append_file_async(dst_path, &mut src_file.into_std().await)
                    .await?;
            }
            BuildInput::AddDirectory(dir) => archive.builder.append_dir(&dir.0, ".")?,
            BuildInput::AddFile { mapped_path, .. } => {
                let src = &mapped_path.from;
                let dst = &mapped_path.to;
                progress.set_message(format!("adding file: {}", src).into());
                archive
                    .builder
                    .append_path_with_name_async(src, dst)
                    .await
                    .context(format!("Failed to add file '{}' to '{}'", src, dst,))?;
            }
            BuildInput::AddBlob { path, blob } => {
                // TODO: Like the rust packages being built ahead-of-time,
                // we could ensure all the blobs have been downloaded before
                // adding them to this package?
                //
                // That seems important it we want downloads to be concurrent.
                // Granted, this optimization matters less for an incremental
                // workflow.
                let blobs_path = path.from.parent().unwrap();
                std::fs::create_dir_all(blobs_path)?;

                let blob_path = match &blob {
                    blob::Source::S3(s) => blobs_path.join(s),
                    blob::Source::Buildomat(spec) => blobs_path.join(&spec.artifact),
                };

                blob::download(progress, blob, &blob_path)
                    .await
                    .with_context(|| format!("failed to download blob: {}", blob.get_url()))?;
            }
            BuildInput::AddPackage(component_package) => {
                progress.set_message(format!("adding package: {}", component_package.0).into());
                add_package_to_zone_archive(archive, &component_package.0).await?;
            }
        }
        progress.increment_completed(1);
        Ok(())
    }

    async fn create_tarball_package(
        &self,
        name: &PackageName,
        output_directory: &Utf8Path,
        config: &BuildConfig<'_>,
    ) -> Result<File> {
        let progress = &config.progress;

        if !matches!(self.source, PackageSource::Local { .. }) {
            bail!("Cannot create non-local tarball");
        }

        let output_path = self.get_output_path(name, output_directory);
        let mut cache = Cache::new(output_directory).await?;
        cache.set_disable(config.cache_disabled);

        let zoned = false;
        let inputs = self
            .get_all_inputs(name, config.target, output_directory, zoned, None)
            .context("Identifying all input paths")?;
        progress.increment_total(inputs.0.len() as u64);

        match cache.lookup(&inputs, &output_path).await {
            Ok(_) => {
                progress.set_message("Cache hit".into());
                return Ok(File::open(output_path)?);
            }
            Err(CacheError::CacheMiss { reason: _ }) => {
                progress.set_message("Cache miss".into());
            }
            Err(CacheError::Other(other)) => {
                return Err(other).context("Reading from package cache");
            }
        }

        let file = create_tarfile(&output_path)?;
        // TODO: We could add compression here, if we'd like?
        let mut archive = ArchiveBuilder::new(Builder::new(file));
        archive.builder.mode(tar::HeaderMode::Deterministic);

        for input in inputs.0.iter() {
            self.add_input_to_package(&**progress, &mut archive, input)
                .await?;
        }

        let file = archive
            .builder
            .into_inner()
            .map_err(|err| anyhow!("Failed to finalize archive: {}", err))?;

        progress.set_message("Updating cached copy".into());
        cache
            .update(&inputs, &output_path)
            .await
            .context("Updating package cache")?;

        Ok(file)
    }
}

/// Describes configuration for a package which contains a Rust binary.
#[derive(Clone, Deserialize, Debug, PartialEq)]
pub struct RustPackage {
    /// The name of the compiled binary to be used.
    // TODO: Could be extrapolated to "produced build artifacts", we don't
    // really care about the individual binary file.
    pub binary_names: Vec<String>,

    /// True if the package has been built in release mode.
    pub release: bool,
}

impl RustPackage {
    // Returns the path to the compiled binary.
    fn local_binary_path(name: &str, release: bool) -> Utf8PathBuf {
        format!(
            "target/{}/{}",
            if release { "release" } else { "debug" },
            name,
        )
        .into()
    }
}

/// A string which can be modified with key-value pairs.
#[derive(Clone, Deserialize, Debug, PartialEq)]
pub struct InterpolatedString(String);

impl InterpolatedString {
    // Interpret the string for the specified target.
    // Substitutes key/value pairs as necessary.
    pub fn interpolate(&self, target: &Target) -> Result<String> {
        let mut input = self.0.as_str();
        let mut output = String::new();

        const START_STR: &str = "{{";
        const END_STR: &str = "}}";

        while let Some(sub_idx) = input.find(START_STR) {
            output.push_str(&input[..sub_idx]);
            input = &input[sub_idx + START_STR.len()..];

            let Some(end_idx) = input.find(END_STR) else {
                bail!("Missing closing '{END_STR}' character in '{}'", self.0);
            };
            let key = &input[..end_idx];
            let Some(value) = target.0.get(key) else {
                bail!(
                    "Key '{key}' not found in target, but required in '{}'",
                    self.0
                );
            };
            output.push_str(value);
            input = &input[end_idx + END_STR.len()..];
        }
        output.push_str(input);
        Ok(output)
    }
}

/// A pair of path templates, mapping from a file or directory on the host to the target.
///
/// These paths may require target-specific interpretation before being
/// transformed to an actual [MappedPath].
#[derive(Clone, Deserialize, Debug, PartialEq)]
pub struct InterpolatedMappedPath {
    /// Source path.
    pub from: InterpolatedString,
    /// Destination path.
    pub to: InterpolatedString,
}

impl InterpolatedMappedPath {
    fn interpolate(&self, target: &Target) -> Result<MappedPath> {
        Ok(MappedPath {
            from: Utf8PathBuf::from(self.from.interpolate(target)?),
            to: Utf8PathBuf::from(self.to.interpolate(target)?),
        })
    }
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn interpolate_noop() {
        let target = Target(BTreeMap::new());
        let is = InterpolatedString(String::from("nothing to change"));

        let s = is.interpolate(&target).unwrap();
        assert_eq!(s, is.0);
    }

    #[test]
    fn interpolate_single() {
        let mut target = Target(BTreeMap::new());
        target.0.insert("key1".to_string(), "value1".to_string());
        let is = InterpolatedString(String::from("{{key1}}"));

        let s = is.interpolate(&target).unwrap();
        assert_eq!(s, "value1");
    }

    #[test]
    fn interpolate_single_with_prefix() {
        let mut target = Target(BTreeMap::new());
        target.0.insert("key1".to_string(), "value1".to_string());
        let is = InterpolatedString(String::from("prefix-{{key1}}"));

        let s = is.interpolate(&target).unwrap();
        assert_eq!(s, "prefix-value1");
    }

    #[test]
    fn interpolate_single_with_suffix() {
        let mut target = Target(BTreeMap::new());
        target.0.insert("key1".to_string(), "value1".to_string());
        let is = InterpolatedString(String::from("{{key1}}-suffix"));

        let s = is.interpolate(&target).unwrap();
        assert_eq!(s, "value1-suffix");
    }

    #[test]
    fn interpolate_multiple() {
        let mut target = Target(BTreeMap::new());
        target.0.insert("key1".to_string(), "value1".to_string());
        target.0.insert("key2".to_string(), "value2".to_string());
        let is = InterpolatedString(String::from("{{key1}}-{{key2}}"));

        let s = is.interpolate(&target).unwrap();
        assert_eq!(s, "value1-value2");
    }

    #[test]
    fn interpolate_missing_key() {
        let mut target = Target(BTreeMap::new());
        target.0.insert("key1".to_string(), "value1".to_string());
        let is = InterpolatedString(String::from("{{key3}}"));

        let err = is
            .interpolate(&target)
            .expect_err("Interpolating string should have failed");
        assert_eq!(
            err.to_string(),
            "Key 'key3' not found in target, but required in '{{key3}}'"
        );
    }

    #[test]
    fn interpolate_missing_closing() {
        let mut target = Target(BTreeMap::new());
        target.0.insert("key1".to_string(), "value1".to_string());
        let is = InterpolatedString(String::from("{{key1"));

        let err = is
            .interpolate(&target)
            .expect_err("Interpolating string should have failed");
        assert_eq!(
            err.to_string(),
            "Missing closing '}}' character in '{{key1'"
        );
    }

    // This is mostly an example of "what not to do", but hey, we're here to
    // test that we don't fall over.
    //
    // Until we see the "}}" sequence, all intermediate characters are treated
    // as part of they key -- INCLUDING other "{{" characters.
    #[test]
    fn interpolate_key_as_literal() {
        let mut target = Target(BTreeMap::new());
        target.0.insert("oh{{no".to_string(), "value".to_string());
        let is = InterpolatedString(String::from("{{oh{{no}}"));

        let s = is.interpolate(&target).unwrap();
        assert_eq!(s, "value");
    }
}
