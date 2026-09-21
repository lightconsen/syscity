//! Tool execution hooks
//!
//! Provides pre- and post-execution hooks for tools, enabling audit logging,
//! permission checks, metrics collection, and result caching at the call site.
//!
//! ## Policy hooks
//!
//! In addition to fire-and-forget before/after hooks, a *policy hook* can
//! **allow or deny** a tool call before it executes:
//!
//! ```rust,no_run
//! use syscity::tools::hooks::{ToolHooks, ToolPolicyDecision};
//!
//! let hooks = ToolHooks::new().policy(|name, args, _ctx| {
//!     let name = name.to_string();
//!     Box::pin(async move {
//!         if name == "shell" {
//!             ToolPolicyDecision::Deny {
//!                 reason: "shell tool is disabled".into(),
//!             }
//!         } else {
//!             ToolPolicyDecision::Allow
//!         }
//!     })
//! });
//! ```

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use serde_json::Value;

use super::{RiskLevel, ToolContext, ToolExecutionResult};

// ── Policy decision
// ───────────────────────────────────────────────────────────

/// The outcome of a policy hook evaluation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ToolPolicyDecision {
    /// The tool call is permitted — continue with execution.
    Allow,
    /// The tool call is denied.
    Deny {
        /// Human-readable reason returned to the caller.
        reason: String,
    },
    /// High-risk tool call requires human approval before execution.
    /// The tool execution will suspend until approved or denied.
    NeedsApproval {
        /// Unique approval request ID
        approval_id: String,
        /// Name of the tool being requested
        tool_name: String,
        /// Arguments that will be passed to the tool
        args: Value,
        /// Risk level assessment
        risk_level: RiskLevel,
        /// User or agent that requested the tool
        requested_by: String,
        /// Human-readable explanation for why approval is needed
        message: String,
    },
}

impl ToolPolicyDecision {
    /// Return `true` if this decision allows the call immediately.
    pub fn is_allow(&self) -> bool {
        matches!(self, ToolPolicyDecision::Allow)
    }

    /// Return `true` if this decision requires human approval.
    pub fn is_needs_approval(&self) -> bool {
        matches!(self, ToolPolicyDecision::NeedsApproval { .. })
    }

    /// Return `true` if this decision denies the call.
    pub fn is_deny(&self) -> bool {
        matches!(self, ToolPolicyDecision::Deny { .. })
    }
}

// ── Post-execute decision
// ───────────────────────────────────────────────────────────

/// The decision a post-execute hook makes for a finished tool call.
///
/// Post-execute hooks run after the tool body completes and before content
/// filtering / audit. Unlike [`ToolPolicyDecision`] (which gates *whether*
/// a call runs), a post-execute decision governs *what the model gets to
/// see* of the result — it can replace the output, or confiscate the result
/// with corrective feedback so the model self-corrects on its next request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PostExecuteDecision {
    /// Keep the result unchanged.
    Accept,
    /// Replace the model-facing output; success flag, error, and the
    /// structured `data` side-channel are preserved.
    ReplaceOutput(String),
    /// Confiscate the result: it becomes an error whose message IS the
    /// feedback. The original output, error, and `data` are discarded
    /// entirely — a blocked result cannot smuggle content through any
    /// side channel.
    Block(String),
}

/// A boxed async function called after a tool executes, with the power to
/// replace or block the result before it goes back to the model.
///
/// Receives the tool name, the call arguments, the original (read-only)
/// result, and the upstream decision from hooks registered earlier. Hooks
/// run in registration order as a chain: each hook's returned decision
/// becomes the next hook's upstream; the chain starts from
/// [`PostExecuteDecision::Accept`].
pub type PostExecuteHookFn = Arc<
    dyn Fn(
            &str,
            &Value,
            &ToolExecutionResult,
            &ToolContext,
            PostExecuteDecision,
        ) -> Pin<Box<dyn Future<Output = PostExecuteDecision> + Send>>
        + Send
        + Sync,
>;

// ── Hook type aliases
// ─────────────────────────────────────────────────────────

