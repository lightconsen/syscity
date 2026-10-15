//! Model tuning command handlers.

use super::*;

pub(super) async fn handle_model(
    req: &WsRequest,
    _conn: &Arc<RwLock<ProtocolConnection>>,
    state: &Arc<GatewayState>,
    args: &str,
) -> WsResponse {
    let trimmed = args.trim();
    if trimmed.is_empty() || trimmed == "status" {
        let cfg = state.config.read().await;
        let settings = state.infra.runtime_settings.read().await;
        let override_model = settings.get("model.override").and_then(|v| v.as_str());
        let text = format!(
            "🧠 **Model Status**\n\nDefault: {} (provider: {})\nOverride: {}",
            cfg.model,
            cfg.model_provider,
            override_model.unwrap_or("none")
        );
        return WsResponse::ok(&req.id, serde_json::json!({ "text": text }));
    }

    let mut settings = state.infra.runtime_settings.write().await;
    settings.insert("model.override".to_string(), serde_json::json!(trimmed));
    WsResponse::ok(
        &req.id,
        serde_json::json!({ "text": format!("🧠 Model override set to '{}'.", trimmed) }),
    )
}

pub(super) async fn handle_think(
    req: &WsRequest,
    state: &Arc<GatewayState>,
    args: &str,
) -> WsResponse {
    let level = args.trim();
    if level.is_empty() {
        let settings = state.infra.runtime_settings.read().await;
        let current = settings
            .get("think.level")
            .and_then(|v| v.as_str())
            .unwrap_or("medium");
        return WsResponse::ok(
            &req.id,
            serde_json::json!({ "text": format!("🧠 Thinking level: {}", current) }),
        );
    }
    let valid = ["off", "minimal", "low", "medium", "high"];
    if !valid.contains(&level) {
        return WsResponse::err(
            &req.id,
            "INVALID_ARGS",
            format!("Valid levels: {}", valid.join(", ")),
        );
    }
    let mut settings = state.infra.runtime_settings.write().await;
    settings.insert("think.level".to_string(), serde_json::json!(level));
    WsResponse::ok(
        &req.id,
        serde_json::json!({ "text": format!("🧠 Thinking level set to '{}'.", level) }),
    )
}

pub(super) async fn handle_verbose(
    req: &WsRequest,
    state: &Arc<GatewayState>,
    args: &str,
) -> WsResponse {
    let mode = args.trim();
    if mode.is_empty() {
        let settings = state.infra.runtime_settings.read().await;
        let current = settings
            .get("verbose.mode")
            .and_then(|v| v.as_str())
            .unwrap_or("off");
        return WsResponse::ok(
            &req.id,
            serde_json::json!({ "text": format!("🔊 Verbose mode: {}", current) }),
        );
    }
    let mut settings = state.infra.runtime_settings.write().await;
    settings.insert("verbose.mode".to_string(), serde_json::json!(mode));
    WsResponse::ok(
        &req.id,
        serde_json::json!({ "text": format!("🔊 Verbose mode set to '{}'.", mode) }),
    )
}

pub(super) async fn handle_trace(
    req: &WsRequest,
    state: &Arc<GatewayState>,
    args: &str,
) -> WsResponse {
    let mode = args.trim();
    if mode.is_empty() {
        let settings = state.infra.runtime_settings.read().await;
        let current = settings
            .get("trace.enabled")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        return WsResponse::ok(
            &req.id,
            serde_json::json!({ "text": format!("🔍 Plugin trace: {}", if current { "on" } else { "off" }) }),
        );
    }
    let enabled = mode == "on";
    state.infra.plugin_manager.set_trace_enabled(enabled);
    let mut settings = state.infra.runtime_settings.write().await;
    settings.insert("trace.enabled".to_string(), serde_json::json!(enabled));
    WsResponse::ok(
        &req.id,
        serde_json::json!({ "text": format!("🔍 Plugin trace {}.", if enabled { "enabled" } else { "disabled" }) }),
    )
}

