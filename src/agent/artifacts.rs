//! Artifacts System
//!
//! Tracks code snippets, documents, images, and links bound to session
//! lifecycle. ts`.
//!
//! Features:
//! - Artifact creation, retrieval, listing
//! - Session-bound lifecycle (auto-cleanup on session end)
//! - Multiple artifact types
//! - File-backed storage for large artifacts

use std::collections::HashMap;
use std::io::{BufRead, BufReader, BufWriter, Write};
use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use tracing::{debug, info, warn};

/// Type of artifact.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ArtifactType {
    /// Code snippet (any language).
    Code,
    /// Document / text content.
    Document,
    /// Image (base64 or URL).
    Image,
    /// External link.
    Link,
    /// Data (JSON, CSV, etc.).
    Data,
    /// Generic file.
    File,
}

impl std::fmt::Display for ArtifactType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ArtifactType::Code => write!(f, "code"),
            ArtifactType::Document => write!(f, "document"),
            ArtifactType::Image => write!(f, "image"),
            ArtifactType::Link => write!(f, "link"),
            ArtifactType::Data => write!(f, "data"),
            ArtifactType::File => write!(f, "file"),
        }
    }
}

/// An artifact produced during a session.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Artifact {
    /// Unique artifact ID.
    pub id: String,
    /// Session ID this artifact belongs to.
    pub session_id: String,
    /// Human-readable title.
    pub title: String,
    /// Artifact type.
    pub artifact_type: ArtifactType,
    /// Content (for small artifacts).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
    /// File path (for large artifacts stored on disk).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub file_path: Option<String>,
    /// For code artifacts: language identifier.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub language: Option<String>,
    /// For link artifacts: URL.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    /// MIME type (if known).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mime_type: Option<String>,
    /// When the artifact was created.
    pub created_at: DateTime<Utc>,
    /// Size in bytes.
    pub size_bytes: usize,
    /// Optional tags.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tags: Option<Vec<String>>,
    /// Optional metadata.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub metadata: Option<serde_json::Value>,
}

impl Artifact {
    /// Create a new code artifact.
    pub fn code(
        id: impl Into<String>,
        session_id: impl Into<String>,
        title: impl Into<String>,
        language: impl Into<String>,
        content: impl Into<String>,
    ) -> Self {
        let content = content.into();
        let size = content.len();
        Self {
            id: id.into(),
            session_id: session_id.into(),
            title: title.into(),
            artifact_type: ArtifactType::Code,
            content: Some(content),
            file_path: None,
            language: Some(language.into()),
            url: None,
            mime_type: Some("text/plain".to_string()),
            created_at: Utc::now(),
            size_bytes: size,
            tags: None,
            metadata: None,
        }
    }

    /// Create a new document artifact.
    pub fn document(
        id: impl Into<String>,
        session_id: impl Into<String>,
        title: impl Into<String>,
        content: impl Into<String>,
    ) -> Self {
        let content = content.into();
        let size = content.len();
        Self {
            id: id.into(),
            session_id: session_id.into(),
            title: title.into(),
            artifact_type: ArtifactType::Document,
            content: Some(content),
            file_path: None,
            language: None,
            url: None,
            mime_type: Some("text/plain".to_string()),
            created_at: Utc::now(),
            size_bytes: size,
            tags: None,
            metadata: None,
        }
    }

    /// Create a new link artifact.
    pub fn link(
        id: impl Into<String>,
        session_id: impl Into<String>,
        title: impl Into<String>,
        url: impl Into<String>,
    ) -> Self {
        let url_str = url.into();
        Self {
            id: id.into(),
            session_id: session_id.into(),
            title: title.into(),
            artifact_type: ArtifactType::Link,
            content: None,
            file_path: None,
            language: None,
            url: Some(url_str.clone()),
            mime_type: None,
            created_at: Utc::now(),
            size_bytes: url_str.len(),
            tags: None,
            metadata: None,
        }
    }

