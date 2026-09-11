//! Goal runner — the sub-agent that executes a [`GoalPlan`] in a loop.
//!
//! The [`GoalRunner`] spawns as a background task, running iterations of
//! "agent acts → check conditions → feedback → repeat" until all conditions
//! pass or a guardrail trips.
//!
//! # Loop modes
//!
//! - **Legacy (default)** — one agent loop driven through the model router,
//!   with condition feedback as the only cross-round signal.
//! - **Fresh-context ("Ralph", [`GoalPlan::fresh_context`])** — every round
//!   runs in a brand-new seedless sub-agent turn: the message vector is built
//!   from scratch (system prompt + one user message) with no parent
//!   conversation prefix, no session history, and no personality/memory
//!   seeding. The workspace on disk is the long-term memory; between rounds
//!   the only LLM-produced state carried is a bounded, strictly-validated
//!   [`RoundHandoff`](crate::goal::handoff::RoundHandoff). Fatal configuration
//!   errors (missing model/provider) abort the loop loudly instead of being
//!   swallowed as transient agent failures.

use std::sync::Arc;

use tokio_util::sync::CancellationToken;

use crate::error::ConfigError;
use crate::goal::condition::CheckResult;
use crate::goal::event::GoalEvent;
use crate::goal::handoff::{extract_handoff, HandoffStatus, RoundHandoff};
use crate::goal::persist;
use crate::goal::plan::GoalPlan;
use crate::model_router::ModelRouter;
use crate::providers::{Message, ToolDefinition};
use crate::tools::ToolContext;
use crate::tools::ToolRegistry;
use crate::Result;

/// Why a single fresh-context round could not produce a valid outcome.
#[derive(Debug)]
enum FreshRoundFailure {
    /// Missing/broken model or provider configuration. Not retryable from
    /// inside the loop — re-raised loudly so the operator fixes config.
    FatalConfig(String),
    /// The round's final reply carried no valid structured handoff.
    InvalidHandoff(String),
    /// Any other agent-side failure (LLM error, transport error, ...).
    Agent(String),
}

/// Maximum consecutive identical failures before loop detection triggers.
const MAX_CONSECUTIVE_IDENTICAL_FAILURES: usize = 3;

/// Maximum number of LLM → tool → LLM iterations within a single agent round.
const MAX_TOOL_ITERATIONS: usize = 25;

/// Cap for a single tool result before it enters the round context. A tool
/// call can return arbitrarily large output, and up to `MAX_TOOL_ITERATIONS`
/// results accumulate in one round, so one uncapped result can blow the
/// context window. Mirrors the cron executor's head-kept byte cap.
const MAX_TOOL_RESULT_BYTES: usize = 8 * 1024;

/// Write a human-readable round note so users can browse long-running goal
/// progress in the default workspace: `<workspace>/goals/<goal-id>/round-N.md`.
///
/// `base` is separated out for testability; production callers pass the
/// default workspace. Best-effort — callers log failures, never abort the
/// loop for them.
pub(crate) async fn write_round_note(
    base: &std::path::Path,
    goal_id: &str,
    description: &str,
    round: usize,
    handoff: &RoundHandoff,
) -> std::io::Result<std::path::PathBuf> {
    let dir = base.join("goals").join(goal_id);
    tokio::fs::create_dir_all(&dir).await?;
    let path = dir.join(format!("round-{}.md", round));

    let mut out = String::new();
    out.push_str(&format!("# Round {} — {}\n\n", round, description));
    out.push_str(&format!("- **Goal**: `{}`\n", goal_id));
    out.push_str(&format!("- **Status**: `{:?}`\n", handoff.status));
    out.push_str(&format!("- **Time**: {}\n", chrono::Local::now().to_rfc3339()));
    out.push_str("\n## Summary\n\n");
    out.push_str(&handoff.summary);
    out.push('\n');
    if !handoff.evidence.is_empty() {
        out.push_str("\n## Evidence\n\n");
        for e in &handoff.evidence {
            out.push_str(&format!("- {}\n", e));
        }
    }
    if !handoff.next_steps.is_empty() {
        out.push_str("\n## Next steps\n\n");
        for s in &handoff.next_steps {
            out.push_str(&format!("- {}\n", s));
        }
    }
    tokio::fs::write(&path, out).await?;
    Ok(path)
}

/// A background goal runner that acts and checks conditions in a loop.
pub struct GoalRunner {
    /// Unique identifier for this goal run.
    pub id: String,
    /// Session ID of the parent (originating) session.
    pub parent_session_id: String,
    /// The goal plan with conditions.
    pub plan: GoalPlan,
    /// Current round number (1-indexed).
    pub round: usize,
    /// Tool registry for tool execution.
    tools: Arc<ToolRegistry>,
    /// Model router for LLM completions.
    model_router: Arc<ModelRouter>,
    /// Optional model override for the sub-agent (from GoalPlan).
    model_override: Option<String>,
    /// History of round results (for loop detection).
    pub condition_history: Vec<RoundResult>,
    /// Cancel signal.
    cancel: CancellationToken,
    /// Channel to emit events back to the originating session.
    event_tx: tokio::sync::mpsc::UnboundedSender<GoalEvent>,
    /// Optional persisted state store for checkpoint persistence.
    store: Option<crate::goal::persist::SharedGoalStore>,
    /// Last validated structured handoff (fresh-context mode only).
    last_handoff: Option<RoundHandoff>,
    /// Cumulative executor token spend across all LLM calls of this goal
    /// (cost axis: report efficiency alongside the verdict).
    token_usage: crate::agent::turns::TurnUsage,
    /// In-flight round message history: consumed once at round entry when
    /// resuming a mid-round checkpoint; `None` means "build the round fresh".
    round_messages: Option<Vec<Message>>,
}

#[derive(Debug, Clone)]
pub struct RoundResult {
    pub round: usize,
    pub results: Vec<CheckResult>,
}

impl GoalRunner {
    /// Create a new goal runner.
    pub fn new(
        id: impl Into<String>,
        parent_session_id: impl Into<String>,
        plan: GoalPlan,
        tools: Arc<ToolRegistry>,
        model_router: Arc<ModelRouter>,
        event_tx: tokio::sync::mpsc::UnboundedSender<GoalEvent>,
    ) -> Self {
        let model_override = plan.model_override.clone();
        Self {
            id: id.into(),
            parent_session_id: parent_session_id.into(),
            plan,
            round: 0,
            tools,
            model_router,
            model_override,
            condition_history: Vec::new(),
            cancel: CancellationToken::new(),
            event_tx,
            store: None,
            last_handoff: None,
            token_usage: Default::default(),
            round_messages: None,
        }
    }

    /// Attach a persistence store so the runner checkpoints state after each
    /// round.
    pub fn with_store(mut self, store: crate::goal::persist::SharedGoalStore) -> Self {
        self.store = Some(store);
        self
    }

    /// Set the initial round and condition history (used when restoring from
    /// persistence).
    pub fn with_progress(mut self, round: usize, condition_history: Vec<RoundResult>) -> Self {
        self.round = round;
        self.condition_history = condition_history;
        self
    }

    /// Restore the last validated structured handoff (used when resuming a
    /// persisted fresh-context goal).
    pub fn with_handoff(mut self, handoff: Option<RoundHandoff>) -> Self {
        self.last_handoff = handoff;
        self
    }

    /// Restore the cumulative token usage from a persisted checkpoint so a
    /// resumed goal keeps counting from where it left off.
    pub fn with_token_usage(mut self, usage: Option<crate::agent::turns::TurnUsage>) -> Self {
        if let Some(u) = usage {
            self.token_usage = u;
        }
        self
    }

    /// Restore an in-flight round's message history from a mid-round
    /// checkpoint so the resumed run continues the round instead of
    /// restarting it. Consumed by the first round entry.
    pub fn with_round_messages(mut self, messages: Option<Vec<Message>>) -> Self {
        self.round_messages = messages;
        self
    }

    /// Get a cancel token for this runner (for external cancellation).
    pub fn cancel_token(&self) -> CancellationToken {
        self.cancel.clone()
    }

    /// Cancel the goal runner.
    pub fn cancel(&self) {
        self.cancel.cancel();
    }

