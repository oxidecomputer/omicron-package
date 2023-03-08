// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Utility for bundling target binaries as tarfiles.

use crate::blob::{self, BLOB};
use crate::progress::{NoProgress, Progress};
use crate::target::Target;

use anyhow::{anyhow, bail, Context, Result};
use async_trait::async_trait;
use flate2::write::GzEncoder;
use futures_util::{stream, StreamExt, TryStreamExt};
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
    #[deprecated(note = "Call Self::create_for_target instead")]
    pub async fn create(&self, name: &str, output_directory: &Path) -> Result<File> {
        let null_target = Target(BTreeMap::new());
        self.create_internal(&null_target, &NoProgress, name, output_directory)
            .await
    }

    pub async fn create_for_target(
        &self,
        target: &Target,
        name: &str,
        output_directory: &Path,
    ) -> Result<File> {
        self.create_internal(&target, &NoProgress, name, output_directory)
            .await
    }

    /// Returns the "total number of things to be done" when constructing a
    /// package.
    ///
    /// This is intentionally vaguely defined, but it intended to
    /// be a rough indication of progress when using [`Self::create_with_progress`].
    #[deprecated(note = "Call Self::get_total_work_for_target instead")]
    pub fn get_total_work(&self) -> u64 {
        let null_target = Target(BTreeMap::new());
        self.get_total_work_for_target(&null_target).unwrap()
    }

    /// Returns the "total number of things to be done" when constructing a
    /// package for a particular target.
    ///
    /// This is intentionally vaguely defined, but it intended to
    /// be a rough indication of progress when using [`Self::create_with_progress`].
    pub fn get_total_work_for_target(&self, target: &Target) -> Result<u64> {
        // Tally up some information so we can report progress:
        //
        // - 1 tick for each included path
        // - 1 tick per rust binary
        // - 1 tick per blob + 1 tick for appending blob dir to archive
        let progress_total = match &self.source {
            PackageSource::Local { blobs, rust, paths } => {
                let blob_work = blobs.as_ref().map(|b| b.len() + 1).unwrap_or(0);

                let rust_work = rust.as_ref().map(|r| r.binary_names.len()).unwrap_or(0);

                let mut paths_work = 0;
                for path in paths {
                    let from = PathBuf::from(path.from.interpolate(&target)?);
                    paths_work += walkdir::WalkDir::new(&from)
                        .follow_links(true)
                        .into_iter()
                        .count();
                }

                rust_work + blob_work + paths_work
            }
            _ => 1,
        };
        Ok(progress_total.try_into()?)
    }

    /// Identical to [`Self::create`], but allows a caller to receive updates
    /// about progress while constructing the package.
    pub async fn create_with_progress(
        &self,
        progress: &impl Progress,
        name: &str,
        output_directory: &Path,
    ) -> Result<File> {
        let null_target = Target(BTreeMap::new());
        self.create_internal(&null_target, progress, name, output_directory)
            .await
    }

    async fn create_internal(
        &self,
        target: &Target,
        progress: &impl Progress,
        name: &str,
        output_directory: &Path,
    ) -> Result<File> {
        match self.output {
            PackageOutput::Zone { .. } => {
                self.create_zone_package(target, progress, name, output_directory)
                    .await
            }
            PackageOutput::Tarball => {
                self.create_tarball_package(target, progress, name, output_directory)
                    .await
            }
        }
    }

    async fn add_paths<W: std::io::Write + Send + Sync>(
        &self,
        target: &Target,
        progress: &impl Progress,
        archive: &mut Builder<W>,
        paths: &Vec<MappedPath>,
    ) -> Result<()> {
        progress.set_message("adding paths".into());

        for path in paths {
            let from = PathBuf::from(path.from.interpolate(&target)?);
            let to = PathBuf::from(path.to.interpolate(&target)?);

            match self.output {
                PackageOutput::Zone { .. } => {
                    // Zone images require all paths to have their parents before
                    // they may be unpacked.
                    add_directory_and_parents(archive, to.parent().unwrap())?;
                }
                PackageOutput::Tarball => {}
            }
            if !from.exists() {
                // Strictly speaking, this check is redundant, but it provides
                // a better error message.
                return Err(anyhow!(
                    "Cannot add path \"{}\" to package \"{}\" because it does not exist",
                    from.to_string_lossy(),
                    self.service_name,
                ));
            }

            let from_root = std::fs::canonicalize(&from).map_err(|e| {
                anyhow!(
                    "failed to canonicalize \"{}\": {}",
                    from.to_string_lossy(),
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
                let dst = &to.join(entry.path().strip_prefix(&from_root)?);

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
        if let Some(blobs) = self.source.blobs() {
            progress.set_message("downloading blobs".into());
            let blobs_path = download_directory.join(&self.service_name);
            std::fs::create_dir_all(&blobs_path)?;
            stream::iter(blobs.iter())
                .map(Ok)
                .try_for_each_concurrent(None, |blob| {
                    let blob_path = blobs_path.join(blob);
                    async move {
                        blob::download(progress, &blob.to_string_lossy(), &blob_path)
                            .await
                            .with_context(|| {
                                format!("failed to download blob: {}", blob.to_string_lossy())
                            })?;
                        progress.increment(1);
                        Ok::<_, anyhow::Error>(())
                    }
                })
                .await?;
            progress.set_message("adding blobs".into());
            archive
                .append_dir_all_async(&destination_path, &blobs_path)
                .await?;
            progress.increment(1);
        }
        Ok(())
    }

    async fn create_zone_package(
        &self,
        target: &Target,
        progress: &impl Progress,
        name: &str,
        output_directory: &Path,
    ) -> Result<File> {
        let mut archive = new_zone_archive_builder(name, output_directory).await?;

        match &self.source {
            PackageSource::Local { paths, .. } => {
                // Add mapped paths.
                self.add_paths(target, progress, &mut archive, paths)
                    .await?;

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
        target: &Target,
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
                self.add_paths(target, progress, &mut archive, paths)
                    .await?;

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
            progress.set_message(format!("adding rust binary: {name}").into());
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

/// A string which can be modified with key-value pairs.
#[derive(Deserialize, Debug)]
pub struct InterpolatedString(String);

impl InterpolatedString {
    // Interpret the string for the specified target.
    // Substitutes key/value pairs as necessary.
    fn interpolate(&self, target: &Target) -> Result<String> {
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
                bail!("Key '{key}' not found in target, but required in '{}'", self.0);
            };
            output.push_str(&value);
            input = &input[end_idx + END_STR.len()..];
        }
        output.push_str(&input[..]);
        Ok(output)
    }
}

/// A pair of paths, mapping from a directory on the host to the target.
#[derive(Deserialize, Debug)]
pub struct MappedPath {
    /// Source path.
    pub from: InterpolatedString,
    /// Destination path.
    pub to: InterpolatedString,
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
