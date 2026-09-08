//! Gateway runtime types — agent handles and event bus messages.
//!
//! Extracted from `gateway/mod.rs` to reduce the main control-plane file.
//! Re-exported via `pub use runtime::*;` so existing import paths
//! (`crate::gateway::AgentHandle`, etc.) continue to work.

use std::sync::Arc;

use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;

use crate::agent::{Agent, AgentConfig};

/// A buffered message awaiting batch processing (FollowUp / Collect modes).
#[derive(Debug, Clone)]
pub struct BufferedMessage {
    pub content: String,
    pub user_id: String,
    pub channel: String,
}

/// Handle to a running agent
#[derive(Clone)]
pub struct AgentHandle {
    /// Agent ID
    pub id: String,
    /// Agent configuration
    pub config: AgentConfig,
    /// Base configuration before per-agent overrides were applied. Captured at
    /// spawn so a later `config.set agent_overrides.*` can recompute the
    /// effective config (`base + overrides`) and push it to the running agent.
    pub base_config: AgentConfig,
    /// Fire-and-forget command channel (ProcessMessage, Cancel, UpdateConfig,
    /// Shutdown)
    pub tx: mpsc::Sender<AgentCommand>,
    /// Request/response query channel (introspection + skill invocations)
    pub query_tx: mpsc::Sender<AgentQuery>,
    /// Whether agent is currently processing.
    ///
    /// Uses acquire/release ordering: `store(true, Release)` in the agent loop
    /// happens-before any reader that observes it with `load(Acquire)`. This
    /// guarantees that a reader that sees `busy == true` also sees the message
    /// processing state set up by the loop.
    pub busy: Arc<std::sync::atomic::AtomicBool>,
    /// Reference to the agent for ACP orchestration
    pub agent: Arc<Agent>,
}

/// Commands sent to agents
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum AgentCommand {
    /// Process a message
    ProcessMessage {
        session_id: String,
        message: String,
        user_id: String,
        channel: String,
        /// Optional model override (e.g. from OpenAI-compatible API
        /// header/query).
        #[serde(skip_serializing_if = "Option::is_none")]
        model_override: Option<String>,
    },
    /// Cancel current operation
    Cancel,
    /// Update configuration
    UpdateConfig(AgentConfig),
    /// Shutdown agent
    Shutdown,
}

/// Query messages that require a typed response via oneshot channel.
/// Kept separate from AgentCommand because oneshot::Sender<T> cannot implement
/// the Clone/Serialize/Deserialize derives that AgentCommand carries.
#[allow(clippy::type_complexity)]
pub enum AgentQuery {
    /// Return all thread summaries for this agent's session store.
    GetThreadSummaries {
        response_tx: tokio::sync::oneshot::Sender<Vec<(String, String, usize, String)>>,
    },
    /// Return the turns for a specific conversation/thread.
    GetThreadTurns {
        conv_id: String,
        response_tx: tokio::sync::oneshot::Sender<Option<Vec<(usize, String, String, String)>>>,
    },
    /// Undo the last turn in a conversation.
    UndoLastTurn {
        conv_id: String,
        response_tx: tokio::sync::oneshot::Sender<bool>,
    },
    /// Redo the most recently undone turn in a conversation.
    RedoLastTurn {
        conv_id: String,
        response_tx: tokio::sync::oneshot::Sender<bool>,
    },
    /// Process a message as a skill invocation (request/response pattern).
    RunSkill {
        session_id: String,
        message: String,
        user_id: String,
        /// Trust level of the invoking skill — constrains which tools are
        /// available.
        skill_trust: crate::tools::SkillTrust,
        response_tx:
            tokio::sync::oneshot::Sender<crate::error::Result<crate::channels::OutgoingMessage>>,
    },
}