    /// Run the goal execution loop.
    ///
    /// 1. Emits `Started` event (or `Started` with resume note if restoring).
    /// 2. Loop: emit `Retry` → agent acts → check conditions → emit `Check`
    /// 3. If all pass: emit `Done` and return.
    /// 4. If guardrail: emit `Aborted` and return.
    pub async fn run(mut self) {
        let is_resume = self.round > 0;

        if !is_resume {
            // Emit started event (fresh goal).
            let conditions_desc: Vec<String> =
                self.plan.conditions.iter().map(|c| c.to_string()).collect();
            self.emit(GoalEvent::Started {
                id: self.id.clone(),
                description: self.plan.description.clone(),
                conditions: conditions_desc,
                max_rounds: self.plan.max_rounds,
            });
            self.round = 1;
        } else {
            // Resuming a persisted goal — emit a resume notification.
            self.emit(GoalEvent::Started {
                id: self.id.clone(),
                description: format!(
                    "{} (resumed, round {}/{})",
                    self.plan.description, self.round, self.plan.max_rounds
                ),
                conditions: self.plan.conditions.iter().map(|c| c.to_string()).collect(),
                max_rounds: self.plan.max_rounds,
            });
        }

        while self.round <= self.plan.max_rounds {
            // Check cancellation.
            if self.cancel.is_cancelled() {
                self.emit(GoalEvent::Aborted {
                    reason: "cancelled".to_string(),
                    round: self.round,
                    results: Vec::new(),
                    blocked_reason: Some(crate::goal::BlockedReason {
                        code: crate::goal::BlockedReasonCode::Cancelled,
                        message: "cancelled by user".to_string(),
                    }),
                    token_usage: self.reported_usage(),
                });
                self.cleanup_store().await;
                return;
            }

            // Build feedback for the agent from previous round.
            let feedback = if self.plan.fresh_context {
                self.build_fresh_feedback()
            } else {
                self.build_feedback()
            };

            // Emit retry event (or initial started feedback).
            if self.round > 1 || !feedback.is_empty() {
                self.emit(GoalEvent::Retry { round: self.round, feedback });
            }

            // Agent acts: run the agent with the goal context.
            if self.plan.fresh_context {
                match self.run_fresh_round().await {
                    Ok(Some(handoff)) => {
                        tracing::info!(
                            "[goal {}] Round {} handoff: {:?}",
                            self.id,
                            self.round,
                            handoff.status
                        );
                        // Best-effort: also leave a human-readable note in the
                        // workspace so users can watch long-goal progress.
                        if let Err(e) = write_round_note(
                            &crate::dirs::workspace_data_dir(),
                            &self.id,
                            &self.plan.description,
                            self.round,
                            &handoff,
                        )
                        .await
                        {
                            tracing::warn!("[goal {}] Failed to write round note: {}", self.id, e);
                        }
                        self.last_handoff = Some(handoff);
                    }
                    // Cancelled mid-round — the post-round cancel check below
                    // owns the abort.
                    Ok(None) => {}
                    Err(FreshRoundFailure::FatalConfig(msg)) => {
                        tracing::error!(
                            "[goal {}] Fatal configuration error in round {}: {}",
                            self.id,
                            self.round,
                            msg
                        );
                        self.abort_with_terminal(
                            format!("fatal_config_error: {}", msg),
                            crate::goal::BlockedReason {
                                code: crate::goal::BlockedReasonCode::FatalConfigError,
                                message: msg,
                            },
                        )
                        .await;
                        return;
                    }
                    Err(FreshRoundFailure::InvalidHandoff(msg)) => {
                        // Strict rejection: an invalid handoff is a policy
                        // stop, not something to guess or truncate around.
                        tracing::warn!(
                            "[goal {}] Invalid handoff in round {}: {}",
                            self.id,
                            self.round,
                            msg
                        );
                        self.abort_with_terminal(
                            format!("invalid_handoff: {}", msg),
                            crate::goal::BlockedReason {
                                code: crate::goal::BlockedReasonCode::InvalidHandoff,
                                message: msg,
                            },
                        )
                        .await;
                        return;
                    }
                    Err(FreshRoundFailure::Agent(msg)) => {
                        tracing::warn!("[goal {}] Agent round failed: {}", self.id, msg);
                        self.emit(GoalEvent::Aborted {
                            reason: format!("agent_error: {}", msg),
                            round: self.round,
                            results: Vec::new(),
                            blocked_reason: Some(crate::goal::BlockedReason {
                                code: crate::goal::BlockedReasonCode::AgentError,
                                message: msg,
                            }),
                            token_usage: self.reported_usage(),
                        });
                        self.cleanup_store().await;
                        return;
                    }
                }
            } else if let Err(e) = self.run_agent_round().await {
                tracing::warn!("[goal {}] Agent round failed: {}", self.id, e);
                self.emit(GoalEvent::Aborted {
                    reason: format!("agent_error: {}", e),
                    round: self.round,
                    results: Vec::new(),
                    blocked_reason: Some(crate::goal::BlockedReason {
                        code: crate::goal::BlockedReasonCode::AgentError,
                        message: e.to_string(),
                    }),
                    token_usage: self.reported_usage(),
                });
                self.cleanup_store().await;
                return;
            }

            // Check all conditions.
            let mut results: Vec<CheckResult> = Vec::new();
            for condition in &self.plan.conditions {
                let result = condition.check().await;
                results.push(result);
            }

            let passed = results.iter().filter(|r| r.passed).count();
            let total = results.len();

            self.emit(GoalEvent::Check {
                round: self.round,
                results: results.clone(),
                passed,
                total,
            });

            // Check if all passed.
            if passed == total {
                let summary = format!(
                    "目标完成: {} ({} 轮, {}/{} 条件通过)",
                    self.plan.description, self.round, passed, total
                );
                self.emit(GoalEvent::Done {
                    total_rounds: self.round,
                    all_passed: true,
                    summary,
                    token_usage: self.reported_usage(),
                });
                // Clean up persisted state on successful completion.
                if let Some(ref store) = self.store {
                    let goal_id = self.id.clone();
                    let store_clone = Arc::clone(store);
                    tokio::spawn(async move {
                        let store = store_clone.read().await;
                        store.delete(&goal_id).await;
                    });
                }
                return;
            }

            // A cancel that arrived mid-round now wins over any policy stop:
            // a cancelled round must not leave a persisted terminal checkpoint.
            if self.cancel.is_cancelled() {
                self.emit(GoalEvent::Aborted {
                    reason: "cancelled".to_string(),
                    round: self.round,
                    results,
                    blocked_reason: Some(crate::goal::BlockedReason {
                        code: crate::goal::BlockedReasonCode::Cancelled,
                        message: "cancelled by user".to_string(),
                    }),
                    token_usage: self.reported_usage(),
                });
                self.cleanup_store().await;
                return;
            }

            // Save checkpoint after each round.
            self.save_checkpoint().await;

            // Fresh-context policies (only relevant when continuing).
            if self.plan.fresh_context {
                if let Some(handoff) = &self.last_handoff {
                    match handoff.status {
                        HandoffStatus::Failed => {
                            // The agent declared it cannot proceed — stop for
                            // human review, keeping the checkpoint so the
                            // failure summary and progress survive.
                            let reason = crate::goal::BlockedReason {
                                code: crate::goal::BlockedReasonCode::AgentError,
                                message: format!("agent reported failure: {}", handoff.summary),
                            };
                            tracing::warn!(
                                "[goal {}] Agent declared failure: {}",
                                self.id,
                                handoff.summary
                            );
                            self.abort_with_terminal(
                                format!("goal_failed_by_agent: {}", handoff.summary),
                                reason,
                            )
                            .await;
                            return;
                        }
                        HandoffStatus::Complete => {
                            // The agent claims completion but deterministic
                            // conditions disagree; conditions are authoritative.
                            tracing::warn!(
                                "[goal {}] Round {} claimed complete but {}/{} conditions pass; continuing",
                                self.id,
                                self.round,
                                passed,
                                total
                            );
                        }
                        HandoffStatus::Continue => {}
                    }
                }
            }

            // Loop detection: consecutive identical failures.
            self.condition_history.push(RoundResult {
                round: self.round,
                results: results.clone(),
            });
            if self.detect_loop() {
                let reason = crate::goal::BlockedReason {
                    code: crate::goal::BlockedReasonCode::LoopDetected,
                    message: "same conditions failed 3 rounds in a row with identical output"
                        .to_string(),
                };
                self.emit(GoalEvent::Aborted {
                    reason: format!("loop_detected: {}", reason.message),
                    round: self.round,
                    results,
                    blocked_reason: Some(reason.clone()),
                    token_usage: self.reported_usage(),
                });
                // Policy stop: keep the checkpoint with the reason so the
                // cause survives and a human can resume deliberately.
                self.save_terminal(reason).await;
                return;
            }

            self.round += 1;
        }

        // Max rounds reached.
        let results: Vec<CheckResult> = self
            .condition_history
            .last()
            .map(|r| r.results.clone())
            .unwrap_or_default();
        let reason = crate::goal::BlockedReason {
            code: crate::goal::BlockedReasonCode::MaxRounds,
            message: format!("reached {} rounds", self.plan.max_rounds),
        };
        self.emit(GoalEvent::Aborted {
            reason: format!("max_rounds: {}", reason.message),
            round: self.plan.max_rounds,
            results,
            blocked_reason: Some(reason.clone()),
            token_usage: self.reported_usage(),
        });
        // Policy stop: keep the checkpoint with the reason (see above).
        self.save_terminal(reason).await;
    }

