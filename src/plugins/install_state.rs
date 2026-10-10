//! Per-plugin install state: provenance, pinning, and rollback history.
//!
//! The record lives beside the plugin it describes, at
//! `plugins/<name>/.syscity-install.json`; superseded copies live OUTSIDE the
//! plugin directory, at `plugins/.history/<name>/<version>/` — inside it they
//! would be destroyed by the very removal that replaces the plugin (and by an
//! uninstall).
//!
//! The unit here is the whole package directory, not one file: a plugin is
//! `plugin.json` plus a `.wasm` (and anything else it ships), and rolling back
//! only the manifest would roll back nothing that runs. That is the one way
//! this differs from the skills' equivalent — see [`crate::skills::install_state`].
//!
//! Deliberately not a versioned directory layout (`plugins/<name>/<version>/`):
//! discovery (`PluginRuntime::initialize`) and the filesystem sync both key off
//! `plugins/<name>/plugin.json`, and a symlinked current-version indirection
//! would put Windows installs behind a developer-mode privilege.

use serde::{Deserialize, Serialize};

/// The filename beside `plugin.json` in an installed plugin's directory.
pub const STATE_FILE: &str = ".syscity-install.json";

/// The directory (inside the plugins dir) holding superseded copies.
pub const HISTORY_DIR: &str = ".history";

/// Where a plugin's history lives: `<plugins dir>/.history/<name>/…`.
fn history_dir(plugin_dir: &std::path::Path) -> crate::Result<std::path::PathBuf> {
    let plugins_dir = plugin_dir.parent().ok_or_else(|| {
        crate::error::SyscityError::Validation("plugin directory has no parent".to_string())
    })?;
    let name = plugin_dir
        .file_name()
        .and_then(|n| n.to_str())
        .ok_or_else(|| {
            crate::error::SyscityError::Validation(format!(
                "plugin directory name is not usable: {}",
                plugin_dir.display()
            ))
        })?;
    Ok(plugins_dir.join(HISTORY_DIR).join(name))
}

/// Provenance and lifecycle for one installed plugin.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct InstallState {
    /// Version of the installed package, from the catalog entry or from
    /// `plugin.json` at install time.
    pub version: Option<String>,
    /// Where it came from: `catalog:<id>` or `local:<path>`.
    pub source: Option<String>,
    /// Archive checksum when the source supplied one (catalog installs).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sha256: Option<String>,
    /// A pinned plugin is not replaced by an update.
    #[serde(default)]
    pub pinned: bool,
    /// When it was installed.
    pub installed_at: Option<chrono::DateTime<chrono::Utc>>,
}

/// Load the record for a plugin directory. Absent or unreadable file = no
/// record: an operator-authored plugin, or one that predates this feature.
/// Either way it is unpinned.
pub async fn load(plugin_dir: &std::path::Path) -> InstallState {
    match tokio::fs::read_to_string(plugin_dir.join(STATE_FILE)).await {
        Ok(text) => serde_json::from_str(&text).unwrap_or_default(),
        Err(_) => InstallState::default(),
    }
}

/// Write the record, atomically.
pub async fn save(plugin_dir: &std::path::Path, state: &InstallState) -> crate::Result<()> {
    let text = serde_json::to_string_pretty(state)?;
    let path = plugin_dir.join(STATE_FILE);
    let tmp = plugin_dir.join(format!("{STATE_FILE}.tmp"));
    tokio::fs::write(&tmp, text).await?;
    tokio::fs::rename(&tmp, &path).await?;
    Ok(())
}

/// The version of an installed plugin, from the record when there is one and
/// from `plugin.json` otherwise.
pub async fn installed_version(plugin_dir: &std::path::Path) -> Option<String> {
    if let Some(v) = load(plugin_dir).await.version {
        return Some(v);
    }
    manifest_version(plugin_dir).await
}

/// The `version` field of the plugin's manifest, if it can be read.
async fn manifest_version(plugin_dir: &std::path::Path) -> Option<String> {
    manifest_string(plugin_dir, "version").await
}

/// The `id` in the plugin's manifest.
///
/// This is what the runtime and every WS method key on — the directory name is
/// only where the package happens to sit, and the two can differ.
pub async fn manifest_id(plugin_dir: &std::path::Path) -> Option<String> {
    manifest_string(plugin_dir, "id").await
}

async fn manifest_string(plugin_dir: &std::path::Path, field: &str) -> Option<String> {
    let text = tokio::fs::read_to_string(plugin_dir.join("plugin.json"))
        .await
        .ok()?;
    let value: serde_json::Value = serde_json::from_str(&text).ok()?;
    value
        .get(field)
        .and_then(|v| v.as_str())
        .map(str::to_string)
}