    /// Create a new data artifact.
    pub fn data(
        id: impl Into<String>,
        session_id: impl Into<String>,
        title: impl Into<String>,
        content: impl Into<String>,
    ) -> Self {
        let content = content.into();
        let size = content.len();
        Self {
            id: id.into(),
            session_id: session_id.into(),
            title: title.into(),
            artifact_type: ArtifactType::Data,
            content: Some(content),
            file_path: None,
            language: None,
            url: None,
            mime_type: Some("application/json".to_string()),
            created_at: Utc::now(),
            size_bytes: size,
            tags: None,
            metadata: None,
        }
    }

    /// Add a tag.
    pub fn with_tag(mut self, tag: impl Into<String>) -> Self {
        self.tags.get_or_insert_with(Vec::new).push(tag.into());
        self
    }

    /// Add metadata.
    pub fn with_metadata(mut self, metadata: serde_json::Value) -> Self {
        self.metadata = Some(metadata);
        self
    }

    /// Set file path for large artifacts.
    pub fn with_file_path(mut self, path: impl Into<String>) -> Self {
        self.file_path = Some(path.into());
        self
    }

    /// Get the content (from memory or file).
    pub async fn get_content(&self) -> Option<String> {
        if let Some(ref content) = self.content {
            return Some(content.clone());
        }
        if let Some(ref path) = self.file_path {
            return match tokio::fs::read_to_string(path).await {
                Ok(content) => Some(content),
                Err(e) => {
                    warn!("Failed to read artifact file {}: {}", path, e);
                    None
                }
            };
        }
        None
    }

    /// Render as markdown.
    pub async fn to_markdown(&self) -> String {
        match self.artifact_type {
            ArtifactType::Code => {
                let lang = self.language.as_deref().unwrap_or("");
                let content = self.get_content().await.unwrap_or_default();
                format!("### {}\n\n```{lang}\n{}\n```\n", self.title, content, lang = lang)
            }
            ArtifactType::Document => {
                let content = self.get_content().await.unwrap_or_default();
                format!("### {}\n\n{}\n", self.title, content)
            }
            ArtifactType::Link => {
                let url = self.url.as_deref().unwrap_or("#");
                format!("- [{}]({})\n", self.title, url)
            }
            ArtifactType::Data => {
                let content = self.get_content().await.unwrap_or_default();
                format!("### {} (data)\n\n```json\n{}\n```\n", self.title, content)
            }
            ArtifactType::Image => {
                let url = self.url.as_deref().unwrap_or("");
                format!("### {}\n\n![{}]({})\n", self.title, self.title, url)
            }
            ArtifactType::File => {
                format!(
                    "- **{}** ({}, {} bytes)\n",
                    self.title,
                    self.mime_type.as_deref().unwrap_or("unknown"),
                    self.size_bytes
                )
            }
        }
    }
}

/// In-memory artifact store with session-bound lifecycle.
pub struct ArtifactStore {
    /// session_id -> Vec<Artifact>
    artifacts: std::sync::Mutex<HashMap<String, Vec<Artifact>>>,
    /// Root directory for file-backed artifacts.
    root_dir: PathBuf,
    /// Max sessions before LRU eviction.
    max_sessions: usize,
    /// Timestamp of last access per session (for LRU eviction).
    last_accessed: std::sync::Mutex<HashMap<String, DateTime<Utc>>>,
}

/// Default max active artifact sessions to prevent unbounded memory growth.
const DEFAULT_MAX_SESSIONS: usize = 1000;

impl ArtifactStore {
    /// Create a new artifact store.
    pub fn new(root_dir: impl Into<PathBuf>) -> Self {
        Self {
            artifacts: std::sync::Mutex::new(HashMap::new()),
            root_dir: root_dir.into(),
            max_sessions: DEFAULT_MAX_SESSIONS,
            last_accessed: std::sync::Mutex::new(HashMap::new()),
        }
    }

    /// Set the max sessions cap.
    pub fn set_max_sessions(&mut self, max: usize) {
        self.max_sessions = max;
    }

    /// Mark a session as recently accessed.
    fn touch_session(&self, session_id: &str) {
        let mut last = self.last_accessed.lock().unwrap_or_else(|e| e.into_inner());
        last.insert(session_id.to_string(), Utc::now());
    }

