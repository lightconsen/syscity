//! One benchmark task: build an agent over a remote desktop, let it attempt
//! the instruction, and write the result the Python glue reads back.
//!
//! The output contract is [`BenchResult`] (JSON, `schema: 1`). It is
//! deliberately small: the harness scores with its own evaluator, so this side
//! only has to say *what the agent did and why it stopped*.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

use serde::{Deserialize, Serialize};
use tracing::info;

use crate::agent::{Agent, AgentConfig};
use crate::channels::{
    ConversationId, IncomingMessage, InputProvenance, MentionState, MessageMetadata, UserId,
};
use crate::computer::remote::{OsworldDesktop, RemoteDesktopConfig};
use crate::computer::ComputerAdapter;
use crate::core::models::Id;
use crate::error::SyscityError;
use crate::providers::resolver::resolve_provider;
use crate::providers::Provider;
use crate::tools::computer::ComputerTool;
use crate::tools::screen_state::ScreenStateTool;
use crate::tools::{PermissionsConfig, PermissionsRuntime, TimeTool, TodoTool, ToolRegistry};
use crate::Result;

/// Version of the result JSON. Bump it when a field changes meaning, not when
/// one is added.
pub const RESULT_SCHEMA: u32 = 1;

/// Default tool-iteration budget, matching the eval suite's 60.
pub const DEFAULT_MAX_STEPS: usize = 60;

/// Everything `syscity bench run` needs. Mirrors the CLI flags one for one.
#[derive(Debug, Clone, Default)]
pub struct BenchOptions {
    /// The task, verbatim from the benchmark.
    pub instruction: String,
    /// Base URL of the VM's desktop service, e.g. `http://10.0.2.15:5000`.
    pub computer_url: String,
    /// Benchmark task id — names the conversation and the output file.
    pub task_id: String,
    /// Tool-iteration budget. `None` falls back to
    /// `SYSCITY_MAX_TOOL_ITERATIONS`, then [`DEFAULT_MAX_STEPS`].
    pub max_steps: Option<usize>,
    /// Where to write the result JSON. `None` prints the contract to stdout
    /// only.
    pub out: Option<PathBuf>,
    /// Provider preset override.
    pub provider: Option<String>,
    /// Model override.
    pub model: Option<String>,
    /// API key override.
    pub api_key: Option<String>,
    /// Base URL override for the provider.
    pub base_url: Option<String>,
    /// Bearer token for the desktop service, when it asks for one.
    pub auth_token: Option<String>,
}

/// The result file. Field meanings are part of the harness contract.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BenchResult {
    pub schema: u32,
    pub task_id: String,
    pub instruction: String,
    /// The agent's final text, verbatim. Empty when the run errored.
    pub answer: String,
    /// `final_answer` | `max_steps` | `error`.
    pub stop_reason: String,
    /// Model rounds that used tools (`Context::tool_iterations`).
    pub steps: usize,
    /// Individual tool invocations, including non-desktop ones.
    pub tool_calls: usize,
    pub elapsed_ms: u64,
    pub provider: String,
    pub model: String,
    pub error: Option<String>,
    /// RFC 3339, UTC, second precision.
    pub started_at: String,
}

/// Run one instruction against a remote desktop and return the result.
///
/// A failure of the *agent turn* is not a failure of this function: it becomes
/// a [`BenchResult`] with `stop_reason: "error"` and the message in `error`, so
/// the harness always gets a file to read. `Err` is reserved for setup
/// failures — no URL, no provider, an unwritable output path.
pub async fn run_once(opts: BenchOptions) -> Result<BenchResult> {
    let started = Instant::now();
    let started_at = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true);

    if opts.instruction.trim().is_empty() {
        return Err(SyscityError::Validation("--instruction must not be empty".to_string()));
    }
    if opts.computer_url.trim().is_empty() {
        return Err(SyscityError::Validation(
            "--computer-url is required: the benchmark owns the desktop, this side only drives it"
                .to_string(),
        ));
    }

    let budget = opts
        .max_steps
        .or_else(|| {
            std::env::var("SYSCITY_MAX_TOOL_ITERATIONS")
                .ok()
                .and_then(|v| v.trim().parse().ok())
        })
        .unwrap_or(DEFAULT_MAX_STEPS);
    // The agent reads its budget from the environment at context creation; there
    // is no per-agent field for it. Set it before the Agent exists.
    std::env::set_var("SYSCITY_MAX_TOOL_ITERATIONS", budget.to_string());

    let (provider, provider_name, model_name) = build_provider(&opts).await?;
    run_with_provider(&opts, provider, &provider_name, &model_name, budget, started, started_at)
        .await
}

