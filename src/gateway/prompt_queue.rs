//! Prompts queued for a session's next turn.
//!
//! A message that arrives while a turn is running has three ways to behave in
//! this gateway — steer the running turn, interrupt it, or be collected
//! ([`crate::inbound::queue::QueueMode`]) — and none of them is "wait your turn,
//! and let me change my mind first". That is what this is: a per-session line of
//! prompts that the agent picks up when the turn it was queued behind finishes,
//! and that `/queue` can inspect, edit, reorder and clear in the meantime.
//!
//! The queue holds *text*, not a message: what was queued is a prompt, and it is
//! turned into an [`crate::channels::IncomingMessage`] at the moment it is sent,
//! with the sender and channel it was queued for. Anything else would freeze
//! decisions (routing, provenance) that belong to the moment of sending.

use std::collections::{HashMap, VecDeque};

use tokio::sync::RwLock;

/// One prompt waiting for its turn.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QueuedPrompt {
    /// What to send.
    pub text: String,
    /// Who queued it — the reply goes back to them.
    pub user_id: String,
    /// The channel it was queued from, for the reply's route and for display.
    pub channel: String,
}

/// Prompts queued per session, in the order they will be sent.
#[derive(Debug, Default)]
pub struct PendingPrompts {
    sessions: RwLock<HashMap<String, VecDeque<QueuedPrompt>>>,
}

impl PendingPrompts {
    pub fn new() -> Self {
        Self::default()
    }

    /// Append a prompt. Returns its 1-based position, which is what the other
    /// operations take — the numbers `/queue list` prints.
    pub async fn enqueue(&self, session_id: &str, prompt: QueuedPrompt) -> usize {
        let mut sessions = self.sessions.write().await;
        let queue = sessions.entry(session_id.to_string()).or_default();
        queue.push_back(prompt);
        queue.len()
    }

    /// Everything queued for a session, oldest first.
    pub async fn list(&self, session_id: &str) -> Vec<QueuedPrompt> {
        self.sessions
            .read()
            .await
            .get(session_id)
            .map(|queue| queue.iter().cloned().collect())
            .unwrap_or_default()
    }

    /// Take the next prompt, if any. This is the drain the agent calls.
    pub async fn take(&self, session_id: &str) -> Option<QueuedPrompt> {
        let mut sessions = self.sessions.write().await;
        let queue = sessions.get_mut(session_id)?;
        let prompt = queue.pop_front();
        if queue.is_empty() {
            sessions.remove(session_id);
        }
        prompt
    }

    /// Drop and return the prompt at `index` (1-based).
    pub async fn remove(&self, session_id: &str, index: usize) -> Option<QueuedPrompt> {
        let mut sessions = self.sessions.write().await;
        let queue = sessions.get_mut(session_id)?;
        let removed = index
            .checked_sub(1)
            .filter(|i| *i < queue.len())
            .and_then(|i| queue.remove(i));
        if queue.is_empty() {
            sessions.remove(session_id);
        }
        removed
    }

    /// Replace the text of the prompt at `index` (1-based), keeping its sender.
    pub async fn edit(&self, session_id: &str, index: usize, text: String) -> bool {
        let mut sessions = self.sessions.write().await;
        let Some(queue) = sessions.get_mut(session_id) else {
            return false;
        };
        match index.checked_sub(1).and_then(|i| queue.get_mut(i)) {
            Some(prompt) => {
                prompt.text = text;
                true
            }
            None => false,
        }
    }

    /// Move the prompt at `from` to position `to` (both 1-based).
    pub async fn move_to(&self, session_id: &str, from: usize, to: usize) -> bool {
        let mut sessions = self.sessions.write().await;
        let Some(queue) = sessions.get_mut(session_id) else {
            return false;
        };
        let (Some(from), Some(to)) = (
            from.checked_sub(1).filter(|i| *i < queue.len()),
            to.checked_sub(1).filter(|i| *i < queue.len()),
        ) else {
            return false;
        };
        // Both indices are in range, so the removal yields the prompt.
        if let Some(prompt) = queue.remove(from) {
            queue.insert(to, prompt);
            return true;
        }
        false
    }

    /// Drop everything queued for a session; returns how many went.
    pub async fn clear(&self, session_id: &str) -> usize {
        let mut sessions = self.sessions.write().await;
        sessions
            .remove(session_id)
            .map(|queue| queue.len())
            .unwrap_or(0)
    }
}

