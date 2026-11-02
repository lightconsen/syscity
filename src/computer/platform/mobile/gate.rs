//! The gate in front of an irreversible tap.
//!
//! [`risk`](super::risk) decides *what* a tap is; this decides *whether* it
//! runs. Two people can be in the loop and the answer differs for each:
//!
//! - **A human is reachable.** The tap is put to them in their own words, and
//!   only an explicit yes lets it through. Silence is not consent: a question
//!   that times out is a refusal, because the tap cannot be taken back and the
//!   agent can try again later.
//! - **Nobody is reachable.** Refusing would strand every unattended job — the
//!   message that was scheduled to go out at 9am does not go out because no one
//!   was at the desk. The tap runs, and the result carries what it was judged
//!   to be (`risk`) so the transcript says so afterwards. Pacing, not refusal,
//!   is what limits an agent with nobody watching.
//!
//! Configuration sits between the two: `[computer.mobile]` says which kinds of
//! tap this agent asks about at all (`ask_kinds`, `ask_before_irreversible`).
//! It can turn the question off; it cannot make a refusal into an approval.
//!
//! # Why this lives inside the tool
//!
//! The registry's own approval path (`requires_approval`) is the wrong shape
//! here. It is a per-*tool* capability, so it would either gate every
//! `android_input` — including scrolling a list — or none of them, and its
//! unattended fallback is a refusal. The classification needs the tree that is
//! only read inside the tool call, a few milliseconds before the tap.

use std::time::Duration;

use crate::tools::ToolContext;

use super::risk::{Offer, Risk};

/// How long to leave the question open.
///
/// Comfortably under the 120 s tool budget an agent's context carries, so the
/// agent gets a reasoned "declined" instead of a timeout that killed the whole
/// call. Long enough that a person stepping away for a moment is not read as a
/// refusal.
const ASK_TIMEOUT: Duration = Duration::from_secs(60);

/// What the gate decided, and why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Clearance {
    /// The classification, when there was one. Carried into the tool result so
    /// an unattended run still says what it did.
    pub offer: Option<Offer>,
    /// Whether the question was actually put to someone.
    pub asked: bool,
}

impl Clearance {
    /// A note for the tool result's `risk` field, or `None` for an ordinary tap.
    pub fn note(&self) -> Option<String> {
        self.offer.as_ref().map(|offer| offer.describe())
    }

    /// Whether this action reached outward, and so bought itself a pause.
    pub fn reaches_outward(&self) -> bool {
        self.offer
            .as_ref()
            .is_some_and(|offer| reaches_outward(offer.risk))
    }
}

/// Whether this kind of action is one a person would space out.
///
/// Sending, submitting, paying, deleting and ordering all reach past the phone
/// — another person, an account, a form someone else reads. Reading and
/// navigating do not, and waiting on them would only make the agent slow.
pub fn reaches_outward(risk: Risk) -> bool {
    matches!(risk, Risk::Send | Risk::Submit | Risk::Pay | Risk::Delete | Risk::Order)
}

/// A refusal, worded for the model.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Refusal {
    pub reason: String,
}

