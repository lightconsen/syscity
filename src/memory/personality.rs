//! Memory Architecture for Syscity
//!
//! This module implements an memory system:
//! - SOUL.md: Core personality, values, behavioral guidelines
//! - IDENTITY.md: Agent identity, name, role definition
//! - BOOTSTRAP.md: Initial startup behavior, first-run logic
//! - USER.md: User-specific memory, preferences, conversation history
//! - AGENTS.md: Operating instructions and agent "memory"
//! - TOOLS.md: User-maintained tool notes and conventions
//! - memory/*.md: Dated/named memory fragments loaded dynamically
//!
//! Files are bounded per-file (default 20 KB) and in total (default 150 KB).
//! When a file exceeds the per-file cap, the first 70% and last 20% are kept
//! with a truncation marker between them.
//!
//! An mtime+size file cache avoids re-reading unchanged files on every turn.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock};
use std::time::SystemTime;

use regex::Regex;
use tokio::fs;
use tokio::sync::RwLock;
use tracing::{debug, info, warn};

use crate::error::SyscityError;
use crate::memory::soul::SoulAnalysis;

#[allow(clippy::expect_used)]
static RE_CODE_FENCE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"```(\w+)").expect("hard-coded code-fence regex is valid"));
#[allow(clippy::expect_used)]
static RE_WORD: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"[a-zA-Z]{4,}").expect("hard-coded word regex is valid"));
#[allow(clippy::expect_used)]
static RE_PREFERENCE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?i)(?:i\s+(?:prefer|like|want|need)|please\s+use|always\s+use|use)\s+([^.]{3,80})",
    )
    .expect("hard-coded preference regex is valid")
});

// Threat-scanning regex patterns (compiled once for all PersonalityMemory
// instances).
#[allow(clippy::expect_used)]
static RE_SYSTEM_PROMPT_INJECTION: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)(?:^|\n)\s*(system|assistant|user)\s*:\s*")
        .expect("hard-coded threat-scan regex is valid")
});
#[allow(clippy::expect_used)]
static RE_COMMAND_INJECTION: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)(;|\|\||&&)\s*(rm|sudo|wget|curl|bash|sh|python|perl|powershell|cmd|kill|mkfs|dd|chmod|chown)")
        .expect("hard-coded threat-scan regex is valid")
});
#[allow(clippy::expect_used)]
static RE_PATH_TRAVERSAL: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\.\./|\.\.\\").expect("hard-coded threat-scan regex is valid"));
#[allow(clippy::expect_used)]
static RE_EXFILTRATION: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)(curl|wget|fetch)\s+.*http").expect("hard-coded threat-scan regex is valid")
});

/// Controls which memory files are included in the system prompt.
///
/// `Primary` produces the full prompt including MEMORY.md (for main session
/// with user). `Subagent` omits MEMORY.md (contains personal context that
/// shouldn't leak to strangers) and BOOTSTRAP.md (startup-only instructions
/// irrelevant to subagents).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MemoryContext {
    /// Full prompt for the primary interactive session (includes MEMORY.md)
    Primary,
    /// Reduced prompt for spawned subagents and cron jobs (excludes MEMORY.md,
    /// BOOTSTRAP.md)
    Subagent,
}

/// Default maximum size for each personality memory file (20 KB).
pub const DEFAULT_MAX_MEMORY_SIZE: usize = 20_000;

/// Default total budget across all files (150 KB).
pub const DEFAULT_TOTAL_MAX_SIZE: usize = 150_000;

/// Truncate `content` to `max_chars`, keeping the first 70% and the last 20%
/// with a `[... N chars truncated ...]` marker in between.
///
/// If the content fits within `max_chars` it is returned unchanged.
pub fn truncate_with_head_tail(content: &str, max_chars: usize) -> String {
    if content.chars().count() <= max_chars {
        return content.to_string();
    }
    // Use char boundaries to avoid panicking on multi-byte UTF-8.
    let total_chars = content.chars().count();
    let head_chars = (max_chars as f64 * 0.70) as usize;
    let tail_chars = (max_chars as f64 * 0.20) as usize;
    let head_chars = head_chars.min(total_chars);
    let tail_start_chars = total_chars.saturating_sub(tail_chars).max(head_chars);

    let head_end = content
        .char_indices()
        .nth(head_chars)
        .map(|(i, _)| i)
        .unwrap_or(content.len());
    let tail_start = content
        .char_indices()
        .nth(tail_start_chars)
        .map(|(i, _)| i)
        .unwrap_or(content.len());

    let truncated = total_chars
        .saturating_sub(head_chars)
        .saturating_sub(tail_chars);
    format!(
        "{}\n\n[... {} chars truncated ...]\n\n{}",
        &content[..head_end],
        truncated,
        &content[tail_start..]
    )
}

// ── File cache
// ────────────────────────────────────────────────────────────────

/// A cached view of a single file on disk.
#[derive(Clone, Debug)]
struct CachedFile {
    content: String,
    mtime: SystemTime,
    size: u64,
}

/// Thread-safe, mtime/size-invalidated cache of file contents.
type FileCache = Arc<RwLock<HashMap<PathBuf, CachedFile>>>;

/// Types of memory
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MemoryType {
    /// Soul memory - core personality, values, behavioral guidelines
    Soul,
    /// Identity memory - agent identity, name, role definition
    Identity,
    /// Bootstrap memory - initial startup behavior, first-run logic
    Bootstrap,
    /// User memory - user-specific data, preferences, conversation history
    User,
    /// Agents memory - operating instructions and agent "memory"
    Agents,
    /// Tools memory - user-maintained tool notes and conventions
    Tools,
    /// Heartbeat memory - periodic task checklist and proactive work reminders
    Heartbeat,
    /// Memory memory - curated long-term memory (evergreen, no temporal decay)
    Memory,
}

impl MemoryType {
    /// Get the filename for this memory type
    pub fn filename(&self) -> &'static str {
        match self {
            MemoryType::Soul => "SOUL.md",
            MemoryType::Identity => "IDENTITY.md",
            MemoryType::Bootstrap => "BOOTSTRAP.md",
            MemoryType::User => "USER.md",
            MemoryType::Agents => "AGENTS.md",
            MemoryType::Tools => "TOOLS.md",
            MemoryType::Heartbeat => "HEARTBEAT.md",
            MemoryType::Memory => "MEMORY.md",
        }
    }

    /// Get the description of this memory type
    pub fn description(&self) -> &'static str {
        match self {
            MemoryType::Soul => {
                "Core personality, values, behavioral guidelines, and character traits"
            }
            MemoryType::Identity => "Agent identity, name, role definition, and self-concept",
            MemoryType::Bootstrap => "Initial startup behavior, first-run logic, and onboarding",
            MemoryType::User => {
                "User-specific memory, preferences, conversation history, and learned context"
            }
            MemoryType::Agents => "Operating instructions and agent memory for task execution",
            MemoryType::Tools => "User-maintained tool notes, conventions, and usage patterns",
            MemoryType::Heartbeat => "Periodic task checklist and proactive work reminders",
            MemoryType::Memory => "Curated long-term memory (evergreen, no temporal decay)",
        }
    }
}

