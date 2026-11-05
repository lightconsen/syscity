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
    /// What a person actually approved, when they did.
    ///
    /// The answer "allow" is not permission to tap whatever is under the
    /// finger when the call finally dispatches — it is permission for the
    /// thing that was described in the question. [`verify`] checks that this
    /// is still what is about to happen.
    pub granted: Option<Grant>,
}

/// An approval, bound to the thing that was approved.
///
/// The question reads "the agent wants to tap "发送" in com.example.chat —
/// send ...". All three parts of that are in here, because all three can
/// change between the question and the dispatch, and a `bool` would carry
/// none of them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Grant {
    /// The app the question named.
    pub package: Option<String>,
    /// The kind of action, as classified.
    pub risk: Risk,
    /// The element label, as it appeared on screen.
    pub label: Option<String>,
    /// What the person read, for the mismatch message.
    pub subject: String,
    /// For an approval of a whole sequence: the steps it covered.
    ///
    /// `None` for a single action, which is then bound by `risk` and `label`
    /// above. A sequence is approved once for every step it declared, and each
    /// of those steps is still checked against the declaration when it runs —
    /// so one yes is not a licence for anything the sequence did not say.
    pub plan: Option<Vec<Planned>>,
}

/// One step a sequence declared before it started.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Planned {
    /// The kind of action, as classified from the step's own arguments.
    pub risk: Risk,
    /// The label the step names, when it can be known before the step runs.
    pub label: Option<String>,
    /// The step in a person's words, for the question and the result.
    pub what: String,
}

impl Grant {
    fn describes(&self, offer: &Offer, package: Option<&str>) -> bool {
        // The app must match when either side knows it. A missing package
        // on one side means the tree could not name one, not that any app
        // will do.
        if !match (self.package.as_deref(), package) {
            (Some(approved), Some(now)) => approved == now,
            _ => true,
        } {
            return false;
        }
        match &self.plan {
            // An approval of a sequence covers the steps it declared, one at a
            // time, and only those.
            Some(plan) => plan.iter().any(|step| {
                step.risk == offer.risk
                    && label_agrees(step.label.as_deref(), offer.element.as_deref())
            }),
            // The kind must match: approving a send is not approving a payment
            // because the screen changed under the same label.
            None => {
                self.risk == offer.risk
                    && label_agrees(self.label.as_deref(), offer.element.as_deref())
            }
        }
    }

    /// The single-action grant one declared step of this plan amounts to.
    ///
    /// Keeping the *declared* label rather than the one seen now is the point:
    /// the step still has to be the step that was described, or [`verify`]
    /// withholds it.
    fn step(&self, planned: &Planned) -> Self {
        Self {
            package: self.package.clone(),
            risk: planned.risk,
            label: planned.label.clone(),
            subject: planned.what.clone(),
            plan: None,
        }
    }
}

/// Whether an approved label and the label seen now are the same action.
///
/// A missing label on the *offer* is not a match for a named one: the person
/// approved a named button, and "something unnameable is there now" is the
/// screen having moved on. A missing label on the approval accepts anything of
/// the same kind — that is the "focus unknown" enter, and the kind is all that
/// was ever known about it.
fn label_agrees(approved: Option<&str>, now: Option<&str>) -> bool {
    match (approved, now) {
        (Some(approved), Some(now)) => approved == now,
        (Some(_), None) => false,
        _ => true,
    }
}

/// Why a granted action was not carried out.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Withheld {
    pub reason: String,
}

