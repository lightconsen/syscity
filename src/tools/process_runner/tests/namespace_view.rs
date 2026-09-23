// Split out of the old single-file test module; behavior is unchanged.

/// The namespace view end to end against the real kernel: user namespace,
/// read-only root, private `/tmp`, the working trees re-bound on top, and
/// the seccomp escape deny list beneath it all. Skips when the
/// environment cannot build the view — unprivileged user namespaces
/// switched off is a legitimate deployment, and the guard is exactly the
/// degradation the posture promises.
use std::time::Duration;

use crate::tools::process_runner::landlock::LandlockRunner;
use crate::tools::process_runner::ProcessRunner;
use crate::tools::process_runner::{NamespacePosture, ProcessRequest, WriteFence};

#[cfg(target_os = "linux")]
mod namespace_view {
    use super::*;

    fn fence_auto(workspace_root: &str, allowed: &[&str]) -> WriteFence {
        WriteFence::new(
            std::path::PathBuf::from(workspace_root),
            allowed.iter().map(std::path::PathBuf::from).collect(),
            false,
            NamespacePosture::Auto,
        )
    }

    fn fence_off(workspace_root: &str) -> WriteFence {
        WriteFence::new(
            std::path::PathBuf::from(workspace_root),
            Vec::new(),
            false,
            NamespacePosture::Off,
        )
    }

    /// Whether this environment can create an unprivileged user namespace
    /// at all. `unshare` ships in util-linux on every test platform; a
    /// missing binary counts as unavailable.
    /// Whether the FULL namespace view builds end to end in this
    /// environment: a `Require`-posture run only spawns when every step —
    /// user namespace, private mount namespace, read-only root, re-binds —
    /// succeeds. Container runtimes lock their `/dev` submounts, which the
    /// re-bind cannot cross, so there the view degrades to Landlock-only
    /// and these assertions skip rather than test a degraded environment.
    async fn full_view_available() -> bool {
        let tmp = match tempfile::tempdir() {
            Ok(t) => t,
            Err(_) => return false,
        };
        let ws = match std::fs::canonicalize(tmp.path()) {
            Ok(w) => w,
            Err(_) => return false,
        };
        let req = ProcessRequest {
            argv: vec!["/bin/true".to_string()],
            fence: Some(WriteFence::new(ws, Vec::new(), false, NamespacePosture::Require)),
            timeout: Some(Duration::from_secs(10)),
            ..Default::default()
        };
        LandlockRunner::default()
            .run(&req)
            .await
            .map(|out| out.success())
            .unwrap_or(false)
    }

    fn runner() -> LandlockRunner {
        LandlockRunner::default()
    }

    /// An outside write must be refused by the read-only root — saying
    /// "Read-only file system" (EROFS), not Landlock's EACCES, is what
    /// distinguishes the view from the rules beneath it.
    #[tokio::test]
    async fn outside_writes_hit_the_read_only_root() {
        if !full_view_available().await {
            eprintln!("skipping: full namespace view unavailable here");
            return;
        }
        let tmp = tempfile::tempdir().unwrap();
        let ws = std::fs::canonicalize(tmp.path()).unwrap();
        let target = format!("/etc/syscity_ns_view_{}", std::process::id());
        let _ = std::fs::remove_file(&target);
        let req = ProcessRequest {
            argv: vec![
                "/bin/sh".to_string(),
                "-c".to_string(),
                format!("echo x > {target}"),
            ],
            fence: Some(fence_auto(ws.to_str().unwrap(), &[])),
            timeout: Some(Duration::from_secs(10)),
            ..Default::default()
        };
        let out = runner().run(&req).await.unwrap();
        assert!(
            !out.success(),
            "write outside the workspace must hit the read-only root: {}",
            out.stderr_string()
        );
        assert!(
            out.stderr_string()
                .to_lowercase()
                .contains("read-only file system"),
            "expected EROFS from the view, got: {}",
            out.stderr_string()
        );
        assert!(!std::path::Path::new(&target).exists());
    }

