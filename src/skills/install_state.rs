//! Per-skill install state: provenance, pinning, rollback history, and usage.
//!
//! The record lives beside the skill it describes, at
//! `skills/<name>/.syscity-install.json`; superseded copies live OUTSIDE the
//! skill directory, at `skills/.history/<name>/<version>/` — inside the skill
//! directory they would be destroyed by the very removal that replaces the
//! skill (and by an uninstall). The loader reads `SKILL.md` and nothing else,
//! so neither location affects discovery.
//!
//! Deliberately not a versioned directory layout (`skills/<name>/<version>/`):
//! the loader's discovery and the manager's hot-reload watcher key off
//! `skills/<name>/SKILL.md`, and a symlinked current-version indirection would
//! put Windows installs behind a developer-mode privilege. A rollback therefore
//! restores from the history copy rather than re-pointing a link.

use serde::{Deserialize, Serialize};

/// The filename beside `SKILL.md` in an installed skill's directory.
pub const STATE_FILE: &str = ".syscity-install.json";

/// The directory (inside the user skills dir) holding superseded copies.
pub const HISTORY_DIR: &str = ".history";

/// Where a skill's history lives: `<user skills dir>/.history/<name>/…`.
/// Outside the skill directory by design — see the module note.
fn history_dir(skill_dir: &std::path::Path) -> crate::Result<std::path::PathBuf> {
    let user_dir = skill_dir.parent().ok_or_else(|| {
        crate::error::SyscityError::Validation("skill directory has no parent".to_string())
    })?;
    let name = skill_dir
        .file_name()
        .and_then(|n| n.to_str())
        .ok_or_else(|| {
            crate::error::SyscityError::Validation(format!(
                "skill directory name is not usable: {}",
                skill_dir.display()
            ))
        })?;
    Ok(user_dir.join(HISTORY_DIR).join(name))
}

/// Provenance and lifecycle for one installed skill.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct InstallState {
    /// Version of the installed content (from the catalog entry or the
    /// SKILL.md frontmatter at install time).
    pub version: Option<String>,
    /// Where it came from: `catalog:<id>`, `git:<url>`, `local:<path>`.
    /// Absent for operator-authored skills.
    pub source: Option<String>,
    /// Archive checksum when the source supplied one (catalog installs).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sha256: Option<String>,
    /// A pinned skill is not touched by updates.
    #[serde(default)]
    pub pinned: bool,
    /// When it was installed.
    pub installed_at: Option<chrono::DateTime<chrono::Utc>>,
    /// Usage counters, incremented at activation.
    #[serde(default)]
    pub usage: Usage,
}

/// How often this skill has been activated.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Usage {
    /// Total activations.
    pub count: u64,
    /// Most recent activation.
    pub last_used: Option<chrono::DateTime<chrono::Utc>>,
}

/// Load the record for a skill directory. Absent or unreadable file = no
/// record — a skill with no record is operator-authored or predates this
/// feature, and both are fine to treat as unpinned with zero usage.
pub async fn load(skill_dir: &std::path::Path) -> InstallState {
    match tokio::fs::read_to_string(skill_dir.join(STATE_FILE)).await {
        Ok(text) => serde_json::from_str(&text).unwrap_or_default(),
        Err(_) => InstallState::default(),
    }
}

/// Write the record, atomically.
pub async fn save(skill_dir: &std::path::Path, state: &InstallState) -> crate::Result<()> {
    let text = serde_json::to_string_pretty(state)?;
    let path = skill_dir.join(STATE_FILE);
    let tmp = skill_dir.join(format!("{STATE_FILE}.tmp"));
    tokio::fs::write(&tmp, text).await?;
    tokio::fs::rename(&tmp, &path).await?;
    Ok(())
}

/// Record one activation of the skill in `skill_dir`.
pub async fn record_use(skill_dir: &std::path::Path) -> crate::Result<()> {
    let mut state = load(skill_dir).await;
    state.usage.count += 1;
    state.usage.last_used = Some(chrono::Utc::now());
    save(skill_dir, &state).await
}

/// Back up the current `SKILL.md` before a replacement overwrites it.
///
/// The backup is named for the version being replaced: from the install record
/// when one exists, else from the frontmatter itself, else "unknown". Replacing
/// a same-version copy replaces the backup of that version — the older one is
/// the same content by definition.
pub async fn backup_current(
    skill_dir: &std::path::Path,
) -> crate::Result<Option<std::path::PathBuf>> {
    let current = skill_dir.join("SKILL.md");
    if !current.exists() {
        return Ok(None);
    }

    let state = load(skill_dir).await;
    let version = match state.version {
        Some(v) => v,
        None => {
            let text = tokio::fs::read_to_string(&current).await?;
            let (frontmatter, _) = crate::skills::parse_skill_md(&text)?;
            serde_norway::from_str::<serde_norway::Mapping>(&frontmatter)
                .ok()
                .and_then(|m| {
                    m.get(serde_norway::Value::String("version".to_string()))
                        .cloned()
                })
                .and_then(|v| v.as_str().map(str::to_string))
                .unwrap_or_else(|| "unknown".to_string())
        }
    };

    let dir = history_dir(skill_dir)?.join(&version);
    tokio::fs::create_dir_all(&dir).await?;
    let backup = dir.join("SKILL.md");
    tokio::fs::copy(&current, &backup).await?;
    Ok(Some(backup))
}

