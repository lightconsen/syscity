//! Agent Personality Discovery and Loading
//!
//! This provides:
//! - Automatic discovery of agents from `agents/` directory
//! - Loading of personality files (SOUL.md, IDENTITY.md, BOOTSTRAP.md, USER.md)
//! - Personality-based AgentConfig generation
//! - Agent registry for on-demand spawning

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::LazyLock;

use regex::Regex;
use tokio::fs;
use tracing::{debug, info, warn};

use crate::agent::AgentConfig;
use crate::dirs::SyscityPaths;

/// Regex for matching placeholder headings that should not be used as display
/// names.
#[allow(clippy::expect_used)] // Static regex with a known-valid pattern.
static PLACEHOLDER_HEADING_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)(?:IDENTITY|SOUL|BOOTSTRAP|USER|AGENTS|TOOLS|HEARTBEAT|MEMORY)\.md|我是谁|身份信息|agent\s*identity")
        .expect("PLACEHOLDER_HEADING_RE is valid")
});

/// Regex for markdown list style name entries like `- **名称**: 小明`.
#[allow(clippy::expect_used)] // Static regex with a known-valid pattern.
static NAME_LIST_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)^\s*[-*]\s*\*\*\s*(?:名称|name|display\s*name)\s*\*\*\s*[:：]\s*(.+)\s*$")
        .expect("NAME_LIST_RE is valid")
});

/// Regex for YAML-style `name: 小明` entries.
#[allow(clippy::expect_used)] // Static regex with a known-valid pattern.
static NAME_YAML_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)^\s*name\s*[:：]\s*(.+?)\s*$").expect("NAME_YAML_RE is valid")
});

/// Regex for markdown list style emoji entries like `- **Emoji**: 🐼`.
#[allow(clippy::expect_used)] // Static regex with a known-valid pattern.
static EMOJI_LIST_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^\s*[-*]\s*\*\*\s*[Ee]moji\s*\*\*\s*[:：]\s*(.+)\s*$")
        .expect("EMOJI_LIST_RE is valid")
});

/// Maximum size for personality files (4KB default)
const DEFAULT_MAX_FILE_SIZE: usize = 4096;

/// Controls which personality files are included in the system prompt.
///
/// `Primary` produces the full prompt (Bootstrap + Identity + Soul + Agents +
/// Tools). `Subagent` omits Bootstrap and User — these contain startup-only
/// instructions that are irrelevant (and wasteful) for spawned subagents and
/// cron jobs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PersonalityContext {
    /// Full prompt for the primary interactive session.
    Primary,
    /// Reduced prompt for spawned subagents and cron jobs.
    Subagent,
}

/// Parameters for seeding a new agent personality from the unified template.
#[derive(Debug, Clone)]
pub struct AgentTemplateParams {
    /// Agent directory name (used as fallback display name).
    pub agent_id: String,
    /// Human-readable display name (e.g. "Code Reviewer").
    pub display_name: String,
    /// Short description of the agent's role (e.g. "Senior code reviewer
    /// focused on safety").
    pub description: String,
    /// Signature emoji.
    pub emoji: String,
}

impl Default for AgentTemplateParams {
    fn default() -> Self {
        Self {
            agent_id: "default".to_string(),
            display_name: "Default Agent".to_string(),
            description: "Your friendly local AI assistant running on your machine.".to_string(),
            emoji: "🦑".to_string(),
        }
    }
}

/// Seed an agent directory with standard personality files.
///
/// Uses the unified template with placeholder substitution so every agent
/// gets a consistent IDENTITY.md + SOUL.md structure, while still allowing
/// per-agent customisation of name, description, and emoji.
pub async fn seed_agent_personality(
    paths: &SyscityPaths,
    agent_dir: &Path,
    params: &AgentTemplateParams,
) -> crate::Result<()> {
    if !agent_dir.exists() {
        tokio::fs::create_dir_all(agent_dir).await.map_err(|e| {
            crate::error::SyscityError::Storage {
                context: format!("Failed to create agent dir: {:?}", agent_dir),
                details: e.to_string(),
            }
        })?;
    }

    // Ensure workspace/ and data/ subdirectories exist
    let id = params.agent_id.clone();
    for sub in [&paths.agent_workspace_dir(&id), &paths.agent_data_dir(&id)] {
        if !sub.exists() {
            tokio::fs::create_dir_all(sub).await.map_err(|e| {
                crate::error::SyscityError::Storage {
                    context: format!("Failed to create agent subdirectory: {:?}", sub),
                    details: e.to_string(),
                }
            })?;
        }
    }

    // IDENTITY.md — simple heading + ## name format (parsed by display_name())
    let identity_path = agent_dir.join("IDENTITY.md");
    if !identity_path.exists() {
        let identity = format_identity(params);
        tokio::fs::write(&identity_path, identity)
            .await
            .map_err(|e| crate::error::SyscityError::Storage {
                context: format!("Failed to write IDENTITY.md: {:?}", identity_path),
                details: e.to_string(),
            })?;
        info!("Created IDENTITY.md for agent '{}'", id);
    }

    // SOUL.md — structured YAML frontmatter + markdown body
    let soul_path = agent_dir.join("SOUL.md");
    if !soul_path.exists() {
        let soul = format_soul(params);
        tokio::fs::write(&soul_path, soul).await.map_err(|e| {
            crate::error::SyscityError::Storage {
                context: format!("Failed to write SOUL.md: {:?}", soul_path),
                details: e.to_string(),
            }
        })?;
        info!("Created SOUL.md for agent '{}'", id);
    }

    Ok(())
}

/// Synchronous version of `seed_agent_personality`.
pub fn seed_agent_personality_sync(
    paths: &SyscityPaths,
    agent_dir: &Path,
    params: &AgentTemplateParams,
) -> crate::Result<()> {
    if !agent_dir.exists() {
        std::fs::create_dir_all(agent_dir).map_err(|e| crate::error::SyscityError::Storage {
            context: format!("Failed to create agent dir: {:?}", agent_dir),
            details: e.to_string(),
        })?;
    }

    let id = params.agent_id.clone();
    for sub in [&paths.agent_workspace_dir(&id), &paths.agent_data_dir(&id)] {
        if !sub.exists() {
            std::fs::create_dir_all(sub).map_err(|e| crate::error::SyscityError::Storage {
                context: format!("Failed to create agent subdirectory: {:?}", sub),
                details: e.to_string(),
            })?;
        }
    }

    let identity_path = agent_dir.join("IDENTITY.md");
    if !identity_path.exists() {
        let identity = format_identity(params);
        std::fs::write(&identity_path, identity).map_err(|e| {
            crate::error::SyscityError::Storage {
                context: format!("Failed to write IDENTITY.md: {:?}", identity_path),
                details: e.to_string(),
            }
        })?;
        info!("Created IDENTITY.md for agent '{}'", id);
    }

    let soul_path = agent_dir.join("SOUL.md");
    if !soul_path.exists() {
        let soul = format_soul(params);
        std::fs::write(&soul_path, soul).map_err(|e| crate::error::SyscityError::Storage {
            context: format!("Failed to write SOUL.md: {:?}", soul_path),
            details: e.to_string(),
        })?;
        info!("Created SOUL.md for agent '{}'", id);
    }

    Ok(())
}

