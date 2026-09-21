//! The two prompts that take the keyboard over: approvals and questions.

use std::sync::Arc;

use tokio::sync::RwLock;

use crate::tui::actions::TuiAction;
use crate::tui::error::TuiError;
use crate::tui::gateway_calls as gw;
use crate::tui::state::{AppState, ApprovalChoice, AskPrompt, LiveMode};
use crate::tui::ws_client::WsClient;

/// Handle a key action while an approval is pending.
pub(super) async fn handle_approval_action(
    action: TuiAction,
    state: &Arc<RwLock<AppState>>,
    ws: &WsClient,
) -> Result<(), TuiError> {
    let (choice, decide) = match action {
        // `y`/Enter confirm the highlighted choice, `n`/Esc deny outright,
        // `a` approves and remembers, arrows move the highlight.
        TuiAction::InputChar('y') | TuiAction::InputChar('Y') | TuiAction::SendMessage => {
            let choice = state.read().await.approval_selection;
            (Some(choice), true)
        }
        TuiAction::InputChar('a') | TuiAction::InputChar('A') => {
            (Some(ApprovalChoice::ApproveRemember), true)
        }
        TuiAction::InputChar('n') | TuiAction::InputChar('N') | TuiAction::Escape => {
            (Some(ApprovalChoice::Deny), true)
        }
        TuiAction::CursorLeft | TuiAction::CursorRight => {
            let mut s = state.write().await;
            s.approval_selection = match s.approval_selection {
                ApprovalChoice::Approve => ApprovalChoice::ApproveRemember,
                ApprovalChoice::ApproveRemember => ApprovalChoice::Deny,
                ApprovalChoice::Deny => ApprovalChoice::Approve,
            };
            (None, false)
        }
        TuiAction::Quit => {
            state.write().await.should_quit = true;
            (None, false)
        }
        // Ctrl+C gets the keyboard back. It cannot answer the approval — that
        // is the gateway's to resolve, and a tool call blocked on a human
        // stays blocked — but a prompt that swallows every key with no way out
        // is a trap, and this is the key that means "stop waiting on this"
        // everywhere else in the TUI.
        TuiAction::Abort => {
            let mut s = state.write().await;
            s.pop_approval();
            s.transcript.push_notice(
                "⚠ approval left unanswered — the tool call stays blocked on the gateway until \
                 it times out; another client can still answer it",
            );
            (None, false)
        }
        // Everything else is swallowed: while a tool is blocked on a human,
        // typing must not go into the composer.
        _ => (None, false),
    };

    let Some(approval) = state.read().await.current_approval().cloned() else {
        state.write().await.live_mode = LiveMode::Composer;
        return Ok(());
    };
    let Some(choice) = choice else {
        return Ok(());
    };
    if !decide {
        return Ok(());
    }

    let approve = choice != ApprovalChoice::Deny;
    let remember = choice == ApprovalChoice::ApproveRemember;
    match gw::approvals_decide(ws, &approval.id, approve, remember, None).await {
        Ok(remembered_rule) => {
            let mut s = state.write().await;
            let mark = if approve {
                "✔ approved"
            } else {
                "✘ denied"
            };
            s.transcript.push_notice(format!(
                "{mark} {} (risk: {})",
                approval.tool_name, approval.risk_level
            ));
            if let Some(rule) = remembered_rule {
                s.transcript
                    .push_notice(format!("✓ will not ask again: {rule}"));
            }
            s.pop_approval();
            s.dirty = true;
        }
        // `NOT_FOUND` means the approval is gone — resolved by another client,
        // expired, or the turn ended. Retrying cannot succeed, and the prompt
        // owns the keyboard, so it has to go.
        Err(TuiError::Gateway { ref code, .. }) if code == "NOT_FOUND" => {
            let mut s = state.write().await;
            s.transcript.push_notice(format!(
                "⚠ {} was already resolved — the {} was not recorded",
                approval.tool_name,
                if approve { "approval" } else { "denial" }
            ));
            s.pop_approval();
            s.dirty = true;
        }
        // Anything else may pass on a second try, and the tool call is still
        // blocked on this prompt: dropping it would leave the human with no
        // way to unblock anything.
        Err(e) => {
            state.write().await.transcript.push_notice(format!(
                "✘ could not answer the approval: {e} — y/a or n tries again, Ctrl+C dismisses"
            ));
        }
    }
    Ok(())
}

