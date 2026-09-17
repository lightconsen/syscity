//! Multi-Agent Session Orchestration
//!
//! This provides:
//! - Multi-agent sessions with multiple agents collaborating
//! - Session thread binding (isolated, parent, shared, new)
//! - Agent lifecycle management within a session
//! - Context sharing between agents
//! - Intent-based agent routing

use std::collections::HashMap;
use std::sync::Arc;

use chrono::{DateTime, Utc};
use tokio::sync::{mpsc, Mutex, RwLock};
use tracing::{debug, info, warn};
use uuid::Uuid;

pub use crate::acp::ThreadBinding;
use crate::agent::personality::AgentPersonality;
use crate::channels::IncomingMessage;

/// Compute the thread ID for a given binding mode within a session.
///
/// `active_threads` is an optional set of thread IDs already in use within the
/// session. When `ThreadBinding::Auto` is chosen and `active_threads` is
/// provided, the parent thread is reused only if it already exists in the set;
/// otherwise a fresh thread is created.
pub fn get_thread_id(
    binding: &ThreadBinding,
    parent_thread: &str,
    active_threads: Option<&std::collections::HashSet<String>>,
) -> String {
    match binding {
        ThreadBinding::New => format!("thread-{}", Uuid::new_v4()),
        ThreadBinding::Parent => parent_thread.to_string(),
        ThreadBinding::Thread(id) => id.clone(),
        ThreadBinding::Auto => {
            if let Some(threads) = active_threads {
                if threads.contains(parent_thread) {
                    parent_thread.to_string()
                } else {
                    format!("thread-{}", Uuid::new_v4())
                }
            } else {
                format!("shared-{}", parent_thread)
            }
        }
    }
}

/// Backward-compatible wrapper for [`get_thread_id`] without active-thread
/// tracking.
pub fn get_thread_id_legacy(binding: &ThreadBinding, parent_thread: &str) -> String {
    get_thread_id(binding, parent_thread, None)
}

/// Agent instance within a session
#[derive(Debug, Clone)]
pub struct SessionAgent {
    /// Agent ID
    pub id: String,
    /// Agent personality
    pub personality: AgentPersonality,
    /// Thread binding mode
    pub binding: ThreadBinding,
    /// Thread ID this agent is bound to
    pub thread_id: String,
    /// Whether agent is currently active
    pub is_active: bool,
    /// Agent status
    pub status: AgentInstanceStatus,
    /// Spawn time
    pub spawned_at: DateTime<Utc>,
    /// Last activity time
    pub last_activity: DateTime<Utc>,
}

/// Status of an agent instance
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentInstanceStatus {
    /// Agent is starting up
    Starting,
    /// Agent is ready to process messages
    Ready,
    /// Agent is busy processing
    Busy,
    /// Agent is shutting down
    ShuttingDown,
    /// Agent has terminated
    Terminated,
}

impl SessionAgent {
    /// Create a new session agent with optional active-thread tracking for Auto
    /// binding.
    pub fn new_with_threads(
        id: String,
        personality: AgentPersonality,
        binding: ThreadBinding,
        parent_thread: &str,
        active_threads: Option<&std::collections::HashSet<String>>,
    ) -> Self {
        let thread_id = get_thread_id(&binding, parent_thread, active_threads);

        Self {
            id,
            personality,
            binding,
            thread_id,
            is_active: true,
            status: AgentInstanceStatus::Starting,
            spawned_at: Utc::now(),
            last_activity: Utc::now(),
        }
    }

    /// Create a new session agent (backward-compatible wrapper).
    pub fn new(
        id: String,
        personality: AgentPersonality,
        binding: ThreadBinding,
        parent_thread: &str,
    ) -> Self {
        Self::new_with_threads(id, personality, binding, parent_thread, None)
    }

    /// Mark agent as ready
    pub fn mark_ready(&mut self) {
        self.status = AgentInstanceStatus::Ready;
        self.last_activity = Utc::now();
    }

    /// Mark agent as busy
    pub fn mark_busy(&mut self) {
        self.status = AgentInstanceStatus::Busy;
        self.last_activity = Utc::now();
    }

