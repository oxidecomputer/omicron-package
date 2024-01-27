// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Implements file digest support for caching

use anyhow::Context;
use async_trait::async_trait;
use blake3::{Hash as BlakeDigest, Hasher as BlakeHasher};
use camino::Utf8Path;
use hex::ToHex;
use ring::digest::{Context as DigestContext, Digest as ShaDigest, SHA256};
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncReadExt, BufReader};

// The buffer size used to hash smaller files.
const HASH_BUFFER_SIZE: usize = 16 * (1 << 10);

// When files are larger than this size, we try to hash them using techniques
// like memory mapping and rayon.
//
// NOTE: This is currently only blake3-specific.
const LARGE_HASH_SIZE: usize = 1 << 20;

/// Implemented by algorithms which can take digests of files.
#[async_trait]
pub trait FileDigester {
    async fn get_digest(path: &Utf8Path) -> anyhow::Result<Digest>;
}

#[async_trait]
impl FileDigester for ShaDigest {
    async fn get_digest(path: &Utf8Path) -> anyhow::Result<Digest> {
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
    async fn get_digest(path: &Utf8Path) -> anyhow::Result<Digest> {
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

/// Although we support both interfaces, we use blake3 digests by default.
pub type DefaultDigest = BlakeDigest;
