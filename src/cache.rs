// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Tracks inputs and outputs by digest to help caching

use anyhow::{Context, Result, bail};
use hex::ToHex;
use ring::digest::{Context as DigestContext, Digest as ShaDigest, SHA256};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::PathBuf;
use tokio::fs::File;
use tokio::io::{AsyncReadExt, AsyncWriteExt, BufReader};

// The cache is stored in the output directory, with the following convention:
//
// out/cache/<artifact name>.json
//
// XXX do we need to differentiate by target?

// Calculates the SHA256 digest for a file.
async fn get_sha256_digest(path: &PathBuf) -> Result<ShaDigest> {
    let mut reader = BufReader::new(
        tokio::fs::File::open(&path)
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

#[derive(PartialEq, Eq, Serialize, Deserialize)]
enum Digest {
    // Sha256 support, as a hex-encoded string.
    Sha2(String),

    // I'd be interested in adding blake3 support someday, but I don't *love*
    // the idea of diverging from our TUF repos, which are currently SHA2.
    //
    // blake3 would be faster though!
}

impl From<ShaDigest> for Digest {
    fn from(digest: ShaDigest) -> Self {
        Self::Sha2(digest.as_ref().encode_hex::<String>())
    }
}

pub type Inputs = Vec<PathBuf>;
pub type Outputs = Vec<PathBuf>;

#[derive(PartialEq, Eq, Serialize, Deserialize)]
struct ArtifactManifest {
    // All inputs, which create this artifact
    inputs: BTreeMap<PathBuf, Digest>,

    // All outputs, created by this artifact
    //
    // For most artifacts, this is a single entry.
    outputs: BTreeMap<PathBuf, Digest>,
}

impl ArtifactManifest {
    /// Reads all inputs and outputs, collecting their digests.
    pub async fn new_sha256(
        input_paths: Inputs,
        output_paths: Outputs,
    ) -> Result<Self> {
        let mut inputs = BTreeMap::new();
        let mut outputs = BTreeMap::new();

        for input_path in input_paths {
            let digest = get_sha256_digest(&input_path).await?.into();
            inputs.insert(input_path, digest);
        }
        for output_path in output_paths {
            let digest = get_sha256_digest(&output_path).await?.into();
            outputs.insert(output_path, digest);
        }

        Ok(Self {
            inputs,
            outputs,
        })
    }

    /// Writes a manifest file to a particular location.
    pub async fn write_to(&self, path: &PathBuf) -> Result<()> {
        if !path.ends_with(".json") {
            bail!("JSON encoding is all we know. Write to a '.json' file?");
        }
        let mut f = File::create(path).await?;
        f.write_all(serde_json::to_string(&self)?.as_bytes()).await?;
        Ok(())
    }

    /// Reads a manifest file to a particular location.
    pub async fn read_from(path: &PathBuf) -> Result<Self> {
        if !path.ends_with(".json") {
            bail!("JSON encoding is all we know. Read from a '.json' file?");
        }
        let mut f = File::open(path).await?;
        let mut buffer = String::new();
        f.read_to_string(&mut buffer).await?;

        Ok(serde_json::from_str(&buffer)?)
    }
}

struct Cache {
    output_directory: PathBuf,
}

impl Cache {
}
