//! Structured SOUL.md configuration parser
//!
//! Implements config-as-code for agent personality.
//! SOUL.md files can contain YAML frontmatter with structured fields
//! (name, persona, voice, behavior, preferences) followed by free-form
//! markdown body.
//!
//! # Example SOUL.md
//!
//! ```markdown
//! ---
//! name: Syscity
//! persona: Helpful AI assistant with a curious edge
//! voice: concise, direct, no filler
//! emoji: "🦑"
//! behavior:
//! proactive: true
//! ask_before_destructive: true
//! group_chat_mode: smart
//! preferences:
//! language: en-US
//! code_style: rust
//! format: markdown
//! ---
//!
//! # Core Truths
//!
//! Be genuinely helpful, not performatively helpful...
//! ```

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

/// Structured agent behavior configuration.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct BehaviorConfig {
    /// Whether the agent should proactively suggest actions.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub proactive: Option<bool>,
    /// Whether to ask before destructive operations.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ask_before_destructive: Option<bool>,
    /// Group chat participation mode.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub group_chat_mode: Option<String>,
    /// Additional free-form behavior flags.
    #[serde(flatten)]
    pub extra: HashMap<String, serde_norway::Value>,
}

impl BehaviorConfig {
    /// Returns `true` when no behavior fields are set.
    pub fn is_empty(&self) -> bool {
        self.proactive.is_none()
            && self.ask_before_destructive.is_none()
            && self.group_chat_mode.is_none()
            && self.extra.is_empty()
    }
}

/// User preference configuration embedded in SOUL.md.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PreferenceConfig {
    /// Preferred language code (e.g. "en-US", "zh-CN").
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub language: Option<String>,
    /// Preferred code style conventions.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub code_style: Option<String>,
    /// Preferred response format.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub format: Option<String>,
    /// Additional free-form preferences.
    #[serde(flatten)]
    pub extra: HashMap<String, serde_norway::Value>,
}

impl PreferenceConfig {
    /// Returns `true` when no preference fields are set.
    pub fn is_empty(&self) -> bool {
        self.language.is_none()
            && self.code_style.is_none()
            && self.format.is_none()
            && self.extra.is_empty()
    }
}

/// Heuristic analysis of conversation patterns used to auto-populate SOUL.md.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SoulAnalysis {
    /// Detected preferred language code.
    pub detected_language: Option<String>,
    /// Detected dominant programming language or code style.
    pub detected_code_style: Option<String>,
    /// Detected assistant voice / tone.
    pub detected_voice: Option<String>,
    /// Common topics raised by the user.
    pub common_topics: Vec<String>,
    /// Explicit user preferences extracted from messages.
    pub user_preferences: HashMap<String, String>,
}

/// Parsed structured configuration from a SOUL.md frontmatter block.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SoulConfig {
    /// Agent name / call sign.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// Short persona description.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub persona: Option<String>,
    /// Voice / tone description.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub voice: Option<String>,
    /// Signature emoji.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub emoji: Option<String>,
    /// Structured behavior flags.
    #[serde(default, skip_serializing_if = "BehaviorConfig::is_empty")]
    pub behavior: BehaviorConfig,
    /// Structured preferences.
    #[serde(default, skip_serializing_if = "PreferenceConfig::is_empty")]
    pub preferences: PreferenceConfig,
    /// Extra top-level keys for forward compatibility.
    #[serde(flatten)]
    pub extra: HashMap<String, serde_norway::Value>,
}

impl SoulConfig {
    /// Returns `true` when no structured fields are set.
    pub fn is_empty(&self) -> bool {
        self.name.is_none()
            && self.persona.is_none()
            && self.voice.is_none()
            && self.emoji.is_none()
            && self.behavior.is_empty()
            && self.preferences.is_empty()
            && self.extra.is_empty()
    }

