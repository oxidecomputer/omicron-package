// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Tracks inputs and outputs by digest to help caching

use anyhow::{anyhow, bail, Context};
use hex::ToHex;
use ring::digest::{Context as DigestContext, Digest as ShaDigest, SHA256};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use thiserror::Error;
use tokio::fs::File;
use tokio::io::{AsyncReadExt, AsyncWriteExt, BufReader};

// The cache is stored in the output directory, with the following convention:
//
// out/cache/<artifact name>.json
//
// XXX do we need to differentiate by target?

// Calculates the SHA256 digest for a file.
async fn get_sha256_digest(path: &PathBuf) -> anyhow::Result<ShaDigest> {
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

#[derive(PartialEq, Eq, Serialize, Deserialize)]
pub struct ArtifactManifest {
    // All inputs, which create this artifact
    inputs: BTreeMap<PathBuf, Digest>,

    // Output, created by this artifact
    output_path: PathBuf,
    output_digest: Digest,
}

impl ArtifactManifest {
    /// Reads all inputs and outputs, collecting their digests.
    pub async fn new_sha256(input_paths: Inputs, output_path: PathBuf) -> anyhow::Result<Self> {
        let mut inputs = BTreeMap::new();

        for input_path in input_paths {
            let digest = get_sha256_digest(&input_path).await?.into();
            inputs.insert(input_path, digest);
        }
        let output_digest = get_sha256_digest(&output_path).await?.into();

        Ok(Self {
            inputs,
            output_path,
            output_digest,
        })
    }

    // Writes a manifest file to a particular location.
    async fn write_to(&self, path: &PathBuf) -> anyhow::Result<()> {
        let Some(extension) = path.extension() else {
            bail!("Missing extension?");
        };
        if extension != "json" {
            bail!("JSON encoding is all we know. Write to a '.json' file?");
        }
        let mut f = File::create(path).await?;
        f.write_all(serde_json::to_string(&self)?.as_bytes())
            .await?;
        Ok(())
    }

    // Reads a manifest file to a particular location.
    //
    // Does not validate whether or not any corresponding artifacts exist.
    //
    // NOTE: It would probably be worth embedding this notion of "not validated"
    // into the type system?
    async fn read_from(path: &PathBuf) -> anyhow::Result<Option<Self>> {
        let Some(extension) = path.extension() else {
            bail!("Missing extension?");
        };
        if extension != "json" {
            bail!("JSON encoding is all we know. Read from a '.json' file?");
        }

        let mut f = match File::open(path).await {
            Ok(f) => f,
            Err(e) => {
                if matches!(e.kind(), std::io::ErrorKind::NotFound) {
                    return Ok(None);
                } else {
                    bail!(e);
                }
            }
        };
        let mut buffer = String::new();
        f.read_to_string(&mut buffer).await?;

        Ok(Some(serde_json::from_str(&buffer)?))
    }
}

#[derive(Error, Debug)]
pub enum CacheError {
    #[error("Cache Corrupted: {reason}\nDelete the directory at {cache_directory}")]
    Corrupted {
        cache_directory: PathBuf,
        reason: String,
    },

    #[error(transparent)]
    Other(#[from] anyhow::Error),
}

pub struct Cache {
    output_directory: PathBuf,
    cache_directory: PathBuf,
}

impl Cache {
    /// Ensures the cache directory exists within the output directory
    pub async fn new(output_directory: &Path) -> anyhow::Result<Self> {
        let cache_directory = output_directory.join("manifest-cache");
        tokio::fs::create_dir_all(&cache_directory).await?;
        Ok(Self {
            output_directory: output_directory.to_path_buf(),
            cache_directory,
        })
    }

    /// Looks up an entry from the cache.
    ///
    /// Confirms that the artifact exists.
    pub async fn lookup(
        &self,
        artifact_filename: &OsStr,
    ) -> Result<Option<ArtifactManifest>, CacheError> {
        let mut manifest_filename = OsString::from(artifact_filename);
        manifest_filename.push(".json");

        let manifest_path = self.cache_directory.join(manifest_filename);
        let artifact_path = self.output_directory.join(artifact_filename);

        let Some(manifest) = ArtifactManifest::read_from(&manifest_path)
            .await
            .with_context(|| format!("Could not lookup {} in cache", manifest_path.display()))?
        else {
            return Ok(None);
        };

        // TODO:
        // - ArtifactManifest::new_sha256(manifest.input_paths, manifest.output_path)
        //
        // - This would give us a new manifest we could compare with the
        // original? See if anything has changed?
        //
        // - but we ALSO need to see if the set of inputs has changed. Need to
        // walk the spots where we could add inputs AHEAD of time
        //
        // - As an optimization, we could do the following:
        //   - Check for the set of inputs/outputs (before hashing anything)
        //   - If eq, THEN hash one-by-one checking for equality of inputs
        //   - ... do we actually *need* to hash the output at all? We could,
        //   to verify the build is deterministic from the inputs?

        tokio::fs::try_exists(&artifact_path)
            .await
            .map_err(|e| CacheError::Corrupted {
                cache_directory: self.cache_directory.clone(),
                reason: format!("Manifest exists, but artifact doesn't: {e}"),
            })?;

        let Some(observed_filename) = manifest.output_path.file_name() else {
            return Err(CacheError::Corrupted {
                cache_directory: self.cache_directory.clone(),
                reason: format!(
                    "Missing output file name from manifest {}",
                    manifest.output_path.display()
                ),
            });
        };
        if observed_filename != artifact_filename {
            return Err(CacheError::Corrupted {
                cache_directory: self.cache_directory.clone(),
                reason: format!(
                    "Wrong output name in manifest (saw {}, expected {})",
                    observed_filename.to_string_lossy(),
                    artifact_filename.to_string_lossy()
                ),
            });
        }

        Ok(Some(manifest))
    }

    /// Updates an artifact's entry within the cache
    pub async fn update(&self, manifest: &ArtifactManifest) -> Result<(), CacheError> {
        let Some(artifact_filename) = manifest.output_path.file_name() else {
            return Err(anyhow!("Bad manifest: Missing output name").into());
        };

        let mut manifest_filename = OsString::from(artifact_filename);
        manifest_filename.push(".json");
        let manifest_path = self.cache_directory.join(manifest_filename);
        manifest.write_to(&manifest_path).await?;

        Ok(())
    }
}

// TODO: I could test this in isolation from the rest of packaging?
