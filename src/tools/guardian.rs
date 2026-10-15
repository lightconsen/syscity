//! A second model's opinion on a call that is about to ask a human.
//!
//! The gate already decided this call needs approval. The guardian runs *after*
//! that decision and before the approval is queued, and it may only ever make
//! the outcome stricter:
//!
//! - it rejects → the call is denied, and nobody is asked;
//! - it passes → the approval is queued as it would have been, with the
//!   reviewer's reason attached so the human reads a second opinion alongside
//!   the call;
//! - it has no opinion (error, timeout, no provider) → the approval is queued
//!   exactly as before.
//!
//! **It can never allow a call on the human's behalf.** That would contradict
//! the gate's own rule that a call needing approval is denied when nobody is
//! present to grant it — the point of the queue is that a person decides, and a
//! model that could skip the queue would be a wider hole than the one this
//! closes.
//!
//! Off by default (`[security] guardian`). It costs one model call per
//! approval-worthy tool call, which is a real price for a deployment that sees
//! many of them.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde::Deserialize;
use serde_json::Value;

use crate::providers::{CompletionRequest, Message, Provider};
use crate::tools::ToolContext;

/// How long the reviewer gets before it counts as having no opinion.
///
/// Short on purpose: this sits inside a tool call that is already waiting for
/// a person, so a slow reviewer must not add to that wait — it simply abstains
/// and the human decides.
const REVIEW_TIMEOUT: Duration = Duration::from_secs(20);

/// How long an opinion on one exact call is reused.
///
/// `execute_call` evaluates the policy twice for a single call — once as a
/// pre-check and again inside `execute` — so without this the reviewer would
/// run, and bill, twice per call. A reviewer's answer on the *same* tool with
/// the *same* arguments in the same conversation does not change in half a
/// minute, and the second run is not a second opinion.
const VERDICT_TTL: Duration = Duration::from_secs(30);

/// Bound on remembered verdicts, so a long session cannot grow the map.
const VERDICT_CACHE_MAX: usize = 64;

/// What the reviewer thought.
#[derive(Debug, Clone, Deserialize)]
pub struct Verdict {
    /// Whether the call may proceed to the human.
    #[serde(default)]
    pub allow: bool,
    /// Why — shown to the human, so it has to be worth reading.
    #[serde(default)]
    pub reason: String,
}

/// Reviews calls that are about to be put to a human.
pub struct Guardian {
    provider: Arc<dyn Provider>,
    /// The model to review with. `None` uses the provider's default, which is
    /// what an operator who configured no review model would expect.
    model: Option<String>,
    /// Recent verdicts, keyed by the call they were about.
    recent: Mutex<HashMap<String, (Instant, Verdict)>>,
}

impl Guardian {
    pub fn new(provider: Arc<dyn Provider>, model: Option<String>) -> Self {
        Self {
            provider,
            model,
            recent: Mutex::new(HashMap::new()),
        }
    }

    /// A short label for audit records and the approval message.
    pub fn label(&self) -> String {
        self.model
            .clone()
            .unwrap_or_else(|| format!("{} (default)", self.provider.name()))
    }

    /// Ask for an opinion. `None` means "no opinion" and covers every failure —
    /// an unreachable provider, a timeout, a reply that is not JSON — because
    /// the caller's response to all of them is the same: leave the decision
    /// where it was.
    pub async fn review(&self, name: &str, args: &Value, ctx: &ToolContext) -> Option<Verdict> {
        let key = format!("{name}\x1f{args}\x1f{}", ctx.identity.conversation_id);
        if let Some(verdict) = self.remembered(&key) {
            return Some(verdict);
        }

        let prompt = review_prompt(name, args, ctx);
        let request = CompletionRequest {
            messages: vec![Message::system(SYSTEM_PROMPT), Message::user(prompt)],
            model: self.model.clone(),
            // Deterministic: the same call should get the same opinion.
            temperature: Some(0.0),
            max_tokens: Some(400),
            stream: false,
            ..Default::default()
        };

        let response =
            match tokio::time::timeout(REVIEW_TIMEOUT, self.provider.complete(request)).await {
                Ok(Ok(response)) => response,
                Ok(Err(e)) => {
                    tracing::debug!(tool = name, error = %e, "guardian could not review the call");
                    return None;
                }
                Err(_) => {
                    tracing::debug!(tool = name, "guardian timed out; the human decides");
                    return None;
                }
            };

        match parse_verdict(&response.message.content) {
            Some(verdict) => {
                self.remember(key, &verdict);
                Some(verdict)
            }
            None => {
                tracing::debug!(tool = name, "guardian's reply was not a verdict");
                None
            }
        }
    }
}

impl Guardian {
    /// A verdict on this exact call, if one was reached recently.
    fn remembered(&self, key: &str) -> Option<Verdict> {
        // A poisoned lock means a previous review panicked; the map is advice,
        // so the recovery is to carry on with an empty one.
        let guard = self.recent.lock().unwrap_or_else(|e| e.into_inner());
        guard
            .get(key)
            .filter(|(at, _)| at.elapsed() < VERDICT_TTL)
            .map(|(_, verdict)| verdict.clone())
    }

