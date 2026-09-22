//! Tests for the delegation tool.

use super::tool::clamp_wait_seconds;
use super::*;
use std::sync::Mutex;

use crate::agent::subagent_registry::SubagentStatus;
use crate::agent::{Agent, AgentConfig};
use crate::providers::mock::MockProvider;
use crate::providers::Message;
use crate::tools::ToolRegistry;

#[test]
fn test_delegation_tracker() {
    let tracker = DelegationTracker::new(0);
    assert_eq!(tracker.depth, 0);
}

#[test]
fn test_task_spec_creation() {
    let task = TaskSpec {
        prompt: "Test task".to_string(),
        output_format: Some("json".to_string()),
        max_iterations: Some(10),
        allowed_tools: vec!["file_read".to_string()],
        context: HashMap::new(),
        target_agent: None,
        task_id: None,
    };
    assert_eq!(task.prompt, "Test task");
}

#[test]
fn test_child_status_serialization() {
    let status = ChildStatus::Running;
    let json = serde_json::to_string(&status).unwrap();
    assert_eq!(json, "\"running\"");
}

#[tokio::test]
async fn test_delegation_tracker_can_delegate_at_depth_0() {
    let tracker = DelegationTracker::new(0);
    assert!(tracker.can_delegate().await);
}

#[tokio::test]
async fn test_delegation_tracker_can_delegate_at_max_depth() {
    let cfg = DelegationConfig::default();
    let tracker = DelegationTracker::with_limits(cfg.max_depth as usize, cfg.clone());
    assert!(!tracker.can_delegate().await);

    let tracker = DelegationTracker::with_limits(cfg.max_depth as usize + 1, cfg);
    assert!(!tracker.can_delegate().await);
}

#[tokio::test]
async fn test_delegation_tracker_can_delegate_at_max_children() {
    let cfg = DelegationConfig::default();
    let tracker = DelegationTracker::with_limits(0, cfg.clone());
    for i in 0..cfg.max_children {
        let child = ChildAgent {
            id: format!("child-{}", i),
            parent_id: "parent".to_string(),
            task: TaskSpec {
                prompt: "test".to_string(),
                output_format: None,
                max_iterations: None,
                allowed_tools: vec![],
                context: HashMap::new(),
                target_agent: None,
                task_id: None,
            },
            status: ChildStatus::Pending,
            created_at: chrono::Utc::now(),
            result: None,
            error: None,
            budget: IterationBudget::new(10),
            iterations: Arc::new(AtomicUsize::new(0)),
        };
        tracker.register_child(child).await;
    }
    assert_eq!(tracker.child_count().await, cfg.max_children);
    assert!(!tracker.can_delegate().await);
}

#[tokio::test]
async fn test_delegation_tracker_register_and_get_child() {
    let tracker = DelegationTracker::new(0);
    let child = ChildAgent {
        id: "c1".to_string(),
        parent_id: "p1".to_string(),
        task: TaskSpec {
            prompt: "hello".to_string(),
            output_format: None,
            max_iterations: None,
            allowed_tools: vec![],
            context: HashMap::new(),
            target_agent: None,
            task_id: None,
        },
        status: ChildStatus::Pending,
        created_at: chrono::Utc::now(),
        result: None,
        error: None,
        budget: IterationBudget::new(10),
        iterations: Arc::new(AtomicUsize::new(0)),
    };
    tracker.register_child(child.clone()).await;

    let fetched = tracker.get_child("c1").await;
    assert!(fetched.is_some());
    assert_eq!(fetched.unwrap().id, "c1");

    assert!(tracker.get_child("missing").await.is_none());
}

#[tokio::test]
async fn test_delegation_tracker_update_status() {
    let tracker = DelegationTracker::new(0);
    let child = ChildAgent {
        id: "c1".to_string(),
        parent_id: "p1".to_string(),
        task: TaskSpec {
            prompt: "test".to_string(),
            output_format: None,
            max_iterations: None,
            allowed_tools: vec![],
            context: HashMap::new(),
            target_agent: None,
            task_id: None,
        },
        status: ChildStatus::Pending,
        created_at: chrono::Utc::now(),
        result: None,
        error: None,
        budget: IterationBudget::new(10),
        iterations: Arc::new(AtomicUsize::new(0)),
    };
    tracker.register_child(child).await;

    tracker.update_status("c1", ChildStatus::Running).await;
    let fetched = tracker.get_child("c1").await.unwrap();
    assert_eq!(fetched.status, ChildStatus::Running);
}