fn format_identity(params: &AgentTemplateParams) -> String {
    format!(
        "# {}\n\n## name\n{}\n\n{}\n",
        params.display_name, params.display_name, params.description
    )
}

fn format_soul(params: &AgentTemplateParams) -> String {
    format!(
        "---\nname: {}\npersona: {}\nvoice: concise, direct, no filler\nemoji: \
         \"{}\"\nbehavior:\nproactive: false\nask_before_destructive: \
         true\npreferences:\nlanguage: en-US\nformat: markdown\n---\n\n# Core Principles\n\nBe \
         genuinely helpful, not performatively helpful.\nPrioritize correctness and clarity over \
         speed.\n",
        params.display_name, params.description, params.emoji
    )
}

/// Agent personality loaded from markdown files
#[derive(Debug, Clone, Default)]
pub struct AgentPersonality {
    /// Agent ID (directory name)
    pub id: String,
    /// SOUL.md - Core personality, values, behavioral guidelines
    pub soul: String,
    /// IDENTITY.md - Agent identity, name, role definition
    pub identity: String,
    /// BOOTSTRAP.md - Initial startup behavior, first-run logic
    pub bootstrap: String,
    /// USER.md - User-specific memory, preferences
    pub user: String,
    /// AGENTS.md - Operating instructions for other agents
    pub agents: String,
    /// TOOLS.md - Tool notes and conventions
    pub tools: String,
    /// HEARTBEAT.md - Periodic task checklist and proactive work reminders
    pub heartbeat: String,
    /// MEMORY.md - Curated long-term memory (personal context)
    pub memory: String,
    /// Path to the agent directory
    pub path: PathBuf,
    /// Whether this personality is valid (has at least SOUL.md or IDENTITY.md)
    pub is_valid: bool,
}

impl AgentPersonality {
    /// Load personality from an agent directory
    pub async fn load(agent_dir: &Path) -> crate::Result<Self> {
        let id = agent_dir
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("unknown")
            .to_string();

        info!("Loading agent personality: {}", id);

        let mut personality = Self {
            id: id.clone(),
            path: agent_dir.to_path_buf(),
            ..Default::default()
        };

        // Load each personality file
        personality.soul = personality.load_file("SOUL.md").await;
        personality.identity = personality.load_file("IDENTITY.md").await;
        personality.bootstrap = personality.load_file("BOOTSTRAP.md").await;
        personality.user = personality.load_file("USER.md").await;
        personality.agents = personality.load_file("AGENTS.md").await;
        personality.tools = personality.load_file("TOOLS.md").await;
        personality.heartbeat = personality.load_file("HEARTBEAT.md").await;
        personality.memory = personality.load_file("MEMORY.md").await;

        // Valid if has SOUL.md or IDENTITY.md
        personality.is_valid = !personality.soul.is_empty() || !personality.identity.is_empty();

        if personality.is_valid {
            info!("✅ Loaded personality for agent '{}'", id);
        } else {
            warn!("⚠️  Agent '{}' has no SOUL.md or IDENTITY.md", id);
        }

        Ok(personality)
    }

    /// Load a specific file from the agent directory
    async fn load_file(&self, filename: &str) -> String {
        let file_path = self.path.join(filename);

        // Check metadata first to avoid OOM from large files
        match fs::metadata(&file_path).await {
            Ok(meta) if meta.len() > DEFAULT_MAX_FILE_SIZE as u64 => {
                debug!(
                    "Personality file {} for agent {} is {} bytes, exceeding {} byte limit, \
                     reading truncated",
                    filename,
                    self.id,
                    meta.len(),
                    DEFAULT_MAX_FILE_SIZE
                );
                // Read only the first DEFAULT_MAX_FILE_SIZE bytes
                // using a fixed-size buffer (NOT read_to_end, which ignores
                // Vec::with_capacity and reads until EOF).
                let mut buf = vec![0u8; DEFAULT_MAX_FILE_SIZE];
                use tokio::io::AsyncReadExt;
                let mut file = match tokio::fs::File::open(&file_path).await {
                    Ok(f) => f,
                    Err(e) => {
                        warn!("Failed to open {} for agent {}: {}", filename, self.id, e);
                        return String::new();
                    }
                };
                let n = match file.read(&mut buf).await {
                    Ok(n) => n,
                    Err(e) => {
                        warn!("Failed to read {} for agent {}: {}", filename, self.id, e);
                        return String::new();
                    }
                };
                let s = String::from_utf8_lossy(&buf[..n]);
                s.chars().take(DEFAULT_MAX_FILE_SIZE).collect()
            }
            Ok(_) => {
                // File is within size limits, read normally
                match fs::read_to_string(&file_path).await {
                    Ok(content) => content,
                    Err(e) => {
                        warn!("Failed to read {} for agent {}: {}", filename, self.id, e);
                        String::new()
                    }
                }
            }
            Err(e) => {
                debug!("Failed to stat {} for agent {}: {}", filename, self.id, e);
                String::new()
            }
        }
    }

    /// Convert personality to AgentConfig using the full (Primary) prompt.
    pub fn to_agent_config(&self) -> AgentConfig {
        self.to_agent_config_for(PersonalityContext::Primary)
    }

    /// Convert personality to AgentConfig for the given context.
    ///
    /// Use [`PersonalityContext::Subagent`] when spawning child agents or cron
    /// jobs to omit startup-only sections (Bootstrap, User) and reduce token
    /// usage.
    pub fn to_agent_config_for(&self, ctx: PersonalityContext) -> AgentConfig {
        let system_prompt = match ctx {
            PersonalityContext::Primary => self.build_system_prompt(),
            PersonalityContext::Subagent => self.build_subagent_prompt(),
        };

        // Inject agent identity so the agent knows its own ID and can manage its files
        let system_prompt = format!(
            "{}\n\n## Agent Identity\n\nYour agent ID is: `{}`\n\
             You may edit files in your agent directory (including HEARTBEAT.md) to manage \
             your personality and periodic tasks when explicitly asked by the user.",
            system_prompt, self.id,
        );

        AgentConfig {
            system_prompt,
            max_context_tokens: 4096,
            max_concurrent_tools: 5,
            temperature: 0.7,
            max_tokens: 2048,
            skills_prompt: None,
            max_turns: None,
            compaction_model: None,
            workspace_dir: None,
            workspace_only: true,
            fence_network: false,
            // Stamped from `[security]` at spawn (`spawn_agent_inner`), like
            // `fence_network`.
            fence_namespaces: crate::tools::process_runner::NamespacePosture::Auto,
            heartbeat: None,
            agent_id: None,
            reflection_config: None,
        }
    }

