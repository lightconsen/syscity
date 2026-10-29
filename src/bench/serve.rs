//! `syscity bench serve` — the step protocol.
//!
//! `bench run` performs a whole task and hands the harness a list of snippets to
//! replay. That shape has one known cost: the two loops do not interleave, so a
//! task whose next step depends on a load finishing between steps is timed by
//! the harness's pause rather than by the agent's own waiting.
//!
//! Here they interleave. The harness owns the step: syscity sends one snippet,
//! blocks until the harness has executed it and taken its post-step
//! observation, and only then reads the screen again. Observations still come
//! from the VM over HTTP, so the agent sees exactly what it would see in a
//! local run.
//!
//! ```text
//! syscity → harness   {"type":"ready","schema":1}
//! harness → syscity   {"type":"task","task_id":"...","instruction":"..."}
//! syscity → harness   {"type":"action","code":"import pyautogui\n..."}
//! harness → syscity   {"type":"result","ok":true,"output":""}
//!                     … one round trip per snippet …
//! syscity → harness   {"type":"done","schema":1,"result":{…BenchResult…}}
//! ```
//!
//! `stop` (or closing stdin) ends the run early. One process serves one task,
//! because one VM is restored per task and element ids are addresses inside
//! that state only.

use std::sync::Arc;
use std::time::Instant;

use serde::Deserialize;
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncWrite, AsyncWriteExt};
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;

use crate::bench::run::{prepare_run_with, run_with_desktop, BenchOptions, BenchResult};
use crate::computer::remote::ActionSink;
use crate::computer::Result as ComputerResult;
use crate::computer::{ActionResult, ComputerError};
use crate::error::SyscityError;
use crate::providers::Provider;
use crate::Result;

/// Version of the stdio protocol. Bump it when a line's meaning changes.
pub const SERVE_SCHEMA: u32 = 1;

/// Drive one task over stdin/stdout.
pub async fn serve(opts: BenchOptions) -> Result<BenchResult> {
    let reader = tokio::io::BufReader::new(tokio::io::stdin());
    serve_with_io(opts, reader, tokio::io::stdout()).await
}

/// The protocol over any reader/writer pair, so a test can drive it without
/// touching the process's real stdio.
pub async fn serve_with_io<R, W>(opts: BenchOptions, reader: R, writer: W) -> Result<BenchResult>
where
    R: AsyncBufRead + Unpin + Send + 'static,
    W: AsyncWrite + Unpin + Send + 'static,
{
    serve_with_io_inner(opts, reader, writer, None).await
}

#[cfg(test)]
pub(crate) async fn serve_with_io_and_provider<R, W>(
    opts: BenchOptions,
    reader: R,
    writer: W,
    provider: Arc<dyn Provider>,
) -> Result<BenchResult>
where
    R: AsyncBufRead + Unpin + Send + 'static,
    W: AsyncWrite + Unpin + Send + 'static,
{
    serve_with_io_inner(opts, reader, writer, Some(provider)).await
}

