//! Multi-level skill storage
//!
//! Manages skills at multiple levels:
//! - Bundled: Built-in skills shipped with Syscity
//! - User: Skills in ~/.syscity/skills/
//! - Project: Skills in ./.syscity/skills/ (current project)
//! - Workspace: Skills in workspace root

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use tracing::{debug, info, warn};

/// Skill storage levels
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum StorageLevel {
    /// Built-in skills (highest priority for availability)
    Bundled,
    /// User-level skills in ~/.syscity/skills/
    #[default]
    User,
    /// Workspace-level skills
    Workspace,
    /// Project-level skills in ./.syscity/skills/ (highest override priority)
    Project,
}

impl std::fmt::Display for StorageLevel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.name())
    }
}

impl StorageLevel {
    /// Get the priority (lower = higher priority for loading)
    pub fn priority(&self) -> u8 {
        match self {
            StorageLevel::Bundled => 0,
            StorageLevel::User => 1,
            StorageLevel::Workspace => 2,
            StorageLevel::Project => 3,
        }
    }

    /// Get display name
    pub fn name(&self) -> &'static str {
        match self {
            StorageLevel::Bundled => "bundled",
            StorageLevel::User => "user",
            StorageLevel::Workspace => "workspace",
            StorageLevel::Project => "project",
        }
    }
}

/// Skill location info
#[derive(Debug, Clone)]
pub struct SkillLocation {
    /// Storage level
    pub level: StorageLevel,
    /// Path to the skill directory
    pub path: PathBuf,
    /// Skill name (directory name)
    pub name: String,
    /// Path to SKILL.md file
    pub skill_file: PathBuf,
}

/// Multi-level skill storage
pub struct SkillStorage {
    /// Bundled skills directory
    bundled_dir: Option<PathBuf>,
    /// User skills directory
    user_dir: PathBuf,
    /// Project skills directory (./.syscity/skills/)
    project_dir: Option<PathBuf>,
    /// Workspace skills directory
    workspace_dir: Option<PathBuf>,
}

impl SkillStorage {
    /// Create a new skill storage instance rooted at `paths`.
    pub fn new(paths: std::sync::Arc<crate::dirs::SyscityPaths>) -> crate::Result<Self> {
        let user_dir = paths.skills_dir();

        Ok(Self {
            bundled_dir: Self::bundled_skills_dir(),
            user_dir,
            project_dir: Self::project_skills_dir(),
            workspace_dir: Self::workspace_skills_dir(),
        })
    }

    /// Get the bundled skills directory
    fn bundled_skills_dir() -> Option<PathBuf> {
        // Try to find bundled skills relative to executable
        std::env::current_exe()
            .ok()
            .and_then(|exe| exe.parent().map(|p| p.join("skills")))
            .or_else(|| {
                // Fallback: check for skills in source tree during development
                Some(PathBuf::from("./skills"))
            })
            .filter(|p| p.exists())
    }

    /// Get the project skills directory (./.syscity/skills/)
    fn project_skills_dir() -> Option<PathBuf> {
        std::env::current_dir()
            .ok()
            .map(|cwd| cwd.join(".syscity").join("skills"))
            .filter(|p| p.exists())
    }

    /// Get the workspace skills directory
    fn workspace_skills_dir() -> Option<PathBuf> {
        // Look for workspace root marker
        let cwd = std::env::current_dir().ok()?;
        let mut current = cwd.as_path();

        loop {
            // Check for workspace markers
            let markers = [".syscity-workspace", ".git", "syscity.workspace.toml"];
            for marker in &markers {
                if current.join(marker).exists() {
                    let workspace_skills = current.join(".syscity").join("skills");
                    if workspace_skills.exists()
                        && workspace_skills != cwd.join(".syscity").join("skills")
                    {
                        return Some(workspace_skills);
                    }
                }
            }

            // Go up one level
            match current.parent() {
                Some(parent) => current = parent,
                None => break,
            }
        }

        None
    }

    /// Ensure user skills directory exists
    pub async fn ensure_user_dir(&self) -> crate::Result<()> {
        tokio::fs::create_dir_all(&self.user_dir)
            .await
            .map_err(crate::error::SyscityError::Io)?;
        Ok(())
    }