/// The part that depends on a provider rather than on how one is chosen, so a
/// test can drive a full task against a fake desktop and a scripted model.
async fn run_with_provider(
    opts: &BenchOptions,
    provider: Arc<dyn Provider>,
    provider_name: &str,
    model_name: &str,
    budget: usize,
    started: Instant,
    started_at: String,
) -> Result<BenchResult> {
    let mut desktop_config = RemoteDesktopConfig::new(opts.computer_url.trim());
    if let Some(token) = &opts.auth_token {
        desktop_config = desktop_config.with_token(token.clone());
    }
    let adapter: Arc<dyn ComputerAdapter> = Arc::new(OsworldDesktop::new(desktop_config)?);

    // One adapter per task: the VM was reset to the task's initial state, and
    // element ids are addresses inside that state only.
    let registry = bench_registry(adapter);
    let available = registry.list().join(", ");

    let mut agent_config = AgentConfig {
        agent_id: Some(format!("bench-{}", opts.task_id)),
        ..AgentConfig::default()
    };
    agent_config.max_tokens = 8192;
    agent_config.temperature = 0.0;
    agent_config
        .system_prompt
        .push_str(&bench_prompt_suffix(&available));

    let conversation = format!("bench-{}", opts.task_id);
    let agent = Arc::new(Agent::new(agent_config, provider, Arc::new(registry)));

    info!(
        "Bench task {} on {} (provider: {}, model: {}, budget: {})",
        opts.task_id, opts.computer_url, provider_name, model_name, budget
    );

    let message = IncomingMessage {
        id: Id::new(),
        user_id: UserId::new(format!("bench-{}", opts.task_id)),
        conversation_id: ConversationId::new(conversation.clone()),
        content: opts.instruction.clone(),
        attachments: vec![],
        metadata: MessageMetadata::new(),
        provenance: InputProvenance::ExternalUser {
            channel: "bench".into(),
            is_direct: true,
        },
        mention: MentionState::DirectMessage,
    };
    let noop_cb: crate::agent::ProgressCallback = Arc::new(|_| Box::pin(async {}));
    let outcome = agent.process_message_with_progress(message, noop_cb).await;

    let (answer, error) = match outcome {
        Ok(outgoing) => (outgoing.content, None),
        Err(e) => (String::new(), Some(e.to_string())),
    };
    let counts = collect_counts(&agent, &conversation).await;

    let stop_reason = if error.is_some() {
        "error"
    } else if counts.budget_reached {
        // The budget path lets the agent write one final tool-free summary, so
        // "the agent said something" does not mean it finished. This does.
        "max_steps"
    } else {
        "final_answer"
    };

    let result = BenchResult {
        schema: RESULT_SCHEMA,
        task_id: opts.task_id.clone(),
        instruction: opts.instruction.clone(),
        answer,
        stop_reason: stop_reason.to_string(),
        steps: counts.steps,
        tool_calls: counts.tool_calls,
        elapsed_ms: started.elapsed().as_millis() as u64,
        provider: provider_name.to_string(),
        model: model_name.to_string(),
        error,
        started_at,
    };

    if let Some(path) = &opts.out {
        write_result(path, &result).await?;
    }

    Ok(result)
}

/// What the thread log says about the run.
struct Counts {
    steps: usize,
    tool_calls: usize,
    budget_reached: bool,
}

