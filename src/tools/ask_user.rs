//! Human-in-the-loop clarification — the `ask_user` tool.
//!
//! Lets the agent pause a turn and ask the human a question, then resumes
//! with the answer. Mirrors the approval suspend/resume mechanism
//! (`src/tools/approval.rs`):
//!
//! 1. `AskUserTool::execute` runs inside the tool body (unlike approval,
//!    which is a registry-level gate *before* the tool body) and needs a
//!    handle to the queue, so the queue is injected into `ToolContext`.
//! 2. The tool creates a oneshot channel and calls `AskQueue::submit`, which
//!    stores the pending question and broadcasts an `AskRequiredEvent`.
//! 3. A gateway forwarder lifts that broadcast onto the WS event bus as
//!    `ask.required`; the web UI renders a modal and answers over the
//!    `ask.respond` WS method.
//! 4. `AskQueue::resolve` fires the oneshot, waking the blocked tool.
//!
//! Background/autonomous contexts (delegated sub-agents, goal runner, cron,
//! heartbeat, standing orders) have no interactive human — `ask_user`
//! refuses there with a clear message instead of silently blocking. They all
//! run as the `"system"` actor and are told apart from an interactive session
//! by their conversation-id prefix (see [`background_context_reason`]).

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use tokio::sync::{broadcast, oneshot, RwLock};
use tracing::{debug, info, warn};

use super::{Tool, ToolContext, ToolExecutionResult};
use crate::tools::sdk::ToolCapabilities;

/// A question the agent wants answered.
#[derive(Debug, Clone)]
pub struct AskRequest {
    /// Unique ask ID.
    pub id: String,
    /// Session / conversation the question belongs to (for event routing).
    pub conversation_id: String,
    /// The question text.
    pub question: String,
    /// Optional multiple-choice options; empty means free-text only.
    pub options: Vec<String>,
    /// Whether the human must answer (UI hint; `false` allows dismissing).
    pub required: bool,
    /// Optional default answer (pre-filled in the UI).
    pub default: Option<String>,
}

/// A pending question with the oneshot back-channel to the blocked tool.
#[derive(Debug)]
pub struct PendingQuestion {
    pub request: AskRequest,
    /// Channel to send the answer back to suspended execution.
    pub(crate) response_tx: Option<oneshot::Sender<String>>,
}

/// Broadcast payload when a question is submitted.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AskRequiredEvent {
    pub ask_id: String,
    pub session_id: String,
    pub question: String,
    pub options: Vec<String>,
    pub required: bool,
    pub default: Option<String>,
}

/// Broadcast payload when a question is answered or cancelled.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AskResolvedEvent {
    pub ask_id: String,
    /// Session the question belonged to (for WS routing).
    pub session_id: String,
    pub cancelled: bool,
}

/// Events carried by the `AskQueue` broadcast channel.
#[derive(Debug, Clone)]
pub enum AskEvent {
    Required(AskRequiredEvent),
    Resolved(AskResolvedEvent),
}

/// Thread-safe queue of pending questions with broadcast notifications.
#[derive(Debug, Clone)]
pub struct AskQueue {
    pending: Arc<RwLock<HashMap<String, PendingQuestion>>>,
    /// Broadcast channel for question lifecycle events.
    pub event_tx: broadcast::Sender<AskEvent>,
    /// Default timeout while waiting for a human answer.
    pub default_timeout: Duration,
}

impl AskQueue {
    /// Create a new empty queue.
    pub fn new() -> Self {
        let (event_tx, _) = broadcast::channel(256);
        Self {
            pending: Arc::new(RwLock::new(HashMap::new())),
            event_tx,
            default_timeout: Duration::from_secs(300), // 5 minutes
        }
    }

    /// Submit a question. Returns the ask ID; the caller awaits the oneshot
    /// receiver it supplied in `PendingQuestion.response_tx`.
    pub async fn submit(&self, pending: PendingQuestion) -> String {
        let id = pending.request.id.clone();
        let event = AskRequiredEvent {
            ask_id: id.clone(),
            session_id: pending.request.conversation_id.clone(),
            question: pending.request.question.clone(),
            options: pending.request.options.clone(),
            required: pending.request.required,
            default: pending.request.default.clone(),
        };

        {
            let mut pending_map = self.pending.write().await;
            pending_map.insert(id.clone(), pending);
        }

        info!("Question {} submitted: {}", id, event.question);

        if self.event_tx.send(AskEvent::Required(event)).is_err() {
            warn!("No active subscribers for ask event");
        }

        id
    }

