// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Utility for bundling target binaries as tarfiles.

use crate::blob::BLOB;
use crate::progress::{NoProgress, Progress};
use anyhow::{anyhow, Context, Result};
use async_trait::async_trait;
use flate2::write::GzEncoder;
use serde_derive::Deserialize;
use std::collections::BTreeMap;
use std::convert::TryInto;
use std::fs::{File, OpenOptions};
use std::path::{Path, PathBuf};
use tar::Builder;
use tokio::io::{AsyncSeekExt, AsyncWriteExt};

#[async_trait]
trait AsyncAppendFile {
    async fn append_file_async<P>(&mut self, path: P, file: &mut File) -> std::io::Result<()>
    where
        P: AsRef<Path> + Send;

    async fn append_path_with_name_async<P, N>(&mut self, path: P, name: N) -> std::io::Result<()>
    where
        P: AsRef<Path> + Send,
        N: AsRef<Path> + Send;

    async fn append_dir_all_async<P, Q>(&mut self, path: P, src_path: Q) -> std::io::Result<()>
    where
        P: AsRef<Path> + Send,
        Q: AsRef<Path> + Send;
}

#[async_trait]
impl<W: std::io::Write + Send> AsyncAppendFile for Builder<W> {
    async fn append_file_async<P>(&mut self, path: P, file: &mut File) -> std::io::Result<()>
    where
        P: AsRef<Path> + Send,
    {
        tokio::task::block_in_place(move || self.append_file(path, file))
    }

    async fn append_path_with_name_async<P, N>(&mut self, path: P, name: N) -> std::io::Result<()>
    where
        P: AsRef<Path> + Send,
        N: AsRef<Path> + Send,
    {
        tokio::task::block_in_place(move || self.append_path_with_name(path, name))
    }

    async fn append_dir_all_async<P, Q>(&mut self, path: P, src_path: Q) -> std::io::Result<()>
    where
        P: AsRef<Path> + Send,
        Q: AsRef<Path> + Send,
    {
        tokio::task::block_in_place(move || self.append_dir_all(path, src_path))
    }
}

// Helper to open a tarfile for reading/writing.
fn create_tarfile<P: AsRef<Path> + std::fmt::Debug>(tarfile: P) -> Result<File> {
    OpenOptions::new()
        .write(true)
        .read(true)
        .truncate(true)
        .create(true)
        .open(tarfile.as_ref())
        .map_err(|err| anyhow!("Cannot create tarfile {:?}: {}", tarfile, err))
}

// Helper to open a tarfile for reading.
fn open_tarfile<P: AsRef<Path> + std::fmt::Debug>(tarfile: P) -> Result<File> {
    OpenOptions::new()
        .read(true)
        .open(tarfile.as_ref())
        .map_err(|err| anyhow!("Cannot open tarfile {:?}: {}", tarfile, err))
}

// Returns the path as it should be placed within an archive, by
// prepending "root/".
//
// Example:
// - /opt/oxide -> root/opt/oxide
fn archive_path(path: &Path) -> Result<PathBuf> {
    let leading_slash = std::path::MAIN_SEPARATOR.to_string();
    Ok(Path::new("root").join(&path.strip_prefix(leading_slash)?))
}

// Adds all parent directories of a path to the archive.
//
// For example, if we wanted to insert the file into the archive:
//
// - /opt/oxide/foo/bar.txt
//
// We could call the following:
//
// ```
// let path = Path::new("/opt/oxide/foo/bar.txt");
// add_directory_and_parents(&mut archive, path.parent().unwrap());
// ```
//
// Which would add the following directories to the archive:
//
// - /root
// - /root/opt
// - /root/opt/oxide
// - /root/opt/oxide/foo
fn add_directory_and_parents<W: std::io::Write>(
    archive: &mut tar::Builder<W>,
    to: &Path,
) -> Result<()> {
    let mut parents: Vec<&Path> = to.ancestors().collect::<Vec<&Path>>();
    parents.reverse();

    if to.is_relative() {
        return Err(anyhow!(
            "Cannot add 'to = {}'; absolute path required",
            to.to_string_lossy()
        ));
    }

    for parent in parents {
        let dst = archive_path(parent)?;
        archive.append_dir(&dst, ".")?;
    }

    Ok(())
}

