// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Utility for bundling target binaries as tarfiles.

use anyhow::{anyhow, bail, Result};
use chrono::{DateTime, FixedOffset, Utc};
use reqwest::header::{CONTENT_LENGTH, LAST_MODIFIED};
use serde_derive::Deserialize;
use std::borrow::Cow;
use std::collections::BTreeMap;
use std::convert::TryInto;
use std::fs::{File, OpenOptions};
use std::path::{Path, PathBuf};
use std::str::FromStr;
use tar::Builder;
use tokio::io::{AsyncSeekExt, AsyncWriteExt};

// Path to the blob S3 Bucket.
const S3_BUCKET: &str = "https://oxide-omicron-build.s3.amazonaws.com";
// Name for the directory component where downloaded blobs are stored.
const BLOB: &str = "blob";

#[test]
fn test_converts() {
    let content_length = "1966080";
    let last_modified = "Fri, 30 Apr 2021 22:37:39 GMT";

    let content_length: u64 = u64::from_str(content_length).unwrap();
    assert_eq!(1966080, content_length);

    let _last_modified: DateTime<FixedOffset> =
        chrono::DateTime::parse_from_rfc2822(last_modified).unwrap();
}

// Downloads "source" from S3_BUCKET to "destination".
pub async fn download(source: &str, destination: &Path) -> Result<()> {
    let url = format!("{}/{}", S3_BUCKET, source);
    let client = reqwest::Client::new();

    if destination.exists() {
        // If destination exists, check against size and last modified time. If
        // both are the same, then return Ok
        let head_response = client.head(&url).send().await?;
        if !head_response.status().is_success() {
            bail!("head failed! {:?}", head_response);
        }

        let headers = head_response.headers();

        // From S3, header looks like:
        //
        //    "Content-Length: 49283072"
        let content_length = headers
            .get(CONTENT_LENGTH)
            .ok_or_else(|| anyhow!("no content length on {} HEAD response!", url))?;
        let content_length: u64 = u64::from_str(content_length.to_str()?)?;

        // From S3, header looks like:
        //
        //    "Last-Modified: Fri, 27 May 2022 20:50:17 GMT"
        let last_modified = headers
            .get(LAST_MODIFIED)
            .ok_or_else(|| anyhow!("no last modified on {} HEAD response!", url))?;
        let last_modified: DateTime<FixedOffset> =
            chrono::DateTime::parse_from_rfc2822(last_modified.to_str()?)?;
        let metadata = tokio::fs::metadata(&destination).await?;
        let metadata_modified: DateTime<Utc> = metadata.modified()?.into();

        if metadata.len() == content_length && metadata_modified == last_modified {
            return Ok(());
        }
    }

    println!(
        "Downloading {} to {}",
        source,
        destination.to_string_lossy()
    );

    let response = client.get(url).send().await?;

    // Store modified time from HTTPS response
    let last_modified = response
        .headers()
        .get(LAST_MODIFIED)
        .ok_or_else(|| anyhow!("no last modified on GET response!"))?;
    let last_modified: DateTime<FixedOffset> =
        chrono::DateTime::parse_from_rfc2822(last_modified.to_str()?)?;

    // Write file bytes to destination
    let mut file = tokio::fs::File::create(destination).await?;
    file.write_all(&response.bytes().await?).await?;
    drop(file);

    // Set destination file's modified time based on HTTPS response
    filetime::set_file_mtime(
        destination,
        filetime::FileTime::from_system_time(last_modified.into()),
    )?;

    Ok(())
}

// Helper to open a tarfile for reading/writing.
fn open_tarfile(tarfile: &Path) -> Result<File> {
    OpenOptions::new()
        .write(true)
        .read(true)
        .truncate(true)
        .create(true)
        .open(&tarfile)
        .map_err(|err| anyhow!("Cannot create tarfile: {}", err))
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

    for parent in parents {
        let dst = archive_path(&parent)?;
        archive.append_dir(&dst, ".")?;
    }

    Ok(())
}

/// Trait for propagating progress information while constructing the package.
pub trait Progress {
    /// Updates the message displayed regarding progress constructing
    /// the package.
    fn set_message(&self, msg: impl Into<Cow<'static, str>>);

    /// Increments the number of things which have completed.
    fn increment(&self, delta: u64);
}

/// Implements [`Progress`] as a no-op.
struct NoProgress;
impl Progress for NoProgress {
    fn set_message(&self, _msg: impl Into<Cow<'static, str>>) {}
    fn increment(&self, _delta: u64) {}
}

/// A single package.
#[derive(Deserialize, Debug)]
pub struct Package {
    /// The name of the service name to be used on the target OS.
    pub service_name: String,

    /// A list of blobs from the Omicron build S3 bucket which should be placed
    /// within this package.
    pub blobs: Option<Vec<PathBuf>>,

    /// Configuration for packages containing Rust binaries.
    pub rust: Option<RustPackage>,

    /// A set of mapped paths which appear within the archive.
    #[serde(default)]
    pub paths: Vec<MappedPath>,

    /// Identifies if the package should be packaged into a zone image.
    pub zone: bool,

    /// Identifies the targets for which the package should be included.
    ///
    /// If ommitted, the package is assumed to be included for all targets.
    pub only_for_targets: Option<BTreeMap<String, String>>,

    /// A human-readable string with suggestions for setup if packaging fails.
    #[serde(default)]
    pub setup_hint: Option<String>,
}

impl Package {
    pub fn get_output_path(&self, name: &str, output_directory: &Path) -> PathBuf {
        if self.zone {
            output_directory.join(format!("{}.tar.gz", name))
        } else {
            output_directory.join(format!("{}.tar", name))
        }
    }

