//! SKILL.md frontmatter parser
//!
//! Parses YAML frontmatter from SKILL.md files.
//!
//! # Format Example
//! ```yaml
//! ---
//! name: weather
//! emoji: 🌤️
//! always: false
//! requires:
//! bins:
//! - curl
//! env:
//! - OPENWEATHER_API_KEY
//! config:
//! - ~/.weather/config.toml
//! install:
//! - action: download
//! binary: weather
//! from: https://example.com/weather-cli
//! - action: npm
//! package: @weather/cli
//! trigger:
//! user_invocable: true
//! model_invocable: true
//! slash:
//! name: weather
//! description: Get weather information
//! ---
//! ```

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

/// Parsed SKILL.md content with frontmatter and body
#[derive(Debug, Clone, Default)]
pub struct SkillFile {
    /// YAML frontmatter metadata
    pub frontmatter: SkillFrontmatter,
    /// Markdown content after frontmatter
    pub content: String,
    /// Raw file path
    pub path: std::path::PathBuf,
}

/// SKILL.md YAML frontmatter structure
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct SkillFrontmatter {
    /// Skill name (identifier)
    pub name: String,

    /// Display emoji
    #[serde(default)]
    pub emoji: String,

    /// Whether to always include in prompt
    #[serde(default)]
    pub always: bool,

    /// Runtime requirements
    #[serde(default)]
    pub requires: RequiresConfig,

    /// Installation specifications
    #[serde(default)]
    pub install: Vec<InstallSpec>,

    /// Trigger configuration
    #[serde(default)]
    pub trigger: TriggerConfig,

    /// Slash command configuration
    #[serde(default)]
    pub slash: Option<SlashConfig>,

    /// Skill version
    #[serde(default = "default_version")]
    pub version: String,

    /// Human-readable description
    #[serde(default)]
    pub description: String,

    /// Author who created the skill
    #[serde(default)]
    pub author: String,

    /// Triggers that activate this skill
    #[serde(default)]
    pub triggers: Vec<SkillTriggerItem>,

    /// Skill-specific metadata (emoji, category, tags, etc.)
    #[serde(rename = "syscity", default)]
    pub syscity: SyscityMetadata,

    /// Skill dependencies: name -> version constraint
    /// Example: `{ "weather": "^1.0.0", "base-utils": ">=2.0.0" }`
    #[serde(default)]
    pub depends_on: HashMap<String, String>,

    /// Capabilities this skill provides
    #[serde(default)]
    pub provides: Vec<String>,

    /// Skills to chain after this one (execution pipeline)
    #[serde(default)]
    pub chain: Vec<String>,

    /// Custom configuration values
    #[serde(flatten)]
    pub extra: HashMap<String, serde_norway::Value>,
}

fn default_version() -> String {
    "1.0.0".to_string()
}

/// Single trigger item in the triggers array
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SkillTriggerItem {
    /// Trigger type: command, keyword, regex, intent
    #[serde(rename = "type")]
    pub trigger_type: String,
    /// The pattern to match
    pub pattern: String,
    /// Priority (higher = checked first)
    #[serde(default)]
    pub priority: i32,
}

/// Nested `syscity` metadata block in SKILL.md frontmatter
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct SyscityMetadata {
    /// Display emoji override
    #[serde(default)]
    pub emoji: String,
    /// Category for organization
    #[serde(default)]
    pub category: String,
    /// Tags for filtering
    #[serde(default)]
    pub tags: Vec<String>,
    /// Runtime requirements declared under the syscity block.
    /// Built-in skills declare `requires` inside `syscity:` rather than
    /// at the top level, so we need this field to capture them.
    #[serde(default)]
    pub requires: RequiresConfig,
}

/// Runtime requirements configuration
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct RequiresConfig {
    /// Required binaries
    #[serde(default)]
    pub bins: Vec<String>,

    /// Required environment variables
    #[serde(default)]
    pub env: Vec<String>,

    /// Required config files (paths that must exist)
    #[serde(default)]
    pub config: Vec<String>,

    /// Allowed operating systems
    #[serde(default)]
    pub os: Vec<String>,
}

/// Installation specification
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "lowercase")]
pub enum InstallSpec {
    /// Homebrew installation
    Brew {
        package: String,
        #[serde(default)]
        tap: Option<String>,
        #[serde(default)]
        binary: Option<String>,
    },

    /// NPM package installation
    Npm {
        package: String,
        #[serde(default)]
        global: bool,
        #[serde(default)]
        binary: Option<String>,
    },