/// Read the run's cost back from the conversation thread.
///
/// `steps` is the tool-iteration counter (one per model round that used
/// tools), not the number of tool invocations: a round can call three tools.
/// `outgoing.tool_calls` cannot be used for this — the engine leaves it unset,
/// so the per-turn records are the source of truth.
async fn collect_counts(agent: &Agent, conversation: &str) -> Counts {
    let threads = agent.thread_map.lock().await;
    let Some(thread) = threads.get(conversation) else {
        return Counts {
            steps: 0,
            tool_calls: 0,
            budget_reached: false,
        };
    };
    Counts {
        steps: thread.context.tool_iterations(),
        tool_calls: thread.turns.iter().map(|t| t.tool_calls.len()).sum(),
        budget_reached: thread.context.is_tool_limit_reached(),
    }
}

async fn write_result(path: &PathBuf, result: &BenchResult) -> Result<()> {
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            tokio::fs::create_dir_all(parent).await.map_err(|e| {
                SyscityError::Validation(format!(
                    "could not create the result directory {}: {e}",
                    parent.display()
                ))
            })?;
        }
    }
    let body = serde_json::to_string_pretty(result)
        .map_err(|e| SyscityError::Validation(format!("could not serialize the result: {e}")))?;
    tokio::fs::write(path, body).await.map_err(|e| {
        SyscityError::Validation(format!("could not write the result to {}: {e}", path.display()))
    })?;
    Ok(())
}

/// Provider, provider name, and the model actually in use.
///
/// Mirrors the eval runner's resolution so a benchmark run and an eval run on
/// the same machine pick the same provider: explicit flags, then the gateway
/// config, then the environment.
async fn build_provider(opts: &BenchOptions) -> Result<(Arc<dyn Provider>, String, String)> {
    let gateway_cfg = crate::eval::standalone::try_load_gateway_config().await;

    let provider_type = opts.provider.clone().unwrap_or_else(|| {
        gateway_cfg
            .as_ref()
            .map(|g| g.model_provider.clone())
            .unwrap_or_else(|| "anthropic".to_string())
    });
    let model = opts
        .model
        .clone()
        .or_else(|| gateway_cfg.as_ref().map(|g| g.model.clone()));

    let api_key = match opts.api_key.clone() {
        Some(key) => Some(key),
        None => match gateway_cfg
            .as_ref()
            .and_then(|g| g.providers.get(&provider_type))
        {
            Some(preset) => {
                let secrets = crate::secrets::SecretStoreHandle::new();
                let key = preset.effective_key(Some(&secrets)).await;
                if key.is_empty() {
                    None
                } else {
                    Some(key)
                }
            }
            None => None,
        }
        .or_else(|| std::env::var("ANTHROPIC_API_KEY").ok())
        .or_else(|| std::env::var("OPENAI_API_KEY").ok()),
    };

    let base_url = opts.base_url.clone().or_else(|| {
        gateway_cfg
            .as_ref()
            .and_then(|g| g.providers.get(&provider_type))
            .and_then(|p| p.base_url.clone())
            .or_else(|| std::env::var("SYSCITY_BASE_URL").ok())
    });

    let provider = resolve_provider(&provider_type, api_key, base_url, model.clone(), None)
        .map_err(|e| {
            SyscityError::Validation(format!(
                "Failed to create provider '{provider_type}': {e}\n\
                 Set the provider's API key env var, or pass --api-key"
            ))
        })?;
    let model_name = model.unwrap_or_else(|| provider.default_model().to_string());

    Ok((provider, provider_type, model_name))
}

/// The tool set a benchmark run gets: the desktop, the screen, the clock, and
/// a scratchpad.
///
/// This is the run's permission boundary as much as its tool list. `shell`,
/// `file`, `browser` and `web` are absent on purpose: they act on the **host**
/// running this process, not on the benchmark VM, so registering them would let
/// a trial score by working around the desktop it was asked to operate — and
/// would make the number meaningless.
fn bench_registry(adapter: Arc<dyn ComputerAdapter>) -> ToolRegistry {
    let mut registry = ToolRegistry::new();
    registry.register(Box::new(ComputerTool::new(Some(adapter.clone()))));
    registry.register(Box::new(ScreenStateTool::new(
        Some(adapter),
        #[cfg(feature = "vision")]
        crate::tools::screen_state::new_shared_ocr(),
        #[cfg(feature = "vision")]
        crate::tools::screen_state::new_shared_ui_detector(),
    )));
    registry.register(Box::new(TimeTool));
    registry.register(Box::new(TodoTool::new()));

    let allow = registry.list();
    registry.with_permissions(bench_permissions(allow))
}

