//! The `computer_run` tool: hand a whole desktop goal to a run.
//!
//! `computer` is one action at a time — the model looks, decides, clicks, and
//! comes back for another round. That is the right shape for a step it is
//! reasoning about, and the wrong shape for "open the invoice in Numbers and
//! put the total in a note": twenty round trips through the conversation, with
//! the model's own context paying for a screen it is not looking at.
//!
//! This tool is the other shape. It starts the run the gateway already has
//! (`computer.run`), hands the goal to a driver, and returns as soon as the run
//! is moving. The turn gets a `run_id` and a one-line answer; the run's steps,
//! cost and screenshots live in its checkpoint, and [`crate::gateway::ws`]'s
//! `turns.replay` folds them into the turn that started it.
//!
//! Deliberately **not** a long-blocking tool. A run is bounded by
//! `[computer] max_steps` and a per-run budget, which can be minutes; a tool
//! call holds the turn open for that whole time, and the turn's own timeout
//! (`ToolContext::timeout`) is shorter than a run. Returning the `run_id` is
//! what lets the model keep working and lets a person watch.
//!
//! Why a tool and not only the WS method: `computer.run` is reachable from a
//! client that holds a connection and knows it wants a run. The agent that is
//! already looking at a screen has no such client — it has a tool registry —
//! and without this it can only drive the screen one action at a time.

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{json, Value};

use crate::gateway::state::GatewayState;
use crate::gateway::ws::inproc::start_run;
use crate::tools::{
    approval::RiskLevel, create_schema, sdk::ToolCapabilities, Tool, ToolContext,
    ToolExecutionResult,
};

/// Start a computer run from inside a turn.
///
/// Holds the gateway's state, like [`crate::tools::GatewayTool`]: the run lives
/// in the gateway's registry and is answered by its WS methods, so a tool with
/// its own copy of the world would start a run nobody else can see.
pub struct ComputerRunTool {
    state: Arc<GatewayState>,
}

impl ComputerRunTool {
    pub fn new(state: Arc<GatewayState>) -> Self {
        Self { state }
    }
}

#[async_trait]
impl Tool for ComputerRunTool {
    fn name(&self) -> &str {
        "computer_run"
    }

    fn description(&self) -> &str {
        r#"Hand a whole desktop goal to an autonomous run and get its id back.

Use this when the goal is a sequence the run can carry out on its own — "open
the invoice in Numbers and put the total in a note", "set the editor to dark
mode", "find the newest export and move it to the inbox". The run decides and
acts step by step without returning to this conversation, so it costs one call
instead of one per action.

Returns immediately with `run_id`; it does NOT wait for the goal to be met.
Report progress or read the outcome with `computer.status` (`{run_id}`), and
stop it with `computer.control`.

Use `computer` instead when the next action depends on something only this
conversation knows, or when the goal is a single action. The run cannot ask a
question mid-way: it stops with `need_help` and leaves a handoff note.

A run is bound by `[computer] max_steps` and a per-run cost budget; it stops
with a `stop_reason` rather than running forever."#
    }

    fn parameters_schema(&self) -> Value {
        create_schema(
            "Start an autonomous desktop run",
            json!({
                "goal": {
                    "type": "string",
                    "description": "The goal in plain words, e.g. 'open the invoice in Numbers and read out the total'. The run plans the steps; it does not follow a script."
                }
            }),
            vec!["goal"],
        )
    }

    fn capabilities(&self) -> ToolCapabilities {
        ToolCapabilities {
            requires_approval: true,
            // A run acts on a live screen for many steps. The approval is for
            // the whole sequence: asking per step would be the `computer`
            // tool's shape, which is the thing this replaces.
            risk_level: RiskLevel::High,
            categories: vec!["computer".to_string(), "desktop".to_string()],
            // Starting a second run is a new run, not a retry of a stale one:
            // repeating the call must not be collapsed by the "same tool, same
            // args" guard into a single run.
            deduplicate_identical_calls: false,
            ..Default::default()
        }
    }

    fn is_available(&self, _context: &ToolContext) -> bool {
        true
    }