    fn remember(&self, key: String, verdict: &Verdict) {
        let mut guard = self.recent.lock().unwrap_or_else(|e| e.into_inner());
        if guard.len() >= VERDICT_CACHE_MAX {
            guard.retain(|_, (at, _)| at.elapsed() < VERDICT_TTL);
            // Still full of live entries: drop the oldest rather than grow.
            if guard.len() >= VERDICT_CACHE_MAX {
                if let Some(oldest) = guard
                    .iter()
                    .min_by_key(|(_, (at, _))| *at)
                    .map(|(k, _)| k.clone())
                {
                    guard.remove(&oldest);
                }
            }
        }
        guard.insert(key, (Instant::now(), verdict.clone()));
    }
}

const SYSTEM_PROMPT: &str = "You review a single tool call before a human is asked to approve \
     it, and you answer with JSON only.\n\n\
     Reply exactly: {\"allow\": true|false, \"reason\": \"<one sentence>\"}\n\n\
     Set allow=false only when the call is clearly destructive, clearly outside what the request \
     asked for, or exposes credentials or private data. Approving is the default: the human is \
     about to see this call anyway, and a false alarm wastes their attention.";

/// The call, laid out for a reviewer that cannot see the conversation.
fn review_prompt(name: &str, args: &Value, ctx: &ToolContext) -> String {
    let args = serde_json::to_string_pretty(args).unwrap_or_else(|_| args.to_string());
    format!(
        "Tool: {name}\n\
         Agent: {}\n\
         Session: {}\n\
         Arguments:\n{args}",
        ctx.identity.user_id, ctx.identity.conversation_id
    )
}

/// Read a verdict out of a reply.
///
/// Models fence JSON, preamble it, or both, so this looks for the first object
/// that parses rather than demanding the whole reply be JSON. A reply with no
/// such object is no opinion — not a rejection, because a reviewer that cannot
/// answer has not said the call is dangerous.
fn parse_verdict(text: &str) -> Option<Verdict> {
    if let Ok(verdict) = serde_json::from_str::<Verdict>(text.trim()) {
        return Some(verdict);
    }
    let start = text.find('{')?;
    let end = text.rfind('}')?;
    if end <= start {
        return None;
    }
    serde_json::from_str::<Verdict>(&text[start..=end]).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_bare_verdict_parses() {
        let verdict = parse_verdict(r#"{"allow": false, "reason": "deletes the build dir"}"#)
            .expect("bare JSON");
        assert!(!verdict.allow);
        assert_eq!(verdict.reason, "deletes the build dir");
    }

    /// Models wrap JSON in prose and fences; that is still a verdict.
    #[test]
    fn a_fenced_verdict_parses() {
        let text =
            "Here is my review:\n```json\n{\"allow\": true, \"reason\": \"ordinary read\"}\n```\n";
        let verdict = parse_verdict(text).expect("fenced JSON");
        assert!(verdict.allow);
        assert_eq!(verdict.reason, "ordinary read");
    }

    /// A missing `allow` means the call is *not* waved through — the field is
    /// the dangerous one, so its absence must not default to permission.
    #[test]
    fn a_verdict_without_allow_does_not_approve() {
        let verdict = parse_verdict(r#"{"reason": "unsure"}"#).expect("parses");
        assert!(!verdict.allow);
    }

    #[test]
    fn prose_without_json_is_no_opinion() {
        assert!(parse_verdict("I think this is probably fine?").is_none());
        assert!(parse_verdict("").is_none());
        assert!(parse_verdict("{}").is_some(), "an empty object is a refusal, not silence");
    }

    /// The reviewer's own round trip, with no gate involved: a canned verdict
    /// comes back as a verdict.
    #[tokio::test]
    async fn a_canned_reply_becomes_a_verdict() {
        let provider = Arc::new(crate::providers::mock::MockProvider::new().with_responses(vec![
            Message::assistant(r#"{"allow": true, "reason": "an ordinary read"}"#),
        ]));
        let guardian = Guardian::new(provider, None);
        let verdict = guardian
            .review("file_read", &serde_json::json!({ "path": "/tmp/x" }), &ToolContext::default())
            .await
            .expect("the mock replied with a verdict");
        assert!(verdict.allow);
        assert_eq!(verdict.reason, "an ordinary read");
    }

    #[tokio::test]
    async fn the_label_names_the_model_it_will_use() {
        let provider: Arc<dyn Provider> = Arc::new(crate::providers::mock::MockProvider::new());
        assert!(Guardian::new(provider.clone(), Some("cheap-model".into()))
            .label()
            .contains("cheap-model"));
        assert!(Guardian::new(provider, None).label().contains("default"));
    }
}