/// Check that what is about to happen is what was approved.
///
/// Called immediately before dispatch, with the tree read for *this* attempt.
/// The gap this closes: a question is put to a person, they take a few seconds
/// to read it, and by the time they say yes the app has moved on — a message
/// arrived and the list scrolled, the page redirected. The approval was for
/// the thing described; this refuses to let it authorise anything else.
///
/// Nothing to check when no grant was made (nobody was asked, or nobody could
/// be), which is the unattended case and keeps its own rules.
pub fn verify(
    clearance: &Clearance,
    offer: Option<&Offer>,
    package: Option<&str>,
) -> Result<(), Withheld> {
    let Some(grant) = &clearance.granted else {
        return Ok(());
    };
    let Some(offer) = offer else {
        return Err(Withheld {
            reason: format!(
                "Refused: \"{}\" was approved, and by the time it was, the screen no longer \
                 shows a {} action there. Nothing was done. Read the screen and, if it is still \
                 the right thing to do, try again.",
                grant.subject,
                grant.risk.label()
            ),
        });
    };
    if grant.describes(offer, package) {
        return Ok(());
    }
    Err(Withheld {
        reason: format!(
            "Refused: \"{}\" was approved as a {}{}, and what is there now is a {} on {:?}{}. \
             An approval covers the action that was described, not whatever took its place. \
             Nothing was done — read the screen and ask again if it is still wanted.",
            grant.subject,
            grant.risk.label(),
            grant
                .package
                .as_deref()
                .map(|p| format!(" in {p}"))
                .unwrap_or_default(),
            offer.risk.label(),
            offer.element.as_deref().unwrap_or("that"),
            package
                .filter(|now| Some(*now) != grant.package.as_deref())
                .map(|now| format!(" in {now}"))
                .unwrap_or_default(),
        ),
    })
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

/// What a run's sequence was approved for.
///
/// A run is a plan made in one turn and carried out over the next thirty, so
/// it cannot ask about each step: that is thirty questions, and the person
/// answering them has less information than the plan does. It is instead put
/// to a human once, with every irreversible step named — and then each of
/// those steps is checked against the plan as it runs, so the approval covers
/// what was described and nothing that took its place.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Plan {
    /// What a person approved, when there was one to ask. Empty when the run
    /// was unattended, in which case the steps still check against `planned`
    /// but nothing external was promised.
    pub grant: Option<Grant>,
    /// Every irreversible step the run declared, in order.
    pub planned: Vec<Planned>,
}

impl Plan {
    /// Whether the plan already accounts for this action.
    ///
    /// A run calls this before dispatching a step: if the step is one the plan
    /// named, the person's yes already covers it and the step must not ask
    /// again. A step the plan did *not* name is left to the ordinary gate,
    /// which asks its own question — so an unplanned send still meets a
    /// person.
    pub fn accounts_for(&self, offer: &Offer) -> bool {
        self.planned.iter().any(|step| {
            step.risk == offer.risk && label_agrees(step.label.as_deref(), offer.element.as_deref())
        })
    }

    /// What this plan adds to a step's result, or `None` for an ordinary run.
    pub fn note(&self) -> Option<String> {
        if self.planned.is_empty() {
            return None;
        }
        let kinds = self
            .planned
            .iter()
            .map(|step| step.risk.label())
            .collect::<Vec<_>>()
            .join(", ");
        Some(if self.grant.is_some() {
            format!(
                "Approved as a whole: {} irreversible step(s) ({kinds}) were named in the plan \
                 and a human allowed them.",
                self.planned.len()
            )
        } else {
            format!(
                "{} irreversible step(s) ({kinds}) in this run; nobody was asked, so they ran \
                 and this is the record.",
                self.planned.len()
            )
        })
    }

    /// The approval a single step of this plan runs under.
    ///
    /// `None` when the step is not one the plan declared — which is the case
    /// that matters: the screen's own classifier should not be able to produce
    /// a different kind mid-run than the plan asked about, and if it does, the
    /// step is refused rather than quietly run on an approval meant for
    /// something else.
    pub fn clearance(&self, offer: Option<&Offer>, package: Option<&str>) -> Option<Clearance> {
        let Some(offer) = offer else {
            return Some(Clearance {
                offer: None,
                asked: false,
                granted: None,
            });
        };
        let matched = self.planned.iter().find(|step| {
            step.risk == offer.risk && label_agrees(step.label.as_deref(), offer.element.as_deref())
        })?;
        // The grant names the app it was given for; a step that is now in
        // another app is not covered, even by a plan — and then nobody has
        // been asked about *this* step, which is what `asked` reports.
        let granted = self
            .grant
            .as_ref()
            .filter(|grant| match (grant.package.as_deref(), package) {
                (Some(approved), Some(now)) => approved == now,
                _ => true,
            })
            .map(|grant| grant.step(matched));
        Some(Clearance {
            offer: Some(offer.clone()),
            asked: granted.is_some(),
            granted,
        })
    }
}

