//! Call-level permissions: running outside the fence, on purpose and on record.
//!
//! Two halves of the same idea, both about a command that the workspace fence
//! refuses:
//!
//! - **Declared** — the model puts a `permissions` block in the call
//!   (`{"require_escalated": true, "justification": "..."}`). The registry's
//!   gate turns that into an approval request before the tool ever runs, so
//!   the human is asked *with the reason in front of them* instead of watching
//!   a command fail and guessing why.
//! - **Failure-driven** — the command ran, the fence refused it, and the
//!   output says so. [`escalate_and_rerun`] asks the same question and, on
//!   approval, runs the command again with no fence.
//!
//! Both end at the same place: an approval the operator gave, naming what it
//! was for. Neither is a way to *decide* anything automatically — the
//! classification below only chooses when to ask.
//!
//! **Bypass mode does not suppress this prompt, deliberately.** Bypass is a
//! permission posture ("stop asking me about tool calls"); the workspace fence
//! is a different constraint that bypass does not lift — the same orthogonality
//! the sandbox has against every permission mode. A refusal under bypass is
//! therefore still a refusal, and the operator is still the only one who can
//! let the command out; the alternative is a model stuck against a wall with no
//! way to ask. `deny` rules are unaffected either way: they block the call
//! before it runs, so nothing ever reaches the fence to escalate from.

use serde_json::Value;
use std::sync::Arc;
use tracing::{info, warn};

use super::approval::{ApprovalDecision, ApprovalQueue, PendingApproval, RiskLevel};
use super::process_runner::{CommandOutput, ProcessRequest};
use super::types::ToolContext;

/// What a call asked for outside its fence.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeclaredEscalation {
    /// The model's reason, shown to whoever the request goes to.
    pub justification: String,
}

/// Read a call's `permissions` block, if it has one that asks for escalation.
///
/// The block is a reserved argument key: tools read their own fields and
/// ignore it, which is what keeps this from being a new parameter on every
/// command tool.
pub fn declared_escalation(args: &Value) -> Option<DeclaredEscalation> {
    let block = args.get("permissions")?;
    let requested = block.get("require_escalated").and_then(Value::as_bool)?;
    if !requested {
        return None;
    }
    Some(DeclaredEscalation {
        justification: block
            .get("justification")
            .and_then(Value::as_str)
            .unwrap_or("no reason given")
            .to_string(),
    })
}

/// Whether a failed command's output looks like the *fence* refused it.
///
/// A heuristic, and only ever used to decide whether to *ask* — never whether
/// to allow. Denominations differ per platform (Seatbelt and this project's
/// seccomp filter raise `EPERM`, "Operation not permitted"; a read-only mount
/// says so), and a command can fail for its own reasons with the same words,
/// so a false positive costs one prompt and a false negative costs the model a
/// retry. Landlock's `EACCES` ("Permission denied") is deliberately *not*
/// classified: that is also what an ordinary unreadable file says, and
/// prompting on every permission error would train the operator to approve
/// without reading.
pub fn fence_denial_reason(stderr: &str) -> Option<&'static str> {
    let lowered = stderr.to_lowercase();
    if lowered.contains("operation not permitted") {
        return Some("the workspace fence refused it (operation not permitted)");
    }
    if lowered.contains("read-only file system") {
        return Some("the workspace fence refused it (read-only file system)");
    }
    None
}

/// Ask a human for permission to run this command outside its fence, and run
/// it unfenced if they say yes.
///
/// Returns the re-run's output, or `None` when there was nobody to ask, the
/// answer was no, or the prompt timed out — in which case the caller keeps the
/// original refusal. The prompt names the command's tool, its arguments and
/// `reason`, and says that the first attempt may already have had effects: the
/// fence refuses the *write*, not the process, so anything the command did
/// before hitting it stands.
pub async fn escalate_and_rerun(
    ctx: &ToolContext,
    tool_name: &str,
    args: &Value,
    request: &ProcessRequest,
    reason: &str,
) -> Option<CommandOutput> {
    // Only a fenced run can have been refused by the fence.
    request.fence.as_ref()?;
    // An interactive context with an approval queue is the only place a
    // question can be put to someone; cron, goals and delegated children have
    // no human to ask and keep the refusal.
    if !super::ask_user::can_ask_a_human(ctx) {
        return None;
    }
    let queue: Arc<ApprovalQueue> = ctx.approval_queue()?.clone();

    let (tx, rx) = tokio::sync::oneshot::channel();
    let approval = PendingApproval::new(
        format!("escalation-{}", uuid::Uuid::new_v4()),
        tool_name,
        args.clone(),
        ctx.user_id.clone(),
    )
    .with_risk_level(RiskLevel::High)
    .with_message(format!(
        "Run outside the workspace fence? {reason}. The first attempt may already have had \
         effects — the fence refuses the write, not the process."
    ))
    .with_session(Some(ctx.conversation_id.clone()))
    .with_response_tx(tx);
    let approval_id = approval.id.clone();
    queue.submit(approval).await;
    info!(
        approval_id = %approval_id,
        tool = tool_name,
        "fenced command refused; asking whether to run it outside the fence"
    );

    let decision = tokio::time::timeout(queue.default_timeout, rx).await;
    match decision {
        Ok(Ok(ApprovalDecision::Approve)) => {
            let mut unfenced = request.clone();
            unfenced.fence = None;
            info!(approval_id = %approval_id, tool = tool_name, "escalation approved; re-running unfenced");
            match super::process_runner::run_collect(&unfenced).await {
                Ok(output) => Some(output),
                Err(e) => {
                    warn!(approval_id = %approval_id, "approved escalation could not re-run: {e}");
                    None
                }
            }
        }
        Ok(Ok(ApprovalDecision::Deny { reason })) => {
            info!(approval_id = %approval_id, "escalation denied: {reason}");
            None
        }
        Ok(Err(_)) => None,
        Err(_) => {
            warn!(approval_id = %approval_id, "escalation prompt timed out");
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn declared_escalation_reads_the_block() {
        let declared = declared_escalation(&json!({
            "command": "git fetch",
            "permissions": { "require_escalated": true, "justification": "needs the network" }
        }))
        .expect("declared");
        assert_eq!(declared.justification, "needs the network");

        // No block, or one that does not ask, is not a declaration.
        assert_eq!(declared_escalation(&json!({ "command": "ls" })), None);
        assert_eq!(
            declared_escalation(&json!({ "permissions": { "require_escalated": false } })),
            None
        );
        assert_eq!(declared_escalation(&json!({ "permissions": {} })), None);
        assert_eq!(declared_escalation(&json!({ "permissions": "yes" })), None);

        // A reason is optional; the prompt still says something honest.
        let bare = declared_escalation(&json!({ "permissions": { "require_escalated": true } }))
            .expect("declared");
        assert_eq!(bare.justification, "no reason given");
    }

    #[test]
    fn fence_denials_are_recognised_and_ordinary_errors_are_not() {
        assert!(fence_denial_reason("sh: /ws/.git/hooks/x: Operation not permitted").is_some());
        assert!(fence_denial_reason("touch: cannot touch '/x': Read-only file system").is_some());

        // An ordinary failure must not turn into a prompt: this is the
        // difference between a fence and a permissions problem.
        assert_eq!(fence_denial_reason("cat: /etc/shadow: Permission denied"), None);
        assert_eq!(fence_denial_reason("ls: no such file or directory"), None);
        assert_eq!(fence_denial_reason(""), None);
    }
}