/// Events broadcast by gateway
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum GatewayEvent {
    /// Message received from channel
    MessageReceived {
        channel: String,
        user_id: String,
        content: String,
        timestamp: chrono::DateTime<chrono::Utc>,
    },
    /// Agent response ready
    AgentResponse {
        session_id: String,
        agent_id: String,
        content: String,
        channel: String,
        /// Channel-specific conversation ID for routing responses
        conversation_id: String,
        /// Token usage (prompt, completion, total) if available
        usage: Option<crate::providers::Usage>,
    },
    /// Agent status changed
    AgentStatus {
        agent_id: String,
        status: AgentStatus,
    },
    /// Channel connected/disconnected
    ChannelStatus { channel: String, connected: bool },
    /// Tool execution started
    ToolCalling {
        session_id: String,
        agent_id: String,
        tool_name: String,
        arguments: String,
    },
    /// Tool execution completed
    ToolResult {
        session_id: String,
        agent_id: String,
        tool_name: String,
        result: String,
        data: Option<serde_json::Value>,
    },
    /// High-risk tool call is waiting for human approval
    ApprovalRequired {
        approval_id: String,
        tool_name: String,
        requested_by: String,
        risk_level: crate::tools::approval::RiskLevel,
        message: String,
    },
    /// Device pairing request initiated
    DevicePairRequested {
        device_id: String,
        code: String,
        display_name: Option<String>,
    },
    /// New session auto-created during chat.send
    SessionCreated {
        session_id: String,
        agent_id: String,
        user_id: String,
    },
    /// Session display name was auto-generated or updated
    SessionRenamed { session_id: String, name: String },
    /// Session pin status changed
    SessionPinned { session_id: String, pinned: bool },
    /// Session model binding changed (pinned model ID set or cleared)
    SessionModelChanged {
        session_id: String,
        model: Option<String>,
    },
    /// ACP subagent spawned
    AcpSpawned {
        session_id: String,
        subagent_id: String,
        parent_id: String,
        mode: String,
        thread_id: String,
    },
    /// ACP subagent completed / terminated / crashed
    AcpCompleted {
        session_id: String,
        subagent_id: String,
        status: String,
    },
    /// ACP subagent runtime state changed (pause/resume/step/cancel)
    AcpStatusChanged {
        session_id: String,
        runtime_state: String,
    },
    /// ACP crashed subagent recovered
    AcpRecovered {
        session_id: String,
        old_subagent_id: String,
        new_subagent_id: String,
        crash_count: u32,
    },
    /// ACP thread active subagent switched
    AcpThreadSwitched {
        thread_id: String,
        active_subagent: Option<String>,
    },
    /// MCP server connected
    McpConnected {
        server_id: String,
        tools: usize,
        prompts: usize,
        resources: usize,
    },
    /// MCP server disconnected or marked unhealthy
    McpDisconnected { server_id: String, reason: String },
    /// MCP server recovered after automatic reconnect
    McpRecovered { server_id: String, attempt: u32 },
    /// MCP subscribed resource changed
    McpResourceChanged { server_id: String, uri: String },
    /// MCP OAuth authorization required
    McpAuthRequired { server_id: String, auth_url: String },
    /// MCP OAuth authorization completed
    McpAuthComplete { server_id: String },
    /// MCP OAuth authorization failed
    McpAuthFailed { server_id: String, reason: String },
    /// MCP OAuth token was silently refreshed
    McpTokenRefreshed { server_id: String },
    /// A connector's lifecycle state changed (installed/enabled/disabled/updated).
    ConnectorChanged {
        id: String,
        state: String,
        summary: Option<serde_json::Value>,
    },
    /// Self-repair action taken (agent or channel restarted)
    RepairAction {
        /// "agent" or "channel"
        kind: String,
        target_id: String,
        description: String,
        restart_count: u32,
    },
    /// LLM generation completed (fires during progress callback, before
    /// AgentResponse)
    Completed {
        session_id: String,
        agent_id: String,
        response: String,
        /// Stable per-turn identifier, surfaced to clients for feedback.vote.
        turn_id: String,
        /// Token/credit usage reported by the provider for this turn
        /// (`None` for guard rejections and cache hits).
        usage: Option<crate::providers::Usage>,
    },
    /// Agent encountered a processing error during message handling
    ProcessingError {
        session_id: String,
        agent_id: String,
        message: String,
        /// Machine-readable error code when known (e.g.
        /// `insufficient_credits` from the cloud relay); `None` otherwise.
        code: Option<String>,
    },
    /// Cron job announcement scheduled for delivery
    CronAnnounce {
        channel: String,
        to: String,
        message: String,
    },
    /// Agent is thinking/generating response (typing indicator)
    Thinking {
        session_id: String,
        agent_id: String,
        content: Option<String>,
    },
    /// Streaming text content delta (for real-time typing effect)
    ContentDelta {
        session_id: String,
        agent_id: String,
        delta: String,
    },
    /// Device status changed (connected, disconnected, error, degraded).
    DeviceStatusChanged {
        device_id: String,
        status: String,
        message: Option<String>,
    },
    /// Goal progress event from a sub-agent runner.
    GoalProgress {
        goal_id: String,
        session_id: String,
        event: crate::goal::GoalEvent,
    },
    /// A question was submitted; the session must answer it.
    AskRequired(crate::tools::ask_user::AskRequiredEvent),
    /// A question was answered or timed out.
    AskResolved(crate::tools::ask_user::AskResolvedEvent),
}

/// Agent status
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum AgentStatus {
    Idle,
    Processing { session_id: String },
    Error(String),
    Shutdown,
}