    /// Answer a pending question, waking the blocked tool.
    ///
    /// Returns `true` if the question was found and answered, `false` if it
    /// was already resolved or never existed.
    pub async fn resolve(&self, ask_id: &str, response: String) -> bool {
        let pending = {
            let mut pending_map = self.pending.write().await;
            pending_map.remove(ask_id)
        };

        match pending {
            Some(mut p) => {
                let session_id = p.request.conversation_id.clone();
                if let Some(tx) = p.response_tx.take() {
                    let _ = tx.send(response);
                    info!("Question {} answered", ask_id);
                } else {
                    warn!("Question {} already resolved", ask_id);
                    return false;
                }
                let _ = self.event_tx.send(AskEvent::Resolved(AskResolvedEvent {
                    ask_id: ask_id.into(),
                    session_id,
                    cancelled: false,
                }));
                true
            }
            None => {
                warn!("Question {} not found", ask_id);
                false
            }
        }
    }

    /// Cancel a pending question (e.g. on tool timeout). Returns `true` if a
    /// pending entry was removed.
    pub async fn cancel(&self, ask_id: &str) -> bool {
        let (removed, session_id) = {
            let mut pending_map = self.pending.write().await;
            match pending_map.remove(ask_id) {
                Some(p) => (true, p.request.conversation_id),
                None => (false, String::new()),
            }
        };
        if removed {
            let _ = self.event_tx.send(AskEvent::Resolved(AskResolvedEvent {
                ask_id: ask_id.into(),
                session_id,
                cancelled: true,
            }));
            debug!("Question {} cancelled", ask_id);
        }
        removed
    }

    /// Number of pending questions.
    pub async fn len(&self) -> usize {
        self.pending.read().await.len()
    }

    /// Whether the queue has no pending questions.
    pub async fn is_empty(&self) -> bool {
        self.pending.read().await.is_empty()
    }
}

impl Default for AskQueue {
    fn default() -> Self {
        Self::new()
    }
}

/// Return a reason string when `ctx` has no interactive human to answer a
/// question, or `None` for a foreground human session.
pub fn background_context_reason(ctx: &ToolContext) -> Option<&'static str> {
    if ctx.delegation.is_some() {
        return Some("delegated sub-agent context has no interactive human");
    }
    if ctx.user_id == "system" {
        // Cron, heartbeat, standing orders and goal runs all execute with no
        // human present; they are told apart from an interactive session by the
        // conversation-id prefix they are launched under (goal ids are minted as
        // `goal_<uuid>`).
        let cid = ctx.conversation_id.as_str();
        if cid.starts_with("cron:")
            || cid.starts_with("heartbeat:")
            || cid.starts_with("standing_order:")
            || cid.starts_with("goal_")
        {
            return Some("background system context has no interactive human");
        }
    }
    None
}

/// Tool that pauses a turn and asks the human a clarifying question.
#[derive(Debug, Default)]
pub struct AskUserTool;

#[async_trait]
impl Tool for AskUserTool {
    fn name(&self) -> &str {
        "ask_user"
    }

