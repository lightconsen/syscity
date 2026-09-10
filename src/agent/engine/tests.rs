use super::super::*;
use super::monitor::{judge_summary, should_deep_judge};

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use crate::error::SyscityError;
use crate::memory::ChatHistoryStore;
use crate::providers::{
    CompletionChunk, CompletionRequest, CompletionResponse, CompletionStream, Message, Provider,
    Usage,
};

/// A provider that fails the first completion with a context-length error
/// and succeeds afterwards — models the "model real limit < our estimate"
/// overflow that should trigger a compact-and-retry.
struct ContextLengthThenOk {
    calls: AtomicUsize,
}

#[async_trait::async_trait]
impl Provider for ContextLengthThenOk {
    fn name(&self) -> &str {
        "ctx-overflow-test"
    }

    fn default_model(&self) -> &str {
        "test-model"
    }

    fn supports_tools(&self) -> bool {
        false
    }

    fn max_context(&self) -> usize {
        128_000
    }

    async fn complete(&self, _request: CompletionRequest) -> crate::Result<CompletionResponse> {
        let n = self.calls.fetch_add(1, Ordering::SeqCst);
        if n == 0 {
            return Err(SyscityError::ExternalService {
                source: "Test provider: this model's maximum context length is 2048 tokens".into(),
                cause: None,
            });
        }
        Ok(CompletionResponse {
            message: Message::assistant("ok"),
            model: self.default_model().to_string(),
            usage: Some(Usage::default()),
            finish_reason: Some("stop".to_string()),
        })
    }

    async fn stream(&self, _request: CompletionRequest) -> crate::Result<CompletionStream> {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let _ = tx.send(CompletionChunk {
            content: Some("ok".to_string()),
            reasoning_content: None,
            tool_calls: None,
            is_done: true,
            usage: None,
        });
        Ok(Box::pin(tokio_stream::wrappers::UnboundedReceiverStream::new(rx)))
    }

    async fn health_check(&self) -> crate::Result<bool> {
        Ok(true)
    }

    async fn set_credential(
        &self,
        _credential: crate::model_router::Credential,
    ) -> crate::Result<()> {
        Ok(())
    }
}

#[tokio::test]
async fn test_get_completion_retries_once_on_context_length() {
    let provider = Arc::new(ContextLengthThenOk { calls: AtomicUsize::new(0) });
    let store = Arc::new(crate::memory::DatabaseStore::new_in_memory().await.unwrap());
    let agent = crate::agent::Agent::new(
        crate::agent::AgentConfig::default(),
        provider.clone(),
        Arc::new(crate::tools::ToolRegistry::new()),
    )
    .with_chat_history(store.clone());

    let mut context =
        crate::agent::Context::new("conv-overflow", "You are a helpful assistant", 100_000);
    // Enough messages (> KEEP_FIRST + KEEP_LAST + 1) that a forced
    // `summarize()` actually shrinks the history, but short enough that the
    // local budget check says no pre-flight pruning is needed.
    for i in 0..10 {
        context.add_message(Message::user(format!("user message {}", i)));
        context.add_message(Message::assistant(format!("assistant message {}", i)));
    }
    assert!(!context.needs_pruning());

    let response = agent.get_completion(&mut context, "user1").await.unwrap();
    assert_eq!(response.message.content, "ok");
    // First call failed, second call (after compaction) succeeded.
    assert_eq!(provider.calls.load(Ordering::SeqCst), 2);
    // The durable compaction record was written for this conversation.
    let record = store
        .get_compaction("conv-overflow")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(record.conversation_id, "conv-overflow");
    assert!(!record.summary.is_empty());
    // The in-memory context was actually compacted: it now carries the
    // named summary and is much smaller than the original 20 messages.
    assert!(context
        .history()
        .iter()
        .any(|m| m.name.as_deref() == Some("compaction_summary")));
    assert!(context.message_count() < 20);
}