    /// Build full system prompt from personality files
    /// Priority: BOOTSTRAP > IDENTITY > SOUL
    fn build_system_prompt(&self) -> String {
        let mut sections = Vec::new();

        // BOOTSTRAP.md - Initial behavior (highest priority)
        if !self.bootstrap.is_empty() {
            sections.push(format!("## Bootstrap\n{}\n", self.bootstrap.trim()));
        }

        // IDENTITY.md - Who the agent is
        if !self.identity.is_empty() {
            sections.push(format!("## Identity\n{}\n", self.identity.trim()));
        }

        // SOUL.md - Core personality
        if !self.soul.is_empty() {
            sections.push(format!("## Soul\n{}\n", self.soul.trim()));
        }

        // AGENTS.md - Operating instructions
        if !self.agents.is_empty() {
            sections.push(format!("## Agents\n{}\n", self.agents.trim()));
        }

        // TOOLS.md - Tool conventions
        if !self.tools.is_empty() {
            sections.push(format!("## Tools\n{}\n", self.tools.trim()));
        }

        // HEARTBEAT.md - Periodic tasks and proactive work
        if !self.heartbeat.is_empty() {
            sections.push(format!("## Heartbeat\n{}\n", self.heartbeat.trim()));
        }

        // MEMORY.md - Curated long-term memory (personal context)
        if !self.memory.is_empty() {
            sections.push(format!("## Memory\n{}\n", self.memory.trim()));
        }

        if sections.is_empty() {
            // Fallback to default
            AgentConfig::default().system_prompt
        } else {
            sections.join("\n")
        }
    }

    /// Build a reduced system prompt for subagents and cron jobs.
    ///
    /// Includes: Identity, Soul, Agents, Tools, User.
    /// Excludes: Bootstrap (startup-only), Heartbeat (periodic tasks), Memory
    /// (personal context).
    fn build_subagent_prompt(&self) -> String {
        let mut sections = Vec::new();

        if !self.identity.is_empty() {
            sections.push(format!("## Identity\n{}\n", self.identity.trim()));
        }

        if !self.soul.is_empty() {
            sections.push(format!("## Soul\n{}\n", self.soul.trim()));
        }

        if !self.agents.is_empty() {
            sections.push(format!("## Agents\n{}\n", self.agents.trim()));
        }

        if !self.tools.is_empty() {
            sections.push(format!("## Tools\n{}\n", self.tools.trim()));
        }

        if !self.user.is_empty() {
            sections.push(format!("## User\n{}\n", self.user.trim()));
        }

        // Explicitly excluded: bootstrap, heartbeat, memory
        // - Bootstrap: startup-only instructions irrelevant to subagents
        // - Heartbeat: periodic task checklist for main session only
        // - Memory: contains personal context that shouldn't leak to strangers

        if sections.is_empty() {
            AgentConfig::default().system_prompt
        } else {
            sections.join("\n")
        }
    }

    /// Get the agent's display name from identity.
    ///
    /// Supports multiple common formats:
    /// 1. `## name` / `##name` / `name:` followed by the name on the next line
    /// 2. Markdown list items like `- **名称**: 小明` or `- **Name**: Xiao
    ///    Ming`
    /// 3. YAML-style `name: 小明`
    /// 4. First heading `# Title` as a fallback
    pub fn display_name(&self) -> String {
        let identity = &self.identity;

        // 1. Structured format: ## name / ##name / name:
        let lines: Vec<&str> = identity.lines().collect();
        for (i, line) in lines.iter().enumerate() {
            let trimmed = line.trim();
            if trimmed.eq_ignore_ascii_case("## name")
                || trimmed.eq_ignore_ascii_case("##name")
                || trimmed.eq_ignore_ascii_case("name:")
            {
                if let Some(val) = lines.get(i + 1) {
                    let name = val.trim();
                    if !name.is_empty() {
                        return name.to_string();
                    }
                }
            }
        }

        // 2. Markdown list: `- **名称**: 小明` or `- **Name**: Xiao Ming`
        for line in identity.lines() {
            if let Some(caps) = NAME_LIST_RE.captures(line) {
                let name = caps.get(1).map(|m| m.as_str().trim()).unwrap_or("");
                if !name.is_empty() {
                    return name.to_string();
                }
            }
        }

        // 3. YAML-style inline: `name: 小明`
        for line in identity.lines() {
            if let Some(caps) = NAME_YAML_RE.captures(line) {
                let name = caps.get(1).map(|m| m.as_str().trim()).unwrap_or("");
                // Avoid catching the `## name` marker itself.
                if !name.is_empty() && !name.eq_ignore_ascii_case("name") {
                    return name.to_string();
                }
            }
        }

        // 4. Fallback to first heading line, but ignore placeholder headings.
        identity
            .lines()
            .next()
            .and_then(|line| {
                let trimmed = line.trim();
                let title = trimmed.strip_prefix("#").map(|s| s.trim())?;
                if PLACEHOLDER_HEADING_RE.is_match(title) {
                    return None;
                }
                Some(title.to_string())
            })
            .unwrap_or_else(|| self.id.clone())
    }

    /// Get the agent's emoji.
    ///
    /// Checks SOUL.md YAML frontmatter first, then IDENTITY.md for `Emoji:` or
    /// `- **Emoji**: ...` style entries, and falls back to 🤖.
    pub fn emoji(&self) -> String {
        let emoji_from_text = |text: &str| -> Option<String> {
            for line in text.lines() {
                let trimmed = line.trim();
                if let Some(value) = trimmed
                    .strip_prefix("emoji:")
                    .or_else(|| trimmed.strip_prefix("Emoji:"))
                    .or_else(|| trimmed.strip_prefix("emoji："))
                    .or_else(|| trimmed.strip_prefix("Emoji："))
                {
                    let value = value.trim().trim_matches('"').trim_matches('\'');
                    if !value.is_empty() {
                        return Some(value.to_string());
                    }
                }
                // Markdown list: `- **Emoji**: 🐼`
                if let Some(caps) = EMOJI_LIST_RE.captures(line) {
                    let value = caps.get(1).map(|m| m.as_str().trim()).unwrap_or("");
                    if !value.is_empty() {
                        return Some(value.to_string());
                    }
                }
            }
            None
        };

        emoji_from_text(&self.soul)
            .or_else(|| emoji_from_text(&self.identity))
            .unwrap_or_else(|| "🤖".to_string())
    }
    pub fn can_handle(&self, task_type: &str) -> bool {
        let content = format!("{} {} {}", self.soul, self.identity, self.bootstrap);
        let keywords: Vec<&str> = match task_type {
            "code" => vec!["code", "program", "develop", "software", "debug"],
            "review" => vec!["review", "audit", "check", "analyze"],
            "write" => vec!["write", "document", "compose"],
            "research" => vec!["research", "investigate", "study"],
            "lead" => vec!["lead", "manage", "coordinate", "architect"],
            _ => vec![task_type],
        };

        let content_lower = content.to_lowercase();
        keywords.iter().any(|kw| content_lower.contains(kw))
    }