/// Decide whether `offer` may proceed, asking a human when there is one and
/// the configuration says to.
///
/// `subject` names what is being done in the question ("tap on \"发送\""), so
/// the person reading it knows which app and which button, not just that some
/// send was classified.
pub async fn check(
    offer: Option<Offer>,
    ctx: &ToolContext,
    subject: &str,
) -> Result<Clearance, Refusal> {
    let Some(offer) = offer else {
        // Nothing to judge. This is the common case and must stay free.
        return Ok(Clearance { offer: None, asked: false });
    };

    let config = ctx.mobile_control();
    if !config.asks(offer.risk.label()) {
        return Ok(Clearance {
            offer: Some(offer),
            asked: false,
        });
    }

    // No queue, or a context that exists precisely because no one is there
    // (cron, goals, delegated children). Let it run; the transcript keeps the
    // record of what it was.
    if !crate::tools::ask_user::can_ask_a_human(ctx) {
        return Ok(Clearance {
            offer: Some(offer),
            asked: false,
        });
    }
    let Some(queue) = ctx.ask_queue.clone() else {
        return Ok(Clearance {
            offer: Some(offer),
            asked: false,
        });
    };

    let question = format!("The agent wants to {subject} — {}. Allow it?", offer.describe());
    let conversation_id = ctx.conversation_id.clone();

    use crate::tools::ask_user::{AskRequest, PendingQuestion};
    let (tx, rx) = tokio::sync::oneshot::channel();
    let id = queue
        .submit(PendingQuestion {
            request: AskRequest {
                id: format!("mobile-risk-{}", uuid::Uuid::new_v4()),
                conversation_id,
                question,
                options: vec!["Allow".to_string(), "Refuse".to_string()],
                required: true,
                default: None,
            },
            response_tx: Some(tx),
        })
        .await;

    let answer = match tokio::time::timeout(ASK_TIMEOUT, rx).await {
        Ok(Ok(answer)) => answer,
        // Timed out or the queue dropped the question: nobody said yes.
        _ => {
            queue.cancel(&id).await;
            return Err(Refusal {
                reason: format!(
                    "{} and nobody answered within {}s; nothing was done",
                    offer.describe(),
                    ASK_TIMEOUT.as_secs()
                ),
            });
        }
    };

    // Only an explicit yes. The options are ours, so this compares against the
    // word we sent rather than guessing at free text.
    if answer.trim().eq_ignore_ascii_case("allow") {
        Ok(Clearance {
            offer: Some(offer),
            asked: true,
        })
    } else {
        Err(Refusal {
            reason: format!("a human refused: {}", offer.describe()),
        })
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::computer::platform::mobile::risk::Risk;
    use crate::tools::ask_user::AskQueue;

    fn offer() -> Offer {
        Offer {
            risk: Risk::Send,
            matched: "发送".to_string(),
            element: Some("发送".to_string()),
        }
    }

    fn context(queue: Arc<AskQueue>) -> ToolContext {
        ToolContext::new("user-1", "conv-1").with_ask_queue(queue)
    }

    /// Answer the first question that arrives, from the other side of the
    /// queue, the way a person tapping a button would.
    ///
    /// The subscription is taken here, before the task is spawned: a broadcast
    /// channel only delivers to receivers that existed when the message was
    /// sent, and a spawned task has not necessarily run yet by the time `check`
    /// submits. That race is what made these tests hang.
    fn answer(queue: Arc<AskQueue>, reply: &'static str) -> tokio::task::JoinHandle<()> {
        let mut events = queue.event_tx.subscribe();
        tokio::spawn(async move {
            while let Ok(event) = events.recv().await {
                if let crate::tools::ask_user::AskEvent::Required(question) = event {
                    queue.resolve(&question.ask_id, reply.to_string()).await;
                    return;
                }
            }
        })
    }

    #[tokio::test]
    async fn a_send_with_a_human_present_is_asked_about() {
        let queue = Arc::new(AskQueue::new());
        let responder = answer(Arc::clone(&queue), "Allow");
        let clearance = check(Some(offer()), &context(queue), "tap 发送")
            .await
            .expect("allowed");
        responder.await.unwrap();
        assert!(clearance.asked);
        assert_eq!(clearance.offer.as_ref().map(|o| o.risk), Some(Risk::Send));
    }

    #[tokio::test]
    async fn a_refusal_stops_the_tap_and_says_why() {
        let queue = Arc::new(AskQueue::new());
        let responder = answer(Arc::clone(&queue), "Refuse");
        let refused = check(Some(offer()), &context(queue), "tap 发送")
            .await
            .expect_err("refused");
        responder.await.unwrap();
        assert!(refused.reason.contains("refused"), "{}", refused.reason);
        assert!(refused.reason.contains("send"), "{}", refused.reason);
    }

    #[tokio::test]
    async fn nobody_to_ask_means_it_runs_and_reports_the_risk() {
        // A cron turn: user_id `system`, a `cron:` conversation, and no queue.
        let ctx = ToolContext::new("system", "cron:nightly");
        let clearance = check(Some(offer()), &ctx, "tap 发送")
            .await
            .expect("unattended sends are paced, not refused");
        assert!(!clearance.asked);
        assert!(clearance.note().expect("risk reported").contains("send"));
    }

    #[tokio::test]
    async fn configuration_can_turn_the_question_off() {
        let queue = Arc::new(AskQueue::new());
        let config = crate::config::MobileControlConfig {
            ask_before_irreversible: false,
            ..Default::default()
        };
        let ctx = context(queue).with_mobile_control(config);
        let clearance = check(Some(offer()), &ctx, "tap 发送")
            .await
            .expect("allowed");
        assert!(!clearance.asked);
        assert_eq!(clearance.note().is_some(), true);
    }

    #[tokio::test]
    async fn a_kind_that_is_not_listed_is_not_asked() {
        // Submit is not asked by default; the switch is per kind, not one bit.
        let queue = Arc::new(AskQueue::new());
        let submit = Offer {
            risk: Risk::Submit,
            matched: "提交".to_string(),
            element: Some("提交".to_string()),
        };
        let clearance = check(Some(submit), &context(queue), "tap 提交")
            .await
            .expect("allowed");
        assert!(!clearance.asked);
    }

    #[tokio::test]
    async fn a_delegated_child_does_not_block_on_a_question_nobody_can_answer() {
        let queue = Arc::new(AskQueue::new());
        let ctx = ToolContext::new("user-1", "conv-1")
            .with_ask_queue(queue)
            .with_delegation(Some(crate::delegation::DelegationScope::new("root", "task", 2, 3)));
        let clearance = check(Some(offer()), &ctx, "tap 发送")
            .await
            .expect("allowed");
        assert!(!clearance.asked);
    }

    #[tokio::test]
    async fn nothing_irreversible_costs_nothing() {
        let queue = Arc::new(AskQueue::new());
        let clearance = check(None, &context(queue), "tap 首页")
            .await
            .expect("allowed");
        assert!(clearance.offer.is_none() && !clearance.asked);
    }
}