    /// Storage scoped to an explicit user skills directory, with no bundled /
    /// project / workspace levels.
    ///
    /// Used by the connector subsystem (which installs bundled skills into a
    /// caller-chosen location) and by tests that must not touch `~/.syscity`.
    pub fn with_user_dir(user_dir: PathBuf) -> Self {
        Self {
            bundled_dir: None,
            user_dir,
            project_dir: None,
            workspace_dir: None,
        }
    }

    /// Ensure project skills directory exists
    pub async fn ensure_project_dir(&self) -> crate::Result<PathBuf> {
        let dir = std::env::current_dir()
            .map_err(crate::error::SyscityError::Io)?
            .join(".syscity")
            .join("skills");

        tokio::fs::create_dir_all(&dir)
            .await
            .map_err(crate::error::SyscityError::Io)?;

        Ok(dir)
    }

    /// Get the path for a skill at a specific level
    pub fn skill_path(&self, name: &str, level: StorageLevel) -> Option<PathBuf> {
        let base = match level {
            StorageLevel::Bundled => self.bundled_dir.as_ref()?,
            StorageLevel::User => &self.user_dir,
            StorageLevel::Project => self.project_dir.as_ref()?,
            StorageLevel::Workspace => self.workspace_dir.as_ref()?,
        };

        Some(base.join(name))
    }

    /// Get the SKILL.md path for a skill
    pub fn skill_file_path(&self, name: &str, level: StorageLevel) -> Option<PathBuf> {
        self.skill_path(name, level).map(|p| p.join("SKILL.md"))
    }

    /// Discover all skills across all levels
    pub async fn discover_all(&self) -> Vec<SkillLocation> {
        let mut skills = Vec::new();
        let mut seen = std::collections::HashSet::new();

        // Discover in order of priority (highest first so overrides work)
        for level in [
            StorageLevel::Project,
            StorageLevel::Workspace,
            StorageLevel::User,
            StorageLevel::Bundled,
        ] {
            let discovered = self.discover_at_level(level).await;

            for skill in discovered {
                // Higher priority levels override lower ones
                if seen.insert(skill.name.clone()) {
                    debug!("Found skill '{}' at {:?} level", skill.name, level);
                    skills.push(skill);
                } else {
                    debug!("Skill '{}' at {:?} level overrides lower priority", skill.name, level);
                }
            }
        }

        skills
    }

    /// Discover skills at a specific level
    pub async fn discover_at_level(&self, level: StorageLevel) -> Vec<SkillLocation> {
        let base_dir = match level {
            StorageLevel::Bundled => match &self.bundled_dir {
                Some(d) => d.clone(),
                None => return Vec::new(),
            },
            StorageLevel::User => self.user_dir.clone(),
            StorageLevel::Project => match &self.project_dir {
                Some(d) => d.clone(),
                None => return Vec::new(),
            },
            StorageLevel::Workspace => match &self.workspace_dir {
                Some(d) => d.clone(),
                None => return Vec::new(),
            },
        };

        if !base_dir.exists() {
            return Vec::new();
        }

        let mut skills = Vec::new();

        match tokio::fs::read_dir(&base_dir).await {
            Ok(mut entries) => {
                while let Ok(Some(entry)) = entries.next_entry().await {
                    let path = entry.path();

                    if path.is_dir() {
                        let skill_file = path.join("SKILL.md");

                        if skill_file.exists() {
                            let name = path
                                .file_name()
                                .and_then(|n| n.to_str())
                                .unwrap_or("unknown")
                                .to_string();

                            skills.push(SkillLocation { level, path, name, skill_file });
                        }
                    }
                }
            }
            Err(e) => {
                warn!("Failed to read skills directory {:?}: {}", base_dir, e);
            }
        }

        skills
    }

    /// Install a skill from a directory to user level
    pub async fn install_to_user(&self, source_dir: &Path, name: &str) -> crate::Result<PathBuf> {
        let dest = self.user_dir.join(name);

        info!("Installing skill '{}' from {:?} to {:?}", name, source_dir, dest);

        // Remove existing if present
        if dest.exists() {
            tokio::fs::remove_dir_all(&dest)
                .await
                .map_err(crate::error::SyscityError::Io)?;
        }

        // Copy directory
        copy_dir_recursive(source_dir, &dest).await?;

        info!("Successfully installed skill '{}' to {:?}", name, dest);
        Ok(dest)
    }