pub(super) async fn handle_fast(
    req: &WsRequest,
    state: &Arc<GatewayState>,
    args: &str,
) -> WsResponse {
    let mode = args.trim();
    if mode.is_empty() || mode == "status" {
        let settings = state.infra.runtime_settings.read().await;
        let current = settings
            .get("fast.mode")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let active_model = settings
            .get("fast.active_model")
            .and_then(|v| v.as_str())
            .unwrap_or("default");
        return WsResponse::ok(
            &req.id,
            serde_json::json!({
                "text": format!(
                    "⚡ Fast mode: {} (active model: {})",
                    if current { "on" } else { "off" },
                    active_model
                )
            }),
        );
    }
    let enabled = mode == "on";

    if enabled {
        // The fast model is a configured concrete model ID (runtime setting);
        // fall back to the current default model when none is set.
        let fast_model = {
            let settings = state.infra.runtime_settings.read().await;
            settings
                .get("fast_model")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string())
        };
        let current_model = state.config.read().await.model.clone();
        let active_model = fast_model.clone().unwrap_or_else(|| current_model.clone());

        {
            let mut settings = state.infra.runtime_settings.write().await;
            settings.insert("fast.original_model".to_string(), serde_json::json!(current_model));
            settings.insert("fast.active_model".to_string(), serde_json::json!(active_model));
            settings.insert("fast.mode".to_string(), serde_json::json!(true));
        }

        if let Some(fast_model) = fast_model {
            WsResponse::ok(
                &req.id,
                serde_json::json!({ "text": format!("⚡ Fast mode enabled. Model switched to '{}'.", fast_model) }),
            )
        } else {
            WsResponse::ok(
                &req.id,
                serde_json::json!({ "text": "⚡ Fast mode enabled (no fast model configured, using current model)." }),
            )
        }
    } else {
        // Restore the original default model and clear fast state in a single
        // runtime_settings write. Config is no longer mutated directly.
        let (original, active_model) = {
            let settings = state.infra.runtime_settings.read().await;
            (
                settings
                    .get("fast.original_model")
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string()),
                settings
                    .get("fast.active_model")
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string()),
            )
        };

        {
            let mut settings = state.infra.runtime_settings.write().await;
            settings.insert("fast.mode".to_string(), serde_json::json!(false));
            if let Some(ref orig) = original {
                settings.insert("fast.active_model".to_string(), serde_json::json!(orig));
            } else {
                settings.remove("fast.active_model");
            }
        }

        WsResponse::ok(
            &req.id,
            serde_json::json!({
                "text": format!(
                    "⚡ Fast mode disabled. Model restored to '{}'.",
                    active_model.unwrap_or_else(|| "default".to_string())
                )
            }),
        )
    }
}

pub(super) async fn handle_reasoning(
    req: &WsRequest,
    state: &Arc<GatewayState>,
    args: &str,
) -> WsResponse {
    let mode = args.trim();
    if mode.is_empty() {
        let settings = state.infra.runtime_settings.read().await;
        let current = settings
            .get("reasoning.visibility")
            .and_then(|v| v.as_str())
            .unwrap_or("on");
        return WsResponse::ok(
            &req.id,
            serde_json::json!({ "text": format!("💭 Reasoning visibility: {}", current) }),
        );
    }
    let mut settings = state.infra.runtime_settings.write().await;
    settings.insert("reasoning.visibility".to_string(), serde_json::json!(mode));
    WsResponse::ok(
        &req.id,
        serde_json::json!({ "text": format!("💭 Reasoning visibility set to '{}'.", mode) }),
    )
}