    /// Get all possible aliases for this agent.
    ///
    /// Includes the display name, the agent ID, and short forms derived from
    /// both. Example: "secretary-xiaowang" with display name "秘书小王"
    /// produces `["secretary-xiaowang", "xiaowang", "秘书小王", "小王"]`.
    pub fn aliases(&self) -> Vec<String> {
        let mut aliases = Vec::new();

        // Agent ID (always included)
        aliases.push(self.id.clone());

        // Short form from ID: "secretary-xiaowang" -> "xiaowang"
        if let Some(short) = self.id.rsplit('-').next() {
            if short != self.id {
                aliases.push(short.to_string());
            }
        }

        // Display name from IDENTITY.md
        let display = self.display_name();
        if !display.is_empty() && display != self.id {
            aliases.push(display.clone());
            // Extract short nicknames from display name:
            // "秘书小王" -> "小王"
            // "My Agent Name" -> "My", "Agent", "Name", "Agent Name"
            for word in display.split_whitespace() {
                let trimmed = word.trim();
                if trimmed.len() >= 2 && !aliases.iter().any(|a| a == trimmed) {
                    aliases.push(trimmed.to_string());
                }
            }
            // Also try last 2-4 chars as a common nickname pattern (Chinese)
            if display.chars().count() >= 3 {
                let suffix: String = display
                    .chars()
                    .rev()
                    .take(2)
                    .collect::<Vec<_>>()
                    .into_iter()
                    .rev()
                    .collect();
                if suffix.len() >= 2 && !aliases.iter().any(|a| a == &suffix) {
                    aliases.push(suffix);
                }
            }
        }

        aliases
    }
}

// ── Identity rewriting (rename) ─────────────────────────────────────────────
//
// The writers below mirror the precedence [`AgentPersonality::display_name`]
// and [`AgentPersonality::emoji`] read with, so the value a rename writes is
// the value read back afterwards. Each one rewrites the *highest-precedence*
// source it finds and leaves the rest of the file untouched; only a file with
// no usable source at all gains a new section.

/// Replace the value of `line` matched by capture group 1 of `re`.
///
/// Returns `None` when the regex does not match (or the group is missing), so
/// callers can fall through to the next source.
fn replace_capture(line: &str, re: &Regex, value: &str) -> Option<String> {
    let caps = re.captures(line)?;
    let m = caps.get(1)?;
    let mut out = String::with_capacity(line.len() + value.len());
    out.push_str(&line[..m.start()]);
    out.push_str(value);
    out.push_str(&line[m.end()..]);
    Some(out)
}

/// True for the `## name` / `##name` / `name:` marker whose *next* line holds
/// the display name.
fn is_name_marker(line: &str) -> bool {
    let t = line.trim();
    t.eq_ignore_ascii_case("## name")
        || t.eq_ignore_ascii_case("##name")
        || t.eq_ignore_ascii_case("name:")
}

/// Rewrite an IDENTITY.md body so [`AgentPersonality::display_name`] returns
/// `name` for it.
///
/// Follows the same order `display_name()` reads in: the `## name` marker's
/// following line, then a `- **Name**: x` list item, then an inline `name: x`,
/// then the first non-placeholder heading. A body with none of those gets the
/// canonical `# <name>` + `## name` header that `seed_agent_personality`
/// writes.
pub fn write_display_name(identity: &str, name: &str) -> String {
    let mut lines: Vec<String> = identity.split('\n').map(str::to_string).collect();

    // 1. `## name` / `##name` / `name:` — the name is on the next line.
    for i in 0..lines.len() {
        if is_name_marker(&lines[i]) {
            match lines.get_mut(i + 1) {
                Some(next) => *next = name.to_string(),
                None => lines.push(name.to_string()),
            }
            return lines.join("\n");
        }
    }

    // 2. `- **名称**: 小明` / `- **Name**: Xiao Ming`
    for line in lines.iter_mut() {
        if let Some(replaced) = replace_capture(line, &NAME_LIST_RE, name) {
            *line = replaced;
            return lines.join("\n");
        }
    }

    // 3. Inline `name: 小明`
    for line in lines.iter_mut() {
        if let Some(replaced) = replace_capture(line, &NAME_YAML_RE, name) {
            *line = replaced;
            return lines.join("\n");
        }
    }

    // 4. First heading, unless the reader would treat it as a placeholder.
    if let Some(first) = lines.first() {
        let trimmed = first.trim();
        if let Some(title) = trimmed.strip_prefix('#') {
            if !PLACEHOLDER_HEADING_RE.is_match(title.trim()) {
                let indent = &first[..first.len() - first.trim_start().len()];
                lines[0] = format!("{}# {}", indent, name);
                return lines.join("\n");
            }
        }
    }

    // 5. Nothing to rewrite — write the canonical header.
    let header = format!("# {}\n\n## name\n{}\n", name, name);
    if identity.trim().is_empty() {
        header
    } else {
        format!("{}\n{}", header, identity)
    }
}

/// Rewrite SOUL.md / IDENTITY.md so [`AgentPersonality::emoji`] returns
/// `emoji` for them.
///
/// SOUL.md wins over IDENTITY.md in `emoji()`, so the emoji is written there
/// whenever it already carries one; otherwise IDENTITY.md's entry is updated.
/// A personality with an emoji nowhere gets one inserted into SOUL.md's YAML
/// frontmatter (or as its first line, when it has no frontmatter).
///
/// Returns the `(soul, identity)` pair to write back; either may be unchanged.
pub fn write_emoji(soul: &str, identity: &str, emoji: &str) -> (String, String) {
    let emoji_line = |line: &str| -> Option<String> {
        let trimmed = line.trim();
        for prefix in ["emoji:", "Emoji:", "emoji：", "Emoji："] {
            if trimmed.strip_prefix(prefix).is_some() {
                // Keep the file's own spelling of the key and indentation.
                let indent = &line[..line.len() - line.trim_start().len()];
                return Some(format!("{}{} \"{}\"", indent, prefix, emoji));
            }
        }
        replace_capture(line, &EMOJI_LIST_RE, emoji)
    };

    let mut soul_lines: Vec<String> = soul.split('\n').map(str::to_string).collect();
    for line in soul_lines.iter_mut() {
        if let Some(replaced) = emoji_line(line) {
            *line = replaced;
            return (soul_lines.join("\n"), identity.to_string());
        }
    }

    let mut identity_lines: Vec<String> = identity.split('\n').map(str::to_string).collect();
    for line in identity_lines.iter_mut() {
        if let Some(replaced) = emoji_line(line) {
            *line = replaced;
            return (soul.to_string(), identity_lines.join("\n"));
        }
    }

    // No emoji anywhere: give SOUL.md one so the next read finds it.
    if soul.is_empty() {
        return (format!("emoji: \"{}\"\n", emoji), identity.to_string());
    }
    if soul_lines.first().is_some_and(|l| l.trim() == "---") {
        soul_lines.insert(1, format!("emoji: \"{}\"", emoji));
        return (soul_lines.join("\n"), identity.to_string());
    }
    (format!("emoji: \"{}\"\n{}", emoji, soul), identity.to_string())
}