    /// Go tool installation
    Go {
        package: String,
        #[serde(default)]
        binary: Option<String>,
    },

    /// Python/uv tool installation
    Uv {
        package: String,
        #[serde(default)]
        binary: Option<String>,
    },

    /// Direct download
    Download {
        binary: String,
        from: String,
        #[serde(default)]
        extract: Option<String>, // tar.gz, zip, etc.
    },

    /// Shell command
    Shell {
        command: String,
        #[serde(default)]
        binary: Option<String>,
    },

    /// Cargo installation
    Cargo {
        package: String,
        #[serde(default)]
        binary: Option<String>,
    },
}

/// Trigger configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TriggerConfig {
    /// Can be invoked by user via slash command
    #[serde(default = "default_true")]
    pub user_invocable: bool,

    /// Can be invoked by the AI model
    #[serde(default = "default_true")]
    pub model_invocable: bool,
}

impl Default for TriggerConfig {
    fn default() -> Self {
        Self {
            user_invocable: true,
            model_invocable: true,
        }
    }
}

/// Slash command configuration
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct SlashConfig {
    /// Command name (e.g., "weather")
    pub name: String,

    /// Short description
    pub description: String,

    /// Arguments schema
    #[serde(default)]
    pub args: Vec<SlashArg>,
}

/// Slash command argument
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SlashArg {
    pub name: String,
    pub description: String,
    #[serde(default)]
    pub required: bool,
    #[serde(default)]
    pub default: Option<String>,
}

fn default_true() -> bool {
    true
}

impl SkillFile {
    /// Parse a SKILL.md file from content
    pub fn parse(content: &str, path: std::path::PathBuf) -> crate::Result<Self> {
        let (frontmatter, body) = Self::extract_frontmatter(content)?;

        Ok(Self {
            frontmatter,
            content: body.to_string(),
            path,
        })
    }

    /// Load and parse a SKILL.md file from disk
    pub async fn load(path: &std::path::Path) -> crate::Result<Self> {
        let content = tokio::fs::read_to_string(path)
            .await
            .map_err(crate::error::SyscityError::Io)?;

        Self::parse(&content, path.to_path_buf())
    }

    /// Extract YAML frontmatter from markdown content
    fn extract_frontmatter(content: &str) -> crate::Result<(SkillFrontmatter, &str)> {
        // Look for frontmatter between --- markers
        let trimmed = content.trim_start();

        if !trimmed.starts_with("---") {
            // No frontmatter - return default and full content as body
            return Ok((SkillFrontmatter::default(), content));
        }

        // Find the end of frontmatter (second ---)
        let after_first = &trimmed[3..]; // Skip first ---

        if let Some(end_pos) = after_first.find("\n---") {
            let yaml_content = &after_first[..end_pos];
            let body = &after_first[end_pos + 4..]; // Skip \n---

            // Parse YAML frontmatter
            let frontmatter: SkillFrontmatter =
                serde_norway::from_str(yaml_content).map_err(|e| {
                    crate::error::SyscityError::Config(crate::error::ConfigError::Parse(format!(
                        "Failed to parse SKILL.md frontmatter: {}",
                        e
                    )))
                })?;

            Ok((frontmatter, body.trim_start()))
        } else {
            // No closing --- found - treat as no frontmatter
            Ok((SkillFrontmatter::default(), content))
        }
    }

    /// Get the skill name from frontmatter or filename
    pub fn skill_name(&self) -> String {
        if !self.frontmatter.name.is_empty() {
            self.frontmatter.name.clone()
        } else {
            // Extract from filename (SKILL.md -> parent dir name)
            self.path
                .parent()
                .and_then(|p| p.file_name())
                .and_then(|n| n.to_str())
                .unwrap_or("unknown")
                .to_string()
        }
    }

    /// Get the prompt text for this skill
    pub fn to_prompt(&self) -> String {
        let mut prompt = String::new();

        // Add emoji if present
        if !self.frontmatter.emoji.is_empty() {
            prompt.push_str(&self.frontmatter.emoji);
            prompt.push(' ');
        }

        // Add name
        prompt.push_str(&format!("**{}**\n\n", self.skill_name()));

        // Add content
        prompt.push_str(&self.content);

        prompt
    }
}