/// Read a plan's steps out of the arguments a run declared.
///
/// Each step is classified by the same rules a single call would use, so a
/// step cannot be described as one thing in the plan and be another at run
/// time without the mismatch being visible. The classification is from the
/// arguments alone: there is no tree yet, and a plan written before the first
/// read is a plan about intent, not about a particular screen.
pub fn plan_steps(commands: &[serde_json::Value]) -> Vec<Planned> {
    let mut planned = Vec::new();
    for command in commands {
        let op = command
            .get("op")
            .and_then(|value| value.as_str())
            .unwrap_or("");
        let text = |key: &str| command.get(key).and_then(|value| value.as_str());
        let target = text("targetText");
        let offer = match op {
            // A step's own target is a label the model wrote down. It is the
            // only thing known before the step runs, and the same words a
            // person would read in the question.
            "tap" => target.and_then(super::risk::classify_named),
            // A type step commits when its text ends in a newline — that is
            // the enter `input text` emits. It is described without the
            // field's label: the label is known only once the screen the step
            // will type into has been read, and binding to a guess would
            // refuse the run for the step being the step.
            "type" => text("text")
                .filter(|text| text.contains('\n'))
                .map(|_| Offer {
                    risk: Risk::Send,
                    matched: "换行".to_string(),
                    element: None,
                }),
            // Likewise a key: which control it activates is a property of the
            // screen the step runs on. A step that turns out to activate an
            // irreversible control the plan did not name is refused at run
            // time, which is where that answer first exists.
            "key" => text("keycode")
                .map(super::android::normalize_keycode)
                .filter(|code| super::risk::is_submit_key(code))
                .map(|code| Offer {
                    risk: Risk::Send,
                    matched: code,
                    element: None,
                }),
            _ => None,
        };
        let Some(offer) = offer else {
            continue;
        };
        if !reaches_outward(offer.risk) {
            continue;
        }
        planned.push(Planned {
            risk: offer.risk,
            label: offer.element.clone(),
            what: describe_step(op, target, &offer),
        });
    }
    planned
}

/// A step in the words a person reading the question would use.
fn describe_step(op: &str, target: Option<&str>, offer: &Offer) -> String {
    let place = target
        .map(|label| format!(" \"{label}\""))
        .or_else(|| offer.element.as_ref().map(|label| format!(" \"{label}\"")))
        .unwrap_or_default();
    match op {
        "tap" => format!("tap{place}"),
        "type" => format!("type into{place} and press enter"),
        "clear" => format!("clear{place}"),
        "key" => format!("press {}", offer.matched),
        other => format!("{other}{place}"),
    }
}