/// A boxed async function called before a tool executes.
///
/// Receives the tool name and the arguments that will be passed to the tool.
pub type BeforeHookFn =
    Arc<dyn Fn(&str, &Value) -> Pin<Box<dyn Future<Output = ()> + Send>> + Send + Sync>;

/// A boxed async function called after a tool executes.
///
/// Receives the tool name, the original arguments, and the execution result.
pub type AfterHookFn = Arc<
    dyn Fn(&str, &Value, &ToolExecutionResult) -> Pin<Box<dyn Future<Output = ()> + Send>>
        + Send
        + Sync,
>;

/// A boxed async policy function called before a tool executes.
///
/// Returns a [`ToolPolicyDecision`] that can block the tool call.  All
/// registered policy hooks are evaluated in registration order; the first
/// `Deny` short-circuits further evaluation.
pub type PolicyHookFn = Arc<
    dyn Fn(&str, &Value, &ToolContext) -> Pin<Box<dyn Future<Output = ToolPolicyDecision> + Send>>
        + Send
        + Sync,
>;

/// A collection of before/after/policy hooks for tool execution.
///
/// Hooks are opt-in and layered: all registered hooks run in registration
/// order.
///
/// # Example
///
/// ```rust,no_run
/// use syscity::tools::hooks::ToolHooks;
///
/// let hooks = ToolHooks::new()
///     .before(|name, args| {
///         let name = name.to_string();
///         let args = args.to_string(); // stringify before entering the async block
///         Box::pin(async move {
///             tracing::info!("Calling tool: {} with args: {}", name, args);
///         })
///     })
///     .after(|name, _args, result| {
///         let name = name.to_string();
///         let success = result.success;
///         Box::pin(async move {
///             tracing::info!("Tool {} completed, success={}", name, success);
///         })
///     });
/// ```
#[derive(Default, Clone)]
pub struct ToolHooks {
    before_call: Vec<BeforeHookFn>,
    after_call: Vec<AfterHookFn>,
    policy_hooks: Vec<PolicyHookFn>,
    post_execute_hooks: Vec<PostExecuteHookFn>,
}

impl std::fmt::Debug for ToolHooks {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ToolHooks")
            .field("before_hooks", &self.before_call.len())
            .field("after_hooks", &self.after_call.len())
            .field("policy_hooks", &self.policy_hooks.len())
            .field("post_execute_hooks", &self.post_execute_hooks.len())
            .finish()
    }
}

impl ToolHooks {
    /// Create a new empty `ToolHooks`.
    pub fn new() -> Self {
        Self::default()
    }

    /// Add a hook to run before tool execution.
    ///
    /// The hook receives the tool name and the call arguments.
    pub fn before<F, Fut>(mut self, f: F) -> Self
    where
        F: Fn(&str, &Value) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = ()> + Send + 'static,
    {
        self.before_call.push(Arc::new(move |name, args| {
            Box::pin(f(name, args)) as Pin<Box<dyn Future<Output = ()> + Send>>
        }));
        self
    }

    /// Add a hook to run after tool execution.
    ///
    /// The hook receives the tool name, the original arguments, and the result.
    pub fn after<F, Fut>(mut self, f: F) -> Self
    where
        F: Fn(&str, &Value, &ToolExecutionResult) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = ()> + Send + 'static,
    {
        self.after_call.push(Arc::new(move |name, args, result| {
            Box::pin(f(name, args, result)) as Pin<Box<dyn Future<Output = ()> + Send>>
        }));
        self
    }

    /// Add a policy hook that can allow or deny a tool call.
    ///
    /// Policy hooks run before before-hooks and before tool execution.
    /// The first hook that returns `Deny` short-circuits evaluation.
    pub fn policy<F, Fut>(mut self, f: F) -> Self
    where
        F: Fn(&str, &Value, &ToolContext) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = ToolPolicyDecision> + Send + 'static,
    {
        self.policy_hooks.push(Arc::new(move |name, args, ctx| {
            Box::pin(f(name, args, ctx)) as Pin<Box<dyn Future<Output = ToolPolicyDecision> + Send>>
        }));
        self
    }

