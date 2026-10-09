//! Plugin Installer
//!
//! Places plugin packages into the local plugins directory, and uninstalls
//! them. Packages themselves come from the marketplace catalog, which hands
//! over an already-downloaded-and-verified package root (see
//! `src/mcp/connectors`), or from the CLI's local install.

use std::path::{Path, PathBuf};

use tracing::info;

/// Installs and uninstalls plugins in the local plugins directory.
pub struct PluginInstaller {
    plugins_dir: PathBuf,
}

impl PluginInstaller {
    pub fn new(plugins_dir: PathBuf) -> Self {
        Self { plugins_dir }
    }

    /// Copy a plugin package into `plugins_dir/<name>` and return the
    /// destination.
    ///
    /// The copy lands in a staging sibling first and is renamed over the
    /// destination, so a failed copy never leaves an installed plugin
    /// truncated — the previous one stays until the new one is complete.
    pub async fn stage_directory(&self, src: &Path, name: &str) -> crate::Result<PathBuf> {
        // The name becomes a directory component; the catalog's own id rules
        // are not this module's to trust.
        if name.is_empty() || name == "." || name == ".." || name.contains(['/', '\\']) {
            return Err(crate::error::SyscityError::Validation(format!(
                "'{name}' is not a usable plugin directory name"
            )));
        }

        let dest = self.plugins_dir.join(name);
        let staging = self.plugins_dir.join(format!(".staging-{name}"));
        tokio::fs::create_dir_all(&self.plugins_dir).await?;
        if staging.exists() {
            tokio::fs::remove_dir_all(&staging).await?;
        }
        copy_dir_all(src, &staging).await?;

        if dest.exists() {
            tokio::fs::remove_dir_all(&dest).await?;
        }
        tokio::fs::rename(&staging, &dest).await?;
        info!("Plugin package staged at {:?}", dest);
        Ok(dest)
    }

    /// Extract an archive into the target directory.
    ///
    /// Dispatches on the file extension: `.tar.gz`/`.tgz` via
    /// [`extract_tar_gz`](extract_tar_gz), `.zip` via
    /// [`extract_zip`](extract_zip). Shared with `src/mcp/connectors` for
    /// connector package installs.
    pub(crate) async fn extract_archive(
        archive_path: &std::path::Path,
        target_dir: &std::path::Path,
    ) -> crate::Result<()> {
        let name = archive_path.file_name().and_then(|n| n.to_str());
        match name.map(|n| n.to_ascii_lowercase()) {
            Some(n) if n.ends_with(".zip") => extract_zip(archive_path, target_dir).await,
            _ => Self::extract_tar_gz(archive_path, target_dir).await,
        }
    }

    /// Extract a .tar.gz archive into the target directory.
    pub(crate) async fn extract_tar_gz(
        archive_path: &std::path::Path,
        target_dir: &std::path::Path,
    ) -> crate::Result<()> {
        let archive_bytes = tokio::fs::read(archive_path).await?;

        // Decode gzip and unpack tar in a blocking task (I/O heavy)
        tokio::task::spawn_blocking({
            let target = target_dir.to_path_buf();
            move || -> crate::Result<()> {
                let decoder =
                    flate2::read::GzDecoder::new(std::io::BufReader::new(&archive_bytes[..]));
                let mut archive = tar::Archive::new(decoder);

                // Unpack each entry, sanitising paths to prevent zip-slip
                for entry in archive.entries().map_err(|e| {
                    crate::error::SyscityError::Internal(format!(
                        "Failed to read tar entries: {}",
                        e
                    ))
                })? {
                    let mut entry = entry.map_err(|e| {
                        crate::error::SyscityError::Internal(format!(
                            "Failed to read tar entry: {}",
                            e
                        ))
                    })?;

                    // Path sanitisation: reject entries with absolute paths or
                    // parent-directory traversal.
                    let raw_path = entry.path().map_err(|e| {
                        crate::error::SyscityError::Internal(format!(
                            "Failed to get tar entry path: {}",
                            e
                        ))
                    })?;
                    sanitize_archive_path(&raw_path)?;
                    let raw_path_clone = raw_path.to_path_buf();
                    entry.unpack_in(&target).map_err(|e| {
                        crate::error::SyscityError::Internal(format!(
                            "Failed to extract tar entry '{}': {}",
                            raw_path_clone.display(),
                            e
                        ))
                    })?;
                }

                info!("Extracted archive to {:?}", target);
                Ok(())
            }
        })
        .await
        .map_err(|e| {
            crate::error::SyscityError::Internal(format!("Extraction task failed: {}", e))
        })?
    }