/// Describes the origin of an externally-built package.
#[derive(Deserialize, Debug)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum PackageSource {
    /// Describes a package which should be assembled locally.
    Local {
        /// A list of blobs from the Omicron build S3 bucket which should be placed
        /// within this package.
        blobs: Option<Vec<PathBuf>>,

        /// Configuration for packages containing Rust binaries.
        rust: Option<RustPackage>,

        /// A set of mapped paths which appear within the archive.
        #[serde(default)]
        paths: Vec<MappedPath>,
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

    fn blobs(&self) -> Option<&[PathBuf]> {
        match self {
            PackageSource::Local {
                blobs: Some(blobs), ..
            } => Some(blobs),
            _ => None,
        }
    }
}

/// Describes the output format of the package.
#[derive(Deserialize, Debug, Clone)]
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
#[derive(Deserialize, Debug)]
pub struct Package {
    /// The name of the service name to be used on the target OS.
    pub service_name: String,

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

async fn new_zone_archive_builder(
    package_name: &str,
    output_directory: &Path,
) -> Result<tar::Builder<GzEncoder<File>>> {
    let tarfile = output_directory.join(format!("{}.tar.gz", package_name));
    let file = create_tarfile(tarfile)?;
    // TODO: Consider using async compression, async tar.
    // It's not the *worst* thing in the world for a packaging tool to block
    // here, but it would help the other async threads remain responsive if
    // we avoided blocking.
    let gzw = GzEncoder::new(file, flate2::Compression::fast());
    let mut archive = Builder::new(gzw);
    archive.mode(tar::HeaderMode::Deterministic);

    // The first file in the archive must always be a JSON file
    // which identifies the format of the rest of the archive.
    //
    // See the OMICRON1(5) man page for more detail.
    let mut root_json = tokio::fs::File::from_std(tempfile::tempfile()?);
    let contents = r#"{"v":"1","t":"layer"}"#;
    root_json.write_all(contents.as_bytes()).await?;
    root_json.seek(std::io::SeekFrom::Start(0)).await?;
    archive
        .append_file_async(&Path::new("oxide.json"), &mut root_json.into_std().await)
        .await?;

    Ok(archive)
}

impl Package {
    pub fn get_output_path(&self, name: &str, output_directory: &Path) -> PathBuf {
        output_directory.join(self.get_output_file(name))
    }

    pub fn get_output_file(&self, name: &str) -> String {
        match self.output {
            PackageOutput::Zone { .. } => format!("{}.tar.gz", name),
            PackageOutput::Tarball => format!("{}.tar", name),
        }
    }

    /// Constructs the package file in the output directory.
    pub async fn create(&self, name: &str, output_directory: &Path) -> Result<File> {
        self.create_internal(&NoProgress, name, output_directory)
            .await
    }

    /// Returns the "total number of things to be done" when constructing a
    /// package.
    ///
    /// This is intentionally vaguely defined, but it intended to
    /// be a rough indication of progress when using [`Self::create_with_progress`].
    pub fn get_total_work(&self) -> u64 {
        // Tally up some information so we can report progress:
        //
        // - 1 tick for each included path
        // - 1 tick per rust binary
        // - 1 tick per blob + 1 tick for appending blob dir to archive
        let progress_total = match &self.source {
            PackageSource::Local { blobs, rust, paths } => {
                let blob_work = blobs.as_ref().map(|b| b.len() + 1).unwrap_or(0);

                let rust_work = rust.as_ref().map(|r| r.binary_names.len()).unwrap_or(0);

                let paths_work = paths
                    .iter()
                    .map(|path| {
                        walkdir::WalkDir::new(&path.from)
                            .follow_links(true)
                            .into_iter()
                            .count()
                    })
                    .sum::<usize>();

                rust_work + blob_work + paths_work
            }
            _ => 1,
        };
        progress_total.try_into().unwrap()
    }

    /// Identical to [`Self::create`], but allows a caller to receive updates
    /// about progress while constructing the package.
    pub async fn create_with_progress(
        &self,
        progress: &impl Progress,
        name: &str,
        output_directory: &Path,
    ) -> Result<File> {
        self.create_internal(progress, name, output_directory).await
    }

    async fn create_internal(
        &self,
        progress: &impl Progress,
        name: &str,
        output_directory: &Path,
    ) -> Result<File> {
        match self.output {
            PackageOutput::Zone { .. } => {
                self.create_zone_package(progress, name, output_directory)
                    .await
            }
            PackageOutput::Tarball => {
                self.create_tarball_package(progress, name, output_directory)
                    .await
            }
        }
    }

