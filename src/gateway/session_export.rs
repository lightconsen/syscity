//! Exporting a session's conversation to a file.
//!
//! One path, two callers: the `sessions.export` method (what the TUI's `/save`
//! calls) and the `/export-session` command. They used to disagree — the command
//! wrote HTML and nothing else, from the transcript buffer this process happened
//! to be holding — so a format you could ask for over one was not available over
//! the other, and a session untouched since the last start could not be exported
//! at all.
//!
//! The conversation comes from the **session store**: it is the durable record,
//! and reading it is what makes the two callers agree about which conversations
//! exist as well as which formats do.

use std::path::PathBuf;

use chrono::Utc;

use crate::agent::{Transcript, TranscriptFormat, TranscriptMessage};
use crate::gateway::GatewayState;

/// Parse a format name as a user would write it.
///
/// `md` is the spelling people type; `markdown` is what `TranscriptFormat` calls
/// itself. Both are accepted, and nothing else is.
pub(crate) fn parse_format(name: &str) -> Option<TranscriptFormat> {
    match name.to_ascii_lowercase().as_str() {
        "md" | "markdown" => Some(TranscriptFormat::Markdown),
        "html" => Some(TranscriptFormat::Html),
        "json" => Some(TranscriptFormat::Json),
        "text" | "txt" => Some(TranscriptFormat::Text),
        _ => None,
    }
}

/// The formats an export can be asked for, for a usage message.
pub(crate) const FORMATS: &str = "md, html, json or text";

/// Why an export did not produce a file.
///
/// "This session has nothing in it" and "the export failed" are different
/// answers, and a caller that cannot tell them apart reports a broken gateway
/// for an empty conversation.
pub(crate) enum ExportError {
    /// The session exists but has no messages.
    Empty,
    /// The conversation could not be read or written.
    Failed(String),
}

/// Write `session_id`'s conversation to a file in the transcript store.
pub(crate) async fn export_session(
    state: &GatewayState,
    session_id: &str,
    format: TranscriptFormat,
) -> Result<PathBuf, ExportError> {
    let messages = load_all_messages(state, session_id)
        .await
        .map_err(ExportError::Failed)?;
    if messages.is_empty() {
        return Err(ExportError::Empty);
    }

    let transcript = Transcript {
        session_id: session_id.to_string(),
        channel: "session".to_string(),
        peer: session_id.to_string(),
        scope: "session".to_string(),
        started_at: messages
            .first()
            .map(|m| m.timestamp)
            .unwrap_or_else(Utc::now),
        updated_at: messages
            .last()
            .map(|m| m.timestamp)
            .unwrap_or_else(Utc::now),
        title: session_name(state, session_id).await,
        tags: None,
        messages,
    };

    state
        .infra
        .transcript_store
        .write(&transcript, format)
        .await
        .map_err(ExportError::Failed)
}

/// Every stored message of a session, oldest first.
///
/// Paged rather than asked for in one go: `get_messages` takes a limit, and a
/// conversation longer than any single limit is exactly the one somebody wants
/// to export.
async fn load_all_messages(
    state: &GatewayState,
    session_id: &str,
) -> Result<Vec<TranscriptMessage>, String> {
    let Some(ref store) = state.agents.store else {
        return Err("this gateway has no session store, so there is nothing to export".to_string());
    };

    const PAGE: i64 = 500;
    let mut newest_first: Vec<TranscriptMessage> = Vec::new();
    let mut before: Option<chrono::DateTime<Utc>> = None;
    loop {
        let rows = store
            .get_messages(session_id, PAGE, before)
            .await
            .map_err(|e| format!("failed to read session '{session_id}': {e}"))?;
        let short_page = (rows.len() as i64) < PAGE;
        let oldest = rows.last().map(|row| row.5);
        for row in rows {
            let (_id, role, content, reasoning, tool_calls, ts, _transcript_id, _run_id, _turn_id) =
                row;
            // A reasoning transcript is not something the model *said*, and an
            // assistant message that existed only to carry tool calls has no
            // prose to render; both are dropped, so the export is the
            // conversation rather than every column of the table.
            if reasoning.is_some() || (content.is_empty() && tool_calls.is_some()) {
                continue;
            }
            newest_first.push(TranscriptMessage {
                role,
                content,
                timestamp: ts,
                metadata: None,
            });
        }
        // `get_messages` is newest-first and takes a `before`, so the next page
        // is whatever is older than the oldest row of this one.
        match oldest {
            Some(ts) if !short_page => before = Some(ts),
            _ => break,
        }
    }

    newest_first.reverse();
    Ok(newest_first)
}

/// A session's name, for the exported document's title.
async fn session_name(state: &GatewayState, session_id: &str) -> Option<String> {
    let store = state.agents.store.as_ref()?;
    store
        .get_session_name(session_id)
        .await
        .ok()
        .flatten()
        .filter(|name| !name.is_empty())
}