/// `/queue [prompt | list | clear | rm N | edit N <text> | move A B | mode <mode>]`
///
/// Two things share this name, and the distinction is worth stating: the *mode*
/// (how an arriving message treats a running turn — steer, interrupt, follow-up)
/// and the *queue* (prompts waiting for the turn after this one). `/queue mode`
/// sets the first; a bare prompt adds to the second.
///
/// The routing follows hermes': a leading verb whose arguments fit manages the
/// queue, and anything else is a prompt — so `/queue clear the logs` queues
/// "clear the logs" rather than clearing the queue. `/queue add <prompt>` is the
/// way to force the other reading.
pub(super) async fn handle_queue(
    req: &WsRequest,
    conn: &Arc<RwLock<ProtocolConnection>>,
    state: &Arc<GatewayState>,
    args: &str,
    named_session: Option<&str>,
) -> WsResponse {
    let args = args.trim();
    let session_id = match named_session.filter(|id| !id.is_empty()) {
        Some(id) => id.to_string(),
        None => match conn.read().await.subscriptions.first().cloned() {
            Some(id) => id,
            None => {
                return WsResponse::err(
                    &req.id,
                    "INVALID_ARGS",
                    "no session_id given and this connection is not subscribed to one",
                )
            }
        },
    };

    let (verb, rest) = match args.split_once(char::is_whitespace) {
        Some((verb, rest)) => (verb.to_ascii_lowercase(), rest.trim()),
        None => (args.to_ascii_lowercase(), ""),
    };

    match (verb.as_str(), rest) {
        ("", "") | ("list", "") => return list_queue(req, state, &session_id).await,
        ("clear", "") => {
            let cleared = state.pipelines.prompt_queue.clear(&session_id).await;
            return WsResponse::ok(
                &req.id,
                serde_json::json!({
                    "text": if cleared == 1 {
                        "📥 Cleared 1 queued prompt.".to_string()
                    } else {
                        format!("📥 Cleared {cleared} queued prompts.")
                    },
                }),
            );
        }
        ("rm" | "remove", index) => {
            let Some(index) = parse_index(index) else {
                return usage_error(req, "/queue rm <n>");
            };
            return match state
                .pipelines
                .prompt_queue
                .remove(&session_id, index)
                .await
            {
                Some(removed) => WsResponse::ok(
                    &req.id,
                    serde_json::json!({
                        "text": format!("📥 Removed {index}: {}", preview(&removed.text)),
                    }),
                ),
                None => WsResponse::err(
                    &req.id,
                    "INVALID_ARGS",
                    format!("nothing queued at {index} in this session"),
                ),
            };
        }
        ("edit", rest) => {
            let (index, text) = match rest.split_once(char::is_whitespace) {
                Some((index, text)) => (parse_index(index), text.trim()),
                None => (None, ""),
            };
            let Some(index) = index.filter(|_| !text.is_empty()) else {
                return usage_error(req, "/queue edit <n> <text>");
            };
            if state
                .pipelines
                .prompt_queue
                .edit(&session_id, index, text.to_string())
                .await
            {
                return WsResponse::ok(
                    &req.id,
                    serde_json::json!({
                        "text": format!("📥 {index} is now: {}", preview(text)),
                    }),
                );
            }
            return WsResponse::err(
                &req.id,
                "INVALID_ARGS",
                format!("nothing queued at {index} in this session"),
            );
        }
        ("move", rest) => {
            let (from, to) = match rest.split_once(char::is_whitespace) {
                Some((from, to)) => (parse_index(from), parse_index(to)),
                None => (None, None),
            };
            let (Some(from), Some(to)) = (from, to) else {
                return usage_error(req, "/queue move <from> <to>");
            };
            if state
                .pipelines
                .prompt_queue
                .move_to(&session_id, from, to)
                .await
            {
                return WsResponse::ok(
                    &req.id,
                    serde_json::json!({ "text": format!("📥 Moved {from} to {to}.") }),
                );
            }
            return WsResponse::err(
                &req.id,
                "INVALID_ARGS",
                format!("{from} and {to} must both be positions in this session's queue"),
            );
        }
        ("mode", mode) => {
            let valid = ["steer", "interrupt", "followup"];
            if !valid.contains(&mode) {
                return WsResponse::err(
                    &req.id,
                    "INVALID_ARGS",
                    format!("Valid queue modes: {}", valid.join(", ")),
                );
            }
            let mut settings = state.infra.runtime_settings.write().await;
            settings.insert("queue.mode".to_string(), serde_json::json!(mode));
            return WsResponse::ok(
                &req.id,
                serde_json::json!({ "text": format!("📥 Queue mode set to '{mode}'.") }),
            );
        }
        ("add", prompt) if !prompt.is_empty() => {
            return enqueue(req, conn, state, &session_id, prompt).await;
        }
        _ => {}
    }

    // Whatever is left is a prompt. The old bare `/queue steer|interrupt|
    // followup` spelling is kept out of this path: it set the mode, and reading
    // it as "queue that word" would silently turn a configuration command into
    // a message.
    if matches!(args, "steer" | "interrupt" | "followup") {
        let mut settings = state.infra.runtime_settings.write().await;
        settings.insert("queue.mode".to_string(), serde_json::json!(args));
        return WsResponse::ok(
            &req.id,
            serde_json::json!({ "text": format!("📥 Queue mode set to '{args}'.") }),
        );
    }

    enqueue(req, conn, state, &session_id, args).await
}

