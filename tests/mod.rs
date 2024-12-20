// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

#[cfg(test)]
mod test {
    use anyhow::Result;
    use camino::Utf8PathBuf;
    use std::convert::TryInto;
    use std::fs::File;
    use std::io::Read;
    use tar::Archive;

    use omicron_zone_package::blob::download;
    use omicron_zone_package::config::{self, PackageName, ServiceName};
    use omicron_zone_package::package::BuildConfig;
    use omicron_zone_package::progress::NoProgress;
    use omicron_zone_package::target::Target;

    const MY_PACKAGE: PackageName = PackageName::new_const("my-package");

    /// The package name called the same as the service name.
    const MY_SERVICE_PACKAGE: PackageName = PackageName::new_const("my-service");
    const MY_SERVICE: ServiceName = ServiceName::new_const("my-service");

    fn entry_path<'a, R>(entry: &tar::Entry<'a, R>) -> Utf8PathBuf
    where
        R: 'a + Read,
    {
        entry
            .path()
            .expect("Failed to access path")
            .into_owned()
            .try_into()
            .expect("Invalid UTF-8")
    }

    trait EasyIteratorAccess {
        type Entry;

        fn next_entry(&mut self) -> Self::Entry;
        fn next_path(&mut self) -> Utf8PathBuf;
    }

    impl<'a, R> EasyIteratorAccess for tar::Entries<'a, R>
    where
        R: 'a + Read,
    {
        type Entry = tar::Entry<'a, R>;

        fn next_entry(&mut self) -> Self::Entry {
            self.next()
                .expect("No additional entries in iterator")
                .expect("I/O error accessing next entry")
        }

        fn next_path(&mut self) -> Utf8PathBuf {
            entry_path(&self.next_entry())
        }
    }

    // Tests a package of arbitrary files is being placed into a Zone image
    #[tokio::test(flavor = "multi_thread")]
    async fn test_package_as_zone() {
        // Parse the configuration
        let cfg = config::parse("tests/service-a/cfg.toml").unwrap();
        let package = cfg.packages.get(&MY_SERVICE_PACKAGE).unwrap();

        // Create the packaged file
        let out = camino_tempfile::tempdir().unwrap();
        let build_config = BuildConfig::default();
        package
            .create(&MY_SERVICE_PACKAGE, out.path(), &build_config)
            .await
            .unwrap();

        // Verify the contents
        let path = package.get_output_path(&MY_SERVICE_PACKAGE, out.path());
        assert!(path.exists());
        let gzr = flate2::read::GzDecoder::new(File::open(path).unwrap());
        let mut archive = Archive::new(gzr);
        let mut ents = archive.entries().unwrap();
        assert_eq!("oxide.json", ents.next_path());
        assert_eq!("root/", ents.next_path());
        assert_eq!("root/opt", ents.next_path());
        assert_eq!("root/opt/oxide", ents.next_path());
        assert_eq!("root/opt/oxide/my-service", ents.next_path());
        assert_eq!("root/opt/oxide/my-service/contents.txt", ents.next_path());
        assert_eq!("root/", ents.next_path());
        assert_eq!("root/opt", ents.next_path());
        assert_eq!("root/opt/oxide", ents.next_path());
        assert_eq!("root/opt/oxide/my-service", ents.next_path());
        assert_eq!(
            "root/opt/oxide/my-service/single-file.txt",
            ents.next_path()
        );
        assert!(ents.next().is_none());
    }

    // Tests a rust package being placed into a Zone image
    #[tokio::test(flavor = "multi_thread")]
    async fn test_rust_package_as_zone() {
        // Parse the configuration
        let cfg = config::parse("tests/service-b/cfg.toml").unwrap();
        let package = cfg.packages.get(&MY_SERVICE_PACKAGE).unwrap();

        // Create the packaged file
        let out = camino_tempfile::tempdir().unwrap();
        let build_config = BuildConfig::default();
        package
            .create(&MY_SERVICE_PACKAGE, out.path(), &build_config)
            .await
            .unwrap();

        // Verify the contents
        let path = package.get_output_path(&MY_SERVICE_PACKAGE, out.path());
        assert!(path.exists());
        let gzr = flate2::read::GzDecoder::new(File::open(path).unwrap());
        let mut archive = Archive::new(gzr);
        let mut ents = archive.entries().unwrap();
        assert_eq!("oxide.json", ents.next_path());
        assert_eq!("root/", ents.next_path());
        assert_eq!("root/opt", ents.next_path());
        assert_eq!("root/opt/oxide", ents.next_path());
        assert_eq!("root/opt/oxide/my-service", ents.next_path());
        assert_eq!("root/opt/oxide/my-service/contents.txt", ents.next_path());
        assert_eq!("root/", ents.next_path());
        assert_eq!("root/opt", ents.next_path());
        assert_eq!("root/opt/oxide", ents.next_path());
        assert_eq!("root/opt/oxide/my-service", ents.next_path());
        assert_eq!("root/opt/oxide/my-service/bin", ents.next_path());
        assert_eq!(
            "root/opt/oxide/my-service/bin/test-service",
            ents.next_path()
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
        let package = cfg.packages.get(&MY_SERVICE_PACKAGE).unwrap();

        // Create the packaged file
        let out = camino_tempfile::tempdir().unwrap();
        let build_config = BuildConfig::default();
        package
            .create(&MY_SERVICE_PACKAGE, out.path(), &build_config)
            .await
            .unwrap();

        // Verify the contents
        let path = package.get_output_path(&MY_SERVICE_PACKAGE, out.path());
        assert!(path.exists());
        let mut archive = Archive::new(File::open(path).unwrap());
        let mut ents = archive.entries().unwrap();
        let mut entry = ents.next_entry();
        assert_eq!("VERSION", entry_path(&entry));
        let mut s = String::new();
        entry.read_to_string(&mut s).unwrap();
        assert_eq!(s, "0.0.0");

        assert_eq!("test-service", ents.next_path());
        assert!(ents.next().is_none());

        // Try stamping it, verify the contents again
        let expected_semver = semver::Version::new(3, 3, 3);
        let path = package
            .stamp(&MY_SERVICE_PACKAGE, out.path(), &expected_semver)
            .await
            .unwrap();
        assert!(path.exists());
        let mut archive = Archive::new(File::open(path).unwrap());
        let mut ents = archive.entries().unwrap();
        assert_eq!("./", ents.next_path());
        assert_eq!("test-service", ents.next_path());
        let mut entry = ents.next_entry();
        assert_eq!("VERSION", entry_path(&entry));
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
        let package = cfg.packages.get(&MY_PACKAGE).unwrap();

        assert_eq!(package.service_name, MY_SERVICE);

        // Create the packaged file
        let out = camino_tempfile::tempdir().unwrap();
        let build_config = BuildConfig::default();
        package
            .create(&MY_PACKAGE, out.path(), &build_config)
            .await
            .unwrap();

        // Verify the contents
        let path = package.get_output_path(&MY_PACKAGE, out.path());
        assert!(path.exists());
        let mut archive = Archive::new(File::open(path).unwrap());
        let mut ents = archive.entries().unwrap();
        assert_eq!("VERSION", ents.next_path());
        assert_eq!("test-service", ents.next_path());
        assert!(ents.next().is_none());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_composite_package() {
        // Parse the configuration
        let cfg = config::parse("tests/service-e/cfg.toml").unwrap();
        let out = camino_tempfile::tempdir().unwrap();

        // Ask for the order of packages to-be-built
        let packages = cfg.packages_to_build(&Target::default());
        let mut build_order = packages.build_order();

        // Build the dependencies first.
        let batch = build_order.next().expect("Missing dependency batch");
        let mut batch_pkg_names: Vec<_> = batch.iter().map(|(name, _)| *name).collect();
        batch_pkg_names.sort();
        assert_eq!(
            batch_pkg_names,
            vec![
                &PackageName::new_const("pkg-1"),
                &PackageName::new_const("pkg-2"),
            ]
        );
        let build_config = BuildConfig::default();
        for (package_name, package) in batch {
            // Create the packaged file
            package
                .create(package_name, out.path(), &build_config)
                .await
                .unwrap();
        }

        // Build the composite package
        let batch = build_order.next().expect("Missing dependency batch");
        let batch_pkg_names: Vec<_> = batch.iter().map(|(name, _)| *name).collect();
        let package_name = PackageName::new_const("pkg-3");
        assert_eq!(batch_pkg_names, vec![&package_name]);
        let package = cfg.packages.get(&package_name).unwrap();
        let build_config = BuildConfig::default();
        package
            .create(&package_name, out.path(), &build_config)
            .await
            .unwrap();

        // Verify the contents
        let path = package.get_output_path(&package_name, out.path());
        assert!(path.exists());
        let gzr = flate2::read::GzDecoder::new(File::open(path).unwrap());
        let mut archive = Archive::new(gzr);
        let mut ents = archive.entries().unwrap();
        assert_eq!("oxide.json", ents.next_path());
        assert_eq!("root/", ents.next_path());
        assert_eq!("root/opt", ents.next_path());
        assert_eq!("root/opt/oxide", ents.next_path());
        assert_eq!("root/opt/oxide/pkg-1-file.txt", ents.next_path());
        assert_eq!("root/", ents.next_path());
        assert_eq!("root/opt", ents.next_path());
        assert_eq!("root/opt/oxide", ents.next_path());
        assert_eq!("root/opt/oxide/pkg-2-file.txt", ents.next_path());
        assert_eq!("root/", ents.next_path());
        assert_eq!("root/opt", ents.next_path());
        assert_eq!("root/opt/oxide", ents.next_path());
        assert_eq!("root/opt/oxide/svc-2", ents.next_path());
        assert_eq!("root/opt/oxide/svc-2/bin", ents.next_path());
        assert_eq!("root/opt/oxide/svc-2/bin/test-service", ents.next_path());
        assert!(ents.next().is_none());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_download() -> Result<()> {
        let out = camino_tempfile::tempdir()?;

        let path = Utf8PathBuf::from("OVMF_CODE.fd");
        let src = omicron_zone_package::blob::Source::S3(path.clone());
        let dst = out.path().join(&path);

        download(&NoProgress::new(), &src, &dst).await?;
        download(&NoProgress::new(), &src, &dst).await?;

        Ok(())
    }
}