    /// Mark agent as terminated
    pub fn mark_terminated(&mut self) {
        self.status = AgentInstanceStatus::Terminated;
        self.is_active = false;
    }

    /// Check if agent shares context with another agent
    pub fn shares_context_with(&self, other: &SessionAgent) -> bool {
        match (&self.binding, &other.binding) {
            // New isolated threads never share
            (ThreadBinding::New, _) | (_, ThreadBinding::New) => false,
            // Same thread ID - share context
            _ => self.thread_id == other.thread_id,
        }
    }
}

/// Multi-agent session for orchestrating multiple agents
#[derive(Debug)]
pub struct MultiAgentSession {
    /// Session ID
    pub id: String,
    /// Primary thread ID for this session
    pub primary_thread_id: String,
    /// Agents in this session
    agents: HashMap<String, SessionAgent>,
    /// Context shared across the session (for Shared binding mode)
    shared_context: Arc<RwLock<HashMap<String, String>>>,
    /// Session creation time
    pub created_at: DateTime<Utc>,
    /// Last activity time
    pub last_activity: DateTime<Utc>,
    /// Message channel for routing
    message_tx: mpsc::Sender<SessionMessage>,
}

/// Message within a session
#[derive(Debug)]
pub enum SessionMessage {
    /// Route message to specific agent
    RouteToAgent {
        agent_id: String,
        message: IncomingMessage,
    },
    /// Broadcast to all agents in session
    Broadcast {
        message: IncomingMessage,
        exclude_agent: Option<String>,
    },
    /// Spawn new agent in session
    SpawnAgent {
        agent_id: String,
        personality: AgentPersonality,
        binding: ThreadBinding,
    },
    /// Terminate agent
    TerminateAgent { agent_id: String },
    /// Get session status
    GetStatus {
        respond_to: oneshot::Sender<SessionStatus>,
    },
}

/// Session status
#[derive(Debug, Clone)]
pub struct SessionStatus {
    pub session_id: String,
    pub agent_count: usize,
    pub active_agents: Vec<String>,
    pub thread_count: usize,
}

impl MultiAgentSession {
    /// Create a new multi-agent session
    pub fn new(id: String) -> (Self, mpsc::Receiver<SessionMessage>) {
        let (message_tx, message_rx) = mpsc::channel(100);
        let primary_thread_id = format!("session-{}", id);

        let session = Self {
            id: id.clone(),
            primary_thread_id,
            agents: HashMap::new(),
            shared_context: Arc::new(RwLock::new(HashMap::new())),
            created_at: Utc::now(),
            last_activity: Utc::now(),
            message_tx,
        };

        (session, message_rx)
    }

    /// Get the message sender for this session
    pub fn sender(&self) -> mpsc::Sender<SessionMessage> {
        self.message_tx.clone()
    }

    /// Spawn an agent in this session
    pub fn spawn_agent(
        &mut self,
        agent_id: String,
        personality: AgentPersonality,
        binding: ThreadBinding,
    ) -> &SessionAgent {
        info!(
            "Spawning agent '{}' in session '{}' with binding {:?}",
            agent_id, self.id, binding
        );

        // Build set of active thread IDs for Auto binding resolution
        let active_threads: std::collections::HashSet<String> =
            self.agents.values().map(|a| a.thread_id.clone()).collect();

        let agent = SessionAgent::new_with_threads(
            agent_id.clone(),
            personality,
            binding,
            &self.primary_thread_id,
            Some(&active_threads),
        );

        self.agents.insert(agent_id.clone(), agent);
        self.last_activity = Utc::now();

        #[allow(clippy::expect_used)] // just inserted above
        self.agents.get(&agent_id).expect("agent just inserted")
    }

    /// Terminate an agent
    pub fn terminate_agent(&mut self, agent_id: &str) {
        if let Some(agent) = self.agents.get_mut(agent_id) {
            agent.mark_terminated();
            info!("Terminated agent '{}' in session '{}'", agent_id, self.id);
        }
    }