async fn enqueue(
    req: &WsRequest,
    conn: &Arc<RwLock<ProtocolConnection>>,
    state: &Arc<GatewayState>,
    session_id: &str,
    prompt: &str,
) -> WsResponse {
    // Who queued it, so the reply goes back to them rather than to whoever
    // happens to be asking when it finally runs.
    let (user_id, channel) = {
        let cg = conn.read().await;
        (
            cg.user_id
                .as_ref()
                .map(|u| u.to_string())
                .unwrap_or_else(|| "user".to_string()),
            cg.client
                .as_ref()
                .map(|c| c.id.clone())
                .unwrap_or_else(|| "ws".to_string()),
        )
    };
    let position = state
        .pipelines
        .prompt_queue
        .enqueue(
            session_id,
            crate::gateway::prompt_queue::QueuedPrompt {
                text: prompt.to_string(),
                user_id,
                channel,
            },
        )
        .await;

    WsResponse::ok(
        &req.id,
        serde_json::json!({
            "text": format!(
                "📥 Queued at {position} — it runs when the current turn finishes: {}",
                preview(prompt)
            ),
        }),
    )
}

async fn list_queue(req: &WsRequest, state: &Arc<GatewayState>, session_id: &str) -> WsResponse {
    let queued = state.pipelines.prompt_queue.list(session_id).await;
    if queued.is_empty() {
        let mode = {
            let settings = state.infra.runtime_settings.read().await;
            settings
                .get("queue.mode")
                .and_then(|v| v.as_str())
                .unwrap_or("steer")
                .to_string()
        };
        return WsResponse::ok(
            &req.id,
            serde_json::json!({
                "text": format!(
                    "📥 Nothing queued. Mode: {mode}. /queue <prompt> queues one; \
                     list, rm N, edit N <text>, move A B, clear manage them."
                ),
                "queue": [],
            }),
        );
    }

    let mut lines = vec![format!("📥 {} queued:", queued.len())];
    for (index, prompt) in queued.iter().enumerate() {
        lines.push(format!("  {}. {}", index + 1, preview(&prompt.text)));
    }
    WsResponse::ok(
        &req.id,
        serde_json::json!({
            "text": lines.join("\n"),
            "queue": queued
                .iter()
                .map(|p| serde_json::json!({ "text": p.text, "channel": p.channel }))
                .collect::<Vec<_>>(),
        }),
    )
}

/// A 1-based position as `/queue list` prints it.
fn parse_index(text: &str) -> Option<usize> {
    let index: usize = text.trim().parse().ok()?;
    (index >= 1).then_some(index)
}

/// One line of a prompt, for a listing.
fn preview(text: &str) -> String {
    let flat = text.replace('\n', " ");
    if flat.chars().count() <= 60 {
        return flat;
    }
    let head: String = flat.chars().take(57).collect();
    format!("{head}…")
}