    /// Evict the LRU session if we're at capacity (caller must hold
    /// `artifacts`).
    fn evict_lru(&self, artifacts: &mut HashMap<String, Vec<Artifact>>, new_session_id: &str) {
        if artifacts.len() <= self.max_sessions {
            return;
        }
        let oldest = self
            .last_accessed
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .filter(|(id, _)| *id != new_session_id)
            .min_by_key(|(_, ts)| **ts)
            .map(|(id, _)| id.clone());
        if let Some(oldest_id) = oldest {
            artifacts.remove(&oldest_id);
        }
    }

    /// Initialize directories and load the persistent index (if present).
    pub async fn init(&self) -> std::io::Result<()> {
        tokio::fs::create_dir_all(&self.root_dir).await?;
        let path = self.index_path();
        if path.exists() {
            self.load_index(&path);
        }
        debug!("Artifact store initialized at {:?}", self.root_dir);
        Ok(())
    }

    /// Path to the append-only metadata index (one JSON `Artifact` per line).
    fn index_path(&self) -> PathBuf {
        self.root_dir.join("index.jsonl")
    }

    /// Load artifacts from `index.jsonl`, skipping and warning on corrupt lines.
    fn load_index(&self, path: &Path) {
        let file = match std::fs::File::open(path) {
            Ok(file) => file,
            Err(e) => {
                warn!("Failed to open artifact index {}: {}", path.display(), e);
                return;
            }
        };
        let mut artifacts = self.artifacts.lock().unwrap_or_else(|e| e.into_inner());
        let mut corrupt = 0usize;
        for line in BufReader::new(file).lines() {
            let line = match line {
                Ok(line) => line,
                Err(e) => {
                    corrupt += 1;
                    warn!("Failed to read artifact index line: {}", e);
                    continue;
                }
            };
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            match serde_json::from_str::<Artifact>(line) {
                Ok(artifact) => {
                    let session_id = artifact.session_id.clone();
                    artifacts.entry(session_id).or_default().push(artifact);
                }
                Err(e) => {
                    corrupt += 1;
                    warn!("Skipping corrupt artifact index line: {}", e);
                }
            }
        }
        if corrupt > 0 {
            warn!("Skipped {} corrupt lines from artifact index {}", corrupt, path.display());
        }
    }

    /// Append a single serialized `Artifact` line to the index.
    fn append_index_line(&self, line: &str) {
        let path = self.index_path();
        let mut file = match std::fs::OpenOptions::new()
            .append(true)
            .create(true)
            .open(&path)
        {
            Ok(file) => file,
            Err(e) => {
                warn!("Failed to open artifact index {}: {}", path.display(), e);
                return;
            }
        };
        if let Err(e) = writeln!(file, "{}", line) {
            warn!("Failed to append to artifact index {}: {}", path.display(), e);
        }
    }

    /// Rewrite the index from the current in-memory contents (compact).
    fn rewrite_index(&self, artifacts: &HashMap<String, Vec<Artifact>>) {
        let path = self.index_path();
        let file = match std::fs::File::create(&path) {
            Ok(file) => file,
            Err(e) => {
                warn!("Failed to rewrite artifact index {}: {}", path.display(), e);
                return;
            }
        };
        let mut writer = BufWriter::new(file);
        for artifact in artifacts.values().flatten() {
            let line = match serde_json::to_string(artifact) {
                Ok(line) => line,
                Err(e) => {
                    warn!("Failed to serialize artifact for index rewrite: {}", e);
                    continue;
                }
            };
            if let Err(e) = writeln!(writer, "{}", line) {
                warn!("Failed to write artifact index {}: {}", path.display(), e);
                return;
            }
        }
        if let Err(e) = writer.flush() {
            warn!("Failed to flush artifact index {}: {}", path.display(), e);
        }
    }

    /// Add an artifact to a session.
    pub fn add(&self, artifact: Artifact) {
        // Serialize before moving `artifact` into the in-memory map.
        let index_line = match serde_json::to_string(&artifact) {
            Ok(line) => Some(line),
            Err(e) => {
                warn!("Failed to serialize artifact for index: {}", e);
                None
            }
        };
        let mut artifacts = self.artifacts.lock().unwrap_or_else(|e| e.into_inner());
        let id = artifact.id.clone();
        let session_id = artifact.session_id.clone();
        let is_new = !artifacts.contains_key(&session_id);
        let list = artifacts.entry(session_id.clone()).or_default();
        list.push(artifact);
        self.touch_session(&session_id);
        if is_new {
            self.evict_lru(&mut artifacts, &session_id);
        }
        drop(artifacts);
        debug!("Added artifact '{}' to session {}", id, session_id);
        if let Some(line) = index_line {
            self.append_index_line(&line);
        }
    }

