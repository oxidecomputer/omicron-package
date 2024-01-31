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

use crate::digest::{DefaultDigest, Digest, FileDigester};
use crate::input::{BuildInput, BuildInputs};

use anyhow::{anyhow, bail, Context};
use camino::{Utf8Path, Utf8PathBuf};
use serde::{Deserialize, Serialize};
use std::marker::PhantomData;
use thiserror::Error;
use tokio::fs::File;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

pub const CACHE_SUBDIRECTORY: &str = "manifest-cache";

pub type Inputs = Vec<BuildInput>;

// It's not actually a map, because serde doesn't like enum keys.
//
// This has the side-effect that changing the order of input files
// changes the package.
#[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
struct InputMap(Vec<InputEntry>);

#[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
struct InputEntry {
    key: BuildInput,
    value: Option<Digest>,
}

#[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ArtifactManifest<D = DefaultDigest> {
    // All inputs, which create this artifact
    inputs: InputMap,

    // Output, created by this artifact
    output_path: Utf8PathBuf,

    // Which digest is being used?
    phantom: PhantomData<D>,
}

impl<D: FileDigester> ArtifactManifest<D> {
    /// Reads all inputs and outputs, collecting their digests.
    async fn new(inputs: &BuildInputs, output_path: Utf8PathBuf) -> anyhow::Result<Self> {
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
        output_path: Utf8PathBuf,
        compare_with: Option<&Self>,
    ) -> Result<Self, CacheError> {
        let input_entry_tasks = inputs.0.iter().cloned().enumerate().map(|(i, input)| {
            let expected_input = compare_with.map(|manifest| &manifest.inputs.0[i]);
            async move {
                let digest = if let Some(input_path) = input.input_path() {
                    Some(D::get_digest(input_path).await?)
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
    async fn write_to(&self, path: &Utf8PathBuf) -> anyhow::Result<()> {
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
    async fn read_from(path: &Utf8PathBuf) -> Result<Self, CacheError> {
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
                    return Err(CacheError::miss(format!("File {} not found", path)));
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
                path
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

/// Provides access to a set of manifests describing packages.
///
/// Provides two primary operations:
/// - [Self::lookup]: Support for finding previously-built packages
/// - [Self::update]: Support for updating a package's latest manifest
pub struct Cache {
    disabled: bool,
    cache_directory: Utf8PathBuf,
}

impl Cache {
    /// Ensures the cache directory exists within the output directory
    pub async fn new(output_directory: &Utf8Path) -> anyhow::Result<Self> {
        let cache_directory = output_directory.join(CACHE_SUBDIRECTORY);
        tokio::fs::create_dir_all(&cache_directory).await?;
        Ok(Self {
            disabled: false,
            cache_directory,
        })
    }

    /// If "disable" is true, causes cache operations to be no-ops.
    /// Otherwise, causes the cache to act normally.
    pub fn set_disable(&mut self, disable: bool) {
        self.disabled = disable;
    }

    /// Looks up an entry from the cache.
    ///
    /// Confirms that the artifact exists.
    pub async fn lookup(
        &self,
        inputs: &BuildInputs,
        output_path: &Utf8Path,
    ) -> Result<ArtifactManifest, CacheError> {
        if self.disabled {
            return Err(CacheError::miss("Cache disabled"));
        }

        let artifact_filename = output_path
            .file_name()
            .ok_or_else(|| CacheError::Other(anyhow!("Output has no file name")))?;
        let mut manifest_filename = String::from(artifact_filename);
        manifest_filename.push_str(".json");

        let manifest_path = self.cache_directory.join(manifest_filename);

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
        if output_path != manifest.output_path {
            return Err(CacheError::miss(format!(
                "Output path changed from {} -> {}",
                manifest.output_path, output_path,
            )));
        }

        // Confirm the output file exists
        if !tokio::fs::try_exists(&output_path)
            .await
            .map_err(|e| CacheError::miss(format!("Cannot locate output artifact: {e}")))?
        {
            return Err(CacheError::miss("Output does not exist"));
        }

        // Confirm the output matches.
        let Some(observed_filename) = manifest.output_path.file_name() else {
            return Err(CacheError::miss(format!(
                "Missing output file name from manifest {}",
                manifest.output_path
            )));
        };
        if observed_filename != artifact_filename {
            return Err(CacheError::miss(format!(
                "Wrong output name in manifest (saw {}, expected {})",
                observed_filename, artifact_filename
            )));
        }

        // Finally, compare the manifests, including their digests.
        //
        // This calculation bails out early if any inputs don't match.
        let calculated_manifest =
            ArtifactManifest::new_internal(inputs, output_path.to_path_buf(), Some(&manifest))
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
    pub async fn update(
        &self,
        inputs: &BuildInputs,
        output_path: &Utf8Path,
    ) -> Result<(), CacheError> {
        if self.disabled {
            // Return immediately, regardless of the input. We have nothing to
            // calculate, and nothing to save.
            return Ok(());
        }

        // This call actually acquires the digests for all inputs
        let manifest =
            ArtifactManifest::<DefaultDigest>::new(inputs, output_path.to_path_buf()).await?;

        let Some(artifact_filename) = manifest.output_path.file_name() else {
            return Err(anyhow!("Bad manifest: Missing output name").into());
        };

        let mut manifest_filename = String::from(artifact_filename);
        manifest_filename.push_str(".json");
        let manifest_path = self.cache_directory.join(manifest_filename);
        manifest.write_to(&manifest_path).await?;

        Ok(())
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use crate::input::MappedPath;
    use camino::Utf8PathBuf;
    use camino_tempfile::{tempdir, Utf8TempDir};

    struct CacheTest {
        _input_dir: Utf8TempDir,
        output_dir: Utf8TempDir,

        input_path: Utf8PathBuf,
        output_path: Utf8PathBuf,
    }

    impl CacheTest {
        fn new() -> Self {
            let input_dir = tempdir().unwrap();
            let output_dir = tempdir().unwrap();
            let input_path = input_dir.path().join("binary.exe");
            let output_path = output_dir.path().join("output.tar.gz");
            Self {
                _input_dir: input_dir,
                output_dir,
                input_path,
                output_path,
            }
        }

        async fn create_input(&self, contents: &str) {
            tokio::fs::write(&self.input_path, contents).await.unwrap()
        }

        async fn create_output(&self, contents: &str) {
            tokio::fs::write(&self.output_path, contents).await.unwrap()
        }

        async fn remove_output(&self) {
            tokio::fs::remove_file(&self.output_path).await.unwrap()
        }
    }

    fn expect_missing_manifest(err: &CacheError, file: &str) {
        match &err {
            CacheError::CacheMiss { reason } => {
                let expected = format!("{file}.json not found");
                assert!(reason.contains(&expected), "{}", reason);
            }
            _ => panic!("Unexpected error: {}", err),
        }
    }

    fn expect_cache_disabled(err: &CacheError) {
        match &err {
            CacheError::CacheMiss { reason } => {
                assert!(reason.contains("Cache disabled"), "{}", reason);
            }
            _ => panic!("Unexpected error: {}", err),
        }
    }

    fn expect_changed_manifests(err: &CacheError) {
        match &err {
            CacheError::CacheMiss { reason } => {
                assert!(reason.contains("Manifests appear different"), "{}", reason);
            }
            _ => panic!("Unexpected error: {}", err),
        }
    }

    fn expect_missing_output(err: &CacheError) {
        match &err {
            CacheError::CacheMiss { reason } => {
                assert!(reason.contains("Output does not exist"), "{}", reason);
            }
            _ => panic!("Unexpected error: {}", err),
        }
    }

    #[tokio::test]
    async fn test_cache_lookup_misses_before_update() {
        let test = CacheTest::new();

        test.create_input("Hi I'm the input file").await;
        let inputs = BuildInputs(vec![BuildInput::add_file(MappedPath {
            from: test.input_path.to_path_buf(),
            to: Utf8PathBuf::from("/very/important/file"),
        })
        .unwrap()]);

        let cache = Cache::new(test.output_dir.path()).await.unwrap();

        // Look for the package in the cache. It shouldn't exist.
        let err = cache.lookup(&inputs, &test.output_path).await.unwrap_err();
        expect_missing_manifest(&err, "output.tar.gz");

        // Create the output we're expecting
        test.create_output("Hi I'm the output file").await;

        // Still expect a failure; we haven't called "cache.update".
        let err = cache.lookup(&inputs, &test.output_path).await.unwrap_err();
        expect_missing_manifest(&err, "output.tar.gz");
    }

    #[tokio::test]
    async fn test_cache_lookup_hits_after_update() {
        let test = CacheTest::new();

        test.create_input("Hi I'm the input file").await;
        let inputs = BuildInputs(vec![BuildInput::add_file(MappedPath {
            from: test.input_path.to_path_buf(),
            to: Utf8PathBuf::from("/very/important/file"),
        })
        .unwrap()]);

        // Create the output we're expecting
        test.create_output("Hi I'm the output file").await;

        let cache = Cache::new(test.output_dir.path()).await.unwrap();

        // If we update the cache, we expect a hit.
        cache.update(&inputs, &test.output_path).await.unwrap();
        cache.lookup(&inputs, &test.output_path).await.unwrap();

        // If we update the input again, we expect a miss.
        test.create_input("hi i'M tHe InPuT fIlE").await;
        let err = cache.lookup(&inputs, &test.output_path).await.unwrap_err();
        expect_changed_manifests(&err);
    }

    #[tokio::test]
    async fn test_cache_lookup_misses_after_removing_output() {
        let test = CacheTest::new();

        test.create_input("Hi I'm the input file").await;
        let inputs = BuildInputs(vec![BuildInput::add_file(MappedPath {
            from: test.input_path.to_path_buf(),
            to: Utf8PathBuf::from("/very/important/file"),
        })
        .unwrap()]);

        // Create the output we're expecting
        test.create_output("Hi I'm the output file").await;

        let cache = Cache::new(test.output_dir.path()).await.unwrap();

        // If we update the cache, we expect a hit.
        cache.update(&inputs, &test.output_path).await.unwrap();
        cache.lookup(&inputs, &test.output_path).await.unwrap();

        // If we remove the output file, we expect a miss.
        // This is somewhat of a "special case", as all the inputs are the same.
        test.remove_output().await;
        let err = cache.lookup(&inputs, &test.output_path).await.unwrap_err();
        expect_missing_output(&err);
    }

    #[tokio::test]
    async fn test_cache_disabled_always_misses() {
        let test = CacheTest::new();

        test.create_input("Hi I'm the input file").await;
        let inputs = BuildInputs(vec![BuildInput::add_file(MappedPath {
            from: test.input_path.to_path_buf(),
            to: Utf8PathBuf::from("/very/important/file"),
        })
        .unwrap()]);

        // Create the output we're expecting
        test.create_output("Hi I'm the output file").await;

        let mut cache = Cache::new(test.output_dir.path()).await.unwrap();
        cache.set_disable(true);

        // Updating the cache should still succeed, though it'll do nothing.
        cache.update(&inputs, &test.output_path).await.unwrap();

        // The lookup will miss, as the cache has been disabled.
        let err = cache.lookup(&inputs, &test.output_path).await.unwrap_err();
        expect_cache_disabled(&err);
    }
}