    async fn add_paths<W: std::io::Write + Send + Sync>(
        &self,
        progress: &impl Progress,
        archive: &mut Builder<W>,
        paths: &Vec<MappedPath>,
    ) -> Result<()> {
        progress.set_message("adding paths");

        for path in paths {
            match self.output {
                PackageOutput::Zone { .. } => {
                    // Zone images require all paths to have their parents before
                    // they may be unpacked.
                    add_directory_and_parents(archive, path.to.parent().unwrap())?;
                }
                PackageOutput::Tarball => {}
            }
            if !path.from.exists() {
                // Strictly speaking, this check is redundant, but it provides
                // a better error message.
                return Err(anyhow!(
                    "Cannot add path \"{}\" to package \"{}\" because it does not exist",
                    path.from.to_string_lossy(),
                    self.service_name,
                ));
            }

            let from_root = std::fs::canonicalize(&path.from).map_err(|e| {
                anyhow!(
                    "failed to canonicalize \"{}\": {}",
                    path.from.to_string_lossy(),
                    e
                )
            })?;
            let entries = walkdir::WalkDir::new(&from_root)
                // Pick up symlinked files.
                .follow_links(true)
                // Ensure the output tarball is deterministic.
                .sort_by_file_name();
            for entry in entries {
                let entry = entry?;
                let dst = &path.to.join(entry.path().strip_prefix(&from_root)?);

                let dst = match self.output {
                    PackageOutput::Zone { .. } => {
                        // Zone images must explicitly label all destination paths
                        // as within "root/".
                        archive_path(dst)?
                    }
                    PackageOutput::Tarball => dst.to_path_buf(),
                };

                if entry.file_type().is_dir() {
                    archive.append_dir(&dst, ".")?;
                } else if entry.file_type().is_file() {
                    archive
                        .append_path_with_name_async(entry.path(), &dst)
                        .await
                        .context(format!(
                            "Failed to add file '{}' to '{}'",
                            entry.path().display(),
                            dst.display()
                        ))?;
                } else {
                    panic!(
                        "Unsupported file type: {:?} for {:?}",
                        entry.file_type(),
                        entry
                    );
                }
                progress.increment(1);
            }
        }
        Ok(())
    }

    async fn add_rust<W: std::io::Write + Send>(
        &self,
        progress: &impl Progress,
        archive: &mut Builder<W>,
    ) -> Result<()> {
        if let Some(rust_pkg) = self.source.rust_package() {
            let dst = match self.output {
                PackageOutput::Zone { .. } => {
                    let dst = Path::new("/opt/oxide").join(&self.service_name).join("bin");
                    add_directory_and_parents(archive, &dst)?;
                    archive_path(&dst)?
                }
                PackageOutput::Tarball => PathBuf::from(""),
            };
            rust_pkg
                .add_binaries_to_archive(progress, archive, &dst)
                .await?;
        }
        Ok(())
    }

    // Adds blobs from S3 to the package.
    //
    // - `progress`: Reports progress while adding blobs.
    // - `archive`: The archive to add the blobs into
    // - `package`: The package being constructed
    // - `download_directory`: The location to which the blobs should be downloaded
    // - `destination_path`: The destination path of the blobs within the archive
    async fn add_blobs<W: std::io::Write + Send>(
        &self,
        progress: &impl Progress,
        archive: &mut Builder<W>,
        download_directory: &Path,
        destination_path: &Path,
    ) -> Result<()> {
        progress.set_message("adding blobs");
        if let Some(blobs) = self.source.blobs() {
            let blobs_path = download_directory.join(&self.service_name);
            std::fs::create_dir_all(&blobs_path)?;
            for blob in blobs {
                let blob_path = blobs_path.join(blob);
                crate::blob::download(&blob.to_string_lossy(), &blob_path).await?;
                progress.increment(1);
            }
            archive
                .append_dir_all_async(&destination_path, &blobs_path)
                .await?;
            progress.increment(1);
        }
        Ok(())
    }