/// Agent Registry for discovered personalities
#[derive(Debug, Default)]
pub struct AgentRegistry {
    /// Registered agent personalities
    personalities: HashMap<String, AgentPersonality>,
    /// Whether agents have been discovered
    discovered: bool,
}

impl AgentRegistry {
    /// Create new empty registry
    pub fn new() -> Self {
        Self {
            personalities: HashMap::new(),
            discovered: false,
        }
    }

    /// Discover agents from the configured agents/ directory.
    pub async fn discover(&mut self, paths: &SyscityPaths) -> crate::Result<usize> {
        self.discover_in_dir(paths, &paths.agents_dir()).await
    }

    /// Discover agents from a specific directory.
    pub async fn discover_in_dir(
        &mut self,
        paths: &SyscityPaths,
        agents_dir: &Path,
    ) -> crate::Result<usize> {
        if !agents_dir.exists() {
            info!("Agents directory does not exist: {:?}", agents_dir);
            return Ok(0);
        }

        info!("Discovering agents from: {:?}", agents_dir);

        let mut count = 0;
        let mut entries = fs::read_dir(agents_dir).await?;

        while let Some(entry) = entries.next_entry().await? {
            let path = entry.path();

            // Skip non-directories
            if !path.is_dir() {
                continue;
            }

            // Load personality
            match AgentPersonality::load(&path).await {
                Ok(personality) => {
                    let agent_id = personality.id.clone();
                    if personality.is_valid {
                        // Ensure agent subdirectories exist (workspace/, data/)
                        let workspace_dir = paths.agent_workspace_dir(&agent_id);
                        let data_dir = paths.agent_data_dir(&agent_id);
                        for dir in [&workspace_dir, &data_dir] {
                            if let Err(e) = tokio::fs::create_dir_all(dir).await {
                                warn!("Failed to create agent directory {:?}: {}", dir, e);
                            }
                        }
                        self.personalities.insert(agent_id, personality);
                        count += 1;
                    } else {
                        // Directory exists but no valid personality — seed from template
                        info!(
                            "Agent '{}' has no personality files, seeding from template",
                            agent_id
                        );
                        let params = AgentTemplateParams {
                            agent_id: agent_id.clone(),
                            display_name: humanize_agent_id(&agent_id),
                            description: format!(
                                "AI assistant specialised for the '{}' role.",
                                agent_id
                            ),
                            emoji: "🤖".to_string(),
                        };
                        if let Err(e) = seed_agent_personality(paths, &path, &params).await {
                            warn!("Failed to seed personality for '{}': {}", agent_id, e);
                        } else {
                            // Reload after seeding
                            match AgentPersonality::load(&path).await {
                                Ok(reloaded) if reloaded.is_valid => {
                                    self.personalities.insert(agent_id.clone(), reloaded);
                                    count += 1;
                                }
                                _ => {
                                    warn!("Agent '{}' still invalid after seeding", agent_id);
                                }
                            }
                        }
                    }
                }
                Err(e) => {
                    warn!("Failed to load agent from {:?}: {}", path, e);
                }
            }
        }

        self.discovered = true;
        info!("Discovered {} valid agents", count);

        // List discovered agents
        if count > 0 {
            debug!("Discovered agents:");
            for (id, personality) in &self.personalities {
                debug!("  - {} ({})", id, personality.display_name());
            }
        }

        Ok(count)
    }

    /// Get a personality by ID
    pub fn get(&self, id: &str) -> Option<&AgentPersonality> {
        self.personalities.get(id)
    }

    /// Insert a personality directly. Test-only: production code populates the
    /// registry via `discover` / `discover_in_dir`.
    #[cfg(test)]
    pub(crate) fn insert_for_test(&mut self, personality: AgentPersonality) {
        self.personalities
            .insert(personality.id.clone(), personality);
    }

    /// Drop a personality from the registry, returning it if it was present.
    ///
    /// Used when an agent's directory is purged: `discover` only ever inserts,
    /// so a deleted agent would otherwise stay listed until restart.
    pub fn remove(&mut self, id: &str) -> Option<AgentPersonality> {
        self.personalities.remove(id)
    }

    /// Get all personality IDs
    pub fn list(&self) -> Vec<String> {
        self.personalities.keys().cloned().collect()
    }

    /// Check if a personality exists
    pub fn has(&self, id: &str) -> bool {
        self.personalities.contains_key(id)
    }

    /// Get number of registered personalities
    pub fn len(&self) -> usize {
        self.personalities.len()
    }

    /// Check if registry is empty
    pub fn is_empty(&self) -> bool {
        self.personalities.is_empty()
    }

    /// Check if discovery has been run
    pub fn is_discovered(&self) -> bool {
        self.discovered
    }

    /// Find the best agent for a task
    pub fn find_for_task(&self, task_type: &str) -> Option<&AgentPersonality> {
        self.personalities
            .values()
            .find(|p| p.can_handle(task_type))
    }

    /// Get all personalities that can handle a task
    pub fn find_all_for_task(&self, task_type: &str) -> Vec<&AgentPersonality> {
        self.personalities
            .values()
            .filter(|p| p.can_handle(task_type))
            .collect()
    }

    /// Iterate over all personalities
    pub fn iter(&self) -> impl Iterator<Item = &AgentPersonality> {
        self.personalities.values()
    }

    /// Find an agent whose aliases match the given name.
    ///
    /// Matches exact alias strings (case-insensitive). Returns the first
    /// matching personality and the matched alias text so the caller can
    /// strip it from the original message.
    pub fn find_by_alias(&self, name: &str) -> Option<(&AgentPersonality, String)> {
        let name_lower = name.to_lowercase();
        for personality in self.personalities.values() {
            for alias in personality.aliases() {
                if alias.to_lowercase() == name_lower {
                    return Some((personality, alias));
                }
            }
        }
        None
    }
}

