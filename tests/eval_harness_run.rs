//! Eval harness execution.
//!
//! `tests/run_all.rs` and `tests/eval_suites_validate.rs` load YAML suites and
//! assert the *assets* are well-formed — neither ever calls
//! `EvalHarness::run`, so the scoring path (trial loop → condition check →
//! pass rate → Wilson interval) had no end-to-end coverage at all.
//!
//! Design note that shapes these tests: a `GoalCondition` is checked against
//! the **environment**, not against the model's prose — `Pattern{command,..}`
//! runs a shell command and inspects its output, `FileExists{path}` looks at
//! the filesystem. So a meaningful eval trial is "the agent acts, the
//! condition verifies the effect". A condition whose *command itself* contains
//! the needle passes vacuously; the trial below therefore has the agent write
//! a file with the shell tool and scores the file's existence.

mod common;

use std::sync::Arc;

use syscity::agent::Agent;
use syscity::agent::AgentConfig;
use syscity::eval::{EvalHarness, EvalTask};
use syscity::goal::GoalCondition;
use syscity::providers::mock::MockProvider;
use syscity::providers::{FunctionCall, Message as ProviderMessage, Role, ToolCall};
use syscity::tools::shell::ShellTool;
use syscity::tools::ToolRegistry;

/// Register the shell tool so the agent can act on the environment.
///
/// The permissions runtime carries an `allow` rule for `shell`: eval trials
/// run unattended, and an approval-gated tool in a context with nobody to ask
/// fails closed (`Tool 'shell' requires approval and no human is present`).
/// A real eval suite that drives such tools needs the same pre-approval — this
/// is the documented migration path, exercised here.
fn registry_with_shell() -> Arc<ToolRegistry> {
    use syscity::tools::permissions::PermissionsRuntime;
    use syscity::tools::PermissionsConfig;

    let mut registry = ToolRegistry::new().with_permissions(Arc::new(
        PermissionsRuntime::from_config(&PermissionsConfig {
            allow: vec!["shell".to_string()],
            ..Default::default()
        }),
    ));
    registry.register(Box::new(ShellTool::new()));
    Arc::new(registry)
}

/// A provider that asks for `shell: <command>` on the first turn and then
/// answers, modelling an agent that does the work the condition checks.
fn provider_running(command: &str) -> MockProvider {
    let command = command.to_string();
    MockProvider::new().with_callback(move |messages| {
        if messages.iter().any(|m| m.role == Role::Tool) {
            return ProviderMessage::assistant("done");
        }
        ProviderMessage::assistant("running the command").with_tool_calls(vec![ToolCall {
            id: "call_1".to_string(),
            call_type: "function".to_string(),
            function: FunctionCall {
                name: "shell".to_string(),
                arguments: serde_json::json!({ "command": command }).to_string(),
            },
            index: None,
            result: None,
        }])
    })
}

#[tokio::test]
async fn harness_runs_every_trial_and_reports_a_bracketing_interval() {
    common::install_test_root();

    let dir = std::env::temp_dir().join("syscity_eval_harness_pass");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("trial dir");
    let marker = dir.join("work-done.txt");

    let provider = Arc::new(provider_running(&format!("echo ok > '{}'", marker.display())));
    let agent = Arc::new(Agent::new(AgentConfig::default(), provider, registry_with_shell()));

    // The condition verifies the effect the agent was asked to produce.
    let task = EvalTask {
        id: "harness-passes".to_string(),
        input: "create the work marker".to_string(),
        conditions: vec![GoalCondition::FileExists {
            path: marker.to_string_lossy().to_string(),
        }],
        ..Default::default()
    };

    // `None` critic: scoring stays offline and deterministic (a real Critic is
    // a live LLM whose mock replies would have to be critique JSON).
    let summary = EvalHarness::new(agent, None)
        .run(task, 3)
        .await
        .expect("harness run");

    assert_eq!(summary.total_trials, 3, "every requested trial runs");
    assert_eq!(summary.per_trial.len(), 3, "each trial is reported");
    assert_eq!(
        summary.pass_rate, 1.0,
        "the agent produced the file the condition checks: {summary:?}"
    );
    let (lo, hi) = summary.confidence_interval;
    assert!(
        lo <= summary.pass_rate && summary.pass_rate <= hi,
        "the Wilson interval brackets the observed rate: {lo} <= {} <= {hi}",
        summary.pass_rate
    );
    assert!(lo >= 0.0 && hi <= 1.0, "intervals stay in [0,1]: {lo}..{hi}");
}

#[tokio::test]
async fn a_condition_the_environment_never_satisfies_scores_zero() {
    common::install_test_root();

    // The agent runs a command that does *not* create the file the condition
    // watches — so every trial must score as a failure. This is the check that
    // a broken scorer cannot silently report success.
    let provider = Arc::new(provider_running("true"));
    let agent = Arc::new(Agent::new(AgentConfig::default(), provider, registry_with_shell()));

    let never = std::env::temp_dir().join("syscity_eval_harness_never_created.marker");
    let _ = std::fs::remove_file(&never);

    let task = EvalTask {
        id: "harness-fails".to_string(),
        input: "create the marker".to_string(),
        conditions: vec![GoalCondition::FileExists {
            path: never.to_string_lossy().to_string(),
        }],
        ..Default::default()
    };

    let summary = EvalHarness::new(agent, None)
        .run(task, 3)
        .await
        .expect("harness run");

    assert_eq!(summary.total_trials, 3);
    assert_eq!(summary.pass_rate, 0.0, "an unsatisfied condition must score zero: {summary:?}");
    assert!(!summary.at_least_once_success, "no trial satisfied the condition");
}
