//! SQLite Session Storage
//!
//! Provides persistent session storage using SQLite instead of in-memory
//! HashMaps. This gives us ACID guarantees, automatic crash recovery, and
//! simpler querying.

use std::sync::Arc;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::{Pool, Sqlite};
use tokio::sync::RwLock;

mod acp_sessions;
pub mod metrics;
mod recovery;
pub(crate) mod request_snapshots;
mod schema;
mod sessions;
mod subagent_runs;
mod threads;

pub(crate) use self::recovery::invariant_checks as session_store_invariant_checks;
pub use self::recovery::TOOL_OUTCOME_UNKNOWN;
pub use self::request_snapshots::{compact_tools_json, RequestSnapshot, RequestSnapshotRow};
pub use self::subagent_runs::SubagentRunRecord;
pub use self::threads::StoredMessage;

/// Session metadata for querying
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionMetadata {
    /// Session ID (UUID)
    pub session_id: String,
    /// Agent ID (e.g. "default", "coder").
    pub agent_id: String,
    /// Channel ("discord", "telegram", etc.)
    pub channel: String,
    /// Channel-specific ID (user ID, channel ID)
    pub channel_id: String,
    /// Session creation time
    pub created_at: DateTime<Utc>,
    /// Last activity time
    pub last_activity: DateTime<Utc>,
    /// Whether session is active
    pub is_active: bool,
    /// Whether session is pinned in the UI
    #[serde(default)]
    pub pinned: bool,
    /// Message count
    #[serde(default)]
    pub message_count: usize,
    /// Display name (auto-generated or user-set)
    #[serde(default)]
    pub name: Option<String>,
    /// Bound agent ID for unified session model (agent binding)
    #[serde(default)]
    pub bound_agent_id: Option<String>,
    /// Transcript ID for conversation grouping (transcript tracking)
    #[serde(default)]
    pub transcript_id: Option<String>,
    /// Model ID pinned to this session (per-session model binding)
    #[serde(default)]
    pub model: Option<String>,
    /// The user this conversation belongs to, when the transport established
    /// one.
    ///
    /// Before this existed a session's owner was implicit in `channel_id` and
    /// the `ws:<user>` id convention — a naming habit, not something a query
    /// could rely on. `None` means "not recorded": either a single-identity
    /// deployment (`shared` / `anonymous`, where there is no owner to record)
    /// or a row written before the column existed.
    #[serde(default)]
    pub user_id: Option<String>,
}

impl SessionMetadata {
    /// Create new session metadata
    pub fn new(
        session_id: impl Into<String>,
        agent_id: impl Into<String>,
        channel: impl Into<String>,
        channel_id: impl Into<String>,
    ) -> Self {
        let now = Utc::now();
        Self {
            session_id: session_id.into(),
            agent_id: agent_id.into(),
            channel: channel.into(),
            channel_id: channel_id.into(),
            created_at: now,
            last_activity: now,
            is_active: true,
            pinned: false,
            message_count: 0,
            name: None,
            bound_agent_id: None,
            transcript_id: None,
            model: None,
            user_id: None,
        }
    }

    /// Record who the conversation belongs to.
    ///
    /// [`SHARED_IDENTITIES`](crate::memory::SHARED_IDENTITIES) and an empty id
    /// store `None` rather than the literal: `"shared"` is every caller under
    /// `shared_token`, so writing it would claim a per-person owner for a
    /// deployment that has none.
    pub fn with_user_id(mut self, user_id: Option<&str>) -> Self {
        let trimmed = user_id.map(str::trim).filter(|id| !id.is_empty());
        let is_shared = trimmed.is_some_and(|id| {
            crate::memory::SHARED_IDENTITIES
                .iter()
                .any(|s| s.eq_ignore_ascii_case(id))
        });
        self.user_id = match trimmed {
            Some(id) if !is_shared => Some(id.to_string()),
            _ => None,
        };
        self
    }

    /// Update last activity
    pub fn touch(&mut self) {
        self.last_activity = Utc::now();
    }
}

/// Persisted session data
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PersistedSession {
    /// Session ID
    pub id: String,
    /// Session metadata
    pub metadata: SessionMetadata,
    /// Serialized session state (JSON)
    pub state_json: String,
    /// Message count
    pub message_count: i64,
}

/// Session storage using SQLite
#[derive(Debug, Clone)]
pub struct SessionStore {
    /// SQLite connection pool. `pub(super)` so the persistence submodules
    /// (`schema`, `sessions`, `threads`, `subagent_runs`, `acp_sessions`) can
    /// execute queries.
    pub(super) pool: Pool<Sqlite>,
    /// In-memory cache of active sessions (session_id -> last_accessed)
    pub(super) cache: Arc<RwLock<lru::LruCache<String, DateTime<Utc>>>>,
}

/// Parameters for [`SessionStore::append_message`].
#[derive(Default)]
pub struct AppendMessageParams<'a> {
    pub session_id: &'a str,
    pub role: &'a str,
    pub content: &'a str,
    pub metadata_json: Option<&'a str>,
    pub reasoning_content: Option<&'a str>,
    pub tool_calls_json: Option<&'a str>,
    pub transcript_id: Option<&'a str>,
    pub run_id: Option<&'a str>,
}

/// Parameters for [`SessionStore::save_subagent_run`].
#[derive(Debug)]
pub struct SaveSubagentRunParams<'a> {
    pub run_id: &'a str,
    pub subagent_id: &'a str,
    pub session_id: &'a str,
    pub parent_id: &'a str,
    pub label: Option<&'a str>,
    pub task_prompt: Option<&'a str>,
    pub mode: &'a str,
    pub thread_id: Option<&'a str>,
}

/// Session statistics
#[derive(Debug, Clone)]
pub struct SessionStats {
    pub total_sessions: i64,
    pub active_sessions: i64,
    pub total_messages: i64,
    pub total_subagent_runs: i64,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_session_metadata_new_and_touch() {
        let meta = SessionMetadata::new("sid", "agent", "chan", "cid");
        assert_eq!(meta.session_id, "sid");
        assert_eq!(meta.agent_id, "agent");
        assert_eq!(meta.channel, "chan");
        assert_eq!(meta.channel_id, "cid");
        assert!(meta.is_active);
        assert_eq!(meta.message_count, 0);

        let before = meta.last_activity;
        let mut meta2 = meta.clone();
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        meta2.touch();
        assert!(meta2.last_activity >= before);
    }
}