    /// Constructs the package file in the output directory.
    pub async fn create(&self, name: &str, output_directory: &Path) -> Result<File> {
        self.create_internal(&NoProgress, name, output_directory).await
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
        // - 1 tick for the rust binary
        // - 1 tick per blob
        let progress_total = self
            .paths
            .iter()
            .map(|path| {
                walkdir::WalkDir::new(&path.from)
                    .follow_links(true)
                    .into_iter()
                    .count()
            })
            .sum::<usize>()
            + if self.rust.is_some() { 1 } else { 0 }
            + if let Some(blobs) = &self.blobs {
                blobs.len()
            } else {
                0
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
        if self.zone {
            self.create_zone_package(progress, name, output_directory).await
        } else {
            self.create_tarball_package(progress, name, output_directory)
                .await
        }
    }

    // Add mapped paths to the package.
    async fn add_paths<W: std::io::Write>(
        &self,
        progress: &impl Progress,
        archive: &mut Builder<W>,
    ) -> Result<()> {
        progress.set_message("adding paths");
        for path in &self.paths {
            if self.zone {
                // Zone images require all paths to have their parents before
                // they may be unpacked.
                add_directory_and_parents(archive, path.to.parent().unwrap())?;
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
                let dst = if self.zone {
                    // Zone images must explicitly label all destination paths
                    // as within "root/".
                    archive_path(dst)?
                } else {
                    dst.to_path_buf()
                };

                if entry.file_type().is_dir() {
                    archive.append_dir(&dst, ".")?;
                } else if entry.file_type().is_file() {
                    archive.append_path_with_name(entry.path(), &dst)?;
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

    // Adds blobs from S3 to the package.
    //
    // - `progress`: Reports progress while adding blobs.
    // - `archive`: The archive to add the blobs into
    // - `package`: The package being constructed
    // - `download_directory`: The location to which the blobs should be downloaded
    // - `destination_path`: The destination path of the blobs within the archive
    async fn add_blobs<W: std::io::Write>(
        &self,
        progress: &impl Progress,
        archive: &mut Builder<W>,
        download_directory: &Path,
        destination_path: &Path,
    ) -> Result<()> {
        progress.set_message("adding blobs");
        if let Some(blobs) = &self.blobs {
            let blobs_path = download_directory.join(&self.service_name);
            std::fs::create_dir_all(&blobs_path)?;
            for blob in blobs {
                let blob_path = blobs_path.join(blob);
                download(&blob.to_string_lossy(), &blob_path).await?;
                progress.increment(1);
            }
            archive.append_dir_all(&destination_path, &blobs_path)?;
        }
        Ok(())
    }

    async fn create_zone_package(
        &self,
        progress: &impl Progress,
        name: &str,
        output_directory: &Path,
    ) -> Result<File> {
        // Create a tarball which will become an Omicron-brand image
        // archive.
        let tarfile = self.get_output_path(name, output_directory);
        let file = open_tarfile(&tarfile)?;

        // TODO: Consider using async compression, async tar.
        // It's not the *worst* thing in the world for a packaging tool to block
        // here, but it would help the other async threads remain responsive if
        // we avoided blocking.
        let gzw = flate2::write::GzEncoder::new(file, flate2::Compression::fast());
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
        archive.append_file("oxide.json", &mut root_json.into_std().await)?;

        // Add mapped paths.
        self.add_paths(progress, &mut archive).await?;

        // Attempt to add the rust binary, if one was built.
        progress.set_message("adding rust binaries");
        if let Some(rust_pkg) = &self.rust {
            let dst = Path::new("/opt/oxide").join(&self.service_name).join("bin");
            add_directory_and_parents(&mut archive, &dst)?;
            let dst = archive_path(&dst)?;
            rust_pkg.add_binaries_to_archive(&mut archive, &dst)?;
            progress.increment(1);
        }

        // Add (and possibly download) blobs
        let blob_dst = Path::new("/opt/oxide").join(&self.service_name).join(BLOB);
        self.add_blobs(
            progress,
            &mut archive,
            output_directory,
            &archive_path(&blob_dst)?,
        )
        .await?;

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
        let file = open_tarfile(&tarfile)?;
        // TODO: We could add compression here, if we'd like?
        let mut archive = Builder::new(file);
        archive.mode(tar::HeaderMode::Deterministic);

        // Add mapped paths.
        self.add_paths(progress, &mut archive).await?;

        // Attempt to add the rust binary, if one was built.
        progress.set_message("adding rust binaries");
        if let Some(rust_pkg) = &self.rust {
            rust_pkg.add_binaries_to_archive(&mut archive, Path::new(""))?;
            progress.increment(1);
        }

        // Add (and possibly download) blobs
        self.add_blobs(progress, &mut archive, output_directory, &Path::new(BLOB))
            .await?;

        let file = archive
            .into_inner()
            .map_err(|err| anyhow!("Failed to finalize archive: {}", err))?;

        Ok(file)
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
    fn add_binaries_to_archive<W: std::io::Write>(
        &self,
        archive: &mut tar::Builder<W>,
        dst_directory: &Path,
    ) -> Result<()> {
        for name in &self.binary_names {
            archive
                .append_path_with_name(
                    Self::local_binary_path(&name, self.release),
                    dst_directory.join(&name),
                )
                .map_err(|err| anyhow!("Cannot append binary to tarfile: {}", err))?;
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
