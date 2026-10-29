//! `syscity bench serve` speaks a line protocol on stdout.
//!
//! The harness pipes stdout straight into a JSON parser, so anything else
//! written there — a tracing line, a banner, a warning — is a parse error on
//! the other side of the wire. This test is the guard: stdout carries the
//! protocol and nothing else, and a run that never got a task fails rather
//! than reporting a score.

use std::process::{Command, Output, Stdio};

type TestResult = Result<(), Box<dyn std::error::Error>>;

/// Start `bench serve` and close its stdin without sending a task.
///
/// Nothing here needs a VM or a model: the run ends before either is touched.
fn run_serve_without_a_task() -> Result<Output, Box<dyn std::error::Error>> {
    let exe = std::env::var("CARGO_BIN_EXE_syscity")?;
    let mut child = Command::new(exe)
        .args([
            "bench",
            "serve",
            "--computer-url",
            "http://127.0.0.1:1",
            "--max-steps",
            "1",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;

    // Closing stdin is the protocol's "no more tasks".
    drop(child.stdin.take());
    Ok(child.wait_with_output()?)
}

#[test]
fn stdout_carries_the_protocol_and_nothing_else() -> TestResult {
    let output = run_serve_without_a_task()?;
    let stdout = String::from_utf8(output.stdout)?;
    let lines: Vec<&str> = stdout
        .lines()
        .filter(|line| !line.trim().is_empty())
        .collect();

    assert_eq!(lines.len(), 1, "stdout must carry only the protocol, got {stdout:?}");
    let ready: serde_json::Value = serde_json::from_str(lines[0])?;
    assert_eq!(ready["type"], "ready");
    assert_eq!(ready["schema"], syscity::bench::SERVE_SCHEMA);
    Ok(())
}

#[test]
fn a_run_with_no_task_fails_instead_of_reporting_a_score() -> TestResult {
    let output = run_serve_without_a_task()?;
    assert!(!output.status.success(), "a run that never received a task is an error");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("before sending a task"), "the reason is on stderr: {stderr}");
    Ok(())
}