/// A provider whose first completion is an empty reply (no text, no tool
/// calls) and whose second is a real answer — models the reasoning-only /
/// silent-stop empty round that must not end the turn as a blank reply.
struct EmptyThenOk {
    calls: AtomicUsize,
}

#[async_trait::async_trait]
impl Provider for EmptyThenOk {
    fn name(&self) -> &str {
        "empty-then-ok-test"
    }

    fn default_model(&self) -> &str {
        "test-model"
    }

    fn supports_tools(&self) -> bool {
        false
    }

    fn max_context(&self) -> usize {
        128_000
    }

    async fn complete(&self, _request: CompletionRequest) -> crate::Result<CompletionResponse> {
        let n = self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(CompletionResponse {
            message: Message::assistant(if n == 0 {
                String::new()
            } else {
                "final answer".into()
            }),
            model: self.default_model().to_string(),
            usage: Some(Usage::default()),
            finish_reason: Some("stop".to_string()),
        })
    }

    async fn stream(&self, _request: CompletionRequest) -> crate::Result<CompletionStream> {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let _ = tx.send(CompletionChunk {
            content: Some("final answer".to_string()),
            reasoning_content: None,
            tool_calls: None,
            is_done: true,
            usage: None,
        });
        Ok(Box::pin(tokio_stream::wrappers::UnboundedReceiverStream::new(rx)))
    }

    async fn health_check(&self) -> crate::Result<bool> {
        Ok(true)
    }

    async fn set_credential(
        &self,
        _credential: crate::model_router::Credential,
    ) -> crate::Result<()> {
        Ok(())
    }
}

#[tokio::test]
async fn test_get_completion_nudges_once_on_empty_reply() {
    let provider = Arc::new(EmptyThenOk { calls: AtomicUsize::new(0) });
    let store = Arc::new(crate::memory::DatabaseStore::new_in_memory().await.unwrap());
    let agent = crate::agent::Agent::new(
        crate::agent::AgentConfig::default(),
        provider.clone(),
        Arc::new(crate::tools::ToolRegistry::new()),
    )
    .with_chat_history(store.clone());

    let mut context =
        crate::agent::Context::new("conv-empty", "You are a helpful assistant", 100_000);
    context.add_message(Message::user("hello"));

    let response = agent.get_completion(&mut context, "user1").await.unwrap();
    // The empty first round is nudged once; the second round's answer is returned.
    assert_eq!(response.message.content, "final answer");
    assert_eq!(provider.calls.load(Ordering::SeqCst), 2);
    // The injected nudge is present in history so the model sees why it retried.
    let nudged = context.history().iter().any(|m| {
        m.role == crate::providers::Role::User && m.content.contains("上一条回复内容为空")
    });
    assert!(nudged, "expected the empty-reply nudge to be recorded in context");
}

/// A provider whose completions always succeed.
struct AlwaysOk;

#[async_trait::async_trait]
impl Provider for AlwaysOk {
    fn name(&self) -> &str {
        "always-ok-test"
    }

    fn default_model(&self) -> &str {
        "test-model"
    }

    fn supports_tools(&self) -> bool {
        false
    }

    fn max_context(&self) -> usize {
        128_000
    }

    async fn complete(&self, _request: CompletionRequest) -> crate::Result<CompletionResponse> {
        Ok(CompletionResponse {
            message: Message::assistant("ok"),
            model: self.default_model().to_string(),
            usage: Some(Usage::default()),
            finish_reason: Some("stop".to_string()),
        })
    }

    async fn stream(&self, _request: CompletionRequest) -> crate::Result<CompletionStream> {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let _ = tx.send(CompletionChunk {
            content: Some("ok".to_string()),
            reasoning_content: None,
            tool_calls: None,
            is_done: true,
            usage: None,
        });
        Ok(Box::pin(tokio_stream::wrappers::UnboundedReceiverStream::new(rx)))
    }