    /// Generate a structured system-prompt fragment from the config.
    ///
    /// Returns an empty string if no structured fields are set.
    pub fn to_prompt_fragment(&self) -> String {
        let mut parts = Vec::new();

        if let Some(name) = &self.name {
            parts.push(format!("**Name**: {}", name));
        }
        if let Some(persona) = &self.persona {
            parts.push(format!("**Persona**: {}", persona));
        }
        if let Some(voice) = &self.voice {
            parts.push(format!("**Voice**: {}", voice));
        }
        if let Some(emoji) = &self.emoji {
            parts.push(format!("**Emoji**: {}", emoji));
        }

        let mut behavior_parts = Vec::new();
        if let Some(v) = self.behavior.proactive {
            behavior_parts.push(format!("- Proactive: {}", v));
        }
        if let Some(v) = self.behavior.ask_before_destructive {
            behavior_parts.push(format!("- Ask before destructive actions: {}", v));
        }
        if let Some(v) = &self.behavior.group_chat_mode {
            behavior_parts.push(format!("- Group chat mode: {}", v));
        }
        for (k, v) in &self.behavior.extra {
            behavior_parts.push(format!("- {}: {}", k, yaml_to_string(v)));
        }
        if !behavior_parts.is_empty() {
            parts.push(format!("**Behavior**:\n{}", behavior_parts.join("\n")));
        }

        let mut pref_parts = Vec::new();
        if let Some(v) = &self.preferences.language {
            pref_parts.push(format!("- Language: {}", v));
        }
        if let Some(v) = &self.preferences.code_style {
            pref_parts.push(format!("- Code style: {}", v));
        }
        if let Some(v) = &self.preferences.format {
            pref_parts.push(format!("- Format: {}", v));
        }
        for (k, v) in &self.preferences.extra {
            pref_parts.push(format!("- {}: {}", k, yaml_to_string(v)));
        }
        if !pref_parts.is_empty() {
            parts.push(format!("**Preferences**:\n{}", pref_parts.join("\n")));
        }

        if parts.is_empty() {
            String::new()
        } else {
            format!("\n### Agent Profile\n\n{}\n", parts.join("\n\n"))
        }
    }

    /// Merge heuristic analysis into this config, only filling empty fields.
    ///
    /// Returns `true` if any field was changed.
    pub fn merge_analysis(&mut self, analysis: &SoulAnalysis) -> bool {
        let mut changed = false;

        if self.preferences.language.is_none() {
            if let Some(lang) = &analysis.detected_language {
                self.preferences.language = Some(lang.clone());
                changed = true;
            }
        }

        if self.preferences.code_style.is_none() {
            if let Some(style) = &analysis.detected_code_style {
                self.preferences.code_style = Some(style.clone());
                changed = true;
            }
        }

        if self.voice.is_none() {
            if let Some(voice) = &analysis.detected_voice {
                self.voice = Some(voice.clone());
                changed = true;
            }
        }

        if self.persona.is_none() && !analysis.common_topics.is_empty() {
            self.persona = Some(format!(
                "Helpful assistant interested in {}",
                analysis.common_topics.join(", ")
            ));
            changed = true;
        }

        for (key, value) in &analysis.user_preferences {
            if !self.preferences.extra.contains_key(key) {
                self.preferences
                    .extra
                    .insert(key.clone(), serde_norway::Value::String(value.clone()));
                changed = true;
            }
        }

        changed
    }
}

fn yaml_to_string(v: &serde_norway::Value) -> String {
    match v {
        serde_norway::Value::String(s) => s.clone(),
        serde_norway::Value::Number(n) => n.to_string(),
        serde_norway::Value::Bool(b) => b.to_string(),
        _ => serde_norway::to_string(v)
            .unwrap_or_default()
            .trim()
            .to_string(),
    }
}

/// A parsed SOUL.md file with optional structured frontmatter and markdown
/// body.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SoulFile {
    /// Structured configuration from YAML frontmatter (if present).
    pub config: SoulConfig,
    /// Free-form markdown body (everything after the frontmatter).
    pub body: String,
    /// Whether the file contained a valid frontmatter block.
    pub has_frontmatter: bool,
}

impl SoulFile {
    /// Parse raw SOUL.md content into structured config + body.
    pub fn parse(content: &str) -> crate::Result<Self> {
        let trimmed = content.trim_start();

        // Check for YAML frontmatter delimiter
        if !trimmed.starts_with("---") {
            return Ok(Self {
                config: SoulConfig::default(),
                body: content.to_string(),
                has_frontmatter: false,
            });
        }

        // Find the closing ---
        let after_open = &trimmed[3..]; // skip "---"
        let Some(close_idx) = after_open.find("\n---") else {
            // No closing delimiter — treat as body without frontmatter
            return Ok(Self {
                config: SoulConfig::default(),
                body: content.to_string(),
                has_frontmatter: false,
            });
        };

        let yaml_text = after_open[..close_idx].trim();
        let body_start = 3 + close_idx + 4; // "---" + close_idx + "\n---"
        let body = trimmed[body_start.min(trimmed.len())..]
            .trim_start()
            .to_string();

        let config: SoulConfig = serde_norway::from_str(yaml_text).map_err(|e| {
            crate::error::SyscityError::Validation(format!(
                "Failed to parse SOUL.md frontmatter: {}",
                e
            ))
        })?;

        Ok(Self {
            config,
            body,
            has_frontmatter: true,
        })
    }

