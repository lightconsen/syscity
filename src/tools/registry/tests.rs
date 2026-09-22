//! Tests for the tool registry: execution, policy gate and streaming paths.

use std::sync::atomic::{AtomicBool, Ordering};

use super::*;
use crate::tools::sdk::ToolCapabilities;
use crate::tools::PermissionsConfig;

/// A tool whose only job is to declare retry semantics.
struct DeclaredTool {
    name: String,
    caps: ToolCapabilities,
}

#[async_trait::async_trait]
impl Tool for DeclaredTool {
    fn name(&self) -> &str {
        &self.name
    }
    fn description(&self) -> &str {
        "declares retry semantics"
    }
    fn parameters_schema(&self) -> Value {
        serde_json::json!({ "type": "object" })
    }
    fn capabilities(&self) -> ToolCapabilities {
        self.caps.clone()
    }
    async fn execute(
        &self,
        _args: Value,
        _ctx: &ToolContext,
    ) -> crate::Result<ToolExecutionResult> {
        Ok(ToolExecutionResult::success("ok"))
    }
}

/// The retry declaration has to be honest in the careful direction, and the
/// timeout path is where it is quoted: a caller that has just lost a tool to
/// a timeout is deciding whether to try again.
#[test]
fn retry_safety_is_declared_and_quoted_on_timeout() {
    // The default is the careful answer: a tool that says nothing is
    // assumed not to be safely repeatable.
    let default = ToolCapabilities::default();
    assert!(!default.idempotent);
    assert!(default.compensation.is_none());

    let mut registry = ToolRegistry::new();
    registry.register(Box::new(DeclaredTool {
        name: "repeatable".into(),
        caps: ToolCapabilities {
            idempotent: true,
            ..Default::default()
        },
    }));
    registry.register(Box::new(DeclaredTool {
        name: "undoable".into(),
        caps: ToolCapabilities {
            compensation: Some("delete the thing it made"),
            ..Default::default()
        },
    }));
    registry.register(Box::new(DeclaredTool {
        name: "one_way".into(),
        caps: ToolCapabilities::default(),
    }));

    assert!(registry.uncertainty_note("repeatable").contains("safe"));
    let undoable = registry.uncertainty_note("undoable");
    assert!(
        undoable.contains("delete the thing it made"),
        "the compensating action is what the caller needs: {undoable}"
    );
    let one_way = registry.uncertainty_note("one_way");
    assert!(
        one_way.contains("duplicate"),
        "a tool with no way back has to say so: {one_way}"
    );
}

/// The tools that act on the world declare it, so the answer does not
/// depend on a caller's guess.
#[test]
fn the_consequential_tools_declare_their_retry_safety() {
    // A read is repeatable.
    assert!(
        crate::tools::grep::GrepTool::new()
            .capabilities()
            .idempotent
    );
    // A command is not, and has nothing to compensate with.
    let shell = crate::tools::shell::ShellTool::default().capabilities();
    assert!(!shell.idempotent);
    assert!(shell.compensation.is_none());
}

/// A minimal tool that records whether its body actually ran.
struct SpyTool {
    name: &'static str,
    ran: Arc<AtomicBool>,
    requires_approval: bool,
}

#[async_trait::async_trait]
impl Tool for SpyTool {
    fn name(&self) -> &str {
        self.name
    }
    fn description(&self) -> &str {
        "spy tool"
    }
    fn parameters_schema(&self) -> Value {
        serde_json::json!({})
    }
    async fn execute(
        &self,
        _args: Value,
        _ctx: &ToolContext,
    ) -> crate::Result<ToolExecutionResult> {
        self.ran.store(true, Ordering::SeqCst);
        Ok(ToolExecutionResult::success(format!("{} ran", self.name)))
    }
    fn capabilities(&self) -> crate::tools::sdk::ToolCapabilities {
        crate::tools::sdk::ToolCapabilities {
            requires_approval: self.requires_approval,
            ..Default::default()
        }
    }
}

fn spy(name: &'static str, ran: Arc<AtomicBool>) -> Box<dyn Tool> {
    Box::new(SpyTool {
        name,
        ran,
        requires_approval: false,
    })
}

fn approval_spy(name: &'static str, ran: Arc<AtomicBool>) -> Box<dyn Tool> {
    Box::new(SpyTool {
        name,
        ran,
        requires_approval: true,
    })
}

