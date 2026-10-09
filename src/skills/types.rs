//! Skill trigger and metadata types.
//!
//! [`TriggerType`], [`SkillTrigger`], [`SkillRequires`], and [`SkillMetadata`]
//! describe how a skill is activated and what it needs at runtime.

use serde::{Deserialize, Serialize};

use super::SkillInstallSpec;

/// Types of skill triggers
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TriggerType {
    /// Regex pattern match on user input
    Regex,
    /// Exact keyword match
    Keyword,
    /// Intent classification
    Intent,
    /// Command prefix (e.g., "/weather")
    Command,
}

/// A trigger that activates a skill
#[derive(Debug, Clone, Serialize)]
pub struct SkillTrigger {
    /// Trigger type
    #[serde(rename = "type")]
    pub trigger_type: TriggerType,
    /// The pattern or condition
    pub pattern: String,
    /// Priority (higher = checked first)
    #[serde(default)]
    pub priority: i32,
    /// Whether this trigger is user-invocable as a command
    #[serde(default = "default_true")]
    pub user_invocable: bool,
    /// Whether the model can invoke this skill
    #[serde(default = "default_true")]
    pub model_invocable: bool,
}

impl SkillTrigger {
    /// Create a new `SkillTrigger`, validating the pattern when `trigger_type`
    /// is `Regex`.
    pub fn try_new(
        trigger_type: TriggerType,
        pattern: String,
        priority: i32,
        user_invocable: bool,
        model_invocable: bool,
    ) -> Result<Self, String> {
        if trigger_type == TriggerType::Regex {
            regex::Regex::new(&pattern)
                .map_err(|e| format!("Invalid regex pattern '{}': {}", pattern, e))?;
        }
        Ok(Self {
            trigger_type,
            pattern,
            priority,
            user_invocable,
            model_invocable,
        })
    }

    /// A slash-command trigger for the given name: user-invocable, and never
    /// keyword-matched against conversation text.
    ///
    /// Used to make an agentskills.io-standard skill (which declares no
    /// triggers at all) loadable: it must not start firing on conversation
    /// keywords it never claimed, but a user typing `/<name>` expects it.
    pub fn command(name: impl Into<String>) -> Self {
        Self {
            trigger_type: TriggerType::Command,
            pattern: name.into(),
            priority: 0,
            user_invocable: true,
            model_invocable: true,
        }
    }
}

impl<'de> Deserialize<'de> for SkillTrigger {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(Deserialize)]
        struct SkillTriggerHelper {
            #[serde(rename = "type")]
            trigger_type: TriggerType,
            pattern: String,
            #[serde(default)]
            priority: i32,
            #[serde(default = "default_true")]
            user_invocable: bool,
            #[serde(default = "default_true")]
            model_invocable: bool,
        }

        let helper = SkillTriggerHelper::deserialize(deserializer)?;
        if helper.trigger_type == TriggerType::Regex {
            regex::Regex::new(&helper.pattern).map_err(|e| {
                serde::de::Error::custom(format!(
                    "invalid regex pattern '{}': {}",
                    helper.pattern, e
                ))
            })?;
        }
        Ok(SkillTrigger {
            trigger_type: helper.trigger_type,
            pattern: helper.pattern,
            priority: helper.priority,
            user_invocable: helper.user_invocable,
            model_invocable: helper.model_invocable,
        })
    }
}

fn default_true() -> bool {
    true
}

/// Runtime requirements for a skill
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SkillRequires {
    /// Required binaries on PATH
    #[serde(default)]
    pub bins: Vec<String>,
    /// Required environment variables
    #[serde(default)]
    pub env: Vec<String>,
    /// Required config paths that must be truthy
    #[serde(default)]
    pub config: Vec<String>,
    /// Supported operating systems (darwin, linux, win32)
    #[serde(default)]
    pub os: Vec<String>,
}

/// Skill metadata
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SkillMetadata {
    /// Display emoji
    #[serde(default)]
    pub emoji: String,
    /// Whether to always include this skill
    #[serde(default)]
    pub always: bool,
    /// Runtime requirements
    #[serde(default)]
    pub requires: SkillRequires,
    /// Installation specifications
    #[serde(default)]
    pub install: Vec<SkillInstallSpec>,
    /// Override key for config lookup
    #[serde(rename = "skillKey", default)]
    pub skill_key: Option<String>,
    /// Primary environment variable for API keys
    #[serde(rename = "primaryEnv", default)]
    pub primary_env: Option<String>,
    /// Maximum skill file size in bytes (default: 256KB)
    #[serde(rename = "maxSize", default = "default_max_size")]
    pub max_size: usize,
    /// Trust level for this skill.
    ///
    /// Community-trust skills restrict the agent to read-only (non-privileged)
    /// tools so mixing a community skill with a trusted one doesn't escalate
    /// privileges.
    #[serde(default)]
    pub trust: crate::tools::SkillTrust,
}

impl Default for SkillMetadata {
    fn default() -> Self {
        Self {
            emoji: String::new(),
            always: false,
            requires: SkillRequires::default(),
            install: Vec::new(),
            skill_key: None,
            primary_env: None,
            max_size: default_max_size(),
            trust: crate::tools::SkillTrust::Trusted,
        }
    }
}

fn default_max_size() -> usize {
    256_000
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_trigger_type_variants() {
        assert_eq!(TriggerType::Regex, TriggerType::Regex);
        assert_eq!(TriggerType::Keyword, TriggerType::Keyword);
        assert_eq!(TriggerType::Intent, TriggerType::Intent);
        assert_eq!(TriggerType::Command, TriggerType::Command);
        assert_ne!(TriggerType::Regex, TriggerType::Keyword);
    }

    #[test]
    fn test_skill_requires_default() {
        let req = SkillRequires::default();
        assert!(req.bins.is_empty());
        assert!(req.env.is_empty());
        assert!(req.config.is_empty());
        assert!(req.os.is_empty());
    }

    #[test]
    fn test_skill_metadata_default() {
        let meta = SkillMetadata::default();
        assert_eq!(meta.emoji, "");
        assert!(!meta.always);
        assert_eq!(meta.max_size, 256_000);
        assert!(meta.skill_key.is_none());
        assert!(meta.primary_env.is_none());
    }

    #[test]
    fn test_default_max_size() {
        assert_eq!(default_max_size(), 256_000);
    }

    #[test]
    fn test_skill_trigger_defaults() {
        let trigger = SkillTrigger {
            trigger_type: TriggerType::Keyword,
            pattern: "test".to_string(),
            priority: 0,
            user_invocable: true,
            model_invocable: true,
        };
        assert!(trigger.user_invocable);
        assert!(trigger.model_invocable);
    }
}