    /// Remove an installed plugin by name.
    pub async fn uninstall(&self, name: &str) -> crate::Result<()> {
        let plugin_dir = self.plugins_dir.join(name);
        if !plugin_dir.exists() {
            return Err(crate::error::SyscityError::Internal(format!(
                "Plugin '{}' not found at {:?}",
                name, plugin_dir
            )));
        }
        tokio::fs::remove_dir_all(&plugin_dir).await?;
        info!("Plugin '{}' uninstalled", name);
        Ok(())
    }
}

/// Reject archive entry paths that escape the extraction root.
///
/// Shared by the tar and zip extractors: absolute paths, parent-directory
/// traversal, and Windows prefixes are all zip-slip vectors.
pub(crate) fn sanitize_archive_path(raw: &std::path::Path) -> crate::Result<()> {
    let components: Vec<_> = raw.components().collect();
    if components.iter().any(|c| {
        matches!(
            c,
            std::path::Component::ParentDir
                | std::path::Component::RootDir
                | std::path::Component::Prefix(_)
        )
    }) || raw.is_absolute()
    {
        return Err(crate::error::SyscityError::Internal(format!(
            "Zip-slip detected: archive entry '{}' contains unsafe path components",
            raw.display()
        )));
    }
    Ok(())
}

/// Extract a .zip archive into the target directory (zip-slip sanitized).
pub(crate) async fn extract_zip(
    archive_path: &std::path::Path,
    target_dir: &std::path::Path,
) -> crate::Result<()> {
    let archive_bytes = tokio::fs::read(archive_path).await?;
    let target = target_dir.to_path_buf();
    tokio::task::spawn_blocking(move || -> crate::Result<()> {
        let reader = std::io::Cursor::new(&archive_bytes[..]);
        let mut archive = zip::ZipArchive::new(reader).map_err(|e| {
            crate::error::SyscityError::Internal(format!("Failed to open zip archive: {e}"))
        })?;
        for i in 0..archive.len() {
            let mut entry = archive.by_index(i).map_err(|e| {
                crate::error::SyscityError::Internal(format!("Failed to read zip entry {i}: {e}"))
            })?;
            let Some(name) = entry.enclosed_name() else {
                // enclosed_name() already filters traversal; treat as unsafe.
                return Err(crate::error::SyscityError::Internal(format!(
                    "Zip-slip detected: zip entry '{i}' has an unsafe path"
                )));
            };
            sanitize_archive_path(&name)?;

            let out_path = target.join(name);
            if entry.is_dir() {
                std::fs::create_dir_all(&out_path).map_err(|e| {
                    crate::error::SyscityError::IoContext {
                        context: format!("Failed to create {}", out_path.display()),
                        source: e,
                    }
                })?;
            } else {
                if let Some(parent) = out_path.parent() {
                    std::fs::create_dir_all(parent).map_err(|e| {
                        crate::error::SyscityError::IoContext {
                            context: format!("Failed to create {}", parent.display()),
                            source: e,
                        }
                    })?;
                }
                let mut out = std::fs::File::create(&out_path).map_err(|e| {
                    crate::error::SyscityError::IoContext {
                        context: format!("Failed to create {}", out_path.display()),
                        source: e,
                    }
                })?;
                std::io::copy(&mut entry, &mut out).map_err(|e| {
                    crate::error::SyscityError::IoContext {
                        context: format!("Failed to write {}", out_path.display()),
                        source: e,
                    }
                })?;
            }
        }
        info!("Extracted zip archive to {:?}", target);
        Ok(())
    })
    .await
    .map_err(|e| crate::error::SyscityError::Internal(format!("Extraction task failed: {}", e)))?
}

/// Check whether an extracted package directory contains a `connector.json`
/// directly or inside a single top-level wrapper directory (common when a
/// tarball is built from a folder). Returns the effective package root.
pub(crate) fn locate_package_root(extract_dir: &std::path::Path, marker: &str) -> PathBuf {
    if extract_dir.join(marker).exists() {
        return extract_dir.to_path_buf();
    }
    if let Ok(entries) = std::fs::read_dir(extract_dir) {
        let dirs: Vec<_> = entries.flatten().filter(|e| e.path().is_dir()).collect();
        if dirs.len() == 1 && dirs[0].path().join(marker).exists() {
            return dirs[0].path();
        }
    }
    extract_dir.to_path_buf()
}