    /// Run the agent for one round.
    ///
    /// Builds a goal-specific system prompt + current progress, sends it to the
    /// LLM via the model router, and executes any tool calls the LLM makes.
    /// Repeats (LLM → tools → LLM → …) up to [`MAX_TOOL_ITERATIONS`]
    /// iterations.
    async fn run_agent_round(&mut self) -> Result<()> {
        // Get tool definitions from the registry.
        let tool_defs: Vec<ToolDefinition> = self
            .tools
            .get_definitions()
            .into_iter()
            .map(|f| ToolDefinition {
                tool_type: "function".to_string(),
                function: f,
            })
            .collect();

        // Take a mid-round resume context if a mid-round checkpoint was
        // restored; otherwise build the round from scratch (system prompt +
        // current progress feedback). `take()` makes this one-shot: later
        // rounds always build fresh.
        let mut messages = match self.round_messages.take() {
            Some(restored) => {
                tracing::info!(
                    "[goal {}] Round {} resumed mid-round from {} message(s)",
                    self.id,
                    self.round,
                    restored.len()
                );
                restored
            }
            None => vec![
                // Build system prompt describing the goal and available tools.
                Message::system(self.build_agent_system_prompt()),
                // Build the user message with current progress / feedback.
                Message::user(self.build_feedback()),
            ],
        };

        // Owned so the borrow lives independently of `self` (the round may
        // mutate self, e.g. to accumulate token usage).
        let model = self
            .model_override
            .clone()
            .unwrap_or_else(|| "default".to_string());

        // Create a tool context for tool execution.
        let tool_ctx = ToolContext::new("system", &self.id)
            .with_workspace_root(crate::dirs::workspace_data_dir())
            .with_model_name(model.clone())
            .with_provider_name("model_router");

        for iteration in 0..MAX_TOOL_ITERATIONS {
            // Check cancellation between iterations.
            if self.cancel.is_cancelled() {
                tracing::info!(
                    "[goal {}] Cancelled during agent round (iteration {})",
                    self.id,
                    iteration
                );
                return Ok(());
            }

            let response = self
                .model_router
                .complete(&model, messages.clone(), Some(tool_defs.clone()))
                .await
                .map_err(|e| {
                    crate::error::SyscityError::Internal(format!(
                        "LLM completion failed in goal round {}: {}",
                        self.round, e
                    ))
                })?;
            self.accumulate_usage(&response.usage);

            let has_tool_calls = response
                .message
                .tool_calls
                .as_ref()
                .is_some_and(|c: &Vec<crate::providers::ToolCall>| !c.is_empty());

            if !has_tool_calls {
                // No more tool calls — agent is done responding.
                tracing::debug!(
                    "[goal {}] Agent finished after {} iteration(s)",
                    self.id,
                    iteration + 1
                );
                break;
            }

            // Take the tool calls out of the response (clone to keep response.message
            // valid).
            let tool_calls = response.message.tool_calls.clone().unwrap_or_default();

            // Push the assistant's response (with tool_calls) to the context.
            messages.push(response.message);

            // Execute each tool call sequentially.
            for tc in &tool_calls {
                if self.cancel.is_cancelled() {
                    return Ok(());
                }

                tracing::debug!(
                    "[goal {}] Executing tool: {} (id={})",
                    self.id,
                    tc.function.name,
                    tc.id
                );

                let result = self.tools.execute_call(&tc.function, &tool_ctx).await;

                let result_str = match result {
                    Ok(exec_result) => {
                        if exec_result.success {
                            exec_result.output
                        } else {
                            format!("Error: {}", exec_result.error.unwrap_or_default())
                        }
                    }
                    Err(e) => format!("Tool execution error: {}", e),
                };

                messages.push(Message::tool(truncate_tool_output(&result_str), &tc.id));
            }

            // Mid-round snapshot: the prefix always ends with a complete
            // assistant→tool group here, so a crash resumes the round instead
            // of restarting it.
            self.save_midround_checkpoint(&messages).await;
        }

        tracing::info!("[goal {}] Agent round {} completed", self.id, self.round);
        Ok(())
    }

    /// Build the system prompt for the sub-agent with goal context.
    fn build_agent_system_prompt(&self) -> String {
        let conditions: Vec<String> = self
            .plan
            .conditions
            .iter()
            .map(|c| format!("  [ ] {}", c))
            .collect();

        format!(
            r#"You are an autonomous goal-execution agent. Your task is to complete the following goal.

## Goal
{}

## Check Conditions (all must pass)
{}

## Rules
1. Use the available tools to accomplish the goal.
2. After each tool call, the LLM will receive the tool result.
3. Conditions are checked automatically after each round — you only need to take action.
4. You may call tools multiple times to iterate and refine your work.
5. Working directory: {}
6. Do not modify .git directories or sensitive configuration files.
7. When done, reply with a brief completion message."#,
            self.plan.description,
            conditions.join("\n"),
            crate::dirs::workspace_data_dir().display(),
        )
    }

    /// Run one fresh-context ("Ralph") round.
    ///
    /// Spawns a brand-new seedless sub-agent turn: the conversation starts
    /// from scratch (system prompt + one user message carrying the previous
    /// handoff and condition status) with no parent prefix or session state.
    /// On success returns the validated [`RoundHandoff`] parsed from the
    /// agent's final reply (`None` if cancelled mid-round).
    async fn run_fresh_round(
        &mut self,
    ) -> std::result::Result<Option<RoundHandoff>, FreshRoundFailure> {
        let tool_defs: Vec<ToolDefinition> = self
            .tools
            .get_definitions()
            .into_iter()
            .map(|f| ToolDefinition {
                tool_type: "function".to_string(),
                function: f,
            })
            .collect();

        // Take a mid-round resume context if a mid-round checkpoint was
        // restored; otherwise build the round from scratch. Fresh by
        // construction across rounds: nothing accumulates except through
        // this one-shot resume seam.
        let mut messages = match self.round_messages.take() {
            Some(restored) => {
                tracing::info!(
                    "[goal {}] Round {} resumed mid-round from {} message(s)",
                    self.id,
                    self.round,
                    restored.len()
                );
                restored
            }
            None => vec![
                Message::system(self.build_fresh_system_prompt()),
                Message::user(self.build_fresh_feedback()),
            ],
        };

        // Owned so the borrow lives independently of `self` (the round may
        // mutate self, e.g. to accumulate token usage).
        let model = self
            .model_override
            .clone()
            .unwrap_or_else(|| "default".to_string());

        let tool_ctx = ToolContext::new("system", &self.id)
            .with_workspace_root(crate::dirs::workspace_data_dir())
            .with_model_name(model.clone())
            .with_provider_name("model_router");

        let mut final_content: Option<String> = None;

        for iteration in 0..MAX_TOOL_ITERATIONS {
            if self.cancel.is_cancelled() {
                tracing::info!(
                    "[goal {}] Cancelled during fresh round (iteration {})",
                    self.id,
                    iteration
                );
                return Ok(None);
            }

            let response = self
                .model_router
                .complete(&model, messages.clone(), Some(tool_defs.clone()))
                .await
                .map_err(classify_completion_error)?;
            self.accumulate_usage(&response.usage);

            let has_tool_calls = response
                .message
                .tool_calls
                .as_ref()
                .is_some_and(|c: &Vec<crate::providers::ToolCall>| !c.is_empty());

            if !has_tool_calls {
                tracing::debug!(
                    "[goal {}] Fresh round finished after {} iteration(s)",
                    self.id,
                    iteration + 1
                );
                final_content = Some(response.message.content);
                break;
            }

            let tool_calls = response.message.tool_calls.clone().unwrap_or_default();
            messages.push(response.message);

            for tc in &tool_calls {
                if self.cancel.is_cancelled() {
                    return Ok(None);
                }

                tracing::debug!(
                    "[goal {}] Executing tool: {} (id={})",
                    self.id,
                    tc.function.name,
                    tc.id
                );

                let result = self.tools.execute_call(&tc.function, &tool_ctx).await;

                let result_str = match result {
                    Ok(exec_result) => {
                        if exec_result.success {
                            exec_result.output
                        } else {
                            format!("Error: {}", exec_result.error.unwrap_or_default())
                        }
                    }
                    Err(e) => format!("Tool execution error: {}", e),
                };

                messages.push(Message::tool(truncate_tool_output(&result_str), &tc.id));
            }

            // Mid-round snapshot: the prefix always ends with a complete
            // assistant→tool group here, so a crash resumes the round instead
            // of restarting it.
            self.save_midround_checkpoint(&messages).await;
        }

        // Iteration cap exhausted without a plain-text closing reply: feed the
        // extractor whatever we have so it can reject precisely instead of
        // inventing a handoff.
        let final_text = final_content.unwrap_or_default();
        let handoff = extract_handoff(&final_text).map_err(FreshRoundFailure::InvalidHandoff)?;
        Ok(Some(handoff))
    }