    /// Get all artifacts for a session.
    pub fn get_for_session(&self, session_id: &str) -> Vec<Artifact> {
        let artifacts = self.artifacts.lock().unwrap_or_else(|e| e.into_inner());
        let result = artifacts.get(session_id).cloned().unwrap_or_default();
        self.touch_session(session_id);
        result
    }

    /// Get a specific artifact by ID.
    pub fn get(&self, session_id: &str, artifact_id: &str) -> Option<Artifact> {
        let artifacts = self.artifacts.lock().unwrap_or_else(|e| e.into_inner());
        let result = artifacts
            .get(session_id)
            .and_then(|list| list.iter().find(|a| a.id == artifact_id).cloned());
        self.touch_session(session_id);
        result
    }

    /// List all artifacts across all sessions.
    pub fn list_all(&self) -> Vec<Artifact> {
        let artifacts = self.artifacts.lock().unwrap_or_else(|e| e.into_inner());
        artifacts.values().flatten().cloned().collect()
    }

    /// List artifact IDs for a session.
    pub fn list_session(&self, session_id: &str) -> Vec<String> {
        let artifacts = self.artifacts.lock().unwrap_or_else(|e| e.into_inner());
        let result = artifacts
            .get(session_id)
            .map(|list| list.iter().map(|a| a.id.clone()).collect())
            .unwrap_or_default();
        self.touch_session(session_id);
        result
    }

    /// Search artifacts across all sessions by case-insensitive substring match.
    ///
    /// Matches against `title`, `tags`, `mime_type`, `language`, and
    /// `session_id`. Results are returned in descending `created_at` order.
    pub fn search(&self, query: &str) -> Vec<Artifact> {
        let needle = query.to_lowercase();
        let artifacts = self.artifacts.lock().unwrap_or_else(|e| e.into_inner());
        let mut results: Vec<Artifact> = artifacts
            .values()
            .flatten()
            .filter(|a| {
                a.title.to_lowercase().contains(&needle)
                    || a.session_id.to_lowercase().contains(&needle)
                    || a.language
                        .as_deref()
                        .is_some_and(|s| s.to_lowercase().contains(&needle))
                    || a.mime_type
                        .as_deref()
                        .is_some_and(|s| s.to_lowercase().contains(&needle))
                    || a.tags
                        .as_ref()
                        .is_some_and(|tags| tags.iter().any(|t| t.to_lowercase().contains(&needle)))
            })
            .cloned()
            .collect();
        results.sort_by_key(|a| std::cmp::Reverse(a.created_at));
        results
    }

    /// Remove a specific artifact.
    pub fn remove(&self, session_id: &str, artifact_id: &str) -> Option<Artifact> {
        let mut artifacts = self.artifacts.lock().unwrap_or_else(|e| e.into_inner());
        let mut removed = None;
        if let Some(list) = artifacts.get_mut(session_id) {
            let pos = list.iter().position(|a| a.id == artifact_id);
            if let Some(pos) = pos {
                removed = Some(list.remove(pos));
                self.rewrite_index(&artifacts);
            }
        }
        drop(artifacts);
        if removed.is_none() {
            self.touch_session(session_id);
        }
        removed
    }

    /// Remove all artifacts for a session (cleanup on session end).
    pub fn clear_session(&self, session_id: &str) -> Vec<Artifact> {
        let mut artifacts = self.artifacts.lock().unwrap_or_else(|e| e.into_inner());
        let removed = artifacts.remove(session_id).unwrap_or_default();
        let mut last = self.last_accessed.lock().unwrap_or_else(|e| e.into_inner());
        last.remove(session_id);
        self.rewrite_index(&artifacts);
        info!("Cleared {} artifacts for session {}", removed.len(), session_id);
        removed
    }

