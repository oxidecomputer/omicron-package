// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use std::{borrow::Cow, fmt, str::FromStr};

use serde::{Deserialize, Serialize};
use thiserror::Error;

macro_rules! ident_newtype {
    ($id:ident) => {
        impl $id {
            /// Creates a new identifier at runtime.
            pub fn new<S: Into<String>>(s: S) -> Result<Self, InvalidConfigIdent> {
                ConfigIdent::new(s).map(Self)
            }

            /// Creates a new identifier from a static string.
            pub fn new_static(s: &'static str) -> Result<Self, InvalidConfigIdent> {
                ConfigIdent::new_static(s).map(Self)
            }

            /// Creates a new identifier at compile time, panicking if it is
            /// invalid.
            pub const fn new_const(s: &'static str) -> Self {
                Self(ConfigIdent::new_const(s))
            }

            /// Returns the identifier as a string.
            #[inline]
            pub fn as_str(&self) -> &str {
                self.0.as_str()
            }

            #[inline]
            #[allow(dead_code)]
            pub(crate) fn as_ident(&self) -> &ConfigIdent {
                &self.0
            }
        }

        impl AsRef<str> for $id {
            #[inline]
            fn as_ref(&self) -> &str {
                self.0.as_ref()
            }
        }

        impl std::fmt::Display for $id {
            #[inline]
            fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                self.0.fmt(f)
            }
        }

        impl FromStr for $id {
            type Err = InvalidConfigIdent;

            fn from_str(s: &str) -> Result<Self, Self::Err> {
                ConfigIdent::new(s).map(Self)
            }
        }
    };
}

/// A unique identifier for a package name.
///
/// Package names must be:
///
/// * non-empty
/// * ASCII printable
/// * first character must be a letter
/// * contain only letters, numbers, underscores, and hyphens
///
/// These generally match the rules of Rust package names.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Deserialize, Serialize)]
#[serde(transparent)]
pub struct PackageName(ConfigIdent);
ident_newtype!(PackageName);

/// A unique identifier for a service name.
///
/// Package names must be:
///
/// * non-empty
/// * ASCII printable
/// * first character must be a letter
/// * contain only letters, numbers, underscores, and hyphens
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Deserialize, Serialize)]
#[serde(transparent)]
pub struct ServiceName(ConfigIdent);
ident_newtype!(ServiceName);

/// A unique identifier for a target preset.
///
/// Package names must be:
///
/// * non-empty
/// * ASCII printable
/// * first character must be a letter
/// * contain only letters, numbers, underscores, and hyphens
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Deserialize, Serialize)]
#[serde(transparent)]
pub struct PresetName(ConfigIdent);
ident_newtype!(PresetName);

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(transparent)]
pub(crate) struct ConfigIdent(Cow<'static, str>);

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
    use serde_json::json;
    use test_strategy::proptest;

    static IDENT_REGEX: &str = r"[a-zA-Z][a-zA-Z0-9_-]*";

    #[test]
    fn valid_identifiers() {
        let valid = [
            "a", "ab", "a1", "a_", "a-", "a_b", "a-b", "a1_", "a1-", "a1_b", "a1-b",
        ];
        for &id in &valid {
            ConfigIdent::new(id).unwrap_or_else(|error| {
                panic!(
                    "ConfigIdent::new for {} should have succeeded, but failed with: {:?}",
                    id, error
                );
            });
            PackageName::new(id).unwrap_or_else(|error| {
                panic!(
                    "PackageName::new for {} should have succeeded, but failed with: {:?}",
                    id, error
                );
            });
            ServiceName::new(id).unwrap_or_else(|error| {
                panic!(
                    "ServiceName::new for {} should have succeeded, but failed with: {:?}",
                    id, error
                );
            });
            PresetName::new(id).unwrap_or_else(|error| {
                panic!(
                    "PresetName::new for {} should have succeeded, but failed with: {:?}",
                    id, error
                );
            });
        }
    }

    #[test]
    fn invalid_identifiers() {
        let invalid = [
            "", "1", "_", "-", "1_", "-a", "_a", "a!", "a ", "a\n", "a\t", "a\r", "a\x7F", "aɑ",
        ];
        for &id in &invalid {
            ConfigIdent::new(id)
                .expect_err(&format!("ConfigIdent::new for {} should have failed", id));
            PackageName::new(id)
                .expect_err(&format!("PackageName::new for {} should have failed", id));
            ServiceName::new(id)
                .expect_err(&format!("ServiceName::new for {} should have failed", id));
            PresetName::new(id)
                .expect_err(&format!("PresetName::new for {} should have failed", id));

            // Also ensure that deserialization fails.
            let json = json!(id);
            serde_json::from_value::<ConfigIdent>(json.clone()).expect_err(&format!(
                "ConfigIdent deserialization for {} should have failed",
                id
            ));
            serde_json::from_value::<PackageName>(json.clone()).expect_err(&format!(
                "PackageName deserialization for {} should have failed",
                id
            ));
            serde_json::from_value::<ServiceName>(json.clone()).expect_err(&format!(
                "ServiceName deserialization for {} should have failed",
                id
            ));
            serde_json::from_value::<PresetName>(json.clone()).expect_err(&format!(
                "PresetName deserialization for {} should have failed",
                id
            ));
        }
    }

    #[proptest]
    fn valid_identifiers_proptest(#[strategy(IDENT_REGEX)] id: String) {
        ConfigIdent::new(&id).unwrap_or_else(|error| {
            panic!(
                "ConfigIdent::new for {} should have succeeded, but failed with: {:?}",
                id, error
            );
        });
        PackageName::new(&id).unwrap_or_else(|error| {
            panic!(
                "PackageName::new for {} should have succeeded, but failed with: {:?}",
                id, error
            );
        });
        ServiceName::new(&id).unwrap_or_else(|error| {
            panic!(
                "ServiceName::new for {} should have succeeded, but failed with: {:?}",
                id, error
            );
        });
        PresetName::new(&id).unwrap_or_else(|error| {
            panic!(
                "PresetName::new for {} should have succeeded, but failed with: {:?}",
                id, error
            );
        });
    }

    #[derive(Debug, Deserialize, Serialize, PartialEq, Eq)]
    struct AllIdentifiers {
        config: ConfigIdent,
        package: PackageName,
        service: ServiceName,
        preset: PresetName,
    }

    #[proptest]
    fn valid_identifiers_proptest_serde(#[strategy(IDENT_REGEX)] id: String) {
        let all = AllIdentifiers {
            config: ConfigIdent::new(&id).unwrap(),
            package: PackageName::new(&id).unwrap(),
            service: ServiceName::new(&id).unwrap(),
            preset: PresetName::new(&id).unwrap(),
        };

        let json = serde_json::to_value(&all).unwrap();
        let deserialized: AllIdentifiers = serde_json::from_value(json).unwrap();
        assert_eq!(all, deserialized);
    }
}