    /// Build the static part of a fresh-round prompt: role, freshness
    /// contract, workspace-as-memory rules, and the handoff output schema.
    fn build_fresh_system_prompt(&self) -> String {
        let conditions: Vec<String> = self
            .plan
            .conditions
            .iter()
            .map(|c| format!("  [ ] {}", c))
            .collect();

        format!(
            r#"You are an autonomous worker executing ONE round of a long-running goal loop.

## Freshness contract
- You start with NO memory of previous rounds. Do not assume anything happened
  unless it is stated below or readable from files in the workspace.
- The workspace on disk is the ONLY long-term memory. Persist durable findings,
  decisions and progress to files in the working directory before you finish;
  later rounds will rely on reading them back.

## Goal
{goal}

## Check Conditions (all must pass; checked automatically after each round)
{conditions}

## Rules
1. Use the available tools to accomplish this round's work.
2. Working directory: {workdir}
3. Do not modify .git directories or sensitive configuration files.

## Required output: structured handoff
End your FINAL reply with exactly one fenced block tagged `{tag}` containing JSON:

```{tag}
{{"status": "continue", "summary": "...", "next_steps": ["..."], "evidence": ["..."]}}
```

Schema rules (strictly enforced; violations fail the whole round):
- "status": one of "continue", "complete", "failed".
  - "continue" — work remains; "next_steps" MUST list concrete tasks for the next round.
  - "complete" — you believe every condition passes; "evidence" MUST list proof
    (file paths, command output quotes).
  - "failed" — you cannot proceed; say why in "summary".
- "summary": non-empty description of what THIS round did and found.
- Keep the whole block under {limit} characters; oversized or schema-invalid
  blocks are rejected outright, never truncated."#,
            goal = self.plan.description,
            conditions = conditions.join("\n"),
            workdir = crate::dirs::workspace_data_dir().display(),
            tag = crate::goal::handoff::HANDOFF_FENCE_TAG,
            limit = crate::goal::handoff::MAX_HANDOFF_CHARS,
        )
    }

    /// Build the dynamic per-round user message: prior handoff (the only
    /// carried LLM state) plus the deterministic condition results.
    fn build_fresh_feedback(&self) -> String {
        let mut lines = vec![
            format!("## Round {} of {}", self.round, self.plan.max_rounds),
            String::new(),
        ];

        match &self.last_handoff {
            Some(handoff) => {
                lines.push(
                    "## Handoff from the previous round (your only memory of it)".to_string(),
                );
                lines.push(format!("- status: {:?}", handoff.status));
                lines.push(format!("- summary: {}", handoff.summary));
                if !handoff.next_steps.is_empty() {
                    lines.push("- next steps:".to_string());
                    for step in &handoff.next_steps {
                        lines.push(format!("  - {}", step));
                    }
                }
                if !handoff.evidence.is_empty() {
                    lines.push("- evidence:".to_string());
                    for item in &handoff.evidence {
                        lines.push(format!("  - {}", item));
                    }
                }
            }
            None => {
                lines.push(
                    "No prior handoff — this is the first round (or the goal was resumed); \
                     rely on the workspace."
                        .to_string(),
                );
            }
        }

        lines.push(String::new());
        lines.push("## Latest condition check (authoritative ground truth)".to_string());
        match self.condition_history.last() {
            Some(last) => {
                let passed = last.results.iter().filter(|r| r.passed).count();
                let total = last.results.len();
                lines.push(format!("Round {}: {}/{} conditions passed", last.round, passed, total));
                for r in &last.results {
                    let icon = if r.passed { "PASS" } else { "FAIL" };
                    lines.push(format!("  [{}] {} — {}", icon, r.condition, r.detail));
                }
            }
            None => lines.push("No checks have run yet.".to_string()),
        }

        lines.join("\n")
    }

    /// Fold one LLM call's usage into the goal's cumulative total (cost
    /// axis). The total is derived as prompt + completion (providers report
    /// it inconsistently — some echo 0); providers that do not echo usage
    /// simply contribute nothing.
    fn accumulate_usage(&mut self, usage: &Option<crate::providers::Usage>) {
        if let Some(u) = usage {
            self.token_usage.prompt_tokens = self
                .token_usage
                .prompt_tokens
                .saturating_add(u.prompt_tokens);
            self.token_usage.completion_tokens = self
                .token_usage
                .completion_tokens
                .saturating_add(u.completion_tokens);
            self.token_usage.total_tokens = self
                .token_usage
                .prompt_tokens
                .saturating_add(self.token_usage.completion_tokens);
        }
    }

    /// The goal's cumulative usage, reported only when at least one provider
    /// echoed real numbers — mirrors the eval harness convention of not
    /// presenting an all-zero measurement as data.
    fn reported_usage(&self) -> Option<crate::agent::turns::TurnUsage> {
        (self.token_usage.total_tokens > 0).then_some(self.token_usage)
    }

    /// Save a checkpoint of the current goal state to the persistence store.
    ///
    /// Round-end semantics: `round_messages` is written as `None`, clearing
    /// any mid-round snapshot so an on-disk `Some` always means genuinely
    /// mid-round.
    async fn save_checkpoint(&self) {
        if let Some(ref store) = self.store {
            let store_guard = store.read().await;
            let state = persist::to_persisted(
                &self.id,
                &self.parent_session_id,
                &self.plan,
                self.round,
                &self.condition_history,
                None,
                self.last_handoff.as_ref(),
                self.reported_usage(),
                None,
            );
            if let Err(e) = store_guard.save(&state).await {
                tracing::warn!("[goal {}] Failed to save checkpoint: {}", self.id, e);
            }
        }
    }

    /// Emit an `Aborted` event for a policy stop and persist a terminal
    /// checkpoint carrying its blocking reason, so the cause survives and a
    /// human can resume deliberately.
    async fn abort_with_terminal(&self, event_reason: String, reason: crate::goal::BlockedReason) {
        self.emit(GoalEvent::Aborted {
            reason: event_reason,
            round: self.round,
            results: Vec::new(),
            blocked_reason: Some(reason.clone()),
            token_usage: self.reported_usage(),
        });
        self.save_terminal(reason).await;
    }

    /// Save the final checkpoint with its blocking reason, keeping the file
    /// so the cause survives and the goal can be resumed deliberately.
    ///
    /// Like [`Self::save_checkpoint`], writes `round_messages: None` — a
    /// blocked goal resumes by starting its round fresh.
    async fn save_terminal(&self, reason: crate::goal::BlockedReason) {
        if let Some(ref store) = self.store {
            let store_guard = store.read().await;
            let state = persist::to_persisted(
                &self.id,
                &self.parent_session_id,
                &self.plan,
                self.round,
                &self.condition_history,
                Some(reason),
                self.last_handoff.as_ref(),
                self.reported_usage(),
                None,
            );
            if let Err(e) = store_guard.save(&state).await {
                tracing::warn!("[goal {}] Failed to save terminal state: {}", self.id, e);
            }
        }
    }