    /// `/tmp` is a fresh tmpfs: the host's leftovers are invisible to the
    /// child. (The other direction is fenced by Landlock itself — the
    /// child cannot write host `/tmp` at all, so there is nothing to
    /// leak.)
    #[tokio::test]
    async fn host_tmp_is_invisible_under_the_view() {
        if !full_view_available().await {
            eprintln!("skipping: full namespace view unavailable here");
            return;
        }
        let tmp = tempfile::tempdir().unwrap();
        let ws = std::fs::canonicalize(tmp.path()).unwrap();
        let host_marker = format!("/tmp/syscity_ns_host_{}", std::process::id());
        std::fs::write(&host_marker, "host").unwrap();
        let script = format!("test -e {host_marker} && echo LEAK; echo done");
        let req = ProcessRequest {
            argv: vec!["/bin/sh".to_string(), "-c".to_string(), script],
            fence: Some(fence_auto(ws.to_str().unwrap(), &[])),
            timeout: Some(Duration::from_secs(10)),
            ..Default::default()
        };
        let out = runner().run(&req).await.unwrap();
        assert!(out.success(), "the view must not break ordinary work: {}", out.stderr_string());
        assert!(
            !out.stdout_string().contains("LEAK"),
            "the child must not see the host's /tmp: {}",
            out.stdout_string()
        );
        let _ = std::fs::remove_file(&host_marker);
    }

    /// `2>/dev/null` is the ubiquitous idiom: the `/dev/null` grant
    /// (Landlock) plus the `/dev` re-bind (mount layer) together keep it
    /// working across the read-only flip.
    #[tokio::test]
    async fn dev_null_still_swallows_output() {
        if !full_view_available().await {
            eprintln!("skipping: full namespace view unavailable here");
            return;
        }
        let tmp = tempfile::tempdir().unwrap();
        let ws = std::fs::canonicalize(tmp.path()).unwrap();
        let req = ProcessRequest {
            argv: vec![
                "/bin/sh".to_string(),
                "-c".to_string(),
                "echo stdout-ok; echo hidden 2>/dev/null; cat /dev/null; echo done".to_string(),
            ],
            fence: Some(fence_auto(ws.to_str().unwrap(), &[])),
            timeout: Some(Duration::from_secs(10)),
            ..Default::default()
        };
        let out = runner().run(&req).await.unwrap();
        assert!(
            out.success(),
            "/dev/null must stay usable under the view: {}",
            out.stderr_string()
        );
        assert!(out.stdout_string().contains("stdout-ok"));
        assert!(
            !out.stderr_string().contains("hidden"),
            "the redirect to /dev/null must swallow stderr"
        );
    }

    /// A workspace under `/tmp` survives the tmpfs cover: the write lands
    /// in the real workspace, visible to the host.
    #[tokio::test]
    async fn workspace_under_tmp_survives_the_cover() {
        if !full_view_available().await {
            eprintln!("skipping: full namespace view unavailable here");
            return;
        }
        let tmp = tempfile::tempdir().unwrap();
        let ws = std::fs::canonicalize(tmp.path()).unwrap();
        let file = ws.join("f");
        let req = ProcessRequest {
            argv: vec![
                "/bin/sh".to_string(),
                "-c".to_string(),
                format!("echo x > '{}'", file.display()),
            ],
            fence: Some(fence_auto(ws.to_str().unwrap(), &[])),
            timeout: Some(Duration::from_secs(10)),
            ..Default::default()
        };
        let out = runner().run(&req).await.unwrap();
        assert!(
            out.success(),
            "workspace writes must work under the view: {}",
            out.stderr_string()
        );
        assert!(file.exists(), "the re-bound workspace must be the real directory");
    }

    /// Landlock still fences beneath the view: `/tmp` is writable again
    /// (a fresh tmpfs the view owns), so the ONLY thing that can refuse a
    /// write to a `/tmp` path outside the grant is Landlock.
    #[tokio::test]
    async fn landlock_holds_beneath_the_view() {
        if !full_view_available().await {
            eprintln!("skipping: full namespace view unavailable here");
            return;
        }
        let tmp = tempfile::tempdir().unwrap();
        let ws = std::fs::canonicalize(tmp.path()).unwrap();
        let target = format!("/tmp/syscity_ns_other_{}", std::process::id());
        let _ = std::fs::remove_file(&target);
        let req = ProcessRequest {
            argv: vec![
                "/bin/sh".to_string(),
                "-c".to_string(),
                format!("echo x > {target}"),
            ],
            fence: Some(fence_auto(ws.to_str().unwrap(), &[])),
            timeout: Some(Duration::from_secs(10)),
            ..Default::default()
        };
        let out = runner().run(&req).await.unwrap();
        assert!(
            !out.success(),
            "the view must not widen the workspace grant: {}",
            out.stderr_string()
        );
        assert!(!std::path::Path::new(&target).exists());
    }

