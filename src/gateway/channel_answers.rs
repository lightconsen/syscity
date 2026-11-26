//! Prompts held for a remote human to answer in chat.
//!
//! An agent turn can suspend on one of two questions — `ask_user` (it needs a
//! fact) or a tool approval (it wants to run something gated). Both reach the
//! built-in UI over the WebSocket event bus, and both time out after 300 s.
//! Neither had any way to reach a person whose conversation arrived over a
//! messaging channel, so a channel user's agent stalled the first time it
//! needed a decision.
//!
//! Two halves fix that, and both live here:
//!
//! 1. **Delivery.** [`record`] is called when a prompt is raised for a session
//!    that belongs to a channel; the caller then posts the prompt text to that
//!    channel.
//! 2. **Answering.** The next plain message from that person is checked against
//!    the table ([`take`]) before it is routed to an agent. If it is an answer
//!    (`1`, `yes`, `/approve`, or a free-text reply to an `ask`), it resolves
//!    the suspended call and never reaches the model.
//!
//! The table is keyed by the **session** (`conversation_id`), not by the
//! sender, because that is the one identifier both sides agree on. Keying by
//! `{channel}:{user_id}` looks equivalent and is not: on Telegram the reply
//! address is the *chat* id, and in a group that is not the sender's user id —
//! so a question asked in a group would be recorded under one key and the
//! answer arrive under another, and the prompt would sit unanswered until it
//! timed out. The session is the same for the question and the reply in both
//! direct and group chats.
//!
//! The entry also remembers the channel it was posted to, and an answer must
//! arrive on that same channel — see [`PendingPrompts`].

use std::collections::HashMap;

use tokio::sync::RwLock;

/// What the person is being asked, and how an answer is applied.
#[derive(Debug, Clone)]
pub struct PendingPrompt {
    pub kind: PromptKind,
    /// The queue id: an `ask_id` for [`PromptKind::Ask`], an `approval_id` for
    /// the approval variants.
    pub id: String,
    /// Multiple-choice options for an `ask`, empty for free text.
    pub options: Vec<String>,
    /// `true` when the prompt is an `ask` that accepts free text, so a reply
    /// that is not `1`/`yes` is still a valid answer rather than a new request.
    pub free_text: bool,
}

/// Which queue a pending prompt belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PromptKind {
    /// An `ask_user` question — the reply text is the answer.
    Ask,
    /// A tool approval — `yes`/`1` approves, `no`/`0` denies.
    Approval,
}

impl PendingPrompt {
    /// Interpret one chat message as an answer.
    ///
    /// Returns [`Answer::Approve`], [`Answer::Deny`], or
    /// [`Answer::Text`] when the message resolves the prompt, and `None` when
    /// it is not an answer at all (so the caller routes it to the agent as an
    /// ordinary message). Case and surrounding whitespace are ignored; a
    /// leading `/` is tolerated so the chat platform's command style works.
    pub fn interpret(&self, text: &str) -> Option<Answer> {
        // The by-id form the chat prompt advertises: `/answer <id> <text>`,
        // `/approve <id>`, `/deny <id>`. A message that is one of these is
        // *only* ever an answer for the id it names — a stale or mistyped id
        // leaves the prompt pending and must not be read as free text.
        if let Some(command) = Self::id_command(text) {
            return self.interpret_by_id(command, text);
        }

        let trimmed = text.trim().trim_start_matches('/').trim();
        if trimmed.is_empty() {
            return None;
        }
        let lower = trimmed.to_ascii_lowercase();

        match self.kind {
            PromptKind::Approval => match lower.as_str() {
                "y" | "yes" | "ok" | "approve" | "allow" | "1" => Some(Answer::Approve),
                "n" | "no" | "deny" | "reject" | "0" => Some(Answer::Deny),
                _ => None,
            },
            PromptKind::Ask => {
                // A numbered choice is unambiguous even when free text is
                // allowed, so it is tried first.
                if self.free_text {
                    if let Ok(n) = lower.parse::<usize>() {
                        if !self.options.is_empty() && n >= 1 && n <= self.options.len() {
                            return Some(Answer::Text(self.options[n - 1].clone()));
                        }
                    }
                    return Some(Answer::Text(trimmed.to_string()));
                }
                if let Ok(n) = lower.parse::<usize>() {
                    if n >= 1 && n <= self.options.len() {
                        return Some(Answer::Text(self.options[n - 1].clone()));
                    }
                    return None;
                }
                match lower.as_str() {
                    "y" | "yes" => Some(Answer::Approve),
                    "n" | "no" => Some(Answer::Deny),
                    _ => None,
                }
            }
        }
    }

