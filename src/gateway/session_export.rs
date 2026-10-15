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

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

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

/// A conversation packaged to be carried to another gateway.
///
/// This is the honest shape of `/handoff` for a project with no transport
/// between gateways: a file that holds the conversation *and* the session state
/// that is not in the messages — its name, its model, and the plan it was
/// working through. What it deliberately does not pretend to be is a live move:
/// the receiving gateway applies it to a session of its own, and nothing about
/// the sender's session changes.
///
/// Versioned, because the file outlives the code that wrote it: a reader that
/// does not know a version says so rather than importing part of one.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HandoffBundle {
    /// Format version. Bump when a reader could no longer read an older file.
    pub version: u32,
    /// The gateway that wrote it, and when.
    pub exported_at: DateTime<Utc>,
    /// Where it came from, for the reader's benefit.
    pub origin: HandoffOrigin,
    /// The conversation, oldest first.
    pub messages: Vec<TranscriptMessage>,
    /// The plan the session was working through, if it had one.
    ///
    /// Recorded rather than applied: a plan is the receiving agent's own
    /// bookkeeping (the planning model builds it from the conversation), so
    /// forcing one in would be this gateway writing another agent's state. It
    /// travels because it is what the session was doing, and a reader — human
    /// or model — is better off knowing.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plan: Option<serde_json::Value>,
}

/// What a bundle says about where it came from.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HandoffOrigin {
    /// The id the session had on the sending gateway.
    pub session_id: String,
    /// Its name there, if it had one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// The model it was using.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
}

/// The version this build writes and the oldest it reads.
pub(crate) const HANDOFF_VERSION: u32 = 1;

/// Write a session out as a bundle, and return where it landed.
pub(crate) async fn write_handoff(
    state: &GatewayState,
    session_id: &str,
    file: Option<&str>,
    plan: Option<serde_json::Value>,
) -> Result<(PathBuf, HandoffBundle), ExportError> {
    let messages = load_all_messages(state, session_id)
        .await
        .map_err(ExportError::Failed)?;
    if messages.is_empty() {
        return Err(ExportError::Empty);
    }

    let bundle = HandoffBundle {
        version: HANDOFF_VERSION,
        exported_at: Utc::now(),
        origin: HandoffOrigin {
            session_id: session_id.to_string(),
            name: session_name(state, session_id).await,
            model: session_model(state, session_id).await,
        },
        messages,
        plan,
    };

    let name = match file {
        Some(file) => handoff_file_name(file)?,
        None => format!(
            "handoff_{}_{}.json",
            sanitize_for_file(session_id),
            Utc::now().format("%Y%m%d_%H%M%S")
        ),
    };
    let path = state.infra.transcript_store.root_dir().join(&name);
    let text = serde_json::to_string_pretty(&bundle)
        .map_err(|e| ExportError::Failed(format!("could not serialize the bundle: {e}")))?;
    tokio::fs::create_dir_all(state.infra.transcript_store.root_dir())
        .await
        .map_err(|e| ExportError::Failed(format!("could not create the directory: {e}")))?;
    tokio::fs::write(&path, text)
        .await
        .map_err(|e| ExportError::Failed(format!("could not write {}: {e}", path.display())))?;

    Ok((path, bundle))
}

/// Apply a bundle to `session_id`.
pub(crate) async fn take_handoff(
    state: &GatewayState,
    session_id: &str,
    file: &str,
) -> Result<TakenHandoff, ExportError> {
    let store = state
        .agents
        .store
        .as_ref()
        .ok_or_else(|| ExportError::Failed("this gateway has no session store".to_string()))?;

    let name = handoff_file_name(file)?;
    let path = state.infra.transcript_store.root_dir().join(&name);
    let text = tokio::fs::read_to_string(&path)
        .await
        .map_err(|e| ExportError::Failed(format!("could not read {name}: {e}")))?;
    let bundle: HandoffBundle = serde_json::from_str(&text)
        .map_err(|e| ExportError::Failed(format!("{name} is not a handoff bundle: {e}")))?;

    // Refusing an unknown version is the point of having one: a newer bundle
    // may carry state this build would silently drop.
    if bundle.version != HANDOFF_VERSION {
        return Err(ExportError::Failed(format!(
            "{name} is handoff version {}, and this gateway reads version {HANDOFF_VERSION}",
            bundle.version
        )));
    }
    if bundle.messages.is_empty() {
        return Err(ExportError::Empty);
    }

    let mut imported = 0usize;
    for message in &bundle.messages {
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

    // The name and the model are session state that is not in the messages, so
    // they are applied — but only where the receiving session has none, because
    // a handoff is not a rename of the session that takes it.
    let mut applied = Vec::new();
    if let Some(origin_name) = bundle.origin.name.as_deref() {
        if store
            .get_session_name(session_id)
            .await
            .ok()
            .flatten()
            .is_none()
        {
            store
                .set_session_name(session_id, origin_name)
                .await
                .map_err(|e| ExportError::Failed(format!("failed to name the session: {e}")))?;
            applied.push(format!("name '{origin_name}'"));
        }
    }
    if let Some(model) = bundle.origin.model.as_deref() {
        let current = store.load_session(session_id).await.ok().flatten();
        if current
            .as_ref()
            .and_then(|s| s.metadata.model.clone())
            .is_none()
        {
            store
                .set_session_model(session_id, Some(model))
                .await
                .map_err(|e| ExportError::Failed(format!("failed to set the model: {e}")))?;
            applied.push(format!("model '{model}'"));
        }
    }

    Ok(TakenHandoff {
        messages: imported,
        from_session: bundle.origin.session_id,
        applied,
        plan_steps: bundle
            .plan
            .as_ref()
            .and_then(|plan| plan.get("tasks"))
            .and_then(|tasks| tasks.as_array())
            .map(|tasks| tasks.len())
            .unwrap_or(0),
    })
}

/// What taking a bundle did.
pub(crate) struct TakenHandoff {
    /// How many messages were appended.
    pub messages: usize,
    /// The session id it came from.
    pub from_session: String,
    /// Which parts of the session state were applied, for the report.
    pub applied: Vec<String>,
    /// Steps in the plan the bundle carries, which is *not* applied.
    pub plan_steps: usize,
}

/// A session's model, for the bundle.
async fn session_model(state: &GatewayState, session_id: &str) -> Option<String> {
    let store = state.agents.store.as_ref()?;
    store
        .load_session(session_id)
        .await
        .ok()
        .flatten()
        .and_then(|session| session.metadata.model)
}

/// A bundle's file name: one name, inside the transcripts directory.
///
/// The same rule as `/import`: the name comes from the caller, and a name that
/// could name any file on the filesystem would make this "write to, or read
/// from, wherever the gateway can".
fn handoff_file_name(file: &str) -> Result<String, ExportError> {
    let name = std::path::Path::new(file)
        .file_name()
        .map(|name| name.to_string_lossy().to_string())
        .ok_or_else(|| ExportError::Failed(format!("'{file}' is not a file name")))?;
    if name != file {
        return Err(ExportError::Failed(format!(
            "'{file}' is a path: give the file's name, not a path to it"
        )));
    }
    Ok(name)
}

/// A session id as part of a file name.
fn sanitize_for_file(input: &str) -> String {
    input
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '-'
            }
        })
        .collect()
}
