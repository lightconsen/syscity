//! Line mode: the TUI for a stdout that is not a terminal.

use std::io::{self, BufRead, IsTerminal, Write};
use std::sync::Arc;

use tokio::sync::{mpsc, RwLock};

use crate::tui::app::{Endpoint, SessionChoice};
use crate::tui::commands::handle_slash_command;
use crate::tui::error::TuiError;
use crate::tui::resume;
use crate::tui::state::{AppState, ConnectionState, LiveMode};
use crate::tui::ws_client::{WsClient, WsMessage};

use super::absorb_action_error;
use super::events::handle_event;
use super::prompts::{answer_prompt_from_line, answer_prompt_without_a_human};
use super::send::send_message;

/// Where line mode reads its lines and writes its output.
///
/// Injected rather than hard-wired to the process's stdio so the path can be
/// driven from a test. Line mode had no coverage at all, which is how the line
/// it read came to be thrown away without anyone noticing.
pub struct PlainIo {
    /// The command lines.
    pub input: Box<dyn BufRead + Send>,
    /// Everything line mode prints.
    pub output: Box<dyn Write + Send>,
    /// Whether a human can answer a prompt through this input. False for a
    /// pipe: nothing can answer, so a prompt has to fail closed rather than
    /// hold the whole pipe until the gateway times it out.
    pub interactive: bool,
}

impl PlainIo {
    /// The real thing: the process's stdin and stdout.
    ///
    /// `syscity tui > out.txt` is line mode with a terminal still on stdin, so
    /// prompts can be answered by typing; `echo … | syscity tui` cannot be.
    pub fn stdio() -> Self {
        Self {
            // `Stdin` is only `Read`; the buffering lives in the wrapper.
            input: Box::new(io::BufReader::new(io::stdin())),
            output: Box::new(io::stdout()),
            interactive: io::stdin().is_terminal(),
        }
    }
}

/// Run the TUI in line mode, for a stdout that is not a terminal.
///
/// No cursor addressing and no raw mode: input is read line by line, output is
/// printed as it arrives. Enough for `echo "…" | syscity tui > out.txt`, and a
/// safe landing spot when the terminal cannot do an inline viewport.
///
/// The transcript is the only writer: a line is printed when it graduates out
/// of the transcript, so nothing is printed twice and nothing needs to know
/// which path a line came from. Streaming therefore shows up at line
/// granularity — a partial line waits for its newline — which is what a pipe
/// or a file wants anyway.
pub async fn run_plain(endpoint: Endpoint, session: SessionChoice) -> Result<(), TuiError> {
    run_plain_with(endpoint, session, PlainIo::stdio()).await
}

/// Line mode, against an injected reader and writer.
pub async fn run_plain_with(
    endpoint: Endpoint,
    session: SessionChoice,
    io: PlainIo,
) -> Result<(), TuiError> {
    let PlainIo { input, mut output, interactive } = io;
    let (ws, hello) =
        WsClient::connect(&endpoint.url, &endpoint.auth, &["chat", "read", "write"]).await?;

    let state = Arc::new(RwLock::new(AppState::default()));
    {
        let mut s = state.write().await;
        s.connection = ConnectionState::Connected {
            features: hello.features,
            scopes_granted: hello.scopes_granted,
            server_version: hello.server.version,
        };
        s.current_session = endpoint.session.clone();
    }

    match resume::resolve_startup_session(&session, &state, &ws).await {
        Ok(resume::StartupSession::Use(id)) => {
            if let Err(e) = resume::switch_to(&id, &state, &ws).await {
                eprintln!("could not resume {id}: {e}");
            }
        }
        Ok(resume::StartupSession::ListAndWait) => {
            if let Ok(sessions) = resume::refresh_sessions(&state, &ws).await {
                for line in resume::session_lines(&sessions) {
                    let _ = writeln!(output, "{line}");
                }
            }
        }
        Ok(resume::StartupSession::Fresh) => {}
        Err(e) => eprintln!("could not list sessions: {e}"),
    }
    drain(&state, output.as_mut()).await;

    // Input is read on a blocking thread: it is a blocking source and has no
    // place in the async runtime.
    let (line_tx, mut line_rx) = mpsc::unbounded_channel::<String>();
    tokio::task::spawn_blocking(move || {
        let mut input = input;
        loop {
            let mut line = String::new();
            match input.read_line(&mut line) {
                Ok(0) | Err(_) => break,
                Ok(_) => {
                    if line_tx.send(line.trim_end().to_string()).is_err() {
                        break;
                    }
                }
            }
        }
    });

    loop {
        tokio::select! {
            Some(line) = line_rx.recv() => {
                if interactive && state.read().await.live_mode != LiveMode::Composer {
                    answer_prompt_from_line(&line, &state, &ws).await?;
                } else if line.trim().is_empty() {
                    continue;
                } else if line.starts_with('/') {
                    // A command that fails is a line of output, not the end of
                    // the pipe: a script feeding several lines should get the
                    // rest of them run.
                    if let Err(e) =
                        handle_slash_command(&line, Arc::clone(&state), &ws).await
                    {
                        absorb_action_error(e, &state).await?;
                    }
                } else if let Err(e) = submit_plain_line(&line, &state, &ws).await {
                    // A send failure is not a line of transcript: it goes to
                    // stderr so the pipe's stdout stays the conversation.
                    if e.is_fatal() {
                        return Err(e);
                    }
                    eprintln!("send failed: {e}");
                }
                drain(&state, output.as_mut()).await;
            }
            Some(message) = ws.next() => {
                match message {
                    WsMessage::Disconnected => {
                        eprintln!("connection closed");
                        break;
                    }
                    WsMessage::Event(event) => {
                        // Every event goes through the transcript, which is
                        // then drained to the writer. Deltas that also went
                        // straight to stdout came out twice: once as they
                        // arrived, and again when `chat.final` re-stated the
                        // whole turn.
                        handle_event(event, &state, &ws).await;
                    }
                    WsMessage::OrphanResponse(_) => {}
                }
                // A prompt with nobody to answer it is settled as it arrives:
                // waiting for a line that a pipe will never send would park the
                // whole pipe on the gateway's timeout.
                if !interactive {
                    answer_prompt_without_a_human(&state, &ws).await?;
                }
                drain(&state, output.as_mut()).await;
            }
        }
    }
    Ok(())
}

/// Submit one line of piped input as a chat message.
///
/// The line came from the reader, not from a composer, so it has to be put
/// into the input buffer first — [`send_message`] submits what is in that
/// buffer. Without this the line was read, checked for a leading `/`, and then
/// silently discarded along with the whole buffer.
async fn submit_plain_line(
    line: &str,
    state: &Arc<RwLock<AppState>>,
    ws: &WsClient,
) -> Result<(), TuiError> {
    state.write().await.set_input(line.to_string());
    send_message(state, ws).await
}

/// Print everything the transcript has graduated.
async fn drain(state: &Arc<RwLock<AppState>>, out: &mut (dyn Write + Send)) {
    let lines = state.write().await.transcript.take_flushable();
    for line in lines {
        // A closed pipe (`| head`) is not worth reporting — the reader asked
        // us to stop — but the transcript still has to be drained.
        let _ = writeln!(out, "{}", line.text);
    }
    let _ = out.flush();
}