    /// Export all artifacts for a session as a single markdown file.
    pub async fn export_session(&self, session_id: &str) -> Result<PathBuf, String> {
        let artifacts = self.get_for_session(session_id);
        if artifacts.is_empty() {
            return Err("No artifacts for session".to_string());
        }

        let mut content = format!("# Session Artifacts: {}\n\n", session_id);
        for artifact in &artifacts {
            content.push_str(&artifact.to_markdown().await);
            content.push('\n');
        }

        let filename = format!("artifacts_{}.md", sanitize(session_id));
        let path = self.root_dir.join(&filename);
        tokio::fs::write(&path, content)
            .await
            .map_err(|e| format!("Failed to write artifact export: {}", e))?;
        info!("Exported {} artifacts to {:?}", artifacts.len(), path);
        Ok(path)
    }

    /// Get store stats.
    pub fn stats(&self) -> ArtifactStoreStats {
        let artifacts = self.artifacts.lock().unwrap_or_else(|e| e.into_inner());
        let total = artifacts.values().map(|v| v.len()).sum();
        let total_size: usize = artifacts.values().flatten().map(|a| a.size_bytes).sum();
        ArtifactStoreStats {
            session_count: artifacts.len(),
            artifact_count: total,
            total_size_bytes: total_size,
        }
    }
}

/// Artifact store statistics.
#[derive(Debug, Clone)]
pub struct ArtifactStoreStats {
    pub session_count: usize,
    pub artifact_count: usize,
    pub total_size_bytes: usize,
}