#[tokio::test]
async fn test_delegation_tracker_set_result() {
    let tracker = DelegationTracker::new(0);
    let child = ChildAgent {
        id: "c1".to_string(),
        parent_id: "p1".to_string(),
        task: TaskSpec {
            prompt: "test".to_string(),
            output_format: None,
            max_iterations: None,
            allowed_tools: vec![],
            context: HashMap::new(),
            target_agent: None,
            task_id: None,
        },
        status: ChildStatus::Running,
        created_at: chrono::Utc::now(),
        result: None,
        error: None,
        budget: IterationBudget::new(10),
        iterations: Arc::new(AtomicUsize::new(0)),
    };
    tracker.register_child(child).await;

    tracker.set_result("c1", "done".to_string()).await;
    let fetched = tracker.get_child("c1").await.unwrap();
    assert_eq!(fetched.status, ChildStatus::Completed);
    assert_eq!(fetched.result, Some("done".to_string()));
}

#[tokio::test]
async fn test_delegation_tracker_set_error() {
    let tracker = DelegationTracker::new(0);
    let child = ChildAgent {
        id: "c1".to_string(),
        parent_id: "p1".to_string(),
        task: TaskSpec {
            prompt: "test".to_string(),
            output_format: None,
            max_iterations: None,
            allowed_tools: vec![],
            context: HashMap::new(),
            target_agent: None,
            task_id: None,
        },
        status: ChildStatus::Running,
        created_at: chrono::Utc::now(),
        result: None,
        error: None,
        budget: IterationBudget::new(10),
        iterations: Arc::new(AtomicUsize::new(0)),
    };
    tracker.register_child(child).await;

    tracker.set_error("c1", "oops".to_string()).await;
    let fetched = tracker.get_child("c1").await.unwrap();
    assert_eq!(fetched.status, ChildStatus::Failed);
    assert_eq!(fetched.error, Some("oops".to_string()));
}

#[tokio::test]
async fn test_delegation_tracker_list_children() {
    let tracker = DelegationTracker::new(0);
    for i in 0..3 {
        let child = ChildAgent {
            id: format!("c{}", i),
            parent_id: "p".to_string(),
            task: TaskSpec {
                prompt: format!("task {}", i),
                output_format: None,
                max_iterations: None,
                allowed_tools: vec![],
                context: HashMap::new(),
                target_agent: None,
                task_id: None,
            },
            status: ChildStatus::Pending,
            created_at: chrono::Utc::now(),
            result: None,
            error: None,
            budget: IterationBudget::new(10),
            iterations: Arc::new(AtomicUsize::new(0)),
        };
        tracker.register_child(child).await;
    }
    let list = tracker.list_children().await;
    assert_eq!(list.len(), 3);
}

#[tokio::test]
async fn test_delegation_tracker_remove_child() {
    let tracker = DelegationTracker::new(0);
    let child = ChildAgent {
        id: "c1".to_string(),
        parent_id: "p1".to_string(),
        task: TaskSpec {
            prompt: "test".to_string(),
            output_format: None,
            max_iterations: None,
            allowed_tools: vec![],
            context: HashMap::new(),
            target_agent: None,
            task_id: None,
        },
        status: ChildStatus::Pending,
        created_at: chrono::Utc::now(),
        result: None,
        error: None,
        budget: IterationBudget::new(10),
        iterations: Arc::new(AtomicUsize::new(0)),
    };
    tracker.register_child(child).await;

    let removed = tracker.remove_child("c1").await;
    assert!(removed.is_some());
    assert_eq!(removed.unwrap().id, "c1");
    assert!(tracker.get_child("c1").await.is_none());
    assert!(tracker.remove_child("c1").await.is_none());
}

#[test]
fn test_delegate_tool_new() {
    let tool = DelegateTool::new(1);
    let debug = format!("{:?}", tool);
    assert!(debug.contains("DelegateTool"));
    assert!(debug.contains("has_agent: false"));
}

#[test]
fn test_delegate_tool_root() {
    let tool = DelegateTool::root();
    assert_eq!(tool.tracker.depth, 0);
}