    /// Get an agent by ID
    pub fn get_agent(&self, agent_id: &str) -> Option<&SessionAgent> {
        self.agents.get(agent_id)
    }

    /// Get mutable agent by ID
    pub fn get_agent_mut(&mut self, agent_id: &str) -> Option<&mut SessionAgent> {
        self.agents.get_mut(agent_id)
    }

    /// Get all agents
    pub fn get_agents(&self) -> &HashMap<String, SessionAgent> {
        &self.agents
    }

    /// Get agents by thread binding
    pub fn get_agents_by_thread(&self, thread_id: &str) -> Vec<&SessionAgent> {
        self.agents
            .values()
            .filter(|a| a.thread_id == thread_id && a.is_active)
            .collect()
    }

    /// Get active agents
    pub fn get_active_agents(&self) -> Vec<&SessionAgent> {
        self.agents.values().filter(|a| a.is_active).collect()
    }

    /// Get shared context
    pub fn shared_context(&self) -> Arc<RwLock<HashMap<String, String>>> {
        self.shared_context.clone()
    }

    /// Get session status
    pub fn get_status(&self) -> SessionStatus {
        let active_agents: Vec<String> = self
            .agents
            .values()
            .filter(|a| a.is_active)
            .map(|a| a.id.clone())
            .collect();

        let thread_count = self
            .agents
            .values()
            .map(|a| &a.thread_id)
            .collect::<std::collections::HashSet<_>>()
            .len();

        SessionStatus {
            session_id: self.id.clone(),
            agent_count: self.agents.len(),
            active_agents,
            thread_count,
        }
    }

    /// Find best agent for a message based on intent
    pub fn find_agent_for_intent(&self, message: &str) -> Option<&SessionAgent> {
        let message_lower = message.to_lowercase();

        // Simple intent-based routing
        let intent_keywords: Vec<(&str, Vec<&str>)> = vec![
            ("code", vec!["code", "program", "debug", "fix", "error", "bug"]),
            ("review", vec!["review", "check", "audit", "analyze"]),
            ("lead", vec!["design", "architect", "plan", "coordinate"]),
            ("write", vec!["write", "document", "create", "draft"]),
        ];

        for (intent, keywords) in intent_keywords {
            if keywords.iter().any(|kw| message_lower.contains(kw)) {
                // Find an agent that can handle this intent
                return self.agents.values().find(|a| {
                    a.is_active
                        && a.status == AgentInstanceStatus::Ready
                        && a.personality.can_handle(intent)
                });
            }
        }

        // Fallback: return first ready agent
        self.agents
            .values()
            .find(|a| a.is_active && a.status == AgentInstanceStatus::Ready)
    }

    /// Check if session has timed out (no activity)
    pub fn is_timed_out(&self, timeout: std::time::Duration) -> bool {
        let elapsed = Utc::now().signed_duration_since(self.last_activity);
        match chrono::Duration::from_std(timeout) {
            Ok(timeout) => elapsed > timeout,
            Err(_) => false,
        }
    }

    /// Cleanup terminated agents
    pub fn cleanup_terminated(&mut self) {
        self.agents
            .retain(|_, a| a.is_active || a.status != AgentInstanceStatus::Terminated);
    }
}

/// Session manager for all multi-agent sessions
#[derive(Debug, Default)]
pub struct SessionManager {
    /// Active sessions (Arc-wrapped so the background processing task can
    /// access them)
    sessions: HashMap<String, Arc<Mutex<MultiAgentSession>>>,
    /// Session timeout
    timeout: std::time::Duration,
    /// Optional persistent session store for unified session model
    store: Option<Arc<crate::agent::session_store::SessionStore>>,
}

impl SessionManager {
    /// Create new session manager
    pub fn new() -> Self {
        Self {
            sessions: HashMap::new(),
            timeout: std::time::Duration::from_secs(3600), // 1 hour default
            store: None,
        }
    }

    /// Attach a persistent session store for unified session operations.
    pub fn with_store(&mut self, store: Arc<crate::agent::session_store::SessionStore>) {
        self.store = Some(store);
    }