    async fn create_zone_package(
        &self,
        progress: &impl Progress,
        name: &str,
        output_directory: &Path,
    ) -> Result<File> {
        let mut archive = new_zone_archive_builder(name, output_directory).await?;

        match &self.source {
            PackageSource::Local { paths, .. } => {
                // Add mapped paths.
                self.add_paths(progress, &mut archive, paths).await?;

                // Attempt to add the rust binary, if one was built.
                self.add_rust(progress, &mut archive).await?;

                // Add (and possibly download) blobs
                let blob_dst = Path::new("/opt/oxide").join(&self.service_name).join(BLOB);
                self.add_blobs(
                    progress,
                    &mut archive,
                    output_directory,
                    &archive_path(&blob_dst)?,
                )
                .await?;
            }
            PackageSource::Composite { packages } => {
                // For each of the component packages, open the tarfile, and add
                // it to our top-level archive.
                let tmp = tempfile::tempdir()?;
                for component_package in packages {
                    let component_path = output_directory.join(component_package);
                    let gzr = flate2::read::GzDecoder::new(open_tarfile(&component_path)?);
                    if gzr.header().is_none() {
                        return Err(anyhow!("Missing gzip header from {}. Note that composite packages can currently only consist of zone images", component_path.display()));
                    }
                    let mut component_reader = tar::Archive::new(gzr);
                    let entries = component_reader.entries()?;

                    // First, unpack the existing entries
                    for entry in entries {
                        let mut entry = entry?;

                        // Ignore the JSON header files
                        let entry_path = entry.path()?;
                        if entry_path == Path::new("oxide.json") {
                            continue;
                        }

                        let entry_unpack_path = tmp.path().join(entry_path.strip_prefix("root/")?);
                        entry.unpack(&entry_unpack_path)?;
                        let entry_path = entry.path()?;
                        assert!(entry_unpack_path.exists());

                        archive
                            .append_path_with_name_async(entry_unpack_path, entry_path)
                            .await?;
                    }
                }
            }
            _ => {
                return Err(anyhow!(
                    "Cannot create a zone package with source: {:?}",
                    self.source
                ));
            }
        }

        let file = archive
            .into_inner()
            .map_err(|err| anyhow!("Failed to finalize archive: {}", err))?;

        Ok(file.finish()?)
    }

    async fn create_tarball_package(
        &self,
        progress: &impl Progress,
        name: &str,
        output_directory: &Path,
    ) -> Result<File> {
        // Create a tarball containing the necessary executable and auxiliary
        // files.
        let tarfile = self.get_output_path(name, output_directory);
        let file = create_tarfile(&tarfile)?;
        // TODO: We could add compression here, if we'd like?
        let mut archive = Builder::new(file);
        archive.mode(tar::HeaderMode::Deterministic);

        match &self.source {
            PackageSource::Local { paths, .. } => {
                // Add mapped paths.
                self.add_paths(progress, &mut archive, paths).await?;

                // Attempt to add the rust binary, if one was built.
                self.add_rust(progress, &mut archive).await?;

                // Add (and possibly download) blobs
                self.add_blobs(progress, &mut archive, output_directory, Path::new(BLOB))
                    .await?;

                Ok(archive
                    .into_inner()
                    .map_err(|err| anyhow!("Failed to finalize archive: {}", err))?)
            }
            _ => Err(anyhow!("Cannot create non-local tarball")),
        }
    }
}

/// Describes configuration for a package which contains a Rust binary.
#[derive(Deserialize, Debug)]
pub struct RustPackage {
    /// The name of the compiled binary to be used.
    // TODO: Could be extrapolated to "produced build artifacts", we don't
    // really care about the individual binary file.
    pub binary_names: Vec<String>,

    /// True if the package has been built in release mode.
    pub release: bool,
}

impl RustPackage {
    // Adds a rust binary to the archive.
    //
    // - `archive`: The archive to which the binary should be added
    // - `dst_directory`: The path where the binary should be added in the archive
    async fn add_binaries_to_archive<W: std::io::Write + Send>(
        &self,
        progress: &impl Progress,
        archive: &mut tar::Builder<W>,
        dst_directory: &Path,
    ) -> Result<()> {
        for name in &self.binary_names {
            progress.set_message(format!("adding rust binary: {name}"));
            archive
                .append_path_with_name_async(
                    Self::local_binary_path(name, self.release),
                    dst_directory.join(&name),
                )
                .await
                .map_err(|err| anyhow!("Cannot append binary to tarfile: {}", err))?;
            progress.increment(1);
        }
        Ok(())
    }

    // Returns the path to the compiled binary.
    fn local_binary_path(name: &str, release: bool) -> PathBuf {
        format!(
            "target/{}/{}",
            if release { "release" } else { "debug" },
            name,
        )
        .into()
    }
}

/// A pair of paths, mapping from a directory on the host to the target.
#[derive(Deserialize, Debug)]
pub struct MappedPath {
    /// Source path.
    pub from: PathBuf,
    /// Destination path.
    pub to: PathBuf,
}
