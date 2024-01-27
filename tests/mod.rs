// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

#[cfg(test)]
mod test {
    use anyhow::Result;
    use omicron_zone_package::config;
    use omicron_zone_package::target::Target;
    use std::fs::File;
    use std::io::Read;
    use std::path::{Path, PathBuf};
    use tar::Archive;

    use omicron_zone_package::blob::download;
    use omicron_zone_package::progress::NoProgress;

    fn get_next_path<'a, R: 'a + Read>(entries: &mut tar::Entries<'a, R>) -> PathBuf {
        entries
            .next()
            .unwrap()
            .unwrap()
            .path()
            .unwrap()
            .into_owned()
    }

    fn get_next_entry<'a, R: 'a + Read>(entries: &mut tar::Entries<'a, R>) -> tar::Entry<'a, R> {
        entries.next().unwrap().unwrap()
    }

    // Tests a package of arbitrary files is being placed into a Zone image
    #[tokio::test(flavor = "multi_thread")]
    async fn test_package_as_zone() {
        // Parse the configuration
        let cfg = config::parse("tests/service-a/cfg.toml").unwrap();
        let package_name = "my-service";
        let package = cfg.packages.get(package_name).unwrap();

        // Create the packaged file
        let out = tempfile::tempdir().unwrap();
        package
            .create_for_target(&Target::default(), package_name, out.path())
            .await
            .unwrap();

        // Verify the contents
        let path = package.get_output_path(package_name, &out.path());
        assert!(path.exists());
        let gzr = flate2::read::GzDecoder::new(File::open(path).unwrap());
        let mut archive = Archive::new(gzr);
        let mut ents = archive.entries().unwrap();
        assert_eq!(Path::new("oxide.json"), get_next_path(&mut ents));
        assert_eq!(Path::new("root/"), get_next_path(&mut ents));
        assert_eq!(Path::new("root/opt"), get_next_path(&mut ents));
        assert_eq!(Path::new("root/opt/oxide"), get_next_path(&mut ents));
        assert_eq!(
            Path::new("root/opt/oxide/my-service"),
            get_next_path(&mut ents)
        );
        assert_eq!(
            Path::new("root/opt/oxide/my-service/contents.txt"),
            get_next_path(&mut ents)
        );
        assert_eq!(Path::new("root/"), get_next_path(&mut ents));
        assert_eq!(Path::new("root/opt"), get_next_path(&mut ents));
        assert_eq!(Path::new("root/opt/oxide"), get_next_path(&mut ents));
        assert_eq!(
            Path::new("root/opt/oxide/my-service"),
            get_next_path(&mut ents)
        );
        assert_eq!(
            Path::new("root/opt/oxide/my-service/single-file.txt"),
            get_next_path(&mut ents)
        );
        assert!(ents.next().is_none());
    }

    // Tests a rust package being placed into a Zone image
    #[tokio::test(flavor = "multi_thread")]
    async fn test_rust_package_as_zone() {
        // Parse the configuration
        let cfg = config::parse("tests/service-b/cfg.toml").unwrap();
        let package_name = "my-service";
        let package = cfg.packages.get(package_name).unwrap();

        // Create the packaged file
        let out = tempfile::tempdir().unwrap();
        package
            .create_for_target(&Target::default(), package_name, out.path())
            .await
            .unwrap();

        // Verify the contents
        let path = package.get_output_path(package_name, &out.path());
        assert!(path.exists());
        let gzr = flate2::read::GzDecoder::new(File::open(path).unwrap());
        let mut archive = Archive::new(gzr);
        let mut ents = archive.entries().unwrap();
        assert_eq!(Path::new("oxide.json"), get_next_path(&mut ents));
        assert_eq!(Path::new("root/"), get_next_path(&mut ents));
        assert_eq!(Path::new("root/opt"), get_next_path(&mut ents));
        assert_eq!(Path::new("root/opt/oxide"), get_next_path(&mut ents));
        assert_eq!(
            Path::new("root/opt/oxide/my-service"),
            get_next_path(&mut ents)
        );
        assert_eq!(
            Path::new("root/opt/oxide/my-service/contents.txt"),
            get_next_path(&mut ents)
        );
        assert_eq!(Path::new("root/"), get_next_path(&mut ents));
        assert_eq!(Path::new("root/opt"), get_next_path(&mut ents));
        assert_eq!(Path::new("root/opt/oxide"), get_next_path(&mut ents));
        assert_eq!(
            Path::new("root/opt/oxide/my-service"),
            get_next_path(&mut ents)
        );
        assert_eq!(
            Path::new("root/opt/oxide/my-service/bin"),
            get_next_path(&mut ents)
        );
        assert_eq!(
            Path::new("root/opt/oxide/my-service/bin/test-service"),
            get_next_path(&mut ents)
        );
        assert!(ents.next().is_none());
    }

    // Tests a rust package being placed into a non-Zone image.
    //
    // This is used for building packages that exist in the Global Zone,
    // and don't need (nor want) to be packaged into a full Zone image.
    #[tokio::test(flavor = "multi_thread")]
    async fn test_rust_package_as_tarball() {
        // Parse the configuration
        let cfg = config::parse("tests/service-c/cfg.toml").unwrap();
        let package_name = "my-service";
        let package = cfg.packages.get(package_name).unwrap();

        // Create the packaged file
        let out = tempfile::tempdir().unwrap();
        package
            .create_for_target(&Target::default(), package_name, out.path())
            .await
            .unwrap();

        // Verify the contents
        let path = package.get_output_path(package_name, &out.path());
        assert!(path.exists());
        let mut archive = Archive::new(File::open(path).unwrap());
        let mut ents = archive.entries().unwrap();
        let mut entry = get_next_entry(&mut ents);
        assert_eq!(Path::new("VERSION"), entry.path().unwrap());
        let mut s = String::new();
        entry.read_to_string(&mut s).unwrap();
        assert_eq!(s, "0.0.0");

        assert_eq!(Path::new("test-service"), get_next_path(&mut ents));
        assert!(ents.next().is_none());

        // Try stamping it, verify the contents again
        let expected_semver = semver::Version::new(3, 3, 3);
        let path = package
            .stamp(package_name, &out.path(), &expected_semver)
            .await
            .unwrap();
        assert!(path.exists());
        let mut archive = Archive::new(File::open(path).unwrap());
        let mut ents = archive.entries().unwrap();
        assert_eq!(Path::new("./"), get_next_path(&mut ents));
        assert_eq!(Path::new("test-service"), get_next_path(&mut ents));
        let mut entry = get_next_entry(&mut ents);
        assert_eq!(Path::new("VERSION"), entry.path().unwrap());
        s.clear();
        entry.read_to_string(&mut s).unwrap();
        assert_eq!(s, expected_semver.to_string());

        assert!(ents.next().is_none());
    }

    // Although package and service names are often the same, they do
    // not *need* to be the same. This is an example of them both
    // being explicitly different.
    #[tokio::test(flavor = "multi_thread")]
    async fn test_rust_package_with_distinct_service_name() {
        // Parse the configuration
        let cfg = config::parse("tests/service-d/cfg.toml").unwrap();
        let package_name = "my-package";
        let service_name = "my-service";
        let package = cfg.packages.get(package_name).unwrap();

        assert_eq!(package.service_name, service_name);

        // Create the packaged file
        let out = tempfile::tempdir().unwrap();
        package
            .create_for_target(&Target::default(), package_name, out.path())
            .await
            .unwrap();

        // Verify the contents
        let path = package.get_output_path(package_name, &out.path());
        assert!(path.exists());
        let mut archive = Archive::new(File::open(path).unwrap());
        let mut ents = archive.entries().unwrap();
        assert_eq!(Path::new("VERSION"), get_next_path(&mut ents));
        assert_eq!(Path::new("test-service"), get_next_path(&mut ents));
        assert!(ents.next().is_none());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_composite_package() {
        // Parse the configuration
        let cfg = config::parse("tests/service-e/cfg.toml").unwrap();
        let out = tempfile::tempdir().unwrap();

        // Ask for the order of packages to-be-built
        let packages = cfg.packages_to_build(&Target::default());
        let mut build_order = packages.build_order();

        // Build the dependencies first.
        let batch = build_order.next().expect("Missing dependency batch");
        let mut batch_pkg_names: Vec<_> = batch.iter().map(|(name, _)| *name).collect();
        batch_pkg_names.sort();
        assert_eq!(batch_pkg_names, vec!["pkg-1", "pkg-2"]);
        for (package_name, package) in batch {
            // Create the packaged file
            package
                .create_for_target(&Target::default(), package_name, out.path())
                .await
                .unwrap();
        }

        // Build the composite package
        let batch = build_order.next().expect("Missing dependency batch");
        let batch_pkg_names: Vec<_> = batch.iter().map(|(name, _)| *name).collect();
        let package_name = "pkg-3";
        assert_eq!(batch_pkg_names, vec![package_name]);
        let package = cfg.packages.get(package_name).unwrap();
        package
            .create_for_target(&Target::default(), package_name, out.path())
            .await
            .unwrap();

        // Verify the contents
        let path = package.get_output_path(package_name, &out.path());
        assert!(path.exists());
        let gzr = flate2::read::GzDecoder::new(File::open(path).unwrap());
        let mut archive = Archive::new(gzr);
        let mut ents = archive.entries().unwrap();
        assert_eq!(Path::new("oxide.json"), get_next_path(&mut ents));
        assert_eq!(Path::new("root/"), get_next_path(&mut ents));
        assert_eq!(Path::new("root/opt"), get_next_path(&mut ents));
        assert_eq!(Path::new("root/opt/oxide"), get_next_path(&mut ents));
        assert_eq!(
            Path::new("root/opt/oxide/pkg-1-file.txt"),
            get_next_path(&mut ents)
        );
        assert_eq!(Path::new("root/"), get_next_path(&mut ents));
        assert_eq!(Path::new("root/opt"), get_next_path(&mut ents));
        assert_eq!(Path::new("root/opt/oxide"), get_next_path(&mut ents));
        assert_eq!(
            Path::new("root/opt/oxide/pkg-2-file.txt"),
            get_next_path(&mut ents)
        );
        assert_eq!(Path::new("root/"), get_next_path(&mut ents));
        assert_eq!(Path::new("root/opt"), get_next_path(&mut ents));
        assert_eq!(Path::new("root/opt/oxide"), get_next_path(&mut ents));
        assert_eq!(Path::new("root/opt/oxide/svc-2"), get_next_path(&mut ents));
        assert_eq!(
            Path::new("root/opt/oxide/svc-2/bin"),
            get_next_path(&mut ents)
        );
        assert_eq!(
            Path::new("root/opt/oxide/svc-2/bin/test-service"),
            get_next_path(&mut ents)
        );
        assert!(ents.next().is_none());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_download() -> Result<()> {
        let out = tempfile::tempdir()?;

        let path = PathBuf::from("OVMF_CODE.fd");
        let src = omicron_zone_package::blob::Source::S3(path.clone());
        let dst = out.path().join(&path);

        download(&NoProgress::new(), &src, &dst).await?;
        download(&NoProgress::new(), &src, &dst).await?;

        Ok(())
    }
}