#[test]
fn test_delegate_tool_registry_access() {
    let tool = DelegateTool::new(0);
    let _registry = tool.registry();
    // Registry is accessible and non-null
}

#[test]
fn test_child_status_variants() {
    assert_eq!(ChildStatus::Pending, ChildStatus::Pending);
    assert_eq!(ChildStatus::Running, ChildStatus::Running);
    assert_eq!(ChildStatus::Completed, ChildStatus::Completed);
    assert_eq!(ChildStatus::Failed, ChildStatus::Failed);
    assert_eq!(ChildStatus::Cancelled, ChildStatus::Cancelled);
    assert_ne!(ChildStatus::Pending, ChildStatus::Running);
}

#[test]
fn test_blocked_tools_const() {
    assert!(BLOCKED_TOOLS.contains(&"delegate"));
    assert!(BLOCKED_TOOLS.contains(&"clarify"));
    assert!(BLOCKED_TOOLS.contains(&"memory"));
    assert!(BLOCKED_TOOLS.contains(&"send_message"));
    assert!(BLOCKED_TOOLS.contains(&"execute_code"));
}

#[test]
fn test_delegation_config_defaults() {
    let cfg = DelegationConfig::default();
    assert_eq!(cfg.max_depth, 3);
    assert_eq!(cfg.max_children, 3);
}

#[test]
fn test_delegation_tracker_clone() {
    let tracker = DelegationTracker::new(0);
    let cloned = tracker.clone();
    assert_eq!(cloned.depth, tracker.depth);
    assert_eq!(cloned.max_children, tracker.max_children);
}

/// A resolver that hands back a named agent, so a test can tell "the name
/// was honoured" from "it fell back to the parent".
struct StubResolver {
    agents: HashMap<String, Arc<Agent>>,
}

#[async_trait]
impl AgentResolver for StubResolver {
    async fn resolve(&self, name: &str) -> Option<Arc<Agent>> {
        self.agents.get(name).cloned()
    }
}

fn named_agent(id: &str, response: &str) -> Arc<Agent> {
    let provider = Arc::new(MockProvider::new().with_responses(vec![Message::assistant(response)]));
    Arc::new(Agent::new(
        AgentConfig {
            agent_id: Some(id.to_string()),
            ..Default::default()
        },
        provider,
        Arc::new(ToolRegistry::new()),
    ))
}

/// `target_agent` is parsed out of the model's tool arguments, so it must
/// not be able to pick which agent — and therefore which workspace, secrets
/// and skill trust — the child runs under. And the task row must name the
/// agent that actually ran: it used to record the requested string even when
/// the lookup fell back to the parent.
#[tokio::test]
async fn target_agent_cannot_select_another_agent() {
    let store = Arc::new(
        DelegationTaskStore::new("sqlite::memory:")
            .await
            .expect("in-memory store"),
    );
    let parent = named_agent("parent", "done");
    let resolver: Arc<dyn AgentResolver> = Arc::new(StubResolver {
        agents: HashMap::from([
            ("parent".to_string(), parent.clone()),
            ("other".to_string(), named_agent("other", "done")),
        ]),
    });
    let tool = DelegateTool::with_agent(0, parent)
        .with_agent_resolver(resolver)
        .with_task_store(store.clone());
    let context = ToolContext::new("user", "parent-session");

    let result = tool
        .execute(
            serde_json::json!({
                "action": "spawn",
                "task": { "prompt": "do the other agent's work", "target_agent": "other" }
            }),
            &context,
        )
        .await
        .expect("the delegation still runs — as the parent agent");
    assert!(result.success, "spawn should succeed: {:?}", result);
    let child_id = result
        .data
        .as_ref()
        .and_then(|d| d.get("child_id"))
        .and_then(|v| v.as_str())
        .expect("child_id in result")
        .to_string();

    // The row is written inside the spawned child, so it may land just
    // after `execute` returns.
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(2);
    let row = loop {
        if let Some(row) = store.get_task(&child_id).await.expect("read task row") {
            break row;
        }
        assert!(tokio::time::Instant::now() < deadline, "the child's task row was never written");
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    };
    assert_eq!(
        row.agent_id, "parent",
        "the row must name the agent that ran the child, not the one asked for"
    );
}

