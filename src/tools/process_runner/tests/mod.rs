//! Tests for the process runner, grouped by the platform path each one
//! drives. The std suite covers the unfenced baseline; the per-platform
//! suites live beside it and skip themselves when the platform capability
//! is absent.

use super::*;

mod escape_program;
mod seccomp_program;

#[cfg(target_os = "linux")]
mod escape_contract;

#[cfg(target_os = "macos")]
mod seatbelt;

#[cfg(target_os = "linux")]
mod landlock;

#[cfg(target_os = "linux")]
mod namespace_view;

#[cfg(target_os = "windows")]
mod appcontainer;

fn argv(parts: &[&str]) -> ProcessRequest {
    ProcessRequest {
        argv: parts.iter().map(|s| s.to_string()).collect(),
        timeout: Some(Duration::from_secs(10)),
        ..Default::default()
    }
}

/// The fence descriptor is platform-neutral (`WriteFence::new` computes
/// the protected carve-outs for every runner), so its shape is asserted
/// here rather than only in the per-platform fence modules.
#[test]
fn write_fence_new_computes_protected_paths() {
    let f = WriteFence::new(
        std::path::PathBuf::from("/tmp/ws"),
        vec![std::path::PathBuf::from("/tmp/extra")],
        false,
        NamespacePosture::Auto,
    );
    for root in ["/tmp/ws", "/tmp/extra"] {
        for name in PROTECTED_WRITE_NAMES {
            assert!(
                f.protected_paths
                    .contains(&std::path::PathBuf::from(format!("{root}/{name}"))),
                "protected path {root}/{name} missing from {:?}",
                f.protected_paths
            );
        }
    }
    assert!(!f.deny_network, "the default posture leaves the network alone");
    assert_eq!(f.namespaces, NamespacePosture::Auto);

    let denied = WriteFence::new(
        std::path::PathBuf::from("/tmp/ws"),
        Vec::new(),
        true,
        NamespacePosture::Require,
    );
    assert!(denied.deny_network, "the requested posture is carried through");
    assert_eq!(denied.namespaces, NamespacePosture::Require);
}

#[tokio::test]
async fn test_run_captures_stdout() {
    let out = StdProcessRunner
        .run(&argv(&["/bin/echo", "hello"]))
        .await
        .unwrap();
    assert!(out.success());
    assert_eq!(out.stdout_string().trim(), "hello");
}

#[tokio::test]
async fn test_run_captures_stderr_and_exit_code() {
    let out = StdProcessRunner
        .run(&argv(&["/bin/sh", "-c", "echo err 1>&2; exit 3"]))
        .await
        .unwrap();
    assert!(!out.success());
    assert_eq!(out.exit_code(), Some(3));
    // Normal exit: no terminating signal, not a timeout.
    assert_eq!(out.signal, None);
    assert!(!out.timed_out);
    assert!(out.stderr_string().contains("err"));
}

#[cfg(unix)]
#[tokio::test]
async fn run_reports_signal_when_child_is_killed() {
    let out = StdProcessRunner
        .run(&argv(&["/bin/sh", "-c", "kill -TERM $$"]))
        .await
        .unwrap();
    assert!(!out.success());
    // A signaled process has no exit code; the signal is surfaced
    // instead and is distinct from our own timeout kill (`timed_out`
    // stays false — this termination was requested by the child).
    assert_eq!(out.exit_code(), None);
    assert_eq!(out.signal, Some(libc::SIGTERM));
    assert!(!out.timed_out);
}

#[tokio::test]
async fn run_collect_timeout_kills_backgrounded_descendants() {
    // Unfenced spawn: `sleep 5` is backgrounded by sh. Without the
    // process-group kill the orphan keeps the shared stdout pipe open
    // and the pipe-drain below blocks ~5s past the 300ms timeout.
    let mut req = argv(&["/bin/sh", "-c", "echo partial-out; sleep 5 & wait"]);
    req.timeout = Some(Duration::from_millis(300));
    let started = std::time::Instant::now();
    let out = StdProcessRunner.run_collect(&req).await.unwrap();
    assert!(out.timed_out, "run_collect should report the timeout");
    // Our own SIGKILL discards the wait status, so no signal surfaces.
    assert_eq!(out.signal, None);
    assert!(
        out.stdout_string().contains("partial-out"),
        "partial output must survive the timeout: {:?}",
        out.stdout_string()
    );
    assert!(
        started.elapsed() < Duration::from_secs(3),
        "timeout must not wait for the orphaned sleep"
    );
}

#[tokio::test]
async fn test_run_cwd() {
    let tmp = tempfile::tempdir().unwrap();
    let req = ProcessRequest {
        cwd: Some(tmp.path().to_path_buf()),
        ..argv(&["/bin/pwd"])
    };
    let out = StdProcessRunner.run(&req).await.unwrap();
    assert_eq!(
        std::fs::canonicalize(out.stdout_string().trim()).unwrap(),
        std::fs::canonicalize(tmp.path()).unwrap()
    );
}

#[tokio::test]
async fn test_run_env_clear_and_env() {
    let req = ProcessRequest {
        env_clear: true,
        env: HashMap::from([("FOO".to_string(), "bar".to_string())]),
        ..argv(&["/usr/bin/env"])
    };
    let out = StdProcessRunner.run(&req).await.unwrap();
    assert!(out.stdout_string().contains("FOO=bar"));
    // HOME is inherited only when not cleared; with env_clear it is gone.
    assert!(!out.stdout_string().contains("HOME="));
}

#[tokio::test]
async fn test_run_stdin_piped() {
    let req = ProcessRequest {
        stdin: Some(b"ping\n".to_vec()),
        ..argv(&["/bin/cat"])
    };
    let out = StdProcessRunner.run(&req).await.unwrap();
    assert!(out.success());
    assert_eq!(out.stdout_string().trim(), "ping");
}

#[tokio::test]
async fn test_run_timeout() {
    let req = ProcessRequest {
        timeout: Some(Duration::from_millis(100)),
        ..argv(&["/bin/sleep", "5"])
    };
    let err = StdProcessRunner.run(&req).await.unwrap_err();
    assert!(matches!(err, ProcessError::Timeout { .. }));
}

#[tokio::test]
async fn test_run_empty_argv() {
    let req = ProcessRequest::default();
    let err = StdProcessRunner.run(&req).await.unwrap_err();
    assert!(matches!(err, ProcessError::EmptyArgv));
}

#[tokio::test]
async fn test_spawn_and_wait() {
    let mut child = StdProcessRunner
        .spawn(&argv(&["/bin/echo", "hi"]))
        .await
        .unwrap();
    assert!(child.id().is_some());
    let status = child.wait().await.unwrap();
    assert!(status.success());
}

#[tokio::test]
async fn test_pre_exec_does_not_break_basic_run() {
    let req = ProcessRequest {
        pre_exec: Some(Arc::new(|| Ok(()))),
        ..argv(&["/bin/echo", "ok"])
    };
    let out = StdProcessRunner.run(&req).await.unwrap();
    assert!(out.success());
}