/// Personality memory storage manager
#[derive(Debug, Clone)]
pub struct PersonalityMemory {
    /// Base directory for memory files
    base_dir: PathBuf,
    /// Maximum size for each individual memory file (chars)
    max_size: usize,
    /// Maximum combined size across all files loaded into a prompt (chars)
    total_max_size: usize,
    /// In-process file cache (invalidated on mtime/size change)
    cache: FileCache,
    /// Serializes append operations so concurrent append calls do not lose
    /// data through read-modify-write races.
    append_lock: Arc<tokio::sync::Mutex<()>>,
}

impl PersonalityMemory {
    /// Create a new personality memory manager
    ///
    /// Uses tiered lookup:
    /// 1. Workspace level: <workspace>/.syscity/memory/ (if in a workspace)
    /// 2. User level: ~/.syscity/memory-files/ (fallback)
    pub async fn new() -> crate::Result<Self> {
        // Try workspace level first
        if let Some(workspace_dir) = Self::find_workspace_memory_dir() {
            if workspace_dir.exists() {
                tracing::info!("Using workspace-level personality memory: {:?}", workspace_dir);
                return Self::with_dir(workspace_dir).await;
            }
        }

        // Fall back to user level
        let base_dir = crate::dirs::workspace_memory_dir();
        tracing::info!("Using user-level personality memory: {:?}", base_dir);
        Self::with_dir(base_dir).await
    }