async fn serve_with_io_inner<R, W>(
    opts: BenchOptions,
    reader: R,
    writer: W,
    provider: Option<Arc<dyn Provider>>,
) -> Result<BenchResult>
where
    R: AsyncBufRead + Unpin + Send + 'static,
    W: AsyncWrite + Unpin + Send + 'static,
{
    let (out_tx, out_rx) = mpsc::unbounded_channel::<String>();
    let writer_task = tokio::spawn(write_lines(writer, out_rx));

    let link = Arc::new(HarnessLink {
        out: out_tx.clone(),
        pending: tokio::sync::Mutex::new(None),
    });
    send_line(&out_tx, serde_json::json!({"type": "ready", "schema": SERVE_SCHEMA}));

    let (done_tx, mut done_rx) = oneshot::channel::<Result<BenchResult>>();
    let mut done_tx = Some(done_tx);
    let mut runner: Option<JoinHandle<()>> = None;
    let mut eof = false;
    let mut lines = reader.lines();

    let outcome = loop {
        tokio::select! {
            line = lines.next_line(), if !eof => {
                let line = match line {
                    Ok(Some(line)) => line,
                    Ok(None) => {
                        // A harness that closes the pipe after its last step is
                        // done sending; let the run finish and report itself.
                        eof = true;
                        if runner.is_none() {
                            break Err(SyscityError::Validation(
                                "the harness closed the connection before sending a task"
                                    .to_string(),
                            ));
                        }
                        continue;
                    }
                    Err(e) => break Err(SyscityError::Validation(
                        format!("could not read from the harness: {e}"),
                    )),
                };
                if line.trim().is_empty() {
                    continue;
                }
                match parse_line(&line) {
                    Ok(HarnessLine::Task { task_id, instruction }) => {
                        if runner.is_some() {
                            send_line(&out_tx, serde_json::json!({
                                "type": "error",
                                "error": "one process serves one task; start another for the next one",
                            }));
                            continue;
                        }
                        runner = Some(spawn_task(
                            opts.clone(),
                            task_id,
                            instruction,
                            link.clone(),
                            out_tx.clone(),
                            done_tx.take(),
                            provider.clone(),
                        ));
                    }
                    Ok(HarnessLine::Result { ok, output, error }) => {
                        let sender = link.pending.lock().await.take();
                        match sender {
                            Some(sender) => {
                                let _ = sender.send(StepReply { ok, output, error });
                            }
                            None => send_line(&out_tx, serde_json::json!({
                                "type": "error",
                                "error": "no action is waiting for a result",
                            })),
                        }
                    }
                    Ok(HarnessLine::Stop) => break Err(SyscityError::Validation(
                        "the harness stopped the run".to_string(),
                    )),
                    Err(e) => send_line(&out_tx, serde_json::json!({
                        "type": "error",
                        "error": e,
                    })),
                }
            }
            done = &mut done_rx => {
                break done.map_err(|_| {
                    SyscityError::Validation("the run ended without a result".to_string())
                })?;
            }
        }
    };

    // A runner still alive here was cut short; its pending delivery fails
    // rather than hangs once the link's sender is dropped.
    if let Some(runner) = runner {
        runner.abort();
    }
    drop(link);
    drop(out_tx);
    let _ = writer_task.await;
    outcome
}

/// What the harness sends.
#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum HarnessLine {
    Task {
        task_id: String,
        instruction: String,
    },
    Result {
        ok: bool,
        #[serde(default)]
        output: String,
        #[serde(default)]
        error: Option<String>,
    },
    Stop,
}

#[derive(Debug)]
struct StepReply {
    ok: bool,
    output: String,
    error: Option<String>,
}

fn parse_line(line: &str) -> std::result::Result<HarnessLine, String> {
    serde_json::from_str(line).map_err(|e| format!("unreadable line from the harness: {e}"))
}

/// Send one JSON line, ignoring a closed pipe: the run is already ending, and
/// a "the harness is gone" message has nowhere to go anyway.
fn send_line(out: &mpsc::UnboundedSender<String>, value: serde_json::Value) {
    let _ = out.send(value.to_string());
}

async fn write_lines<W>(writer: W, mut lines: mpsc::UnboundedReceiver<String>)
where
    W: AsyncWrite + Unpin,
{
    let mut writer = tokio::io::BufWriter::new(writer);
    while let Some(line) = lines.recv().await {
        if writer.write_all(line.as_bytes()).await.is_err()
            || writer.write_all(b"\n").await.is_err()
            || writer.flush().await.is_err()
        {
            return;
        }
    }
    let _ = writer.flush().await;
}

#[allow(clippy::too_many_arguments)]
fn spawn_task(
    opts: BenchOptions,
    task_id: String,
    instruction: String,
    link: Arc<HarnessLink>,
    out: mpsc::UnboundedSender<String>,
    done: Option<oneshot::Sender<Result<BenchResult>>>,
    provider: Option<Arc<dyn Provider>>,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let outcome = run_one(opts, task_id, instruction, link, provider).await;
        match &outcome {
            Ok(result) => send_line(
                &out,
                serde_json::json!({
                    "type": "done",
                    "schema": SERVE_SCHEMA,
                    "result": result,
                }),
            ),
            // Setup failures have no result to report, so the line carries the
            // reason instead — the harness must not read this as a score.
            Err(error) => send_line(
                &out,
                serde_json::json!({
                    "type": "error",
                    "schema": SERVE_SCHEMA,
                    "error": error.to_string(),
                }),
            ),
        }
        if let Some(done) = done {
            let _ = done.send(outcome);
        }
    })
}