fn call(name: &str) -> FunctionCall {
    FunctionCall {
        name: name.to_string(),
        arguments: "{}".to_string(),
    }
}

// ── permission gate tests ─────────────────────────────────────────────

fn shell_call(command: &str) -> FunctionCall {
    FunctionCall {
        name: "spy".to_string(),
        arguments: serde_json::json!({ "command": command }).to_string(),
    }
}

fn perms(mode: PermissionMode, allow_bypass: bool) -> Arc<PermissionsRuntime> {
    Arc::new(PermissionsRuntime::from_config(&PermissionsConfig {
        mode,
        allow_bypass,
        ..Default::default()
    }))
}

fn read_only_declared(name: &'static str) -> Box<dyn Tool> {
    Box::new(DeclaredTool {
        name: name.to_string(),
        caps: ToolCapabilities {
            read_only: true,
            ..Default::default()
        },
    })
}

#[tokio::test]
async fn a_deny_rule_blocks_before_hooks_and_the_tool_body() {
    let ran = Arc::new(AtomicBool::new(false));
    let mut registry = ToolRegistry::new().with_permissions(perms(PermissionMode::Default, false));
    registry.register(spy("spy", ran.clone()));
    registry.permissions().reload(&PermissionsConfig {
        deny: vec!["spy".to_string()],
        ..Default::default()
    });

    // Even a hook that would deny "more loudly" must not matter: the
    // engine blocks first.
    registry.set_hooks(
        ToolHooks::new()
            .policy(|_, _, _| async { ToolPolicyDecision::Deny { reason: "hook denial".into() } }),
    );

    let err = registry
        .execute_call(&call("spy"), &ToolContext::new("u", "conv1"))
        .await
        .expect_err("deny rule blocks");
    assert!(err.to_string().contains("deny rule"), "got {err}");
    assert!(!ran.load(Ordering::SeqCst), "the tool body must not run");
}

#[tokio::test]
async fn an_allow_rule_suppresses_the_requires_approval_fallback() {
    let ran = Arc::new(AtomicBool::new(false));
    let approval_queue = Arc::new(ApprovalQueue::new());
    let mut registry = ToolRegistry::new()
        .with_approval_queue(approval_queue.clone())
        .with_permissions(perms(PermissionMode::Default, false));
    registry.register(approval_spy("spy", ran.clone()));
    registry.permissions().reload(&PermissionsConfig {
        allow: vec!["spy".to_string()],
        ..Default::default()
    });

    let result = registry
        .execute_call(&call("spy"), &ToolContext::new("u", "conv1"))
        .await
        .expect("allowed call executes without approval");
    assert!(result.success);
    assert!(ran.load(Ordering::SeqCst));
    assert!(
        approval_queue.is_empty().await,
        "an allow rule is the don't-ask-again: nothing submitted"
    );
}

#[tokio::test]
async fn an_ask_rule_routes_to_the_approval_queue() {
    let ran = Arc::new(AtomicBool::new(false));
    let approval_queue = Arc::new(ApprovalQueue::new());
    let queue = approval_queue.clone();
    let mut registry = ToolRegistry::new()
        .with_approval_queue(approval_queue.clone())
        .with_permissions(perms(PermissionMode::Default, false));
    registry.register(spy("spy", ran.clone()));
    registry.permissions().reload(&PermissionsConfig {
        ask: vec!["spy".to_string()],
        ..Default::default()
    });

    let mut rx = approval_queue.event_tx.subscribe();
    let approver = tokio::spawn(async move {
        let event = rx.recv().await.expect("approval event");
        queue
            .resolve(&event.approval_id, ApprovalDecision::Approve)
            .await;
    });

    let result = registry
        .execute_call(&call("spy"), &ToolContext::new("u", "conv1"))
        .await
        .expect("ask rule suspends, then approval releases");
    assert!(result.success);
    assert!(ran.load(Ordering::SeqCst));
    approver.await.expect("approver task");
}

