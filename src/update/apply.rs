//! Apply a downloaded release tarball by replacing the running binary.
//!
//! The new binary is extracted from the gzipped tarball, written to a temp
//! file in the executable's directory, and atomically renamed over the target
//! path. A crash mid-apply never leaves a truncated binary in place, and on
//! Unix renaming over a running executable is safe (the old inode stays mapped
//! until the process exits).

use std::io::Read;
use std::path::Path;

use crate::error::SyscityError;
use crate::Result;

/// Replace the currently running executable with the `syscity` binary from
/// the release tarball `pkg`.
pub fn apply_binary(pkg: &Path) -> Result<()> {
    let exe = std::env::current_exe().map_err(crate::error::SyscityError::Io)?;
    apply_binary_to(pkg, &exe)
}

/// Replace the binary at `exe` with the `syscity` binary from `pkg`.
///
/// Split out from [`apply_binary`] so tests can target a temp path instead of
/// the real test executable.
pub fn apply_binary_to(pkg: &Path, exe: &Path) -> Result<()> {
    let bytes = extract_binary_from_tarball(pkg)?;

    let exe_dir = exe
        .parent()
        .ok_or_else(|| SyscityError::Internal(format!("target has no parent dir: {exe:?}")))?;
    let tmp = tempfile::NamedTempFile::new_in(exe_dir)?;
    {
        use std::io::Write;
        let mut f = tmp.reopen().map_err(crate::error::SyscityError::Io)?;
        f.write_all(&bytes)
            .map_err(crate::error::SyscityError::Io)?;
        f.sync_all().map_err(crate::error::SyscityError::Io)?;
    }

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(tmp.path(), std::fs::Permissions::from_mode(0o755))
            .map_err(crate::error::SyscityError::Io)?;
    }

    tmp.persist(exe).map_err(|e| SyscityError::Io(e.error))?;
    Ok(())
}

/// Read the `syscity` binary entry out of a gzipped tarball.
fn extract_binary_from_tarball(pkg: &Path) -> Result<Vec<u8>> {
    let file = std::fs::File::open(pkg).map_err(crate::error::SyscityError::Io)?;
    let gz = flate2::read::GzDecoder::new(file);
    let mut archive = tar::Archive::new(gz);

    let entries = archive.entries().map_err(crate::error::SyscityError::Io)?;
    for entry in entries {
        let mut entry = entry.map_err(crate::error::SyscityError::Io)?;
        let is_binary = entry.header().entry_type().is_file()
            && entry
                .path()
                .map(|p| p.file_name().map(|n| n == "syscity").unwrap_or(false))
                .unwrap_or(false);
        if is_binary {
            let mut buf = Vec::new();
            entry
                .read_to_end(&mut buf)
                .map_err(crate::error::SyscityError::Io)?;
            return Ok(buf);
        }
    }

    Err(SyscityError::NotFound {
        resource: "syscity binary in update tarball".to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_tarball(dir: &Path, entries: &[(&str, &[u8])]) -> std::path::PathBuf {
        let path = dir.join("pkg.tar.gz");
        let file = std::fs::File::create(&path).unwrap();
        let enc = flate2::write::GzEncoder::new(file, flate2::Compression::default());
        let mut tar = tar::Builder::new(enc);
        for (name, bytes) in entries {
            let mut header = tar::Header::new_gnu();
            header.set_size((*bytes).len() as u64);
            header.set_mode(0o755);
            header.set_cksum();
            tar.append_data(&mut header, name, *bytes).unwrap();
        }
        tar.into_inner().unwrap().finish().unwrap();
        path
    }

    #[test]
    fn apply_replaces_target_with_tarball_binary() {
        let dir = tempfile::tempdir().unwrap();
        let pkg = make_tarball(dir.path(), &[("syscity", b"#!/bin/sh\nnew-binary".as_slice())]);
        let exe = dir.path().join("syscity");

        apply_binary_to(&pkg, &exe).unwrap();

        let got = std::fs::read(&exe).unwrap();
        assert_eq!(got, b"#!/bin/sh\nnew-binary");

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&exe).unwrap().permissions().mode();
            assert!(mode & 0o111 != 0, "binary must be executable");
        }
    }

    #[test]
    fn apply_ignores_other_entries_and_picks_syscity() {
        let dir = tempfile::tempdir().unwrap();
        let pkg = make_tarball(
            dir.path(),
            &[
                ("README.md", b"docs".as_slice()),
                ("syscity", b"real-binary".as_slice()),
            ],
        );
        let exe = dir.path().join("syscity");

        apply_binary_to(&pkg, &exe).unwrap();
        assert_eq!(std::fs::read(&exe).unwrap(), b"real-binary");
    }

    #[test]
    fn apply_errors_when_binary_missing() {
        let dir = tempfile::tempdir().unwrap();
        let pkg = make_tarball(dir.path(), &[("README.md", b"docs".as_slice())]);
        let exe = dir.path().join("syscity");

        let result = apply_binary_to(&pkg, &exe);
        assert!(result.is_err());
    }
}