fn usage_error(req: &WsRequest, usage: &str) -> WsResponse {
    WsResponse::err(&req.id, "INVALID_ARGS", format!("Usage: {usage}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gateway::state_tests::{make_test_conn, make_test_state};
    use crate::gateway::GatewayConfig;

    fn req(id: &str) -> WsRequest {
        WsRequest {
            frame_type: "req".into(),
            id: id.into(),
            method: "x".into(),
            params: None,
        }
    }

    async fn state() -> Arc<GatewayState> {
        Arc::new(make_test_state(GatewayConfig::default()).await)
    }

    #[tokio::test]
    async fn model_status_when_no_args() {
        let state = state().await;
        let conn = make_test_conn(&[]);
        let resp = handle_model(&req("r1"), &conn, &state, "").await;
        assert!(resp.ok);
        let text = resp.payload.as_ref().unwrap()["text"].as_str().unwrap();
        assert!(text.contains("Default"), "status text should show default");
    }

    #[tokio::test]
    async fn model_sets_runtime_override() {
        let state = state().await;
        let conn = make_test_conn(&[]);
        let resp = handle_model(&req("r1"), &conn, &state, "  gpt-4.1-mini  ").await;
        assert!(resp.ok);
        let settings = state.infra.runtime_settings.read().await;
        assert_eq!(settings.get("model.override").and_then(|v| v.as_str()), Some("gpt-4.1-mini"));
    }

    #[tokio::test]
    async fn think_status_default_medium() {
        let state = state().await;
        let resp = handle_think(&req("r1"), &state, "").await;
        assert!(resp.ok);
        let text = resp.payload.as_ref().unwrap()["text"].as_str().unwrap();
        assert!(text.contains("medium"));
    }

    #[tokio::test]
    async fn think_invalid_level_errors() {
        let state = state().await;
        let resp = handle_think(&req("r1"), &state, "bogus").await;
        assert!(!resp.ok);
        assert_eq!(resp.error.as_ref().unwrap().code, "INVALID_ARGS");
    }

    #[tokio::test]
    async fn think_set_level_persists() {
        let state = state().await;
        let resp = handle_think(&req("r1"), &state, "high").await;
        assert!(resp.ok);
        let settings = state.infra.runtime_settings.read().await;
        assert_eq!(settings.get("think.level").and_then(|v| v.as_str()), Some("high"));
    }

    #[tokio::test]
    async fn verbose_set_and_status() {
        let state = state().await;
        let resp = handle_verbose(&req("r1"), &state, "on").await;
        assert!(resp.ok);
        let settings = state.infra.runtime_settings.read().await;
        assert_eq!(settings.get("verbose.mode").and_then(|v| v.as_str()), Some("on"));

        let resp = handle_verbose(&req("r2"), &state, "").await;
        assert!(resp.ok);
        assert!(resp.payload.as_ref().unwrap()["text"]
            .as_str()
            .unwrap()
            .contains("on"));
    }

    #[tokio::test]
    async fn trace_enable_and_status() {
        let state = state().await;
        let resp = handle_trace(&req("r1"), &state, "on").await;
        assert!(resp.ok);
        let settings = state.infra.runtime_settings.read().await;
        assert_eq!(settings.get("trace.enabled").and_then(|v| v.as_bool()), Some(true));

        let resp = handle_trace(&req("r2"), &state, "").await;
        assert!(resp.ok);
        assert!(resp.payload.as_ref().unwrap()["text"]
            .as_str()
            .unwrap()
            .contains("on"));
    }

    #[tokio::test]
    async fn fast_status_default_off() {
        let state = state().await;
        let resp = handle_fast(&req("r1"), &state, "").await;
        assert!(resp.ok);
        assert!(resp.payload.as_ref().unwrap()["text"]
            .as_str()
            .unwrap()
            .contains("off"));
    }

    #[tokio::test]
    async fn fast_enable_and_disable() {
        let state = state().await;
        let resp = handle_fast(&req("r1"), &state, "on").await;
        assert!(resp.ok);
        {
            let settings = state.infra.runtime_settings.read().await;
            assert_eq!(settings.get("fast.mode").and_then(|v| v.as_bool()), Some(true));
        }

        let resp = handle_fast(&req("r2"), &state, "off").await;
        assert!(resp.ok);
        {
            let settings = state.infra.runtime_settings.read().await;
            assert_eq!(settings.get("fast.mode").and_then(|v| v.as_bool()), Some(false));
        }
    }

    #[tokio::test]
    async fn reasoning_status_default_on() {
        let state = state().await;
        let resp = handle_reasoning(&req("r1"), &state, "").await;
        assert!(resp.ok);
        assert!(resp.payload.as_ref().unwrap()["text"]
            .as_str()
            .unwrap()
            .contains("on"));
    }

    /// A connection watching one session, which is what `/queue` without a
    /// session id resolves against.
    async fn conn_on(session: &str) -> Arc<RwLock<ProtocolConnection>> {
        let conn = make_test_conn(&["chat"]);
        conn.write().await.subscriptions.push(session.to_string());
        conn
    }

    #[tokio::test]
    async fn queue_takes_a_prompt() {
        let state = state().await;
        let conn = conn_on("s1").await;
        let resp = handle_queue(&req("r1"), &conn, &state, "summarise the logs", None).await;
        assert!(resp.ok);
        let text = resp.payload.as_ref().unwrap()["text"].as_str().unwrap();
        assert!(text.contains("Queued at 1"), "{text}");
        assert_eq!(
            state.pipelines.prompt_queue.list("s1").await.len(),
            1,
            "the prompt is in the session's queue"
        );
    }

    /// The routing rule, stated as a test: a leading verb with arguments that
    /// fit manages the queue, and everything else is a prompt — so "clear the
    /// logs" is a prompt, not a clear.
    #[tokio::test]
    async fn queue_distinguishes_a_management_verb_from_a_prompt() {
        let state = state().await;
        let conn = conn_on("s1").await;

        handle_queue(&req("r1"), &conn, &state, "clear the logs", None).await;
        handle_queue(&req("r2"), &conn, &state, "clear", None).await;

        // "clear the logs" was queued and then cleared by the bare verb.
        assert!(state.pipelines.prompt_queue.list("s1").await.is_empty());

        handle_queue(&req("r3"), &conn, &state, "add clear", None).await;
        let queued = state.pipelines.prompt_queue.list("s1").await;
        assert_eq!(queued.len(), 1, "`add` forces the other reading");
        assert_eq!(queued[0].text, "clear");
    }

    #[tokio::test]
    async fn queue_list_edit_move_and_rm() {
        let state = state().await;
        let conn = conn_on("s1").await;
        for text in ["first", "second"] {
            handle_queue(&req("r"), &conn, &state, text, None).await;
        }

        let listed = handle_queue(&req("r"), &conn, &state, "list", None).await;
        let text = listed.payload.as_ref().unwrap()["text"].as_str().unwrap();
        assert!(text.contains("1. first") && text.contains("2. second"), "{text}");
        assert_eq!(
            listed.payload.as_ref().unwrap()["queue"]
                .as_array()
                .unwrap()
                .len(),
            2
        );

        handle_queue(&req("r"), &conn, &state, "edit 1 first, edited", None).await;
        handle_queue(&req("r"), &conn, &state, "move 2 1", None).await;
        let queued = state.pipelines.prompt_queue.list("s1").await;
        assert_eq!(
            queued.iter().map(|p| p.text.as_str()).collect::<Vec<_>>(),
            vec!["second", "first, edited"]
        );

        handle_queue(&req("r"), &conn, &state, "rm 1", None).await;
        let queued = state.pipelines.prompt_queue.list("s1").await;
        assert_eq!(queued.len(), 1);
        assert_eq!(queued[0].text, "first, edited");

        // An index nobody is at is an error rather than a silent no-op.
        let resp = handle_queue(&req("r"), &conn, &state, "rm 5", None).await;
        assert!(!resp.ok);
        assert_eq!(resp.error.as_ref().unwrap().code, "INVALID_ARGS");
    }

    #[tokio::test]
    async fn queue_empty_listing_says_what_can_be_done() {
        let state = state().await;
        let conn = conn_on("s1").await;
        let resp = handle_queue(&req("r1"), &conn, &state, "list", None).await;
        let text = resp.payload.as_ref().unwrap()["text"].as_str().unwrap();
        assert!(text.contains("Nothing queued"), "{text}");
        assert!(text.contains("edit N"), "the usage is in the answer: {text}");
    }

    /// The mode keeps the spelling it always had, including bare, so an
    /// existing `/queue interrupt` does not start queuing the word.
    #[tokio::test]
    async fn queue_set_mode_persists() {
        let state = state().await;
        let conn = conn_on("s1").await;
        for args in ["interrupt", "mode steer"] {
            let resp = handle_queue(&req("r1"), &conn, &state, args, None).await;
            assert!(resp.ok, "{args}");
        }
        let settings = state.infra.runtime_settings.read().await;
        assert_eq!(settings.get("queue.mode").and_then(|v| v.as_str()), Some("steer"));
        assert!(
            state.pipelines.prompt_queue.list("s1").await.is_empty(),
            "setting the mode is not a prompt"
        );

        let resp = handle_queue(&req("r1"), &conn, &state, "mode bogus", None).await;
        assert!(!resp.ok);
        assert_eq!(resp.error.as_ref().unwrap().code, "INVALID_ARGS");
    }
}