#[tokio::test]
async fn plan_mode_hides_and_refuses_everything_not_read_only() {
    let ran = Arc::new(AtomicBool::new(false));
    let mut registry = ToolRegistry::new().with_permissions(perms(PermissionMode::Plan, false));
    registry.register(spy("mutator", ran.clone()));
    registry.register(read_only_declared("reader"));

    let ctx = ToolContext::new("u", "conv1");

    // Advertisement: the mutator is gone from the offered toolset.
    let offered: Vec<String> = registry
        .get_available(&ctx)
        .into_iter()
        .map(|d| d.name)
        .collect();
    assert!(!offered.contains(&"mutator".to_string()), "got {offered:?}");
    assert!(offered.contains(&"reader".to_string()));

    // Backstop: a call that slips through is denied with the plan message.
    let err = registry
        .execute_call(&call("mutator"), &ctx)
        .await
        .expect_err("plan mode denies a mutating call");
    assert!(err.to_string().contains("plan mode"), "got {err}");
    assert!(!ran.load(Ordering::SeqCst));

    // And the read-only tool still runs.
    let result = registry
        .execute_call(&call("reader"), &ctx)
        .await
        .expect("read-only tools run in plan mode");
    assert!(result.success);
}

#[tokio::test]
async fn accept_edits_pre_approves_write_category_tools() {
    let ran = Arc::new(AtomicBool::new(false));
    let approval_queue = Arc::new(ApprovalQueue::new());
    let mut registry = ToolRegistry::new()
        .with_approval_queue(approval_queue.clone())
        .with_permissions(perms(PermissionMode::AcceptEdits, false));
    registry.register(Box::new(DeclaredTool {
        name: "writer_declared".to_string(),
        caps: ToolCapabilities {
            requires_approval: true,
            categories: vec!["file".to_string(), "write".to_string()],
            ..Default::default()
        },
    }));

    let ctx = ToolContext::new("u", "conv1");
    let result = registry
        .execute_call(&call("writer_declared"), &ctx)
        .await
        .expect("accept_edits pre-approves write-category tools");
    assert!(result.success);
    assert!(approval_queue.is_empty().await, "nothing submitted under accept_edits");
}

#[tokio::test]
async fn bypass_needs_the_config_flag_and_hooks_still_win() {
    let ran = Arc::new(AtomicBool::new(false));
    let approval_queue = Arc::new(ApprovalQueue::new());
    // allow_bypass = false: bypass behaves as default.
    let runtime = Arc::new(PermissionsRuntime::from_config(&PermissionsConfig {
        mode: PermissionMode::Bypass,
        allow_bypass: false,
        ..Default::default()
    }));
    runtime.set_session_mode("conv1", Some(PermissionMode::Bypass));
    let mut registry = ToolRegistry::new()
        .with_approval_queue(approval_queue.clone())
        .with_permissions(runtime);
    registry.register(approval_spy("spy", ran.clone()));
    let ctx = ToolContext::new("u", "conv1")
        .with_ask_queue(Arc::new(crate::tools::ask_user::AskQueue::new()));

    let mut rx = approval_queue.event_tx.subscribe();
    let queue = approval_queue.clone();
    let approver = tokio::spawn(async move {
        let event = rx.recv().await.expect("fallback still asks");
        queue
            .resolve(&event.approval_id, ApprovalDecision::Approve)
            .await;
    });
    let result = registry
        .execute_call(&call("spy"), &ctx)
        .await
        .expect("without allow_bypass, bypass == default: approval flow runs");
    assert!(result.success);
    approver.await.expect("approver task");

    // Flip the flag: now the same call runs without any approval.
    registry.permissions().reload(&PermissionsConfig {
        mode: PermissionMode::Bypass,
        allow_bypass: true,
        ..Default::default()
    });
    let result = registry
        .execute_call(&call("spy"), &ctx)
        .await
        .expect("bypass skips the approval flow once allowed");
    assert!(result.success);
    assert!(approval_queue.is_empty().await, "bypass must not submit approvals");

    // But an explicit hook Deny still blocks under bypass.
    registry.set_hooks(
        ToolHooks::new()
            .policy(|_, _, _| async { ToolPolicyDecision::Deny { reason: "hook denial".into() } }),
    );
    let err = registry
        .execute_call(&call("spy"), &ctx)
        .await
        .expect_err("hook deny wins under bypass");
    assert!(err.to_string().contains("hook denial"), "got {err}");
}

