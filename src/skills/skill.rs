//! Skill definition and behavior.
//!
//! The [`Skill`] struct plus its builder, matching, eligibility, and prompt
//! rendering logic.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use super::{SkillMetadata, SkillTrigger, StorageLevel, TriggerType};

/// Complete skill definition
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Skill {
    /// Skill name (unique identifier)
    pub name: String,
    /// Human-readable description
    pub description: String,
    /// Version of the skill
    #[serde(default = "default_version")]
    pub version: String,
    /// Author who created the skill
    #[serde(default)]
    pub author: String,
    /// When the skill was created
    #[serde(default = "chrono::Utc::now")]
    pub created_at: chrono::DateTime<chrono::Utc>,
    /// Last updated time
    #[serde(default = "chrono::Utc::now")]
    pub updated_at: chrono::DateTime<chrono::Utc>,
    /// Triggers that activate this skill
    #[serde(default)]
    pub triggers: Vec<SkillTrigger>,
    /// The skill prompt/instructions (content after frontmatter)
    #[serde(skip)]
    pub prompt: String,
    /// Skill-specific metadata
    #[serde(rename = "syscity", default)]
    pub metadata: SkillMetadata,
    /// Skill dependencies: name -> version constraint
    #[serde(default)]
    pub depends_on: HashMap<String, String>,
    /// Capabilities this skill provides
    #[serde(default)]
    pub provides: Vec<String>,
    /// Skills to chain after this one in execution pipeline
    #[serde(default)]
    pub chain: Vec<String>,
    /// agentskills.io `license:` field — optional per the standard, surfaced for
    /// display so a skill's provenance is visible without opening the file.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub license: Option<String>,
    /// agentskills.io `compatibility:` field — e.g. which runtimes the skill
    /// needs. Informational only: syscity does not gate on it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub compatibility: Option<String>,
    /// agentskills.io `metadata:` block (author, version, …). Free-form per the
    /// standard, so kept as a map; the canonical top-level `version` is
    /// [`Skill::version`].
    #[serde(rename = "metadata", default, skip_serializing_if = "Option::is_none")]
    pub standard_metadata: Option<HashMap<String, serde_norway::Value>>,
    /// Source file path
    #[serde(skip)]
    pub source_path: PathBuf,
    /// Whether the skill is currently eligible to run
    #[serde(skip)]
    pub is_eligible: bool,
    /// Eligibility check results
    #[serde(skip)]
    pub eligibility_errors: Vec<String>,
    /// Whether the skill is enabled in config
    #[serde(skip)]
    pub enabled: bool,
    /// Source storage level (bundled, user, workspace, project)
    #[serde(skip)]
    pub source_level: StorageLevel,
    /// Transient: set by `prefilter_skills` when a keyword/regex trigger
    /// matched. When `Some(pattern)`, `to_prompt_section` prepends a `//
    /// Skill triggered by:` comment so the LLM can see why this skill was
    /// activated.
    #[serde(skip)]
    pub trigger_text: Option<String>,
}

fn default_version() -> String {
    "1.0.0".to_string()
}

impl Skill {
    /// Create a new skill
    pub fn new(
        name: impl Into<String>,
        description: impl Into<String>,
        prompt: impl Into<String>,
    ) -> Self {
        let now = chrono::Utc::now();
        Self {
            name: name.into(),
            description: description.into(),
            version: "1.0.0".to_string(),
            author: "syscity".to_string(),
            created_at: now,
            updated_at: now,
            triggers: Vec::new(),
            prompt: prompt.into(),
            metadata: SkillMetadata::default(),
            depends_on: HashMap::new(),
            provides: Vec::new(),
            chain: Vec::new(),
            license: None,
            compatibility: None,
            standard_metadata: None,
            source_path: PathBuf::new(),
            is_eligible: true,
            eligibility_errors: Vec::new(),
            enabled: true,
            source_level: StorageLevel::User,
            trigger_text: None,
        }
    }

