//! Archive agent-authored skills that have gone unused.
//!
//! Skills the agent writes for itself accumulate, and a stale one costs a line
//! in the system-prompt catalog of every session whether or not it is ever
//! used. This is the counterpart to that: a skill the *agent* authored, that
//! nobody has pinned and nobody has activated for long enough, is moved out of
//! the live directory into `skills/.archive/`.
//!
//! Three deliberate limits, all inherited from the skills lifecycle:
//!
//! - **Only agent-authored skills.** The record says who wrote it
//!   ([`install_state::is_agent_authored`]); a skill the operator placed by hand
//!   has no record and is never touched, and neither is anything installed from
//!   the catalog.
//! - **Archive, never delete.** `restore` puts it back, and the install record
//!   travels with it.
//! - **A pin is a hold.** A pinned skill is out of scope regardless of age.
//!
//! `.archive` is a dot-directory holding one directory per skill, so
//! `SkillStorage::discover_at_level` — which looks for a `SKILL.md` directly
//! inside a child of the skills directory — does not see it or its contents.

use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};

use super::install_state;

/// Directory under the skills root holding archived skills.
pub const ARCHIVE_DIR: &str = ".archive";

/// How the curator behaves.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CuratorConfig {
    /// Run the periodic pass at all.
    pub enabled: bool,
    /// How often it looks.
    pub interval_seconds: u64,
    /// Days without an activation before an agent-authored skill is archived.
    pub archive_after_days: i64,
}

impl Default for CuratorConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            interval_seconds: 6 * 60 * 60,
            archive_after_days: 30,
        }
    }
}

/// An agent-authored skill that has gone unused long enough to archive.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StaleSkill {
    /// Directory name under the skills root.
    pub name: String,
    /// Last activation, if it was ever activated.
    pub last_used: Option<DateTime<Utc>>,
    /// When the agent wrote it.
    pub installed_at: Option<DateTime<Utc>>,
}

impl StaleSkill {
    /// What the age was measured from: the last activation, or the install for
    /// a skill nobody has ever run.
    pub fn reference(&self) -> Option<DateTime<Utc>> {
        self.last_used.or(self.installed_at)
    }
}

/// Reject a name that could escape the skills directory.
fn usable_name(name: &str) -> bool {
    !name.is_empty() && name != "." && name != ".." && !name.contains(['/', '\\'])
}

/// Agent-authored, unpinned skills whose reference time is older than
/// `archive_after_days`, oldest first.
///
/// A skill whose age cannot be determined is left alone: the curator's job is
/// to tidy up, and it has no business guessing.
pub async fn stale(
    skills_dir: &Path,
    archive_after_days: i64,
    now: DateTime<Utc>,
) -> Vec<StaleSkill> {
    let mut candidates = Vec::new();

    let mut entries = match tokio::fs::read_dir(skills_dir).await {
        Ok(entries) => entries,
        Err(_) => return candidates,
    };
    while let Ok(Some(entry)) = entries.next_entry().await {
        let path = entry.path();
        if !path.is_dir() || !path.join("SKILL.md").exists() {
            continue;
        }
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        let state = install_state::load(&path).await;
        if !install_state::is_agent_authored(&state) || state.pinned {
            continue;
        }
        let candidate = StaleSkill {
            name: name.to_string(),
            last_used: state.usage.last_used,
            installed_at: state.installed_at,
        };
        let Some(reference) = candidate.reference() else {
            continue;
        };
        let age = now.signed_duration_since(reference);
        if age.num_days() >= archive_after_days {
            candidates.push(candidate);
        }
    }

    candidates.sort_by_key(|c| c.reference());
    candidates
}

/// Move `name` out of the live directory into `skills/.archive/`.
///
/// Returns where it landed. Refuses to overwrite an archived copy of the same
/// name — the earlier one is a different skill that happens to share a name,
/// and losing it is not this function's call to make.
pub async fn archive(skills_dir: &Path, name: &str) -> crate::Result<PathBuf> {
    if !usable_name(name) {
        return Err(crate::error::SyscityError::Validation(format!(
            "'{name}' is not a usable skill name"
        )));
    }
    let archive_root = skills_dir.join(ARCHIVE_DIR);
    let destination = archive_root.join(name);
    // Checked before the live directory: a name that is already in the archive
    // is the more useful thing to report, and after a first archive the live
    // directory is gone — so the other order answers "not found" to a question
    // that is really "already archived".
    if destination.exists() {
        return Err(crate::error::SyscityError::Validation(format!(
            "an archived copy of '{name}' already exists at {}",
            destination.display()
        )));
    }
    let live = skills_dir.join(name);
    if !live.join("SKILL.md").exists() {
        return Err(crate::error::SyscityError::NotFound {
            resource: format!("skill '{name}'"),
        });
    }
    tokio::fs::create_dir_all(&archive_root).await?;
    tokio::fs::rename(&live, &destination).await?;
    Ok(destination)
}

/// The skills currently sitting in the archive, oldest name first.
pub async fn archived(skills_dir: &Path) -> Vec<String> {
    let mut names = Vec::new();
    let mut entries = match tokio::fs::read_dir(skills_dir.join(ARCHIVE_DIR)).await {
        Ok(entries) => entries,
        Err(_) => return names,
    };
    while let Ok(Some(entry)) = entries.next_entry().await {
        if entry.path().join("SKILL.md").exists() {
            names.push(entry.file_name().to_string_lossy().to_string());
        }
    }
    names.sort();
    names
}