#[tokio::test]
async fn test_spawn_uses_registry_for_routing_limits() {
    use std::time::Duration;

    let tool = DelegateTool::new(0);
    let registry = Arc::clone(tool.registry());
    let context = ToolContext::new("user", "parent-session");

    let args = serde_json::json!({
        "action": "spawn",
        "task": { "prompt": "test task" }
    });

    let result = tool.execute(args, &context).await.expect("execute spawn");
    assert!(result.success, "spawn should succeed: {:?}", result);

    let child_id = result
        .data
        .as_ref()
        .and_then(|d| d.get("child_id"))
        .and_then(|v| v.as_str())
        .expect("child_id in result")
        .to_string();

    // The registry must know about the same run id.
    assert!(
        registry.get_run(&child_id).await.is_some(),
        "registry should contain run with child_id"
    );

    // Cancel should kill the registry run.
    let cancel_args = serde_json::json!({
        "action": "cancel",
        "child_id": child_id
    });
    let cancel_result = tool
        .execute(cancel_args, &context)
        .await
        .expect("execute cancel");
    assert!(cancel_result.success);

    let run = registry
        .get_run(&child_id)
        .await
        .expect("run still in registry");
    assert!(matches!(run.status, crate::agent::SubagentStatus::Killed));
}

fn make_child(id: &str, status: ChildStatus) -> ChildAgent {
    ChildAgent {
        id: id.to_string(),
        parent_id: "parent".to_string(),
        task: TaskSpec {
            prompt: "test".to_string(),
            output_format: None,
            max_iterations: None,
            allowed_tools: vec![],
            context: HashMap::new(),
            target_agent: None,
            task_id: None,
        },
        status,
        created_at: chrono::Utc::now(),
        result: None,
        error: None,
        budget: IterationBudget::new(10),
        iterations: Arc::new(AtomicUsize::new(0)),
    }
}

#[tokio::test]
async fn test_wait_returns_completed_result() {
    let tool = DelegateTool::root();
    let mut child = make_child("c1", ChildStatus::Completed);
    child.result = Some("42 done".to_string());
    tool.tracker.register_child(child).await;

    let result = tool
        .execute(json!({"action": "wait", "child_id": "c1"}), &ToolContext::new("user", "s1"))
        .await
        .unwrap();
    assert!(result.success, "wait should succeed: {:?}", result.output);
    assert!(result.output.contains("42 done"));
    assert_eq!(result.data.as_ref().unwrap()["status"], "completed");
}

#[tokio::test]
async fn test_wait_returns_failed_child() {
    let tool = DelegateTool::root();
    let mut child = make_child("c1", ChildStatus::Failed);
    child.error = Some("boom".to_string());
    tool.tracker.register_child(child).await;

    let result = tool
        .execute(json!({"action": "wait", "child_id": "c1"}), &ToolContext::new("user", "s1"))
        .await
        .unwrap();
    assert!(!result.success, "failed child must surface as an error result");
    assert!(result.error.as_deref().unwrap_or("").contains("boom"));
}

#[tokio::test]
async fn test_wait_returns_cancelled() {
    let tool = DelegateTool::root();
    tool.tracker
        .register_child(make_child("c1", ChildStatus::Cancelled))
        .await;

    let result = tool
        .execute(json!({"action": "wait", "child_id": "c1"}), &ToolContext::new("user", "s1"))
        .await
        .unwrap();
    assert!(result.success);
    assert!(result.output.contains("was cancelled"));
}

#[tokio::test]
async fn test_wait_unknown_child() {
    let tool = DelegateTool::root();
    let result = tool
        .execute(json!({"action": "wait", "child_id": "ghost"}), &ToolContext::new("user", "s1"))
        .await
        .unwrap();
    assert!(!result.success);
    assert!(result.error.as_deref().unwrap_or("").contains("ghost"));
}

#[tokio::test]
async fn test_wait_timeout_returns_still_running() {
    // A running child with a 1 s budget times out as an Ok result, so the
    // circuit breaker is never tripped by a waiting call.
    let tool = DelegateTool::root();
    tool.tracker
        .register_child(make_child("c1", ChildStatus::Running))
        .await;

    let result = tool
        .execute(
            json!({"action": "wait", "child_id": "c1", "seconds": 1}),
            &ToolContext::new("user", "s1"),
        )
        .await
        .unwrap();
    assert!(result.success, "timeout must be Ok, not Err: {:?}", result.output);
    assert!(result.output.contains("still running"));
}

