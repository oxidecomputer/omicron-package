// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

#[cfg(test)]
mod test {
    use anyhow::Result;
    use omicron_zone_package::config;
    use std::fs::File;
    use std::io::Read;
    use std::path::{Path, PathBuf};
    use tar::Archive;

    use omicron_zone_package::blob::download;
    use omicron_zone_package::progress::NoProgress;

    fn get_next<'a, R: 'a + Read>(entries: &mut tar::Entries<'a, R>) -> PathBuf {
        entries
            .next()
            .unwrap()
            .unwrap()
            .path()
            .unwrap()
            .into_owned()
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
        package.create(package_name, out.path()).await.unwrap();

        // Verify the contents
        let path = package.get_output_path(package_name, &out.path());
        assert!(path.exists());
        let gzr = flate2::read::GzDecoder::new(File::open(path).unwrap());
        let mut archive = Archive::new(gzr);
        let mut ents = archive.entries().unwrap();
        assert_eq!(Path::new("oxide.json"), get_next(&mut ents));
        assert_eq!(Path::new("root/"), get_next(&mut ents));
        assert_eq!(Path::new("root/opt"), get_next(&mut ents));
        assert_eq!(Path::new("root/opt/oxide"), get_next(&mut ents));
        assert_eq!(Path::new("root/opt/oxide/my-service"), get_next(&mut ents));
        assert_eq!(
            Path::new("root/opt/oxide/my-service/contents.txt"),
            get_next(&mut ents)
        );
        assert_eq!(Path::new("root/"), get_next(&mut ents));
        assert_eq!(Path::new("root/opt"), get_next(&mut ents));
        assert_eq!(Path::new("root/opt/oxide"), get_next(&mut ents));
        assert_eq!(Path::new("root/opt/oxide/my-service"), get_next(&mut ents));
        assert_eq!(
            Path::new("root/opt/oxide/my-service/single-file.txt"),
            get_next(&mut ents)
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
        package.create(package_name, out.path()).await.unwrap();

        // Verify the contents
        let path = package.get_output_path(package_name, &out.path());
        assert!(path.exists());
        let gzr = flate2::read::GzDecoder::new(File::open(path).unwrap());
        let mut archive = Archive::new(gzr);
        let mut ents = archive.entries().unwrap();
        assert_eq!(Path::new("oxide.json"), get_next(&mut ents));
        assert_eq!(Path::new("root/"), get_next(&mut ents));
        assert_eq!(Path::new("root/opt"), get_next(&mut ents));
        assert_eq!(Path::new("root/opt/oxide"), get_next(&mut ents));
        assert_eq!(Path::new("root/opt/oxide/my-service"), get_next(&mut ents));
        assert_eq!(
            Path::new("root/opt/oxide/my-service/contents.txt"),
            get_next(&mut ents)
        );
        assert_eq!(Path::new("root/"), get_next(&mut ents));
        assert_eq!(Path::new("root/opt"), get_next(&mut ents));
        assert_eq!(Path::new("root/opt/oxide"), get_next(&mut ents));
        assert_eq!(Path::new("root/opt/oxide/my-service"), get_next(&mut ents));
        assert_eq!(
            Path::new("root/opt/oxide/my-service/bin"),
            get_next(&mut ents)
        );
        assert_eq!(
            Path::new("root/opt/oxide/my-service/bin/test-service"),
            get_next(&mut ents)
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
        package.create(package_name, out.path()).await.unwrap();

        // Verify the contents
        let path = package.get_output_path(package_name, &out.path());
        assert!(path.exists());
        let mut archive = Archive::new(File::open(path).unwrap());
        let mut ents = archive.entries().unwrap();
        assert_eq!(Path::new("test-service"), get_next(&mut ents));
        assert!(ents.next().is_none());
    }

    // Although package and service names are often the same, they do
    // not *need* to be the same. This is an example of them both
    // being explicitly different.
    #[tokio::test(flavor = "multi_thread")]
    async fn test_rust_package_with_disinct_service_name() {
        // Parse the configuration
        let cfg = config::parse("tests/service-d/cfg.toml").unwrap();
        let package_name = "my-package";
        let service_name = "my-service";
        let package = cfg.packages.get(package_name).unwrap();

        assert_eq!(package.service_name, service_name);

        // Create the packaged file
        let out = tempfile::tempdir().unwrap();
        package.create(package_name, out.path()).await.unwrap();

        // Verify the contents
        let path = package.get_output_path(package_name, &out.path());
        assert!(path.exists());
        let mut archive = Archive::new(File::open(path).unwrap());
        let mut ents = archive.entries().unwrap();
        assert_eq!(Path::new("test-service"), get_next(&mut ents));
        assert!(ents.next().is_none());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_composite_package() {
        // Parse the configuration
        let cfg = config::parse("tests/service-e/cfg.toml").unwrap();
        let out = tempfile::tempdir().unwrap();

        // Build the dependencies first.
        let package_dependencies = ["pkg-1", "pkg-2"];
        for package_name in package_dependencies {
            let package = cfg.packages.get(package_name).unwrap();
            // Create the packaged file
            package.create(package_name, out.path()).await.unwrap();
        }

        // Build the composite package
        let package_name = "pkg-3";
        let package = cfg.packages.get(package_name).unwrap();
        package.create(package_name, out.path()).await.unwrap();

        // Verify the contents
        let path = package.get_output_path(package_name, &out.path());
        assert!(path.exists());
        let gzr = flate2::read::GzDecoder::new(File::open(path).unwrap());
        let mut archive = Archive::new(gzr);
        let mut ents = archive.entries().unwrap();
        assert_eq!(Path::new("oxide.json"), get_next(&mut ents));
        assert_eq!(Path::new("root/"), get_next(&mut ents));
        assert_eq!(Path::new("root/opt"), get_next(&mut ents));
        assert_eq!(Path::new("root/opt/oxide"), get_next(&mut ents));
        assert_eq!(
            Path::new("root/opt/oxide/pkg-1-file.txt"),
            get_next(&mut ents)
        );
        assert_eq!(Path::new("root/"), get_next(&mut ents));
        assert_eq!(Path::new("root/opt"), get_next(&mut ents));
        assert_eq!(Path::new("root/opt/oxide"), get_next(&mut ents));
        assert_eq!(
            Path::new("root/opt/oxide/pkg-2-file.txt"),
            get_next(&mut ents)
        );
        assert_eq!(Path::new("root/"), get_next(&mut ents));
        assert_eq!(Path::new("root/opt"), get_next(&mut ents));
        assert_eq!(Path::new("root/opt/oxide"), get_next(&mut ents));
        assert_eq!(Path::new("root/opt/oxide/svc-2"), get_next(&mut ents));
        assert_eq!(Path::new("root/opt/oxide/svc-2/bin"), get_next(&mut ents));
        assert_eq!(
            Path::new("root/opt/oxide/svc-2/bin/test-service"),
            get_next(&mut ents)
        );
        assert!(ents.next().is_none());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_download() -> Result<()> {
        let out = tempfile::tempdir()?;

        let url = "OVMF_CODE.fd";
        let dst = out.path().join(url);

        download(&NoProgress, &url, &dst).await?;
        download(&NoProgress, &url, &dst).await?;

        Ok(())
    }
}
