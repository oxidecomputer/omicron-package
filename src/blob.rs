// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Tools for downloading blobs

use anyhow::{anyhow, Context, Result};
use chrono::{DateTime, FixedOffset, Utc};
use futures_util::StreamExt;
use reqwest::header::{CONTENT_LENGTH, LAST_MODIFIED};
use ring::digest::{Context as DigestContext, Digest, SHA256};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::str::FromStr;
use tokio::io::{AsyncReadExt, AsyncWriteExt, BufReader};

use crate::progress::{NoProgress, Progress};

// Path to the blob S3 Bucket.
const S3_BUCKET: &str = "https://oxide-omicron-build.s3.amazonaws.com";
// Name for the directory component where downloaded blobs are stored.
pub(crate) const BLOB: &str = "blob";

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum Source {
    S3(PathBuf),
    Buildomat(crate::package::PrebuiltBlob),
}

impl Source {
    pub(crate) fn get_url(&self) -> String {
        match self {
            Self::S3(s) => format!("{}/{}", S3_BUCKET, s.to_string_lossy()),
            Self::Buildomat(spec) => {
                format!(
                    "https://buildomat.eng.oxide.computer/public/file/oxidecomputer/{}/{}/{}/{}",
                    spec.repo, spec.series, spec.commit, spec.artifact
                )
            }
        }
    }

    async fn download_required(
        &self,
        url: &str,
        client: &reqwest::Client,
        destination: &Path,
    ) -> Result<bool> {
        if !destination.exists() {
            return Ok(true);
        }

        match self {
            Self::S3(_) => {
                // Issue a HEAD request to get the blob's size and last modified
                // time. If these match what's on disk, assume the blob is
                // current and don't re-download it.
                let head_response = client
                    .head(url)
                    .send()
                    .await?
                    .error_for_status()
                    .with_context(|| format!("HEAD failed for {}", url))?;
                let headers = head_response.headers();
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

                Ok(metadata.len() != content_length || metadata_modified != last_modified)
            }
            Self::Buildomat(blob_spec) => {
                let digest = get_sha256_digest(destination).await?;
                let expected_digest = hex::decode(&blob_spec.sha256)?;
                Ok(digest.as_ref() != expected_digest)
            }
        }
    }
}

// Downloads "source" from S3_BUCKET to "destination".
pub async fn download(progress: &impl Progress, source: &Source, destination: &Path) -> Result<()> {
    let blob = destination
        .file_name()
        .ok_or_else(|| anyhow!("missing blob filename"))?;

    let url = source.get_url();
    let client = reqwest::Client::new();
    if !source.download_required(&url, &client, destination).await? {
        return Ok(());
    }

    let response = client.get(url).send().await?.error_for_status()?;
    let response_headers = response.headers();

    // Grab update Content-Length from response headers, if present.
    // We only use it as a hint for the progress so no need to fail.
    let content_length = if let Some(Ok(Ok(resp_len))) = response_headers
        .get(CONTENT_LENGTH)
        .map(|c| c.to_str().map(u64::from_str))
    {
        Some(resp_len)
    } else {
        None
    };

    // If the server advertised a last-modified time for the blob, save it here
    // so that the downloaded blob's last-modified time can be set to it.
    let last_modified = if let Some(time) = response_headers.get(LAST_MODIFIED) {
        Some(chrono::DateTime::parse_from_rfc2822(time.to_str()?)?)
    } else {
        None
    };

    // Write file bytes to destination
    let mut file = tokio::fs::File::create(destination).await?;

    // Create a sub-progress for the blob download
    let blob_progress = if let Some(length) = content_length {
        progress.sub_progress(length)
    } else {
        Box::new(NoProgress::new())
    };
    blob_progress.set_message(blob.to_string_lossy().into_owned().into());

    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk?;
        file.write_all(&chunk).await?;
        blob_progress.increment_completed(chunk.len() as u64);
    }
    drop(blob_progress);

    // tokio performs async file I/O via thread pools in the background
    // and so just completing the `write_all` futures and dropping the
    // file here is not necessarily enough to ensure the blob has been
    // written out to the filesystem. This unfortunately can cause a race
    // condition as `tar-rs` will read the file metadata to write out the
    // tar Header and then subsequently read the file itself to write to
    // the archive. This can cause us to create a corrupted archive if the
    // file content size does not match the header size from the metadata.
    // All this to say we need to explicitly sync here before returning
    // and trying to add the blob to the archive.
    file.sync_all().await?;
    drop(file);

    // Set destination file's modified time based on HTTPS response
    if let Some(last_modified) = last_modified {
        filetime::set_file_mtime(
            destination,
            filetime::FileTime::from_system_time(last_modified.into()),
        )?;
    }

    Ok(())
}

async fn get_sha256_digest(path: &Path) -> Result<Digest> {
    let mut reader = BufReader::new(
        tokio::fs::File::open(path)
            .await
            .with_context(|| format!("could not open {path:?}"))?,
    );
    let mut context = DigestContext::new(&SHA256);
    let mut buffer = [0; 1024];

    loop {
        let count = reader
            .read(&mut buffer)
            .await
            .with_context(|| format!("failed to read {path:?}"))?;
        if count == 0 {
            break;
        } else {
            context.update(&buffer[..count]);
        }
    }
    Ok(context.finish())
}

#[test]
fn test_converts() {
    let content_length = "1966080";
    let last_modified = "Fri, 30 Apr 2021 22:37:39 GMT";

    let content_length: u64 = u64::from_str(content_length).unwrap();
    assert_eq!(1966080, content_length);

    let _last_modified: DateTime<FixedOffset> =
        chrono::DateTime::parse_from_rfc2822(last_modified).unwrap();
}
