// Split out of the old single-file test module; behavior is unchanged.

use std::time::Duration;

use crate::tools::process_runner::appcontainer::WindowsAppContainerRunner;
use crate::tools::process_runner::{NamespacePosture, ProcessRequest, WriteFence};
use crate::tools::process_runner::{ProcessChild, ProcessRunner};

#[cfg(target_os = "windows")]
mod appcontainer {
    use super::*;

    fn fence(workspace_root: &str, allowed: &[&str]) -> WriteFence {
        WriteFence::new(
            std::path::PathBuf::from(workspace_root),
            allowed.iter().map(std::path::PathBuf::from).collect(),
            false,
            // The namespace view is Linux-only; AppContainer ignores it.
            NamespacePosture::Off,
        )
    }

    fn win_argv(cmd: &str) -> ProcessRequest {
        ProcessRequest {
            argv: vec!["cmd".to_string(), "/C".to_string(), cmd.to_string()],
            timeout: Some(Duration::from_secs(10)),
            ..Default::default()
        }
    }

    #[test]
    fn prepare_passes_through_without_fence() {
        let runner = WindowsAppContainerRunner::default();
        let req = win_argv("echo hi");
        // No fence on the request => delegate unfenced (Ok(None)), never
        // a Sandbox error.
        assert!(runner.prepare(&req).unwrap().is_none());
    }

    #[test]
    fn prepare_fails_closed_when_appcontainer_unavailable() {
        let runner = WindowsAppContainerRunner::default();
        let req = ProcessRequest {
            fence: Some(fence(r"Z:\syscity_nonexistent\ws", &[])),
            ..win_argv("echo hi")
        };
        // The fence is requested; the runner must never fall back to an
        // unfenced run. Either AppContainer is unavailable (Sandbox
        // error) or, on a machine that has it, the ACL grant on a
        // nonexistent Z: drive fails.
        assert!(runner.prepare(&req).is_err());
    }

    #[tokio::test]
    async fn fence_allows_write_inside_workspace() {
        if !win_appcontainer::available() {
            eprintln!("skipping: AppContainer unavailable");
            return;
        }
        let runner = WindowsAppContainerRunner::default();
        let tmp = tempfile::tempdir().unwrap();
        let ws = std::fs::canonicalize(tmp.path()).unwrap();
        let file = ws.join("f");
        let req = ProcessRequest {
            argv: vec![
                "cmd".to_string(),
                "/C".to_string(),
                format!("echo x > \"{}\"", file.display()),
            ],
            fence: Some(fence(ws.to_str().unwrap(), &[])),
            timeout: Some(Duration::from_secs(10)),
            ..Default::default()
        };
        let out = runner.run(&req).await.unwrap();
        assert!(out.success(), "write inside workspace must be allowed: {}", out.stderr_string());
        assert!(file.exists());
    }

    #[tokio::test]
    async fn fence_blocks_write_outside_workspace() {
        if !win_appcontainer::available() {
            eprintln!("skipping: AppContainer unavailable");
            return;
        }
        let runner = WindowsAppContainerRunner::default();
        let tmp = tempfile::tempdir().unwrap();
        let ws = std::fs::canonicalize(tmp.path()).unwrap();
        let target = std::env::temp_dir().join(format!("syscity_fence_{}.txt", std::process::id()));
        let _ = std::fs::remove_file(&target);
        let req = ProcessRequest {
            argv: vec![
                "cmd".to_string(),
                "/C".to_string(),
                format!("echo x > \"{}\"", target.display()),
            ],
            fence: Some(fence(ws.to_str().unwrap(), &[])),
            timeout: Some(Duration::from_secs(10)),
            ..Default::default()
        };
        let out = runner.run(&req).await.unwrap();
        assert!(
            !out.success(),
            "write outside workspace must be denied: {}",
            out.stderr_string()
        );
        assert!(!target.exists());
    }

    #[tokio::test]
    async fn run_collect_timeout_kills_job_tree() {
        if !win_appcontainer::available() {
            eprintln!("skipping: AppContainer unavailable");
            return;
        }
        let runner = WindowsAppContainerRunner::default();
        let tmp = tempfile::tempdir().unwrap();
        let ws = std::fs::canonicalize(tmp.path()).unwrap();
        let req = ProcessRequest {
            argv: vec![
                "cmd".to_string(),
                "/C".to_string(),
                "echo partial-out & timeout /t 5".to_string(),
            ],
            fence: Some(fence(ws.to_str().unwrap(), &[])),
            timeout: Some(Duration::from_millis(300)),
            ..Default::default()
        };
        let started = std::time::Instant::now();
        let out = runner.run_collect(&req).await.unwrap();
        assert!(out.timed_out, "run_collect should report the timeout");
        assert!(
            out.stdout_string().contains("partial-out"),
            "partial output must survive the timeout: {:?}",
            out.stdout_string()
        );
        assert!(
            started.elapsed() < Duration::from_secs(3),
            "timeout must not wait for the orphaned child"
        );
    }

    #[tokio::test]
    async fn spawn_delegates_without_fence() {
        let runner = WindowsAppContainerRunner::default();
        let mut child = runner.spawn(&win_argv("echo hi")).await.unwrap();
        assert!(child.id().is_some());
        let status = child.wait().await.unwrap();
        assert!(status.success());
    }

    #[tokio::test]
    async fn spawn_returns_fenced_child_when_requested() {
        if !win_appcontainer::available() {
            eprintln!("skipping: AppContainer unavailable");
            return;
        }
        let runner = WindowsAppContainerRunner::default();
        let tmp = tempfile::tempdir().unwrap();
        let ws = std::fs::canonicalize(tmp.path()).unwrap();
        let req = ProcessRequest {
            fence: Some(fence(ws.to_str().unwrap(), &[])),
            ..win_argv("echo hi")
        };
        let mut child = runner.spawn(&req).await.unwrap();
        assert!(child.id().is_some());
        let status = child.wait().await.unwrap();
        assert!(status.success());
    }
}