    /// The seccomp deny list is real at runtime: `unshare` is on it, and
    /// the util-linux CLI hits exactly that syscall.
    #[tokio::test]
    async fn seccomp_denies_unshare_at_runtime() {
        if !full_view_available().await {
            eprintln!("skipping: full namespace view unavailable here");
            return;
        }
        if !std::path::Path::new("/usr/bin/unshare").is_file() {
            eprintln!("skipping: no unshare binary");
            return;
        }
        let tmp = tempfile::tempdir().unwrap();
        let ws = std::fs::canonicalize(tmp.path()).unwrap();
        let req = ProcessRequest {
            argv: vec!["unshare".to_string(), "-m".to_string(), "true".to_string()],
            fence: Some(fence_auto(ws.to_str().unwrap(), &[])),
            timeout: Some(Duration::from_secs(10)),
            ..Default::default()
        };
        let out = runner().run(&req).await.unwrap();
        assert!(
            !out.success(),
            "unshare must be denied inside the fence: {}",
            out.stderr_string()
        );
    }

    /// `Off` is the regression baseline: the shared /tmp is back (host
    /// markers visible) and Landlock alone fences outside writes.
    #[tokio::test]
    async fn off_keeps_landlock_only() {
        let tmp = tempfile::tempdir().unwrap();
        let ws = std::fs::canonicalize(tmp.path()).unwrap();
        let host_marker = format!("/tmp/syscity_ns_off_{}", std::process::id());
        std::fs::write(&host_marker, "host").unwrap();
        let req = ProcessRequest {
            argv: vec![
                "/bin/sh".to_string(),
                "-c".to_string(),
                format!("test -e {host_marker}"),
            ],
            fence: Some(fence_off(ws.to_str().unwrap())),
            timeout: Some(Duration::from_secs(10)),
            ..Default::default()
        };
        let out = runner().run(&req).await.unwrap();
        assert!(out.success(), "Off must keep the shared /tmp: {}", out.stderr_string());
        let _ = std::fs::remove_file(&host_marker);

        let target = format!("/tmp/syscity_ns_off_write_{}", std::process::id());
        let _ = std::fs::remove_file(&target);
        let req = ProcessRequest {
            argv: vec![
                "/bin/sh".to_string(),
                "-c".to_string(),
                format!("echo x > {target}"),
            ],
            fence: Some(fence_off(ws.to_str().unwrap())),
            timeout: Some(Duration::from_secs(10)),
            ..Default::default()
        };
        let out = runner().run(&req).await.unwrap();
        assert!(
            !out.success(),
            "Landlock must still deny outside writes with Off: {}",
            out.stderr_string()
        );
        assert!(!std::path::Path::new(&target).exists());
    }

    /// The timeout group kill works through the view: the sandboxed child
    /// and its descendants die promptly, partial output survives.
    #[tokio::test]
    async fn run_collect_timeout_kills_through_the_view() {
        if !full_view_available().await {
            eprintln!("skipping: full namespace view unavailable here");
            return;
        }
        let tmp = tempfile::tempdir().unwrap();
        let ws = std::fs::canonicalize(tmp.path()).unwrap();
        // `sleep 5` is backgrounded by sh; without the group kill the
        // orphan keeps the shared stdout pipe open and the drain below
        // blocks ~5s past the 300ms timeout.
        let req = ProcessRequest {
            argv: vec![
                "/bin/sh".to_string(),
                "-c".to_string(),
                "echo partial-out; sleep 5 & wait".to_string(),
            ],
            fence: Some(fence_auto(ws.to_str().unwrap(), &[])),
            timeout: Some(Duration::from_millis(300)),
            ..Default::default()
        };
        let started = std::time::Instant::now();
        let out = runner().run_collect(&req).await.unwrap();
        assert!(out.timed_out, "run_collect should report the timeout");
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
}
