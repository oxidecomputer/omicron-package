// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use std::{borrow::Cow, fmt, str::FromStr};

use serde::{Deserialize, Serialize};
use thiserror::Error;

/// A unique identifier for a configuration parameter.
///
/// Config identifiers must be:
///
/// * non-empty
/// * ASCII printable
/// * first character must be a letter
/// * contain only letters, numbers, underscores, and hyphens
///
/// In general, config identifiers represent Rust package and Oxide service names.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(transparent)]
pub struct ConfigIdent(Cow<'static, str>);

impl ConfigIdent {
    /// Creates a new config identifier at runtime.
    pub fn new<S: Into<String>>(s: S) -> Result<Self, InvalidConfigIdent> {
        let s = s.into();
        Self::validate(&s)?;
        Ok(Self(Cow::Owned(s)))
    }

    /// Creates a new config identifier from a static string.
    pub fn new_static(s: &'static str) -> Result<Self, InvalidConfigIdent> {
        Self::validate(s)?;
        Ok(Self(Cow::Borrowed(s)))
    }

    /// Creates a new config identifier at compile time, panicking if the
    /// identifier is invalid.
    pub const fn new_const(s: &'static str) -> Self {
        match Self::validate(s) {
            Ok(_) => Self(Cow::Borrowed(s)),
            Err(error) => panic!("{}", error.as_static_str()),
        }
    }

    const fn validate(id: &str) -> Result<(), InvalidConfigIdent> {
        if id.is_empty() {
            return Err(InvalidConfigIdent::Empty);
        }

        let bytes = id.as_bytes();
        if !bytes[0].is_ascii_alphabetic() {
            return Err(InvalidConfigIdent::StartsWithNonLetter);
        }

        let mut bytes = match bytes {
            [_, rest @ ..] => rest,
            [] => panic!("already checked that it's non-empty"),
        };
        while let [next, rest @ ..] = &bytes {
            if !(next.is_ascii_alphanumeric() || *next == b'_' || *next == b'-') {
                break;
            }
            bytes = rest;
        }

        if !bytes.is_empty() {
            return Err(InvalidConfigIdent::ContainsInvalidCharacters);
        }

        Ok(())
    }

    /// Returns the identifier as a string.
    #[inline]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl FromStr for ConfigIdent {
    type Err = InvalidConfigIdent;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::new(s)
    }
}

impl<'de> Deserialize<'de> for ConfigIdent {
    fn deserialize<D>(deserializer: D) -> Result<ConfigIdent, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let s = String::deserialize(deserializer)?;
        Self::new(s).map_err(serde::de::Error::custom)
    }
}

impl AsRef<str> for ConfigIdent {
    #[inline]
    fn as_ref(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for ConfigIdent {
    #[inline]
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

/// Errors that can occur when creating a `ConfigIdent`.
#[derive(Clone, Debug, Error)]
pub enum InvalidConfigIdent {
    Empty,
    NonAsciiPrintable,
    StartsWithNonLetter,
    ContainsInvalidCharacters,
}

impl InvalidConfigIdent {
    pub const fn as_static_str(&self) -> &'static str {
        match self {
            Self::Empty => "config identifier must be non-empty",
            Self::NonAsciiPrintable => "config identifier must be ASCII printable",
            Self::StartsWithNonLetter => "config identifier must start with a letter",
            Self::ContainsInvalidCharacters => {
                "config identifier must contain only letters, numbers, underscores, and hyphens"
            }
        }
    }
}

impl fmt::Display for InvalidConfigIdent {
    #[inline]
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        self.as_static_str().fmt(f)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn valid_identifiers() {
        let valid = [
            "a", "ab", "a1", "a_", "a-", "a_b", "a-b", "a1_", "a1-", "a1_b", "a1-b",
        ];
        for &id in &valid {
            ConfigIdent::new(id).unwrap_or_else(|error| {
                panic!("{} should have succeeded, but failed with: {:?}", id, error);
            });
        }
    }

    #[test]
    fn invalid_identifiers() {
        let invalid = [
            "", "1", "_", "-", "1_", "-a", "_a", "a!", "a ", "a\n", "a\t", "a\r", "a\x7F", "aɑ",
        ];
        for &id in &invalid {
            ConfigIdent::new(id).expect_err(&format!("{} should have failed", id));
        }
    }
}
