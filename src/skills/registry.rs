//! Skill Registry for Remote Skill Discovery and Installation
//!
//! Provides a client for discovering, installing, and managing skills
//! from a remote registry (e.g., skills.syscity.dev or ClawHub).

use std::path::PathBuf;

use reqwest;
use serde::{Deserialize, Serialize};
use tokio::fs;
use tracing::{debug, info, instrument, warn};

use crate::dirs;
use crate::error::{Result, SyscityError};
use crate::skills::frontmatter::SkillFile;

/// Skill listing from registry
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SkillListing {
    /// Skill name
    pub name: String,
    /// Human-readable description
    pub description: String,
    /// Author name
    pub author: String,
    /// Version string
    pub version: String,
    /// Download count
    #[serde(default)]
    pub downloads: u64,
    /// Rating (0.0 - 5.0)
    #[serde(default)]
    pub rating: f32,
    /// Categories
    #[serde(default)]
    pub categories: Vec<String>,
    /// Tags
    #[serde(default)]
    pub tags: Vec<String>,
    /// Emoji icon
    #[serde(default)]
    pub emoji: String,
}

/// Skill update information
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SkillUpdate {
    /// Skill name
    pub name: String,
    /// Current installed version
    pub current_version: String,
    /// Latest available version
    pub latest_version: String,
    /// Release notes
    pub release_notes: Option<String>,
}

/// Skill registry client
#[derive(Debug, Clone)]
pub struct SkillRegistry {
    /// Registry base URL
    url: String,
    /// HTTP client
    client: reqwest::Client,
    /// Layout root installed skills are written under.
    paths: std::sync::Arc<crate::dirs::SyscityPaths>,
}

impl SkillRegistry {
    /// Create a new skill registry client
    pub fn new(
        url: impl Into<String>,
        paths: std::sync::Arc<crate::dirs::SyscityPaths>,
    ) -> Result<Self> {
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(30))
            .user_agent(format!("syscity/{} (SkillRegistry)", env!("CARGO_PKG_VERSION")))
            .build()
            .map_err(|e| SyscityError::Internal(format!("Failed to create HTTP client: {}", e)))?;