/// Convert a kebab-case agent ID into a human-readable title.
///
/// Examples:
/// - `code-reviewer` -> "Code Reviewer"
/// - `my-agent` -> "My Agent"
/// - `default` -> "Default"
fn humanize_agent_id(id: &str) -> String {
    id.split('-')
        .filter(|s| !s.is_empty())
        .map(|s| {
            let mut chars = s.chars();
            match chars.next() {
                Some(first) => {
                    first.to_uppercase().collect::<String>() + &chars.as_str().to_lowercase()
                }
                None => String::new(),
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// Global registry (can be stored in GatewayState)
pub type SharedAgentRegistry = std::sync::Arc<tokio::sync::RwLock<AgentRegistry>>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_personality_builds_system_prompt() {
        let personality = AgentPersonality {
            id: "test".to_string(),
            soul: "You are helpful.".to_string(),
            identity: "# Test Agent\nI am a test.".to_string(),
            bootstrap: "Start by greeting.".to_string(),
            ..Default::default()
        };

        let prompt = personality.build_system_prompt();
        assert!(prompt.contains("Bootstrap"));
        assert!(prompt.contains("Identity"));
        assert!(prompt.contains("Soul"));
    }

    #[test]
    fn test_display_name_extraction() {
        let personality = AgentPersonality {
            id: "test-agent".to_string(),
            identity: "# My Agent Name\nDescription here.".to_string(),
            ..Default::default()
        };

        assert_eq!(personality.display_name(), "My Agent Name");
    }

    #[test]
    fn test_display_name_from_chinese_list() {
        let identity = "# 身份信息\n\n- **名称**: 小明\n- **角色**: 翻译秘书".to_string();
        let personality = AgentPersonality {
            id: "secretary_xiaoming".to_string(),
            identity,
            ..Default::default()
        };

        assert_eq!(personality.display_name(), "小明");
    }

    #[test]
    fn test_display_name_ignores_placeholder_heading() {
        let identity = "# IDENTITY.md — 我是谁\n\n- **名称**: 小王 (xiaowang)".to_string();
        let personality = AgentPersonality {
            id: "xiaowang".to_string(),
            identity,
            ..Default::default()
        };

        assert_eq!(personality.display_name(), "小王 (xiaowang)");
    }

    #[test]
    fn test_display_name_fallback_to_id() {
        let identity = "# 身份信息\n\n- **角色**: 私人秘书".to_string();
        let personality = AgentPersonality {
            id: "secretary_xiaowang".to_string(),
            identity,
            ..Default::default()
        };

        assert_eq!(personality.display_name(), "secretary_xiaowang");
    }

    #[test]
    fn test_task_matching() {
        let personality = AgentPersonality {
            id: "coder".to_string(),
            soul: "I write code and debug software.".to_string(),
            ..Default::default()
        };

        assert!(personality.can_handle("code"));
        assert!(personality.can_handle("debug"));
    }

    #[test]
    fn test_personality_context_primary_includes_bootstrap() {
        let personality = AgentPersonality {
            id: "agent".to_string(),
            bootstrap: "Start by greeting.".to_string(),
            identity: "I am an agent.".to_string(),
            soul: "Be helpful.".to_string(),
            user: "User prefers terse replies.".to_string(),
            agents: "Work with other agents.".to_string(),
            tools: "Use tools wisely.".to_string(),
            heartbeat: "Check inbox every hour.".to_string(),
            memory: "User likes coffee.".to_string(),
            ..Default::default()
        };

        let config = personality.to_agent_config_for(PersonalityContext::Primary);
        assert!(config.system_prompt.contains("Bootstrap"), "Primary should include Bootstrap");
        assert!(config.system_prompt.contains("Identity"));
        assert!(config.system_prompt.contains("Soul"));
        assert!(config.system_prompt.contains("Heartbeat"));
        assert!(config.system_prompt.contains("Memory"));
    }

    #[test]
    fn test_personality_context_subagent_excludes_bootstrap_heartbeat_and_memory() {
        let personality = AgentPersonality {
            id: "agent".to_string(),
            bootstrap: "Start by greeting.".to_string(),
            identity: "I am an agent.".to_string(),
            soul: "Be helpful.".to_string(),
            user: "User prefers terse replies.".to_string(),
            agents: "Work with other agents.".to_string(),
            tools: "Use tools wisely.".to_string(),
            heartbeat: "Check inbox every hour.".to_string(),
            memory: "User likes coffee.".to_string(),
            ..Default::default()
        };

        let config = personality.to_agent_config_for(PersonalityContext::Subagent);
        // Excluded: Bootstrap (startup-only), Heartbeat (periodic tasks), Memory
        // (personal context)
        assert!(
            !config.system_prompt.contains("Bootstrap"),
            "Subagent should NOT include Bootstrap"
        );
        assert!(
            !config.system_prompt.contains("Heartbeat"),
            "Subagent should NOT include Heartbeat"
        );
        assert!(
            !config.system_prompt.contains("User likes coffee"),
            "Subagent should NOT include Memory section content"
        );
        // Included: Identity, Soul, Agents, Tools, User
        assert!(config.system_prompt.contains("Identity"));
        assert!(config.system_prompt.contains("Soul"));
        assert!(config.system_prompt.contains("Agents"));
        assert!(config.system_prompt.contains("Tools"));
        assert!(config.system_prompt.contains("User prefers terse"));
    }

    #[test]
    fn test_to_agent_config_delegates_to_primary() {
        let personality = AgentPersonality {
            id: "agent".to_string(),
            bootstrap: "Boot!".to_string(),
            soul: "Be nice.".to_string(),
            ..Default::default()
        };

        let default_cfg = personality.to_agent_config();
        let primary_cfg = personality.to_agent_config_for(PersonalityContext::Primary);
        assert_eq!(default_cfg.system_prompt, primary_cfg.system_prompt);
    }

    #[test]
    fn test_subagent_prompt_fallback_when_all_empty() {
        let personality = AgentPersonality {
            id: "empty".to_string(),
            ..Default::default()
        };

        let config = personality.to_agent_config_for(PersonalityContext::Subagent);
        // Should not panic and should return the default system prompt
        assert!(!config.system_prompt.is_empty());
    }

    #[test]
    fn test_aliases_from_id_and_display_name() {
        let personality = AgentPersonality {
            id: "secretary-xiaowang".to_string(),
            identity: "# 秘书小王\n私人秘书".to_string(),
            ..Default::default()
        };

        let aliases = personality.aliases();
        assert!(aliases.contains(&"secretary-xiaowang".to_string()));
        assert!(aliases.contains(&"xiaowang".to_string()));
        assert!(aliases.contains(&"秘书小王".to_string()));
        assert!(aliases.contains(&"小王".to_string()));
    }

    #[test]
    fn test_find_by_alias_exact_match() {
        let mut registry = AgentRegistry::new();
        registry.personalities.insert(
            "secretary-xiaowang".to_string(),
            AgentPersonality {
                id: "secretary-xiaowang".to_string(),
                identity: "# 秘书小王\n私人秘书".to_string(),
                ..Default::default()
            },
        );

        let (p, alias) = registry.find_by_alias("小王").unwrap();
        assert_eq!(p.id, "secretary-xiaowang");
        assert_eq!(alias, "小王");

        let (p2, _) = registry.find_by_alias("xiaowang").unwrap();
        assert_eq!(p2.id, "secretary-xiaowang");
    }

    #[test]
    fn test_find_by_alias_case_insensitive() {
        let mut registry = AgentRegistry::new();
        registry.personalities.insert(
            "coder".to_string(),
            AgentPersonality {
                id: "coder".to_string(),
                identity: "# Code Assistant\nI write code.".to_string(),
                ..Default::default()
            },
        );

        let (p, _) = registry.find_by_alias("CODE ASSISTANT").unwrap();
        assert_eq!(p.id, "coder");
    }

    #[test]
    fn test_find_by_alias_no_match() {
        let registry = AgentRegistry::new();
        assert!(registry.find_by_alias("nonexistent").is_none());
    }

    // ── Template / seeding tests ─────────────────────────────────────────────

    #[tokio::test]
    async fn seed_creates_identity_with_correct_format() {
        let temp_dir = tempfile::tempdir().unwrap();
        let paths = SyscityPaths::from_root(temp_dir.path());
        let agent_id = unique_test_id("identity");
        let agent_dir = temp_dir.path().join(&agent_id);
        let params = AgentTemplateParams {
            agent_id: agent_id.clone(),
            display_name: "Identity Agent".to_string(),
            description: "Tests the identity template.".to_string(),
            emoji: "🆔".to_string(),
        };

        seed_agent_personality(&paths, &agent_dir, &params)
            .await
            .unwrap();

        let identity_path = agent_dir.join("IDENTITY.md");
        assert!(identity_path.exists());
        let content = std::fs::read_to_string(&identity_path).unwrap();
        assert!(content.starts_with("# Identity Agent\n"));
        assert!(content.contains("## name\n"));
        assert!(content.contains("Identity Agent"));
        assert!(content.contains("Tests the identity template."));
    }

    #[tokio::test]
    async fn seed_creates_soul_with_yaml_frontmatter() {
        let temp_dir = tempfile::tempdir().unwrap();
        let paths = SyscityPaths::from_root(temp_dir.path());
        let agent_id = unique_test_id("soul");
        let agent_dir = temp_dir.path().join(&agent_id);
        let params = AgentTemplateParams {
            agent_id: agent_id.clone(),
            display_name: "Soul Agent".to_string(),
            description: "Tests the soul template.".to_string(),
            emoji: "✨".to_string(),
        };

        seed_agent_personality(&paths, &agent_dir, &params)
            .await
            .unwrap();

        let soul_path = agent_dir.join("SOUL.md");
        assert!(soul_path.exists());
        let content = std::fs::read_to_string(&soul_path).unwrap();

        assert!(content.starts_with("---\n"));
        assert!(content.contains("name: Soul Agent\n"));
        assert!(content.contains("persona: Tests the soul template.\n"));
        assert!(content.contains("emoji: \"✨\"\n"));
        assert!(content.contains("voice: concise, direct, no filler\n"));
        assert!(content.contains("proactive: false\n"));
        assert!(content.contains("ask_before_destructive: true\n"));
        assert!(content.contains("language: en-US\n"));
        assert!(content.contains("format: markdown\n"));
        assert!(content.contains("---\n\n# Core Principles\n"));
        assert!(content.contains("Be genuinely helpful"));
    }

    #[tokio::test]
    async fn seeded_personality_loads_and_is_valid() {
        let temp_dir = tempfile::tempdir().unwrap();
        let paths = SyscityPaths::from_root(temp_dir.path());
        let agent_id = unique_test_id("load");
        let agent_dir = temp_dir.path().join(&agent_id);
        let params = AgentTemplateParams {
            agent_id: agent_id.clone(),
            display_name: "Loadable Agent".to_string(),
            description: "Tests load after seed.".to_string(),
            emoji: "📦".to_string(),
        };

        seed_agent_personality(&paths, &agent_dir, &params)
            .await
            .unwrap();

        let personality = AgentPersonality::load(&agent_dir).await.unwrap();
        assert!(personality.is_valid, "Seeded personality should be valid");
        assert_eq!(personality.id, params.agent_id);
        assert_eq!(personality.display_name(), "Loadable Agent");
        assert!(!personality.identity.is_empty());
        assert!(!personality.soul.is_empty());
    }

    #[test]
    fn seed_sync_matches_async_output() {
        let temp_dir = tempfile::tempdir().unwrap();
        let paths = SyscityPaths::from_root(temp_dir.path());
        let agent_id = unique_test_id("sync");
        let agent_dir = temp_dir.path().join(&agent_id);
        let params = AgentTemplateParams {
            agent_id: agent_id.clone(),
            display_name: "Sync Agent".to_string(),
            description: "Tests sync seeding.".to_string(),
            emoji: "⚡".to_string(),
        };

        seed_agent_personality_sync(&paths, &agent_dir, &params).unwrap();

        let identity = std::fs::read_to_string(agent_dir.join("IDENTITY.md")).unwrap();
        let soul = std::fs::read_to_string(agent_dir.join("SOUL.md")).unwrap();

        assert!(identity.contains("# Sync Agent"));
        assert!(identity.contains("## name\n"));
        assert!(soul.contains("name: Sync Agent"));
        assert!(soul.contains("persona: Tests sync seeding."));
        assert!(soul.contains("emoji: \"⚡\""));
        assert!(soul.starts_with("---\n"));
    }

    #[tokio::test]
    async fn seed_does_not_overwrite_existing_files() {
        let temp_dir = tempfile::tempdir().unwrap();
        let paths = SyscityPaths::from_root(temp_dir.path());
        let agent_id = unique_test_id("no-clobber");
        let agent_dir = temp_dir.path().join(&agent_id);
        std::fs::create_dir_all(&agent_dir).unwrap();
        let existing_identity = "# Existing Agent\nCustom content.";
        std::fs::write(agent_dir.join("IDENTITY.md"), existing_identity).unwrap();

        let params = AgentTemplateParams {
            agent_id: agent_id.clone(),
            display_name: "New Agent".to_string(),
            description: "Should not overwrite.".to_string(),
            emoji: "🚫".to_string(),
        };

        seed_agent_personality(&paths, &agent_dir, &params)
            .await
            .unwrap();

        let content = std::fs::read_to_string(agent_dir.join("IDENTITY.md")).unwrap();
        assert_eq!(content, existing_identity);
    }

    #[test]
    fn humanize_agent_id_variations() {
        assert_eq!(humanize_agent_id("code-reviewer"), "Code Reviewer");
        assert_eq!(humanize_agent_id("my-special-agent"), "My Special Agent");
        assert_eq!(humanize_agent_id("default"), "Default");
        assert_eq!(humanize_agent_id(""), "");
        assert_eq!(humanize_agent_id("single"), "Single");
    }

    #[tokio::test]
    async fn test_registry_discovers_valid_skips_invalid_and_seeds_empty() {
        let temp_dir = tempfile::tempdir().unwrap();
        let paths = SyscityPaths::from_root(temp_dir.path());
        let agents_dir = paths.agents_dir();

        let valid_id = unique_test_id("valid");
        let valid_dir = agents_dir.join(&valid_id);
        std::fs::create_dir_all(&valid_dir).unwrap();
        std::fs::write(
            valid_dir.join("IDENTITY.md"),
            format!("# Valid Agent\n## name\n{}\n", valid_id),
        )
        .unwrap();
        std::fs::write(
            valid_dir.join("SOUL.md"),
            "---\nname: Valid\npersona: test\n---\n# Soul\nbe good.".to_string(),
        )
        .unwrap();

        let empty_id = unique_test_id("empty");
        let empty_dir = agents_dir.join(&empty_id);
        std::fs::create_dir_all(&empty_dir).unwrap();

        std::fs::write(agents_dir.join("not-a-dir.txt"), "ignore").unwrap();

        let mut registry = AgentRegistry::new();
        let count = registry.discover_in_dir(&paths, &agents_dir).await.unwrap();
        assert_eq!(count, 2, "Should discover valid agent and seed empty dir");
        assert!(registry.has(&valid_id));
        assert!(registry.has(&empty_id));

        // Everything this test touches — including the workspace/ and data/
        // dirs discovery seeds — lives under `temp_dir`, so the TempDir drop
        // cleans up. No remove_dir_all against the real ~/.syscity.
    }

    #[tokio::test]
    async fn test_primary_prompt_token_budget() {
        let temp_dir = tempfile::tempdir().unwrap();
        let paths = SyscityPaths::from_root(temp_dir.path());
        let agent_dir = temp_dir.path().join("default");
        let params = AgentTemplateParams::default();
        seed_agent_personality(&paths, &agent_dir, &params)
            .await
            .unwrap();

        let personality = AgentPersonality::load(&agent_dir).await.unwrap();
        let config = personality.to_agent_config_for(PersonalityContext::Primary);
        let estimated_tokens = config.system_prompt.chars().count() / 4;
        assert!(
            estimated_tokens <= 8000,
            "Primary system prompt estimated {} tokens, exceeds 8k budget",
            estimated_tokens
        );
    }

    // ── Identity rewriting (rename) ──────────────────────────────────────────

    /// Read the display name back through the same path the registry uses.
    fn read_display_name(identity: &str) -> String {
        AgentPersonality {
            id: "agent".to_string(),
            identity: identity.to_string(),
            ..Default::default()
        }
        .display_name()
    }

    fn read_emoji(soul: &str, identity: &str) -> String {
        AgentPersonality {
            id: "agent".to_string(),
            soul: soul.to_string(),
            identity: identity.to_string(),
            ..Default::default()
        }
        .emoji()
    }

    #[test]
    fn write_display_name_replaces_marker_value() {
        let identity = "# Old Name\n\n## name\nOld Name\n\nA description.\n";
        let out = write_display_name(identity, "New Name");
        assert_eq!(read_display_name(&out), "New Name");
        // The rest of the file survives.
        assert!(out.contains("A description."));
        assert_eq!(out.lines().count(), identity.lines().count());
    }

    #[test]
    fn write_display_name_replaces_list_item() {
        let out = write_display_name("# Identity\n\n- **名称**: 小明\n", "小红");
        assert_eq!(read_display_name(&out), "小红");
        assert!(out.contains("- **名称**: 小红"));
    }

    #[test]
    fn write_display_name_replaces_inline_yaml() {
        let out = write_display_name("# Identity\nname: Old\n", "New");
        assert_eq!(read_display_name(&out), "New");
    }

    /// A heading-only identity file (no marker) is renamed in place — the
    /// reader's heading fallback is what it resolves through.
    #[test]
    fn write_display_name_replaces_first_heading() {
        let out = write_display_name("# Old Name\n\nSome notes.\n", "New Name");
        assert_eq!(read_display_name(&out), "New Name");
        assert!(out.contains("Some notes."));
    }

    /// Placeholder headings are not a name source for the reader, so the
    /// writer must not treat one as the place to write either.
    #[test]
    fn write_display_name_prepends_when_only_placeholder_heading() {
        let out = write_display_name("# IDENTITY.md\n\n## 我是谁\n\nSome notes.\n", "小明");
        assert_eq!(read_display_name(&out), "小明");
        assert!(out.contains("Some notes."));
    }

    #[test]
    fn write_display_name_creates_header_for_empty_identity() {
        let out = write_display_name("", "小明");
        assert_eq!(read_display_name(&out), "小明");
        assert!(out.starts_with("# 小明\n\n## name\n小明\n"));
    }

    #[test]
    fn write_emoji_updates_soul_frontmatter() {
        let soul = "---\nname: X\nemoji: \"🎨\"\n---\n\n# Body\n";
        let (soul, identity) = write_emoji(soul, "", "🐼");
        assert_eq!(read_emoji(&soul, &identity), "🐼");
        assert!(soul.contains("name: X"), "frontmatter survives");
        assert!(soul.contains("# Body"), "body survives");
    }

    /// SOUL.md wins over IDENTITY.md in `emoji()`, so an IDENTITY-only emoji
    /// is updated in place rather than shadowed by a new SOUL.md entry.
    #[test]
    fn write_emoji_updates_identity_when_soul_has_none() {
        let soul = "---\nname: X\n---\n\n# Body\n";
        let identity = "# X\n\n- **Emoji**: 🎨\n";
        let (soul, identity) = write_emoji(soul, identity, "🐼");
        assert_eq!(read_emoji(&soul, &identity), "🐼");
        assert!(!soul.contains("emoji"), "soul untouched");
    }

    #[test]
    fn write_emoji_inserts_into_soul_when_absent() {
        let soul = "---\nname: X\n---\n\n# Body\n";
        let (soul, identity) = write_emoji(soul, "", "🐼");
        assert_eq!(read_emoji(&soul, &identity), "🐼");
        assert!(soul.starts_with("---\nemoji: \"🐼\"\nname: X\n"), "soul: {soul}");
    }

    #[test]
    fn write_emoji_inserts_into_empty_soul() {
        let (soul, identity) = write_emoji("", "# X\n", "🐼");
        assert_eq!(read_emoji(&soul, &identity), "🐼");
    }

    #[test]
    fn registry_remove_drops_entry() {
        let mut registry = AgentRegistry::new();
        registry.insert_for_test(AgentPersonality {
            id: "gone".to_string(),
            identity: "# Gone\n".to_string(),
            ..Default::default()
        });
        assert!(registry.has("gone"));
        assert!(registry.remove("gone").is_some());
        assert!(!registry.has("gone"));
        assert!(registry.remove("gone").is_none());
    }

    // ── helpers ──────────────────────────────────────────────────────────────

    fn unique_test_id(prefix: &str) -> String {
        let ts = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis();
        format!("test-{}-{}-{}", prefix, std::process::id(), ts)
    }
}