#[tokio::test]
async fn test_wait_observes_completion_mid_wait() {
    // The child completes while the parent is waiting; the poll loop picks
    // it up and returns the result instead of timing out.
    let tool = DelegateTool::root();
    let tracker = tool.tracker.clone();
    tool.tracker
        .register_child(make_child("c1", ChildStatus::Running))
        .await;
    tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        tracker.set_result("c1", "late result".to_string()).await;
    });

    let result = tool
        .wait_for_child("c1", std::time::Duration::from_secs(2))
        .await;
    assert!(result.success, "wait should observe mid-wait completion: {:?}", result.output);
    assert!(result.output.contains("late result"));
}

#[test]
fn test_clamp_wait_seconds_bounds() {
    assert_eq!(clamp_wait_seconds(0), 1);
    assert_eq!(clamp_wait_seconds(1), 1);
    assert_eq!(clamp_wait_seconds(30), 30);
    assert_eq!(clamp_wait_seconds(60), 60);
    assert_eq!(clamp_wait_seconds(300), 60);
    assert_eq!(clamp_wait_seconds(u64::MAX), 60);
}

// ── wait + wake integration (parent auto-wake, v2) ─────────────────────
// These drive `execute_child_task` end-to-end against a real agent, a real
// registry, a real task store, and a real [`DelegationWake`] dispatcher,
// then assert the two halves of the delegation result contract: the result
// lands in the tracker that `wait` reads, AND the parent is woken with it.

/// Records every wake delivered through a [`DelegationWake`] dispatcher.
#[derive(Default)]
struct RecordingWakeHandler {
    wakes: Arc<Mutex<Vec<(String, String)>>>,
}

#[async_trait]
impl crate::delegation::WakeHandler for RecordingWakeHandler {
    async fn wake(&self, parent_session: &str, message: &str) -> crate::Result<()> {
        self.wakes
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push((parent_session.to_string(), message.to_string()));
        Ok(())
    }
}

async fn wait_for_wake(handler: &RecordingWakeHandler, len: usize) {
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(2);
    while tokio::time::Instant::now() < deadline {
        if handler
            .wakes
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .len()
            >= len
        {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(2)).await;
    }
    let seen = handler
        .wakes
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .len();
    panic!("timed out waiting for {} wake(s), saw {}", len, seen);
}

fn mock_agent(response: &str) -> Arc<Agent> {
    let provider = Arc::new(MockProvider::new().with_responses(vec![Message::assistant(response)]));
    Arc::new(Agent::new(AgentConfig::default(), provider, Arc::new(ToolRegistry::new())))
}

/// Register a child run in the registry, as `spawn_child` does before
/// `execute_child_task` runs.  Returns the run id (the child id).
async fn register_child_run(registry: &SubagentRegistry, parent_session: &str) -> String {
    registry
        .spawn(parent_session, "worker", "child prompt", 2, |_, _| async {})
        .await
        .expect("spawn child run")
}

async fn wake_fixture() -> (
    Arc<SubagentRegistry>,
    Arc<DelegationTaskStore>,
    Arc<RecordingWakeHandler>,
    Arc<DelegationWake>,
) {
    let registry = Arc::new(SubagentRegistry::new(3, 10));
    let store = Arc::new(
        DelegationTaskStore::new("sqlite::memory:")
            .await
            .expect("in-memory store"),
    );
    let handler = Arc::new(RecordingWakeHandler::default());
    let wake = Arc::new(DelegationWake::new(handler.clone()));
    (registry, store, handler, wake)
}

/// Create the parent's task row (`delegation:parent-run`), optionally in a
/// terminal status so the wake guard has a row to consult.
async fn create_parent_row(store: &DelegationTaskStore, status: &str) {
    store
        .create_task(NewTask {
            id: "parent-run",
            root_id: "root-1",
            parent_id: None,
            depth: 1,
            agent_id: "manager",
            title: "Parent task",
            parent_session: None,
        })
        .await
        .unwrap();
    if status != "running" {
        store.set_status("parent-run", status).await.unwrap();
    }
}