    fn description(&self) -> &str {
        r#"Ask the human user a clarifying question and wait for their answer.

Use this when you need information you cannot obtain yourself: an ambiguous
request, a missing parameter, a decision between alternatives, or permission
to do something that needs human judgment.

Pass a short, specific `question`. Optionally provide `options` (a list of
choices) so the human can answer with one tap; without options they type a
free-text reply. `default` pre-fills the input. The call blocks until the
human answers (up to a few minutes), then returns their response.

This tool only works in interactive foreground sessions — it will not run in
delegated sub-agents or background jobs (goals, cron, heartbeat)."#
    }

    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "question": {
                    "type": "string",
                    "description": "The clarifying question to ask the human"
                },
                "options": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "Optional multiple-choice options"
                },
                "required": {
                    "type": "boolean",
                    "description": "Whether the human must answer (default true)"
                },
                "default": {
                    "type": "string",
                    "description": "Optional default answer to pre-fill"
                }
            },
            "required": ["question"]
        })
    }

    fn capabilities(&self) -> ToolCapabilities {
        ToolCapabilities {
            requires_approval: false,
            risk_level: crate::tools::approval::RiskLevel::Low,
            categories: vec!["interaction".to_string()],
            ..Default::default()
        }
    }

    fn is_available(&self, context: &ToolContext) -> bool {
        background_context_reason(context).is_none() && context.ask_queue.is_some()
    }

    async fn execute(
        &self,
        args: serde_json::Value,
        context: &ToolContext,
    ) -> crate::Result<ToolExecutionResult> {
        // Refuse in contexts with no interactive human.
        if let Some(reason) = background_context_reason(context) {
            return Ok(ToolExecutionResult::error(format!("ask_user cannot run: {reason}")));
        }
        let queue = context.ask_queue.as_ref().ok_or_else(|| {
            crate::error::SyscityError::Validation(
                "ask_user has no interactive channel in this context".to_string(),
            )
        })?;

        let question = args["question"].as_str().ok_or_else(|| {
            crate::error::SyscityError::Validation("question is required".to_string())
        })?;
        let options = args["options"]
            .as_array()
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str().map(String::from))
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        let required = args["required"].as_bool().unwrap_or(true);
        let default = args["default"].as_str().map(String::from);

        let (tx, rx) = oneshot::channel();
        let id = queue
            .submit(PendingQuestion {
                request: AskRequest {
                    id: format!("ask-{}", uuid::Uuid::new_v4()),
                    conversation_id: context.conversation_id.clone(),
                    question: question.to_string(),
                    options,
                    required,
                    default,
                },
                response_tx: Some(tx),
            })
            .await;

        match tokio::time::timeout(queue.default_timeout, rx).await {
            Ok(Ok(answer)) => Ok(ToolExecutionResult::success(format!("Answer: {answer}"))
                .with_data(serde_json::json!({ "answer": answer }))),
            Ok(Err(_)) => {
                queue.cancel(&id).await;
                Err(crate::error::SyscityError::Internal(
                    "ask_user: question channel closed before an answer was received".into(),
                ))
            }
            Err(_) => {
                queue.cancel(&id).await;
                Err(crate::error::SyscityError::Timeout(
                    "ask_user: no answer received before timeout".into(),
                ))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn queue_ctx(queue: &Arc<AskQueue>) -> ToolContext {
        ToolContext::new("user1", "conv1").with_ask_queue(Arc::clone(queue))
    }

    #[tokio::test]
    async fn test_ask_queue_submit_and_resolve() {
        let queue = AskQueue::new();
        let (tx, rx) = oneshot::channel();
        let id = queue
            .submit(PendingQuestion {
                request: AskRequest {
                    id: "ask-1".to_string(),
                    conversation_id: "conv1".to_string(),
                    question: "Proceed?".to_string(),
                    options: vec![],
                    required: true,
                    default: None,
                },
                response_tx: Some(tx),
            })
            .await;
        assert_eq!(id, "ask-1");
        assert_eq!(queue.len().await, 1);

        let answered = queue.resolve(&id, "yes".to_string()).await;
        assert!(answered);
        assert!(queue.is_empty().await);
        assert_eq!(rx.await.unwrap(), "yes");
    }

    #[tokio::test]
    async fn test_ask_queue_resolve_not_found() {
        let queue = AskQueue::new();
        let answered = queue.resolve("ask-missing", "nope".to_string()).await;
        assert!(!answered);
    }

    #[tokio::test]
    async fn test_ask_queue_double_resolve() {
        let queue = AskQueue::new();
        let (tx, rx) = oneshot::channel();
        let id = queue
            .submit(PendingQuestion {
                request: AskRequest {
                    id: "ask-2".to_string(),
                    conversation_id: "conv1".to_string(),
                    question: "Q".to_string(),
                    options: vec![],
                    required: true,
                    default: None,
                },
                response_tx: Some(tx),
            })
            .await;
        assert!(queue.resolve(&id, "a".into()).await);
        assert!(!queue.resolve(&id, "b".into()).await);
        assert_eq!(rx.await.unwrap(), "a");
    }

    #[tokio::test]
    async fn test_ask_queue_cancel() {
        let queue = AskQueue::new();
        let (tx, _rx) = oneshot::channel();
        let id = queue
            .submit(PendingQuestion {
                request: AskRequest {
                    id: "ask-3".to_string(),
                    conversation_id: "conv1".to_string(),
                    question: "Q".to_string(),
                    options: vec![],
                    required: true,
                    default: None,
                },
                response_tx: Some(tx),
            })
            .await;
        assert!(queue.cancel(&id).await);
        assert!(queue.is_empty().await);
        assert!(!queue.cancel(&id).await);
    }

    #[tokio::test]
    async fn test_ask_queue_broadcasts_required() {
        let queue = AskQueue::new();
        let mut rx = queue.event_tx.subscribe();
        let (tx, _unused) = oneshot::channel();
        let id = queue
            .submit(PendingQuestion {
                request: AskRequest {
                    id: "ask-4".to_string(),
                    conversation_id: "conv-sess".to_string(),
                    question: "Which env?".to_string(),
                    options: vec!["dev".into(), "prod".into()],
                    required: true,
                    default: Some("dev".into()),
                },
                response_tx: Some(tx),
            })
            .await;

        match rx.try_recv().expect("broadcast event") {
            AskEvent::Required(evt) => {
                assert_eq!(evt.ask_id, id);
                assert_eq!(evt.session_id, "conv-sess");
                assert_eq!(evt.question, "Which env?");
                assert_eq!(evt.options, vec!["dev", "prod"]);
                assert_eq!(evt.default.as_deref(), Some("dev"));
            }
            other => panic!("unexpected event: {other:?}"),
        }
    }

    #[test]
    fn test_background_context_reason_matrix() {
        // Foreground human session.
        assert!(background_context_reason(&ToolContext::new("user1", "conv1")).is_none());

        // Delegated sub-agent.
        let mut ctx = ToolContext::new("child:abc", "delegation:abc");
        ctx.delegation = Some(crate::delegation::DelegationScope {
            root_id: "r".into(),
            task_id: "t".into(),
            depth: 1,
            max_depth: 3,
            parent_task_id: None,
            allowed_tools: None,
            max_iterations: None,
        });
        assert!(background_context_reason(&ctx).is_some());

        // Goal run (goal ids are minted as `goal_<uuid>`).
        assert!(background_context_reason(&ToolContext::new("system", "goal_1a2b3c4d")).is_some());

        // Background system contexts.
        assert!(background_context_reason(&ToolContext::new("system", "cron:job-1")).is_some());
        assert!(
            background_context_reason(&ToolContext::new("system", "heartbeat:agent-1")).is_some()
        );
        assert!(
            background_context_reason(&ToolContext::new("system", "standing_order:so-1")).is_some()
        );
    }

    #[tokio::test]
    async fn test_ask_user_tool_answer_flow() {
        let queue = Arc::new(AskQueue::new());
        let tool = AskUserTool::default();
        let ctx = queue_ctx(&queue);

        // Resolve the question shortly after it is submitted.
        let q = Arc::clone(&queue);
        let spawn = tokio::spawn(async move {
            // Poll until a question is pending, then answer it.
            for _ in 0..100 {
                if q.is_empty().await {
                    tokio::time::sleep(Duration::from_millis(10)).await;
                    continue;
                }
                let ids = q.pending.read().await.keys().cloned().collect::<Vec<_>>();
                for id in ids {
                    q.resolve(&id, "42".to_string()).await;
                }
                return;
            }
            panic!("no question submitted in time");
        });

        let args = serde_json::json!({ "question": "What is the answer?" });
        let result = tool.execute(args, &ctx).await.expect("tool runs");
        assert!(result.success);
        assert!(result.output.contains("Answer: 42"));
        assert_eq!(
            result
                .data
                .and_then(|d| d["answer"].as_str().map(String::from))
                .as_deref(),
            Some("42")
        );
        let _ = spawn.await;
    }

    #[tokio::test]
    async fn test_ask_user_tool_refuses_background() {
        let queue = Arc::new(AskQueue::new());
        let tool = AskUserTool::default();

        // Goal runner context (no queue anyway).
        let goal_ctx = ToolContext::new("system", "goal_1a2b3c4d");
        let result = tool
            .execute(serde_json::json!({ "question": "Q" }), &goal_ctx)
            .await
            .expect("tool returns result");
        assert!(result.is_error());
        assert!(result.error.unwrap().contains("no interactive human"));

        // Cron context via system user (queue present but still refused).
        let cron_ctx = ToolContext::new("system", "cron:job-1").with_ask_queue(Arc::clone(&queue));
        let result = tool
            .execute(serde_json::json!({ "question": "Q" }), &cron_ctx)
            .await
            .expect("tool returns result");
        assert!(result.is_error());
        assert!(result.error.unwrap().contains("no interactive human"));

        // No queue at all.
        let bare = ToolContext::new("user1", "conv1");
        let result = tool
            .execute(serde_json::json!({ "question": "Q" }), &bare)
            .await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_ask_user_requires_question() {
        let queue = Arc::new(AskQueue::new());
        let tool = AskUserTool::default();
        let ctx = queue_ctx(&queue);
        let result = tool.execute(serde_json::json!({}), &ctx).await;
        assert!(result.is_err());
    }

    #[test]
    fn test_ask_user_not_available_in_background() {
        let queue = Arc::new(AskQueue::new());
        let tool = AskUserTool::default();
        assert!(tool.is_available(&ToolContext::new("system", "cron:job-1")) == false);
        assert!(tool.is_available(&ToolContext::new("user1", "conv1")) == false); // no queue
        assert!(tool.is_available(&queue_ctx(&queue)));
    }
}
