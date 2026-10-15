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
/// Read in one go, because that is what an export is: the whole conversation is
/// about to be written into one file, so streaming it in pages would trade a
/// query's worth of memory for a page boundary that has to be exact. It cannot
/// be exact — `get_messages` pages on the timestamp, and a burst of messages
/// inside one millisecond shares it — so paging here would silently drop the
/// messages that straddle a boundary.
///
/// The order comes from the row id rather than the timestamp for the same
/// reason: ids are appended in order and a tie between two messages written in
/// the same millisecond is not a tie about which came first.
async fn load_all_messages(
    state: &GatewayState,
    session_id: &str,
) -> Result<Vec<TranscriptMessage>, String> {
    let Some(ref store) = state.agents.store else {
        return Err("this gateway has no session store, so there is nothing to export".to_string());
    };

    let rows = store
        .get_messages(session_id, i64::MAX, None)
        .await
        .map_err(|e| format!("failed to read session '{session_id}': {e}"))?;

    let mut by_id: Vec<(i64, TranscriptMessage)> = rows
        .into_iter()
        .filter_map(|row| {
            let (id, role, content, reasoning, tool_calls, ts, _t, _r, _turn) = row;
            // A reasoning transcript is not something the model *said*, and an
            // assistant message that existed only to carry tool calls has no
            // prose to render; both are dropped, so the export is the
            // conversation rather than every column of the table.
            if reasoning.is_some() || (content.is_empty() && tool_calls.is_some()) {
                return None;
            }
            Some((
                id,
                TranscriptMessage {
                    role,
                    content,
                    timestamp: ts,
                    metadata: None,
                },
            ))
        })
        .collect();

    by_id.sort_by_key(|(id, _)| *id);
    Ok(by_id.into_iter().map(|(_, message)| message).collect())
}

/// Import a saved conversation into a session.
///
/// Only the **JSON** rendering can be read back: Markdown, HTML and text are
/// documents written for a person, and re-reading a conversation out of them
/// would be guesswork dressed up as fidelity. Writing JSON and importing it is
/// the round trip, and the reason `render_transcript`'s JSON is the full
/// transcript rather than a summary of it.
///
/// `file` is resolved inside the transcript store's own directory and nowhere
/// else. The name comes from the caller, and a name that could name any file on
/// the filesystem would make this "read whatever the gateway can read".
pub(crate) async fn import_session(
    state: &GatewayState,
    session_id: &str,
    file: &str,
) -> Result<ImportedSession, ExportError> {
    let store = state
        .agents
        .store
        .as_ref()
        .ok_or_else(|| ExportError::Failed("this gateway has no session store".to_string()))?;
    let transcripts = &state.infra.transcript_store;

    let name = std::path::Path::new(file)
        .file_name()
        .map(|n| n.to_owned())
        .ok_or_else(|| ExportError::Failed(format!("'{file}' is not a file name")))?;
    if name != std::ffi::OsStr::new(file) {
        // A path was given rather than a name. Accepting it would be accepting
        // an arbitrary read; the name is enough, since exports land in one
        // directory.
        return Err(ExportError::Failed(format!(
            "'{file}' is a path: give the file's name, as `sessions.export` reported it"
        )));
    }

    let transcript = transcripts.load(file).await.map_err(|e| {
        ExportError::Failed(format!(
            "{e} — only the JSON rendering can be read back ({} is a document, not data)",
            file.rsplit_once('.')
                .map(|(_, ext)| ext)
                .unwrap_or("this file")
        ))
    })?;
    if transcript.messages.is_empty() {
        return Err(ExportError::Empty);
    }

    let mut imported = 0usize;
    for message in &transcript.messages {
        store
            .append_message(&crate::agent::session_store::AppendMessageParams {
                session_id,
                role: &message.role,
                content: &message.content,
                ..Default::default()
            })
            .await
            .map_err(|e| {
                ExportError::Failed(format!("failed to append an imported message: {e}"))
            })?;
        imported += 1;
    }

    Ok(ImportedSession {
        messages: imported,
        from: transcript.session_id,
        title: transcript.title,
    })
}

/// What an import put where.
pub(crate) struct ImportedSession {
    /// How many messages were appended.
    pub messages: usize,
    /// The session the file was exported from, which may be another one.
    pub from: String,
    /// That session's title, when the export carried one.
    pub title: Option<String>,
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
