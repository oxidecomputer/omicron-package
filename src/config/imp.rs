// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Configuration for a package.

use crate::package::{Package, PackageOutput, PackageSource};
use crate::target::Target;
use serde_derive::Deserialize;
use std::collections::BTreeMap;
use std::path::Path;
use thiserror::Error;
use topological_sort::TopologicalSort;

use super::PackageName;

/// Describes a set of packages to act upon.
///
/// This structure maps "package name" to "package"
pub struct PackageMap<'a>(pub BTreeMap<&'a PackageName, &'a Package>);

// The name of a file which should be created by building a package.
#[derive(Clone, Eq, Hash, Ord, PartialEq, PartialOrd)]
struct OutputFile(String);

impl<'a> PackageMap<'a> {
    pub fn build_order(&self) -> PackageDependencyIter<'a> {
        let lookup_by_output = self
            .0
            .iter()
            .map(|(name, package)| (OutputFile(package.get_output_file(name)), (*name, *package)))
            .collect::<BTreeMap<_, _>>();

        // Collect all packages, and sort them in dependency order,
        // so we know which ones to build first.
        let mut outputs = TopologicalSort::<OutputFile>::new();
        for (package_output, (_, package)) in &lookup_by_output {
            match &package.source {
                PackageSource::Local { .. }
                | PackageSource::Prebuilt { .. }
                | PackageSource::Manual => {
                    // Skip intermediate leaf packages; if necessary they'll be
                    // added to the dependency graph by whatever composite package
                    // actually depends on them.
                    if !matches!(
                        package.output,
                        PackageOutput::Zone {
                            intermediate_only: true
                        }
                    ) {
                        outputs.insert(package_output.clone());
                    }
                }
                PackageSource::Composite { packages: deps } => {
                    for dep in deps {
                        outputs.add_dependency(OutputFile(dep.clone()), package_output.clone());
                    }
                }
            }
        }

        PackageDependencyIter {
            lookup_by_output,
            outputs,
        }
    }
}

/// Returns all packages in the order in which they should be built.
///
/// Returns packages in batches that may be built concurrently.
pub struct PackageDependencyIter<'a> {
    lookup_by_output: BTreeMap<OutputFile, (&'a PackageName, &'a Package)>,
    outputs: TopologicalSort<OutputFile>,
}

impl<'a> Iterator for PackageDependencyIter<'a> {
    type Item = Vec<(&'a PackageName, &'a Package)>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.outputs.is_empty() {
            return None;
        }
        let batch = self.outputs.pop_all();
        assert!(
            !batch.is_empty() || self.outputs.is_empty(),
            "cyclic dependency in package manifest!"
        );

        Some(
            batch
                .into_iter()
                .map(|output| {
                    *self.lookup_by_output.get(&output).unwrap_or_else(|| {
                        panic!("Could not find a package which creates '{}'", output.0)
                    })
                })
                .collect(),
        )
    }
}

/// Describes the configuration for a set of packages.
#[derive(Clone, Deserialize, Debug)]
pub struct Config {
    /// Packages to be built and installed.
    #[serde(default, rename = "package")]
    pub packages: BTreeMap<PackageName, Package>,
}

impl Config {
    /// Returns target packages to be assembled on the builder machine.
    pub fn packages_to_build(&self, target: &Target) -> PackageMap<'_> {
        PackageMap(
            self.packages
                .iter()
                .filter(|(_, pkg)| target.includes_package(pkg))
                .collect(),
        )
    }

    /// Returns target packages which should execute on the deployment machine.
    pub fn packages_to_deploy(&self, target: &Target) -> PackageMap<'_> {
        let all_packages = self.packages_to_build(target).0;
        PackageMap(
            all_packages
                .into_iter()
                .filter(|(_, pkg)| match pkg.output {
                    PackageOutput::Zone { intermediate_only } => !intermediate_only,
                    PackageOutput::Tarball => true,
                })
                .collect(),
        )
    }
}

