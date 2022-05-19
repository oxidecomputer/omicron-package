// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Utility for bundling target binaries as tarfiles.

use anyhow::{anyhow, Result};
use serde_derive::Deserialize;
use std::borrow::Cow;
use std::convert::TryInto;
use std::fs::{File, OpenOptions};
use std::path::{Path, PathBuf};
use tar::Builder;
use tokio::io::{AsyncSeekExt, AsyncWriteExt};

// Path to the blob S3 Bucket.
const S3_BUCKET: &str = "https://oxide-omicron-build.s3.amazonaws.com";
// Name for the directory component where downloaded blobs are stored.
const BLOB: &str = "blob";

// Downloads "source" from S3_BUCKET to "destination".
async fn download(source: &str, destination: &Path) -> Result<()> {
    println!(
        "Downloading {} to {}",
        source,
        destination.to_string_lossy()
    );
    let response = reqwest::get(format!("{}/{}", S3_BUCKET, source)).await?;
    let mut file = tokio::fs::File::create(destination).await?;
    file.write_all(&response.bytes().await?).await?;
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
        let dst = archive_path(parent)?;
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
}

impl Package {
    pub fn get_output_path(&self, output_directory: &Path) -> PathBuf {
        if self.zone {
            output_directory.join(format!("{}.tar.gz", self.service_name))
        } else {
            output_directory.join(format!("{}.tar", self.service_name))
        }
    }

    /// Constructs the package file in the output directory.
    pub async fn create(&self, output_directory: &Path) -> Result<File> {
        self.create_internal(&NoProgress, output_directory).await
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
        output_directory: &Path,
    ) -> Result<File> {
        self.create_internal(progress, output_directory).await
    }

    async fn create_internal(
        &self,
        progress: &impl Progress,
        output_directory: &Path,
    ) -> Result<File> {
        if self.zone {
            self.create_zone_package(progress, output_directory).await
        } else {
            self.create_tarball_package(progress, output_directory)
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
                if let Some(parent) = path.to.parent() {
                    add_directory_and_parents(archive, parent)?;
                }
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
                // TODO: Check against hash, download if mismatch (i.e.,
                // corruption/update).
                if !blob_path.exists() {
                    download(&blob.to_string_lossy(), &blob_path).await?;
                }
                progress.increment(1);
            }
            archive.append_dir_all(&destination_path, &blobs_path)?;
        }
        Ok(())
    }

    async fn create_zone_package(
        &self,
        progress: &impl Progress,
        output_directory: &Path,
    ) -> Result<File> {
        // Create a tarball which will become an Omicron-brand image
        // archive.
        let tarfile = self.get_output_path(output_directory);
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
        output_directory: &Path,
    ) -> Result<File> {
        // Create a tarball containing the necessary executable and auxiliary
        // files.
        let tarfile = self.get_output_path(output_directory);
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
        self.add_blobs(progress, &mut archive, output_directory, Path::new(BLOB))
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
                    Self::local_binary_path(name, self.release),
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