/// Versions available to roll back to, newest last.
pub async fn history_versions(skill_dir: &std::path::Path) -> Vec<String> {
    let mut versions = Vec::new();
    let dir = history_dir(skill_dir).unwrap_or_else(|_| {
        skill_dir.join(HISTORY_DIR) // unreachable in practice; only used for a
                                    // graceful empty result when the path cannot be derived
    });
    let mut entries = match tokio::fs::read_dir(&dir).await {
        Ok(entries) => entries,
        Err(_) => return versions,
    };
    while let Ok(Some(entry)) = entries.next_entry().await {
        if entry.path().join("SKILL.md").exists() {
            versions.push(entry.file_name().to_string_lossy().to_string());
        }
    }
    versions.sort();
    versions
}

/// Restore `version` from the history copy: back up the current one, then put
/// the historical `SKILL.md` in its place.
///
/// This restores the markdown body and frontmatter — provenance stays in the
/// state file, which now records the version the skill was rolled back *to*.
pub async fn rollback(skill_dir: &std::path::Path, version: &str) -> crate::Result<()> {
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
    let backup = history_dir(skill_dir)?.join(version).join("SKILL.md");
    if !backup.exists() {
        return Err(crate::error::SyscityError::NotFound {
            resource: format!("no saved copy of version '{version}'"),
        });
    }

    backup_current(skill_dir).await?;
    tokio::fs::copy(&backup, skill_dir.join("SKILL.md")).await?;

    let mut state = load(skill_dir).await;
    state.version = Some(version.to_string());
    save(skill_dir, &state).await
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A temp dir acting as the *user skills dir*, with a `demo` skill inside
    /// it. History lives at `<root>/.history/demo/…`, so the fixture must hold
    /// the root one level above the skill — otherwise the system temp dir gets
    /// the history.
    fn skills_root() -> (tempfile::TempDir, std::path::PathBuf) {
        let root = tempfile::tempdir().expect("temp skills root");
        let dir = root.path().join("demo");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("SKILL.md"),
            "---\nname: demo\ndescription: \"d\"\nversion: \"1.0.0\"\n---\n\nbody\n",
        )
        .unwrap();
        (root, dir)
    }

    #[tokio::test]
    async fn absent_state_reads_as_default() {
        let dir = tempfile::tempdir().unwrap();
        let state = load(dir.path()).await;
        assert!(!state.pinned);
        assert_eq!(state.usage.count, 0);
    }

    #[tokio::test]
    async fn record_use_accumulates_and_roundtrips() {
        let (_root, dir) = skills_root();
        record_use(&dir).await.unwrap();
        record_use(&dir).await.unwrap();
        let state = load(&dir).await;
        assert_eq!(state.usage.count, 2);
        assert!(state.usage.last_used.is_some());
    }

    #[tokio::test]
    async fn backup_current_uses_the_frontmatter_version() {
        let (root, dir) = skills_root();
        let backup = backup_current(&dir).await.unwrap().expect("backup");
        // Outside the skill directory: a copy inside it would be destroyed by
        // the very replacement that needs it.
        assert_eq!(backup, root.path().join(".history/demo/1.0.0/SKILL.md"));
        assert!(backup.exists());
    }

    #[tokio::test]
    async fn backup_survives_the_replacement_it_precedes() {
        let (root, dir) = skills_root();
        backup_current(&dir).await.unwrap();
        // Simulate install_to_user: the whole skill directory is removed.
        std::fs::remove_dir_all(&dir).unwrap();
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("SKILL.md"),
            "---\nname: demo\ndescription: \"d\"\nversion: \"2.0.0\"\n---\n\nnew\n",
        )
        .unwrap();

        let backup = root.path().join(".history/demo/1.0.0/SKILL.md");
        assert!(backup.exists(), "the backup must outlive the removal");
    }

    #[tokio::test]
    async fn rollback_restores_the_historical_copy() {
        let (_root, dir) = skills_root();

        backup_current(&dir).await.unwrap();
        std::fs::write(
            dir.join("SKILL.md"),
            "---\nname: demo\ndescription: \"d\"\nversion: \"2.0.0\"\n---\n\nnew body\n",
        )
        .unwrap();
        save(
            &dir,
            &InstallState {
                version: Some("2.0.0".to_string()),
                pinned: true,
                ..Default::default()
            },
        )
        .await
        .unwrap();

        assert_eq!(history_versions(&dir).await, vec!["1.0.0".to_string()]);

        rollback(&dir, "1.0.0").await.unwrap();
        let restored = std::fs::read_to_string(dir.join("SKILL.md")).unwrap();
        assert!(restored.contains("version: \"1.0.0\""), "{restored}");
        // The state file tracks what is now on disk, and pinning survives.
        let state = load(&dir).await;
        assert_eq!(state.version.as_deref(), Some("1.0.0"));
        assert!(state.pinned);
    }

    #[tokio::test]
    async fn rollback_refuses_a_version_that_is_not_in_history() {
        let (_root, dir) = skills_root();
        let err = rollback(&dir, "9.9.9").await.unwrap_err();
        assert!(format!("{err}").contains("no saved copy"), "{err}");
        // Traversal-shaped names never leave the history directory.
        assert!(rollback(&dir, "..").await.is_err());
        assert!(rollback(&dir, "../x").await.is_err());
        assert!(!dir.join("SKILL.md.bak").exists());
    }

    #[tokio::test]
    async fn rollback_without_a_current_file_only_restores() {
        // No SKILL.md on disk: nothing to back up, but the restore must still
        // work (a skill whose file was deleted by hand can be brought back).
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join("demo");
        std::fs::create_dir_all(&dir).unwrap();
        let history = root.path().join(".history/demo/1.0.0");
        std::fs::create_dir_all(&history).unwrap();
        std::fs::write(
            history.join("SKILL.md"),
            "---\nname: demo\ndescription: \"d\"\n---\n\nold\n",
        )
        .unwrap();

        rollback(&dir, "1.0.0").await.unwrap();
        assert!(dir.join("SKILL.md").exists());
    }
}
