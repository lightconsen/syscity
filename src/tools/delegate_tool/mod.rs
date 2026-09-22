//! Subagent Delegation Tool
//!
//! This tool allows an agent to spawn child agents for parallel task execution.
//! Implements depth limiting, budget sharing, and tool restrictions for
//! children.
//!
//! Integrates with [`SubagentRegistry`] for lifecycle tracking and metrics, and
//! supports opt-in [`ToolHooks`] for audit/observability.

mod child_task;
mod tool;

#[cfg(test)]
mod tests;

pub(crate) use child_task::{execute_child_task, ChildTaskEnv};
pub use tool::DelegateTool;

use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::json;
use tokio::sync::RwLock;
use tracing::{debug, error, info, warn};

use super::{Tool, ToolContext, ToolExecutionResult};
use crate::agent::budget::IterationBudget;
use crate::agent::subagent_registry::SubagentRegistry;
use crate::delegation::{
    child_completion_message, child_failure_message, notify_parent, DelegationConfig,
    DelegationCoordinator, DelegationEvent, DelegationScope, DelegationTaskStore, DelegationWake,
    NewTask,
};
use crate::tools::hooks::ToolHooks;
use crate::tools::sdk::ToolCapabilities;
use uuid::Uuid;

/// Tools stripped from a child's requested allowlist.
///
/// `delegate` is listed for documentation and the spawn-time warning only —
/// its real enforcement is depth-based inside
/// [`DelegationScope::is_tool_allowed`], so interior nodes keep recursion
/// while leaves lose it. The remaining tools match
/// [`DELEGATION_BLOCKED_TOOLS`](crate::delegation::scope::DELEGATION_BLOCKED_TOOLS).
const BLOCKED_TOOLS: &[&str] = &[
    "delegate",
    "clarify",
    "memory",
    "send_message",
    "execute_code",
    "ask_user",
];

/// Upper bound (seconds) for how long the `wait` action may block.
///
/// Kept well under the 120 s tool-call timeout ceiling
/// (`ToolContext::with_timeout`), so a `wait` that has not finished by this
/// budget returns `Ok("still running")` instead of tripping the outer timeout
/// (which would count as a failure against the circuit breaker).
const MAX_WAIT_SECONDS: u64 = 60;

/// Task specification for child agent
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskSpec {
    /// Task description/prompt
    pub prompt: String,
    /// Expected output format
    pub output_format: Option<String>,
    /// Maximum iterations for child
    pub max_iterations: Option<usize>,
    /// Tools allowed for child (empty = all except blocked)
    pub allowed_tools: Vec<String>,
    /// Context to pass to child
    pub context: HashMap<String, serde_json::Value>,
    /// Agent to run this child as. Only the agent the delegation is made for
    /// (its parent) may be named here: this field is filled from the *model's*
    /// tool arguments, and honouring another name would run the child under
    /// that agent's workspace, secrets and skill trust.
    #[serde(default)]
    pub target_agent: Option<String>,
    /// Optional shared-task id for the child (registry run id).  When set, the
    /// child's shared state is tracked under this task.
    #[serde(default)]
    pub task_id: Option<String>,
}

/// Child agent handle
#[derive(Debug, Clone)]
pub struct ChildAgent {
    /// Unique ID
    pub id: String,
    /// Parent agent ID
    pub parent_id: String,
    /// Task specification
    pub task: TaskSpec,
    /// Current status
    pub status: ChildStatus,
    /// Creation time
    pub created_at: chrono::DateTime<chrono::Utc>,
    /// Result (if completed)
    pub result: Option<String>,
    /// Error (if failed)
    pub error: Option<String>,
    /// Shared budget reference
    pub budget: IterationBudget,
    /// Current iteration count
    pub iterations: Arc<AtomicUsize>,
}

/// Child agent status
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChildStatus {
    /// Waiting to start
    Pending,
    /// Currently running
    Running,
    /// Completed successfully
    Completed,
    /// Failed with error
    Failed,
    /// Cancelled by parent
    Cancelled,
}

/// Delegation tracker for managing child agents
#[derive(Debug, Default)]
pub struct DelegationTracker {
    /// Active child agents
    children: Arc<RwLock<HashMap<String, ChildAgent>>>,
    /// Current delegation depth of the agent this tracker belongs to
    depth: usize,
    /// Maximum allowed children
    max_children: usize,
    /// Maximum nesting depth (agents at or beyond this may not delegate)
    max_depth: usize,
}

impl DelegationTracker {
    /// Create a new delegation tracker with default limits.
    pub fn new(depth: usize) -> Self {
        Self::with_limits(depth, DelegationConfig::default())
    }

    /// Create a new delegation tracker with explicit limits.
    pub fn with_limits(depth: usize, config: DelegationConfig) -> Self {
        Self {
            children: Arc::new(RwLock::new(HashMap::new())),
            depth,
            max_children: config.max_children,
            max_depth: config.max_depth as usize,
        }
    }

    /// Replace the depth/concurrency limits (e.g. from a config reload).
    pub fn set_limits(&mut self, config: DelegationConfig) {
        self.max_children = config.max_children;
        self.max_depth = config.max_depth as usize;
    }

    /// Check if delegation is allowed
    pub async fn can_delegate(&self) -> bool {
        if self.depth >= self.max_depth {
            return false;
        }
        let children = self.children.read().await;
        children.len() < self.max_children
    }

    /// Get current child count
    pub async fn child_count(&self) -> usize {
        let children = self.children.read().await;
        children.len()
    }

    /// Register a new child agent
    pub async fn register_child(&self, child: ChildAgent) {
        let mut children = self.children.write().await;
        children.insert(child.id.clone(), child);
    }

    /// Get a child agent by ID
    pub async fn get_child(&self, id: &str) -> Option<ChildAgent> {
        let children = self.children.read().await;
        children.get(id).cloned()
    }

    /// Update child status
    pub async fn update_status(&self, id: &str, status: ChildStatus) {
        let mut children = self.children.write().await;
        if let Some(child) = children.get_mut(id) {
            child.status = status;
        }
    }

    /// Set child result
    pub async fn set_result(&self, id: &str, result: String) {
        let mut children = self.children.write().await;
        if let Some(child) = children.get_mut(id) {
            child.status = ChildStatus::Completed;
            child.result = Some(result);
        }
    }

    /// Set child error
    pub async fn set_error(&self, id: &str, error: String) {
        let mut children = self.children.write().await;
        if let Some(child) = children.get_mut(id) {
            child.status = ChildStatus::Failed;
            child.error = Some(error);
        }
    }

    /// List all children
    pub async fn list_children(&self) -> Vec<ChildAgent> {
        let children = self.children.read().await;
        children.values().cloned().collect()
    }

    /// Remove a child
    pub async fn remove_child(&self, id: &str) -> Option<ChildAgent> {
        let mut children = self.children.write().await;
        children.remove(id)
    }
}

/// Trait for looking up running agents by name/type.
///
/// Used by [`DelegateTool`] to route child tasks to specific agents
/// based on the `target_agent` field in [`TaskSpec`].
#[async_trait]
pub trait AgentResolver: Send + Sync {
    /// Resolve a running agent by name (e.g. "coder", "reviewer").
    /// Returns `None` if no agent with that name is available.
    async fn resolve(&self, name: &str) -> Option<Arc<crate::agent::Agent>>;
}

impl Clone for DelegationTracker {
    fn clone(&self) -> Self {
        Self {
            children: Arc::clone(&self.children),
            depth: self.depth,
            max_children: self.max_children,
            max_depth: self.max_depth,
        }
    }
}
