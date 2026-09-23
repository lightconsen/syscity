// Split out of the old single-file test module; behavior is unchanged.

use std::time::Duration;

use super::argv;
use crate::tools::process_runner::landlock::{
    landlock_available, landlock_optional_rights, landlock_required_rights, LandlockRunner,
};
use crate::tools::process_runner::ProcessRunner;
use crate::tools::process_runner::{NamespacePosture, ProcessRequest, WriteFence};
use ::landlock::AccessFs;

#[cfg(target_os = "linux")]
mod landlock {
    use super::*;

    /// Landlock in isolation (`Off`): the namespace view's private /tmp
    /// would swallow writes aimed at host /tmp and the read-only root
    /// would deny them for the wrong reason. The view itself has its own
    /// test module below.
    fn fence(workspace_root: &str, allowed: &[&str]) -> WriteFence {
        WriteFence::new(
            std::path::PathBuf::from(workspace_root),
            allowed.iter().map(std::path::PathBuf::from).collect(),
            false,
            NamespacePosture::Off,
        )
    }

    #[test]
    fn rights_mask_is_write_only() {
        let required = landlock_required_rights();
        assert!(required.contains(AccessFs::WriteFile));
        assert!(required.contains(AccessFs::MakeReg));
        assert!(required.contains(AccessFs::RemoveFile));
        assert!(required.contains(AccessFs::MakeDir));
        // Not a read fence: reads/exec stay unrestricted outside the
        // workspace.
        assert!(!required.contains(AccessFs::ReadFile));
        assert!(!required.contains(AccessFs::Execute));
        assert!(!required.contains(AccessFs::ReadDir));
        assert_eq!(landlock_optional_rights(), AccessFs::Refer | AccessFs::Truncate);
    }

    #[test]
    fn fence_passes_through_without_fence() {
        let runner = LandlockRunner::default();
        let req = argv(&["/bin/echo", "hi"]);
        let out = runner.fence(&req).unwrap();
        assert_eq!(out.argv, req.argv);
        assert!(out.fence.is_none());
        assert!(out.pre_exec.is_none());
    }

    #[test]
    fn fence_composes_pre_exec_without_argv_rewrite() {
        let tmp = tempfile::tempdir().unwrap();
        let ws = std::fs::canonicalize(tmp.path()).unwrap();
        if !landlock_available(&fence(ws.to_str().unwrap(), &[])) {
            eprintln!("skipping: Landlock unavailable");
            return;
        }
        let runner = LandlockRunner::default();
        let req = ProcessRequest {
            argv: vec!["/bin/echo".to_string(), "hi".to_string()],
            fence: Some(fence(ws.to_str().unwrap(), &[])),
            ..Default::default()
        };
        let fenced = runner.fence(&req).unwrap();
        // Unlike macOS there is no wrapper argv; the fence lives entirely
        // in the composed pre_exec hook.
        assert_eq!(fenced.argv, req.argv);
        assert!(fenced.fence.is_none());
        assert!(fenced.pre_exec.is_some());
    }

    #[tokio::test]
    async fn landlock_blocks_write_outside_workspace() {
        let tmp = tempfile::tempdir().unwrap();
        let ws = std::fs::canonicalize(tmp.path()).unwrap();
        if !landlock_available(&fence(ws.to_str().unwrap(), &[])) {
            eprintln!("skipping: Landlock unavailable");
            return;
        }
        let runner = LandlockRunner::default();
        let target = format!("/tmp/syscity_ll_{}", std::process::id());
        let _ = std::fs::remove_file(&target);
        let req = ProcessRequest {
            argv: vec![
                "/bin/sh".to_string(),
                "-c".to_string(),
                format!("echo x > {target}"),
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
        assert!(!std::path::Path::new(&target).exists());
    }

    #[tokio::test]
    async fn landlock_allows_write_inside_workspace() {
        let tmp = tempfile::tempdir().unwrap();
        let ws = std::fs::canonicalize(tmp.path()).unwrap();
        if !landlock_available(&fence(ws.to_str().unwrap(), &[])) {
            eprintln!("skipping: Landlock unavailable");
            return;
        }
        let runner = LandlockRunner::default();
        let file = ws.join("f");
        let req = ProcessRequest {
            argv: vec![
                "/bin/sh".to_string(),
                "-c".to_string(),
                format!("echo x > '{}'", file.display()),
            ],
            fence: Some(fence(ws.to_str().unwrap(), &[])),
            timeout: Some(Duration::from_secs(10)),
            ..Default::default()
        };
        let out = runner.run(&req).await.unwrap();
        assert!(out.success(), "write inside workspace must be allowed: {}", out.stderr_string());
        assert!(file.exists());
    }
}