    /// The by-id command a message uses, if it is one at all.
    ///
    /// Needs an argument: a bare `/approve` is the plain-word form and is left
    /// to the ordinary parsing below.
    fn id_command(text: &str) -> Option<&'static str> {
        let rest = text.trim().strip_prefix('/')?;
        let mut words = rest.split_whitespace();
        let command = match words.next()?.to_ascii_lowercase().as_str() {
            "answer" => "answer",
            "approve" => "approve",
            "deny" => "deny",
            _ => return None,
        };
        words.next()?;
        Some(command)
    }

    /// Resolve the explicit `/answer <id> <text>` / `/approve <id>` / `/deny
    /// <id>` form. `None` when the message names a different prompt, so the
    /// prompt stays pending rather than being lost to a mistyped id.
    fn interpret_by_id(&self, command: &str, text: &str) -> Option<Answer> {
        let rest = text.trim().strip_prefix('/')?;
        let mut parts = rest.splitn(3, char::is_whitespace);
        parts.next()?;
        if parts.next()? != self.id {
            return None;
        }
        let tail = parts.next().unwrap_or("").trim();
        match (command, self.kind) {
            ("answer", _) if !tail.is_empty() => Some(Answer::Text(tail.to_string())),
            ("approve", PromptKind::Approval) => Some(Answer::Approve),
            ("deny", PromptKind::Approval) => Some(Answer::Deny),
            _ => None,
        }
    }
}

/// How a chat message resolves a pending prompt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Answer {
    Approve,
    Deny,
    Text(String),
}

/// The in-flight prompt table.
///
/// Each entry remembers the channel it was posted to as well as the prompt, so
/// an answer must arrive on the *same* channel. Session keying alone is not
/// quite enough: several internal producers build an `IncomingMessage` with the
/// default provenance (`ExternalUser { channel: "unknown" }`) and the parent
/// session, and one of those answering a prompt would be a privileged decision
/// made by a tool rather than by the person.
#[derive(Debug, Default)]
pub struct PendingPrompts {
    entries: RwLock<HashMap<String, Held>>,
}

#[derive(Debug, Clone)]
struct Held {
    /// The channel instance the prompt was posted to.
    channel: String,
    prompt: PendingPrompt,
}

impl PendingPrompts {
    pub fn new() -> Self {
        Self::default()
    }

    /// Hold `prompt` for the session `key`, as posted to `channel`, replacing
    /// any previous one. A second prompt arrives only after the first was
    /// answered or timed out, so replacing is right; keeping both would mean an
    /// answer resolved a prompt the person is no longer looking at.
    pub async fn record(
        &self,
        key: impl Into<String>,
        channel: impl Into<String>,
        prompt: PendingPrompt,
    ) {
        self.entries.write().await.insert(
            key.into(),
            Held {
                channel: channel.into(),
                prompt,
            },
        );
    }

    /// Drop the entry for `key` if it is still `id`.
    ///
    /// Named rather than "remove whatever is there" so a timeout that fires
    /// late cannot delete a newer prompt.
    pub async fn clear_if(&self, key: &str, id: &str) {
        let mut entries = self.entries.write().await;
        if entries.get(key).map(|h| h.prompt.id.as_str()) == Some(id) {
            entries.remove(key);
        }
    }

    /// Drop every entry for the prompt `id`, whichever person holds it.
    ///
    /// Used when the prompt is resolved somewhere else — an operator clicking
    /// approve in the desktop UI — so a held entry cannot swallow the person's
    /// next plain message as an answer to a question nobody is waiting on.
    /// The key is not known to the resolution event, which is why this matches
    /// on the id across entries.
    pub async fn clear_prompt(&self, id: &str) {
        let mut entries = self.entries.write().await;
        entries.retain(|_, held| held.prompt.id != id);
    }

    /// Take the prompt for `key` if `text` answers it.
    ///
    /// The entry is removed only on a real answer: a message that is not an
    /// answer leaves the prompt waiting, so an unrelated remark mid-prompt does
    /// not lose it.
    pub async fn take(
        &self,
        key: &str,
        channel: &str,
        text: &str,
    ) -> Option<(PendingPrompt, Answer)> {
        let mut entries = self.entries.write().await;
        let held = entries.get(key)?;
        if held.channel != channel {
            return None;
        }
        let answer = held.prompt.interpret(text)?;
        let held = entries.remove(key)?;
        Some((held.prompt, answer))
    }

    /// How many prompts are outstanding (for status/metrics).
    pub async fn len(&self) -> usize {
        self.entries.read().await.len()
    }