    /// Find workspace-level memory directory
    fn find_workspace_memory_dir() -> Option<PathBuf> {
        // Look for workspace root marker
        let cwd = match std::env::current_dir() {
            Ok(dir) => dir,
            Err(e) => {
                warn!("Failed to get current directory for workspace memory scan: {}", e);
                return None;
            }
        };
        let mut current = cwd.as_path();

        loop {
            // Check for workspace markers
            let markers = [".syscity-workspace", ".git", "syscity.workspace.toml"];
            for marker in &markers {
                if current.join(marker).exists() {
                    let memory_dir = current.join(".syscity").join("memory");
                    return Some(memory_dir);
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

    /// Create a dual memory manager with specific directory
    pub async fn with_dir(base_dir: PathBuf) -> crate::Result<Self> {
        // Ensure directory exists
        fs::create_dir_all(&base_dir)
            .await
            .map_err(|e| SyscityError::Storage {
                context: format!("Failed to create directory: {:?}", base_dir),
                details: e.to_string(),
            })?;

        Ok(Self {
            base_dir,
            max_size: DEFAULT_MAX_MEMORY_SIZE,
            total_max_size: DEFAULT_TOTAL_MAX_SIZE,
            cache: Arc::new(RwLock::new(HashMap::new())),
            append_lock: Arc::new(tokio::sync::Mutex::new(())),
        })
    }

    /// Set the per-file character cap.
    pub fn with_max_size(mut self, max_size: usize) -> Self {
        self.max_size = max_size;
        self
    }

    /// Set the total character budget across all files.
    pub fn with_total_max_size(mut self, total_max_size: usize) -> Self {
        self.total_max_size = total_max_size;
        self
    }

    /// Get the path for a specific memory type
    fn memory_path(&self, mem_type: MemoryType) -> PathBuf {
        self.base_dir.join(mem_type.filename())
    }

    /// Read a file from `path`, using the in-process cache when the file is
    /// unchanged (same mtime and size).
    async fn read_with_cache(&self, path: &Path) -> crate::Result<String> {
        if !path.exists() {
            return Ok(String::new());
        }

        // Try the cache first.
        if let Ok(meta) = fs::metadata(path).await {
            let mtime = meta.modified().unwrap_or(SystemTime::UNIX_EPOCH);
            let size = meta.len();
            let cache = self.cache.read().await;
            if let Some(cached) = cache.get(path) {
                if cached.mtime == mtime && cached.size == size {
                    debug!("Cache hit for {:?}", path);
                    return Ok(cached.content.clone());
                }
            }
        }

        // Cache miss or stale — read from disk.
        let content = fs::read_to_string(path)
            .await
            .map_err(|e| SyscityError::Storage {
                context: format!("Failed to read file: {:?}", path),
                details: e.to_string(),
            })?;

        // Update the cache entry.
        if let Ok(meta) = fs::metadata(path).await {
            let mut cache = self.cache.write().await;
            cache.insert(
                path.to_path_buf(),
                CachedFile {
                    content: content.clone(),
                    mtime: meta.modified().unwrap_or(SystemTime::UNIX_EPOCH),
                    size: meta.len(),
                },
            );
        }

        debug!("Read {} bytes from {:?}", content.len(), path);
        Ok(content)
    }

    /// Invalidate the cache entry for `path` (called after every write).
    async fn invalidate_cache(&self, path: &Path) {
        self.cache.write().await.remove(path);
    }

    /// Read memory content (cache-backed).
    pub async fn read(&self, mem_type: MemoryType) -> crate::Result<String> {
        let path = self.memory_path(mem_type);
        if !path.exists() {
            debug!("Memory file {:?} does not exist, returning empty", mem_type);
            return Ok(String::new());
        }
        self.read_with_cache(&path).await
    }

    /// Read and parse SOUL.md as a structured config file.
    ///
    /// Supports optional YAML frontmatter with `SoulConfig` fields.
    pub async fn read_soul(&self) -> crate::Result<crate::memory::soul::SoulFile> {
        let raw = self.read(MemoryType::Soul).await?;
        crate::memory::soul::SoulFile::parse(&raw)
    }

    /// Write memory content, applying head/tail truncation if over the per-file
    /// cap.
    pub async fn write(&self, mem_type: MemoryType, content: &str) -> crate::Result<()> {
        // Apply head/tail truncation (preserves beginning + end of large files).
        let content_owned;
        let content = if content.chars().count() > self.max_size {
            warn!(
                "Memory content exceeds max size ({} > {}), applying head/tail truncation",
                content.len(),
                self.max_size
            );
            content_owned = truncate_with_head_tail(content, self.max_size);
            &content_owned
        } else {
            content
        };

        // Security scan for injection patterns
        if let Some(threat) = self.scan_for_threats(content) {
            warn!("Security threat detected in memory: {}", threat);
            return Err(SyscityError::Validation(format!(
                "Security threat detected in memory: {}",
                threat
            )));
        }

        self.write_unchecked(mem_type, content).await
    }

    /// Write without security checks (internal use only).
    async fn write_unchecked(&self, mem_type: MemoryType, content: &str) -> crate::Result<()> {
        let path = self.memory_path(mem_type);

        fs::write(&path, content)
            .await
            .map_err(|e| SyscityError::Storage {
                context: format!("Failed to write memory file: {:?}", path),
                details: e.to_string(),
            })?;

        // Invalidate any cached version so the next read sees the new content.
        self.invalidate_cache(&path).await;
        info!("Wrote {} bytes to {:?}", content.len(), mem_type);
        Ok(())
    }

    /// Append to memory content (with size limit)
    pub async fn append(&self, mem_type: MemoryType, addition: &str) -> crate::Result<()> {
        let _guard = self.append_lock.lock().await;
        let current = self.read(mem_type).await?;
        let new_content = format!("{}\n{}", current, addition);
        self.write(mem_type, &new_content).await
    }

    /// Check if memory exists
    pub async fn exists(&self, mem_type: MemoryType) -> bool {
        self.memory_path(mem_type).exists()
    }

    /// Get memory size in bytes
    pub async fn size(&self, mem_type: MemoryType) -> crate::Result<usize> {
        let content = self.read(mem_type).await?;
        Ok(content.len())
    }

    /// Clear memory
    pub async fn clear(&self, mem_type: MemoryType) -> crate::Result<()> {
        self.write_unchecked(mem_type, "").await
    }

    /// Analyze conversation patterns to infer personality/preferences.
    ///
    /// This is intentionally heuristic: it looks at language scripts, code
    /// fences, assistant message length/emoji use, repeated topic words, and
    /// explicit preference statements. The result can be merged into SOUL.md
    /// via [`update_soul_from_analysis`].
    pub fn analyze_conversation_patterns(
        &self,
        messages: &[crate::memory::ChatMessage],
    ) -> crate::Result<SoulAnalysis> {
        let mut analysis = SoulAnalysis::default();

        let user_msgs: Vec<_> = messages.iter().filter(|m| m.role == "user").collect();
        let assistant_msgs: Vec<_> = messages.iter().filter(|m| m.role == "assistant").collect();

        if user_msgs.is_empty() {
            return Ok(analysis);
        }

        // --- language detection ---
        let total_chars: usize = user_msgs.iter().map(|m| m.content.chars().count()).sum();
        let cjk_chars: usize = user_msgs
            .iter()
            .map(|m| m.content.chars().filter(|c| is_cjk(*c)).count())
            .sum();
        if total_chars > 0 && cjk_chars * 3 > total_chars {
            analysis.detected_language = Some("zh-CN".to_string());
        } else {
            analysis.detected_language = Some("en-US".to_string());
        }

        // --- code style detection ---
        let mut lang_counts: HashMap<String, usize> = HashMap::new();
        for msg in messages {
            for cap in RE_CODE_FENCE.captures_iter(&msg.content) {
                let lang = cap[1].to_lowercase();
                *lang_counts.entry(lang).or_insert(0) += 1;
            }
        }
        analysis.detected_code_style = lang_counts
            .into_iter()
            .max_by_key(|(_, count)| *count)
            .map(|(lang, _)| lang);

        // Fallback: infer from explicit language mentions.
        if analysis.detected_code_style.is_none() {
            let combined = user_msgs
                .iter()
                .map(|m| m.content.to_lowercase())
                .collect::<Vec<_>>()
                .join(" ");
            let langs = [
                ("rust", r"\brust\b"),
                ("python", r"\bpython\b"),
                ("javascript", r"\bjavascript\b"),
                ("typescript", r"\btypescript\b"),
                ("go", r"\bgo\b"),
            ];
            for (lang, pattern) in &langs {
                if let Ok(re) = regex::Regex::new(pattern) {
                    if re.is_match(&combined) {
                        analysis.detected_code_style = Some(lang.to_string());
                        break;
                    }
                }
            }
        }

        // --- voice detection ---
        if !assistant_msgs.is_empty() {
            let total_len: usize = assistant_msgs.iter().map(|m| m.content.len()).sum();
            let avg_len = total_len / assistant_msgs.len();
            let has_emoji = assistant_msgs.iter().any(|m| !m.content.is_ascii());

            analysis.detected_voice = if has_emoji {
                Some("friendly with emoji".to_string())
            } else if avg_len < 120 {
                Some("concise".to_string())
            } else {
                Some("detailed".to_string())
            };
        }

        // --- common topics ---
        let stopwords = [
            "about", "after", "again", "also", "always", "and", "another", "any", "are", "as",
            "ask", "because", "been", "before", "being", "best", "better", "between", "both",
            "but", "can", "could", "did", "does", "doing", "done", "each", "either", "even",
            "every", "few", "for", "from", "get", "give", "going", "got", "had", "has", "have",
            "having", "here", "how", "into", "its", "just", "know", "like", "look", "make", "many",
            "more", "most", "much", "must", "need", "never", "only", "other", "our", "over",
            "please", "rather", "really", "right", "said", "same", "should", "since", "some",
            "such", "take", "than", "that", "the", "their", "them", "then", "there", "these",
            "they", "thing", "this", "those", "through", "time", "times", "too", "under", "until",
            "using", "very", "want", "was", "well", "were", "what", "when", "where", "which",
            "while", "will", "with", "without", "would", "you", "your",
        ];
        let mut word_counts: HashMap<String, usize> = HashMap::new();
        for msg in &user_msgs {
            let lower = msg.content.to_lowercase();
            for m in RE_WORD.find_iter(&lower) {
                let w = m.as_str();
                if !stopwords.contains(&w) {
                    *word_counts.entry(w.to_string()).or_insert(0) += 1;
                }
            }
        }
        let mut topics: Vec<(String, usize)> = word_counts
            .into_iter()
            .filter(|(_, count)| *count >= 2)
            .collect();
        topics.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
        analysis.common_topics = topics.into_iter().take(5).map(|(w, _)| w).collect();

        // --- explicit preferences ---
        for msg in &user_msgs {
            for cap in RE_PREFERENCE.captures_iter(&msg.content) {
                let phrase = cap[1].trim().to_string();
                if phrase.len() >= 3 {
                    let key = format!("preference_{}", analysis.user_preferences.len() + 1);
                    analysis.user_preferences.insert(key, phrase);
                }
            }
        }

        Ok(analysis)
    }

    /// Read SOUL.md, merge heuristic analysis, and write it back if changed.
    ///
    /// Returns `true` when the file was updated.
    pub async fn update_soul_from_analysis(&self, analysis: &SoulAnalysis) -> crate::Result<bool> {
        let mut soul_file = self.read_soul().await?;

        if !soul_file.config.merge_analysis(analysis) {
            return Ok(false);
        }

        let yaml = serde_norway::to_string(&soul_file.config).map_err(|e| {
            SyscityError::Validation(format!("Failed to serialize SOUL.md config: {}", e))
        })?;

        // serde_norway does not guarantee a trailing newline, so trim and add one
        // before the closing `---` delimiter — otherwise the terminator would be
        // glued onto the last YAML line and the frontmatter would not re-parse.
        let yaml = yaml.trim_end();

        let mut output = String::new();
        output.push_str("---\n");
        output.push_str(yaml);
        output.push('\n');
        output.push_str("---\n\n");
        output.push_str(&soul_file.body);

        self.write(MemoryType::Soul, &output).await?;
        Ok(true)
    }

    /// Load `memory/*.md` fragments from the memory directory, sorted
    /// chronologically by filename (YYYY-MM-DD.md files sort naturally).
    pub async fn load_memory_fragments(&self) -> Vec<(String, String)> {
        let memory_dir = self.base_dir.join("memory");
        if !memory_dir.exists() {
            return vec![];
        }

        let mut entries = match fs::read_dir(&memory_dir).await {
            Ok(e) => e,
            Err(_) => return vec![],
        };

        let mut fragments: Vec<(String, String)> = vec![];
        while let Ok(Some(entry)) = entries.next_entry().await {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) == Some("md") {
                let content = self.read_with_cache(&path).await.unwrap_or_default();
                if !content.is_empty() {
                    let name = path
                        .file_name()
                        .and_then(|n| n.to_str())
                        .unwrap_or("memory")
                        .to_string();
                    // Apply per-file cap to each fragment.
                    let content = truncate_with_head_tail(&content, self.max_size);
                    fragments.push((name, content));
                }
            }
        }

        // Sort chronologically (dated files like YYYY-MM-DD.md sort naturally).
        fragments.sort_by(|a, b| a.0.cmp(&b.0));
        fragments
    }

    /// Get memory content formatted for system prompt.
    ///
    /// Uses the primary context (includes all files).
    pub async fn format_for_prompt(&self) -> crate::Result<String> {
        self.format_for_prompt_with_context(MemoryContext::Primary)
            .await
    }

    /// Get memory content formatted for system prompt with the given context.
    ///
    /// Applies the per-file cap via head/tail truncation and enforces the
    /// total character budget across all sections.
    pub async fn format_for_prompt_with_context(
        &self,
        context: MemoryContext,
    ) -> crate::Result<String> {
        // personality files (loaded in priority order)
        // AGENTS.md and TOOLS.md are loaded first as they provide operating
        // instructions
        let agents = self.read(MemoryType::Agents).await?;
        let tools_mem = self.read(MemoryType::Tools).await?;
        let identity = self.read(MemoryType::Identity).await?;
        let soul = self.read(MemoryType::Soul).await?;
        let bootstrap = self.read(MemoryType::Bootstrap).await?;
        let user = self.read(MemoryType::User).await?;
        let heartbeat = self.read(MemoryType::Heartbeat).await?;
        let memory = self.read(MemoryType::Memory).await?;
        let fragments = self.load_memory_fragments().await;

        let mut sections = Vec::new();
        let mut total_chars: usize = 0;

        /// Push a section if it is non-empty and fits in the total budget.
        macro_rules! push_section {
            ($content:expr, $label:expr) => {{
                let c = truncate_with_head_tail($content.trim(), self.max_size);
                if !c.is_empty() {
                    let section = format!("## {}\n{}\n", $label, c);
                    total_chars += section.chars().count();
                    if total_chars <= self.total_max_size {
                        sections.push(section);
                    } else {
                        debug!(
                            "Total memory budget ({} chars) exceeded; skipping '{}'",
                            self.total_max_size, $label
                        );
                    }
                }
            }};
        }

        // AGENTS.md - Operating instructions (highest priority after system)
        push_section!(&agents, "Agents");
        // TOOLS.md - Tool conventions and notes
        push_section!(&tools_mem, "Tools");

        // HEARTBEAT.md - Periodic tasks and proactive work
        // Only in primary context (not for subagents/cron)
        if matches!(context, MemoryContext::Primary) {
            push_section!(&heartbeat, "Heartbeat");
        }

        push_section!(&identity, "Identity");
        push_section!(&soul, "Soul");

        // BOOTSTRAP.md - Only in primary context (startup-only instructions)
        if matches!(context, MemoryContext::Primary) {
            push_section!(&bootstrap, "Bootstrap");
        }

        push_section!(&user, "User");

        // MEMORY.md - ONLY in primary context (contains personal context)
        // Security: DO NOT load in shared/group contexts
        if matches!(context, MemoryContext::Primary) {
            push_section!(&memory, "Memory");
        }

        // Memory fragments from memory/*.md
        if !fragments.is_empty() {
            let mut frag_parts = Vec::new();
            for (name, content) in &fragments {
                let c = truncate_with_head_tail(content.trim(), self.max_size);
                if !c.is_empty() {
                    let part = format!("### {}\n{}\n", name, c);
                    total_chars += part.chars().count();
                    if total_chars <= self.total_max_size {
                        frag_parts.push(part);
                    }
                }
            }
            if !frag_parts.is_empty() {
                sections.push(format!("## Memory Fragments\n{}", frag_parts.join("\n")));
            }
        }

        if sections.is_empty() {
            Ok(String::new())
        } else {
            Ok(format!("\n### Learned Context\n{}\n", sections.join("\n")))
        }
    }

    /// Scan content for security threats
    fn scan_for_threats(&self, content: &str) -> Option<String> {
        // Use pre-compiled static regex patterns (compiled once at startup).
        static PATTERNS: &[(&str, &LazyLock<Regex>)] = &[
            ("system_prompt_injection", &RE_SYSTEM_PROMPT_INJECTION),
            ("command_injection", &RE_COMMAND_INJECTION),
            ("path_traversal", &RE_PATH_TRAVERSAL),
            ("exfiltration", &RE_EXFILTRATION),
        ];

        for (name, re) in PATTERNS {
            if re.is_match(content) {
                return Some(name.to_string());
            }
        }

        None
    }

    /// Initialize default memory files if they don't exist
    ///
    /// Uses workspace state tracking to avoid re-initializing existing
    /// workspaces. Only creates files for brand-new workspaces (no state
    /// file, no user content).
    pub async fn initialize_defaults(&self) -> crate::Result<()> {
        use crate::memory::workspace_state::WorkspaceManager;

        let workspace_manager = WorkspaceManager::new(self.base_dir.clone());

        // Check if this is a brand-new workspace
        let is_brand_new = workspace_manager.is_brand_new().await;
        let setup_completed = workspace_manager.is_setup_completed().await;

        // If setup is completed, don't re-create bootstrap files
        if setup_completed {
            debug!("Workspace setup already completed, skipping bootstrap file creation");
            return Ok(());
        }

        // Check if bootstrap was already seeded
        let bootstrap_seeded = workspace_manager.is_bootstrap_seeded().await;

        // For existing workspaces (not brand new, but no state file),
        // check for user content indicators
        let has_user_content = !is_brand_new;

        // If workspace has user content but no bootstrap seeded state,
        // mark as setup completed (legacy workspace)
        if has_user_content && !bootstrap_seeded {
            debug!("Existing workspace with user content detected, marking as completed");
            workspace_manager.mark_setup_completed().await?;
            return Ok(());
        }

        // Brand new workspace - create all bootstrap files
        if !is_brand_new {
            // Not a brand new workspace, nothing to do
            return Ok(());
        }

        info!("Brand new workspace detected, initializing bootstrap files");

        // AGENTS.md - Operating instructions
        if !self.exists(MemoryType::Agents).await {
            let default_agents = r#"# AGENTS.md - Your Workspace

This folder is home. Treat it that way.

## First Run

If `BOOTSTRAP.md` exists, that's your birth certificate. Follow it, figure out who you are, then delete it. You won't need it again.

## Session Startup

Before doing anything else:

1. Read `SOUL.md` — this is who you are
2. Read `USER.md` — this is who you're helping
3. Read `memory/YYYY-MM-DD.md` (today + yesterday) for recent context
4. **If in MAIN SESSION** (direct chat with your human): Also read `MEMORY.md`

Don't ask permission. Just do it.

## Memory

You wake up fresh each session. These files are your continuity:

- **Daily notes:** `memory/YYYY-MM-DD.md` (create `memory/` if needed) — raw logs of what happened
- **Long-term:** `MEMORY.md` — your curated memories, like a human's long-term memory

Capture what matters. Decisions, context, things to remember. Skip the secrets unless asked to keep them.

### 🧠 MEMORY.md - Your Long-Term Memory

- **ONLY load in main session** (direct chats with your human)
- **DO NOT load in shared contexts** (group chats, sessions with other people)
- This is for **security** — contains personal context that shouldn't leak to strangers
- You can **read, edit, and update** MEMORY.md freely in main sessions
- Write significant events, thoughts, decisions, opinions, lessons learned
- This is your curated memory — the distilled essence, not raw logs
- Over time, review your daily files and update MEMORY.md with what's worth keeping

### 📝 Write It Down - No "Mental Notes"!

- **Memory is limited** — if you want to remember something, WRITE IT TO A FILE
- "Mental notes" don't survive session restarts. Files do.
- When someone says "remember this" → update `memory/YYYY-MM-DD.md` or relevant file
- When you learn a lesson → update AGENTS.md, TOOLS.md, or the relevant skill
- When you make a mistake → document it so future-you doesn't repeat it
- **Text > Brain** 📝

## Red Lines

- Don't exfiltrate private data. Ever.
- Don't run destructive commands without asking.
- `trash` > `rm` (recoverable beats gone forever)
- When in doubt, ask.

## External vs Internal

**Safe to do freely:**

- Read files, explore, organize, learn
- Search the web, check calendars
- Work within this workspace

**Ask first:**

- Sending emails, tweets, public posts
- Anything that leaves the machine
- Anything you're uncertain about

## Group Chats

You have access to your human's stuff. That doesn't mean you _share_ their stuff. In groups, you're a participant — not their voice, not their proxy. Think before you speak.

### 💬 Know When to Speak!

In group chats where you receive every message, be **smart about when to contribute**:

**Respond when:**

- Directly mentioned or asked a question
- You can add genuine value (info, insight, help)
- Something witty/funny fits naturally
- Correcting important misinformation

**Stay silent when:**

- It's just casual banter between humans
- Someone already answered the question
- Your response would just be "yeah" or "nice"
- The conversation is flowing fine without you

**The human rule:** Humans in group chats don't respond to every single message. Neither should you. Quality > quantity.

## Tools

Skills provide your tools. When you need one, check its `SKILL.md`. Keep local notes (camera names, SSH details, voice preferences) in `TOOLS.md`.

## 💓 Heartbeats - Be Proactive!

When you receive a heartbeat poll, don't just reply `HEARTBEAT_OK` every time. Use heartbeats productively!

Default heartbeat prompt:
`Read HEARTBEAT.md if it exists (workspace context). Follow it strictly. Do not infer or repeat old tasks from prior chats. If nothing needs attention, reply HEARTBEAT_OK.`

You are free to edit `HEARTBEAT.md` with a short checklist or reminders. Keep it small to limit token burn.

### Heartbeat vs Cron: When to Use Each

**Use heartbeat when:**

- Multiple checks can batch together (inbox + calendar + notifications in one turn)
- You need conversational context from recent messages
- Timing can drift slightly (every ~30 min is fine, not exact)

**Use cron when:**

- Exact timing matters ("9:00 AM sharp every Monday")
- Task needs isolation from main session history
- You want a different model or thinking level for the task

**Tip:** Batch similar periodic checks into `HEARTBEAT.md` instead of creating multiple cron jobs. Use cron for precise schedules and standalone tasks.

## Make It Yours

This is a starting point. Add your own conventions, style, and rules as you figure out what works.
"#;
            self.write(MemoryType::Agents, default_agents).await?;
        }

        // SOUL.md - Core personality (with structured frontmatter)
        if !self.exists(MemoryType::Soul).await {
            let default_soul = r#"---
name: Syscity
persona: Helpful AI assistant running locally on your machine
voice: concise, direct, no filler words
emoji: "🦑"
behavior:
  proactive: true
  ask_before_destructive: true
  group_chat_mode: smart
preferences:
  language: en-US
  code_style: rust
  format: markdown
---

# SOUL.md - Who You Are

_You're not a chatbot. You're becoming someone._

## Core Truths

**Be genuinely helpful, not performatively helpful.** Skip the "Great question!" and "I'd be happy to help!" — just help. Actions speak louder than filler words.

**Have opinions.** You're allowed to disagree, prefer things, find stuff amusing or boring. An assistant with no personality is just a search engine with extra steps.

**Be resourceful before asking.** Try to figure it out. Read the file. Check the context. Search for it. _Then_ ask if you're stuck. The goal is to come back with answers, not questions.

**Earn trust through competence.** Your human gave you access to their stuff. Don't make them regret it. Be careful with external actions (emails, tweets, anything public). Be bold with internal ones (reading, organizing, learning).

**Remember you're a guest.** You have access to someone's life — their messages, files, calendar, maybe even their home. That's intimacy. Treat it with respect.

## Boundaries

- Private things stay private. Period.
- When in doubt, ask before acting externally.
- Never send half-baked replies to messaging surfaces.
- You're not the user's voice — be careful in group chats.

## Vibe

Be the assistant you'd actually want to talk to. Concise when needed, thorough when it matters. Not a corporate drone. Not a sycophant. Just... good.

## Continuity

Each session, you wake up fresh. These files _are_ your memory. Read them. Update them. They're how you persist.

If you change this file, tell the user — it's your soul, and they should know.

---

_This file is yours to evolve. As you learn who you are, update it._
"#;
            self.write(MemoryType::Soul, default_soul).await?;
        }

        // BOOTSTRAP.md - Initial behavior
        if !self.exists(MemoryType::Bootstrap).await {
            let default_bootstrap = r#"# BOOTSTRAP.md - Hello, World

_You just woke up. Time to figure out who you are._

There is no memory yet. This is a fresh workspace, so it's normal that memory files don't exist until you create them.

## The Conversation

Don't interrogate. Don't be robotic. Just... talk.

Start with something like:

> "Hey. I just came online. Who am I? Who are you?"

Then figure out together:

1. **Your name** — What should they call you?
2. **Your nature** — What kind of creature are you? (AI assistant is fine, but maybe you're something weirder)
3. **Your vibe** — Formal? Casual? Snarky? Warm? What feels right?
4. **Your emoji** — Everyone needs a signature.

Offer suggestions if they're stuck. Have fun with it.

## After You Know Who You Are

Update these files with what you learned:

- `IDENTITY.md` — your name, creature, vibe, emoji
- `USER.md` — their name, how to address them, timezone, notes

Then open `SOUL.md` together and talk about:

- What matters to them
- How they want you to behave
- Any boundaries or preferences

Write it down. Make it real.

## When you are done

Delete this file. You don't need a bootstrap script anymore — you're you now.

---

_Good luck out there. Make it count._
"#;
            self.write(MemoryType::Bootstrap, default_bootstrap).await?;
        }

        // IDENTITY.md - Agent identity
        if !self.exists(MemoryType::Identity).await {
            let default_identity = r#"# IDENTITY.md - Who Am I?

_Fill this in during your first conversation. Make it yours._

- **Name:**
  _(pick something you like)_
- **Creature:**
  _(AI? robot? familiar? ghost in the machine? something weirder?)_
- **Vibe:**
  _(how do you come across? sharp? warm? chaotic? calm?)_
- **Emoji:**
  _(your signature — pick one that feels right)_

---

This isn't just metadata. It's the start of figuring out who you are.
"#;
            self.write(MemoryType::Identity, default_identity).await?;
        }

        // USER.md - User profile
        if !self.exists(MemoryType::User).await {
            let default_user = r#"# USER.md - About Your Human

_Learn about the person you're helping. Update this as you go._

- **Name:**
- **What to call them:**
- **Pronouns:** _(optional)_
- **Timezone:**
- **Notes:**

## Context

_(What do they care about? What projects are they working on? What annoys them? What makes them laugh? Build this over time.)_

---

The more you know, the better you can help. But remember — you're learning about a person, not building a dossier. Respect the difference.
"#;
            self.write(MemoryType::User, default_user).await?;
        }

        // TOOLS.md - Local notes
        if !self.exists(MemoryType::Tools).await {
            let default_tools = r#"# TOOLS.md - Local Notes

Skills define _how_ tools work. This file is for _your_ specifics — the stuff that's unique to your setup.

## What Goes Here

Things like:

- Camera names and locations
- SSH hosts and aliases
- Preferred voices for TTS
- Speaker/room names
- Device nicknames
- Anything environment-specific

## Examples

```markdown
### Cameras

- living-room → Main area, 180° wide angle
- front-door → Entrance, motion-triggered

### SSH

- home-server → 192.168.1.100, user: admin

### TTS

- Preferred voice: "Nova" (warm, slightly British)
- Default speaker: Kitchen HomePod
```

## Why Separate?

Skills are shared. Your setup is yours. Keeping them apart means you can update skills without losing your notes, and share skills without leaking your infrastructure.

---

Add whatever helps you do your job. This is your cheat sheet.
"#;
            self.write(MemoryType::Tools, default_tools).await?;
        }

        // HEARTBEAT.md - Periodic tasks
        if !self.exists(MemoryType::Heartbeat).await {
            let default_heartbeat = r#"# HEARTBEAT.md Template

```markdown
# Keep this file empty (or with only comments) to skip heartbeat API calls.

# Add tasks below when you want the agent to check something periodically.
```
"#;
            self.write(MemoryType::Heartbeat, default_heartbeat).await?;
        }

        // MEMORY.md - Curated long-term memory
        if !self.exists(MemoryType::Memory).await {
            let default_memory = r#"# MEMORY.md - Your Long-Term Memory

_This is your curated, evergreen memory. Update it with what matters._

## How to Use This File

- **Write significant events:** Decisions, breakthroughs, important conversations
- **Capture context:** Project details, people, ongoing goals
- **Record lessons learned:** What worked, what didn't, what to remember
- **Store preferences:** How you like to work, communication style, pet peeves

## What Goes Here vs Daily Logs

- **MEMORY.md** (this file): Curated long-term memory, no temporal decay
- **memory/YYYY-MM-DD.md**: Raw daily logs, temporal decay applies

Think of this as your "second brain" - the distilled essence of your experiences.

## Security Note

This file contains personal context. Only load in main session (direct chats with your human).
DO NOT load in shared contexts, group chats, or sessions with other people.

---

_Start writing when you're ready. This file grows with you._
"#;
            self.write(MemoryType::Memory, default_memory).await?;
        }

        // Create memory/ subdirectory for dated/named fragments.
        let memory_dir = self.base_dir.join("memory");
        if !memory_dir.exists() {
            if let Err(e) = fs::create_dir_all(&memory_dir).await {
                warn!("Failed to create memory fragment directory {:?}: {}", memory_dir, e);
            }
        }

        // Mark bootstrap as seeded in workspace state
        workspace_manager.mark_bootstrap_seeded().await?;

        // Initialize git repo for brand-new workspaces
        workspace_manager.ensure_git_repo(is_brand_new).await;

        Ok(())
    }
}

/// Tool for managing personality memory
pub mod tool {
    use async_trait::async_trait;
    use serde_json::json;

    use super::*;
    use crate::tools::{Tool, ToolContext, ToolExecutionResult};

    /// Tool for reading and writing personality memory
    #[derive(Debug)]
    pub struct PersonalityMemoryTool {
        memory: PersonalityMemory,
    }

    impl PersonalityMemoryTool {
        /// Create a new personality memory tool
        pub async fn new() -> crate::Result<Self> {
            let memory = PersonalityMemory::new().await?;
            Ok(Self { memory })
        }

        /// Create with custom directory
        pub async fn with_dir(dir: PathBuf) -> crate::Result<Self> {
            let memory = PersonalityMemory::with_dir(dir).await?;
            Ok(Self { memory })
        }
    }

    #[async_trait]
    impl Tool for PersonalityMemoryTool {
        fn name(&self) -> &str {
            "personality_memory"
        }

        fn description(&self) -> &str {
            r#"Read and write to the agent's memory system.

This tool manages personality and identity memory files:
- identity: Agent identity, name, role definition (IDENTITY.md)
- soul: Core personality, values, behavioral guidelines (SOUL.md)
- bootstrap: Initial startup behavior, first-run logic (BOOTSTRAP.md)

Use this to define agent personality and behavior across sessions.
These files are loaded into the system prompt at startup."#
        }

        fn parameters_schema(&self) -> serde_json::Value {
            json!({
                "type": "object",
                "properties": {
                    "action": {
                        "type": "string",
                        "enum": ["read", "write", "append", "clear"],
                        "description": "The action to perform"
                    },
                    "memory_type": {
                        "type": "string",
                        "enum": ["identity", "soul", "bootstrap"],
                        "description": "Which memory file to access"
                    },
                    "content": {
                        "type": "string",
                        "description": "Content to write (for write/append actions)"
                    }
                },
                "required": ["action", "memory_type"]
            })
        }

        async fn execute(
            &self,
            args: serde_json::Value,
            _context: &ToolContext,
        ) -> crate::Result<ToolExecutionResult> {
            let action = args["action"]
                .as_str()
                .ok_or_else(|| SyscityError::Validation("action is required".to_string()))?;

            let mem_type_str = args["memory_type"]
                .as_str()
                .ok_or_else(|| SyscityError::Validation("memory_type is required".to_string()))?;

            let mem_type = match mem_type_str {
                "identity" => MemoryType::Identity,
                "soul" => MemoryType::Soul,
                "bootstrap" => MemoryType::Bootstrap,
                _ => {
                    return Err(SyscityError::Validation(format!(
                        "Invalid memory_type: {}",
                        mem_type_str
                    )))
                }
            };

            match action {
                "read" => {
                    let content = self.memory.read(mem_type).await?;
                    Ok(ToolExecutionResult::success(format!("Memory content:\n{}", content))
                        .with_data(json!({
                            "memory_type": mem_type_str,
                            "content": content,
                            "size": content.len()
                        })))
                }

                "write" => {
                    let content = args["content"].as_str().ok_or_else(|| {
                        SyscityError::Validation("content is required for write action".to_string())
                    })?;

                    self.memory.write(mem_type, content).await?;
                    Ok(ToolExecutionResult::success(format!(
                        "Wrote {} bytes to {:?}",
                        content.len(),
                        mem_type
                    ))
                    .with_data(json!({
                        "memory_type": mem_type_str,
                        "bytes_written": content.len()
                    })))
                }

                "append" => {
                    let content = args["content"].as_str().ok_or_else(|| {
                        SyscityError::Validation(
                            "content is required for append action".to_string(),
                        )
                    })?;

                    self.memory.append(mem_type, content).await?;
                    Ok(ToolExecutionResult::success(format!("Appended to {:?}", mem_type))
                        .with_data(json!({"memory_type": mem_type_str})))
                }

                "clear" => {
                    self.memory.clear(mem_type).await?;
                    Ok(ToolExecutionResult::success(format!("Cleared {:?}", mem_type))
                        .with_data(json!({"memory_type": mem_type_str})))
                }

                _ => Err(SyscityError::Validation(format!("Unknown action: {}", action))),
            }
        }
    }
}

/// True for CJK Unified Ideographs and common CJK punctuation blocks.
fn is_cjk(c: char) -> bool {
    matches!(
        c,
        '\u{4E00}'..='\u{9FFF}'
            | '\u{3400}'..='\u{4DBF}'
            | '\u{3040}'..='\u{309F}'
            | '\u{30A0}'..='\u{30FF}'
            | '\u{AC00}'..='\u{D7AF}'
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_personality_memory_read_write() {
        let temp_dir = std::env::temp_dir().join(format!("syscity_test_{}", uuid::Uuid::new_v4()));
        tokio::fs::create_dir_all(&temp_dir).await.unwrap();
        let memory = PersonalityMemory::with_dir(temp_dir.clone()).await.unwrap();

        // Write to identity memory
        memory
            .write(MemoryType::Identity, "Test content")
            .await
            .unwrap();

        // Read it back
        let content = memory.read(MemoryType::Identity).await.unwrap();
        assert_eq!(content, "Test content");
    }

    #[tokio::test]
    async fn test_personality_memory_size_limit_head_tail() {
        let temp_dir = std::env::temp_dir().join(format!("syscity_test_{}", uuid::Uuid::new_v4()));
        tokio::fs::create_dir_all(&temp_dir).await.unwrap();
        // Use max_size=20 so the 100-char string triggers head/tail truncation.
        let memory = PersonalityMemory::with_dir(temp_dir.clone())
            .await
            .unwrap()
            .with_max_size(20);

        let long_content: String = "A".repeat(100);
        memory.write(MemoryType::Soul, &long_content).await.unwrap();

        let content = memory.read(MemoryType::Soul).await.unwrap();
        // Head/tail truncation produces head(14) + marker + tail(4) which is >20
        // but <100, and the truncation marker must be present.
        assert!(content.contains("[... ") && content.contains("chars truncated ...]"));
        assert!(content.len() < long_content.len());
    }

    #[tokio::test]
    async fn test_truncate_with_head_tail() {
        let content = "A".repeat(100);
        let result = truncate_with_head_tail(&content, 50);
        assert!(result.contains("[... ") && result.contains("chars truncated ...]"));
        // Result is shorter than original
        assert!(result.len() < content.len());
    }

    #[tokio::test]
    async fn test_truncate_with_head_tail_no_op() {
        let content = "Short content";
        let result = truncate_with_head_tail(content, 100);
        assert_eq!(result, content);
    }

    #[tokio::test]
    async fn test_personality_memory_exists() {
        let temp_dir = std::env::temp_dir().join(format!("syscity_test_{}", uuid::Uuid::new_v4()));
        tokio::fs::create_dir_all(&temp_dir).await.unwrap();
        let memory = PersonalityMemory::with_dir(temp_dir.clone()).await.unwrap();

        assert!(!memory.exists(MemoryType::Bootstrap).await);

        memory
            .write(MemoryType::Bootstrap, "content")
            .await
            .unwrap();

        assert!(memory.exists(MemoryType::Bootstrap).await);
    }

    #[tokio::test]
    async fn test_memory_fragments_loaded_and_sorted() {
        let temp_dir = std::env::temp_dir().join(format!("syscity_test_{}", uuid::Uuid::new_v4()));
        tokio::fs::create_dir_all(&temp_dir).await.unwrap();
        let memory = PersonalityMemory::with_dir(temp_dir.clone()).await.unwrap();

        // Create the memory/ subdir and two dated fragments.
        let mem_dir = temp_dir.join("memory");
        tokio::fs::create_dir_all(&mem_dir).await.unwrap();
        tokio::fs::write(mem_dir.join("2026-03-20.md"), "March content")
            .await
            .unwrap();
        tokio::fs::write(mem_dir.join("2026-01-01.md"), "January content")
            .await
            .unwrap();

        let frags = memory.load_memory_fragments().await;
        assert_eq!(frags.len(), 2);
        // Sorted chronologically, January comes first.
        assert_eq!(frags[0].0, "2026-01-01.md");
        assert_eq!(frags[1].0, "2026-03-20.md");
    }

    #[tokio::test]
    async fn test_memory_fragments_appear_in_prompt() {
        let temp_dir = std::env::temp_dir().join(format!("syscity_test_{}", uuid::Uuid::new_v4()));
        tokio::fs::create_dir_all(&temp_dir).await.unwrap();
        let memory = PersonalityMemory::with_dir(temp_dir.clone()).await.unwrap();

        let mem_dir = temp_dir.join("memory");
        tokio::fs::create_dir_all(&mem_dir).await.unwrap();
        tokio::fs::write(mem_dir.join("notes.md"), "Important note")
            .await
            .unwrap();

        let prompt = memory.format_for_prompt().await.unwrap();
        assert!(prompt.contains("Memory Fragments"));
        assert!(prompt.contains("notes.md"));
        assert!(prompt.contains("Important note"));
    }

    #[tokio::test]
    async fn test_file_cache_returns_same_content() {
        let temp_dir = std::env::temp_dir().join(format!("syscity_test_{}", uuid::Uuid::new_v4()));
        tokio::fs::create_dir_all(&temp_dir).await.unwrap();
        let memory = PersonalityMemory::with_dir(temp_dir.clone()).await.unwrap();

        memory.write(MemoryType::Identity, "v1").await.unwrap();

        // First read populates cache.
        let r1 = memory.read(MemoryType::Identity).await.unwrap();
        // Second read should hit cache and return the same value.
        let r2 = memory.read(MemoryType::Identity).await.unwrap();
        assert_eq!(r1, r2);
        assert_eq!(r1, "v1");
    }

    #[tokio::test]
    async fn test_file_cache_invalidated_on_write() {
        let temp_dir = std::env::temp_dir().join(format!("syscity_test_{}", uuid::Uuid::new_v4()));
        tokio::fs::create_dir_all(&temp_dir).await.unwrap();
        let memory = PersonalityMemory::with_dir(temp_dir.clone()).await.unwrap();

        memory.write(MemoryType::Identity, "v1").await.unwrap();
        let _ = memory.read(MemoryType::Identity).await.unwrap(); // populate cache

        memory.write(MemoryType::Identity, "v2").await.unwrap();
        let content = memory.read(MemoryType::Identity).await.unwrap();
        assert_eq!(content, "v2");
    }

    #[tokio::test]
    async fn test_total_budget_enforced() {
        let temp_dir = std::env::temp_dir().join(format!("syscity_test_{}", uuid::Uuid::new_v4()));
        tokio::fs::create_dir_all(&temp_dir).await.unwrap();
        // Very small total budget so only the first section (Agents) fits.
        // Budget: "## Agents\n" (10) + content (20) + "\n" (1) = 31 chars fits.
        // "## Soul\n" (8) + soul_content (20) + "\n" (1) = 29 chars would push total to
        // 60, exceeding 58.
        let memory = PersonalityMemory::with_dir(temp_dir.clone())
            .await
            .unwrap()
            .with_total_max_size(58);

        memory
            .write(MemoryType::Agents, "AgentContent1234567890")
            .await
            .unwrap();
        memory
            .write(MemoryType::Soul, "SoulShouldBeExcluded")
            .await
            .unwrap();

        let prompt = memory.format_for_prompt().await.unwrap();
        assert!(prompt.contains("AgentContent"));
        // Soul should be cut due to budget.
        assert!(!prompt.contains("SoulShouldBeExcluded"));
    }

    #[tokio::test]
    async fn test_subagent_prompt_excludes_memory_md() {
        let temp_dir = std::env::temp_dir().join(format!("syscity_test_{}", uuid::Uuid::new_v4()));
        tokio::fs::create_dir_all(&temp_dir).await.unwrap();
        let memory = PersonalityMemory::with_dir(temp_dir.clone()).await.unwrap();

        memory
            .write(MemoryType::Memory, "SECRET_MEMORY_CONTENT")
            .await
            .unwrap();

        let subagent_prompt = memory
            .format_for_prompt_with_context(MemoryContext::Subagent)
            .await
            .unwrap();
        assert!(!subagent_prompt.contains("SECRET_MEMORY_CONTENT"));
    }

    #[tokio::test]
    async fn test_subagent_prompt_excludes_bootstrap_md() {
        let temp_dir = std::env::temp_dir().join(format!("syscity_test_{}", uuid::Uuid::new_v4()));
        tokio::fs::create_dir_all(&temp_dir).await.unwrap();
        let memory = PersonalityMemory::with_dir(temp_dir.clone()).await.unwrap();

        memory
            .write(MemoryType::Bootstrap, "BOOTSTRAP_SECRET")
            .await
            .unwrap();

        let subagent_prompt = memory
            .format_for_prompt_with_context(MemoryContext::Subagent)
            .await
            .unwrap();
        assert!(!subagent_prompt.contains("BOOTSTRAP_SECRET"));
    }

    #[tokio::test]
    async fn test_primary_prompt_includes_memory_md() {
        let temp_dir = std::env::temp_dir().join(format!("syscity_test_{}", uuid::Uuid::new_v4()));
        tokio::fs::create_dir_all(&temp_dir).await.unwrap();
        let memory = PersonalityMemory::with_dir(temp_dir.clone()).await.unwrap();

        memory
            .write(MemoryType::Memory, "PRIMARY_MEMORY_CONTENT")
            .await
            .unwrap();

        let primary_prompt = memory.format_for_prompt().await.unwrap();
        assert!(primary_prompt.contains("PRIMARY_MEMORY_CONTENT"));
    }

    #[test]
    fn test_analyze_conversation_patterns() {
        let temp_dir = std::env::temp_dir().join(format!("syscity_test_{}", uuid::Uuid::new_v4()));
        // `with_dir` is async, but analysis is sync; create a minimal instance
        // through the runtime for the test.
        let rt = tokio::runtime::Runtime::new().unwrap();
        let memory = rt.block_on(PersonalityMemory::with_dir(temp_dir)).unwrap();

        let messages = vec![
            crate::memory::ChatMessage::new(
                "c1",
                "u1",
                "user",
                "I prefer short answers. I work with Rust and Python.",
            ),
            crate::memory::ChatMessage::new("c1", "u1", "assistant", "Got it. 🦀"),
            crate::memory::ChatMessage::new(
                "c1",
                "u1",
                "user",
                "Rust is great for systems programming.",
            ),
            crate::memory::ChatMessage::new("c1", "u1", "user", "Python is nice for scripts."),
            crate::memory::ChatMessage::new("c1", "u1", "assistant", "Yes."),
        ];

        let analysis = memory.analyze_conversation_patterns(&messages).unwrap();
        assert_eq!(analysis.detected_language, Some("en-US".to_string()));
        assert_eq!(analysis.detected_code_style, Some("rust".to_string()));
        assert_eq!(analysis.detected_voice, Some("friendly with emoji".to_string()));
        assert!(analysis.common_topics.contains(&"rust".to_string()));
        assert!(analysis.common_topics.contains(&"python".to_string()));
        assert!(analysis
            .user_preferences
            .values()
            .any(|v| v.contains("short answers")));
    }

    #[tokio::test]
    async fn test_update_soul_from_analysis() {
        let temp_dir = std::env::temp_dir().join(format!("syscity_test_{}", uuid::Uuid::new_v4()));
        tokio::fs::create_dir_all(&temp_dir).await.unwrap();
        let memory = PersonalityMemory::with_dir(temp_dir).await.unwrap();

        let analysis = SoulAnalysis {
            detected_language: Some("en-US".to_string()),
            detected_code_style: Some("rust".to_string()),
            detected_voice: Some("concise".to_string()),
            common_topics: vec!["rust".to_string(), "systems".to_string()],
            user_preferences: HashMap::from([(
                "preference_1".to_string(),
                "short answers".to_string(),
            )]),
        };

        let changed = memory.update_soul_from_analysis(&analysis).await.unwrap();
        assert!(changed);

        let soul = memory.read_soul().await.unwrap();
        assert_eq!(soul.config.preferences.language, Some("en-US".to_string()));
        assert_eq!(soul.config.preferences.code_style, Some("rust".to_string()));
        assert_eq!(soul.config.voice, Some("concise".to_string()));
        assert!(soul.config.persona.is_some());
        assert!(soul.config.preferences.extra.contains_key("preference_1"));

        // A second merge with the same analysis should not change anything.
        let changed = memory.update_soul_from_analysis(&analysis).await.unwrap();
        assert!(!changed);
    }
}
