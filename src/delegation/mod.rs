//! Delegation trees: shared task state, artifact references, and agent
//! handoff for multi-agent orchestration.
//!
//! This module gives syscity the LoopX-style canonical state for delegated
//! work:
//!
//! - [`DelegationTaskStore`] persists one row per delegated child task, with a
//!   shared JSON state blob, an append-only events ledger, and artifact refs.
//! - [`DelegationScope`] is the per-child contract threaded through message
//!   metadata: which shared task it belongs to, how deep it may recurse, which
//!   tools it may use, and its tool-iteration cap.
//! - [`TaskStateTool`] is how a child reads/writes its shared state.
//! - [`DelegationWake`] is how a parent that ended its turn is re-opened with a
//!   child's result when the child completes after the fact (see
//!   `docs/delegation-wake.md`).
//!
//! Orchestration is built on top of the existing [`DelegateTool`] +
//! [`SubagentRegistry`](crate::agent::subagent_registry::SubagentRegistry)
//! infrastructure; this module only adds the shared-state dimension.
// INVARIANTS-NONE: depth/limit enforcement happens at spawn time (DelegationScope) plus the startup orphan sweep; no state that can silently drift.

pub mod coordinator;
pub mod scope;
pub mod state;
pub mod task_state_tool;
pub mod wake;

pub use coordinator::DelegationCoordinator;
pub use scope::{DelegationScope, DELEGATION_SCOPE_KEY};
pub use state::{
    ArtifactRef, DelegationEvent, DelegationTask, DelegationTaskSnapshot, DelegationTaskStore,
    NewTask,
};
pub use task_state_tool::TaskStateTool;
pub use wake::{
    child_completion_message, child_failure_message, notify_parent, parent_active_for_wake,
    AgentWakeHandler, DelegationWake, WakeHandler, WakeResolver,
};

/// Errors specific to the delegation subsystem.
#[derive(Debug, thiserror::Error)]
pub enum DelegationError {
    /// The tool ran outside any active delegation scope.
    #[error("no active delegation context for this tool call")]
    NoActiveDelegation,
    /// The referenced delegation task row does not exist.
    #[error("delegation task not found: {0}")]
    TaskNotFound(String),
    /// The underlying task store failed.
    #[error("delegation store error: {0}")]
    Store(#[from] crate::error::SyscityError),
}

impl From<DelegationError> for crate::error::SyscityError {
    fn from(e: DelegationError) -> Self {
        match e {
            DelegationError::NoActiveDelegation => crate::error::SyscityError::Validation(
                "no active delegation context for this tool call".to_string(),
            ),
            DelegationError::TaskNotFound(id) => {
                crate::error::SyscityError::NotFound { resource: id }
            }
            DelegationError::Store(s) => s,
        }
    }
}

/// Tunable limits for delegation trees.
///
/// `max_depth` is the maximum nesting depth (top-level delegation = depth 1).
/// `max_children` caps the number of *simultaneously running* subagents.
#[derive(Debug, Clone)]
pub struct DelegationConfig {
    /// Maximum nesting depth of a delegation tree.
    pub max_depth: u32,
    /// Maximum concurrently running subagents.
    pub max_children: usize,
}

impl Default for DelegationConfig {
    fn default() -> Self {
        Self { max_depth: 3, max_children: 3 }
    }
}