/// Back up the whole current package before a replacement overwrites it.
///
/// The backup is named for the version being replaced. The install record is
/// left out of the copy — it describes the installation, not the package, and
/// a restored copy must not resurrect the provenance of the version it came
/// from.
pub async fn backup_current(
    plugin_dir: &std::path::Path,
) -> crate::Result<Option<std::path::PathBuf>> {
    if !plugin_dir.join("plugin.json").exists() {
        return Ok(None);
    }

    let version = installed_version(plugin_dir)
        .await
        .unwrap_or_else(|| "unknown".to_string());
    let destination = history_dir(plugin_dir)?.join(&version);
    if destination.exists() {
        // Replacing a same-version copy replaces the backup of that version —
        // the older one is the same content by definition.
        tokio::fs::remove_dir_all(&destination).await?;
    }
    tokio::fs::create_dir_all(&destination).await?;
    copy_package(plugin_dir, &destination).await?;
    Ok(Some(destination))
}

/// Copy a plugin package, leaving the install record behind.
///
/// The record describes the installation, not the package — a restored copy
/// must not resurrect the provenance of the version it came from.
async fn copy_package(src: &std::path::Path, dst: &std::path::Path) -> crate::Result<()> {
    crate::plugins::installer::copy_dir_all(src, dst).await?;
    let _ = tokio::fs::remove_file(dst.join(STATE_FILE)).await;
    let _ = tokio::fs::remove_file(dst.join(format!("{STATE_FILE}.tmp"))).await;
    Ok(())
}

/// Remove everything inside `dir`, leaving the directory itself.
async fn clear_package(dir: &std::path::Path) -> crate::Result<()> {
    let mut entries = tokio::fs::read_dir(dir).await?;
    while let Some(entry) = entries.next_entry().await? {
        let path = entry.path();
        if entry.file_type().await?.is_dir() {
            tokio::fs::remove_dir_all(&path).await?;
        } else {
            tokio::fs::remove_file(&path).await?;
        }
    }
    Ok(())
}

/// Versions available to roll back to, oldest first.
pub async fn history_versions(plugin_dir: &std::path::Path) -> Vec<String> {
    let mut versions = Vec::new();
    let Ok(dir) = history_dir(plugin_dir) else {
        return versions;
    };
    let mut entries = match tokio::fs::read_dir(&dir).await {
        Ok(entries) => entries,
        Err(_) => return versions,
    };
    while let Ok(Some(entry)) = entries.next_entry().await {
        if entry.path().join("plugin.json").exists() {
            versions.push(entry.file_name().to_string_lossy().to_string());
        }
    }
    versions.sort();
    versions
}