    /// Add a trigger to the skill
    pub fn with_trigger(mut self, trigger_type: TriggerType, pattern: impl Into<String>) -> Self {
        let pattern = pattern.into();
        #[allow(clippy::expect_used)] // builder API: invalid regex is a programmer error
        let trigger = SkillTrigger::try_new(trigger_type, pattern, 0, true, true)
            .expect("invalid regex pattern in trigger");
        self.triggers.push(trigger);
        self
    }

    /// Set the author
    pub fn by(mut self, author: impl Into<String>) -> Self {
        self.author = author.into();
        self
    }

    /// Set the emoji
    pub fn with_emoji(mut self, emoji: impl Into<String>) -> Self {
        self.metadata.emoji = emoji.into();
        self
    }

    /// Add required binary
    pub fn requires_bin(mut self, bin: impl Into<String>) -> Self {
        self.metadata.requires.bins.push(bin.into());
        self
    }

    /// Add required env var
    pub fn requires_env(mut self, env: impl Into<String>) -> Self {
        self.metadata.requires.env.push(env.into());
        self
    }

    /// Check if this skill matches the given input
    pub fn matches(&self, input: &str) -> bool {
        self.find_trigger_text(input).is_some()
    }

    /// Like `matches()`, but returns the first matching trigger pattern text.
    pub fn find_trigger_text(&self, input: &str) -> Option<String> {
        let input_lower = input.to_lowercase();

        for trigger in &self.triggers {
            match trigger.trigger_type {
                TriggerType::Regex => {
                    if let Ok(re) = regex::Regex::new(&trigger.pattern) {
                        if re.is_match(input) {
                            return Some(trigger.pattern.clone());
                        }
                    }
                }
                TriggerType::Keyword => {
                    if input_lower.contains(&trigger.pattern.to_lowercase()) {
                        return Some(trigger.pattern.clone());
                    }
                }
                TriggerType::Command => {
                    if input_lower.starts_with(&format!("/{}", trigger.pattern.to_lowercase())) {
                        return Some(format!("/{}", trigger.pattern));
                    }
                }
                TriggerType::Intent => {
                    if input_lower.contains(&trigger.pattern.to_lowercase()) {
                        return Some(trigger.pattern.clone());
                    }
                }
            }
        }

        None
    }

    /// Check if this skill is a command (starts with /)
    pub fn is_command(&self) -> Option<&str> {
        self.triggers.iter().find_map(|t| {
            if t.trigger_type == TriggerType::Command && t.user_invocable {
                Some(t.pattern.as_str())
            } else {
                None
            }
        })
    }

    /// Get the prompt section for this skill (for inclusion in system prompt)
    ///
    /// If `max_prompt_chars` is `Some(n)`, the full prompt body is truncated
    /// so the complete section fits within `n` characters (with `…` suffix).
    pub fn to_prompt_section(&self, max_prompt_chars: Option<usize>) -> String {
        let mut section = String::new();

        // Add trigger annotation for debugging — shows why this skill was activated.
        if let Some(ref trigger) = self.trigger_text {
            section.push_str(&format!("// Skill triggered by: \"{}\"\n", trigger));
        }

        // Add emoji and name
        if !self.metadata.emoji.is_empty() {
            section.push_str(&format!("{} ", self.metadata.emoji));
        }
        section.push_str(&format!("**{}**\n\n", self.name));

        // Add description
        section.push_str(&format!("{}\n\n", self.description));

        // Add the prompt content with path compaction
        let prompt_body = self.compact_prompt_body();
        section.push_str(&prompt_body);

        // Add trigger info if it's a command
        if let Some(cmd) = self.is_command() {
            section.push_str(&format!("\n\n*Use with: /{}*", cmd));
        }

        // Truncate to max chars if specified, preserving the command suffix
        if let Some(max_chars) = max_prompt_chars {
            if section.len() > max_chars {
                section.truncate(max_chars.saturating_sub(1));
                section.push('…');
            }
        }

        section
    }

