// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Tracks inputs and outputs by digest to help caching.
//!
//! When a package is built, we keep track of all the inputs
//! used to construct that package, as well as the output file
//! name. This information is then converted into an [ArtifactManifest],
//! which tracks the digests of all those inputs, and this manifest
//! is written to the [CACHE_SUBDIRECTORY] within the output directory.
//!
//! When re-building, we can look up this manifest: if all the inputs
//! to build a package are the same, the output should be the same, so
//! we can use the cached output to avoid an unnecessary package construction
//! step.

use crate::input::{BuildInput, BuildInputs};

use anyhow::{anyhow, bail, Context};
use async_trait::async_trait;
use blake3::{Hash as BlakeDigest, Hasher as BlakeHasher};
use hex::ToHex;
use ring::digest::{Context as DigestContext, Digest as ShaDigest, SHA256};
use serde::{Deserialize, Serialize};
use std::ffi::OsString;
use std::marker::PhantomData;
use std::path::{Path, PathBuf};
use thiserror::Error;
use tokio::fs::File;
use tokio::io::{AsyncReadExt, AsyncWriteExt, BufReader};

pub const CACHE_SUBDIRECTORY: &'static str = "manifest-cache";

// The buffer size used to hash smaller files.
const HASH_BUFFER_SIZE: usize = 16 * (1 << 10);

// When files are larger than this size, we try to hash them using techniques
// like memory mapping and rayon.
const LARGE_HASH_SIZE: usize = 1 << 20;

/// Implemented by algorithms which can take digests of files.
#[async_trait]
pub trait FileDigester {
    async fn get_digest(path: &Path) -> anyhow::Result<Digest>;
}

#[async_trait]
impl FileDigester for ShaDigest {
    async fn get_digest(path: &Path) -> anyhow::Result<Digest> {
        let mut reader = BufReader::new(
            tokio::fs::File::open(&path)
                .await
                .with_context(|| format!("could not open {path:?}"))?,
        );
        let mut context = DigestContext::new(&SHA256);
        let mut buffer = [0; HASH_BUFFER_SIZE];
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
        let digest = context.finish().into();

        Ok(digest)
    }
}

#[async_trait]
impl FileDigester for BlakeDigest {
    async fn get_digest(path: &Path) -> anyhow::Result<Digest> {
        let size = path.metadata()?.len();

        let big_digest = size >= LARGE_HASH_SIZE as u64;
        let mut hasher = BlakeHasher::new();

        let digest = if big_digest {
            let path = path.to_path_buf();
            tokio::task::spawn_blocking(move || {
                hasher.update_mmap_rayon(&path)?;
                Ok::<Digest, anyhow::Error>(hasher.finalize().into())
            })
            .await??
        } else {
            let mut reader = BufReader::new(
                tokio::fs::File::open(&path)
                    .await
                    .with_context(|| format!("could not open {path:?}"))?,
            );
            let mut buf = [0; HASH_BUFFER_SIZE];
            loop {
                let count = reader
                    .read(&mut buf)
                    .await
                    .with_context(|| format!("failed to read {path:?}"))?;
                if count == 0 {
                    break;
                }

                let chunk = &buf[..count];
                hasher.update(chunk);
            }
            hasher.finalize().into()
        };

        Ok(digest)
    }
}

#[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Digest {
    // Sha256 support, as a hex-encoded string.
    Sha2(String),
    // Blake3 support, as a hex-encoded string.
    Blake3(String),
}

impl From<ShaDigest> for Digest {
    fn from(digest: ShaDigest) -> Self {
        Self::Sha2(digest.as_ref().encode_hex::<String>())
    }
}

impl From<BlakeDigest> for Digest {
    fn from(digest: BlakeDigest) -> Self {
        Self::Blake3(digest.as_bytes().encode_hex::<String>())
    }
}

pub type Inputs = Vec<BuildInput>;

// It's not actually a map, because serde doesn't like enum keys.
// But this is logically a "BTreeMap".
#[derive(PartialEq, Eq, Serialize, Deserialize)]
struct InputMap(Vec<InputEntry>);

#[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
struct InputEntry {
    key: BuildInput,
    // TODO: could track "mtime", or "length"? Quick alternative to "digest"?
    // TODO: Or should I just *try* sha1??
    //
    // Maybe should be part of BuildInput structure?
    value: Option<Digest>,
}

#[derive(PartialEq, Eq, Serialize, Deserialize)]
pub struct ArtifactManifest<D = BlakeDigest> {
    // All inputs, which create this artifact
    inputs: InputMap,

    // Output, created by this artifact
    output_path: PathBuf,

    // Which digest is being used?
    phantom: PhantomData<D>,
}

impl<D: FileDigester> ArtifactManifest<D> {
    /// Reads all inputs and outputs, collecting their digests.
    pub async fn new(inputs: &BuildInputs, output_path: PathBuf) -> anyhow::Result<Self> {
        let result = Self::new_internal(inputs, output_path, None).await?;
        Ok(result)
    }