    /// Add a post-execute hook that can replace or block a tool result.
    ///
    /// The hook receives the tool name, the call arguments, the original
    /// (read-only) result, and the upstream decision. Returning
    /// [`PostExecuteDecision::Block`] confiscates the result: the model sees
    /// an error whose content is the feedback, and the original output is
    /// discarded.
    pub fn post_execute<F, Fut>(mut self, f: F) -> Self
    where
        F: Fn(&str, &Value, &ToolExecutionResult, &ToolContext, PostExecuteDecision) -> Fut
            + Send
            + Sync
            + 'static,
        Fut: Future<Output = PostExecuteDecision> + Send + 'static,
    {
        self.post_execute_hooks
            .push(Arc::new(move |name, args, result, ctx, decision| {
                Box::pin(f(name, args, result, ctx, decision))
                    as Pin<Box<dyn Future<Output = PostExecuteDecision> + Send>>
            }));
        self
    }

    /// Returns `true` if no hooks are registered.
    pub fn is_empty(&self) -> bool {
        self.before_call.is_empty()
            && self.after_call.is_empty()
            && self.policy_hooks.is_empty()
            && self.post_execute_hooks.is_empty()
    }

    /// Returns `true` if at least one policy hook is registered.
    pub fn has_policy_hooks(&self) -> bool {
        !self.policy_hooks.is_empty()
    }

    /// Returns `true` if at least one post-execute hook is registered.
    pub fn has_post_execute_hooks(&self) -> bool {
        !self.post_execute_hooks.is_empty()
    }

    /// Run all registered policy hooks for the given tool call.
    ///
    /// Returns `Allow` if all hooks allow, or the first `Deny` or
    /// `NeedsApproval` encountered.
    pub async fn run_policy(
        &self,
        name: &str,
        args: &Value,
        ctx: &ToolContext,
    ) -> ToolPolicyDecision {
        for hook in &self.policy_hooks {
            let decision = hook(name, args, ctx).await;
            if !decision.is_allow() {
                return decision;
            }
        }
        ToolPolicyDecision::Allow
    }

    /// Run all registered before-hooks for the given tool call.
    pub async fn run_before(&self, name: &str, args: &Value) {
        for hook in &self.before_call {
            hook(name, args).await;
        }
    }

    /// Run all registered after-hooks for the given tool call.
    pub async fn run_after(&self, name: &str, args: &Value, result: &ToolExecutionResult) {
        for hook in &self.after_call {
            hook(name, args, result).await;
        }
    }

