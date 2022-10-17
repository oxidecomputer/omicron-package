// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Tools for downloading blobs

use anyhow::{anyhow, bail, Result};
use chrono::{DateTime, FixedOffset, Utc};
use reqwest::header::{CONTENT_LENGTH, LAST_MODIFIED};
use std::path::Path;
use std::str::FromStr;
use tokio::io::AsyncWriteExt;

// Path to the blob S3 Bucket.
const S3_BUCKET: &str = "https://oxide-omicron-build.s3.amazonaws.com";
// Name for the directory component where downloaded blobs are stored.
pub(crate) const BLOB: &str = "blob";

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

#[test]
fn test_converts() {
    let content_length = "1966080";
    let last_modified = "Fri, 30 Apr 2021 22:37:39 GMT";

    let content_length: u64 = u64::from_str(content_length).unwrap();
    assert_eq!(1966080, content_length);

    let _last_modified: DateTime<FixedOffset> =
        chrono::DateTime::parse_from_rfc2822(last_modified).unwrap();
}