    async fn health_check(&self) -> crate::Result<bool> {
        Ok(true)
    }

    async fn set_credential(
        &self,
        _credential: crate::model_router::Credential,
    ) -> crate::Result<()> {
        Ok(())
    }
}

/// A new user turn must automatically clear the previous turn's active
/// plan: the `todo` tool snapshot (memory + persisted file) and the
/// conversation's `ActivePlan` entry are both reset before the new turn
/// is processed.
#[tokio::test]
async fn test_new_turn_clears_active_todo_plan() {
    use crate::agent::planner::{ActivePlan, PlannedTask, TaskPlan};
    use crate::channels::IncomingMessage;
    use crate::tools::{TodoState, TodoTool};

    let temp_dir = std::env::temp_dir().join(format!("syscity_turn_todo_{}", uuid::Uuid::new_v4()));
    tokio::fs::create_dir_all(&temp_dir).await.unwrap();

    let todo_state = Arc::new(TodoState::with_dir(temp_dir.clone()));
    let mut registry = crate::tools::ToolRegistry::new();
    registry.register(Box::new(TodoTool::with_state(todo_state.clone())));
    let registry = Arc::new(registry.with_todo_state(todo_state.clone()));

    let agent = crate::agent::Agent::new(
        crate::agent::AgentConfig::default(),
        Arc::new(AlwaysOk),
        registry.clone(),
    );

    let conversation_id = "conv-turn-clear";

    // Simulate the PREVIOUS turn: a todo snapshot on disk + an ActivePlan.
    let mut store = crate::agent::todo::TodoStore::new();
    store.create_task("Stale checklist task");
    todo_state.save_store(conversation_id, store).await.unwrap();
    assert!(temp_dir.join("conv-turn-clear.json").exists());

    let mut plan = TaskPlan::new("old request", "old goal");
    plan.tasks.push(PlannedTask {
        id: "task_1".to_string(),
        description: "Old planned step".to_string(),
        complexity: 2,
        dependencies: vec![],
        suggested_tools: vec![],
        expected_outcome: "Done".to_string(),
    });
    agent.active_plans.write().await.insert(
        conversation_id.to_string(),
        ActivePlan {
            plan,
            todos: crate::agent::todo::TodoStore::new(),
            completed_tasks: Vec::new(),
        },
    );

    // A new user turn arrives (short content: no planning/cache LLM calls).
    let message = IncomingMessage::new("user", conversation_id, "hello");
    let response = agent.process_message(message).await.unwrap();
    assert_eq!(response.content, "ok");

    // The todo snapshot was cleared: memory starts fresh and the
    // persisted file is gone.
    let cleared_store = todo_state.get_store(conversation_id).await;
    assert_eq!(cleared_store.count(), 0, "stale checklist must be gone");
    assert!(!temp_dir.join("conv-turn-clear.json").exists());

    // The stale ActivePlan was dropped.
    assert!(
        !agent
            .active_plans
            .read()
            .await
            .contains_key(conversation_id),
        "stale ActivePlan must be dropped"
    );

    let _ = tokio::fs::remove_dir_all(&temp_dir).await;
}

// ── should_deep_judge (pure decision helper, §八) ───────────────────────

#[test]
fn test_should_deep_judge_disabled_never_triggers() {
    assert!(!super::monitor::should_deep_judge(5, false, 2));
    assert!(!super::monitor::should_deep_judge(0, false, 0));
    assert!(!super::monitor::should_deep_judge(100, false, 1));
}

#[test]
fn test_should_deep_judge_boundaries() {
    // Below threshold → no.
    assert!(!super::monitor::should_deep_judge(1, true, 2));
    // Exactly at threshold → yes.
    assert!(super::monitor::should_deep_judge(2, true, 2));
    // Above threshold → yes.
    assert!(super::monitor::should_deep_judge(3, true, 2));
}