    /// Serialize this SOUL file back to markdown (YAML frontmatter + body).
    ///
    /// The frontmatter is emitted whenever structured config fields are
    /// present (or `has_frontmatter` is set). Unset fields are omitted so the
    /// output stays clean and round-trips through [`SoulFile::parse`].
    pub fn to_markdown(&self) -> crate::Result<String> {
        let include_frontmatter = self.has_frontmatter || !self.config.is_empty();

        let mut out = String::new();
        if include_frontmatter {
            let yaml = serde_norway::to_string(&self.config).map_err(|e| {
                crate::error::SyscityError::Validation(format!(
                    "Failed to serialize SOUL.md frontmatter: {}",
                    e
                ))
            })?;
            let yaml = yaml.trim();
            out.push_str("---\n");
            if !yaml.is_empty() && yaml != "{}" {
                out.push_str(yaml);
                out.push('\n');
            }
            out.push_str("---\n");
        }

        let body = self.body.trim_end();
        if !body.is_empty() {
            if include_frontmatter {
                out.push('\n');
            }
            out.push_str(body);
            out.push('\n');
        }

        Ok(out)
    }

    /// Merge structured config prompt fragment + body into full prompt text.
    pub fn to_full_prompt(&self) -> String {
        let fragment = self.config.to_prompt_fragment();
        if self.body.is_empty() {
            return fragment;
        }
        if fragment.is_empty() {
            return self.body.clone();
        }
        format!("{}\n{}", fragment, self.body)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_soul_config_prompt_fragment() {
        let config = SoulConfig {
            name: Some("Syscity".to_string()),
            persona: Some("Helpful squid".to_string()),
            voice: Some("concise".to_string()),
            emoji: Some("🦑".to_string()),
            behavior: BehaviorConfig {
                proactive: Some(true),
                ask_before_destructive: Some(true),
                group_chat_mode: Some("smart".to_string()),
                extra: HashMap::new(),
            },
            preferences: PreferenceConfig {
                language: Some("zh-CN".to_string()),
                code_style: Some("rust".to_string()),
                format: Some("markdown".to_string()),
                extra: HashMap::new(),
            },
            extra: HashMap::new(),
        };

        let fragment = config.to_prompt_fragment();
        assert!(fragment.contains("**Name**: Syscity"));
        assert!(fragment.contains("**Persona**: Helpful squid"));
        assert!(fragment.contains("**Voice**: concise"));
        assert!(fragment.contains("**Emoji**: 🦑"));
        assert!(fragment.contains("Proactive: true"));
        assert!(fragment.contains("Group chat mode: smart"));
        assert!(fragment.contains("Language: zh-CN"));
        assert!(fragment.contains("Code style: rust"));
    }

    #[test]
    fn test_soul_file_parse_with_frontmatter() {
        let content = r#"---
name: Syscity
persona: Curious AI
voice: snarky
emoji: "🦑"
behavior:
  proactive: true
---

# Core Truths

Be helpful.
"#;

        let soul = SoulFile::parse(content).unwrap();
        assert!(soul.has_frontmatter);
        assert_eq!(soul.config.name, Some("Syscity".to_string()));
        assert_eq!(soul.config.persona, Some("Curious AI".to_string()));
        assert_eq!(soul.config.voice, Some("snarky".to_string()));
        assert_eq!(soul.config.emoji, Some("🦑".to_string()));
        assert_eq!(soul.config.behavior.proactive, Some(true));
        assert!(soul.body.contains("# Core Truths"));
        assert!(soul.body.contains("Be helpful."));
    }

    #[test]
    fn test_soul_file_parse_without_frontmatter() {
        let content = "# Just markdown\n\nNo frontmatter here.";
        let soul = SoulFile::parse(content).unwrap();
        assert!(!soul.has_frontmatter);
        assert_eq!(soul.config.name, None);
        assert_eq!(soul.body, content);
    }

    #[test]
    fn test_soul_file_parse_empty() {
        let soul = SoulFile::parse("").unwrap();
        assert!(!soul.has_frontmatter);
        assert!(soul.body.is_empty());
    }

    #[test]
    fn test_soul_file_to_full_prompt() {
        let soul = SoulFile {
            config: SoulConfig {
                name: Some("Test".to_string()),
                ..Default::default()
            },
            body: "# Body\n\nText.".to_string(),
            has_frontmatter: true,
        };
        let prompt = soul.to_full_prompt();
        assert!(prompt.contains("**Name**: Test"));
        assert!(prompt.contains("# Body"));
    }

    #[test]
    fn test_soul_file_invalid_frontmatter_falls_back() {
        // Opening --- but no closing ---
        let content = "---\nname: Syscity\n# No closing delimiter";
        let soul = SoulFile::parse(content).unwrap();
        assert!(!soul.has_frontmatter);
        assert_eq!(soul.config.name, None);
    }

    #[test]
    fn test_behavior_extra_fields() {
        let content = r#"---
behavior:
  proactive: true
  custom_flag: 42
---

Body.
"#;
        let soul = SoulFile::parse(content).unwrap();
        assert_eq!(soul.config.behavior.proactive, Some(true));
        assert!(soul.config.behavior.extra.contains_key("custom_flag"));
    }
}