/// Put an archived skill back in the live directory.
pub async fn restore(skills_dir: &Path, name: &str) -> crate::Result<()> {
    if !usable_name(name) {
        return Err(crate::error::SyscityError::Validation(format!(
            "'{name}' is not a usable skill name"
        )));
    }
    let archived = skills_dir.join(ARCHIVE_DIR).join(name);
    if !archived.join("SKILL.md").exists() {
        return Err(crate::error::SyscityError::NotFound {
            resource: format!("archived skill '{name}'"),
        });
    }
    let live = skills_dir.join(name);
    if live.exists() {
        return Err(crate::error::SyscityError::Validation(format!(
            "a live skill named '{name}' already exists at {}",
            live.display()
        )));
    }
    tokio::fs::rename(&archived, &live).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A skills dir with one agent-authored skill, aged by `days`.
    async fn fixture(name: &str, days_ago: i64, used: bool) -> tempfile::TempDir {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join(name);
        tokio::fs::create_dir_all(&dir).await.unwrap();
        tokio::fs::write(dir.join("SKILL.md"), "---\nname: x\ndescription: d\n---\n\nbody\n")
            .await
            .unwrap();

        let stamp = Utc::now() - chrono::Duration::days(days_ago);
        let mut state = install_state::InstallState {
            source: Some(format!("{}conv-1", install_state::AGENT_SOURCE_PREFIX)),
            installed_at: Some(stamp),
            ..Default::default()
        };
        if used {
            state.usage.count = 3;
            state.usage.last_used = Some(stamp);
        }
        install_state::save(&dir, &state).await.unwrap();
        root
    }

    fn now() -> DateTime<Utc> {
        Utc::now()
    }

    #[tokio::test]
    async fn an_old_agent_skill_is_stale() {
        let root = fixture("old", 40, false).await;
        let stale = stale(root.path(), 30, now()).await;
        assert_eq!(stale.len(), 1);
        assert_eq!(stale[0].name, "old");
    }

    #[tokio::test]
    async fn a_recently_used_skill_is_not() {
        let root = fixture("busy", 40, true).await;
        // Used 40 days ago → still stale. Now make it recent.
        let dir = root.path().join("busy");
        let mut state = install_state::load(&dir).await;
        state.usage.last_used = Some(Utc::now());
        install_state::save(&dir, &state).await.unwrap();

        assert!(stale(root.path(), 30, now()).await.is_empty());
    }

    /// The operator's own skills have no record, and a pin is a hold.
    #[tokio::test]
    async fn hand_placed_and_pinned_skills_are_left_alone() {
        let root = tempfile::tempdir().unwrap();

        let hand = root.path().join("handwritten");
        tokio::fs::create_dir_all(&hand).await.unwrap();
        tokio::fs::write(hand.join("SKILL.md"), "---\nname: h\n---\n\nb\n")
            .await
            .unwrap();

        let pinned_root = fixture("pinned", 400, false).await;
        let pinned = pinned_root.path().join("pinned");
        let mut state = install_state::load(&pinned).await;
        state.pinned = true;
        install_state::save(&pinned, &state).await.unwrap();

        assert!(stale(root.path(), 30, now()).await.is_empty());
        assert!(stale(pinned_root.path(), 30, now()).await.is_empty());
    }

    /// A marketplace install is not the agent's to tidy away.
    #[tokio::test]
    async fn catalog_installs_are_not_stale() {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join("from-catalog");
        tokio::fs::create_dir_all(&dir).await.unwrap();
        tokio::fs::write(dir.join("SKILL.md"), "---\nname: c\n---\n\nb\n")
            .await
            .unwrap();
        install_state::save(
            &dir,
            &install_state::InstallState {
                source: Some("catalog:pdf".to_string()),
                installed_at: Some(Utc::now() - chrono::Duration::days(400)),
                ..Default::default()
            },
        )
        .await
        .unwrap();

        assert!(stale(root.path(), 30, now()).await.is_empty());
    }

    #[tokio::test]
    async fn archive_hides_the_skill_and_restore_brings_it_back() {
        let root = fixture("tidy", 40, false).await;

        let landed = archive(root.path(), "tidy").await.unwrap();
        assert!(landed.ends_with(".archive/tidy"));
        assert!(!root.path().join("tidy").exists(), "gone from the live dir");
        assert_eq!(archived(root.path()).await, vec!["tidy"]);
        // And it is no longer a candidate — it is not a live skill at all.
        assert!(stale(root.path(), 30, now()).await.is_empty());

        restore(root.path(), "tidy").await.unwrap();
        assert!(root.path().join("tidy/SKILL.md").exists());
        assert!(archived(root.path()).await.is_empty());
        // The record travelled with it.
        let state = install_state::load(&root.path().join("tidy")).await;
        assert!(install_state::is_agent_authored(&state));
    }

    #[tokio::test]
    async fn archiving_refuses_bad_names_missing_skills_and_double_archives() {
        let root = fixture("twice", 40, false).await;

        for bad in ["", ".", "..", "a/b"] {
            assert!(archive(root.path(), bad).await.is_err(), "{bad}");
        }
        assert!(archive(root.path(), "absent").await.is_err());

        archive(root.path(), "twice").await.unwrap();
        let err = archive(root.path(), "twice").await.unwrap_err().to_string();
        assert!(err.contains("already exists"), "{err}");
        // The archived copy is untouched by the refusal.
        assert!(archived(root.path()).await.contains(&"twice".to_string()));
    }

    #[tokio::test]
    async fn restore_refuses_when_a_live_skill_has_taken_the_name() {
        let root = fixture("shadow", 40, false).await;
        archive(root.path(), "shadow").await.unwrap();

        let live = root.path().join("shadow");
        tokio::fs::create_dir_all(&live).await.unwrap();
        tokio::fs::write(live.join("SKILL.md"), "---\nname: s\n---\n\nb\n")
            .await
            .unwrap();

        assert!(restore(root.path(), "shadow").await.is_err());
        assert!(restore(root.path(), "never-archived").await.is_err());
    }
}
