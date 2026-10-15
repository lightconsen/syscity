//! Transcript System
//!
//! Exports conversation records as files in multiple formats.
//! ts`.
//!
//! Features:
//! - Per-session transcript accumulation
//! - Multiple export formats (JSON, Markdown, HTML, Text)
//! - File-based storage with automatic directory creation
//! - Export API integration

use std::collections::HashMap;
use std::path::PathBuf;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use tracing::{debug, info};

/// A single message in a transcript.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TranscriptMessage {
    /// Role: "user", "assistant", "system", "tool"
    pub role: String,
    /// Message content.
    pub content: String,
    /// Timestamp.
    pub timestamp: DateTime<Utc>,
    /// Optional metadata (tool name, etc.).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub metadata: Option<serde_json::Value>,
}

impl TranscriptMessage {
    /// Create a new transcript message.
    pub fn new(role: impl Into<String>, content: impl Into<String>) -> Self {
        Self {
            role: role.into(),
            content: content.into(),
            timestamp: Utc::now(),
            metadata: None,
        }
    }

    /// Add metadata.
    pub fn with_metadata(mut self, metadata: serde_json::Value) -> Self {
        self.metadata = Some(metadata);
        self
    }
}

/// A conversation transcript for a single session.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Transcript {
    /// Session ID.
    pub session_id: String,
    /// Channel name.
    pub channel: String,
    /// User ID (peer).
    pub peer: String,
    /// Conversation scope.
    pub scope: String,
    /// When the transcript started.
    pub started_at: DateTime<Utc>,
    /// When the transcript was last updated.
    pub updated_at: DateTime<Utc>,
    /// Messages in the conversation.
    pub messages: Vec<TranscriptMessage>,
    /// Optional title (auto-generated or user-set).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// Optional tags.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tags: Option<Vec<String>>,
}

impl Transcript {
    /// Create a new transcript.
    pub fn new(
        session_id: impl Into<String>,
        channel: impl Into<String>,
        peer: impl Into<String>,
        scope: impl Into<String>,
    ) -> Self {
        let now = Utc::now();
        Self {
            session_id: session_id.into(),
            channel: channel.into(),
            peer: peer.into(),
            scope: scope.into(),
            started_at: now,
            updated_at: now,
            messages: Vec::new(),
            title: None,
            tags: None,
        }
    }

    /// Append a message.
    pub fn append(&mut self, msg: TranscriptMessage) {
        self.messages.push(msg);
        self.updated_at = Utc::now();
    }

    /// Append a user message.
    pub fn append_user(&mut self, content: impl Into<String>) {
        self.append(TranscriptMessage::new("user", content));
    }

    /// Append an assistant message.
    pub fn append_assistant(&mut self, content: impl Into<String>) {
        self.append(TranscriptMessage::new("assistant", content));
    }

    /// Append a system message.
    pub fn append_system(&mut self, content: impl Into<String>) {
        self.append(TranscriptMessage::new("system", content));
    }

    /// Append a tool result message.
    pub fn append_tool(&mut self, content: impl Into<String>, tool_name: impl Into<String>) {
        let msg = TranscriptMessage::new("tool", content)
            .with_metadata(serde_json::json!({ "tool": tool_name.into() }));
        self.append(msg);
    }

    /// Set the transcript title.
    pub fn set_title(&mut self, title: impl Into<String>) {
        self.title = Some(title.into());
    }

    /// Add a tag.
    pub fn add_tag(&mut self, tag: impl Into<String>) {
        self.tags.get_or_insert_with(Vec::new).push(tag.into());
    }

    /// Get message count.
    pub fn message_count(&self) -> usize {
        self.messages.len()
    }

    /// Get the total content size in bytes.
    pub fn content_size(&self) -> usize {
        self.messages.iter().map(|m| m.content.len()).sum()
    }
}

/// Export format for transcripts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TranscriptFormat {
    /// JSON with full metadata.
    Json,
    /// Markdown with headers and code blocks.
    Markdown,
    /// Plain text.
    Text,
    /// HTML page.
    Html,
}

impl std::fmt::Display for TranscriptFormat {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TranscriptFormat::Json => write!(f, "json"),
            TranscriptFormat::Markdown => write!(f, "markdown"),
            TranscriptFormat::Text => write!(f, "text"),
            TranscriptFormat::Html => write!(f, "html"),
        }
    }
}