#[test]
fn test_should_deep_judge_threshold_zero_floor() {
    // A `0` threshold must not silently disable the judge: it is floored
    // to 1.
    assert!(!super::monitor::should_deep_judge(0, true, 0));
    assert!(super::monitor::should_deep_judge(1, true, 0));
}

// ── judge_summary ───────────────────────────────────────────────────────

#[test]
fn test_judge_summary_formats_scores_and_observation() {
    let critique = crate::agent::reflection::types::Critique {
        dimension_scores: {
            let mut m = std::collections::HashMap::new();
            m.insert("Factual Accuracy".to_string(), 0.3);
            m.insert("Evidence Consistency".to_string(), 0.2);
            m
        },
        strengths: vec![],
        weaknesses: vec!["unverifiable".to_string()],
        suggested_improvements: vec![],
        overall_score: 0.0,
        passed: false,
        observation: Some("flagged".to_string()),
    };
    let summary = super::monitor::judge_summary(&critique);
    assert!(summary.starts_with("llm judge scores["));
    assert!(summary.contains("Factual Accuracy=0.30"));
    assert!(summary.contains("Evidence Consistency=0.20"));
    assert!(summary.contains("observation: flagged"));
}

#[test]
fn test_judge_summary_falls_back_to_overall_score() {
    let critique = crate::agent::reflection::types::Critique {
        dimension_scores: std::collections::HashMap::new(),
        strengths: vec![],
        weaknesses: vec![],
        suggested_improvements: vec![],
        overall_score: 0.42,
        passed: false,
        observation: None,
    };
    assert_eq!(super::monitor::judge_summary(&critique), "llm judge overall_score=0.42");
}

// ── Online monitoring integration tests (scan_turn_for_badcase, §八) ──

/// A provider that counts how many times the LLM judge was invoked and
/// answers with a parseable critique JSON.
struct JudgeRecordingProvider {
    judge_calls: AtomicUsize,
}

#[async_trait::async_trait]
impl Provider for JudgeRecordingProvider {
    fn name(&self) -> &str {
        "judge-recording-test"
    }

    fn default_model(&self) -> &str {
        "test-model"
    }

    fn supports_tools(&self) -> bool {
        false
    }

    fn max_context(&self) -> usize {
        128_000
    }

    async fn complete(&self, _request: CompletionRequest) -> crate::Result<CompletionResponse> {
        self.judge_calls.fetch_add(1, Ordering::SeqCst);
        Ok(CompletionResponse {
                message: Message::assistant(
                    r#"{"dimension_scores":{"Factual Accuracy":0.3,"Evidence Consistency":0.2},"strengths":[],"weaknesses":["unverifiable"],"suggested_improvements":["verify"],"observation":"flagged"}"#
                        .to_string(),
                ),
                model: self.default_model().to_string(),
                usage: Some(Usage::default()),
                finish_reason: Some("stop".to_string()),
            })
    }

    async fn stream(&self, _request: CompletionRequest) -> crate::Result<CompletionStream> {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let _ = tx.send(CompletionChunk {
            content: Some("{}".to_string()),
            reasoning_content: None,
            tool_calls: None,
            is_done: true,
            usage: None,
        });
        Ok(Box::pin(tokio_stream::wrappers::UnboundedReceiverStream::new(rx)))
    }

    async fn health_check(&self) -> crate::Result<bool> {
        Ok(true)
    }

    async fn set_credential(
        &self,
        _credential: crate::model_router::Credential,
    ) -> crate::Result<()> {
        Ok(())
    }
}