    // If the optional "compare_with" field is supplied, construction
    // of the ArtifactManifest exits early if any of the inputs are not
    // equal to the digests found in "compare_with". This helps improve
    // the "cache miss" case, by allowing us to stop calculating hashes
    // as soon as we find any divergence.
    async fn new_internal(
        inputs: &BuildInputs,
        output_path: PathBuf,
        compare_with: Option<&Self>,
    ) -> Result<Self, CacheError> {
        let input_entry_tasks = inputs.0.iter().cloned().enumerate().map(|(i, input)| {
            let expected_input = compare_with.map(|manifest| &manifest.inputs.0[i]);
            async move {
                let digest = if let Some(input_path) = input.input_path() {
                    Some(D::get_digest(input_path).await?.into())
                } else {
                    None
                };
                let input = InputEntry {
                    key: input.clone(),
                    value: digest,
                };

                if let Some(expected_input) = expected_input {
                    if *expected_input != input {
                        CacheError::miss(format!(
                            "Differing build inputs.\nSaw {:#?}\nExpected {:#?})",
                            input, expected_input
                        ));
                    }
                };

                Ok::<_, CacheError>(input)
            }
        });

        let inputs = InputMap(futures::future::try_join_all(input_entry_tasks).await?);

        Ok(Self {
            inputs,
            output_path,
            phantom: PhantomData,
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
        let serialized = serde_json::to_string(&self)
            .with_context(|| "Failed to serialize ArtifactManifest to JSON")?;

        let mut f = File::create(path).await?;
        f.write_all(serialized.as_bytes()).await?;
        Ok(())
    }

    // Reads a manifest file to a particular location.
    //
    // Does not validate whether or not any corresponding artifacts exist.
    async fn read_from(path: &PathBuf) -> Result<Self, CacheError> {
        let Some(extension) = path.extension() else {
            return Err(anyhow!("Missing extension?").into());
        };
        if extension != "json" {
            return Err(anyhow!("JSON encoding is all we know. Read from a '.json' file?").into());
        }

        let mut f = match File::open(path).await {
            Ok(f) => f,
            Err(e) => {
                if matches!(e.kind(), std::io::ErrorKind::NotFound) {
                    return Err(CacheError::miss(format!(
                        "File {} not found",
                        path.display()
                    )));
                } else {
                    return Err(anyhow!(e).into());
                }
            }
        };
        let mut buffer = String::new();
        f.read_to_string(&mut buffer)
            .await
            .map_err(|e| anyhow!(e))?;

        // In the case that we cannot read the manifest, treat it as "missing".
        // This will force a rebuild anyway.
        let Ok(manifest) = serde_json::from_str(&buffer) else {
            return Err(CacheError::miss(format!(
                "Cannot parse manifest at {}",
                path.display()
            )));
        };
        Ok(manifest)
    }
}

/// Errors that can be returned when looking up cached artifacts.
#[derive(Error, Debug)]
pub enum CacheError {
    /// Identifies that cache lookup has failed, for a wide number of reasons,
    /// but that we should probably try to continue with package building
    /// anyway.
    #[error("Cache Miss: {reason}")]
    CacheMiss { reason: String },

    /// Other errors, which could indicate a more fundamental problem.
    ///
    /// These errors encourage callers to exit immediately, rather than
    /// treating the failure like a "miss".
    #[error(transparent)]
    Other(#[from] anyhow::Error),
}

impl CacheError {
    // Convenience wrapper
    fn miss<T: Into<String>>(t: T) -> Self {
        CacheError::CacheMiss { reason: t.into() }
    }
}

pub struct Cache {
    output_directory: PathBuf,
    cache_directory: PathBuf,
}

impl Cache {
    /// Ensures the cache directory exists within the output directory
    pub async fn new(output_directory: &Path) -> anyhow::Result<Self> {
        let cache_directory = output_directory.join(CACHE_SUBDIRECTORY);
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
        artifact_filename: &str,
        inputs: &BuildInputs,
    ) -> Result<ArtifactManifest, CacheError> {
        let mut manifest_filename = OsString::from(artifact_filename);
        manifest_filename.push(".json");

        let manifest_path = self.cache_directory.join(manifest_filename);
        let artifact_path = self.output_directory.join(artifact_filename);

        // Look up the manifest file in the cache
        let manifest = ArtifactManifest::read_from(&manifest_path).await?;

        // Do a quick check if the input files are different.
        //
        // We'll actually validate the digests later, but this lets us bail
        // early if any files were added or removed.
        if inputs
            .0
            .iter()
            .ne(manifest.inputs.0.iter().map(|entry| &entry.key))
        {
            return Err(CacheError::miss("Set of inputs has changed"));
        }
        if artifact_path != manifest.output_path {
            return Err(CacheError::miss(format!(
                "Output path changed from {} -> {}",
                manifest.output_path.display(),
                artifact_path.display()
            )));
        }

        // Confirm the output file exists
        if !tokio::fs::try_exists(&artifact_path)
            .await
            .map_err(|e| CacheError::miss(format!("Cannot locate output artifact: {e}")))?
        {
            return Err(CacheError::miss(format!("Cannot find output artifact")));
        }

        // Confirm the output matches.
        let Some(observed_filename) = manifest.output_path.file_name() else {
            return Err(CacheError::miss(format!(
                "Missing output file name from manifest {}",
                manifest.output_path.display()
            )));
        };
        if observed_filename != artifact_filename {
            return Err(CacheError::miss(format!(
                "Wrong output name in manifest (saw {}, expected {})",
                observed_filename.to_string_lossy(),
                artifact_filename
            )));
        }

        // Finally, compare the manifests, including their digests.
        //
        // This calculation bails out early if any inputs don't match.
        let calculated_manifest =
            ArtifactManifest::new_internal(inputs, artifact_path.to_path_buf(), Some(&manifest))
                .await?;

        // This is a hard stop-gap against any other differences in the
        // manifests. The error message here is worse (we don't know "why"),
        // but it's a quick check that's protective.
        if calculated_manifest != manifest {
            return Err(CacheError::miss("Manifests appear different"));
        }

        Ok(manifest)
    }

    /// Updates an artifact's entry within the cache
    //
    // TODO: Don't take ArtifactManifest as input
    // TODO: Re-create it. This means "cache no-op" options can be a cache
    // parameter.
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