    pub async fn is_empty(&self) -> bool {
        self.entries.read().await.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn approval() -> PendingPrompt {
        PendingPrompt {
            kind: PromptKind::Approval,
            id: "ap1".into(),
            options: vec![],
            free_text: false,
        }
    }

    fn choice() -> PendingPrompt {
        PendingPrompt {
            kind: PromptKind::Ask,
            id: "ask1".into(),
            options: vec!["red".into(), "blue".into()],
            free_text: false,
        }
    }

    fn free_text() -> PendingPrompt {
        PendingPrompt {
            kind: PromptKind::Ask,
            id: "ask2".into(),
            options: vec![],
            free_text: true,
        }
    }

    #[test]
    fn approval_understands_plain_words_and_slash() {
        let p = approval();
        assert_eq!(p.interpret("yes"), Some(Answer::Approve));
        assert_eq!(p.interpret("/approve"), Some(Answer::Approve));
        assert_eq!(p.interpret("OK"), Some(Answer::Approve));
        assert_eq!(p.interpret("no"), Some(Answer::Deny));
        assert_eq!(p.interpret("  0 "), Some(Answer::Deny));
        assert_eq!(p.interpret("what was that?"), None);
    }

    /// The by-id forms the chat prompt advertises must actually work — a user
    /// following the printed instruction is the common case.
    #[test]
    fn by_id_forms_answer_the_prompt() {
        let ap = approval();
        assert_eq!(ap.interpret("/approve ap1"), Some(Answer::Approve));
        assert_eq!(ap.interpret("/deny ap1"), Some(Answer::Deny));

        let ask = free_text();
        assert_eq!(ask.interpret("/answer ask2 Berlin"), Some(Answer::Text("Berlin".into())));

        // A different prompt's id must not resolve this one.
        assert_eq!(ap.interpret("/approve ap9"), None);
        assert_eq!(ask.interpret("/answer ask9 Berlin"), None);
        // An id alone is not a free-text answer.
        assert_eq!(ask.interpret("/answer ask2"), None);
        // `/approve` cannot answer a question.
        assert_eq!(ask.interpret("/approve ask2"), None);
    }

    #[test]
    fn numbered_choice_maps_to_option_text() {
        let p = choice();
        assert_eq!(p.interpret("2"), Some(Answer::Text("blue".into())));
        assert_eq!(p.interpret("3"), None);
    }

    #[test]
    fn free_text_takes_anything() {
        let p = free_text();
        assert_eq!(p.interpret("Berlin"), Some(Answer::Text("Berlin".into())));
        assert_eq!(p.interpret("   "), None);
    }

    #[tokio::test]
    async fn take_removes_only_on_answer() {
        let table = PendingPrompts::new();
        table.record("session-1", "wecom", approval()).await;

        // Not an answer: the prompt stays.
        assert!(table
            .take("session-1", "wecom", "hello there")
            .await
            .is_none());
        assert_eq!(table.len().await, 1);

        let (prompt, answer) = table
            .take("session-1", "wecom", "yes")
            .await
            .expect("answered");
        assert_eq!(prompt.id, "ap1");
        assert_eq!(answer, Answer::Approve);
        assert!(table.is_empty().await);
    }

    /// The answer must come back on the channel the prompt was posted to. Two
    /// bridges can share a session key in tests and in a proxy deployment, and
    /// a prompt must not be resolved by traffic from elsewhere.
    #[tokio::test]
    async fn an_answer_on_another_channel_is_not_an_answer() {
        let table = PendingPrompts::new();
        table.record("session-1", "wecom", approval()).await;
        assert!(table.take("session-1", "telegram", "yes").await.is_none());
        assert_eq!(table.len().await, 1);
        // The right channel still works.
        assert!(table.take("session-1", "wecom", "yes").await.is_some());
    }

    #[tokio::test]
    async fn stale_clear_does_not_drop_a_newer_prompt() {
        let table = PendingPrompts::new();
        table.record("session-1", "wecom", approval()).await;
        // A second prompt replaced the first; the first's timeout must not
        // clear it.
        table
            .record(
                "session-1",
                "wecom",
                PendingPrompt {
                    kind: PromptKind::Approval,
                    id: "ap2".into(),
                    options: vec![],
                    free_text: false,
                },
            )
            .await;
        table.clear_if("session-1", "ap1").await;
        assert_eq!(table.len().await, 1);
    }

    /// A prompt resolved elsewhere (desktop approve, CLI `/approve`) must stop
    /// swallowing the person's messages, whichever session it was recorded in.
    #[tokio::test]
    async fn clear_prompt_drops_the_entry_under_any_key() {
        let table = PendingPrompts::new();
        table.record("session-1", "wecom", approval()).await;
        table.clear_prompt("ap1").await;
        assert!(table.is_empty().await);
        assert!(table.take("session-1", "wecom", "yes").await.is_none());
    }
}