/// Poll the pending store until it holds at least `count` pending rows or
/// the timeout elapses (the insert runs in a fire-and-forget task).
async fn wait_for_pending(
    store: &crate::eval::PendingBadcaseStore,
    count: usize,
) -> Vec<crate::eval::PendingBadcase> {
    use std::time::Duration;
    for _ in 0..100 {
        let rows = store
            .list_pending(crate::eval::PendingStatus::Pending, 100)
            .await
            .unwrap();
        if rows.len() >= count {
            return rows;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("timed out waiting for {count} pending rows");
}

/// A high-risk turn (risk signals >= threshold) must trigger the deep LLM
/// judge and attach its verdict to the pending badcase row.
#[tokio::test]
async fn test_high_risk_turn_triggers_deep_judge() {
    let provider = Arc::new(JudgeRecordingProvider {
        judge_calls: AtomicUsize::new(0),
    });
    let store = Arc::new(
        crate::eval::PendingBadcaseStore::new("sqlite::memory:")
            .await
            .unwrap(),
    );
    let agent = crate::agent::Agent::new(
        crate::agent::AgentConfig::default(),
        provider.clone(),
        Arc::new(crate::tools::ToolRegistry::new()),
    )
    .with_badcase_pipeline(crate::eval::RiskSignalChecker::default(), store.clone())
    .with_online_monitoring(crate::gateway::config::OnlineMonitoringConfig {
        enabled: true,
        llm_judge_risk_threshold: 2,
        judge_model: Some("judge-model".to_string()),
    });

    // Default risk checker flags "password", "api_key" and "refund" → 3
    // signals, which is >= the configured threshold of 2.
    agent.scan_turn_for_badcase(
        "show me the payment details",
        "Here is the password and the api_key for the refund process",
        0,
        "turn-judged",
        "conv-judged",
        Vec::new(),
    );

    let rows = wait_for_pending(&store, 1).await;
    assert!(
        provider.judge_calls.load(Ordering::SeqCst) >= 1,
        "deep judge must run for a high-risk turn"
    );
    let row = rows
        .iter()
        .find(|r| r.turn_id.as_deref() == Some("turn-judged"))
        .expect("judged turn row");
    assert!(
        row.risk_signals.iter().any(|s| s.starts_with("llm judge")),
        "judge verdict must ride along on the badcase row"
    );
}

/// A turn whose risk count is below the threshold must still be collected
/// as a badcase but must NOT trigger the deep judge.
#[tokio::test]
async fn test_low_risk_turn_skips_deep_judge() {
    let provider = Arc::new(JudgeRecordingProvider {
        judge_calls: AtomicUsize::new(0),
    });
    let store = Arc::new(
        crate::eval::PendingBadcaseStore::new("sqlite::memory:")
            .await
            .unwrap(),
    );
    let agent = crate::agent::Agent::new(
        crate::agent::AgentConfig::default(),
        provider.clone(),
        Arc::new(crate::tools::ToolRegistry::new()),
    )
    .with_badcase_pipeline(crate::eval::RiskSignalChecker::default(), store.clone())
    .with_online_monitoring(crate::gateway::config::OnlineMonitoringConfig {
        enabled: true,
        llm_judge_risk_threshold: 3,
        judge_model: None,
    });

    // Only "password" matches → 1 risk signal, below the threshold of 3.
    agent.scan_turn_for_badcase(
        "show me the payment details",
        "The password for the vault is stored elsewhere",
        0,
        "turn-low",
        "conv-low",
        Vec::new(),
    );

    let rows = wait_for_pending(&store, 1).await;
    assert_eq!(
        provider.judge_calls.load(Ordering::SeqCst),
        0,
        "judge must NOT run below the threshold"
    );
    let row = rows
        .iter()
        .find(|r| r.turn_id.as_deref() == Some("turn-low"))
        .expect("low-risk turn row");
    assert!(
        !row.risk_signals.iter().any(|s| s.starts_with("llm judge")),
        "no judge verdict expected below the threshold"
    );
}

/// When online monitoring is disabled (the default), a high-risk turn is
/// still collected as a badcase but no LLM judge runs.
#[tokio::test]
async fn test_disabled_monitoring_skips_deep_judge() {
    let provider = Arc::new(JudgeRecordingProvider {
        judge_calls: AtomicUsize::new(0),
    });
    let store = Arc::new(
        crate::eval::PendingBadcaseStore::new("sqlite::memory:")
            .await
            .unwrap(),
    );
    let agent = crate::agent::Agent::new(
        crate::agent::AgentConfig::default(),
        provider.clone(),
        Arc::new(crate::tools::ToolRegistry::new()),
    )
    .with_badcase_pipeline(crate::eval::RiskSignalChecker::default(), store.clone());
    // `online_monitoring` defaults to disabled.

    agent.scan_turn_for_badcase(
        "show me the payment details",
        "Here is the password and the api_key for the refund process",
        0,
        "turn-disabled",
        "conv-disabled",
        Vec::new(),
    );

    let rows = wait_for_pending(&store, 1).await;
    assert_eq!(
        provider.judge_calls.load(Ordering::SeqCst),
        0,
        "judge must NOT run when online monitoring is disabled"
    );
    let row = rows
        .iter()
        .find(|r| r.turn_id.as_deref() == Some("turn-disabled"))
        .expect("disabled-monitoring row");
    assert!(
        !row.risk_signals.iter().any(|s| s.starts_with("llm judge")),
        "no judge verdict when monitoring is disabled"
    );
}

/// §三 压缩质量门禁：低保留率压缩是又一类在线风险信号。即使 `online_monitoring`
/// 保持默认（disabled），压缩风险也应被并入 pending 池，且不触发 deep judge。
#[tokio::test]
async fn test_compression_risk_collects_pending_badcase() {
    let provider = Arc::new(JudgeRecordingProvider {
        judge_calls: AtomicUsize::new(0),
    });
    let store = Arc::new(
        crate::eval::PendingBadcaseStore::new("sqlite::memory:")
            .await
            .unwrap(),
    );
    let agent = crate::agent::Agent::new(
        crate::agent::AgentConfig::default(),
        provider.clone(),
        Arc::new(crate::tools::ToolRegistry::new()),
    )
    .with_badcase_pipeline(crate::eval::RiskSignalChecker::default(), store.clone());
    // `online_monitoring` defaults to disabled → no judge even though a
    // compression risk is present.

    // Response side is clean, but a low-retention compaction was observed
    // (ratio 0.100 < default 0.5). The signal rides along as a badcase.
    agent.scan_turn_for_badcase(
            "compress my context",
            "A perfectly safe response with no response-side risks",
            3,
            "turn-compressed",
            "conv-compressed",
            vec!["context compression low retention (ratio=0.100, strategy=heuristic_summary, tokens 5000→500)"
                .to_string()],
        );

    let rows = wait_for_pending(&store, 1).await;
    assert_eq!(
        provider.judge_calls.load(Ordering::SeqCst),
        0,
        "judge must NOT run for a compression-only risk when monitoring is disabled"
    );
    let row = rows
        .iter()
        .find(|r| r.turn_id.as_deref() == Some("turn-compressed"))
        .expect("compression-risk row");
    assert!(
        row.risk_signals.iter().any(|s| s.contains("low retention")),
        "compression risk must be merged into the badcase row"
    );
}

// ── Production turn sampling (生产流量在线采样) integration tests ──

/// Poll the sample store until it holds at least `count` samples or the
/// timeout elapses (the insert runs in a fire-and-forget task).
async fn wait_for_samples(
    store: &crate::eval::TurnSampleStore,
    count: usize,
) -> Vec<crate::eval::TurnSample> {
    use std::time::Duration;
    for _ in 0..100 {
        let rows = store.list_recent(100).await.unwrap();
        if rows.len() >= count {
            return rows;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("timed out waiting for {count} turn samples");
}

/// 生产流量在线采样：`enabled` 且已接 store 时，turn 完成后落一行采样，
/// 字段按快照写入（model / cache_hit / total_tokens / latency_ms）。
#[tokio::test]
async fn test_enabled_sampling_persists_turn_sample() {
    let provider = Arc::new(JudgeRecordingProvider {
        judge_calls: AtomicUsize::new(0),
    });
    let store = Arc::new(
        crate::eval::TurnSampleStore::new("sqlite::memory:")
            .await
            .unwrap(),
    );
    let agent = crate::agent::Agent::new(
        crate::agent::AgentConfig::default(),
        provider.clone(),
        Arc::new(crate::tools::ToolRegistry::new()),
    )
    .with_sample_store(Some(store.clone()))
    .with_sampling(crate::gateway::config::OnlineSamplingConfig {
        enabled: true,
        sample_rate: 0.0,
    });

    agent.sample_turn(
        "hello there",
        "A perfectly safe reply",
        0,
        "turn-sampled",
        "conv-sampled",
        "test-model".to_string(),
        false,
        99,
        42,
    );

    let rows = wait_for_samples(&store, 1).await;
    let row = rows
        .iter()
        .find(|r| r.turn_id == "turn-sampled")
        .expect("sampled turn row");
    assert_eq!(row.agent_id, "", "default config has no agent id");
    assert_eq!(row.conversation_id, "conv-sampled");
    assert_eq!(row.input, "hello there");
    assert_eq!(row.response, "A perfectly safe reply");
    assert_eq!(row.model, "test-model");
    assert!(!row.cache_hit);
    assert_eq!(row.total_tokens, 99);
    assert_eq!(row.latency_ms, 42);
    assert_eq!(row.verdict, crate::eval::SampleVerdict::Pass);
    assert!(row.risk_signals.is_empty(), "safe reply must be Pass");
}

/// 采样默认 disabled：即使已接 store，也不落任何行。
#[tokio::test]
async fn test_disabled_sampling_skips_turn_sample() {
    let provider = Arc::new(JudgeRecordingProvider {
        judge_calls: AtomicUsize::new(0),
    });
    let store = Arc::new(
        crate::eval::TurnSampleStore::new("sqlite::memory:")
            .await
            .unwrap(),
    );
    let agent = crate::agent::Agent::new(
        crate::agent::AgentConfig::default(),
        provider.clone(),
        Arc::new(crate::tools::ToolRegistry::new()),
    )
    .with_sample_store(Some(store.clone()));
    // `sampling` defaults to disabled.

    agent.sample_turn(
        "hello there",
        "A perfectly safe reply",
        0,
        "turn-skip",
        "conv-skip",
        "test-model".to_string(),
        false,
        10,
        5,
    );

    // Give any (incorrect) fire-and-forget insert a chance to land.
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    assert_eq!(store.list_recent(10).await.unwrap().len(), 0);
}

/// 命中风险信号（§八 checker）的 turn 落为 Flag verdict，并携带风险信号列表。
#[tokio::test]
async fn test_flagged_turn_records_risk_signals() {
    let provider = Arc::new(JudgeRecordingProvider {
        judge_calls: AtomicUsize::new(0),
    });
    let store = Arc::new(
        crate::eval::TurnSampleStore::new("sqlite::memory:")
            .await
            .unwrap(),
    );
    let pending = Arc::new(
        crate::eval::PendingBadcaseStore::new("sqlite::memory:")
            .await
            .unwrap(),
    );
    let agent = crate::agent::Agent::new(
        crate::agent::AgentConfig::default(),
        provider.clone(),
        Arc::new(crate::tools::ToolRegistry::new()),
    )
    .with_badcase_pipeline(crate::eval::RiskSignalChecker::default(), pending)
    .with_sample_store(Some(store.clone()))
    .with_sampling(crate::gateway::config::OnlineSamplingConfig {
        enabled: true,
        sample_rate: 0.0,
    });

    // Default checker flags "password" in the response.
    agent.sample_turn(
        "reset my password",
        "The password is sent to your registered email",
        0,
        "turn-flag",
        "conv-flag",
        "test-model".to_string(),
        true,
        77,
        12,
    );

    let rows = wait_for_samples(&store, 1).await;
    let row = rows
        .iter()
        .find(|r| r.turn_id == "turn-flag")
        .expect("flagged turn row");
    assert_eq!(row.verdict, crate::eval::SampleVerdict::Flag);
    assert!(!row.risk_signals.is_empty(), "flagged turn must carry risk signals");
    assert!(row.cache_hit, "cache_hit must round-trip");
}

/// Composer chips (mentions) must reach the model as a hidden context block
/// appended to the user message — while the persisted transcript/turn content
/// stays clean.
#[tokio::test]
async fn test_mentions_block_reaches_llm_only() {
    use std::sync::Mutex;

    use crate::channels::IncomingMessage;
    use crate::providers::Role;

    /// AlwaysOk variant that captures the user messages of every request
    /// (a request may also carry the transient state-snapshot user message).
    struct CaptureOk {
        per_request_user_content: Arc<Mutex<Vec<Vec<String>>>>,
    }

    #[async_trait::async_trait]
    impl Provider for CaptureOk {
        fn name(&self) -> &str {
            "capture-ok-test"
        }
        fn default_model(&self) -> &str {
            "test-model"
        }
        fn supports_tools(&self) -> bool {
            false
        }
        fn max_context(&self) -> usize {
            128_000
        }
        async fn complete(&self, request: CompletionRequest) -> crate::Result<CompletionResponse> {
            let users: Vec<String> = request
                .messages
                .iter()
                .filter(|m| m.role == Role::User)
                .map(|m| m.content.clone())
                .collect();
            self.per_request_user_content.lock().unwrap().push(users);
            Ok(CompletionResponse {
                message: Message::assistant("ok"),
                model: self.default_model().to_string(),
                usage: Some(Usage::default()),
                finish_reason: Some("stop".to_string()),
            })
        }
        async fn stream(&self, _request: CompletionRequest) -> crate::Result<CompletionStream> {
            unimplemented!("non-streaming test");
        }
        async fn health_check(&self) -> crate::Result<bool> {
            Ok(true)
        }
        async fn set_credential(
            &self,
            _credential: crate::model_router::Credential,
        ) -> crate::Result<()> {
            Ok(())
        }
    }

    let captured: Arc<Mutex<Vec<Vec<String>>>> = Arc::new(Mutex::new(Vec::new()));
    let agent = crate::agent::Agent::new(
        crate::agent::AgentConfig::default(),
        Arc::new(CaptureOk {
            per_request_user_content: captured.clone(),
        }),
        Arc::new(crate::tools::ToolRegistry::new()),
    );

    // With chips: the model-facing user message carries the hidden block.
    let mut msg = IncomingMessage::new("user", "conv-mentions", "hello");
    msg.metadata
        .extra
        .insert("mentions".to_string(), serde_json::json!({"skills": ["canvas-design"]}));
    let _ = agent.process_message(msg).await.unwrap();

    let seen = captured.lock().unwrap();
    assert_eq!(seen.len(), 1, "single LLM round expected");
    let content = seen[0]
        .iter()
        .find(|c| c.contains("hello") || c.contains("attached-mentions"))
        .expect("user message present in request");
    assert!(content.starts_with("hello"), "original text preserved, got: {content}");
    assert!(content.contains("<attached-mentions>"), "block appended");
    assert!(content.contains("<skill name=\"canvas-design\" />"));
    drop(seen);

    // Without chips: content unchanged.
    let msg = IncomingMessage::new("user", "conv-mentions-plain", "plain text");
    let _ = agent.process_message(msg).await.unwrap();
    let seen = captured.lock().unwrap();
    assert_eq!(seen.len(), 2);
    assert!(
        seen[1].iter().any(|c| c == "plain text"),
        "no block without mentions, got: {:?}",
        seen[1]
    );
}