        Ok(Self { url: url.into(), client, paths })
    }

    /// Create registry with default URL
    pub fn default_registry(paths: std::sync::Arc<crate::dirs::SyscityPaths>) -> Result<Self> {
        Self::new("https://skills.syscity.dev", paths)
    }

    /// Get the ClawHub registry
    pub fn clawhub(paths: std::sync::Arc<crate::dirs::SyscityPaths>) -> Result<Self> {
        Self::new("https://clawhub.syscity.dev", paths)
    }

    /// Search for skills in the registry
    #[instrument(skip(self))]
    pub async fn search(&self, query: &str) -> Result<Vec<SkillListing>> {
        let url = format!("{}/api/v1/skills/search?q={}", self.url, urlencoding::encode(query));

        debug!("Searching skills: {}", url);

        let response = self
            .client
            .get(&url)
            .send()
            .await
            .map_err(SyscityError::Http)?;

        if !response.status().is_success() {
            return Err(SyscityError::ExternalService {
                source: format!("Registry search failed: {}", response.status()),
                cause: None,
            });
        }

        let listings: Vec<SkillListing> = response.json().await.map_err(SyscityError::Http)?;

        info!("Found {} skills matching '{}'", listings.len(), query);
        Ok(listings)
    }

    /// Get popular skills from registry
    #[instrument(skip(self))]
    pub async fn list_popular(&self, limit: usize) -> Result<Vec<SkillListing>> {
        let url = format!("{}/api/v1/skills/popular?limit={}", self.url, limit);

        debug!("Fetching popular skills");

        let response = self
            .client
            .get(&url)
            .send()
            .await
            .map_err(SyscityError::Http)?;

        if !response.status().is_success() {
            return Err(SyscityError::ExternalService {
                source: format!("Failed to fetch popular skills: {}", response.status()),
                cause: None,
            });
        }

        let listings: Vec<SkillListing> = response.json().await.map_err(SyscityError::Http)?;

        Ok(listings)
    }

    /// Get skill details from registry
    #[instrument(skip(self))]
    pub async fn get_skill_info(&self, name: &str) -> Result<SkillListing> {
        let url = format!("{}/api/v1/skills/{}", self.url, name);

        debug!("Fetching skill info: {}", name);

        let response = self
            .client
            .get(&url)
            .send()
            .await
            .map_err(SyscityError::Http)?;

        if response.status() == reqwest::StatusCode::NOT_FOUND {
            return Err(SyscityError::NotFound {
                resource: format!("Skill '{}' not found in registry", name),
            });
        }

        if !response.status().is_success() {
            return Err(SyscityError::ExternalService {
                source: format!("Failed to fetch skill info: {}", response.status()),
                cause: None,
            });
        }

        let listing: SkillListing = response.json().await.map_err(SyscityError::Http)?;

        Ok(listing)
    }

    /// Install a skill from the registry
    #[instrument(skip(self))]
    pub async fn install(&self, name: &str) -> Result<PathBuf> {
        info!("Installing skill '{}' from registry", name);

        // Check if already installed
        let skill_dir = self.paths.skills_dir().join(name);

        if skill_dir.exists() {
            warn!("Skill '{}' already installed at {:?}", name, skill_dir);
            return Err(SyscityError::Validation(format!(
                "Skill '{}' is already installed. Use 'update' to update it.",
                name
            )));
        }

        // Download skill
        let url = format!("{}/api/v1/skills/{}/download", self.url, name);

        debug!("Downloading from: {}", url);

        let response = self
            .client
            .get(&url)
            .send()
            .await
            .map_err(SyscityError::Http)?;

        if response.status() == reqwest::StatusCode::NOT_FOUND {
            return Err(SyscityError::NotFound {
                resource: format!("Skill '{}' not found in registry", name),
            });
        }

        if !response.status().is_success() {
            return Err(SyscityError::ExternalService {
                source: format!("Failed to download skill: {}", response.status()),
                cause: None,
            });
        }

        let content = response.bytes().await.map_err(SyscityError::Http)?;

        // Validate that the downloaded content is a valid SKILL.md
        let content_str = std::str::from_utf8(&content).map_err(|_| {
            SyscityError::Validation("Downloaded skill is not valid UTF-8".to_string())
        })?;
        SkillFile::parse(content_str, PathBuf::from("SKILL.md")).map_err(|e| {
            SyscityError::Validation(format!("Downloaded skill is not a valid SKILL.md: {}", e))
        })?;

        // Enforce max file size (default 256KB)
        if content.len() > 256_000 {
            return Err(SyscityError::Validation(format!(
                "Downloaded skill too large: {} bytes (max: 256000)",
                content.len()
            )));
        }

        // Create skill directory
        fs::create_dir_all(&skill_dir)
            .await
            .map_err(SyscityError::Io)?;

        // Write SKILL.md
        let skill_file = skill_dir.join("SKILL.md");
        fs::write(&skill_file, content)
            .await
            .map_err(SyscityError::Io)?;

        info!("Skill '{}' installed to {:?}", name, skill_dir);
        Ok(skill_dir)
    }

    /// Install skill with specific version
    #[instrument(skip(self))]
    pub async fn install_version(&self, name: &str, version: &str) -> Result<PathBuf> {
        info!("Installing skill '{}' version '{}' from registry", name, version);

        let url = format!("{}/api/v1/skills/{}/download?version={}", self.url, name, version);

        let response = self
            .client
            .get(&url)
            .send()
            .await
            .map_err(SyscityError::Http)?;

        if !response.status().is_success() {
            return Err(SyscityError::ExternalService {
                source: format!("Failed to download skill version: {}", response.status()),
                cause: None,
            });
        }

        let content = response.bytes().await.map_err(SyscityError::Http)?;

        let skill_dir = self.paths.skills_dir().join(name);

        fs::create_dir_all(&skill_dir)
            .await
            .map_err(SyscityError::Io)?;

        let skill_file = skill_dir.join("SKILL.md");
        fs::write(&skill_file, content)
            .await
            .map_err(SyscityError::Io)?;

        info!("Skill '{}' v{} installed", name, version);
        Ok(skill_dir)
    }

    /// Update a skill to the latest version
    #[instrument(skip(self))]
    pub async fn update(&self, name: &str) -> Result<SkillUpdate> {
        info!("Updating skill '{}'", name);

        // Get current version
        let skill_dir = self.paths.skills_dir().join(name);

        if !skill_dir.exists() {
            return Err(SyscityError::NotFound {
                resource: format!("Skill '{}' is not installed", name),
            });
        }

        let skill_file = skill_dir.join("SKILL.md");
        let current_content = fs::read_to_string(&skill_file)
            .await
            .map_err(SyscityError::Io)?;

        // Parse current skill to get version
        let current_skill = SkillFile::parse(&current_content, skill_file.clone())
            .map_err(|e| SyscityError::Validation(format!("Failed to parse skill: {}", e)))?;

        let current_version = current_skill.frontmatter.version;

        // Check for updates
        let listing = self.get_skill_info(name).await?;

        if listing.version == current_version {
            info!("Skill '{}' is already up to date (v{})", name, current_version);
            return Ok(SkillUpdate {
                name: name.to_string(),
                current_version,
                latest_version: listing.version,
                release_notes: None,
            });
        }

        // Download and install update
        self.install_version(name, &listing.version).await?;

        info!("Skill '{}' updated from v{} to v{}", name, current_version, listing.version);

        Ok(SkillUpdate {
            name: name.to_string(),
            current_version,
            latest_version: listing.version,
            release_notes: None,
        })
    }

    /// Check for available updates for all installed skills
    #[instrument(skip(self))]
    pub async fn check_updates(&self) -> Result<Vec<SkillUpdate>> {
        debug!("Checking for skill updates");

        let skill_dir = self.paths.skills_dir();

        let mut updates = Vec::new();

        // List installed skills
        let mut entries = fs::read_dir(&skill_dir).await.map_err(SyscityError::Io)?;

        while let Some(entry) = entries.next_entry().await.map_err(SyscityError::Io)? {
            let path = entry.path();
            if path.is_dir() {
                let skill_file = path.join("SKILL.md");
                if skill_file.exists() {
                    if let Ok(content) = fs::read_to_string(&skill_file).await {
                        if let Ok(skill) = SkillFile::parse(&content, skill_file.clone()) {
                            let name = skill.frontmatter.name;

                            // Check registry for newer version
                            if let Ok(listing) = self.get_skill_info(&name).await {
                                if listing.version != skill.frontmatter.version {
                                    updates.push(SkillUpdate {
                                        name,
                                        current_version: skill.frontmatter.version,
                                        latest_version: listing.version,
                                        release_notes: None,
                                    });
                                }
                            }
                        }
                    }
                }
            }
        }

        info!("Found {} available updates", updates.len());
        Ok(updates)
    }

    /// List skills by category
    #[instrument(skip(self))]
    pub async fn list_by_category(&self, category: &str) -> Result<Vec<SkillListing>> {
        let url = format!("{}/api/v1/skills/category/{}", self.url, category);

        let response = self
            .client
            .get(&url)
            .send()
            .await
            .map_err(SyscityError::Http)?;

        if !response.status().is_success() {
            return Err(SyscityError::ExternalService {
                source: format!("Failed to list skills by category: {}", response.status()),
                cause: None,
            });
        }

        let listings: Vec<SkillListing> = response.json().await.map_err(SyscityError::Http)?;

        Ok(listings)
    }

    /// List skills by tag
    #[instrument(skip(self))]
    pub async fn list_by_tag(&self, tag: &str) -> Result<Vec<SkillListing>> {
        let url = format!("{}/api/v1/skills/tag/{}", self.url, tag);

        let response = self
            .client
            .get(&url)
            .send()
            .await
            .map_err(SyscityError::Http)?;

        if !response.status().is_success() {
            return Err(SyscityError::ExternalService {
                source: format!("Failed to list skills by tag: {}", response.status()),
                cause: None,
            });
        }

        let listings: Vec<SkillListing> = response.json().await.map_err(SyscityError::Http)?;

        Ok(listings)
    }

    /// Uninstall a skill
    #[instrument(skip(self))]
    pub async fn uninstall(&self, name: &str) -> Result<()> {
        info!("Uninstalling skill '{}'", name);

        let skill_dir = self.paths.skills_dir().join(name);

        if !skill_dir.exists() {
            return Err(SyscityError::NotFound {
                resource: format!("Skill '{}' is not installed", name),
            });
        }

        fs::remove_dir_all(&skill_dir)
            .await
            .map_err(SyscityError::Io)?;

        info!("Skill '{}' uninstalled", name);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_skill_registry_creation() {
        let registry = SkillRegistry::new("https://skills.example.com", crate::dirs::paths());
        assert!(registry.is_ok());

        let reg = registry.unwrap();
        assert_eq!(reg.url, "https://skills.example.com");
    }

    #[test]
    fn test_default_registry() {
        let registry = SkillRegistry::default_registry(crate::dirs::paths());
        assert!(registry.is_ok());
        let reg = registry.unwrap();
        assert_eq!(reg.url, "https://skills.syscity.dev");
    }

    #[test]
    fn test_clawhub_registry() {
        let registry = SkillRegistry::clawhub(crate::dirs::paths());
        assert!(registry.is_ok());
        let reg = registry.unwrap();
        assert_eq!(reg.url, "https://clawhub.syscity.dev");
    }

    #[test]
    fn test_skill_listing_serde() {
        let listing = SkillListing {
            name: "test-skill".to_string(),
            description: "A test skill".to_string(),
            author: "tester".to_string(),
            version: "1.0.0".to_string(),
            downloads: 42,
            rating: 4.5,
            categories: vec!["test".to_string()],
            tags: vec!["example".to_string()],
            emoji: "🧪".to_string(),
        };
        let json = serde_json::to_string(&listing).unwrap();
        assert!(json.contains("test-skill"));
        let restored: SkillListing = serde_json::from_str(&json).unwrap();
        assert_eq!(restored.name, "test-skill");
        assert_eq!(restored.downloads, 42);
        assert_eq!(restored.rating, 4.5);
    }

    #[test]
    fn test_skill_listing_default_fields() {
        let json = r#"{"name":"x","description":"y","author":"z","version":"1.0.0"}"#;
        let listing: SkillListing = serde_json::from_str(json).unwrap();
        assert_eq!(listing.downloads, 0);
        assert_eq!(listing.rating, 0.0);
        assert!(listing.categories.is_empty());
        assert!(listing.tags.is_empty());
        assert_eq!(listing.emoji, "");
    }

    #[test]
    fn test_skill_update_serde() {
        let update = SkillUpdate {
            name: "test-skill".to_string(),
            current_version: "1.0.0".to_string(),
            latest_version: "1.1.0".to_string(),
            release_notes: Some("Fixed bugs".to_string()),
        };
        let json = serde_json::to_string(&update).unwrap();
        let restored: SkillUpdate = serde_json::from_str(&json).unwrap();
        assert_eq!(restored.current_version, "1.0.0");
        assert_eq!(restored.latest_version, "1.1.0");
        assert_eq!(restored.release_notes, Some("Fixed bugs".to_string()));
    }

    #[test]
    fn test_skill_update_without_release_notes() {
        let json = r#"{"name":"x","current_version":"1.0.0","latest_version":"2.0.0"}"#;
        let update: SkillUpdate = serde_json::from_str(json).unwrap();
        assert_eq!(update.release_notes, None);
    }
}
