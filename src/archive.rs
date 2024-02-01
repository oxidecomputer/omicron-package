// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Tools for creating and inserting into tarballs.

use anyhow::{anyhow, Context, Result};
use async_trait::async_trait;
use camino::Utf8Path;
use flate2::write::GzEncoder;
use std::convert::TryInto;
use std::fs::{File, OpenOptions};
use tar::Builder;

/// These interfaces are similar to some methods in [tar::Builder].
///
/// They use [tokio::block_in_place] to avoid blocking other async
/// tasks using the executor.
#[async_trait]
pub trait AsyncAppendFile {
    async fn append_file_async<P>(&mut self, path: P, file: &mut File) -> std::io::Result<()>
    where
        P: AsRef<Utf8Path> + Send;

    async fn append_path_with_name_async<P, N>(&mut self, path: P, name: N) -> std::io::Result<()>
    where
        P: AsRef<Utf8Path> + Send,
        N: AsRef<Utf8Path> + Send;

    async fn append_dir_all_async<P, Q>(&mut self, path: P, src_path: Q) -> std::io::Result<()>
    where
        P: AsRef<Utf8Path> + Send,
        Q: AsRef<Utf8Path> + Send;
}

#[async_trait]
impl<W: Encoder> AsyncAppendFile for Builder<W> {
    async fn append_file_async<P>(&mut self, path: P, file: &mut File) -> std::io::Result<()>
    where
        P: AsRef<Utf8Path> + Send,
    {
        tokio::task::block_in_place(move || self.append_file(path.as_ref(), file))
    }

    async fn append_path_with_name_async<P, N>(&mut self, path: P, name: N) -> std::io::Result<()>
    where
        P: AsRef<Utf8Path> + Send,
        N: AsRef<Utf8Path> + Send,
    {
        tokio::task::block_in_place(move || {
            self.append_path_with_name(path.as_ref(), name.as_ref())
        })
    }

    async fn append_dir_all_async<P, Q>(&mut self, path: P, src_path: Q) -> std::io::Result<()>
    where
        P: AsRef<Utf8Path> + Send,
        Q: AsRef<Utf8Path> + Send,
    {
        tokio::task::block_in_place(move || self.append_dir_all(path.as_ref(), src_path.as_ref()))
    }
}

/// Helper to open a tarfile for reading/writing.
pub fn create_tarfile<P: AsRef<Utf8Path> + std::fmt::Debug>(tarfile: P) -> Result<File> {
    OpenOptions::new()
        .write(true)
        .read(true)
        .truncate(true)
        .create(true)
        .open(tarfile.as_ref())
        .map_err(|err| anyhow!("Cannot create tarfile {:?}: {}", tarfile, err))
}

/// Helper to open a tarfile for reading.
pub fn open_tarfile<P: AsRef<Utf8Path> + std::fmt::Debug>(tarfile: P) -> Result<File> {
    OpenOptions::new()
        .read(true)
        .open(tarfile.as_ref())
        .map_err(|err| anyhow!("Cannot open tarfile {:?}: {}", tarfile, err))
}

pub trait Encoder: std::io::Write + Send {}
impl<T> Encoder for T where T: std::io::Write + Send {}

pub struct ArchiveBuilder<E: Encoder> {
    pub builder: tar::Builder<E>,
}

impl<E: Encoder> ArchiveBuilder<E> {
    pub fn new(builder: tar::Builder<E>) -> Self {
        Self { builder }
    }

    pub fn into_inner(self) -> Result<E> {
        self.builder.into_inner().context("Finalizing archive")
    }
}

/// Adds a package at `package_path` to a new zone image
/// being built using the `archive` builder.
pub async fn add_package_to_zone_archive<E: Encoder>(
    archive: &mut ArchiveBuilder<E>,
    package_path: &Utf8Path,
) -> Result<()> {
    let tmp = camino_tempfile::tempdir()?;
    let gzr = flate2::read::GzDecoder::new(open_tarfile(package_path)?);
    if gzr.header().is_none() {
        return Err(anyhow!(
            "Missing gzip header from {} - cannot add it to zone image",
            package_path,
        ));
    }
    let mut component_reader = tar::Archive::new(gzr);
    let entries = component_reader.entries()?;

    // First, unpack the existing entries
    for entry in entries {
        let mut entry = entry?;

        // Ignore the JSON header files
        let entry_path = entry.path()?;
        if entry_path == Utf8Path::new("oxide.json") {
            continue;
        }

        let entry_path: &Utf8Path = entry_path.strip_prefix("root/")?.try_into()?;
        let entry_unpack_path = tmp.path().join(entry_path);
        entry.unpack(&entry_unpack_path)?;

        let entry_path = entry.path()?.into_owned();
        let entry_path: &Utf8Path = entry_path.as_path().try_into()?;
        assert!(entry_unpack_path.exists());

        archive
            .builder
            .append_path_with_name_async(entry_unpack_path, entry_path)
            .await?;
    }
    Ok(())
}

pub async fn new_compressed_archive_builder(
    path: &Utf8Path,
) -> Result<ArchiveBuilder<GzEncoder<File>>> {
    let file = create_tarfile(path)?;
    // TODO: Consider using async compression, async tar.
    // It's not the *worst* thing in the world for a packaging tool to block
    // here, but it would help the other async threads remain responsive if
    // we avoided blocking.
    let gzw = GzEncoder::new(file, flate2::Compression::fast());
    let mut archive = Builder::new(gzw);
    archive.mode(tar::HeaderMode::Deterministic);

    Ok(ArchiveBuilder::new(archive))
}