fn child_scope(child_id: &str, parent_task_id: Option<String>, depth: u32) -> DelegationScope {
    DelegationScope {
        root_id: "root-1".to_string(),
        task_id: child_id.to_string(),
        parent_task_id,
        depth,
        max_depth: 3,
        allowed_tools: None,
        max_iterations: None,
    }
}

/// Even a child that fails before any agent runs records its lineage on
/// the row: the forwarder routes events by it, so the row must carry the
/// session the delegate call ran in from creation.
#[tokio::test]
async fn test_execute_child_task_records_parent_session_on_the_row() {
    let (registry, store, _handler, _wake) = wake_fixture().await;
    // The parent row carries the user session, as a tree root would; the
    // child's own session key (`delegation:<run_id>`) is what a delegated
    // parent records.
    store
        .create_task(NewTask {
            id: "parent-run",
            root_id: "root-1",
            parent_id: None,
            depth: 1,
            agent_id: "manager",
            title: "Parent task",
            parent_session: Some("user-session"),
        })
        .await
        .unwrap();
    let tool = DelegateTool::root();
    let child_id = register_child_run(&registry, "delegation:parent-run").await;
    tool.tracker
        .register_child(make_child(&child_id, ChildStatus::Running))
        .await;

    execute_child_task(
        child_id.clone(),
        completion_task(),
        ChildTaskEnv {
            tracker: tool.tracker.clone(),
            iterations: Arc::new(AtomicUsize::new(0)),
            // No agent: the row is created, then the run fails — and the
            // row must still carry its lineage.
            agent: None,
            registry,
            store: Some(store.clone()),
            scope: child_scope(&child_id, Some("parent-run".to_string()), 2),
            agent_id: "worker".to_string(),
            coordinator: None,
            wake: None,
            parent_session: Some("delegation:parent-run".to_string()),
        },
    )
    .await;

    let task = store
        .get_task(&child_id)
        .await
        .unwrap()
        .expect("row created");
    assert_eq!(task.status, "failed");
    assert_eq!(
        task.parent_session.as_deref(),
        Some("delegation:parent-run"),
        "the lineage is on the row from creation"
    );

    // And it resolves up the chain to the parent row's user session —
    // the walk the gateway forwarder does.
    assert_eq!(
        store.root_session_for_task(&child_id).await.unwrap(),
        Some("user-session".to_string())
    );
}

fn completion_task() -> TaskSpec {
    TaskSpec {
        // Short + contains "this" so the engine's follow-up heuristic skips
        // the cache-classifier LLM call and the single MockProvider
        // response goes straight to the completion.
        prompt: "finish this".to_string(),
        output_format: None,
        max_iterations: None,
        allowed_tools: vec![],
        context: HashMap::new(),
        target_agent: None,
        task_id: None,
    }
}

#[tokio::test]
async fn test_completed_child_is_waitable_and_wakes_active_parent() {
    let (registry, store, handler, wake) = wake_fixture().await;
    // The parent is a live delegated agent: its task row is still active.
    create_parent_row(&store, "running").await;

    // The child run exists in the registry under the parent's session, and
    // the tool's tracker knows the child is running (as `spawn_child`
    // would have registered it).
    let tool = DelegateTool::root();
    let child_id = register_child_run(&registry, "delegation:parent-run").await;
    tool.tracker
        .register_child(make_child(&child_id, ChildStatus::Running))
        .await;

    execute_child_task(
        child_id.clone(),
        completion_task(),
        ChildTaskEnv {
            tracker: tool.tracker.clone(),
            iterations: Arc::new(AtomicUsize::new(0)),
            agent: Some(mock_agent("the answer")),
            registry: registry.clone(),
            store: Some(store.clone()),
            scope: child_scope(&child_id, Some("parent-run".to_string()), 2),
            agent_id: "worker".to_string(),
            coordinator: None,
            wake: Some(wake.clone()),
            parent_session: None,
        },
    )
    .await;

    // Wait (fast path): the shared tracker `wait` reads now reports the
    // child completed with its result.
    let waited = tool
        .wait_for_child(&child_id, std::time::Duration::from_secs(1))
        .await;
    assert!(waited.success, "wait should see completed child: {:?}", waited.output);
    assert!(waited.output.contains("the answer"));

    // Wake (slow path): the parent is woken with the completion message.
    wait_for_wake(&handler, 1).await;
    let wakes = handler
        .wakes
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone();
    assert_eq!(wakes.len(), 1);
    assert_eq!(wakes[0].0, "delegation:parent-run");
    assert!(wakes[0].1.contains("the answer"));
    assert!(wakes[0].1.contains(&child_id));

    // The registry run completed, so `delegate status` / crash recovery
    // still see the outcome.
    let run = registry.get_run(&child_id).await.expect("child run exists");
    match run.status {
        SubagentStatus::Completed(out) => assert_eq!(out, "the answer"),
        other => panic!("expected completed run, got {:?}", other),
    }
}