/// Hand a session's next queued prompt back to the inbound pipeline.
///
/// This is the drain: it is called when a turn finishes, because a prompt was
/// queued behind that turn. It goes to the *inbound entry* rather than straight
/// to the agent so it is a message like any other — routed, written to the
/// history, and answered to the channel it came from.
///
/// Returns whether a prompt was released. A send that fails is reported by the
/// caller: the prompt has already left the queue, so the alternative to saying
/// so is losing it quietly.
pub async fn release_next(
    pipelines: &crate::gateway::state::PipelineState,
    session_id: &str,
) -> Option<Result<(), String>> {
    let queued = pipelines.prompt_queue.take(session_id).await?;
    let message = crate::channels::IncomingMessage::new(queued.user_id, session_id, queued.text)
        .with_provenance(crate::channels::InputProvenance::InternalSystem {
            source: "queued_prompt".to_string(),
        });
    Some(
        pipelines
            .inbound_entry
            .send(message)
            .await
            .map_err(|e| e.to_string()),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn prompt(text: &str) -> QueuedPrompt {
        QueuedPrompt {
            text: text.to_string(),
            user_id: "u".to_string(),
            channel: "tui".to_string(),
        }
    }

    async fn texts(queue: &PendingPrompts, session: &str) -> Vec<String> {
        queue
            .list(session)
            .await
            .into_iter()
            .map(|p| p.text)
            .collect()
    }

    #[tokio::test]
    async fn prompts_come_back_in_the_order_they_were_queued() {
        let queue = PendingPrompts::new();
        assert_eq!(queue.enqueue("s1", prompt("first")).await, 1);
        assert_eq!(queue.enqueue("s1", prompt("second")).await, 2);

        assert_eq!(texts(&queue, "s1").await, vec!["first", "second"]);
        assert_eq!(queue.take("s1").await.map(|p| p.text), Some("first".into()));
        assert_eq!(queue.take("s1").await.map(|p| p.text), Some("second".into()));
        assert_eq!(queue.take("s1").await, None, "an empty queue yields nothing");
        assert!(texts(&queue, "s1").await.is_empty());
    }

    #[tokio::test]
    async fn sessions_do_not_share_a_queue() {
        let queue = PendingPrompts::new();
        queue.enqueue("s1", prompt("mine")).await;
        queue.enqueue("s2", prompt("theirs")).await;

        assert_eq!(texts(&queue, "s1").await, vec!["mine"]);
        assert_eq!(queue.clear("s1").await, 1);
        assert_eq!(texts(&queue, "s2").await, vec!["theirs"]);
    }

    #[tokio::test]
    async fn edit_keeps_the_position_and_reports_an_index_that_is_not_there() {
        let queue = PendingPrompts::new();
        queue.enqueue("s1", prompt("one")).await;
        queue.enqueue("s1", prompt("two")).await;

        assert!(queue.edit("s1", 1, "one, edited".to_string()).await);
        assert_eq!(texts(&queue, "s1").await, vec!["one, edited", "two"]);
        // 0 and out-of-range are refused rather than wrapping around to the end.
        assert!(!queue.edit("s1", 0, "x".to_string()).await);
        assert!(!queue.edit("s1", 3, "x".to_string()).await);
        assert_eq!(texts(&queue, "s1").await, vec!["one, edited", "two"]);
    }

    #[tokio::test]
    async fn move_is_the_reorder_everyone_expects() {
        let queue = PendingPrompts::new();
        for text in ["a", "b", "c"] {
            queue.enqueue("s1", prompt(text)).await;
        }

        assert!(queue.move_to("s1", 3, 1).await, "c to the front");
        assert_eq!(texts(&queue, "s1").await, vec!["c", "a", "b"]);
        assert!(!queue.move_to("s1", 1, 4).await, "4 is past the end");
        assert!(!queue.move_to("s1", 0, 1).await);
        assert_eq!(texts(&queue, "s1").await, vec!["c", "a", "b"]);
    }

    /// The drain: one prompt leaves the queue and becomes an inbound message,
    /// with the sender it was queued for — which is what makes the reply go
    /// back to them rather than to whoever asks next.
    #[tokio::test]
    async fn releasing_a_prompt_hands_it_to_the_inbound_entry() {
        use crate::gateway::state_tests::make_test_state_parts;

        let (state, mut entry_rx) =
            make_test_state_parts(crate::gateway::GatewayConfig::default()).await;
        state
            .pipelines
            .prompt_queue
            .enqueue(
                "s1",
                QueuedPrompt {
                    text: "and then?".to_string(),
                    user_id: "alice".to_string(),
                    channel: "tui".to_string(),
                },
            )
            .await;

        let released = release_next(&state.pipelines, "s1").await;
        assert!(matches!(released, Some(Ok(()))));

        let message = entry_rx
            .recv()
            .await
            .expect("the prompt reached the pipeline");
        assert_eq!(message.content, "and then?");
        assert_eq!(message.user_id.to_string(), "alice");
        assert_eq!(message.conversation_id.to_string(), "s1");

        // Nothing left, nothing released: a drain that ran twice must not
        // invent a message.
        assert!(release_next(&state.pipelines, "s1").await.is_none());
    }

    #[tokio::test]
    async fn removing_the_last_prompt_forgets_the_session() {
        let queue = PendingPrompts::new();
        queue.enqueue("s1", prompt("only")).await;

        assert_eq!(queue.remove("s1", 1).await.map(|p| p.text), Some("only".into()));
        assert_eq!(queue.remove("s1", 1).await, None);
        assert!(queue.sessions.read().await.is_empty(), "no empty queue is left behind");
    }
}