#[tokio::test]
async fn a_session_mode_override_drives_the_gate() {
    let ran = Arc::new(AtomicBool::new(false));
    // Gateway default plan; this session overridden to default mode.
    let runtime = Arc::new(PermissionsRuntime::from_config(&PermissionsConfig {
        mode: PermissionMode::Plan,
        ..Default::default()
    }));
    runtime.set_session_mode("conv1", Some(PermissionMode::Default));
    let mut registry = ToolRegistry::new().with_permissions(runtime);
    registry.register(spy("mutator", ran.clone()));

    let result = registry
        .execute_call(&call("mutator"), &ToolContext::new("u", "conv1"))
        .await
        .expect("the session override exits plan mode for this session");
    assert!(result.success);
}

/// A `Deny` from a policy hook must block a buffered `execute_call`
/// before the tool body runs.
#[tokio::test]
async fn test_execute_call_runs_policy_and_blocks() {
    let ran = Arc::new(AtomicBool::new(false));
    let mut registry =
        ToolRegistry::new().with_hooks(ToolHooks::new().policy(|name, _args, _ctx| {
            let name = name.to_string();
            async move {
                if name == "spy" {
                    ToolPolicyDecision::Deny {
                        reason: "blocked-by-policy".into(),
                    }
                } else {
                    ToolPolicyDecision::Allow
                }
            }
        }));
    registry.register(spy("spy", ran.clone()));

    let err = registry
        .execute_call(&call("spy"), &ToolContext::default())
        .await
        .expect_err("should be denied");
    assert!(err.to_string().contains("blocked-by-policy"), "err: {}", err);
    assert!(!ran.load(Ordering::SeqCst), "tool body must not run when denied");
}

/// A `requires_approval` tool with no policy hooks and nobody to ask runs
/// ungated through `execute_call`.
///
/// This is the half that keeps unattended work moving: with no channel to
/// put the question on, a submitted prompt could only be waited on for five
/// minutes and then fail. The other half is
/// [`test_execute_call_requires_approval_asks_when_someone_can_answer`].
#[tokio::test]
async fn test_execute_call_requires_approval_without_a_human_runs() {
    let ran = Arc::new(AtomicBool::new(false));
    let mut registry = ToolRegistry::new(); // no hooks
    registry.register(approval_spy("spy", ran.clone()));

    let result = registry
        .execute_call(&call("spy"), &ToolContext::default())
        .await
        .expect("requires_approval tool must run ungated through execute_call");
    assert!(result.success);
    assert!(ran.load(Ordering::SeqCst));
}

/// The same tool, in a context that has a question channel but nobody on the
/// other end of it, still runs ungated — and submits nothing.
///
/// A cron run, a heartbeat or a delegation carries an `ask_queue` in some
/// wiring but is explicitly not an interactive human; asking there would
/// stall the very work that has no one to answer.
#[tokio::test]
async fn test_execute_call_requires_approval_runs_ungated_in_a_background_context() {
    let ran = Arc::new(AtomicBool::new(false));
    let approval_queue = Arc::new(ApprovalQueue::new());
    let mut registry = ToolRegistry::new().with_approval_queue(approval_queue.clone());
    registry.register(approval_spy("spy", ran.clone()));

    let ctx = ToolContext::new("system", "cron:job-1")
        .with_ask_queue(Arc::new(crate::tools::ask_user::AskQueue::new()));

    // A five-second cap: an ungated call returns at once, and a regression
    // that submits instead would otherwise sit on the five-minute approval
    // timeout and look like a hang.
    let result =
        tokio::time::timeout(Duration::from_secs(5), registry.execute_call(&call("spy"), &ctx))
            .await
            .expect("a background context must not wait on an approval")
            .expect("requires_approval tool must run ungated where nobody can answer");

    assert!(result.success);
    assert!(ran.load(Ordering::SeqCst));
    assert!(
        approval_queue.is_empty().await,
        "no approval should have been submitted for a background context"
    );
}

/// With both a question channel and an interactive context, the same tool is
/// gated: the call reaches the approval queue and only runs once approved.
#[tokio::test]
async fn test_execute_call_requires_approval_asks_when_someone_can_answer() {
    let ran = Arc::new(AtomicBool::new(false));
    let approval_queue = Arc::new(ApprovalQueue::new());
    let mut registry = ToolRegistry::new().with_approval_queue(approval_queue.clone());
    registry.register(approval_spy("spy", ran.clone()));

    let ctx = ToolContext::new("user1", "conv1")
        .with_ask_queue(Arc::new(crate::tools::ask_user::AskQueue::new()));

    // Approve the first request the way a UI does.
    let mut rx = approval_queue.event_tx.subscribe();
    let queue = approval_queue.clone();
    let approver = tokio::spawn(async move {
        let event = rx.recv().await.expect("approval event");
        queue
            .resolve(&event.approval_id, ApprovalDecision::Approve)
            .await;
    });

    let result = registry
        .execute_call(&call("spy"), &ctx)
        .await
        .expect("approved call should execute");
    assert!(result.success);
    assert!(ran.load(Ordering::SeqCst), "the tool must run once the approval is given");
    approver.await.expect("approver task");
}