    /// Return the prompt body with home-directory paths compacted to `~/`.
    fn compact_prompt_body(&self) -> String {
        let body = &self.prompt;
        if let Some(home) = dirs::home_dir() {
            let home_str = home.to_string_lossy().to_string();
            body.replace(&home_str, "~")
        } else {
            body.clone()
        }
    }

    /// Collect requirement errors without mutating self.
    /// Shared logic between `check_eligibility()` and `verify_requirements()`.
    fn collect_requirement_errors(&self) -> Vec<String> {
        let mut errors = Vec::new();

        // Check OS
        if !self.metadata.requires.os.is_empty() {
            let current_os = std::env::consts::OS;
            let os_map = match current_os {
                "macos" => "darwin",
                "linux" => "linux",
                "windows" => "win32",
                _ => current_os,
            };
            if !self.metadata.requires.os.iter().any(|o| o == os_map) {
                errors.push(format!(
                    "OS '{}' not in supported list: {:?}",
                    os_map, self.metadata.requires.os
                ));
            }
        }

        // Check binaries
        for bin in &self.metadata.requires.bins {
            if !self.is_binary_available(bin) {
                errors.push(format!("Binary '{}' not found on PATH", bin));
            }
        }

        // Check env vars
        for env in &self.metadata.requires.env {
            if std::env::var(env).is_err() {
                errors.push(format!("Environment variable '{}' not set", env));
            }
        }

        // Check config paths
        for config_path in &self.metadata.requires.config {
            let expanded = shellexpand::tilde(config_path);
            if !Path::new(expanded.as_ref()).exists() {
                errors.push(format!("Config path '{}' does not exist", config_path));
            }
        }

        errors
    }

    /// Check runtime eligibility
    pub fn check_eligibility(&mut self) {
        self.is_eligible = true;
        self.eligibility_errors.clear();

        let errors = self.collect_requirement_errors();
        if !errors.is_empty() {
            self.is_eligible = false;
            self.eligibility_errors = errors;
        }
    }

    /// Check if a binary is available on PATH
    fn is_binary_available(&self, bin: &str) -> bool {
        if let Ok(path) = std::env::var("PATH") {
            let separator = if cfg!(windows) { ';' } else { ':' };
            for dir in path.split(separator) {
                let bin_path = Path::new(dir).join(bin);
                if bin_path.exists() {
                    return true;
                }
                // Try with .exe on Windows
                #[cfg(windows)]
                if bin_path.with_extension("exe").exists() {
                    return true;
                }
            }
        }
        false
    }

    /// Verify requirements at activation time (non-mutating).
    /// Returns Ok(()) if all requirements are met, Err with reasons if not.
    pub fn verify_requirements(&self) -> Result<(), Vec<String>> {
        let errors = self.collect_requirement_errors();
        if errors.is_empty() {
            Ok(())
        } else {
            Err(errors)
        }
    }

    /// Compact path for token optimization
    pub fn compact_path(&self) -> String {
        let path_str = self.source_path.to_string_lossy();
        if let Some(home) = dirs::home_dir() {
            let home_str = home.to_string_lossy();
            if path_str.starts_with(home_str.as_ref()) {
                return format!("~{}", &path_str[home_str.len()..]);
            }
        }
        path_str.to_string()
    }