    /// Create a new session and spawn its background processing task.
    /// Also persists to SessionStore when available (unified session model).
    pub fn create_session(&mut self, session_id: String) -> mpsc::Sender<SessionMessage> {
        let (session, message_rx) = MultiAgentSession::new(session_id.clone());
        let sender = session.sender();
        let session_arc = Arc::new(Mutex::new(session));

        // Spawn session processing task, passing a shared handle to the session
        tokio::spawn(session_processing_task(
            session_id.clone(),
            message_rx,
            Arc::clone(&session_arc),
        ));

        self.sessions.insert(session_id.clone(), session_arc);

        // Auto-persist to SessionStore if available. Only create the row when
        // absent: an unconditional `save_session` with empty metadata would
        // race with `handle_sessions_create` and clobber its richer metadata
        // (agent binding, model pin).
        if let Some(ref store) = self.store {
            let store = store.clone();
            let sid = session_id;
            tokio::spawn(async move {
                // Counted: shutdown waits for this write before closing storage.
                let _owed = crate::agent::writes::pending().guard();
                if let Err(e) = store.ensure_session_row(&sid).await {
                    tracing::warn!("Failed to auto-persist session {}: {}", sid, e);
                }
            });
        }

        sender
    }

    /// Get a shared handle to a session
    pub fn get_session(&self, session_id: &str) -> Option<Arc<Mutex<MultiAgentSession>>> {
        self.sessions.get(session_id).cloned()
    }

    /// Number of active sessions.
    pub fn len(&self) -> usize {
        self.sessions.len()
    }

    /// Whether there are no active sessions.
    pub fn is_empty(&self) -> bool {
        self.sessions.is_empty()
    }

    /// Terminate a session. Also updates SessionStore when available.
    pub async fn terminate_session(&mut self, session_id: &str) {
        if let Some(arc) = self.sessions.get(session_id) {
            let mut session = arc.lock().await;
            for agent_id in session.get_agents().keys().cloned().collect::<Vec<_>>() {
                session.terminate_agent(&agent_id);
            }
        }
        self.sessions.remove(session_id);

        // Update SessionStore if available
        if let Some(ref store) = self.store {
            let store = store.clone();
            let sid = session_id.to_string();
            tokio::spawn(async move {
                // Counted: shutdown waits for this write before closing storage.
                let _owed = crate::agent::writes::pending().guard();
                if let Err(e) = store.set_session_active(&sid, false).await {
                    tracing::warn!("Failed to update session {} status in store: {}", sid, e);
                }
            });
        }

        info!("Terminated session '{}'", session_id);
    }

    /// Cleanup timed out sessions
    pub async fn cleanup_timed_out(&mut self) {
        let mut timed_out = Vec::new();
        for (id, arc) in &self.sessions {
            let session = arc.lock().await;
            if session.is_timed_out(self.timeout) {
                timed_out.push(id.clone());
            }
        }

        for session_id in timed_out {
            info!("Session '{}' timed out, terminating", session_id);
            self.terminate_session(&session_id).await;
        }
    }

    /// List all sessions. Delegates to SessionStore when available (unified
    /// model), otherwise falls back to in-memory sessions.
    pub async fn list_sessions(&self) -> Vec<String> {
        if let Some(ref store) = self.store {
            match store.find_sessions(None, None, None, false).await {
                Ok(sessions) => sessions.into_iter().map(|m| m.session_id).collect(),
                Err(e) => {
                    tracing::warn!("Failed to list sessions from store: {}", e);
                    self.sessions.keys().cloned().collect()
                }
            }
        } else {
            self.sessions.keys().cloned().collect()
        }
    }

    /// Set session timeout
    pub fn set_timeout(&mut self, timeout: std::time::Duration) {
        self.timeout = timeout;
    }
}