/// Copy a directory tree recursively.
///
/// The repository has several private copies of this (connectors, skills,
/// computer, two CLI modules); this one exists so the plugin install path does
/// not add another, and so the marketplace and local-install paths share one
/// implementation.
pub(crate) async fn copy_dir_all(src: &Path, dst: &Path) -> crate::Result<()> {
    tokio::fs::create_dir_all(dst).await?;
    let mut entries = tokio::fs::read_dir(src).await?;
    while let Some(entry) = entries.next_entry().await? {
        let from = entry.path();
        let to = dst.join(entry.file_name());
        if from.is_dir() {
            // Boxed: the recursion has to be nameable to be a future.
            Box::pin(copy_dir_all(&from, &to)).await?;
        } else {
            tokio::fs::copy(&from, &to).await?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::io::Write;

    use tempfile::tempdir;

    use super::*;

    #[test]
    fn test_new_installer() {
        let tmp = tempdir().unwrap();
        let installer = PluginInstaller::new(tmp.path().to_path_buf());
        assert_eq!(installer.plugins_dir, tmp.path());
    }

    #[tokio::test]
    async fn stage_directory_places_a_package_under_its_name() {
        let plugins = tempdir().unwrap();
        let src = tempdir().unwrap();
        std::fs::create_dir_all(src.path().join("nested")).unwrap();
        std::fs::write(src.path().join("plugin.json"), "{}").unwrap();
        std::fs::write(src.path().join("nested/data.txt"), "x").unwrap();

        let installer = PluginInstaller::new(plugins.path().to_path_buf());
        let dest = installer.stage_directory(src.path(), "demo").await.unwrap();

        assert_eq!(dest, plugins.path().join("demo"));
        assert!(dest.join("plugin.json").exists());
        assert!(dest.join("nested/data.txt").exists(), "copy must recurse");
        assert!(
            !plugins.path().join(".staging-demo").exists(),
            "the staging directory must not survive the rename"
        );
    }

    #[tokio::test]
    async fn stage_directory_replaces_an_installed_package() {
        let plugins = tempdir().unwrap();
        let src = tempdir().unwrap();
        std::fs::write(src.path().join("plugin.json"), "new").unwrap();

        let installer = PluginInstaller::new(plugins.path().to_path_buf());
        let first = tempdir().unwrap();
        std::fs::write(first.path().join("plugin.json"), "old").unwrap();
        std::fs::write(first.path().join("stale.txt"), "gone").unwrap();
        installer
            .stage_directory(first.path(), "demo")
            .await
            .unwrap();

        installer.stage_directory(src.path(), "demo").await.unwrap();

        // The new package replaces the old one wholesale — a leftover file from
        // the previous version would be loaded alongside the new manifest.
        assert_eq!(
            std::fs::read_to_string(plugins.path().join("demo/plugin.json")).unwrap(),
            "new"
        );
        assert!(!plugins.path().join("demo/stale.txt").exists());
    }

    #[tokio::test]
    async fn stage_directory_refuses_a_name_that_escapes_the_directory() {
        let plugins = tempdir().unwrap();
        let src = tempdir().unwrap();
        std::fs::write(src.path().join("plugin.json"), "{}").unwrap();
        let installer = PluginInstaller::new(plugins.path().to_path_buf());

        for name in ["../escape", "a/b", "", ".."] {
            assert!(
                installer.stage_directory(src.path(), name).await.is_err(),
                "'{name}' must be refused"
            );
        }
        assert_eq!(
            std::fs::read_dir(plugins.path()).unwrap().count(),
            0,
            "a refused name must leave nothing behind"
        );
    }

    #[tokio::test]
    async fn test_extract_archive_valid_tar_gz() {
        let tmp = tempdir().unwrap();
        let target = tmp.path().join("extracted");
        tokio::fs::create_dir_all(&target).await.unwrap();

        // Create a valid tar.gz archive in memory
        let mut tar_builder = tar::Builder::new(Vec::new());
        tar_builder
            .append_dir("plugins/test-plugin", std::path::Path::new("."))
            .unwrap();
        let test_content = b"hello, world!";
        let mut header = tar::Header::new_gnu();
        header.set_path("plugins/test-plugin/hello.txt").unwrap();
        header.set_size(test_content.len() as u64);
        header.set_cksum();
        tar_builder.append(&header, &test_content[..]).unwrap();
        let tar_bytes = tar_builder.into_inner().unwrap();

        // Gzip compress
        use flate2::write::GzEncoder;
        use flate2::Compression;
        let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
        encoder.write_all(&tar_bytes).unwrap();
        let gz_bytes = encoder.finish().unwrap();

        // Write to disk and extract
        let archive_path = tmp.path().join("test.tar.gz");
        tokio::fs::write(&archive_path, &gz_bytes).await.unwrap();
        PluginInstaller::extract_archive(&archive_path, &target)
            .await
            .unwrap();

        // Verify extraction
        let extracted_file = target.join("plugins/test-plugin/hello.txt");
        let content = tokio::fs::read_to_string(&extracted_file).await.unwrap();
        assert_eq!(content, "hello, world!");
    }

    /// Build a raw tar.gz with a single entry at the given path.
    /// Uses raw tar header bytes so we can inject malicious paths that
    /// `tar::Builder` rejects.
    fn build_raw_tar_gz(path: &str, content: &[u8]) -> Vec<u8> {
        use flate2::write::GzEncoder;
        use flate2::Compression;

        // Build a 512-byte tar header, then set fields.
        let mut hdr = [0u8; 512];

        // Name field (bytes 0-99)
        let name_bytes = path.as_bytes();
        let len = name_bytes.len().min(99);
        hdr[..len].copy_from_slice(&name_bytes[..len]);

        // Mode (100-107)
        hdr[100..107].copy_from_slice(b"0000644");

        // UID (108-115)
        hdr[108..115].copy_from_slice(b"0000000");

        // GID (116-123)
        hdr[116..123].copy_from_slice(b"0000000");

        // Size (124-135) — octal
        let size_str = format!("{:011o}", content.len());
        hdr[124..135].copy_from_slice(size_str.as_bytes());

        // Mtime (136-147)
        hdr[136..147].copy_from_slice(b"00000000000");

        // Type flag (156) — '0' = regular file
        hdr[156] = b'0';

        // Compute checksum: sum of all bytes in hdr, treating
        // bytes 148-155 (the checksum field) as spaces.
        let saved_chk = hdr[148..156].to_vec();
        hdr[148..156].copy_from_slice(b"        ");
        let cksum: u32 = hdr.iter().map(|&b| b as u32).sum();
        let cksum_str = format!("{:06o}\0 ", cksum);
        hdr[148..156].copy_from_slice(cksum_str.as_bytes());

        let mut tar_bytes = hdr.to_vec();

        // Content padded to 512-byte block
        tar_bytes.extend_from_slice(content);
        let padding = (512 - (content.len() % 512)) % 512;
        tar_bytes.extend_from_slice(&vec![0u8; padding]);

        // Gzip compress
        let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
        encoder.write_all(&tar_bytes).unwrap();
        encoder.finish().unwrap()
    }

    #[tokio::test]
    async fn test_extract_archive_zip_slip_parent() {
        let tmp = tempdir().unwrap();
        let target = tmp.path().join("extracted");
        tokio::fs::create_dir_all(&target).await.unwrap();

        let gz_bytes = build_raw_tar_gz("../evil.txt", b"malicious");
        let archive_path = tmp.path().join("zip-slip.tar.gz");
        tokio::fs::write(&archive_path, &gz_bytes).await.unwrap();
        let result = PluginInstaller::extract_archive(&archive_path, &target).await;

        assert!(result.is_err(), "expected error but got Ok");
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("Zip-slip") || err.contains("unsafe path"),
            "error '{}' does not mention zip-slip or unsafe path",
            err
        );
    }

    #[tokio::test]
    async fn test_extract_archive_zip_slip_absolute() {
        let tmp = tempdir().unwrap();
        let target = tmp.path().join("extracted");
        tokio::fs::create_dir_all(&target).await.unwrap();

        let gz_bytes = build_raw_tar_gz("/etc/passwd", b"malicious");
        let archive_path = tmp.path().join("absolute.tar.gz");
        tokio::fs::write(&archive_path, &gz_bytes).await.unwrap();
        let result = PluginInstaller::extract_archive(&archive_path, &target).await;

        assert!(result.is_err(), "expected error but got Ok");
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("Zip-slip") || err.contains("unsafe path"),
            "error '{}' does not mention zip-slip or unsafe path",
            err
        );
    }

    #[tokio::test]
    async fn test_uninstall_not_found() {
        let tmp = tempdir().unwrap();
        let installer = PluginInstaller::new(tmp.path().to_path_buf());
        let result = installer.uninstall("nonexistent").await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("not found"));
    }

    #[tokio::test]
    async fn test_uninstall_success() {
        let tmp = tempdir().unwrap();
        let plugin_dir = tmp.path().join("test-plugin");
        tokio::fs::create_dir_all(&plugin_dir).await.unwrap();
        tokio::fs::write(plugin_dir.join("file.txt"), b"data")
            .await
            .unwrap();

        let installer = PluginInstaller::new(tmp.path().to_path_buf());
        let result = installer.uninstall("test-plugin").await;
        assert!(result.is_ok());
        assert!(!plugin_dir.exists());
    }
}