/// The answer a typed line gives to a pending question.
///
/// A digit picks the option at that position; otherwise the text is the answer,
/// falling back to the agent's own default. `None` means there is nothing to
/// send — an empty line and no default — which is not the same as an empty
/// answer: `ask.respond` rejects that outright.
fn resolve_ask_answer(ask: &AskPrompt, typed: &str) -> Option<String> {
    let typed = typed.trim();
    let answer = if let Ok(idx) = typed.parse::<usize>() {
        ask.options
            .get(idx.saturating_sub(1))
            .cloned()
            .unwrap_or_else(|| typed.to_string())
    } else if !typed.is_empty() {
        typed.to_string()
    } else {
        ask.default.clone().unwrap_or_default()
    };
    (!answer.trim().is_empty()).then_some(answer)
}

/// Handle a key action while the agent's question is pending.
pub(super) async fn handle_ask_action(
    action: TuiAction,
    state: &Arc<RwLock<AppState>>,
    ws: &WsClient,
) -> Result<(), TuiError> {
    let (submit, dismiss) = match action {
        TuiAction::SendMessage => (true, false),
        TuiAction::Escape => (false, true),
        TuiAction::Quit => {
            state.write().await.should_quit = true;
            (false, false)
        }
        TuiAction::InputChar(c) if c.is_ascii_digit() => {
            let mut s = state.write().await;
            let options = s.pending_ask.as_ref().map(|a| a.options.len()).unwrap_or(0);
            let idx = c.to_digit(10).unwrap_or(0) as usize;
            if idx >= 1 && idx <= options {
                s.ask_input = c.to_string();
            }
            (false, false)
        }
        TuiAction::InputChar(c) => {
            state.write().await.ask_input.push(c);
            (false, false)
        }
        TuiAction::InputBackspace => {
            state.write().await.ask_input.pop();
            (false, false)
        }
        _ => (false, false),
    };

    if dismiss {
        // Matching the web UI: the question stays pending server-side until it
        // times out, and `/answer` can still pick it up.
        let mut s = state.write().await;
        s.live_mode = LiveMode::Composer;
        s.transcript.push_notice(
            "question left unanswered — /answer <text> will still reach it (it times out in 5 minutes)",
        );
        return Ok(());
    }
    if !submit {
        return Ok(());
    }

    let (ask, typed) = {
        let s = state.read().await;
        (s.pending_ask.clone(), s.ask_input.clone())
    };
    let Some(ask) = ask else {
        state.write().await.live_mode = LiveMode::Composer;
        return Ok(());
    };
    let Some(answer) = resolve_ask_answer(&ask, &typed) else {
        return Ok(());
    };

    match gw::ask_respond(ws, &ask.ask_id, &answer).await {
        Ok(()) => {
            let mut s = state.write().await;
            s.pending_ask = None;
            s.ask_input.clear();
            s.live_mode = LiveMode::Composer;
            s.transcript.push_notice(format!("answered: {answer}"));
            s.dirty = true;
        }
        Err(e) => {
            state
                .write()
                .await
                .transcript
                .push_notice(format!("✘ could not answer: {e}"));
        }
    }
    Ok(())
}

/// Answer a pending prompt from a typed line.
///
/// Only reached when stdin is a terminal. A pipe's lines are messages: there
/// is no reading a `y` that was never typed.
pub(super) async fn answer_prompt_from_line(
    line: &str,
    state: &Arc<RwLock<AppState>>,
    ws: &WsClient,
) -> Result<(), TuiError> {
    // Bound first: a `match` on a temporary guard keeps it alive for the whole
    // block, and the arms below take the write lock.
    let mode = state.read().await.live_mode;
    match mode {
        LiveMode::Composer => return Ok(()),
        LiveMode::Approval => {
            let Some(approval) = state.read().await.current_approval().cloned() else {
                state.write().await.live_mode = LiveMode::Composer;
                return Ok(());
            };
            let approve = match line.trim().to_ascii_lowercase().as_str() {
                "y" | "yes" | "approve" => true,
                "n" | "no" | "deny" => false,
                other => {
                    state.write().await.transcript.push_notice(format!(
                        "⚠ answer the prompt for {}: y approves it, n denies it (got {other:?})",
                        approval.tool_name
                    ));
                    return Ok(());
                }
            };
            match gw::approvals_decide(ws, &approval.id, approve, false, None).await {
                Ok(_remembered) => {
                    let mut s = state.write().await;
                    s.transcript.push_notice(format!(
                        "{} {}",
                        if approve {
                            "✔ approved"
                        } else {
                            "✘ denied"
                        },
                        approval.tool_name
                    ));
                    s.pop_approval();
                }
                Err(TuiError::Gateway { ref code, .. }) if code == "NOT_FOUND" => {
                    let mut s = state.write().await;
                    s.transcript
                        .push_notice(format!("⚠ {} was already resolved", approval.tool_name));
                    s.pop_approval();
                }
                Err(e) => {
                    state.write().await.transcript.push_notice(format!(
                        "✘ could not answer the approval: {e} — send y or n again"
                    ));
                }
            }
        }
        LiveMode::Ask => {
            let Some(ask) = state.read().await.pending_ask.clone() else {
                state.write().await.live_mode = LiveMode::Composer;
                return Ok(());
            };
            let Some(answer) = resolve_ask_answer(&ask, line) else {
                state
                    .write()
                    .await
                    .transcript
                    .push_notice("⚠ type an answer, or the number of an option");
                return Ok(());
            };
            match gw::ask_respond(ws, &ask.ask_id, &answer).await {
                Ok(()) => {
                    let mut s = state.write().await;
                    s.pending_ask = None;
                    s.live_mode = LiveMode::Composer;
                    s.transcript.push_notice(format!("answered: {answer}"));
                }
                Err(e) => {
                    state
                        .write()
                        .await
                        .transcript
                        .push_notice(format!("✘ could not answer: {e}"));
                }
            }
        }
    }
    Ok(())
}