    /// Snapshot the in-flight round's message history so a crash resumes
    /// mid-round instead of restarting the round. Best-effort: failures only
    /// warn and never affect the goal.
    async fn save_midround_checkpoint(&self, messages: &[Message]) {
        if let Some(ref store) = self.store {
            let store_guard = store.read().await;
            let state = persist::to_persisted(
                &self.id,
                &self.parent_session_id,
                &self.plan,
                self.round,
                &self.condition_history,
                None,
                self.last_handoff.as_ref(),
                self.reported_usage(),
                Some(messages.to_vec()),
            );
            if let Err(e) = store_guard.save(&state).await {
                tracing::warn!("[goal {}] Failed to save mid-round checkpoint: {}", self.id, e);
            }
        }
    }

    /// Delete the persisted state file for this goal on terminal state.
    async fn cleanup_store(&self) {
        if let Some(ref store) = self.store {
            let goal_id = self.id.clone();
            let store_owned = Arc::clone(store);
            tokio::spawn(async move {
                let store_guard = store_owned.read().await;
                store_guard.delete(&goal_id).await;
            });
        }
    }

    /// Build feedback string from previous round results.
    fn build_feedback(&self) -> String {
        if self.condition_history.is_empty() {
            let conditions: Vec<String> = self
                .plan
                .conditions
                .iter()
                .map(|c| format!("  [ ] {}", c))
                .collect();
            return format!(
                "目标: {}\n\n需要满足以下条件:\n{}",
                self.plan.description,
                conditions.join("\n")
            );
        }

        let last = match self.condition_history.last() {
            Some(last) => last,
            None => return "尚无执行记录".to_string(),
        };
        let passed = last.results.iter().filter(|r| r.passed).count();
        let total = last.results.len();

        let mut lines = vec![format!(
            "第 {} 轮结果: {}/{} 条件通过",
            last.round, passed, total
        )];

        for r in &last.results {
            let icon = if r.passed { "✓" } else { "✗" };
            lines.push(format!("  {} {} — {}", icon, r.condition, r.detail));
        }

        if passed < total {
            lines.push(String::new());
            lines.push("请修复失败的检查项。".to_string());
        }

        lines.join("\n")
    }

    /// Detect whether the runner is in a loop: same conditions failing with
    /// identical failure signatures for MAX_CONSECUTIVE_IDENTICAL_FAILURES
    /// rounds.
    fn detect_loop(&self) -> bool {
        if self.condition_history.len() < MAX_CONSECUTIVE_IDENTICAL_FAILURES {
            return false;
        }

        let recent = &self.condition_history
            [self.condition_history.len() - MAX_CONSECUTIVE_IDENTICAL_FAILURES..];

        // All recent rounds must have the same failing condition signatures.
        let baseline: Vec<String> = recent[0]
            .results
            .iter()
            .filter(|r| !r.passed)
            .map(|r| r.condition.failure_signature(&r.actual))
            .collect();

        if baseline.is_empty() {
            return false; // all passed, not a loop
        }

        recent.iter().all(|round| {
            let sigs: Vec<String> = round
                .results
                .iter()
                .filter(|r| !r.passed)
                .map(|r| r.condition.failure_signature(&r.actual))
                .collect();
            sigs == baseline
        })
    }

    fn emit(&self, event: GoalEvent) {
        if let Err(e) = self.event_tx.send(event) {
            tracing::warn!("[goal {}] Failed to emit event: {}", self.id, e);
        }
    }
}

/// Keep the head of a tool result up to `MAX_TOOL_RESULT_BYTES`, cutting on a
/// UTF-8 char boundary, and append a marker so the model knows it was cut.
fn truncate_tool_output(output: &str) -> String {
    if output.len() <= MAX_TOOL_RESULT_BYTES {
        return output.to_string();
    }
    // Find the largest char boundary at or before the byte cap. `String`
    // slicing (`&output[..cut]`) panics if `cut` is mid-char; UTF-8 is at
    // most 4 bytes per char, so the loop backs off at most 3 bytes.
    let mut cut = MAX_TOOL_RESULT_BYTES;
    while cut > 0 && !output.is_char_boundary(cut) {
        cut -= 1;
    }
    format!("{}…(truncated {} bytes)", &output[..cut], output.len() - cut)
}