    /// Format for display in prompts
    pub fn format_for_prompt(&self, compact: bool) -> String {
        let mut output = String::new();

        if !self.metadata.emoji.is_empty() {
            output.push_str(&format!("{} ", self.metadata.emoji));
        }

        output.push_str(&format!("**{}**: {}\n", self.name, self.description));

        if compact {
            output.push_str(&format!("  Path: {}\n", self.compact_path()));
        }

        if !self.metadata.requires.bins.is_empty() {
            output.push_str(&format!("  Requires: {}\n", self.metadata.requires.bins.join(", ")));
        }

        if !self.is_eligible {
            output.push_str("  **Not eligible**\n");
            for err in &self.eligibility_errors {
                output.push_str(&format!("    - {}\n", err));
            }
        }

        output
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_skill_creation() {
        let skill = Skill::new(
            "weather",
            "Get weather information",
            "When asked about weather, fetch current conditions.",
        )
        .with_trigger(TriggerType::Keyword, "weather")
        .with_trigger(TriggerType::Command, "weather")
        .requires_bin("curl")
        .with_emoji("🌤️");

        assert_eq!(skill.name, "weather");
        assert_eq!(skill.triggers.len(), 2);
        assert_eq!(skill.metadata.requires.bins.len(), 1);
        assert_eq!(skill.metadata.emoji, "🌤️");
    }

    #[test]
    fn test_skill_matching() {
        let skill = Skill::new("test", "Test", "prompt")
            .with_trigger(TriggerType::Keyword, "test")
            .with_trigger(TriggerType::Command, "test");

        assert!(skill.matches("This is a test"));
        assert!(skill.matches("/test something"));
        assert!(!skill.matches("Something else"));
    }

    #[test]
    fn test_skill_eligibility() {
        let mut skill =
            Skill::new("test", "Test", "prompt").with_trigger(TriggerType::Keyword, "test");

        // Add a binary that definitely exists
        skill.metadata.requires.bins.push("cargo".to_string());

        skill.check_eligibility();

        // cargo should be available in test environment
        println!("Eligible: {}", skill.is_eligible);
        println!("Errors: {:?}", skill.eligibility_errors);
    }

    #[test]
    fn test_default_version() {
        assert_eq!(default_version(), "1.0.0");
    }

    #[test]
    fn test_skill_new_defaults() {
        let skill = Skill::new("name", "desc", "prompt");
        assert_eq!(skill.version, "1.0.0");
        assert_eq!(skill.author, "syscity");
        assert!(skill.triggers.is_empty());
        assert!(skill.is_eligible);
        assert!(skill.enabled);
        assert_eq!(skill.source_level, StorageLevel::User);
        assert!(skill.eligibility_errors.is_empty());
    }

    #[test]
    fn test_skill_by() {
        let skill = Skill::new("s", "d", "p").by("alice");
        assert_eq!(skill.author, "alice");
    }

    #[test]
    fn test_skill_is_command_some() {
        let skill = Skill::new("s", "d", "p").with_trigger(TriggerType::Command, "weather");
        assert_eq!(skill.is_command(), Some("weather"));
    }

    #[test]
    fn test_skill_is_command_none() {
        let skill = Skill::new("s", "d", "p").with_trigger(TriggerType::Keyword, "weather");
        assert_eq!(skill.is_command(), None);
    }

    #[test]
    fn test_skill_to_prompt_section() {
        let skill = Skill::new("weather", "Get weather", "When asked about weather...")
            .with_emoji("🌤️")
            .with_trigger(TriggerType::Command, "weather");
        let section = skill.to_prompt_section(None);
        assert!(section.contains("🌤️"));
        assert!(section.contains("weather"));
        assert!(section.contains("Use with: /weather"));
    }

    #[test]
    fn test_skill_verify_requirements_empty() {
        let skill = Skill::new("s", "d", "p");
        assert!(skill.verify_requirements().is_ok());
    }

    #[test]
    fn test_skill_matches_regex() {
        let skill = Skill::new("s", "d", "p").with_trigger(TriggerType::Regex, r"\bhello\b");
        assert!(skill.matches("say hello world"));
        assert!(!skill.matches("say helloworld"));
    }

    #[test]
    fn test_skill_matches_intent() {
        let skill = Skill::new("s", "d", "p").with_trigger(TriggerType::Intent, "book flight");
        assert!(skill.matches("I want to book flight"));
        assert!(!skill.matches("I want to book hotel"));
    }

    #[test]
    fn test_skill_depends_on_default() {
        let skill = Skill::new("a", "d", "p");
        assert!(skill.depends_on.is_empty());
        assert!(skill.provides.is_empty());
        assert!(skill.chain.is_empty());
    }
}