async fn run_one(
    mut opts: BenchOptions,
    task_id: String,
    instruction: String,
    link: Arc<HarnessLink>,
    provider: Option<Arc<dyn Provider>>,
) -> Result<BenchResult> {
    opts.task_id = task_id;
    opts.instruction = instruction;
    // No replay file in step mode; the snippets still travel in the `done`
    // line, so a run can be reconstructed afterwards.
    opts.out = None;

    let prepared = prepare_run_with(&opts, provider).await?;
    let desktop = Arc::new(crate::bench::run::build_desktop(&opts)?.with_action_sink(link));
    run_with_desktop(
        &opts,
        desktop,
        prepared.provider,
        &prepared.provider_name,
        &prepared.model_name,
        prepared.budget,
        Instant::now(),
        prepared.started_at,
    )
    .await
}

/// The stdio end of an [`ActionSink`]: write the snippet, wait for the harness
/// to say it ran.
///
/// The wait is the whole difference from `bench run`: the agent cannot look at
/// the screen again until the harness has taken its own post-step observation,
/// which is exactly the ordering OSWorld's loop has.
struct HarnessLink {
    out: mpsc::UnboundedSender<String>,
    pending: tokio::sync::Mutex<Option<oneshot::Sender<StepReply>>>,
}

#[async_trait::async_trait]
impl ActionSink for HarnessLink {
    async fn deliver(&self, code: &str) -> ComputerResult<ActionResult> {
        let (sender, receiver) = oneshot::channel();
        {
            let mut pending = self.pending.lock().await;
            if pending.is_some() {
                return Err(ComputerError::ToolFailed(
                    "another action is still waiting for the harness".to_string(),
                ));
            }
            *pending = Some(sender);
        }

        let line = serde_json::json!({ "type": "action", "code": code }).to_string();
        if self.out.send(line).is_err() {
            return Err(ComputerError::ToolFailed(
                "the harness is gone; the action was not delivered".to_string(),
            ));
        }

        match receiver.await {
            Ok(reply) if reply.ok => {
                let output = reply.output.trim();
                Ok(ActionResult::success(if output.is_empty() {
                    "the harness executed the action".to_string()
                } else {
                    output.to_string()
                }))
            }
            Ok(reply) => Err(ComputerError::ToolFailed(
                reply
                    .error
                    .unwrap_or_else(|| "the harness reported that the action failed".to_string()),
            )),
            Err(_) => Err(ComputerError::ToolFailed(
                "the harness stopped without answering the action".to_string(),
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bench::testing::start_fake_desktop;
    use crate::providers::mock::MockProvider;
    use crate::providers::{FunctionCall, Message, Role, ToolCall};

    /// One scripted tool round, then the final answer — the same shape the
    /// end-to-end test in `run.rs` uses.
    fn scripted_provider() -> Arc<dyn Provider> {
        Arc::new(MockProvider::new().with_callback(|messages| {
            if messages.iter().any(|m| m.role == Role::Tool) {
                Message::assistant("Saved it.")
            } else {
                Message::assistant("Saving.").with_tool_calls(vec![ToolCall {
                    id: "call_1".to_string(),
                    call_type: "function".to_string(),
                    function: FunctionCall {
                        name: "computer".to_string(),
                        arguments: r#"{"action":"key_press","keys":["ctrl","s"]}"#.to_string(),
                    },
                    index: None,
                    result: None,
                }])
            }
        }))
    }

    async fn read_json<R>(reader: &mut R) -> serde_json::Value
    where
        R: tokio::io::AsyncBufRead + Unpin,
    {
        let mut line = String::new();
        reader.read_line(&mut line).await.expect("read a line");
        serde_json::from_str(line.trim()).expect("each line is JSON")
    }

    async fn write_json<W>(writer: &mut W, value: serde_json::Value)
    where
        W: tokio::io::AsyncWrite + Unpin,
    {
        let mut bytes = value.to_string().into_bytes();
        bytes.push(b'\n');
        writer.write_all(&bytes).await.expect("write a line");
        writer.flush().await.expect("flush");
    }

    #[tokio::test]
    async fn a_task_is_stepped_through_the_harness_and_never_the_vm() {
        let (url, vm) = start_fake_desktop().await;
        let opts = BenchOptions {
            computer_url: url,
            ..Default::default()
        };

        let (mut to_server, server_in) = tokio::io::duplex(64 * 1024);
        let (server_out, from_server) = tokio::io::duplex(64 * 1024);
        let mut from_server = tokio::io::BufReader::new(from_server);

        let serve = tokio::spawn(serve_with_io_and_provider(
            opts,
            tokio::io::BufReader::new(server_in),
            server_out,
            scripted_provider(),
        ));

        let ready = read_json(&mut from_server).await;
        assert_eq!(ready["type"], "ready");
        assert_eq!(ready["schema"], SERVE_SCHEMA);

        write_json(
            &mut to_server,
            serde_json::json!({"type": "task", "task_id": "t1", "instruction": "save it"}),
        )
        .await;

        // The action goes out as a line; the VM is untouched until the harness
        // runs it itself.
        let action =
            tokio::time::timeout(std::time::Duration::from_secs(10), read_json(&mut from_server))
                .await
                .expect("the action arrives");
        assert_eq!(action["type"], "action");
        let code = action["code"].as_str().unwrap();
        assert!(code.contains("pyautogui"), "code: {code}");
        assert!(
            vm.commands.lock().unwrap().is_empty(),
            "step mode must not POST /execute: {:?}",
            vm.commands.lock().unwrap()
        );

        write_json(&mut to_server, serde_json::json!({"type": "result", "ok": true, "output": ""}))
            .await;

        let done =
            tokio::time::timeout(std::time::Duration::from_secs(10), read_json(&mut from_server))
                .await
                .expect("the run reports itself");
        assert_eq!(done["type"], "done", "{done}");
        assert_eq!(done["result"]["stop_reason"], "final_answer");
        assert_eq!(done["result"]["task_id"], "t1");
        assert_eq!(done["result"]["actions"].as_array().unwrap().len(), 1);

        let result = tokio::time::timeout(std::time::Duration::from_secs(10), serve)
            .await
            .expect("the protocol ends")
            .expect("the task did not panic")
            .expect("the run is a result, not an error");
        assert_eq!(result.answer, "Saved it.");
        assert!(vm.commands.lock().unwrap().is_empty(), "nothing reached /execute");
    }

    #[tokio::test]
    async fn a_failed_step_is_reported_back_to_the_agent() {
        let (url, _vm) = start_fake_desktop().await;
        let opts = BenchOptions {
            computer_url: url,
            ..Default::default()
        };

        let (mut to_server, server_in) = tokio::io::duplex(64 * 1024);
        let (server_out, from_server) = tokio::io::duplex(64 * 1024);
        let mut from_server = tokio::io::BufReader::new(from_server);

        let serve = tokio::spawn(serve_with_io_and_provider(
            opts,
            tokio::io::BufReader::new(server_in),
            server_out,
            scripted_provider(),
        ));

        let _ready = read_json(&mut from_server).await;
        write_json(
            &mut to_server,
            serde_json::json!({"type": "task", "task_id": "t2", "instruction": "save it"}),
        )
        .await;

        let _action = read_json(&mut from_server).await;
        write_json(
            &mut to_server,
            serde_json::json!({"type": "result", "ok": false, "error": "guest refused the key"}),
        )
        .await;

        // The model still gets a turn after the failure; the run must end
        // normally rather than hang or panic.
        let result = tokio::time::timeout(std::time::Duration::from_secs(10), serve)
            .await
            .expect("the protocol ends")
            .expect("no panic")
            .expect("a result");
        assert!(!result.answer.is_empty(), "the agent answered anyway");
    }

    #[tokio::test]
    async fn closing_the_pipe_without_a_task_is_an_error_not_a_score() {
        let opts = BenchOptions {
            computer_url: "http://127.0.0.1:1".into(),
            ..Default::default()
        };
        let (to_server, server_in) = tokio::io::duplex(8 * 1024);
        let (server_out, from_server) = tokio::io::duplex(8 * 1024);
        let mut from_server = tokio::io::BufReader::new(from_server);
        // Closing the harness end is the whole scenario.
        drop(to_server);

        let outcome = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            serve_with_io_and_provider(
                opts,
                tokio::io::BufReader::new(server_in),
                server_out,
                scripted_provider(),
            ),
        )
        .await
        .expect("the protocol ends");

        assert!(outcome.is_err());
        let ready = read_json(&mut from_server).await;
        assert_eq!(ready["type"], "ready");
    }
}