    /// Run the post-execute decision chain for a finished tool call.
    ///
    /// Hooks run in registration order; each receives the original result
    /// (read-only) and the upstream decision, and its returned decision
    /// becomes the next hook's upstream. A panicking hook fails closed: the
    /// result is confiscated with a generic feedback message rather than
    /// passed through uninspected (the panic payload is logged, never
    /// forwarded to the model).
    pub async fn run_post_execute(
        &self,
        name: &str,
        args: &Value,
        result: &ToolExecutionResult,
        ctx: &ToolContext,
    ) -> PostExecuteDecision {
        use futures::FutureExt;

        let mut decision = PostExecuteDecision::Accept;
        for hook in &self.post_execute_hooks {
            let pending = std::panic::AssertUnwindSafe(hook(name, args, result, ctx, decision));
            match pending.catch_unwind().await {
                Ok(d) => decision = d,
                Err(payload) => {
                    let detail = payload
                        .downcast_ref::<&str>()
                        .map(|s| (*s).to_string())
                        .or_else(|| payload.downcast_ref::<String>().cloned())
                        .unwrap_or_else(|| "unknown panic".to_string());
                    tracing::error!("post-execute hook panicked for tool '{}': {}", name, detail);
                    decision = PostExecuteDecision::Block(
                        "Tool result withheld: a post-execute policy hook failed.".to_string(),
                    );
                }
            }
        }
        decision
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;

    #[tokio::test]
    async fn test_before_hook_called() {
        let counter = Arc::new(AtomicUsize::new(0));
        let c = Arc::clone(&counter);

        let hooks = ToolHooks::new().before(move |_name, _args| {
            let c = Arc::clone(&c);
            async move {
                c.fetch_add(1, Ordering::SeqCst);
            }
        });

        hooks.run_before("shell", &serde_json::json!({})).await;
        assert_eq!(counter.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn test_after_hook_called() {
        let counter = Arc::new(AtomicUsize::new(0));
        let c = Arc::clone(&counter);

        let hooks = ToolHooks::new().after(move |_name, _args, _result| {
            let c = Arc::clone(&c);
            async move {
                c.fetch_add(1, Ordering::SeqCst);
            }
        });

        let result = ToolExecutionResult::success("ok".to_string());
        hooks
            .run_after("shell", &serde_json::json!({}), &result)
            .await;
        assert_eq!(counter.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn test_multiple_hooks_run_in_order() {
        let log: Arc<tokio::sync::Mutex<Vec<u32>>> = Arc::new(tokio::sync::Mutex::new(Vec::new()));

        let l1 = Arc::clone(&log);
        let l2 = Arc::clone(&log);

        let hooks = ToolHooks::new()
            .before(move |_, _| {
                let l = Arc::clone(&l1);
                async move {
                    l.lock().await.push(1);
                }
            })
            .before(move |_, _| {
                let l = Arc::clone(&l2);
                async move {
                    l.lock().await.push(2);
                }
            });

        hooks.run_before("tool", &serde_json::json!({})).await;
        let order = log.lock().await.clone();
        assert_eq!(order, vec![1, 2]);
    }

    #[test]
    fn test_is_empty() {
        let hooks = ToolHooks::new();
        assert!(hooks.is_empty());

        let hooks = hooks.before(|_, _| async {});
        assert!(!hooks.is_empty());
    }

    #[tokio::test]
    async fn test_policy_hook_allow() {
        let hooks =
            ToolHooks::new().policy(|_name, _args, _ctx| async { ToolPolicyDecision::Allow });

        let decision = hooks
            .run_policy("shell", &serde_json::json!({}), &ToolContext::default())
            .await;
        assert_eq!(decision, ToolPolicyDecision::Allow);
    }

    #[tokio::test]
    async fn test_policy_hook_deny() {
        let hooks = ToolHooks::new().policy(|name, _args, _ctx| {
            let name = name.to_string();
            async move {
                if name == "shell" {
                    ToolPolicyDecision::Deny {
                        reason: "shell disabled".into(),
                    }
                } else {
                    ToolPolicyDecision::Allow
                }
            }
        });

        let decision = hooks
            .run_policy("shell", &serde_json::json!({}), &ToolContext::default())
            .await;
        assert_eq!(
            decision,
            ToolPolicyDecision::Deny {
                reason: "shell disabled".into()
            }
        );

        let decision = hooks
            .run_policy("memory", &serde_json::json!({}), &ToolContext::default())
            .await;
        assert_eq!(decision, ToolPolicyDecision::Allow);
    }

    #[tokio::test]
    async fn test_policy_short_circuits_on_first_deny() {
        let counter = Arc::new(AtomicUsize::new(0));
        let c = Arc::clone(&counter);

        let hooks = ToolHooks::new()
            .policy(|_, _, _| async { ToolPolicyDecision::Deny { reason: "first".into() } })
            .policy(move |_, _, _| {
                let c = Arc::clone(&c);
                async move {
                    c.fetch_add(1, Ordering::SeqCst);
                    ToolPolicyDecision::Allow
                }
            });

        let decision = hooks
            .run_policy("any", &serde_json::json!({}), &ToolContext::default())
            .await;
        assert_eq!(decision, ToolPolicyDecision::Deny { reason: "first".into() });
        // Second hook should not have run.
        assert_eq!(counter.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn test_no_policy_hooks_returns_allow() {
        let hooks = ToolHooks::new();
        let decision = hooks
            .run_policy("any", &serde_json::json!({}), &ToolContext::default())
            .await;
        assert_eq!(decision, ToolPolicyDecision::Allow);
    }

    // ── Post-execute decision chain ──────────────────────────────────────

    #[tokio::test]
    async fn test_post_execute_no_hooks_accepts() {
        let hooks = ToolHooks::new();
        let result = ToolExecutionResult::success("ok".to_string());
        let decision = hooks
            .run_post_execute("read", &serde_json::json!({}), &result, &ToolContext::default())
            .await;
        assert_eq!(decision, PostExecuteDecision::Accept);
    }

    #[tokio::test]
    async fn test_post_execute_chain_accumulates_decisions() {
        // Hook 1 upgrades Accept → ReplaceOutput; hook 2 must observe the
        // upstream decision and escalates to Block.
        let hooks = ToolHooks::new()
            .post_execute(|_, _, _, _ctx, upstream| async move {
                assert_eq!(upstream, PostExecuteDecision::Accept);
                PostExecuteDecision::ReplaceOutput("replaced".to_string())
            })
            .post_execute(|_, _, _, _ctx, upstream| async move {
                assert_eq!(upstream, PostExecuteDecision::ReplaceOutput("replaced".to_string()));
                PostExecuteDecision::Block("confiscated".to_string())
            });

        let result = ToolExecutionResult::success("original".to_string());
        let decision = hooks
            .run_post_execute("read", &serde_json::json!({}), &result, &ToolContext::default())
            .await;
        assert_eq!(decision, PostExecuteDecision::Block("confiscated".to_string()));
    }

    #[tokio::test]
    async fn test_post_execute_hook_panic_fails_closed() {
        let hooks = ToolHooks::new().post_execute(|_, _, _, _ctx, _| async move {
            panic!("boom");
        });

        let result = ToolExecutionResult::success("sensitive".to_string());
        let decision = hooks
            .run_post_execute("read", &serde_json::json!({}), &result, &ToolContext::default())
            .await;
        // A panicking policy hook must never pass the result through.
        match decision {
            PostExecuteDecision::Block(feedback) => {
                assert!(!feedback.contains("boom"), "panic payload must not leak");
                assert!(feedback.contains("post-execute policy hook failed"));
            }
            other => panic!("expected Block, got {:?}", other),
        }
    }

    #[tokio::test]
    async fn test_policy_hook_needs_approval() {
        use super::super::RiskLevel;

        let hooks = ToolHooks::new().policy(|name, _args, _ctx| {
            let name = name.to_string();
            async move {
                if name == "shell" {
                    ToolPolicyDecision::NeedsApproval {
                        approval_id: "test-123".into(),
                        tool_name: name.clone(),
                        args: serde_json::json!({}),
                        risk_level: RiskLevel::High,
                        requested_by: "user1".into(),
                        message: format!("Shell command requires approval: {}", name),
                    }
                } else {
                    ToolPolicyDecision::Allow
                }
            }
        });

        let decision = hooks
            .run_policy("shell", &serde_json::json!({}), &ToolContext::default())
            .await;
        assert!(decision.is_needs_approval());
        assert!(!decision.is_allow());
        assert!(!decision.is_deny());

        let decision = hooks
            .run_policy("memory", &serde_json::json!({}), &ToolContext::default())
            .await;
        assert!(decision.is_allow());
        assert!(!decision.is_needs_approval());
    }

    #[tokio::test]
    async fn test_policy_short_circuits_on_first_needs_approval() {
        let counter = Arc::new(AtomicUsize::new(0));
        let c = Arc::clone(&counter);

        let hooks = ToolHooks::new()
            .policy(|_, _, _| async {
                ToolPolicyDecision::NeedsApproval {
                    approval_id: "test".into(),
                    tool_name: "shell".into(),
                    args: serde_json::json!({}),
                    risk_level: RiskLevel::High,
                    requested_by: "user".into(),
                    message: "Approval required".into(),
                }
            })
            .policy(move |_, _, _| {
                let c = Arc::clone(&c);
                async move {
                    c.fetch_add(1, Ordering::SeqCst);
                    ToolPolicyDecision::Allow
                }
            });

        let decision = hooks
            .run_policy("any", &serde_json::json!({}), &ToolContext::default())
            .await;
        assert!(decision.is_needs_approval());
        // Second hook should not have run (NeedsApproval short-circuits like Deny).
        assert_eq!(counter.load(Ordering::SeqCst), 0);
    }
}