fn sanitize(input: &str) -> String {
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

    #[tokio::test]
    async fn test_code_artifact() {
        let a = Artifact::code("a1", "s1", "main.rs", "rust", "fn main() {}");
        assert_eq!(a.artifact_type, ArtifactType::Code);
        assert_eq!(a.language, Some("rust".to_string()));
        assert_eq!(a.get_content().await, Some("fn main() {}".to_string()));
    }

    #[test]
    fn test_link_artifact() {
        let a = Artifact::link("a1", "s1", "Docs", "https://example.com");
        assert_eq!(a.artifact_type, ArtifactType::Link);
        assert_eq!(a.url, Some("https://example.com".to_string()));
    }

    #[tokio::test]
    async fn test_store_add_and_get() {
        let tmp = TempDir::new().unwrap();
        let store = ArtifactStore::new(tmp.path());
        store.init().await.unwrap();

        store.add(Artifact::code("a1", "s1", "main.rs", "rust", "fn main() {}"));
        store.add(Artifact::document("a2", "s1", "README", "Hello"));
        store.add(Artifact::link("a3", "s2", "Docs", "https://example.com"));

        let s1 = store.get_for_session("s1");
        assert_eq!(s1.len(), 2);

        let s2 = store.get_for_session("s2");
        assert_eq!(s2.len(), 1);

        let all = store.list_all();
        assert_eq!(all.len(), 3);
    }

    #[tokio::test]
    async fn test_store_clear_session() {
        let tmp = TempDir::new().unwrap();
        let store = ArtifactStore::new(tmp.path());
        store.init().await.unwrap();

        store.add(Artifact::code("a1", "s1", "main.rs", "rust", "fn main() {}"));
        store.clear_session("s1");

        let s1 = store.get_for_session("s1");
        assert!(s1.is_empty());
    }

    #[tokio::test]
    async fn test_markdown_render() {
        let a = Artifact::code("a1", "s1", "main.rs", "rust", "fn main() {}");
        let md = a.to_markdown().await;
        assert!(md.contains("### main.rs"));
        assert!(md.contains("```rust"));
        assert!(md.contains("fn main() {}"));
    }

    #[tokio::test]
    async fn test_export_session() {
        let tmp = TempDir::new().unwrap();
        let store = ArtifactStore::new(tmp.path());
        store.init().await.unwrap();

        store.add(Artifact::code("a1", "s1", "main.rs", "rust", "fn main() {}"));
        store.add(Artifact::document("a2", "s1", "Notes", "Important"));

        let path = store.export_session("s1").await.unwrap();
        assert!(path.exists());
        let content = std::fs::read_to_string(&path).unwrap();
        assert!(content.contains("main.rs"));
        assert!(content.contains("Notes"));
    }

    fn at(secs: i64) -> chrono::DateTime<chrono::Utc> {
        chrono::DateTime::<chrono::Utc>::from_timestamp(secs, 0).unwrap()
    }

    #[tokio::test]
    async fn test_persistence_roundtrip() {
        let tmp = TempDir::new().unwrap();
        let store = ArtifactStore::new(tmp.path());
        store.init().await.unwrap();
        store.add(Artifact::code("a1", "s1", "main.rs", "rust", "fn main() {}"));
        store.add(Artifact::document("a2", "s1", "README", "Hello"));

        // A fresh store over the same directory should reload the index.
        let store2 = ArtifactStore::new(tmp.path());
        store2.init().await.unwrap();
        assert_eq!(store2.get_for_session("s1").len(), 2);
        assert!(store2.get("s1", "a1").is_some());
        assert!(store2.get("s1", "a2").is_some());
    }

    #[tokio::test]
    async fn test_remove_and_clear_rewrite_index() {
        let tmp = TempDir::new().unwrap();
        let store = ArtifactStore::new(tmp.path());
        store.init().await.unwrap();
        store.add(Artifact::code("a1", "s1", "main.rs", "rust", "fn main() {}"));
        store.add(Artifact::document("a2", "s1", "README", "Hello"));
        store.add(Artifact::link("a3", "s2", "Docs", "https://example.com"));

        store.remove("s1", "a1");

        let store2 = ArtifactStore::new(tmp.path());
        store2.init().await.unwrap();
        assert!(store2.get("s1", "a1").is_none());
        assert!(store2.get("s1", "a2").is_some());
        assert_eq!(store2.list_all().len(), 2);

        store2.clear_session("s1");

        let store3 = ArtifactStore::new(tmp.path());
        store3.init().await.unwrap();
        assert_eq!(store3.list_all().len(), 1);
        assert!(store3.get("s1", "a2").is_none());
        assert!(store3.get("s2", "a3").is_some());
    }

    #[tokio::test]
    async fn test_search_filtering() {
        let tmp = TempDir::new().unwrap();
        let store = ArtifactStore::new(tmp.path());
        store.init().await.unwrap();

        let mut a1 =
            Artifact::code("a1", "s1", "Auth Service", "rust", "fn main() {}").with_tag("backend");
        a1.created_at = at(1_000_000);
        store.add(a1);

        let mut a2 = Artifact::document("a2", "s2", "README", "setup").with_tag("docs");
        a2.created_at = at(2_000_000);
        store.add(a2);

        let mut a3 = Artifact::link("a3", "s2", "Docs link", "https://example.com");
        a3.created_at = at(3_000_000);
        store.add(a3);

        let mut a4 = Artifact::data("a4", "s3", "Metrics", "{}");
        a4.created_at = at(4_000_000);
        store.add(a4);

        // Case-insensitive title match.
        assert_eq!(store.search("AUTH").len(), 1);
        assert_eq!(store.search("AUTH")[0].id, "a1");

        // Tag match.
        assert_eq!(store.search("backend")[0].id, "a1");

        // Language match.
        assert_eq!(store.search("RUST")[0].id, "a1");

        // MIME type match.
        assert_eq!(store.search("application/json")[0].id, "a4");

        // Session id match, newest first.
        let by_session = store.search("s2");
        assert_eq!(by_session.len(), 2);
        assert_eq!(by_session[0].id, "a3");
        assert_eq!(by_session[1].id, "a2");

        // Title + tag match, ordered by created_at desc.
        let by_docs = store.search("docs");
        assert_eq!(by_docs.len(), 2);
        assert_eq!(by_docs[0].id, "a3");
        assert_eq!(by_docs[1].id, "a2");
    }

    #[tokio::test]
    async fn test_init_skips_corrupt_lines() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path();
        std::fs::create_dir_all(dir).unwrap();
        let good = Artifact::code("a1", "s1", "main.rs", "rust", "fn main() {}");
        let index = dir.join("index.jsonl");
        std::fs::write(
            &index,
            format!("{}\nnot-json\n{}\n\n", serde_json::to_string(&good).unwrap(), "{\"id\": 42}",),
        )
        .unwrap();

        let store = ArtifactStore::new(dir);
        store.init().await.unwrap();
        let all = store.list_all();
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].id, "a1");
    }
}