    /// Remove a skill from user level
    pub async fn uninstall_from_user(&self, name: &str) -> crate::Result<()> {
        let path = self.user_dir.join(name);

        if !path.exists() {
            return Err(crate::error::SyscityError::NotFound {
                resource: format!("Skill '{}' not found at {:?}", name, path),
            });
        }

        tokio::fs::remove_dir_all(&path)
            .await
            .map_err(crate::error::SyscityError::Io)?;

        info!("Uninstalled skill '{}' from {:?}", name, path);
        Ok(())
    }

    /// Get the storage level for a skill
    pub async fn get_skill_level(&self, name: &str) -> Option<StorageLevel> {
        // Check in priority order
        for level in [
            StorageLevel::Project,
            StorageLevel::Workspace,
            StorageLevel::User,
            StorageLevel::Bundled,
        ] {
            if let Some(path) = self.skill_file_path(name, level) {
                if path.exists() {
                    return Some(level);
                }
            }
        }
        None
    }

    /// Get the user skills directory path
    pub fn user_dir(&self) -> &Path {
        &self.user_dir
    }

    /// Get all storage directory paths
    pub fn get_all_paths(&self) -> Vec<(StorageLevel, PathBuf)> {
        let mut paths = Vec::new();

        if let Some(ref dir) = self.bundled_dir {
            paths.push((StorageLevel::Bundled, dir.clone()));
        }
        paths.push((StorageLevel::User, self.user_dir.clone()));
        if let Some(ref dir) = self.workspace_dir {
            paths.push((StorageLevel::Workspace, dir.clone()));
        }
        if let Some(ref dir) = self.project_dir {
            paths.push((StorageLevel::Project, dir.clone()));
        }

        paths
    }

    /// List all skills with their levels
    pub async fn list_with_levels(&self) -> HashMap<String, StorageLevel> {
        let mut map = HashMap::new();

        for level in [
            StorageLevel::Bundled,
            StorageLevel::User,
            StorageLevel::Workspace,
            StorageLevel::Project,
        ] {
            let skills = self.discover_at_level(level).await;
            for skill in skills {
                // Higher priority levels override
                map.insert(skill.name, level);
            }
        }

        map
    }

    /// Get all storage directories
    pub fn all_dirs(&self) -> Vec<(StorageLevel, PathBuf)> {
        let mut dirs = Vec::new();

        if let Some(ref d) = self.bundled_dir {
            dirs.push((StorageLevel::Bundled, d.clone()));
        }
        dirs.push((StorageLevel::User, self.user_dir.clone()));
        if let Some(ref d) = self.workspace_dir {
            dirs.push((StorageLevel::Workspace, d.clone()));
        }
        if let Some(ref d) = self.project_dir {
            dirs.push((StorageLevel::Project, d.clone()));
        }

        dirs
    }

    /// Refresh project and workspace directories (useful after cwd change)
    pub fn refresh(&mut self) {
        self.project_dir = Self::project_skills_dir();
        self.workspace_dir = Self::workspace_skills_dir();
    }
}

