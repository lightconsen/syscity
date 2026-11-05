//! Put the screen in front of the person, wait, and take it back.

use super::*;

/// Wait until a steer arrives, or forever when nothing can steer.
///
/// `select!` needs a future in every arm; outside an agent turn there is no
/// controller, and "nothing will ever arrive" is the honest shape of that.
async fn wait_for_steer(controller: Option<&crate::acp::ExecutionController>) {
    match controller {
        Some(controller) => controller.steer_notified().await,
        None => std::future::pending().await,
    }
}

/// Hand the agent's display to the person, wait, and take it back.
#[derive(Debug)]
pub struct AdbHandToUserTool {
    device: Option<String>,
}

impl Default for AdbHandToUserTool {
    fn default() -> Self {
        Self::new()
    }
}

impl AdbHandToUserTool {
    pub fn new() -> Self {
        Self { device: None }
    }

    pub fn with_device(mut self, device: String) -> Self {
        self.device = Some(device);
        self
    }
}

#[async_trait]
impl Tool for AdbHandToUserTool {
    fn name(&self) -> &str {
        "android_hand_to_user"
    }

    fn description(&self) -> &str {
        "Stop and ask the person to do a step you cannot: a login, a captcha, a payment \
         confirmation. Their app is brought onto their own screen so they can touch it, the \
         question tells them what is needed, and when they answer it comes back to you — with \
         the screen re-read, so you can see whether it actually changed before acting on their \
         word. Use this instead of guessing at a password field or tapping through a captcha; \
         use it once per blockage, not as a retry loop."
    }

    fn parameters_schema(&self) -> Value {
        create_schema(
            "Ask the person to take over",
            serde_json::json!({
                "display": {
                    "type": "integer",
                    "description": "The display the agent is working on (from android_display start)."
                },
                "question": {
                    "type": "string",
                    "description": "What you need them to do, in their words. Say what to do and how to tell you it is finished."
                },
                "timeout_seconds": {
                    "type": "integer",
                    "description": "How long to wait (default 300, max 1800)."
                },
                "device": { "type": "string", "description": DEVICE_PARAM }
            }),
            vec!["display", "question"],
        )
    }

    async fn execute(
        &self,
        args: Value,
        context: &ToolContext,
    ) -> crate::Result<ToolExecutionResult> {
        let Some(display_id) = args.get("display").and_then(|v| v.as_i64()) else {
            return Ok(ToolExecutionResult::error(
                "`display` is required — the id android_display start returned",
            ));
        };
        let display_id = i32::try_from(display_id).unwrap_or(-1);
        let question = args
            .get("question")
            .and_then(|v| v.as_str())
            .unwrap_or("Please finish this step, then say what you did.")
            .to_string();
        let timeout_secs = args
            .get("timeout_seconds")
            .and_then(|v| v.as_u64())
            .unwrap_or(300)
            .clamp(30, 1800);

        // The display is only reachable from the app itself; a desktop driving
        // a remote phone has no screen to hand over.
        let Ok(bridge) = display_bridge() else {
            return Ok(ToolExecutionResult::error(
                "handing a screen to the user needs the Syscity app on the phone; this host has \
                 no display helper",
            ));
        };

        // No human, no takeover. Refusing beats blocking a background turn for
        // five minutes waiting for someone who was never there.
        let Some(queue) = context.ask_queue.clone() else {
            return Ok(ToolExecutionResult::error(
                "there is nobody to hand this to in this context (no interactive channel)",
            ));
        };

        // The fingerprint *before* the hand-back is what makes "I'm done" a
        // checkable claim.
        let before = super::dump_ui_elements(&self.device, super::Target::Display(display_id))
            .await
            .ok()
            .map(|elements| super::tree_fingerprint(&elements));

        let conversation_id = context.conversation_id.clone();
        // A steer that arrives while the question is open is the person still
        // talking about the step they were handed — they typed at the agent
        // rather than pressing a button. It belongs with the handoff's result,
        // not after it, so the wait watches for it and leaves it as a note.
        let notices_context = context.clone();
        let ask = Box::new(move || {
            Box::pin(async move {
                use crate::tools::ask_user::AskRequest;
                use crate::tools::ask_user::PendingQuestion;
                let (tx, rx) = tokio::sync::oneshot::channel();
                let id = format!("takeover-{}", uuid::Uuid::new_v4());
                queue
                    .submit(PendingQuestion {
                        request: AskRequest {
                            id: id.clone(),
                            conversation_id,
                            question,
                            options: vec![
                                "Done".to_string(),
                                "I'll do the rest myself".to_string(),
                                "Skip this step".to_string(),
                            ],
                            required: true,
                            default: None,
                        },
                        response_tx: Some(tx),
                    })
                    .await;
                let deadline = tokio::time::sleep(std::time::Duration::from_secs(timeout_secs));
                tokio::pin!(deadline);
                let mut answer = rx;
                let controller = notices_context.steers.clone();
                // Waiting for the answer and for anything typed at the agent
                // are the same wait: the person may answer with either. What
                // they typed is moved out of the controller here rather than
                // left for the next iteration, which would be too late — the
                // loop only drains between tool calls.
                //
                // Drained both before sleeping and again after waking, so a
                // message that arrived before this tool started is not left
                // behind either. The remaining gap — a steer landing between
                // the drain and the wait registering — is not a loss: it stays
                // in the queue and reaches the model at the top of the next
                // iteration, which is where it would have gone anyway.
                loop {
                    if let Some(controller) = controller.as_deref() {
                        for steer in controller.drain_steers() {
                            notices_context.note(steer);
                        }
                    }
                    tokio::select! {
                        answered = &mut answer => {
                            return answered.ok();
                        }
                        _ = &mut deadline => {
                            queue.cancel(&id).await;
                            return None;
                        }
                        _ = wait_for_steer(controller.as_deref()) => {}
                    }
                }
            })
                as std::pin::Pin<Box<dyn std::future::Future<Output = Option<String>> + Send>>
        });

        // The hold outlives the question by a minute: the question's own
        // timeout ends it for the caller, and this covers the gap between that
        // and taking the display back. Without it a cancelled turn would leave
        // the display unwritable until the helper died.
        //
        // No `TargetLocks` guard is taken here, deliberately: it would block
        // this phone for as long as the person takes. What holds the display
        // instead is the helper's own takeover flag (`giveToUser`), which every
        // write checks on the device — so it covers a session on another host,
        // or one that connected while this handoff was already in flight.
        let hold = std::time::Duration::from_secs(timeout_secs + 60);
        let outcome =
            match super::takeover::hand_to_user(&bridge, display_id, before, hold, ask).await {
                Ok(outcome) => outcome,
                Err(failed) => return Ok(ToolExecutionResult::error(failed.to_string())),
            };

        Ok(ToolExecutionResult::success(super::takeover::describe(&outcome)).with_data(
            serde_json::json!({
                "verdict": outcome.verdict,
                "answer": outcome.answer,
                "screen_changed": outcome.screen_changed,
                "display": display_id,
                "elements": outcome.elements,
            }),
        ))
    }
}