/// Classify a completion error for the fresh-context loop.
///
/// Configuration problems (unknown model, no provider configured, missing
/// credentials) cannot be fixed by retrying the round, so they are re-raised
/// as fatal instead of being swallowed as transient agent failures.
fn classify_completion_error(err: crate::error::SyscityError) -> FreshRoundFailure {
    match &err {
        crate::error::SyscityError::Config(ConfigError::Missing(_))
        | crate::error::SyscityError::Config(ConfigError::InvalidValue { .. }) => {
            FreshRoundFailure::FatalConfig(err.to_string())
        }
        _ => FreshRoundFailure::Agent(err.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::goal::condition::Comparison;
    use crate::goal::condition::GoalCondition;
    use crate::goal::persist::GoalStore;
    use crate::model_router::ModelRouterConfig;
    use crate::providers::mock::MockProvider;

    fn make_router() -> Arc<ModelRouter> {
        Arc::new(ModelRouter::new(ModelRouterConfig::default()))
    }

    fn make_tools() -> Arc<ToolRegistry> {
        Arc::new(ToolRegistry::new())
    }

    fn make_runner(plan: GoalPlan) -> GoalRunner {
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        GoalRunner::new("test_goal", "test_session", plan, make_tools(), make_router(), tx)
    }

    fn make_plan(description: &str) -> GoalPlan {
        GoalPlan::new(description).with_condition(GoalCondition::ExitCode {
            command: "true".to_string(),
            expected: Some(0),
        })
    }

    /// Build a router whose `test-model` resolves to the given mock provider.
    async fn make_mock_router(mock: Arc<MockProvider>) -> Arc<ModelRouter> {
        let router = ModelRouter::new(ModelRouterConfig::default());
        router.add_provider_instance("mock", mock).await.unwrap();
        router
            .model_catalog
            .register(crate::model_router::model_catalog::ModelCatalogEntry::new(
                "test-model",
                "test-model",
                "mock",
            ))
            .await;
        Arc::new(router)
    }

    #[tokio::test]
    async fn test_token_usage_accumulates_into_events_and_checkpoint() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let store_dir = temp_dir("token_usage");
        let store = Arc::new(tokio::sync::RwLock::new(GoalStore::with_dir(store_dir.clone())));
        let router = make_mock_router(Arc::new(MockProvider::new())).await;

        let mut plan = GoalPlan::new("count tokens");
        plan.model_override = Some("test-model".to_string());
        plan.max_rounds = 1;
        plan = plan.with_condition(crate::goal::GoalCondition::ExitCode {
            command: "false".to_string(),
            expected: Some(0),
        });

        let runner = GoalRunner::new("g", "s", plan, make_tools(), router, tx).with_store(store);
        runner.run().await;

        let events = drain_events(&mut rx);
        let usage = match events.last().expect("at least one event") {
            GoalEvent::Aborted { token_usage, .. } => {
                token_usage.clone().expect("usage reported on abort")
            }
            other => panic!("expected Aborted, got {:?}", other),
        };
        assert!(usage.prompt_tokens > 0, "mock echoes prompt usage");
        assert_eq!(usage.total_tokens, usage.prompt_tokens + usage.completion_tokens);

        // MaxRounds is a policy stop: the terminal checkpoint survives and
        // must carry the same cumulative usage.
        let persisted = GoalStore::with_dir(store_dir.clone()).load_all().await;
        assert_eq!(persisted.len(), 1);
        assert_eq!(persisted[0].token_usage, Some(usage));

        let _ = tokio::fs::remove_dir_all(&store_dir).await;
    }

    /// A final assistant reply carrying a valid handoff block.
    fn handoff_reply(status: &str, summary: &str, steps: &[&str], evidence: &[&str]) -> String {
        let obj = serde_json::json!({
            "status": status,
            "summary": summary,
            "next_steps": steps,
            "evidence": evidence,
        });
        format!("Work done this round.\n\n```handoff\n{}\n```\n", obj)
    }

    fn tool_call_request(id: &str) -> Message {
        Message::assistant("calling tool").with_tool_calls(vec![crate::providers::ToolCall {
            id: id.to_string(),
            call_type: "function".to_string(),
            function: crate::providers::FunctionCall {
                name: "nonexistent_tool".to_string(),
                arguments: "{}".to_string(),
            },
            index: None,
            result: None,
        }])
    }

    #[tokio::test]
    async fn test_midround_checkpoint_snapshot_and_resume_continues_round() {
        use crate::providers::Role;

        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let store_dir = temp_dir("midround");
        let store = Arc::new(tokio::sync::RwLock::new(GoalStore::with_dir(store_dir.clone())));

        let mut plan = GoalPlan::new("resume mid-round");
        plan.model_override = Some("test-model".to_string());
        plan.max_rounds = 1;
        plan = plan.with_condition(GoalCondition::ExitCode {
            command: "false".to_string(),
            expected: Some(0),
        });

        // Iteration 0 requests a tool call (snapshot fires after the tool
        // results land); iteration 1 closes the round with plain text.
        let mock = Arc::new(
            MockProvider::new()
                .with_responses(vec![tool_call_request("call_1"), Message::assistant("done")]),
        );
        let router = make_mock_router(mock).await;
        let mut runner = GoalRunner::new("g", "s", plan.clone(), make_tools(), router, tx.clone())
            .with_store(store.clone())
            .with_progress(1, vec![]);

        // Round in flight: the mid-round checkpoint must carry the message
        // history ending with a complete assistant→tool group.
        runner.run_agent_round().await.unwrap();
        let persisted = store.read().await;
        let states = persisted.load_all().await;
        drop(persisted);
        assert_eq!(states.len(), 1);
        let snapshot = states[0]
            .round_messages
            .as_ref()
            .expect("mid-round snapshot present");
        assert_eq!(snapshot.len(), 4);
        assert_eq!(snapshot[0].role, Role::System);
        assert_eq!(snapshot[1].role, Role::User);
        assert_eq!(snapshot[2].role, Role::Assistant);
        assert_eq!(snapshot[3].role, Role::Tool);
        assert_eq!(snapshot[3].tool_call_id.as_deref(), Some("call_1"));
        assert!(states[0].token_usage.is_some(), "usage accumulates mid-round");

        // Resume: a fresh runner continues the SAME round from the restored
        // history — no re-added system+user prefix, and the round-end
        // checkpoint clears round_messages.
        let saved_messages = states[0].round_messages.clone();
        let mock2 =
            Arc::new(MockProvider::new().with_responses(vec![Message::assistant("resumed final")]));
        let router2 = make_mock_router(mock2.clone()).await;
        let runner2 = GoalRunner::new("g", "s", plan, make_tools(), router2, tx)
            .with_store(store.clone())
            .with_progress(states[0].round, vec![])
            .with_round_messages(saved_messages)
            .with_token_usage(states[0].token_usage);
        runner2.run().await;

        let history = mock2.history();
        assert_eq!(history.len(), 1, "one completion on the resumed round");
        let msgs = &history[0].messages;
        assert_eq!(msgs.len(), 4, "restored prefix sent verbatim");
        assert_eq!(msgs.iter().filter(|m| m.role == Role::User).count(), 1);
        assert_eq!(msgs[3].role, Role::Tool);

        // MaxRounds policy stop keeps the terminal checkpoint — cleared.
        let persisted = GoalStore::with_dir(store_dir.clone()).load_all().await;
        assert_eq!(persisted.len(), 1);
        assert!(persisted[0].round_messages.is_none(), "round-end clears");
        assert!(persisted[0].blocked_reason.is_some());

        let _ = tokio::fs::remove_dir_all(&store_dir).await;
    }

    #[tokio::test]
    async fn test_fresh_mode_midround_snapshot_and_resume_keeps_handoff_contract() {
        use crate::providers::Role;

        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let store_dir = temp_dir("midround_fresh");
        let store = Arc::new(tokio::sync::RwLock::new(GoalStore::with_dir(store_dir.clone())));

        let mut plan = GoalPlan::new("fresh mid-round");
        plan.model_override = Some("test-model".to_string());
        plan.max_rounds = 1;
        plan.fresh_context = true;
        plan = plan.with_condition(GoalCondition::ExitCode {
            command: "false".to_string(),
            expected: Some(0),
        });

        let mock = Arc::new(MockProvider::new().with_responses(vec![
            tool_call_request("call_1"),
            Message::assistant(handoff_reply("continue", "partial work", &["keep going"], &[])),
        ]));
        let router = make_mock_router(mock).await;
        let mut runner = GoalRunner::new("g", "s", plan.clone(), make_tools(), router, tx.clone())
            .with_store(store.clone())
            .with_progress(1, vec![]);

        let handoff = runner.run_fresh_round().await.unwrap().expect("handoff");
        assert_eq!(handoff.status, crate::goal::handoff::HandoffStatus::Continue);

        let states = store.read().await.load_all().await;
        assert_eq!(states.len(), 1);
        let snapshot = states[0]
            .round_messages
            .as_ref()
            .expect("mid-round snapshot");
        assert_eq!(snapshot.len(), 4);
        assert!(snapshot[0].content.contains("ONE round"), "fresh system prompt persisted");

        // Resume mid-round; the round still ends with a valid handoff.
        let mock2 = Arc::new(MockProvider::new().with_responses(vec![Message::assistant(
            handoff_reply("continue", "resumed work", &["next"], &[]),
        )]));
        let router2 = make_mock_router(mock2.clone()).await;
        let mut runner2 = GoalRunner::new("g", "s", plan, make_tools(), router2, tx)
            .with_store(store)
            .with_progress(states[0].round, vec![])
            .with_round_messages(states[0].round_messages.clone());
        let resumed_handoff = runner2.run_fresh_round().await.unwrap().expect("handoff");
        assert_eq!(resumed_handoff.summary, "resumed work");

        let msgs = &mock2.history()[0].messages;
        assert_eq!(msgs.len(), 4);
        assert_eq!(msgs.iter().filter(|m| m.role == Role::User).count(), 1);

        let _ = tokio::fs::remove_dir_all(&store_dir).await;
    }

    fn temp_dir(tag: &str) -> std::path::PathBuf {
        let dir =
            std::env::temp_dir().join(format!("goal_runner_{}_{}", tag, uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).expect("create temp dir");
        dir
    }

    fn drain_events(rx: &mut tokio::sync::mpsc::UnboundedReceiver<GoalEvent>) -> Vec<GoalEvent> {
        let mut events = Vec::new();
        while let Ok(event) = rx.try_recv() {
            events.push(event);
        }
        events
    }

    #[test]
    fn test_with_progress_sets_round_and_history() {
        let plan = make_plan("test");
        let runner = make_runner(plan).with_progress(
            3,
            vec![RoundResult {
                round: 1,
                results: vec![CheckResult {
                    condition: GoalCondition::ExitCode {
                        command: "true".to_string(),
                        expected: Some(0),
                    },
                    passed: true,
                    actual: "exit code: 0".to_string(),
                    detail: "passed".to_string(),
                }],
            }],
        );
        assert_eq!(runner.round, 3);
        assert_eq!(runner.condition_history.len(), 1);
        assert_eq!(runner.condition_history[0].round, 1);
    }

    #[test]
    fn test_build_feedback_empty_history() {
        let plan = make_plan("write tests");
        let runner = make_runner(plan);
        let feedback = runner.build_feedback();
        assert!(feedback.contains("write tests"));
        assert!(feedback.contains("[ ]"));
    }

    #[test]
    fn test_build_feedback_with_results() {
        let plan = make_plan("run tests");
        let mut runner = make_runner(plan);
        runner.condition_history.push(RoundResult {
            round: 1,
            results: vec![
                CheckResult {
                    condition: GoalCondition::ExitCode {
                        command: "true".to_string(),
                        expected: Some(0),
                    },
                    passed: true,
                    actual: "exit code: 0".to_string(),
                    detail: "passed".to_string(),
                },
                CheckResult {
                    condition: GoalCondition::Numeric {
                        command: "echo 2".to_string(),
                        operator: Comparison::Ge,
                        threshold: 5.0,
                    },
                    passed: false,
                    actual: "2".to_string(),
                    detail: "got 2, expected >= 5".to_string(),
                },
            ],
        });
        let feedback = runner.build_feedback();
        assert!(feedback.contains("1/2"));
        assert!(feedback.contains("✓"));
        assert!(feedback.contains("✗"));
        assert!(feedback.contains("请修复"));
    }

    #[test]
    fn test_detect_loop_not_enough_rounds() {
        let plan = make_plan("test");
        let runner = make_runner(plan);
        // 0 entries
        assert!(!runner.detect_loop());
    }

    #[test]
    fn test_detect_loop_two_rounds_not_enough() {
        let plan = make_plan("test");
        let mut runner = make_runner(plan);
        let fail_result = CheckResult {
            condition: GoalCondition::ExitCode {
                command: "false".to_string(),
                expected: Some(0),
            },
            passed: false,
            actual: "exit code: 1".to_string(),
            detail: "failed".to_string(),
        };
        runner.condition_history.push(RoundResult {
            round: 1,
            results: vec![fail_result.clone()],
        });
        runner.condition_history.push(RoundResult {
            round: 2,
            results: vec![fail_result],
        });
        assert!(!runner.detect_loop());
    }

    #[test]
    fn test_detect_loop_three_identical_failures() {
        let plan = make_plan("test");
        let mut runner = make_runner(plan);
        let fail_result = CheckResult {
            condition: GoalCondition::ExitCode {
                command: "false".to_string(),
                expected: Some(0),
            },
            passed: false,
            actual: "exit code: 1".to_string(),
            detail: "failed".to_string(),
        };
        for round in 1..=3 {
            runner.condition_history.push(RoundResult {
                round,
                results: vec![fail_result.clone()],
            });
        }
        assert!(runner.detect_loop());
    }

    #[test]
    fn test_detect_loop_different_failures() {
        let plan = make_plan("test");
        let mut runner = make_runner(plan);
        for round in 1..=3 {
            let actual = format!("exit code: {}", round);
            runner.condition_history.push(RoundResult {
                round,
                results: vec![CheckResult {
                    condition: GoalCondition::ExitCode {
                        command: "false".to_string(),
                        expected: Some(0),
                    },
                    passed: false,
                    actual,
                    detail: "failed".to_string(),
                }],
            });
        }
        assert!(!runner.detect_loop());
    }

    #[test]
    fn test_detect_loop_all_passed_not_loop() {
        let plan = make_plan("test");
        let mut runner = make_runner(plan);
        let pass_result = CheckResult {
            condition: GoalCondition::ExitCode {
                command: "true".to_string(),
                expected: Some(0),
            },
            passed: true,
            actual: "exit code: 0".to_string(),
            detail: "passed".to_string(),
        };
        for round in 1..=3 {
            runner.condition_history.push(RoundResult {
                round,
                results: vec![pass_result.clone()],
            });
        }
        assert!(!runner.detect_loop());
    }

    #[test]
    fn test_truncate_tool_output_under_cap_untouched() {
        assert_eq!(truncate_tool_output("hello"), "hello");
    }

    #[test]
    fn test_truncate_tool_output_cuts_on_char_boundary() {
        // "中" is 3 bytes in UTF-8. 2731 chars = 8193 bytes, just over the
        // 8192-byte cap; the cap lands mid-char (8192 % 3 = 2), so the cut
        // must back off to 8190 and stay valid UTF-8.
        let s = "中".repeat(2731); // 8193 bytes
        let t = truncate_tool_output(&s);
        assert!(t.starts_with('中'));
        assert!(t.contains("truncated"));
        assert!(t.ends_with("bytes)"));
        assert!(t.is_char_boundary(t.len()));
    }

    // ── Fresh-context ("Ralph") mode ─────────────────────────────────────

    #[test]
    fn test_fresh_system_prompt_contains_contract() {
        let runner = make_runner(make_plan("write report").with_fresh_context(true));
        let prompt = runner.build_fresh_system_prompt();
        assert!(prompt.contains("write report"));
        assert!(prompt.contains("```handoff"));
        assert!(prompt.contains("\"next_steps\""));
        assert!(prompt.contains("workspace"));
    }

    #[test]
    fn test_build_fresh_feedback_first_round_has_no_handoff() {
        let runner = make_runner(make_plan("write report").with_fresh_context(true))
            .with_progress(1, Vec::new());
        let feedback = runner.build_fresh_feedback();
        assert!(feedback.contains("Round 1 of"));
        assert!(feedback.contains("No prior handoff"));
    }

    #[test]
    fn test_build_fresh_feedback_embeds_handoff_and_conditions() {
        let mut runner = make_runner(make_plan("write report").with_fresh_context(true));
        runner.last_handoff = Some(RoundHandoff {
            status: HandoffStatus::Continue,
            summary: "wrote chapter 1".to_string(),
            next_steps: vec!["write chapter 2".to_string()],
            evidence: vec![],
        });
        runner.condition_history.push(RoundResult {
            round: 1,
            results: vec![CheckResult {
                condition: GoalCondition::ExitCode {
                    command: "false".to_string(),
                    expected: Some(0),
                },
                passed: false,
                actual: "exit code: 1".to_string(),
                detail: "failed".to_string(),
            }],
        });
        let feedback = runner.build_fresh_feedback();
        assert!(feedback.contains("wrote chapter 1"));
        assert!(feedback.contains("write chapter 2"));
        assert!(feedback.contains("0/1"));
        assert!(feedback.contains("FAIL"));
    }

    #[test]
    fn test_classify_completion_error_config_is_fatal_others_are_agent() {
        let config_err =
            crate::error::SyscityError::Config(ConfigError::Missing("api_key".to_string()));
        assert!(matches!(
            classify_completion_error(config_err),
            FreshRoundFailure::FatalConfig(_)
        ));
        let other = crate::error::SyscityError::Internal("boom".to_string());
        assert!(matches!(classify_completion_error(other), FreshRoundFailure::Agent(_)));
    }

    /// Two fresh rounds: round 1 fails the condition and hands off `continue`
    /// with next steps; round 2 passes and hands off `complete` with evidence.
    #[tokio::test]
    async fn test_fresh_context_two_round_progression() {
        let mock = Arc::new(MockProvider::new().with_callback(|messages| {
            let round_one = messages.iter().any(|m| m.content.contains("## Round 1 of"));
            if round_one {
                Message::assistant(handoff_reply(
                    "continue",
                    "prepared workspace",
                    &["finish the job"],
                    &[],
                ))
            } else {
                Message::assistant(handoff_reply(
                    "complete",
                    "all done",
                    &[],
                    &["marker file now exists"],
                ))
            }
        }));
        let router = make_mock_router(mock.clone()).await;

        // Condition that fails on the first check, then flips to passing
        // (marker file created by the first evaluation).
        let dir = temp_dir("progression");
        let marker = dir.join("marker").display().to_string();
        let plan = GoalPlan::new("flip the marker")
            .with_condition(GoalCondition::ExitCode {
                command: format!(
                    "if [ -f '{m}' ]; then exit 0; else touch '{m}'; exit 1; fi",
                    m = marker
                ),
                expected: Some(0),
            })
            .with_max_rounds(4)
            .with_model("test-model")
            .with_fresh_context(true);

        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let runner = GoalRunner::new("g", "s", plan, make_tools(), router, tx);
        runner.run().await;

        let events = drain_events(&mut rx);
        assert_eq!(events.len(), 6, "events: {:?}", events);
        assert!(matches!(events[0], GoalEvent::Started { .. }));
        assert!(matches!(&events[1], GoalEvent::Retry { round: 1, .. }));
        match &events[2] {
            GoalEvent::Check { round, passed, total, .. } => {
                assert_eq!(*round, 1);
                assert_eq!((*passed, *total), (0, 1));
            }
            other => panic!("expected Check for round 1, got {:?}", other),
        }
        assert!(matches!(&events[3], GoalEvent::Retry { round: 2, .. }));
        match &events[4] {
            GoalEvent::Check { round, passed, total, .. } => {
                assert_eq!(*round, 2);
                assert_eq!((*passed, *total), (1, 1));
            }
            other => panic!("expected Check for round 2, got {:?}", other),
        }
        match &events[5] {
            GoalEvent::Done { total_rounds, all_passed, .. } => {
                assert_eq!(*total_rounds, 2);
                assert!(*all_passed);
            }
            other => panic!("expected Done, got {:?}", other),
        }

        // Exactly one LLM completion per round.
        assert_eq!(mock.call_count(), 2);

        // Fresh-context property: every round's request is exactly two
        // messages (system + user) — nothing accumulates — and round 2's user
        // message carries round 1's handoff next step as its only memory.
        let history = mock.history();
        assert_eq!(history.len(), 2);
        for request in &history {
            assert_eq!(request.messages.len(), 2);
        }
        assert!(history[1].messages[1].content.contains("finish the job"));

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A missing model/provider configuration aborts loudly with a dedicated
    /// blocked reason instead of being swallowed into the loop, and keeps a
    /// terminal checkpoint so the operator can resume after fixing config.
    #[tokio::test]
    async fn test_fresh_context_fatal_config_aborts_loudly() {
        // Router with no providers and no catalog entries at all.
        let router = Arc::new(ModelRouter::new(ModelRouterConfig::default()));

        let plan = make_plan("doomed goal")
            .with_max_rounds(3)
            .with_model("ghost-model")
            .with_fresh_context(true);

        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let store_dir = temp_dir("fatal_config");
        let store = Arc::new(tokio::sync::RwLock::new(GoalStore::with_dir(store_dir.clone())));
        let runner = GoalRunner::new("g", "s", plan, make_tools(), router, tx).with_store(store);
        runner.run().await;

        let events = drain_events(&mut rx);
        assert_eq!(events.len(), 3, "events: {:?}", events);
        match &events[2] {
            GoalEvent::Aborted {
                reason,
                blocked_reason: Some(reason_struct),
                ..
            } => {
                assert!(reason.starts_with("fatal_config_error"), "{}", reason);
                assert_eq!(reason_struct.code, crate::goal::BlockedReasonCode::FatalConfigError);
            }
            other => panic!("expected Aborted, got {:?}", other),
        }

        // Terminal checkpoint survives with the fatal-config reason.
        let persisted = GoalStore::with_dir(store_dir.clone()).load_all().await;
        assert_eq!(persisted.len(), 1);
        assert_eq!(
            persisted[0].blocked_reason.as_ref().unwrap().code,
            crate::goal::BlockedReasonCode::FatalConfigError
        );

        let _ = std::fs::remove_dir_all(&store_dir);
    }

    /// A final reply without a valid handoff block fails the round outright
    /// (strict validation — never guessed or truncated).
    #[tokio::test]
    async fn test_fresh_context_invalid_handoff_rejects_round() {
        let mock = Arc::new(
            MockProvider::new()
                .with_responses(vec![Message::assistant("I did some work, trust me.")]),
        );
        let router = make_mock_router(mock).await;

        let plan = GoalPlan::new("needs handoff")
            .with_condition(GoalCondition::ExitCode {
                command: "false".to_string(),
                expected: Some(0),
            })
            .with_max_rounds(3)
            .with_model("test-model")
            .with_fresh_context(true);

        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let store_dir = temp_dir("invalid_handoff");
        let store = Arc::new(tokio::sync::RwLock::new(GoalStore::with_dir(store_dir.clone())));
        let runner = GoalRunner::new("g", "s", plan, make_tools(), router, tx).with_store(store);
        runner.run().await;

        let events = drain_events(&mut rx);
        match events.last().expect("at least one event") {
            GoalEvent::Aborted {
                reason,
                blocked_reason: Some(structured),
                ..
            } => {
                assert!(reason.starts_with("invalid_handoff"), "{}", reason);
                assert_eq!(structured.code, crate::goal::BlockedReasonCode::InvalidHandoff);
                assert!(structured.message.contains("no valid ```handoff"));
            }
            other => panic!("expected Aborted, got {:?}", other),
        }

        let persisted = GoalStore::with_dir(store_dir.clone()).load_all().await;
        assert_eq!(persisted.len(), 1);
        assert_eq!(
            persisted[0].blocked_reason.as_ref().unwrap().code,
            crate::goal::BlockedReasonCode::InvalidHandoff
        );

        let _ = std::fs::remove_dir_all(&store_dir);
    }

    /// An agent-declared `failed` handoff stops the loop instead of burning
    /// remaining rounds, keeping the failure summary in the checkpoint.
    #[tokio::test]
    async fn test_fresh_context_failed_status_stops_loop() {
        let mock = Arc::new(MockProvider::new().with_responses(vec![Message::assistant(
            handoff_reply("failed", "toolchain is broken", &[], &[]),
        )]));
        let router = make_mock_router(mock).await;

        let plan = GoalPlan::new("hopeless")
            .with_condition(GoalCondition::ExitCode {
                command: "false".to_string(),
                expected: Some(0),
            })
            .with_max_rounds(5)
            .with_model("test-model")
            .with_fresh_context(true);

        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let store_dir = temp_dir("failed_status");
        let store = Arc::new(tokio::sync::RwLock::new(GoalStore::with_dir(store_dir.clone())));
        let runner = GoalRunner::new("g", "s", plan, make_tools(), router, tx).with_store(store);
        runner.run().await;

        let events = drain_events(&mut rx);
        match events.last().expect("at least one event") {
            GoalEvent::Aborted {
                blocked_reason: Some(structured),
                ..
            } => {
                assert_eq!(structured.code, crate::goal::BlockedReasonCode::AgentError);
                assert!(structured.message.contains("agent reported failure"));
                assert!(structured.message.contains("toolchain is broken"));
            }
            other => panic!("expected Aborted, got {:?}", other),
        }
        // Stopped after round 1 despite a budget of 5.
        assert!(matches!(
            &events[2],
            GoalEvent::Check {
                round: 1,
                passed: 0,
                total: 1,
                ..
            }
        ));

        let persisted = GoalStore::with_dir(store_dir.clone()).load_all().await;
        assert_eq!(persisted.len(), 1);
        assert_eq!(persisted[0].last_handoff.as_ref().unwrap().status, HandoffStatus::Failed);

        let _ = std::fs::remove_dir_all(&store_dir);
    }

    /// Resuming a persisted fresh-context goal restores the carried handoff.
    #[tokio::test]
    async fn test_with_handoff_restores_carried_state() {
        let plan = make_plan("resumed").with_fresh_context(true);
        let handoff = RoundHandoff {
            status: HandoffStatus::Continue,
            summary: "prior progress".to_string(),
            next_steps: vec!["next action".to_string()],
            evidence: vec![],
        };
        let runner = make_runner(plan)
            .with_progress(2, Vec::new())
            .with_handoff(Some(handoff));
        let feedback = runner.build_fresh_feedback();
        assert!(feedback.contains("prior progress"));
        assert!(feedback.contains("next action"));
        assert!(runner.last_handoff.is_some());
    }

    /// A round note renders the validated handoff as browsable markdown.
    #[tokio::test]
    async fn test_write_round_note_renders_handoff() {
        let tmp = tempfile::tempdir().unwrap();
        let handoff = RoundHandoff {
            status: HandoffStatus::Continue,
            summary: "found the login bug".to_string(),
            next_steps: vec!["add regression test".to_string()],
            evidence: vec![],
        };
        let path = write_round_note(tmp.path(), "g1", "Fix login", 3, &handoff)
            .await
            .unwrap();
        assert_eq!(path, tmp.path().join("goals").join("g1").join("round-3.md"));
        let content = std::fs::read_to_string(&path).unwrap();
        assert!(content.contains("# Round 3 — Fix login"));
        assert!(content.contains("found the login bug"));
        assert!(content.contains("add regression test"));
        assert!(!content.contains("## Evidence"), "empty sections are omitted");
    }

    /// Completion handoffs include their evidence list.
    #[tokio::test]
    async fn test_write_round_note_includes_evidence() {
        let tmp = tempfile::tempdir().unwrap();
        let handoff = RoundHandoff {
            status: HandoffStatus::Complete,
            summary: "done".to_string(),
            next_steps: vec![],
            evidence: vec!["src/auth.rs:42".to_string()],
        };
        let path = write_round_note(tmp.path(), "g2", "Ship it", 1, &handoff)
            .await
            .unwrap();
        let content = std::fs::read_to_string(&path).unwrap();
        assert!(content.contains("## Evidence"));
        assert!(content.contains("src/auth.rs:42"));
        assert!(!content.contains("## Next steps"));
    }
}