/// Parse SKILL.md content into frontmatter YAML and prompt body.
/// Returns (frontmatter_yaml, prompt_body).
pub fn parse_skill_md(content: &str) -> crate::Result<(String, String)> {
    let trimmed = content.trim_start();

    if !trimmed.starts_with("---") {
        // No frontmatter - return empty frontmatter and full content as body
        return Ok(("---\n---".to_string(), content.to_string()));
    }

    // Find the end of frontmatter
    let after_first = &trimmed[3..];

    if let Some(end_pos) = after_first.find("\n---") {
        let yaml_content = &after_first[..end_pos];
        let body = &after_first[end_pos + 4..];

        let frontmatter_yaml = format!("---{}\n---", yaml_content);
        Ok((frontmatter_yaml, body.trim_start().to_string()))
    } else {
        // No closing --- found
        Ok(("---\n---".to_string(), content.to_string()))
    }
}

/// Format a skill as SKILL.md content.
/// Note: This is a simplified version that formats based on the Skill struct
/// from mod.rs. The actual implementation would need access to the Skill struct
/// definition.
pub fn format_skill_md(name: &str, description: &str, prompt: &str, emoji: &str) -> String {
    let mut content = String::new();

    // Frontmatter
    content.push_str("---\n");
    content.push_str(&format!("name: {}\n", name));
    content.push_str(&format!("emoji: {}\n", emoji));
    content.push_str("---\n\n");

    // Content
    content.push_str(&format!("# {}\n\n", description));
    content.push_str(prompt);

    content
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_basic_frontmatter() {
        let content = r#"---
name: weather
emoji: 🌤️
always: false
---
# Weather Skill

Get weather information for any location.
"#;

        let skill =
            SkillFile::parse(content, std::path::PathBuf::from("weather/SKILL.md")).unwrap();

        assert_eq!(skill.frontmatter.name, "weather");
        assert_eq!(skill.frontmatter.emoji, "🌤️");
        assert!(!skill.frontmatter.always);
        assert!(skill.content.contains("Get weather information"));
    }

    #[test]
    fn test_parse_with_requires() {
        let content = r#"---
name: k8s
requires:
  bins:
    - kubectl
    - helm
  env:
    - KUBECONFIG
  os:
    - linux
    - macos
---
Kubernetes management skill.
"#;

        let skill = SkillFile::parse(content, std::path::PathBuf::from("k8s/SKILL.md")).unwrap();

        assert_eq!(skill.frontmatter.requires.bins, vec!["kubectl", "helm"]);
        assert_eq!(skill.frontmatter.requires.env, vec!["KUBECONFIG"]);
        assert_eq!(skill.frontmatter.requires.os, vec!["linux", "macos"]);
    }

    #[test]
    fn test_parse_install_specs() {
        let content = r#"---
name: tool
install:
  - action: brew
    package: tool-cli
  - action: npm
    package: "@org/tool"
    global: true
  - action: download
    binary: tool
    from: https://example.com/tool.tar.gz
---
Tool skill.
"#;

        let skill = SkillFile::parse(content, std::path::PathBuf::from("tool/SKILL.md")).unwrap();

        assert_eq!(skill.frontmatter.install.len(), 3);

        match &skill.frontmatter.install[0] {
            InstallSpec::Brew { package, .. } => assert_eq!(package, "tool-cli"),
            _ => panic!("Expected Brew install spec"),
        }

        match &skill.frontmatter.install[1] {
            InstallSpec::Npm { package, global, .. } => {
                assert_eq!(package, "@org/tool");
                assert!(*global);
            }
            _ => panic!("Expected Npm install spec"),
        }
    }

    #[test]
    fn test_no_frontmatter() {
        let content = "# Plain Skill\n\nNo frontmatter here.";

        let skill = SkillFile::parse(content, std::path::PathBuf::from("plain/SKILL.md")).unwrap();

        assert!(skill.frontmatter.name.is_empty());
        assert!(skill.content.contains("No frontmatter here"));
    }

    #[test]
    fn test_skill_name_from_path() {
        let content = "---\nname: docker\n---\nContent";
        let skill =
            SkillFile::parse(content, std::path::PathBuf::from("/skills/docker/SKILL.md")).unwrap();

        assert_eq!(skill.skill_name(), "docker");
    }

    #[test]
    fn test_to_prompt() {
        let content = r#"---
name: test
emoji: 🧪
---
Test content."#;

        let skill = SkillFile::parse(content, std::path::PathBuf::from("test/SKILL.md")).unwrap();
        let prompt = skill.to_prompt();

        assert!(prompt.contains("🧪"));
        assert!(prompt.contains("**test**"));
        assert!(prompt.contains("Test content"));
    }
}
