use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use serde::{Deserialize, Serialize};

/// Configuration for automatic crash recovery of subagents.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CrashRecoveryConfig {
    /// Whether to automatically restart crashed subagents.
    pub enabled: bool,
    /// Maximum number of restart attempts for a single subagent.
    pub max_retries: u32,
    /// Backoff delays in seconds between restart attempts.
    pub backoff_seconds: Vec<u64>,
}

impl Default for CrashRecoveryConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            max_retries: 3,
            backoff_seconds: vec![1, 2, 5, 10, 30],
        }
    }
}

/// ACP Session ID - unique identifier for an ACP session
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct AcpSessionId(pub String);

impl AcpSessionId {
    pub fn new() -> Self {
        let mut bytes = [0u8; 32];
        crate::utils::fill_os_entropy(&mut bytes);
        Self(URL_SAFE_NO_PAD.encode(bytes))
    }
}

impl Default for AcpSessionId {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Display for AcpSessionId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// Subagent spawn mode
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[derive(Default)]
pub enum SpawnMode {
    /// One-shot execution (run and terminate)
    #[default]
    Run,
    /// Persistent session (long-running)
    Session,
}

/// Thread binding mode for subagents
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[derive(Default)]
pub enum ThreadBinding {
    /// New isolated thread
    New,
    /// Bind to parent's thread
    Parent,
    /// Bind to specific thread ID
    Thread(String),
    /// Automatic based on context
    #[default]
    Auto,
}

/// Alias for the canonical execution/spawn mode.
///
/// `ExecutionMode` existed as a duplicate enum; it is now a transparent alias
/// to `SpawnMode` to keep the API backward-compatible while eliminating the
/// duplication.
pub type ExecutionMode = SpawnMode;

/// Runtime state of a session's execution
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RuntimeState {
    /// Idle, waiting for input
    Idle,
    /// Actively running
    Running,
    /// Paused between iterations
    Paused,
    /// Will execute one iteration then pause
    Stepping,
    /// Cancelled, will stop at next check
    Cancelled,
}

impl std::fmt::Display for RuntimeState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RuntimeState::Idle => write!(f, "idle"),
            RuntimeState::Running => write!(f, "running"),
            RuntimeState::Paused => write!(f, "paused"),
            RuntimeState::Stepping => write!(f, "stepping"),
            RuntimeState::Cancelled => write!(f, "cancelled"),
        }
    }
}

/// Status of an ACP-managed session
#[derive(Debug, Clone)]
pub struct AcpSessionStatus {
    pub session_id: String,
    pub runtime_state: RuntimeState,
    pub mode: ExecutionMode,
    pub current_iteration: usize,
    pub max_iterations: usize,
    pub queue_depth: usize,
}

/// Subagent configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SubagentConfig {
    /// Spawn mode
    pub mode: SpawnMode,
    /// Thread binding
    pub thread_binding: ThreadBinding,
    /// System prompt override
    pub system_prompt: Option<String>,
    /// Timeout in seconds (for Run mode)
    pub timeout_seconds: Option<u64>,
}

impl Default for SubagentConfig {
    fn default() -> Self {
        Self {
            mode: SpawnMode::Run,
            thread_binding: ThreadBinding::Auto,
            system_prompt: None,
            timeout_seconds: Some(300),
        }
    }
}

impl SubagentConfig {
    /// Create a minimal configuration for the given spawn mode and thread
    /// binding, suitable for tests and programmatic callers.
    pub fn new(mode: SpawnMode, thread_binding: ThreadBinding) -> Self {
        Self {
            mode,
            thread_binding,
            system_prompt: None,
            timeout_seconds: Some(300),
        }
    }
}

/// Subagent status
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SubagentStatus {
    /// Ready for work
    Ready,
    /// Shutting down
    ShuttingDown,
    /// Terminated normally
    Terminated,
    /// Terminated due to a panic — detected by the watchdog task
    Crashed,
}

/// Thread context for serialized execution
#[derive(Debug)]
pub struct ThreadContext {
    /// Thread ID
    pub id: String,
    /// Active subagent on this thread (if any)
    pub active_subagent: Option<String>,
    /// Created timestamp
    pub created_at: chrono::DateTime<chrono::Utc>,
}

/// Lightweight snapshot of a thread context that can be cloned and returned
/// to callers without exposing internal oneshot channels.
#[derive(Debug, Clone)]
pub struct ThreadContextSummary {
    /// Thread ID
    pub id: String,
    /// Active subagent on this thread (if any)
    pub active_subagent: Option<String>,
    /// Created timestamp
    pub created_at: chrono::DateTime<chrono::Utc>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_acp_session_id_new() {
        let id1 = AcpSessionId::new();
        let id2 = AcpSessionId::new();
        assert_ne!(id1.0, id2.0);
        assert!(!id1.0.is_empty());
    }

    #[test]
    fn test_acp_session_id_default() {
        let id = AcpSessionId::default();
        assert!(!id.0.is_empty());
    }

    #[test]
    fn test_acp_session_id_display() {
        let id = AcpSessionId("sess-123".to_string());
        assert_eq!(format!("{}", id), "sess-123");
    }

    #[test]
    fn test_spawn_mode_default() {
        assert_eq!(SpawnMode::default(), SpawnMode::Run);
    }

    #[test]
    fn test_spawn_mode_serde() {
        let run = serde_json::to_value(SpawnMode::Run).unwrap();
        assert_eq!(run, "run");
        let session = serde_json::to_value(SpawnMode::Session).unwrap();
        assert_eq!(session, "session");

        let decoded: SpawnMode = serde_json::from_str("\"session\"").unwrap();
        assert_eq!(decoded, SpawnMode::Session);
    }

    #[test]
    fn test_thread_binding_default() {
        assert!(matches!(ThreadBinding::default(), ThreadBinding::Auto));
    }

    #[test]
    fn test_thread_binding_serde() {
        let new = serde_json::to_value(ThreadBinding::New).unwrap();
        assert_eq!(new, "new");
        let parent = serde_json::to_value(ThreadBinding::Parent).unwrap();
        assert_eq!(parent, "parent");
        let auto = serde_json::to_value(ThreadBinding::Auto).unwrap();
        assert_eq!(auto, "auto");

        let decoded: ThreadBinding = serde_json::from_str("\"auto\"").unwrap();
        assert!(matches!(decoded, ThreadBinding::Auto));
    }

    #[test]
    fn test_subagent_config_default() {
        let config = SubagentConfig::default();
        assert_eq!(config.mode, SpawnMode::Run);
        assert!(matches!(config.thread_binding, ThreadBinding::Auto));
        assert!(config.system_prompt.is_none());
        assert_eq!(config.timeout_seconds, Some(300));
    }

    #[test]
    fn test_subagent_status_serde() {
        let status = serde_json::to_value(SubagentStatus::Ready).unwrap();
        assert_eq!(status, "ready");
        let status = serde_json::to_value(SubagentStatus::Crashed).unwrap();
        assert_eq!(status, "crashed");
    }
}