/// Copy directory recursively
async fn copy_dir_recursive(src: &Path, dst: &Path) -> crate::Result<()> {
    tokio::fs::create_dir_all(dst)
        .await
        .map_err(crate::error::SyscityError::Io)?;

    let mut entries = tokio::fs::read_dir(src)
        .await
        .map_err(crate::error::SyscityError::Io)?;

    while let Ok(Some(entry)) = entries.next_entry().await {
        let src_path = entry.path();
        let dst_path = dst.join(entry.file_name());

        if src_path.is_dir() {
            Box::pin(copy_dir_recursive(&src_path, &dst_path)).await?;
        } else {
            tokio::fs::copy(&src_path, &dst_path)
                .await
                .map_err(crate::error::SyscityError::Io)?;
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use super::*;

    static CWD_LOCK: Mutex<()> = Mutex::new(());

    /// RAII guard to restore the original working directory on drop.
    struct CwdGuard {
        original: PathBuf,
    }

    impl CwdGuard {
        fn new() -> Self {
            Self {
                original: std::env::current_dir().unwrap(),
            }
        }
    }

    impl Drop for CwdGuard {
        fn drop(&mut self) {
            let _ = std::env::set_current_dir(&self.original);
        }
    }

    #[test]
    fn test_storage_level_priority() {
        assert_eq!(StorageLevel::Bundled.priority(), 0);
        assert_eq!(StorageLevel::User.priority(), 1);
        assert_eq!(StorageLevel::Workspace.priority(), 2);
        assert_eq!(StorageLevel::Project.priority(), 3);
    }

    #[test]
    fn test_storage_level_name() {
        assert_eq!(StorageLevel::Bundled.name(), "bundled");
        assert_eq!(StorageLevel::User.name(), "user");
        assert_eq!(StorageLevel::Workspace.name(), "workspace");
        assert_eq!(StorageLevel::Project.name(), "project");
    }

    #[test]
    fn test_storage_level_default() {
        assert_eq!(StorageLevel::default(), StorageLevel::User);
    }

    #[test]
    fn test_storage_level_display() {
        assert_eq!(format!("{}", StorageLevel::Bundled), "bundled");
        assert_eq!(format!("{}", StorageLevel::User), "user");
        assert_eq!(format!("{}", StorageLevel::Workspace), "workspace");
        assert_eq!(format!("{}", StorageLevel::Project), "project");
    }

    #[test]
    fn test_user_skills_dir() {
        let dir = crate::dirs::paths().skills_dir();
        assert!(dir.to_string_lossy().contains("syscity"));
        assert!(dir.to_string_lossy().contains("skills"));
    }

    #[test]
    fn test_skill_location_debug() {
        let loc = SkillLocation {
            level: StorageLevel::User,
            path: PathBuf::from("/skills/docker"),
            name: "docker".to_string(),
            skill_file: PathBuf::from("/skills/docker/SKILL.md"),
        };
        let debug = format!("{:?}", loc);
        assert!(debug.contains("docker"));
        assert!(debug.contains("User"));
    }

    #[test]
    fn test_skill_location_clone() {
        let loc = SkillLocation {
            level: StorageLevel::User,
            path: PathBuf::from("/skills/docker"),
            name: "docker".to_string(),
            skill_file: PathBuf::from("/skills/docker/SKILL.md"),
        };
        let cloned = loc.clone();
        assert_eq!(cloned.name, "docker");
        assert_eq!(cloned.level, StorageLevel::User);
        assert_eq!(cloned.path, PathBuf::from("/skills/docker"));
        assert_eq!(cloned.skill_file, PathBuf::from("/skills/docker/SKILL.md"));
    }

    #[test]
    fn test_skill_storage_new() {
        let storage = SkillStorage::new(crate::dirs::paths());
        assert!(storage.is_ok());
    }

    #[test]
    fn test_skill_storage_default() {
        let storage = SkillStorage::new(crate::dirs::paths()).expect("skill storage");
        assert!(!storage.user_dir().as_os_str().is_empty());
    }

    #[test]
    fn test_skill_path() {
        let temp = tempfile::tempdir().unwrap();
        let storage = SkillStorage {
            bundled_dir: Some(temp.path().join("bundled")),
            user_dir: temp.path().join("user"),
            project_dir: Some(temp.path().join("project")),
            workspace_dir: Some(temp.path().join("workspace")),
        };

        assert_eq!(
            storage.skill_path("docker", StorageLevel::Bundled),
            Some(temp.path().join("bundled").join("docker"))
        );
        assert_eq!(
            storage.skill_path("docker", StorageLevel::User),
            Some(temp.path().join("user").join("docker"))
        );
        assert_eq!(
            storage.skill_path("docker", StorageLevel::Project),
            Some(temp.path().join("project").join("docker"))
        );
        assert_eq!(
            storage.skill_path("docker", StorageLevel::Workspace),
            Some(temp.path().join("workspace").join("docker"))
        );
    }

    #[test]
    fn test_skill_path_missing() {
        let temp = tempfile::tempdir().unwrap();
        let storage = SkillStorage {
            bundled_dir: None,
            user_dir: temp.path().join("user"),
            project_dir: None,
            workspace_dir: None,
        };

        assert_eq!(storage.skill_path("docker", StorageLevel::Bundled), None);
        assert_eq!(
            storage.skill_path("docker", StorageLevel::User),
            Some(temp.path().join("user").join("docker"))
        );
        assert_eq!(storage.skill_path("docker", StorageLevel::Project), None);
        assert_eq!(storage.skill_path("docker", StorageLevel::Workspace), None);
    }

    #[test]
    fn test_skill_file_path() {
        let temp = tempfile::tempdir().unwrap();
        let storage = SkillStorage {
            bundled_dir: Some(temp.path().join("bundled")),
            user_dir: temp.path().join("user"),
            project_dir: None,
            workspace_dir: None,
        };

        assert_eq!(
            storage.skill_file_path("docker", StorageLevel::Bundled),
            Some(temp.path().join("bundled").join("docker").join("SKILL.md"))
        );
        assert_eq!(
            storage.skill_file_path("docker", StorageLevel::User),
            Some(temp.path().join("user").join("docker").join("SKILL.md"))
        );
        assert_eq!(storage.skill_file_path("docker", StorageLevel::Project), None);
    }

    #[test]
    fn test_get_all_paths() {
        let temp = tempfile::tempdir().unwrap();
        let storage = SkillStorage {
            bundled_dir: Some(temp.path().join("bundled")),
            user_dir: temp.path().join("user"),
            project_dir: Some(temp.path().join("project")),
            workspace_dir: Some(temp.path().join("workspace")),
        };

        let paths = storage.get_all_paths();
        assert_eq!(paths.len(), 4);
        assert!(paths.iter().any(|(l, _)| *l == StorageLevel::Bundled));
        assert!(paths.iter().any(|(l, _)| *l == StorageLevel::User));
        assert!(paths.iter().any(|(l, _)| *l == StorageLevel::Workspace));
        assert!(paths.iter().any(|(l, _)| *l == StorageLevel::Project));
    }

    #[test]
    fn test_all_dirs() {
        let temp = tempfile::tempdir().unwrap();
        let storage = SkillStorage {
            bundled_dir: None,
            user_dir: temp.path().join("user"),
            project_dir: None,
            workspace_dir: None,
        };

        let dirs = storage.all_dirs();
        assert_eq!(dirs.len(), 1);
        assert_eq!(dirs[0].0, StorageLevel::User);
    }

    #[test]
    fn test_user_dir() {
        let temp = tempfile::tempdir().unwrap();
        let storage = SkillStorage {
            bundled_dir: None,
            user_dir: temp.path().join("user"),
            project_dir: None,
            workspace_dir: None,
        };

        assert_eq!(storage.user_dir(), temp.path().join("user"));
    }

    #[test]
    fn test_refresh_updates_project_dir() {
        let _lock = CWD_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let _guard = CwdGuard::new();
        let temp = tempfile::tempdir().unwrap();
        let temp_path = temp.path().canonicalize().unwrap();

        let syscity_skills = temp_path.join(".syscity").join("skills");
        std::fs::create_dir_all(&syscity_skills).unwrap();
        std::env::set_current_dir(&temp_path).unwrap();

        let mut storage = SkillStorage {
            bundled_dir: None,
            user_dir: temp_path.join("user"),
            project_dir: None,
            workspace_dir: None,
        };

        storage.refresh();
        assert_eq!(storage.project_dir, Some(syscity_skills));
    }

    #[tokio::test]
    async fn test_copy_dir_recursive() {
        let temp = tempfile::tempdir().unwrap();
        let src = temp.path().join("src");
        let dst = temp.path().join("dst");

        std::fs::create_dir_all(&src).unwrap();
        std::fs::write(src.join("file.txt"), "hello").unwrap();
        std::fs::create_dir_all(src.join("subdir")).unwrap();
        std::fs::write(src.join("subdir").join("nested.txt"), "world").unwrap();

        copy_dir_recursive(&src, &dst).await.unwrap();

        assert!(dst.exists());
        assert!(dst.join("file.txt").exists());
        assert_eq!(std::fs::read_to_string(dst.join("file.txt")).unwrap(), "hello");
        assert!(dst.join("subdir").exists());
        assert!(dst.join("subdir").join("nested.txt").exists());
        assert_eq!(
            std::fs::read_to_string(dst.join("subdir").join("nested.txt")).unwrap(),
            "world"
        );
    }

    #[tokio::test]
    async fn test_ensure_user_dir() {
        let temp = tempfile::tempdir().unwrap();
        let storage = SkillStorage {
            bundled_dir: None,
            user_dir: temp.path().join("user").join("skills"),
            project_dir: None,
            workspace_dir: None,
        };

        storage.ensure_user_dir().await.unwrap();
        assert!(temp.path().join("user").join("skills").exists());
    }

    #[tokio::test]
    async fn test_ensure_project_dir() {
        let _lock = CWD_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let _guard = CwdGuard::new();
        let temp = tempfile::tempdir().unwrap();
        let temp_path = temp.path().canonicalize().unwrap();

        std::env::set_current_dir(&temp_path).unwrap();

        let storage = SkillStorage {
            bundled_dir: None,
            user_dir: temp.path().join("user"),
            project_dir: None,
            workspace_dir: None,
        };

        let dir = storage.ensure_project_dir().await.unwrap();
        assert!(dir.exists());
        assert!(dir.to_string_lossy().contains(".syscity"));
        assert!(dir.to_string_lossy().contains("skills"));
    }

    #[tokio::test]
    async fn test_install_to_user() {
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("source_skill");
        std::fs::create_dir_all(&source).unwrap();
        std::fs::write(source.join("SKILL.md"), "# Test Skill").unwrap();

        let user_dir = temp.path().join("user");
        let storage = SkillStorage {
            bundled_dir: None,
            user_dir: user_dir.clone(),
            project_dir: None,
            workspace_dir: None,
        };

        let dest = storage
            .install_to_user(&source, "test_skill")
            .await
            .unwrap();
        assert_eq!(dest, user_dir.join("test_skill"));
        assert!(dest.exists());
        assert!(dest.join("SKILL.md").exists());
        assert_eq!(std::fs::read_to_string(dest.join("SKILL.md")).unwrap(), "# Test Skill");
    }

    #[tokio::test]
    async fn test_install_to_user_overwrite() {
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("source_skill");
        std::fs::create_dir_all(&source).unwrap();
        std::fs::write(source.join("SKILL.md"), "# New Version").unwrap();

        let user_dir = temp.path().join("user");
        let existing = user_dir.join("test_skill");
        std::fs::create_dir_all(&existing).unwrap();
        std::fs::write(existing.join("SKILL.md"), "# Old Version").unwrap();

        let storage = SkillStorage {
            bundled_dir: None,
            user_dir: user_dir.clone(),
            project_dir: None,
            workspace_dir: None,
        };

        let dest = storage
            .install_to_user(&source, "test_skill")
            .await
            .unwrap();
        assert_eq!(std::fs::read_to_string(dest.join("SKILL.md")).unwrap(), "# New Version");
    }

    #[tokio::test]
    async fn test_uninstall_from_user() {
        let temp = tempfile::tempdir().unwrap();
        let user_dir = temp.path().join("user");
        let skill_dir = user_dir.join("test_skill");
        std::fs::create_dir_all(&skill_dir).unwrap();
        std::fs::write(skill_dir.join("SKILL.md"), "# Test").unwrap();

        let storage = SkillStorage {
            bundled_dir: None,
            user_dir: user_dir.clone(),
            project_dir: None,
            workspace_dir: None,
        };

        storage.uninstall_from_user("test_skill").await.unwrap();
        assert!(!skill_dir.exists());
    }

    #[tokio::test]
    async fn test_uninstall_not_found() {
        let temp = tempfile::tempdir().unwrap();
        let storage = SkillStorage {
            bundled_dir: None,
            user_dir: temp.path().join("user"),
            project_dir: None,
            workspace_dir: None,
        };

        let result = storage.uninstall_from_user("nonexistent").await;
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(err.contains("not found"));
    }

    #[tokio::test]
    async fn test_discover_at_level() {
        let temp = tempfile::tempdir().unwrap();
        let user_dir = temp.path().join("user");

        // Create valid skill
        let docker_dir = user_dir.join("docker");
        std::fs::create_dir_all(&docker_dir).unwrap();
        std::fs::write(docker_dir.join("SKILL.md"), "# Docker").unwrap();

        // Create dir without SKILL.md
        let empty_dir = user_dir.join("empty");
        std::fs::create_dir_all(&empty_dir).unwrap();

        // Create file (not dir)
        std::fs::write(user_dir.join("not_a_skill"), "").unwrap();

        let storage = SkillStorage {
            bundled_dir: None,
            user_dir: user_dir.clone(),
            project_dir: None,
            workspace_dir: None,
        };

        let skills = storage.discover_at_level(StorageLevel::User).await;
        assert_eq!(skills.len(), 1);
        assert_eq!(skills[0].name, "docker");
        assert_eq!(skills[0].level, StorageLevel::User);
        assert_eq!(skills[0].skill_file, docker_dir.join("SKILL.md"));
    }

    #[tokio::test]
    async fn test_discover_at_level_missing_dir() {
        let temp = tempfile::tempdir().unwrap();
        let storage = SkillStorage {
            bundled_dir: None,
            user_dir: temp.path().join("nonexistent"),
            project_dir: None,
            workspace_dir: None,
        };

        let skills = storage.discover_at_level(StorageLevel::User).await;
        assert!(skills.is_empty());
    }

    #[tokio::test]
    async fn test_discover_at_level_none() {
        let temp = tempfile::tempdir().unwrap();
        let storage = SkillStorage {
            bundled_dir: None,
            user_dir: temp.path().join("user"),
            project_dir: None,
            workspace_dir: None,
        };

        // Project dir is None
        let skills = storage.discover_at_level(StorageLevel::Project).await;
        assert!(skills.is_empty());
    }

    #[tokio::test]
    async fn test_discover_all_override() {
        let temp = tempfile::tempdir().unwrap();

        let user_dir = temp.path().join("user");
        let docker_user = user_dir.join("docker");
        std::fs::create_dir_all(&docker_user).unwrap();
        std::fs::write(docker_user.join("SKILL.md"), "# User Docker").unwrap();

        let project_dir = temp.path().join("project");
        let docker_project = project_dir.join("docker");
        std::fs::create_dir_all(&docker_project).unwrap();
        std::fs::write(docker_project.join("SKILL.md"), "# Project Docker").unwrap();

        let storage = SkillStorage {
            bundled_dir: None,
            user_dir: user_dir.clone(),
            project_dir: Some(project_dir.clone()),
            workspace_dir: None,
        };

        let skills = storage.discover_all().await;
        assert_eq!(skills.len(), 1);
        assert_eq!(skills[0].name, "docker");
        // Project overrides user, so path should be project path
        assert_eq!(skills[0].path, docker_project);
        assert_eq!(skills[0].level, StorageLevel::Project);
    }

    #[tokio::test]
    async fn test_discover_all_multiple_skills() {
        let temp = tempfile::tempdir().unwrap();

        let user_dir = temp.path().join("user");
        let docker_dir = user_dir.join("docker");
        std::fs::create_dir_all(&docker_dir).unwrap();
        std::fs::write(docker_dir.join("SKILL.md"), "# Docker").unwrap();

        let k8s_dir = user_dir.join("k8s");
        std::fs::create_dir_all(&k8s_dir).unwrap();
        std::fs::write(k8s_dir.join("SKILL.md"), "# K8s").unwrap();

        let storage = SkillStorage {
            bundled_dir: None,
            user_dir: user_dir.clone(),
            project_dir: None,
            workspace_dir: None,
        };

        let skills = storage.discover_all().await;
        assert_eq!(skills.len(), 2);
        let names: Vec<_> = skills.iter().map(|s| s.name.as_str()).collect();
        assert!(names.contains(&"docker"));
        assert!(names.contains(&"k8s"));
    }

    #[tokio::test]
    async fn test_get_skill_level() {
        let temp = tempfile::tempdir().unwrap();
        let user_dir = temp.path().join("user");
        let docker_dir = user_dir.join("docker");
        std::fs::create_dir_all(&docker_dir).unwrap();
        std::fs::write(docker_dir.join("SKILL.md"), "# Docker").unwrap();

        let storage = SkillStorage {
            bundled_dir: None,
            user_dir: user_dir.clone(),
            project_dir: None,
            workspace_dir: None,
        };

        let level = storage.get_skill_level("docker").await;
        assert_eq!(level, Some(StorageLevel::User));

        let level = storage.get_skill_level("nonexistent").await;
        assert_eq!(level, None);
    }

    #[tokio::test]
    async fn test_list_with_levels() {
        let temp = tempfile::tempdir().unwrap();
        let user_dir = temp.path().join("user");
        let docker_dir = user_dir.join("docker");
        std::fs::create_dir_all(&docker_dir).unwrap();
        std::fs::write(docker_dir.join("SKILL.md"), "# Docker").unwrap();

        let k8s_dir = user_dir.join("k8s");
        std::fs::create_dir_all(&k8s_dir).unwrap();
        std::fs::write(k8s_dir.join("SKILL.md"), "# K8s").unwrap();

        let storage = SkillStorage {
            bundled_dir: None,
            user_dir: user_dir.clone(),
            project_dir: None,
            workspace_dir: None,
        };

        let map = storage.list_with_levels().await;
        assert_eq!(map.len(), 2);
        assert_eq!(map.get("docker"), Some(&StorageLevel::User));
        assert_eq!(map.get("k8s"), Some(&StorageLevel::User));
    }
}