/// Session processing task — handles all `SessionMessage` variants with
/// live access to the shared `MultiAgentSession`.
async fn session_processing_task(
    session_id: String,
    mut message_rx: mpsc::Receiver<SessionMessage>,
    session: Arc<Mutex<MultiAgentSession>>,
) {
    info!("Session processing task started for {}", session_id);

    while let Some(msg) = message_rx.recv().await {
        match msg {
            // ── Status query ────────────────────────────────────────────────
            SessionMessage::GetStatus { respond_to } => {
                let status = {
                    let s = session.lock().await;
                    s.get_status()
                };
                if respond_to.send(status).is_err() {
                    warn!("Session {}: GetStatus receiver dropped", session_id);
                }
            }

            // ── Spawn a new agent in the session ────────────────────────────
            SessionMessage::SpawnAgent { agent_id, personality, binding } => {
                let mut s = session.lock().await;
                s.spawn_agent(agent_id.clone(), personality, binding);
                info!("Session {}: spawned agent {}", session_id, agent_id);
            }

            // ── Terminate an agent ──────────────────────────────────────────
            SessionMessage::TerminateAgent { agent_id } => {
                let mut s = session.lock().await;
                s.terminate_agent(&agent_id);
                info!("Session {}: terminated agent {}", session_id, agent_id);
            }

            // ── Route a message to a specific agent ─────────────────────────
            // SessionAgent is a data-only struct with no own channel; we mark
            // it busy and log. Callers that need actual agent execution should
            // use `Agent::process_message_with_progress` directly.
            SessionMessage::RouteToAgent { agent_id, message } => {
                let mut s = session.lock().await;
                if let Some(agent) = s.get_agent_mut(&agent_id) {
                    agent.mark_busy();
                    debug!(
                        "Session {}: routed {} char message to agent {}",
                        session_id,
                        message.content.len(),
                        agent_id
                    );
                } else {
                    warn!("Session {}: RouteToAgent — agent {} not found", session_id, agent_id);
                }
            }

            // ── Broadcast a message to all (non-excluded) agents ────────────
            SessionMessage::Broadcast { message, exclude_agent } => {
                let mut s = session.lock().await;
                let targets: Vec<String> = s
                    .get_active_agents()
                    .iter()
                    .filter(|a| exclude_agent.as_deref() != Some(&a.id))
                    .map(|a| a.id.clone())
                    .collect();

                debug!(
                    "Session {}: broadcast {} char message to {} agents",
                    session_id,
                    message.content.len(),
                    targets.len()
                );

                for agent_id in targets {
                    if let Some(agent) = s.get_agent_mut(&agent_id) {
                        agent.mark_busy();
                    }
                }
            }
        }
    }

    info!("Session processing task ended for {}", session_id);
}