/// `NeedsApproval` from a policy hook must delegate to the full
/// `execute()` approval flow: a submitted request gets approved and the
/// tool runs.
#[tokio::test]
async fn test_execute_call_needs_approval_delegates_to_approval_queue() {
    let ran = Arc::new(AtomicBool::new(false));
    let approval_queue = Arc::new(ApprovalQueue::new());
    let queue = approval_queue.clone();
    let mut registry = ToolRegistry::new()
        .with_approval_queue(approval_queue.clone())
        .with_hooks(ToolHooks::new().policy(|name, _args, _ctx| {
            let name = name.to_string();
            async move {
                if name == "spy" {
                    ToolPolicyDecision::NeedsApproval {
                        approval_id: "req-1".into(),
                        tool_name: name,
                        args: serde_json::json!({}),
                        risk_level: crate::tools::approval::RiskLevel::High,
                        requested_by: "user1".into(),
                        message: "needs approval".into(),
                    }
                } else {
                    ToolPolicyDecision::Allow
                }
            }
        }));
    registry.register(spy("spy", ran.clone()));

    // Auto-approve the first submitted request.
    let mut rx = approval_queue.event_tx.subscribe();
    let approver = tokio::spawn(async move {
        let event = rx.recv().await.expect("approval event");
        queue
            .resolve(&event.approval_id, ApprovalDecision::Approve)
            .await;
    });

    let result = registry
        .execute_call(&call("spy"), &ToolContext::default())
        .await
        .expect("approved call should execute");
    assert!(result.success);
    assert!(ran.load(Ordering::SeqCst), "tool must run after approval");
    approver.await.expect("approver task");
}

/// `NeedsApproval` with no approval queue must surface `execute()`'s
/// error — proving the call delegated to the full approval flow rather
/// than the ungated path.
#[tokio::test]
async fn test_execute_call_needs_approval_no_queue_errors() {
    let ran = Arc::new(AtomicBool::new(false));
    let mut registry =
        ToolRegistry::new().with_hooks(ToolHooks::new().policy(|_name, _args, _ctx| async {
            ToolPolicyDecision::NeedsApproval {
                approval_id: "req-1".into(),
                tool_name: "spy".into(),
                args: serde_json::json!({}),
                risk_level: crate::tools::approval::RiskLevel::High,
                requested_by: "user1".into(),
                message: "needs approval".into(),
            }
        }));
    registry.register(spy("spy", ran.clone()));

    let err = registry
        .execute_call(&call("spy"), &ToolContext::default())
        .await
        .expect_err("must delegate to execute() which fails without a queue");
    assert!(err.to_string().contains("no approval queue"), "err: {}", err);
    assert!(!ran.load(Ordering::SeqCst), "tool must not run");
}

/// A description override must replace the static description in every
/// emitted `FunctionDefinition` (§十一).
#[test]
fn description_override_replaces_static_description() {
    let ran = Arc::new(AtomicBool::new(false));
    let mut registry = ToolRegistry::new();
    registry.register(spy("spy", ran));

    assert_eq!(registry.get_definitions()[0].description, "spy tool");

    registry.set_metadata(
        "spy",
        crate::tools::metadata::ToolDescriptionMeta::new(
            1,
            "renamed: reports what the spy tool does",
        ),
    );
    let def = registry
        .get_definitions()
        .into_iter()
        .find(|d| d.name == "spy")
        .expect("spy tool present");
    assert_eq!(def.description, "renamed: reports what the spy tool does");

    // `get_available` honors the override too.
    let ctx = ToolContext::default();
    let avail = registry
        .get_available(&ctx)
        .into_iter()
        .find(|d| d.name == "spy")
        .expect("spy tool available");
    assert_eq!(avail.description, "renamed: reports what the spy tool does");
}
