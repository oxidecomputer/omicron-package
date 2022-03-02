// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

#[cfg(test)]
mod test {
    use omicron_package::config;
    use std::io::Read;
    use std::fs::File;
    use std::path::{Path, PathBuf};
    use tar::Archive;

    fn get_next_path<'a, R: 'a + Read>(entries: &mut tar::Entries<'a, R>) -> PathBuf {
        entries.next().unwrap().unwrap().path().unwrap().into_owned()
    }

    // Tests a package of arbitrary files is being placed into a Zone image
    #[tokio::test]
    async fn test_package_as_zone() {
        // Parse the configuration
        let cfg = config::parse("tests/service-a/cfg.toml").unwrap();
        let package = cfg.packages.get("my-service").unwrap();

        // Create the packaged file
        let out = tempfile::tempdir().unwrap();
        package.create(out.path()).await.unwrap();

        // Verify the contents
        let path = package.get_output_path(&out.path());
        assert!(path.exists());
        let gzr = flate2::read::GzDecoder::new(File::open(path).unwrap());
        let mut archive = Archive::new(gzr);
        let mut entries = archive.entries().unwrap();
        assert_eq!(Path::new("oxide.json"), get_next_path(&mut entries));
        assert_eq!(Path::new("root/"), get_next_path(&mut entries));
        assert_eq!(Path::new("root/opt"), get_next_path(&mut entries));
        assert_eq!(Path::new("root/opt/oxide"), get_next_path(&mut entries));
        assert_eq!(Path::new("root/opt/oxide/my-service"), get_next_path(&mut entries));
        assert_eq!(Path::new("root/opt/oxide/my-service/contents.txt"), get_next_path(&mut entries));
        assert!(entries.next().is_none());
    }

    // Tests a rust package being placed into a Zone image
    #[tokio::test]
    async fn test_rust_package_as_zone() {
        // Parse the configuration
        let cfg = config::parse("tests/service-b/cfg.toml").unwrap();
        let package = cfg.packages.get("my-service").unwrap();

        // Create the packaged file
        let out = tempfile::tempdir().unwrap();
        package.create(out.path()).await.unwrap();

        // Verify the contents
        let path = package.get_output_path(&out.path());
        assert!(path.exists());
        let gzr = flate2::read::GzDecoder::new(File::open(path).unwrap());
        let mut archive = Archive::new(gzr);
        let mut entries = archive.entries().unwrap();
        assert_eq!(Path::new("oxide.json"), get_next_path(&mut entries));
        assert_eq!(Path::new("root/"), get_next_path(&mut entries));
        assert_eq!(Path::new("root/opt"), get_next_path(&mut entries));
        assert_eq!(Path::new("root/opt/oxide"), get_next_path(&mut entries));
        assert_eq!(Path::new("root/opt/oxide/my-service"), get_next_path(&mut entries));
        assert_eq!(Path::new("root/opt/oxide/my-service/contents.txt"), get_next_path(&mut entries));
        assert_eq!(Path::new("root/"), get_next_path(&mut entries));
        assert_eq!(Path::new("root/opt"), get_next_path(&mut entries));
        assert_eq!(Path::new("root/opt/oxide"), get_next_path(&mut entries));
        assert_eq!(Path::new("root/opt/oxide/my-service"), get_next_path(&mut entries));
        assert_eq!(Path::new("root/opt/oxide/my-service/bin"), get_next_path(&mut entries));
        assert_eq!(Path::new("root/opt/oxide/my-service/bin/test-service"), get_next_path(&mut entries));
        assert!(entries.next().is_none());
    }

    // Tests a rust package being placed into a non-Zone image.
    //
    // This is used for building packages that exist in the Global Zone,
    // and don't need (nor want) to be packaged into a full Zone image.
    #[tokio::test]
    async fn test_rust_package_as_tarball() {
        // Parse the configuration
        let cfg = config::parse("tests/service-c/cfg.toml").unwrap();
        let package = cfg.packages.get("my-service").unwrap();

        // Create the packaged file
        let out = tempfile::tempdir().unwrap();
        package.create(out.path()).await.unwrap();

        // Verify the contents
        let path = package.get_output_path(&out.path());
        assert!(path.exists());
        let mut archive = Archive::new(File::open(path).unwrap());
        let mut entries = archive.entries().unwrap();
        assert_eq!(Path::new("test-service"), get_next_path(&mut entries));
        assert!(entries.next().is_none());
    }
}