impl TranscriptFormat {
    /// File extension for this format.
    pub fn extension(&self) -> &'static str {
        match self {
            TranscriptFormat::Json => "json",
            TranscriptFormat::Markdown => "md",
            TranscriptFormat::Text => "txt",
            TranscriptFormat::Html => "html",
        }
    }

    /// MIME type for this format.
    pub fn mime_type(&self) -> &'static str {
        match self {
            TranscriptFormat::Json => "application/json",
            TranscriptFormat::Markdown => "text/markdown",
            TranscriptFormat::Text => "text/plain",
            TranscriptFormat::Html => "text/html",
        }
    }
}

/// Render a transcript to a specific format.
pub fn render_transcript(transcript: &Transcript, format: TranscriptFormat) -> String {
    match format {
        TranscriptFormat::Json => render_json(transcript),
        TranscriptFormat::Markdown => render_markdown(transcript),
        TranscriptFormat::Text => render_text(transcript),
        TranscriptFormat::Html => render_html(transcript),
    }
}

fn render_json(transcript: &Transcript) -> String {
    serde_json::to_string_pretty(transcript)
        .unwrap_or_else(|e| format!(r#"{{"error": "Failed to serialize transcript: {}"}}"#, e))
}

fn render_markdown(transcript: &Transcript) -> String {
    let mut output = String::new();
    output.push_str(&format!(
        "# Transcript: {}\n\n",
        transcript
            .title
            .as_deref()
            .unwrap_or(&transcript.session_id)
    ));
    output.push_str(&format!("- **Session**: `{}`\n", transcript.session_id));
    output.push_str(&format!("- **Channel**: {}\n", transcript.channel));
    output.push_str(&format!("- **Peer**: {}\n", transcript.peer));
    output.push_str(&format!(
        "- **Started**: {}\n",
        transcript.started_at.format("%Y-%m-%d %H:%M:%S UTC")
    ));
    output.push_str(&format!("- **Messages**: {}\n\n", transcript.message_count()));
    output.push_str("---\n\n");

    for msg in &transcript.messages {
        let role_label = match msg.role.as_str() {
            "user" => "**User**",
            "assistant" => "**Assistant**",
            "system" => "**System**",
            "tool" => {
                if let Some(ref meta) = msg.metadata {
                    if let Some(tool) = meta.get("tool").and_then(|v| v.as_str()) {
                        &format!("**Tool** ({})", tool)
                    } else {
                        "**Tool**"
                    }
                } else {
                    "**Tool**"
                }
            }
            _ => &format!("**{}**", msg.role),
        };
        output.push_str(&format!("{} @ {}\n\n", role_label, msg.timestamp.format("%H:%M:%S")));
        output.push_str(&msg.content);
        output.push_str("\n\n---\n\n");
    }

    output
}

fn render_text(transcript: &Transcript) -> String {
    let mut output = String::new();
    output.push_str(&format!(
        "Transcript: {}\n",
        transcript
            .title
            .as_deref()
            .unwrap_or(&transcript.session_id)
    ));
    output.push_str(&format!("Session: {}\n", transcript.session_id));
    output.push_str(&format!("Channel: {} | Peer: {}\n", transcript.channel, transcript.peer));
    output.push_str(&format!(
        "Started: {}\n\n",
        transcript.started_at.format("%Y-%m-%d %H:%M:%S UTC")
    ));

    for msg in &transcript.messages {
        let prefix = match msg.role.as_str() {
            "user" => "[User]",
            "assistant" => "[Assistant]",
            "system" => "[System]",
            "tool" => {
                if let Some(ref meta) = msg.metadata {
                    if let Some(tool) = meta.get("tool").and_then(|v| v.as_str()) {
                        &format!("[Tool:{}]", tool)
                    } else {
                        "[Tool]"
                    }
                } else {
                    "[Tool]"
                }
            }
            _ => &format!("[{}]", msg.role),
        };
        output.push_str(&format!("{} {}\n", prefix, msg.timestamp.format("%H:%M:%S")));
        output.push_str(&msg.content);
        output.push_str("\n\n");
    }

    output
}

fn render_html(transcript: &Transcript) -> String {
    let mut messages_html = String::new();
    for msg in &transcript.messages {
        let (role_class, role_label): (&str, String) = match msg.role.as_str() {
            "user" => ("user", "User".to_string()),
            "assistant" => ("assistant", "Assistant".to_string()),
            "system" => ("system", "System".to_string()),
            "tool" => {
                if let Some(ref meta) = msg.metadata {
                    if let Some(tool) = meta.get("tool").and_then(|v| v.as_str()) {
                        ("tool", format!("Tool ({})", tool))
                    } else {
                        ("tool", "Tool".to_string())
                    }
                } else {
                    ("tool", "Tool".to_string())
                }
            }
            _ => ("unknown", msg.role.clone()),
        };

        let content_escaped = html_escape(&msg.content);
        messages_html.push_str(&format!(
            r#"<div class="message {}">
<div class="meta"><span class="role">{}</span> <span class="time">{}</span></div>
<div class="content"><pre>{}</pre></div>
</div>"#,
            role_class,
            role_label,
            msg.timestamp.format("%H:%M:%S"),
            content_escaped
        ));
    }

    format!(
        r#"<!DOCTYPE html>
<html>
<head>
<meta charset="UTF-8">
<title>Transcript: {}</title>
<style>
body {{ font-family: system-ui, sans-serif; max-width: 800px; margin: 0 auto; padding: 20px; background: #f5f5f5; }}
.header {{ background: #fff; padding: 20px; border-radius: 8px; margin-bottom: 20px; }}
.message {{ background: #fff; padding: 16px; border-radius: 8px; margin-bottom: 12px; }}
.meta {{ font-size: 12px; color: #666; margin-bottom: 8px; }}
.role {{ font-weight: bold; }}
.user .role {{ color: #1a73e8; }}
.assistant .role {{ color: #188038; }}
.system .role {{ color: #ea4335; }}
.tool .role {{ color: #f9ab00; }}
.content pre {{ white-space: pre-wrap; margin: 0; font-family: inherit; }}
</style>
</head>
<body>
<div class="header">
<h1>{}</h1>
<p>Session: <code>{}</code></p>
<p>Channel: {} | Peer: {}</p>
<p>Started: {} | Messages: {}</p>
</div>
<div class="messages">
{}
</div>
</body>
</html>"#,
        transcript
            .title
            .as_deref()
            .unwrap_or(&transcript.session_id),
        transcript.title.as_deref().unwrap_or("Transcript"),
        transcript.session_id,
        transcript.channel,
        transcript.peer,
        transcript.started_at.format("%Y-%m-%d %H:%M:%S UTC"),
        transcript.message_count(),
        messages_html
    )
}

fn html_escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

/// File-based transcript store.
pub struct TranscriptStore {
    /// Root directory for transcript storage.
    root_dir: PathBuf,
    /// In-memory buffer of active transcripts (session_id -> Transcript).
    active: std::sync::Mutex<HashMap<String, Transcript>>,
    /// Max active sessions before LRU eviction.
    max_sessions: usize,
}

/// Default max active sessions to prevent unbounded memory growth.
const DEFAULT_MAX_SESSIONS: usize = 1000;

impl TranscriptStore {
    /// Create a new transcript store.
    pub fn new(root_dir: impl Into<PathBuf>) -> Self {
        let root_dir = root_dir.into();
        Self {
            root_dir,
            active: std::sync::Mutex::new(HashMap::new()),
            max_sessions: DEFAULT_MAX_SESSIONS,
        }
    }

    /// Set the max active sessions cap.
    pub fn set_max_sessions(&mut self, max: usize) {
        self.max_sessions = max;
    }

    /// Initialize the store (create directories).
    pub async fn init(&self) -> std::io::Result<()> {
        tokio::fs::create_dir_all(&self.root_dir).await?;
        debug!("Transcript store initialized at {:?}", self.root_dir);
        Ok(())
    }

    /// Internal: get or create a transcript for a session.
    ///
    /// Returns a `MutexGuard` — callers must not re-enter any `&self` method
    /// on this store while the guard is held.
    #[doc(hidden)]
    fn get_or_create_inner(
        &self,
        session_id: impl Into<String>,
        channel: impl Into<String>,
        peer: impl Into<String>,
        scope: impl Into<String>,
    ) -> std::sync::MutexGuard<'_, HashMap<String, Transcript>> {
        let session_id = session_id.into();
        let mut active = self.active.lock().unwrap_or_else(|e| e.into_inner());
        let is_new = !active.contains_key(&session_id);
        active
            .entry(session_id.clone())
            .or_insert_with(|| Transcript::new(session_id, channel, peer, scope));

        // Evict the LRU session if we're over capacity.
        if is_new && active.len() > self.max_sessions {
            let oldest = active
                .iter()
                .min_by_key(|(_, t)| t.updated_at)
                .map(|(id, _)| id.clone());
            if let Some(oldest_id) = oldest {
                active.remove(&oldest_id);
            }
        }
        active
    }

    /// Get or create a transcript for a session.
    ///
    /// Returns the transcript cloned from the store — the caller receives an
    /// owned copy rather than a live lock reference.
    pub fn get_or_create(
        &self,
        session_id: impl Into<String>,
        channel: impl Into<String>,
        peer: impl Into<String>,
        scope: impl Into<String>,
    ) -> Transcript {
        let session_id = session_id.into();
        // Lock once, then check — prevents TOCTOU where another thread
        // inserts between the check and the insert.
        let mut active = self.active.lock().unwrap_or_else(|e| e.into_inner());
        active
            .entry(session_id.clone())
            .or_insert_with(|| Transcript::new(session_id, channel, peer, scope))
            .clone()
    }

    /// Append a message to a session's transcript.
    pub fn append(
        &self,
        session_id: &str,
        channel: impl Into<String>,
        peer: impl Into<String>,
        scope: impl Into<String>,
        msg: TranscriptMessage,
    ) {
        let mut active = self.get_or_create_inner(session_id, channel, peer, scope);
        if let Some(transcript) = active.get_mut(session_id) {
            transcript.append(msg);
        }
    }

    /// Get a transcript by session ID.
    pub fn get(&self, session_id: &str) -> Option<Transcript> {
        let active = self.active.lock().unwrap_or_else(|e| e.into_inner());
        active.get(session_id).cloned()
    }

    /// Export a transcript to a file.
    ///
    /// Only what this process still holds in memory; a session nobody has
    /// touched since the last start is not here. Callers that want any session
    /// build the transcript from the store and use [`Self::write`].
    pub async fn export(
        &self,
        session_id: &str,
        format: TranscriptFormat,
    ) -> Result<PathBuf, String> {
        let transcript = self.get(session_id).ok_or("Transcript not found")?;
        self.write(&transcript, format).await
    }

    /// Render a transcript and write it into the store's directory.
    ///
    /// The single writer: every export goes through here, so the filename
    /// convention and the formats on offer are one thing rather than one per
    /// caller.
    pub async fn write(
        &self,
        transcript: &Transcript,
        format: TranscriptFormat,
    ) -> Result<PathBuf, String> {
        let content = render_transcript(transcript, format);

        let filename = format!(
            "{}_{}.{}",
            sanitize_filename(&transcript.session_id),
            Utc::now().format("%Y%m%d_%H%M%S"),
            format.extension()
        );
        let path = self.root_dir.join(&filename);

        // The store's directory is created by whoever set it up in the running
        // gateway, but writing is the one thing that cannot fail just because
        // nobody has written before.
        tokio::fs::create_dir_all(&self.root_dir)
            .await
            .map_err(|e| format!("Failed to create {}: {}", self.root_dir.display(), e))?;
        tokio::fs::write(&path, content)
            .await
            .map_err(|e| format!("Failed to write transcript: {}", e))?;
        info!("Exported transcript to {:?}", path);
        Ok(path)
    }

    /// Export all active transcripts.
    pub async fn export_all(&self, format: TranscriptFormat) -> Vec<Result<PathBuf, String>> {
        let session_ids = {
            let active = self.active.lock().unwrap_or_else(|e| e.into_inner());
            active.keys().cloned().collect::<Vec<String>>()
        };

        let mut results = Vec::new();
        for id in session_ids {
            results.push(self.export(&id, format).await);
        }
        results
    }

    /// Flush a transcript to disk (persist the active buffer).
    pub async fn flush(&self, session_id: &str) -> Result<PathBuf, String> {
        self.export(session_id, TranscriptFormat::Json).await
    }

    /// Flush all active transcripts.
    pub async fn flush_all(&self) -> Vec<Result<PathBuf, String>> {
        self.export_all(TranscriptFormat::Json).await
    }

    /// Load a transcript from a JSON file.
    pub async fn load(&self, filename: &str) -> Result<Transcript, String> {
        let path = self.root_dir.join(filename);
        let content = tokio::fs::read_to_string(&path)
            .await
            .map_err(|e| format!("Failed to read transcript file: {}", e))?;
        serde_json::from_str(&content)
            .map_err(|e| format!("Failed to parse transcript JSON: {}", e))
    }

    /// The directory exports are written to and imports are read from.
    ///
    /// Exposed so a caller can confine a path to it: a transcript file is named
    /// by the user, and a name that resolved anywhere on the filesystem would
    /// turn "import a saved conversation" into "read any file the gateway can".
    pub fn root_dir(&self) -> &std::path::Path {
        &self.root_dir
    }

    /// List all transcript files in the store.
    pub async fn list_files(&self) -> Vec<PathBuf> {
        let mut files = Vec::new();
        if let Ok(mut entries) = tokio::fs::read_dir(&self.root_dir).await {
            while let Ok(Some(entry)) = entries.next_entry().await {
                let path = entry.path();
                if path.is_file() {
                    files.push(path);
                }
            }
        }
        files.sort();
        files
    }

    /// Get store stats.
    pub async fn stats(&self) -> TranscriptStoreStats {
        let (active_sessions, total_messages) = {
            let active = self.active.lock().unwrap_or_else(|e| e.into_inner());
            (active.len(), active.values().map(|t| t.message_count()).sum())
        };
        let file_count = self.list_files().await.len();
        TranscriptStoreStats {
            active_sessions,
            total_messages,
            total_file_count: file_count,
        }
    }

    /// Remove a transcript from active memory.
    pub fn remove(&self, session_id: &str) -> Option<Transcript> {
        let mut active = self.active.lock().unwrap_or_else(|e| e.into_inner());
        active.remove(session_id)
    }

    /// Clear all active transcripts (files are preserved).
    pub fn clear_active(&self) {
        let mut active = self.active.lock().unwrap_or_else(|e| e.into_inner());
        active.clear();
    }
}

/// Stats for the transcript store.
#[derive(Debug, Clone)]
pub struct TranscriptStoreStats {
    pub active_sessions: usize,
    pub total_messages: usize,
    pub total_file_count: usize,
}

/// Sanitize a string for use in a filename.
fn sanitize_filename(input: &str) -> String {
    input
        .chars()
        .map(|c| match c {
            'a'..='z' | 'A'..='Z' | '0'..='9' | '-' | '_' => c,
            _ => '_',
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use tempfile::TempDir;

    use super::*;

    #[test]
    fn test_transcript_append() {
        let mut t = Transcript::new("s1", "telegram", "user1", "dm");
        t.append_user("Hello");
        t.append_assistant("Hi there!");
        assert_eq!(t.message_count(), 2);
        assert_eq!(t.messages[0].role, "user");
        assert_eq!(t.messages[1].role, "assistant");
    }

    #[test]
    fn test_render_markdown() {
        let mut t = Transcript::new("s1", "telegram", "user1", "dm");
        t.set_title("Test Chat");
        t.append_user("Hello");
        t.append_assistant("Hi!");

        let md = render_markdown(&t);
        assert!(md.contains("# Transcript: Test Chat"));
        assert!(md.contains("**User**"));
        assert!(md.contains("**Assistant**"));
        assert!(md.contains("Hello"));
    }

    #[test]
    fn test_render_json() {
        let mut t = Transcript::new("s1", "telegram", "user1", "dm");
        t.append_user("Hello");

        let json = render_json(&t);
        assert!(json.contains("\"session_id\": \"s1\""));
        assert!(json.contains("\"role\": \"user\""));
    }

    #[tokio::test]
    async fn test_store_export() {
        let tmp = TempDir::new().unwrap();
        let store = TranscriptStore::new(tmp.path());
        store.init().await.unwrap();

        store.append("s1", "telegram", "user1", "dm", TranscriptMessage::new("user", "Hello"));
        store.append("s1", "telegram", "user1", "dm", TranscriptMessage::new("assistant", "Hi!"));

        let path = store
            .export("s1", TranscriptFormat::Markdown)
            .await
            .unwrap();
        assert!(path.exists());
        let content = std::fs::read_to_string(&path).unwrap();
        assert!(content.contains("Hello"));
        assert!(content.contains("Hi!"));
    }

    #[tokio::test]
    async fn test_store_load() {
        let tmp = TempDir::new().unwrap();
        let store = TranscriptStore::new(tmp.path());
        store.init().await.unwrap();

        store.append("s1", "telegram", "user1", "dm", TranscriptMessage::new("user", "Hello"));
        store.flush("s1").await.unwrap();

        let files = store.list_files().await;
        assert_eq!(files.len(), 1);

        let loaded = store
            .load(files[0].file_name().unwrap().to_str().unwrap())
            .await;
        assert!(loaded.is_ok());
        let transcript = loaded.unwrap();
        assert_eq!(transcript.session_id, "s1");
    }

    #[test]
    fn test_html_escape() {
        assert_eq!(html_escape("<script>"), "&lt;script&gt;");
        assert_eq!(html_escape("a & b"), "a &amp; b");
    }
}