/// Restore `version` from the history copy: back up what is installed now, then
/// put the historical package in its place.
///
/// The record keeps the provenance of the *installation* and only its version
/// changes — a rollback says which package is on disk, not that the plugin came
/// from somewhere else.
pub async fn rollback(plugin_dir: &std::path::Path, version: &str) -> crate::Result<()> {
    // Reject a name that could escape the history directory: separators and
    // traversal, not the dots every semver carries.
    if version.is_empty()
        || version.contains('/')
        || version.contains('\\')
        || version.contains("..")
    {
        return Err(crate::error::SyscityError::Validation(format!(
            "'{version}' is not a history version name"
        )));
    }
    let backup = history_dir(plugin_dir)?.join(version);
    if !backup.join("plugin.json").exists() {
        return Err(crate::error::SyscityError::NotFound {
            resource: format!("no saved copy of version '{version}'"),
        });
    }

    // Read the record before anything is removed — it is the only place the
    // source is written down.
    let mut state = load(plugin_dir).await;

    backup_current(plugin_dir).await?;
    clear_package(plugin_dir).await?;
    copy_package(&backup, plugin_dir).await?;

    state.version = Some(version.to_string());
    save(plugin_dir, &state).await
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A plugins directory holding a `demo` plugin, plus the record that says
    /// which version it is.
    async fn fixture(version: &str) -> tempfile::TempDir {
        let root = tempfile::tempdir().unwrap();
        let plugin = root.path().join("demo");
        tokio::fs::create_dir_all(&plugin).await.unwrap();
        tokio::fs::write(
            plugin.join("plugin.json"),
            format!(r#"{{"id":"demo","name":"demo","version":"{version}","main":"demo.wasm"}}"#),
        )
        .await
        .unwrap();
        tokio::fs::write(plugin.join("demo.wasm"), b"wasm-v1")
            .await
            .unwrap();
        let state = InstallState {
            version: Some(version.to_string()),
            source: Some("catalog:demo".to_string()),
            ..Default::default()
        };
        save(&plugin, &state).await.unwrap();
        root
    }

    #[tokio::test]
    async fn a_plugin_with_no_record_is_unpinned() {
        let root = tempfile::tempdir().unwrap();
        let plugin = root.path().join("bare");
        tokio::fs::create_dir_all(&plugin).await.unwrap();
        let state = load(&plugin).await;
        assert!(!state.pinned);
        assert!(state.version.is_none());
    }

    #[tokio::test]
    async fn the_record_round_trips() {
        let root = fixture("1.0.0").await;
        let plugin = root.path().join("demo");
        let state = load(&plugin).await;
        assert_eq!(state.version.as_deref(), Some("1.0.0"));
        assert_eq!(state.source.as_deref(), Some("catalog:demo"));
    }

    /// The fallback that matters when a plugin was placed by hand: no record,
    /// but the manifest still knows its version.
    #[tokio::test]
    async fn version_falls_back_to_the_manifest() {
        let root = tempfile::tempdir().unwrap();
        let plugin = root.path().join("demo");
        tokio::fs::create_dir_all(&plugin).await.unwrap();
        tokio::fs::write(
            plugin.join("plugin.json"),
            r#"{"id":"demo","name":"demo","version":"2.3.4"}"#,
        )
        .await
        .unwrap();
        assert_eq!(installed_version(&plugin).await.as_deref(), Some("2.3.4"));
    }

    /// The whole package is backed up — the `.wasm` included, and the record
    /// excluded so a restore cannot resurrect stale provenance.
    #[tokio::test]
    async fn backup_copies_the_package_without_the_record() {
        let root = fixture("1.0.0").await;
        let plugin = root.path().join("demo");

        let dir = backup_current(&plugin).await.unwrap().expect("a backup");
        assert!(dir.join("plugin.json").exists());
        assert!(dir.join("demo.wasm").exists());
        assert!(
            !dir.join(STATE_FILE).exists(),
            "the install record must not travel with the package"
        );
        // And the live copy is untouched.
        assert!(plugin.join(STATE_FILE).exists());
        assert!(plugin.join("demo.wasm").exists());
    }

    #[tokio::test]
    async fn history_versions_are_listed_oldest_first() {
        let root = fixture("1.0.0").await;
        let plugin = root.path().join("demo");
        backup_current(&plugin).await.unwrap();
        tokio::fs::write(plugin.join("plugin.json"), r#"{"id":"demo","version":"1.1.0"}"#)
            .await
            .unwrap();
        let mut state = load(&plugin).await;
        state.version = Some("1.1.0".to_string());
        save(&plugin, &state).await.unwrap();
        backup_current(&plugin).await.unwrap();

        assert_eq!(history_versions(&plugin).await, vec!["1.0.0", "1.1.0"]);
    }

    #[tokio::test]
    async fn rollback_restores_the_package_and_the_version() {
        let root = fixture("1.0.0").await;
        let plugin = root.path().join("demo");
        backup_current(&plugin).await.unwrap();

        // The install is replaced by 1.1.0.
        tokio::fs::write(plugin.join("demo.wasm"), b"wasm-v2")
            .await
            .unwrap();
        let mut state = load(&plugin).await;
        state.version = Some("1.1.0".to_string());
        save(&plugin, &state).await.unwrap();

        rollback(&plugin, "1.0.0").await.unwrap();

        assert_eq!(tokio::fs::read(plugin.join("demo.wasm")).await.unwrap(), b"wasm-v1");
        assert_eq!(installed_version(&plugin).await.as_deref(), Some("1.0.0"));
        // Provenance is the installation's, not the restored version's.
        assert_eq!(load(&plugin).await.source.as_deref(), Some("catalog:demo"));
        // And 1.1.0 is now itself recoverable.
        assert_eq!(history_versions(&plugin).await, vec!["1.0.0", "1.1.0"]);
    }

    #[tokio::test]
    async fn rollback_refuses_when_pinned_preserved_and_traversal_names() {
        let root = fixture("1.0.0").await;
        let plugin = root.path().join("demo");

        let err = rollback(&plugin, "9.9.9").await.unwrap_err().to_string();
        assert!(err.contains("no saved copy"), "{err}");

        for bad in ["", "..", "../etc", "a/b"] {
            let err = rollback(&plugin, bad).await.unwrap_err().to_string();
            assert!(err.contains("not a history version name"), "{bad}: {err}");
        }
    }
}