/// A benchmark run is unattended: every tool it registered is pre-approved by
/// name and nothing else is granted, so the measurement is of the agent rather
/// than of an approval policy with nobody to answer it.
fn bench_permissions(allow: Vec<String>) -> Arc<PermissionsRuntime> {
    Arc::new(PermissionsRuntime::from_config(&PermissionsConfig {
        allow,
        ..Default::default()
    }))
}

/// What the agent is told about the setting, appended to the normal prompt.
fn bench_prompt_suffix(available: &str) -> String {
    format!(
        "\n\n## Benchmark session\n\n\
         You are operating the desktop of a benchmark virtual machine through the `computer` \
         tool. Everything you see and act on belongs to that machine — the local machine is \
         not available, and there is nobody to answer a question or approve a step.\n\n\
         - Look at the screen before acting, and look again afterwards: the tool tells you \
           whether the screen actually changed.\n\
         - Prefer element ids from the accessibility tree over guessed coordinates; re-read \
           the tree when an id goes stale.\n\
         - Work until the task is done, then reply with one short paragraph saying what you \
           did and what is now on screen. Do not ask for confirmation.\n\n\
         Available tools: {available}.\n"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_adapter() -> Arc<dyn ComputerAdapter> {
        Arc::new(
            OsworldDesktop::new(RemoteDesktopConfig::new("http://127.0.0.1:1"))
                .expect("the adapter builds without a server"),
        )
    }

    #[tokio::test]
    async fn the_bench_tool_set_stays_minimal() {
        // Host-acting tools must never appear here: a trial that could run
        // shell commands on the host could pass without touching the VM.
        let registry = bench_registry(test_adapter());
        let mut names = registry.list();
        names.sort();
        assert_eq!(names, vec!["computer", "screen_state", "time", "todo"]);
    }

    #[tokio::test]
    async fn the_bench_tool_set_is_pre_approved() {
        // An unattended run has nobody to answer an approval prompt; if a tool
        // it registered still asked, the trial would fail closed with an
        // approval error instead of measuring the agent.
        let registry = bench_registry(test_adapter());
        let permissions = bench_permissions(registry.list());
        for name in registry.list() {
            let decision =
                permissions.evaluate(&name, &serde_json::json!({}), "bench-t", false, &[]);
            assert!(
                matches!(decision, crate::tools::EngineDecision::AllowNow),
                "{name} should be pre-approved in a bench run, got {decision:?}"
            );
        }
    }

    #[test]
    fn the_prompt_names_the_tools_it_was_given() {
        let suffix = bench_prompt_suffix("computer, screen_state, time, todo");
        assert!(suffix.contains("computer, screen_state, time, todo"));
        assert!(suffix.contains("nobody to answer"));
    }

    #[test]
    fn a_result_round_trips_through_json() {
        let result = BenchResult {
            schema: RESULT_SCHEMA,
            task_id: "7d8a1c".into(),
            instruction: "Open the file".into(),
            answer: "Opened it.".into(),
            stop_reason: "final_answer".into(),
            steps: 17,
            tool_calls: 24,
            elapsed_ms: 183_000,
            provider: "deepseek".into(),
            model: "deepseek-v4-flash".into(),
            error: None,
            started_at: "2026-10-29T10:00:00Z".into(),
        };
        let json = serde_json::to_string(&result).unwrap();
        let back: BenchResult = serde_json::from_str(&json).unwrap();
        assert_eq!(back.schema, 1);
        assert_eq!(back.steps, 17);
        assert!(back.error.is_none());
    }

    // ── End to end: a scripted model, a fake VM, a written result file ──────

    struct FakeDesktop {
        screens: std::sync::Mutex<usize>,
        commands: std::sync::Mutex<Vec<String>>,
    }

    fn png_bytes(width: u32, height: u32) -> Vec<u8> {
        let mut bytes = vec![0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a];
        bytes.extend_from_slice(&13u32.to_be_bytes());
        bytes.extend_from_slice(b"IHDR");
        bytes.extend_from_slice(&width.to_be_bytes());
        bytes.extend_from_slice(&height.to_be_bytes());
        bytes.extend_from_slice(&[8, 6, 0, 0, 0]);
        bytes
    }

    async fn start_fake_desktop() -> (String, Arc<FakeDesktop>) {
        use axum::routing::{get, post};
        use base64::Engine;

        let state = Arc::new(FakeDesktop {
            screens: std::sync::Mutex::new(0),
            commands: std::sync::Mutex::new(Vec::new()),
        });

        let screenshot = move || async move {
            axum::Json(serde_json::json!({
                "screenshot": base64::engine::general_purpose::STANDARD
                    .encode(png_bytes(1280, 800)),
                "error": null,
            }))
        };
        let tree = || async {
            r#"<desktop-frame role="desktop frame" name="Desktop" x="0" y="0"
                 width="1280" height="800">
                 <push-button role="push button" name="Save" states="enabled"
                   x="100" y="50" width="20" height="20"/>
               </desktop-frame>"#
        };
        let counter = state.clone();
        let app = axum::Router::new()
            .route(
                "/screenshot",
                get(move || {
                    let counter = counter.clone();
                    async move {
                        *counter.screens.lock().unwrap() += 1;
                        screenshot().await
                    }
                }),
            )
            .route("/accessibility", get(tree))
            .route(
                "/screen_size",
                post(|| async {
                    axum::Json(serde_json::json!({
                        "output": { "width": 1280, "height": 800 },
                        "error": null,
                    }))
                }),
            )
            .route(
                "/execute",
                post(
                    |axum::extract::State(state): axum::extract::State<Arc<FakeDesktop>>,
                     axum::Json(body): axum::Json<serde_json::Value>| async move {
                        let command = body
                            .get("command")
                            .and_then(|c| c.as_str())
                            .unwrap_or_default()
                            .to_string();
                        state.commands.lock().unwrap().push(command);
                        axum::Json(serde_json::json!({ "output": "", "error": null }))
                    },
                ),
            )
            .with_state(state.clone());

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        (format!("http://{address}"), state)
    }

    #[tokio::test]
    async fn a_whole_task_runs_and_leaves_the_contract_on_disk() {
        use crate::providers::mock::MockProvider;
        use crate::providers::{FunctionCall, Message, Role, ToolCall};

        let (url, vm) = start_fake_desktop().await;

        // First round: look at the screen. Second round: report.
        let provider = Arc::new(MockProvider::new().with_callback(|messages| {
            let saw_tool = messages.iter().any(|m| m.role == Role::Tool);
            if saw_tool {
                Message::assistant("I read the screen and clicked Save.")
            } else {
                Message::assistant("Let me look at the screen first.").with_tool_calls(vec![
                    ToolCall {
                        id: "call_1".to_string(),
                        call_type: "function".to_string(),
                        function: FunctionCall {
                            name: "computer".to_string(),
                            arguments: r#"{"action":"screenshot"}"#.to_string(),
                        },
                        index: None,
                        result: None,
                    },
                ])
            }
        }));

        let out = std::env::temp_dir().join(format!(
            "syscity-bench-{}-{}/result.json",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let opts = BenchOptions {
            instruction: "Click Save".into(),
            computer_url: url,
            task_id: "t1".into(),
            out: Some(out.clone()),
            ..Default::default()
        };

        let result = run_with_provider(
            &opts,
            provider,
            "mock",
            "mock-1",
            5,
            Instant::now(),
            "2026-10-29T00:00:00Z".to_string(),
        )
        .await
        .expect("the task runs to completion");

        assert_eq!(result.stop_reason, "final_answer");
        assert!(result.answer.contains("clicked Save"), "answer: {}", result.answer);
        assert!(result.steps >= 1, "the tool round is counted");
        assert!(result.tool_calls >= 1, "the tool call is counted");
        assert!(*vm.screens.lock().unwrap() >= 1, "the agent really looked");

        let written = tokio::fs::read_to_string(&out).await.unwrap();
        let json: serde_json::Value = serde_json::from_str(&written).unwrap();
        assert_eq!(json["schema"], 1);
        assert_eq!(json["task_id"], "t1");
        assert_eq!(json["stop_reason"], "final_answer");
        let _ = tokio::fs::remove_dir_all(out.parent().unwrap()).await;
    }
}