    async fn execute(
        &self,
        args: Value,
        context: &ToolContext,
    ) -> crate::Result<ToolExecutionResult> {
        let goal = args["goal"]
            .as_str()
            .map(str::trim)
            .filter(|g| !g.is_empty())
            .ok_or_else(|| {
                crate::error::SyscityError::Validation(
                    "Missing 'goal' argument for computer_run".to_string(),
                )
            })?
            .to_string();

        let config = self.state.config.read().await.clone();
        if !config.computer.enabled {
            return Ok(refusal(
                "COMPUTER_DISABLED",
                "Computer use is disabled (`[computer] enabled = false`)",
            ));
        }

        // The turn this call belongs to, so the run's checkpoint can be found
        // from it later. `ToolContext::trace` is the only place a tool is told;
        // outside an agent turn (a test, a one-off call) there is no turn, and
        // the run is reachable through `computer.runs` alone.
        let turn_id = context
            .trace
            .as_ref()
            .map(|t| t.turn_id().to_string())
            .filter(|t| !t.is_empty());

        // The conversation is where a run's events are addressed, and where its
        // inherited desktop approval is scoped. An empty one would put the
        // events on nobody's stream.
        let conversation =
            (!context.conversation_id.is_empty()).then(|| context.conversation_id.clone());

        let started =
            match start_run(&self.state, &config, goal.clone(), conversation, turn_id).await {
                Ok(started) => started,
                Err(resp) => {
                    let error = resp
                        .error
                        .map(|e| e.message)
                        .unwrap_or_else(|| "the run was refused".to_string());
                    return Ok(refusal("RUN_REFUSED", &error));
                }
            };

        let run_id = started.run_id.clone();
        Ok(ToolExecutionResult {
            success: true,
            output: format!(
                "Started computer run {run_id} for: {goal}\n\
                 It is stepping now ({model}, up to {max_steps} steps). \
                 This call does not wait: use computer.status {{run_id}} to read \
                 progress or the outcome, and computer.control to stop it.",
                model = started.started.model,
                max_steps = started.started.max_steps,
            ),
            error: None,
            data: Some(json!({
                "run_id": run_id,
                "goal": goal,
                "model": started.started.model,
                "model_supports_vision": started.started.model_supports_vision,
                "max_steps": started.started.max_steps,
            })),
            execution_time: std::time::Duration::ZERO,
        })
    }
}

/// A refusal as the model should read it.
///
/// `success: false` rather than an `Err`: none of these is a bug in the call
/// itself — a disabled host, a missing adapter, no model — and the model can
/// act on each of them (say why, or try the step-by-step tool). An `Err` here
/// would be reported as the tool having broken.
fn refusal(code: &str, message: &str) -> ToolExecutionResult {
    ToolExecutionResult {
        success: false,
        output: format!("{message} ({code})"),
        error: Some(format!("{message} ({code})")),
        data: Some(json!({ "code": code })),
        execution_time: std::time::Duration::ZERO,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gateway::state_tests::make_test_state;
    use crate::gateway::GatewayConfig;

    async fn state_with(enabled: bool) -> Arc<GatewayState> {
        let mut config = GatewayConfig::default();
        config.computer.enabled = enabled;
        Arc::new(make_test_state(config).await)
    }

    /// A goal is required, and an empty one must not start a run that can
    /// never decide anything.
    #[tokio::test]
    async fn an_empty_goal_is_refused_before_a_run_exists() {
        let state = state_with(true).await;
        let tool = ComputerRunTool::new(state.clone());
        let err = tool
            .execute(json!({ "goal": "   " }), &ToolContext::default())
            .await
            .expect_err("an empty goal is a caller mistake");
        assert!(err.to_string().contains("goal"), "{err}");
        assert_eq!(state.tools.computer_runs.live_count().await, 0);
    }

    /// Disabled computer use answers as a refusal the model can read, not as
    /// an error, and starts nothing.
    #[tokio::test]
    async fn disabled_computer_use_is_a_refusal_that_starts_nothing() {
        let state = state_with(false).await;
        let tool = ComputerRunTool::new(state.clone());
        let result = tool
            .execute(json!({ "goal": "open the calculator" }), &ToolContext::default())
            .await
            .expect("a refusal is still a tool result");
        assert!(!result.success);
        assert_eq!(result.data.unwrap()["code"], "COMPUTER_DISABLED");
        assert_eq!(state.tools.computer_runs.live_count().await, 0);
    }

    /// With computer use on but no adapter on this host, the run is refused
    /// rather than started and immediately stuck — and nothing is left in the
    /// registry for `computer.status` to find.
    #[tokio::test]
    async fn no_adapter_is_a_refusal_and_leaves_no_run_behind() {
        let state = state_with(true).await;
        let tool = ComputerRunTool::new(state.clone());
        let result = tool
            .execute(json!({ "goal": "open the calculator" }), &ToolContext::default())
            .await
            .expect("a refusal is still a tool result");
        assert!(!result.success);
        assert_eq!(result.data.unwrap()["code"], "RUN_REFUSED");
        assert_eq!(state.tools.computer_runs.live_count().await, 0);
    }

    /// A tool that runs outside a turn has no trace and therefore no turn id;
    /// that must be `None`, not an empty string written into the checkpoint.
    #[test]
    fn a_context_without_a_trace_names_no_turn() {
        let ctx = ToolContext::default();
        let turn = ctx
            .trace
            .as_ref()
            .map(|t| t.turn_id().to_string())
            .filter(|t| !t.is_empty());
        assert_eq!(turn, None);
    }
}