/// Put a run's whole plan to a human, once.
///
/// A run is a plan made in one turn and carried out over the next thirty, so
/// it cannot ask about each step: that is thirty questions, and the person
/// answering them has less information than the plan does. It is instead put
/// to a human once, with every irreversible step named — and then each of
/// those steps is checked against the plan as it runs (see [`Plan::clearance`]).
///
/// Nothing to ask about when no step is irreversible: that is an ordinary run,
/// and the caller gets a plan with no steps and no grant in it.
pub async fn approve_plan(
    planned: Vec<Planned>,
    ctx: &ToolContext,
    package: Option<&str>,
) -> Result<Plan, Refusal> {
    if planned.is_empty() {
        return Ok(Plan { grant: None, planned });
    }
    let config = ctx.mobile_control();
    let asks_any = planned.iter().any(|step| config.asks(step.risk.label()));
    if !asks_any || !crate::tools::ask_user::can_ask_a_human(ctx) {
        return Ok(Plan { grant: None, planned });
    }
    let Some(queue) = ctx.ask_queue.clone() else {
        return Ok(Plan { grant: None, planned });
    };

    let steps = planned
        .iter()
        .enumerate()
        .map(|(index, step)| {
            format!(
                "\n  {}. {} (this looks like a {} action)",
                index + 1,
                step.what,
                step.risk.label()
            )
        })
        .collect::<String>();
    let in_app = package
        .map(|package| format!(" in {package}"))
        .unwrap_or_default();
    let question = format!(
        "The agent wants to run a {count}-step sequence on this phone{in_app}. These steps \
         cannot be taken back:{steps}\nAllow all of them?",
        count = planned.len()
    );

    let conversation_id = ctx.conversation_id.clone();
    use crate::tools::ask_user::{AskRequest, PendingQuestion};
    let (tx, rx) = tokio::sync::oneshot::channel();
    let id = queue
        .submit(PendingQuestion {
            request: AskRequest {
                id: format!("mobile-plan-{}", uuid::Uuid::new_v4()),
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
        _ => {
            queue.cancel(&id).await;
            return Err(Refusal {
                reason: format!(
                    "this run would {} and nobody answered within {}s; nothing was done",
                    planned
                        .iter()
                        .map(|step| step.what.clone())
                        .collect::<Vec<_>>()
                        .join(", "),
                    ASK_TIMEOUT.as_secs()
                ),
            });
        }
    };

    if !answer.trim().eq_ignore_ascii_case("allow") {
        return Err(Refusal {
            reason: format!(
                "a human refused this run: it would {}",
                planned
                    .iter()
                    .map(|step| step.what.clone())
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        });
    }
    Ok(Plan {
        grant: Some(Grant {
            package: package.map(str::to_string),
            // The grant as a whole is bound by its plan, not by one risk.
            risk: planned[0].risk,
            label: None,
            subject: format!("a {}-step sequence", planned.len()),
            plan: Some(planned.clone()),
        }),
        planned,
    })
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
    package: Option<&str>,
) -> Result<Clearance, Refusal> {
    let Some(offer) = offer else {
        // Nothing to judge. This is the common case and must stay free.
        return Ok(Clearance {
            offer: None,
            asked: false,
            granted: None,
        });
    };

    let config = ctx.mobile_control();
    if !config.asks(offer.risk.label()) {
        return Ok(Clearance {
            offer: Some(offer),
            asked: false,
            granted: None,
        });
    }

    // No queue, or a context that exists precisely because no one is there
    // (cron, goals, delegated children). Let it run; the transcript keeps the
    // record of what it was.
    if !crate::tools::ask_user::can_ask_a_human(ctx) {
        return Ok(Clearance {
            offer: Some(offer),
            asked: false,
            granted: None,
        });
    }
    let Some(queue) = ctx.ask_queue.clone() else {
        return Ok(Clearance {
            offer: Some(offer),
            asked: false,
            granted: None,
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
            granted: Some(Grant {
                package: package.map(str::to_string),
                risk: offer.risk,
                label: offer.element.clone(),
                subject: subject.to_string(),
                plan: None,
            }),
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
        let clearance = check(Some(offer()), &context(queue), "tap 发送", Some("com.example.chat"))
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
        let refused = check(Some(offer()), &context(queue), "tap 发送", Some("com.example.chat"))
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
        let clearance = check(Some(offer()), &ctx, "tap 发送", Some("com.example.chat"))
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
        let clearance = check(Some(offer()), &ctx, "tap 发送", Some("com.example.chat"))
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
        let clearance = check(Some(submit), &context(queue), "tap 提交", Some("com.example.chat"))
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
        let clearance = check(Some(offer()), &ctx, "tap 发送", Some("com.example.chat"))
            .await
            .expect("allowed");
        assert!(!clearance.asked);
    }

    #[tokio::test]
    async fn nothing_irreversible_costs_nothing() {
        let queue = Arc::new(AskQueue::new());
        let clearance = check(None, &context(queue), "tap 首页", Some("com.example.chat"))
            .await
            .expect("allowed");
        assert!(clearance.offer.is_none() && !clearance.asked);
    }

    // ── What the approval is bound to ──────────────────────────────────────

    /// Approve a send, then check it against what is about to happen.
    async fn approved() -> Clearance {
        let queue = Arc::new(AskQueue::new());
        let responder = answer(Arc::clone(&queue), "Allow");
        let clearance = check(
            Some(offer()),
            &context(queue),
            "tap \"发送\" in com.example.chat",
            Some("com.example.chat"),
        )
        .await
        .expect("allowed");
        responder.await.unwrap();
        clearance
    }

    #[tokio::test]
    async fn an_approval_covers_the_thing_that_was_approved() {
        let clearance = approved().await;
        verify(&clearance, Some(&offer()), Some("com.example.chat"))
            .expect("the same send is what was approved");
    }

    #[tokio::test]
    async fn an_approval_does_not_cover_a_different_kind_of_action() {
        // A message arrived, the list scrolled, and the button under the
        // finger is now "pay". The person approved a send.
        let clearance = approved().await;
        let pay = Offer {
            risk: Risk::Pay,
            matched: "付款".to_string(),
            element: Some("付款".to_string()),
        };
        let withheld = verify(&clearance, Some(&pay), Some("com.example.chat"))
            .expect_err("a payment is not a send");
        assert!(withheld.reason.contains("send"), "{}", withheld.reason);
        assert!(withheld.reason.contains("pay"), "{}", withheld.reason);
    }

    #[tokio::test]
    async fn an_approval_does_not_cover_the_same_button_in_another_app() {
        let clearance = approved().await;
        let withheld = verify(&clearance, Some(&offer()), Some("com.example.other"))
            .expect_err("a different app is a different action");
        assert!(withheld.reason.contains("com.example.other"), "{}", withheld.reason);
    }

    #[tokio::test]
    async fn an_approval_does_not_cover_a_different_button() {
        let clearance = approved().await;
        let other = Offer {
            risk: Risk::Send,
            matched: "转发".to_string(),
            element: Some("转发".to_string()),
        };
        verify(&clearance, Some(&other), Some("com.example.chat"))
            .expect_err("a different label is a different action");
    }

    #[tokio::test]
    async fn nothing_left_where_it_was_withholds_the_action() {
        // The screen moved on entirely: there is no such button any more.
        let clearance = approved().await;
        let withheld = verify(&clearance, None, Some("com.example.chat"))
            .expect_err("nothing there is not the approved thing");
        assert!(withheld.reason.contains("no longer"), "{}", withheld.reason);
    }

    #[tokio::test]
    async fn nothing_to_verify_when_nobody_was_asked() {
        // Unattended: no grant exists, so the binding check has nothing to
        // say, and the pacing is what limits it.
        let ctx = ToolContext::new("system", "cron:nightly");
        let clearance = check(Some(offer()), &ctx, "tap 发送", Some("com.example.chat"))
            .await
            .expect("allowed");
        verify(&clearance, None, Some("something.else"))
            .expect("nothing was approved, so nothing binds");
    }

    // ── A run's plan, approved once ────────────────────────────────────────

    fn step(risk: Risk, label: Option<&str>, what: &str) -> Planned {
        Planned {
            risk,
            label: label.map(str::to_string),
            what: what.to_string(),
        }
    }

    fn commands() -> Vec<serde_json::Value> {
        serde_json::json!([
            { "op": "tap", "targetText": "搜索" },
            { "op": "tap", "targetText": "发送" },
            { "op": "type", "targetText": "消息", "text": "你好" },
            { "op": "type", "targetText": "消息", "text": "你好\n" },
            { "op": "key", "keycode": "ENTER" },
            { "op": "key", "keycode": "BACK" }
        ])
        .as_array()
        .cloned()
        .unwrap_or_default()
    }

    #[test]
    fn a_plan_names_only_the_steps_that_cannot_be_taken_back() {
        // The tap on a search box, the typing with no line break and the back
        // key are not decisions; the send, the enter-terminated text and the
        // bare enter are.
        let planned = plan_steps(&commands());
        let whats: Vec<&str> = planned.iter().map(|step| step.what.as_str()).collect();
        assert_eq!(
            whats,
            vec![
                "tap \"发送\"",
                "type into \"消息\" and press enter",
                "press KEYCODE_ENTER"
            ]
        );
    }

    #[tokio::test]
    async fn a_plan_is_approved_as_a_whole_and_binds_each_step() {
        let queue = Arc::new(AskQueue::new());
        let responder = answer(Arc::clone(&queue), "Allow");
        let plan = approve_plan(
            vec![
                step(Risk::Send, Some("发送"), "tap \"发送\""),
                step(Risk::Pay, Some("付款"), "tap \"付款\""),
            ],
            &context(queue),
            Some("com.example.chat"),
        )
        .await
        .expect("allowed");
        responder.await.unwrap();

        let send = Offer {
            risk: Risk::Send,
            matched: "发送".to_string(),
            element: Some("发送".to_string()),
        };
        let clearance = plan
            .clearance(Some(&send), Some("com.example.chat"))
            .expect("the send was in the plan");
        assert!(clearance.asked, "the plan's yes covers the step");
        verify(&clearance, Some(&send), Some("com.example.chat"))
            .expect("it is still the send that was approved");

        // The plan named a payment; this is a delete, which nobody approved.
        let delete = Offer {
            risk: Risk::Delete,
            matched: "删除".to_string(),
            element: Some("删除".to_string()),
        };
        assert!(plan
            .clearance(Some(&delete), Some("com.example.chat"))
            .is_none());
        assert!(!plan.accounts_for(&delete));
    }

    #[tokio::test]
    async fn a_plan_approval_does_not_cover_the_same_step_in_another_app() {
        let queue = Arc::new(AskQueue::new());
        let responder = answer(Arc::clone(&queue), "Allow");
        let plan = approve_plan(
            vec![step(Risk::Send, Some("发送"), "tap \"发送\"")],
            &context(queue),
            Some("com.example.chat"),
        )
        .await
        .expect("allowed");
        responder.await.unwrap();

        let send = Offer {
            risk: Risk::Send,
            matched: "发送".to_string(),
            element: Some("发送".to_string()),
        };
        let clearance = plan
            .clearance(Some(&send), Some("com.example.other"))
            .expect("it is a step the plan named");
        assert!(!clearance.asked, "a different app is not the approved one");
        assert!(clearance.granted.is_none());
    }

    #[tokio::test]
    async fn a_plan_with_nothing_irreversible_asks_nobody() {
        let queue = Arc::new(AskQueue::new());
        let plan = approve_plan(Vec::new(), &context(Arc::clone(&queue)), None)
            .await
            .expect("nothing to ask about");
        assert!(plan.grant.is_none());
        assert!(plan.note().is_none());
    }

    #[tokio::test]
    async fn an_unanswered_plan_runs_nothing() {
        // A question nobody answered is not consent: the run does not begin.
        // Cancelled from the queue's side, which is the same outcome as a
        // timeout — the answer channel drops and there is no yes in it.
        let queue = Arc::new(AskQueue::new());
        let canceller = {
            let queue = Arc::clone(&queue);
            let mut events = queue.event_tx.subscribe();
            tokio::spawn(async move {
                while let Ok(event) = events.recv().await {
                    if let crate::tools::ask_user::AskEvent::Required(question) = event {
                        queue.cancel(&question.ask_id).await;
                        return;
                    }
                }
            })
        };
        let refused = approve_plan(
            vec![step(Risk::Send, Some("发送"), "tap \"发送\"")],
            &context(Arc::clone(&queue)),
            Some("com.example.chat"),
        )
        .await
        .expect_err("silence is not a yes");
        canceller.await.unwrap();
        assert!(refused.reason.contains("发送"), "{}", refused.reason);
        assert!(refused.reason.contains("nothing was done"), "{}", refused.reason);
    }

    #[tokio::test]
    async fn a_refused_plan_says_what_it_would_have_done() {
        let queue = Arc::new(AskQueue::new());
        let responder = answer(Arc::clone(&queue), "Refuse");
        let refused = approve_plan(
            vec![
                step(Risk::Pay, Some("付款"), "tap \"付款\""),
                step(Risk::Send, Some("发送"), "tap \"发送\""),
            ],
            &context(queue),
            None,
        )
        .await
        .expect_err("refused");
        responder.await.unwrap();
        assert!(refused.reason.contains("付款"), "{}", refused.reason);
        assert!(refused.reason.contains("发送"), "{}", refused.reason);
    }

    #[tokio::test]
    async fn an_unattended_run_records_its_plan_without_asking() {
        // A cron turn: the steps still run, and the plan is what the transcript
        // says they were.
        let planned = vec![step(Risk::Send, Some("发送"), "tap \"发送\"")];
        let plan = approve_plan(planned, &ToolContext::new("system", "cron:nightly"), None)
            .await
            .expect("unattended runs are paced, not refused");
        assert!(plan.grant.is_none());
        let note = plan.note().expect("the plan is recorded");
        assert!(note.contains("send"), "{note}");
        assert!(note.contains("nobody was asked"), "{note}");
    }
}