use tokio::sync::oneshot;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_thread_binding_get_thread_id() {
        let parent = "parent-thread";

        let isolated = get_thread_id(&ThreadBinding::New, parent, None);
        assert!(isolated.starts_with("thread-"));
        assert_ne!(isolated, parent);

        let parent_binding = get_thread_id(&ThreadBinding::Parent, parent, None);
        assert_eq!(parent_binding, parent);

        let existing = get_thread_id(&ThreadBinding::Thread("custom".to_string()), parent, None);
        assert_eq!(existing, "custom");

        let shared = get_thread_id(&ThreadBinding::Auto, parent, None);
        assert_eq!(shared, format!("shared-{}", parent));

        // Auto with active_threads containing parent -> reuses parent
        let mut active = std::collections::HashSet::new();
        active.insert(parent.to_string());
        let auto_reuse = get_thread_id(&ThreadBinding::Auto, parent, Some(&active));
        assert_eq!(auto_reuse, parent);

        // Auto with active_threads not containing parent -> creates new
        let auto_new =
            get_thread_id(&ThreadBinding::Auto, parent, Some(&std::collections::HashSet::new()));
        assert!(auto_new.starts_with("thread-"));
    }

    #[test]
    fn test_session_agent_shares_context() {
        let parent = "parent-thread";

        let agent1 = SessionAgent::new(
            "agent1".to_string(),
            AgentPersonality::default(),
            ThreadBinding::Auto,
            parent,
        );

        let agent2 = SessionAgent::new(
            "agent2".to_string(),
            AgentPersonality::default(),
            ThreadBinding::Auto,
            parent,
        );

        let agent3 = SessionAgent::new(
            "agent3".to_string(),
            AgentPersonality::default(),
            ThreadBinding::New,
            parent,
        );

        assert!(agent1.shares_context_with(&agent2));
        assert!(!agent1.shares_context_with(&agent3));
    }

    #[test]
    fn test_thread_binding_default() {
        let binding: ThreadBinding = Default::default();
        assert_eq!(binding, ThreadBinding::Auto);
    }

    #[test]
    fn test_agent_instance_status_equality() {
        assert_eq!(AgentInstanceStatus::Ready, AgentInstanceStatus::Ready);
        assert_ne!(AgentInstanceStatus::Ready, AgentInstanceStatus::Busy);
        assert_ne!(AgentInstanceStatus::Starting, AgentInstanceStatus::Terminated);
    }

    #[test]
    fn test_session_agent_new() {
        let agent = SessionAgent::new(
            "a1".to_string(),
            AgentPersonality::default(),
            ThreadBinding::Parent,
            "parent-thread",
        );
        assert_eq!(agent.id, "a1");
        assert_eq!(agent.thread_id, "parent-thread");
        assert!(agent.is_active);
        assert_eq!(agent.status, AgentInstanceStatus::Starting);
    }

    #[test]
    fn test_session_agent_mark_ready() {
        let mut agent = SessionAgent::new(
            "a1".to_string(),
            AgentPersonality::default(),
            ThreadBinding::Parent,
            "t",
        );
        agent.mark_ready();
        assert_eq!(agent.status, AgentInstanceStatus::Ready);
    }

    #[test]
    fn test_session_agent_mark_busy() {
        let mut agent = SessionAgent::new(
            "a1".to_string(),
            AgentPersonality::default(),
            ThreadBinding::Parent,
            "t",
        );
        agent.mark_busy();
        assert_eq!(agent.status, AgentInstanceStatus::Busy);
    }

    #[test]
    fn test_session_agent_mark_terminated() {
        let mut agent = SessionAgent::new(
            "a1".to_string(),
            AgentPersonality::default(),
            ThreadBinding::Parent,
            "t",
        );
        agent.mark_terminated();
        assert_eq!(agent.status, AgentInstanceStatus::Terminated);
        assert!(!agent.is_active);
    }

    #[test]
    fn test_session_agent_shares_context_isolated_never() {
        let parent = "parent-thread";
        let isolated1 = SessionAgent::new(
            "a1".to_string(),
            AgentPersonality::default(),
            ThreadBinding::New,
            parent,
        );
        let isolated2 = SessionAgent::new(
            "a2".to_string(),
            AgentPersonality::default(),
            ThreadBinding::New,
            parent,
        );
        let shared = SessionAgent::new(
            "a3".to_string(),
            AgentPersonality::default(),
            ThreadBinding::Auto,
            parent,
        );

        assert!(!isolated1.shares_context_with(&isolated2));
        assert!(!isolated1.shares_context_with(&shared));
        assert!(!shared.shares_context_with(&isolated1));
    }

    #[test]
    fn test_multi_agent_session_new() {
        let (session, _rx) = MultiAgentSession::new("sess1".to_string());
        assert_eq!(session.id, "sess1");
        assert_eq!(session.primary_thread_id, "session-sess1");
        assert!(session.get_agents().is_empty());
    }

    #[test]
    fn test_multi_agent_session_sender() {
        let (session, _rx) = MultiAgentSession::new("sess1".to_string());
        let _sender = session.sender();
    }

    #[test]
    fn test_multi_agent_session_spawn_agent() {
        let (mut session, _rx) = MultiAgentSession::new("sess1".to_string());
        let agent =
            session.spawn_agent("a1".to_string(), AgentPersonality::default(), ThreadBinding::Auto);
        assert_eq!(agent.id, "a1");
        assert_eq!(session.get_agents().len(), 1);
    }

    #[test]
    fn test_multi_agent_session_terminate_agent() {
        let (mut session, _rx) = MultiAgentSession::new("sess1".to_string());
        session.spawn_agent("a1".to_string(), AgentPersonality::default(), ThreadBinding::Auto);
        session.terminate_agent("a1");
        let agent = session.get_agent("a1").unwrap();
        assert!(!agent.is_active);
        assert_eq!(agent.status, AgentInstanceStatus::Terminated);
    }

    #[test]
    fn test_multi_agent_session_get_agent() {
        let (mut session, _rx) = MultiAgentSession::new("sess1".to_string());
        session.spawn_agent("a1".to_string(), AgentPersonality::default(), ThreadBinding::Auto);
        assert!(session.get_agent("a1").is_some());
        assert!(session.get_agent("nonexistent").is_none());
    }

    #[test]
    fn test_multi_agent_session_get_agent_mut() {
        let (mut session, _rx) = MultiAgentSession::new("sess1".to_string());
        session.spawn_agent("a1".to_string(), AgentPersonality::default(), ThreadBinding::Auto);
        if let Some(agent) = session.get_agent_mut("a1") {
            agent.mark_ready();
        }
        assert_eq!(session.get_agent("a1").unwrap().status, AgentInstanceStatus::Ready);
    }

    #[test]
    fn test_multi_agent_session_get_agents_by_thread() {
        let (mut session, _rx) = MultiAgentSession::new("sess1".to_string());
        session.spawn_agent("a1".to_string(), AgentPersonality::default(), ThreadBinding::Parent);
        session.spawn_agent("a2".to_string(), AgentPersonality::default(), ThreadBinding::New);
        let agents = session.get_agents_by_thread(&session.primary_thread_id);
        assert_eq!(agents.len(), 1);
        assert_eq!(agents[0].id, "a1");
    }

    #[test]
    fn test_multi_agent_session_get_active_agents() {
        let (mut session, _rx) = MultiAgentSession::new("sess1".to_string());
        session.spawn_agent("a1".to_string(), AgentPersonality::default(), ThreadBinding::Auto);
        session.spawn_agent("a2".to_string(), AgentPersonality::default(), ThreadBinding::Auto);
        session.terminate_agent("a2");
        let active = session.get_active_agents();
        assert_eq!(active.len(), 1);
        assert_eq!(active[0].id, "a1");
    }

    #[test]
    fn test_multi_agent_session_get_status() {
        let (mut session, _rx) = MultiAgentSession::new("sess1".to_string());
        session.spawn_agent("a1".to_string(), AgentPersonality::default(), ThreadBinding::Parent);
        session.spawn_agent("a2".to_string(), AgentPersonality::default(), ThreadBinding::New);
        let status = session.get_status();
        assert_eq!(status.session_id, "sess1");
        assert_eq!(status.agent_count, 2);
        assert_eq!(status.active_agents.len(), 2);
        assert_eq!(status.thread_count, 2);
    }

    #[tokio::test]
    async fn test_multi_agent_session_is_timed_out() {
        let (session, _rx) = MultiAgentSession::new("sess1".to_string());
        assert!(!session.is_timed_out(std::time::Duration::from_secs(3600)));
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        assert!(session.is_timed_out(std::time::Duration::from_secs(0)));
    }

    #[test]
    fn test_multi_agent_session_cleanup_terminated() {
        let (mut session, _rx) = MultiAgentSession::new("sess1".to_string());
        session.spawn_agent("a1".to_string(), AgentPersonality::default(), ThreadBinding::Auto);
        session.spawn_agent("a2".to_string(), AgentPersonality::default(), ThreadBinding::Auto);
        session.terminate_agent("a2");
        assert_eq!(session.get_agents().len(), 2);
        session.cleanup_terminated();
        assert_eq!(session.get_agents().len(), 1);
        assert!(session.get_agent("a1").is_some());
    }

    #[test]
    fn test_multi_agent_session_find_agent_for_intent() {
        let (mut session, _rx) = MultiAgentSession::new("sess1".to_string());

        let mut code_personality = AgentPersonality::default();
        code_personality.soul = "I am a code and debug expert".to_string();

        session.spawn_agent("coder".to_string(), code_personality, ThreadBinding::Auto);
        {
            let agent = session.get_agent_mut("coder").unwrap();
            agent.mark_ready();
        }

        // Should find the coder for code-related intent
        let found = session.find_agent_for_intent("please debug this error");
        assert!(found.is_some());
        assert_eq!(found.unwrap().id, "coder");

        // Should return None if no ready agents
        {
            let agent = session.get_agent_mut("coder").unwrap();
            agent.mark_busy();
        }
        let found = session.find_agent_for_intent("please debug this");
        assert!(found.is_none());
    }

    #[test]
    fn test_multi_agent_session_find_agent_fallback() {
        let (mut session, _rx) = MultiAgentSession::new("sess1".to_string());
        session.spawn_agent(
            "general".to_string(),
            AgentPersonality::default(),
            ThreadBinding::Auto,
        );
        let agent = session.get_agent_mut("general").unwrap();
        agent.mark_ready();

        // No intent keywords match, falls back to first ready agent
        let found = session.find_agent_for_intent("hello how are you");
        assert!(found.is_some());
        assert_eq!(found.unwrap().id, "general");
    }

    #[tokio::test]
    async fn test_multi_agent_session_shared_context() {
        let (session, _rx) = MultiAgentSession::new("sess1".to_string());
        let ctx = session.shared_context();
        {
            let mut map = ctx.write().await;
            map.insert("key".to_string(), "value".to_string());
        }
        {
            let map = ctx.read().await;
            assert_eq!(map.get("key"), Some(&"value".to_string()));
        }
    }

    #[test]
    fn test_session_status() {
        let status = SessionStatus {
            session_id: "s1".to_string(),
            agent_count: 2,
            active_agents: vec!["a1".to_string(), "a2".to_string()],
            thread_count: 1,
        };
        assert_eq!(status.session_id, "s1");
        assert_eq!(status.agent_count, 2);
    }

    #[test]
    fn test_session_message_debug() {
        let (tx, _rx) = tokio::sync::oneshot::channel();
        let msg = SessionMessage::GetStatus { respond_to: tx };
        let debug = format!("{:?}", msg);
        assert!(debug.contains("GetStatus"));
    }

    #[tokio::test]
    async fn test_session_manager_new() {
        let manager = SessionManager::new();
        assert!(manager.list_sessions().await.is_empty());
    }

    #[tokio::test]
    async fn test_session_manager_create_and_get() {
        let mut manager = SessionManager::new();
        let _sender = manager.create_session("sess1".to_string());
        assert_eq!(manager.list_sessions().await.len(), 1);

        let session = manager.get_session("sess1");
        assert!(session.is_some());
    }

    #[tokio::test]
    async fn test_session_manager_list_sessions() {
        let mut manager = SessionManager::new();
        manager.create_session("sess1".to_string());
        manager.create_session("sess2".to_string());
        let sessions = manager.list_sessions().await;
        assert_eq!(sessions.len(), 2);
        assert!(sessions.contains(&"sess1".to_string()));
        assert!(sessions.contains(&"sess2".to_string()));
    }

    #[tokio::test]
    async fn test_session_manager_terminate_session() {
        let mut manager = SessionManager::new();
        manager.create_session("sess1".to_string());
        assert_eq!(manager.list_sessions().await.len(), 1);
        manager.terminate_session("sess1").await;
        assert!(manager.list_sessions().await.is_empty());
        assert!(manager.get_session("sess1").is_none());
    }

    #[tokio::test]
    async fn test_session_manager_cleanup_timed_out() {
        let mut manager = SessionManager::new();
        manager.set_timeout(std::time::Duration::from_secs(0));
        manager.create_session("sess1".to_string());

        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        assert_eq!(manager.list_sessions().await.len(), 1);
        manager.cleanup_timed_out().await;
        assert!(manager.list_sessions().await.is_empty());
    }

    #[test]
    fn test_session_manager_set_timeout() {
        let mut manager = SessionManager::new();
        manager.set_timeout(std::time::Duration::from_secs(60));
        // Verify by creating a session and checking it doesn't time out quickly
        // (mostly checking this doesn't panic)
    }
}