#[tokio::test]
async fn test_completed_child_does_not_wake_terminal_parent() {
    let (registry, store, handler, wake) = wake_fixture().await;
    // The parent already finished its part — waking it would be noise, so
    // the guard suppresses the notification.
    create_parent_row(&store, "completed").await;

    let tool = DelegateTool::root();
    let child_id = register_child_run(&registry, "delegation:parent-run").await;
    tool.tracker
        .register_child(make_child(&child_id, ChildStatus::Running))
        .await;

    execute_child_task(
        child_id.clone(),
        completion_task(),
        ChildTaskEnv {
            tracker: tool.tracker.clone(),
            iterations: Arc::new(AtomicUsize::new(0)),
            agent: Some(mock_agent("result")),
            registry: registry.clone(),
            store: Some(store.clone()),
            scope: child_scope(&child_id, Some("parent-run".to_string()), 2),
            agent_id: "worker".to_string(),
            coordinator: None,
            wake: Some(wake.clone()),
            parent_session: None,
        },
    )
    .await;

    // The child still completes and its result is still wait-able…
    let waited = tool
        .wait_for_child(&child_id, std::time::Duration::from_secs(1))
        .await;
    assert!(waited.success, "child should still complete: {:?}", waited.output);
    assert!(waited.output.contains("result"));

    // …but the terminal parent is never woken.
    tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    let wakes = handler
        .wakes
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone();
    assert!(wakes.is_empty(), "terminal parent must not be woken");
}

#[tokio::test]
async fn test_completed_child_always_wakes_root_parent() {
    let (registry, _store, handler, wake) = wake_fixture().await;
    // Root parents (no `delegation:` session, no task row) are always
    // woken — their turn may already have ended with the child outstanding.
    let child_id = register_child_run(&registry, "user-session-1").await;

    // store: None — the no-store path also defaults the guard to active.
    execute_child_task(
        child_id.clone(),
        completion_task(),
        ChildTaskEnv {
            tracker: DelegationTracker::new(1),
            iterations: Arc::new(AtomicUsize::new(0)),
            agent: Some(mock_agent("root answer")),
            registry,
            store: None,
            scope: child_scope(&child_id, None, 1),
            agent_id: "worker".to_string(),
            coordinator: None,
            wake: Some(wake),
            parent_session: None,
        },
    )
    .await;

    wait_for_wake(&handler, 1).await;
    let wakes = handler
        .wakes
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone();
    assert_eq!(wakes.len(), 1);
    assert_eq!(wakes[0].0, "user-session-1");
    assert!(wakes[0].1.contains("root answer"));
}

#[tokio::test]
async fn test_failed_child_wakes_parent_with_failure() {
    let (registry, store, handler, wake) = wake_fixture().await;
    create_parent_row(&store, "running").await;

    let child_id = register_child_run(&registry, "delegation:parent-run").await;

    // No agent configured → the child fails; the failure must still wake
    // the parent so it can decide to retry or re-delegate.
    execute_child_task(
        child_id.clone(),
        completion_task(),
        ChildTaskEnv {
            tracker: DelegationTracker::new(2),
            iterations: Arc::new(AtomicUsize::new(0)),
            agent: None,
            registry: registry.clone(),
            store: Some(store.clone()),
            scope: child_scope(&child_id, Some("parent-run".to_string()), 2),
            agent_id: "worker".to_string(),
            coordinator: None,
            wake: Some(wake.clone()),
            parent_session: None,
        },
    )
    .await;

    wait_for_wake(&handler, 1).await;
    let wakes = handler
        .wakes
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone();
    assert_eq!(wakes.len(), 1);
    assert_eq!(wakes[0].0, "delegation:parent-run");
    assert!(wakes[0].1.contains(&child_id));
    assert!(wakes[0].1.contains("failed"));
}