/// Errors which may be returned when parsing the server configuration.
#[derive(Error, Debug)]
pub enum ParseError {
    #[error("Cannot parse toml: {0}")]
    Toml(#[from] toml::de::Error),
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
}

/// Parses a manifest into a package [`Config`].
pub fn parse_manifest(manifest: &str) -> Result<Config, ParseError> {
    let cfg = toml::from_str::<Config>(manifest)?;
    Ok(cfg)
}
/// Parses a path in the filesystem into a package [`Config`].
pub fn parse<P: AsRef<Path>>(path: P) -> Result<Config, ParseError> {
    let contents = std::fs::read_to_string(path.as_ref())?;
    parse_manifest(&contents)
}

#[cfg(test)]
mod test {
    use crate::config::ServiceName;

    use super::*;

    #[test]
    fn test_order() {
        let pkg_a_name = PackageName::new_const("pkg-a");
        let pkg_a = Package {
            service_name: ServiceName::new_const("a"),
            source: PackageSource::Manual,
            output: PackageOutput::Tarball,
            only_for_targets: None,
            setup_hint: None,
        };

        let pkg_b_name = PackageName::new_const("pkg-b");
        let pkg_b = Package {
            service_name: ServiceName::new_const("b"),
            source: PackageSource::Composite {
                packages: vec![pkg_a.get_output_file(&pkg_a_name)],
            },
            output: PackageOutput::Tarball,
            only_for_targets: None,
            setup_hint: None,
        };

        let cfg = Config {
            packages: BTreeMap::from([
                (pkg_a_name.clone(), pkg_a.clone()),
                (pkg_b_name.clone(), pkg_b.clone()),
            ]),
        };

        let mut order = cfg.packages_to_build(&Target::default()).build_order();
        // "pkg-a" comes first, because "pkg-b" depends on it.
        assert_eq!(order.next(), Some(vec![(&pkg_a_name, &pkg_a)]));
        assert_eq!(order.next(), Some(vec![(&pkg_b_name, &pkg_b)]));
    }

    // We're kinda limited by the topological-sort library here, as this is a documented
    // behavior from [TopologicalSort::pop_all].
    //
    // Regardless, test that circular dependencies cause panics.
    #[test]
    #[should_panic(expected = "cyclic dependency in package manifest")]
    fn test_cyclic_dependency() {
        let pkg_a_name = PackageName::new_const("pkg-a");
        let pkg_b_name = PackageName::new_const("pkg-b");
        let pkg_a = Package {
            service_name: ServiceName::new_const("a"),
            source: PackageSource::Composite {
                packages: vec![String::from("pkg-b.tar")],
            },
            output: PackageOutput::Tarball,
            only_for_targets: None,
            setup_hint: None,
        };
        let pkg_b = Package {
            service_name: ServiceName::new_const("b"),
            source: PackageSource::Composite {
                packages: vec![String::from("pkg-a.tar")],
            },
            output: PackageOutput::Tarball,
            only_for_targets: None,
            setup_hint: None,
        };

        let cfg = Config {
            packages: BTreeMap::from([
                (pkg_a_name.clone(), pkg_a.clone()),
                (pkg_b_name.clone(), pkg_b.clone()),
            ]),
        };

        let mut order = cfg.packages_to_build(&Target::default()).build_order();
        order.next();
    }

    // Make pkg-a depend on pkg-b.tar, but don't include pkg-b.tar anywhere.
    //
    // Ensure that we see an appropriate panic.
    #[test]
    #[should_panic(expected = "Could not find a package which creates 'pkg-b.tar'")]
    fn test_missing_dependency() {
        let pkg_a_name = PackageName::new_const("pkg-a");
        let pkg_a = Package {
            service_name: ServiceName::new_const("a"),
            source: PackageSource::Composite {
                packages: vec![String::from("pkg-b.tar")],
            },
            output: PackageOutput::Tarball,
            only_for_targets: None,
            setup_hint: None,
        };

        let cfg = Config {
            packages: BTreeMap::from([(pkg_a_name.clone(), pkg_a.clone())]),
        };

        let mut order = cfg.packages_to_build(&Target::default()).build_order();
        order.next();
    }
}