/// Resolve a prompt nobody can answer.
///
/// Line mode usually drives a pipe: stdin is a file or another process, and no
/// one will ever type `y`. A prompt that waits for a human who cannot answer
/// blocks every line behind it until the gateway times it out, so it is settled
/// here instead — immediately, and out loud.
pub(super) async fn answer_prompt_without_a_human(
    state: &Arc<RwLock<AppState>>,
    ws: &WsClient,
) -> Result<(), TuiError> {
    loop {
        // Same reason as above: the guard must not outlive this line.
        let mode = state.read().await.live_mode;
        match mode {
            LiveMode::Composer => return Ok(()),
            LiveMode::Approval => {
                // Denying is the fail-closed answer: the tool does not run,
                // and the turn carries on with the refusal.
                let Some(approval) = state.read().await.current_approval().cloned() else {
                    state.write().await.live_mode = LiveMode::Composer;
                    return Ok(());
                };
                let outcome = gw::approvals_decide(ws, &approval.id, false, false, None).await;
                let mut s = state.write().await;
                match outcome {
                    Ok(_remembered) => s.transcript.push_notice(format!(
                        "✘ denied {} — nothing here can answer an approval prompt",
                        approval.tool_name
                    )),
                    Err(e) => s
                        .transcript
                        .push_notice(format!("✘ could not deny {}: {e}", approval.tool_name)),
                }
                // Either way the prompt goes: leaving it up would block every
                // later line behind a question nobody can answer.
                s.pop_approval();
            }
            LiveMode::Ask => {
                let Some(ask) = state.read().await.pending_ask.clone() else {
                    state.write().await.live_mode = LiveMode::Composer;
                    return Ok(());
                };
                // The agent's own default answers the question; without one
                // there is nothing to send that `ask.respond` would accept, and
                // the question would hold the turn for its full timeout — so
                // the turn is stopped instead.
                let Some(answer) = ask.default.clone().filter(|d| !d.trim().is_empty()) else {
                    // Stopping the turn is the only way to decline a question
                    // the gateway will not accept an empty answer for.
                    let session = state.read().await.current_session.clone();
                    let stopped = match session {
                        Some(id) => gw::chat_abort(ws, &id).await.is_ok(),
                        None => true,
                    };
                    let mut s = state.write().await;
                    s.transcript.push_notice(if stopped {
                        "⚠ the agent asked a question with no default and nothing here can \
                         answer it — the turn was stopped"
                    } else {
                        "⚠ the agent asked a question with no default and nothing here can \
                         answer it, and the turn could not be stopped — the gateway will time \
                         it out"
                    });
                    // Either way the prompt goes: nothing here can answer it,
                    // and holding it would stall the rest of the pipe.
                    s.pending_ask = None;
                    s.live_mode = LiveMode::Composer;
                    s.end_run();
                    return Ok(());
                };
                let outcome = gw::ask_respond(ws, &ask.ask_id, &answer).await;
                let mut s = state.write().await;
                match outcome {
                    Ok(()) => s
                        .transcript
                        .push_notice(format!("answered with the default: {answer}")),
                    Err(e) => s
                        .transcript
                        .push_notice(format!("✘ could not answer the question: {e}")),
                }
                s.pending_ask = None;
                s.live_mode = LiveMode::Composer;
            }
        }
    }
}
