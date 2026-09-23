// Split out of the old single-file test module; behavior is unchanged.

use std::time::Duration;

use super::argv;
use crate::tools::process_runner::seatbelt::{can_sandbox, seatbelt_profile, MacSeatbeltRunner};
use crate::tools::process_runner::ProcessRunner;
use crate::tools::process_runner::{NamespacePosture, ProcessRequest, WriteFence};

#[cfg(target_os = "macos")]
mod seatbelt {
    use super::*;

    fn fence(workspace_root: &str, allowed: &[&str]) -> WriteFence {
        WriteFence::new(
            std::path::PathBuf::from(workspace_root),
            allowed.iter().map(std::path::PathBuf::from).collect(),
            false,
            // The namespace view is Linux-only; Seatbelt ignores it.
            NamespacePosture::Off,
        )
    }

    /// Same, with the network posture flipped on.
    fn fence_no_network(workspace_root: &str) -> WriteFence {
        WriteFence::new(
            std::path::PathBuf::from(workspace_root),
            Vec::new(),
            true,
            NamespacePosture::Off,
        )
    }

    #[test]
    fn profile_denies_writes_outside_workspace() {
        let p = seatbelt_profile(&fence("/tmp/ws", &[]));
        assert!(p.contains("(version 1)"));
        assert!(p.contains("(allow default)"));
        assert!(p.contains("(deny file-write*)"));
        assert!(p.contains("(allow file-write* (subpath \"/tmp/ws\"))"));
        assert!(p.contains("(allow file-write* (literal \"/dev/null\"))"));
    }

    #[test]
    fn profile_includes_allowed_paths() {
        let p = seatbelt_profile(&fence("/tmp/ws", &["/tmp/extra"]));
        assert!(p.contains("(allow file-write* (subpath \"/tmp/extra\"))"));
    }

    #[test]
    fn profile_denies_network_only_when_asked() {
        // The default posture leaves the network alone: `curl` and
        // `git fetch` are ordinary work.
        let permissive = seatbelt_profile(&fence("/tmp/ws", &[]));
        assert!(
            !permissive.contains("network"),
            "no network clause unless the deployment asked: {permissive}"
        );

        let denied = seatbelt_profile(&fence_no_network("/tmp/ws"));
        assert!(denied.contains("(deny network*)"), "got {denied}");
        // Order matters: the deny must come after `(allow default)`.
        let allow_at = denied.find("(allow default)").expect("allow default");
        let deny_at = denied.find("(deny network*)").expect("deny network");
        assert!(allow_at < deny_at, "last match wins, so the deny goes last");
    }

    #[test]
    fn profile_carves_git_and_syscity_out_of_the_workspace() {
        let f = fence("/tmp/ws", &["/tmp/extra"]);
        assert!(f
            .protected_paths
            .contains(&std::path::PathBuf::from("/tmp/ws/.git")));
        assert!(f
            .protected_paths
            .contains(&std::path::PathBuf::from("/tmp/extra/.git")));

        let p = seatbelt_profile(&f);
        let ws_allow = p
            .find("(allow file-write* (subpath \"/tmp/ws\"))")
            .expect("workspace allow");
        let git_deny = p
            .find("(deny file-write* (subpath \"/tmp/ws/.git\"))")
            .expect("git deny");
        assert!(ws_allow < git_deny, "the carve-out must follow the allow");
        assert!(p.contains("(deny file-write* (subpath \"/tmp/ws/.syscity\"))"));
        assert!(p.contains("(deny file-write* (subpath \"/tmp/extra/.git\"))"));
    }

    /// A fenced command cannot plant a `.git/hooks` payload even though
    /// the workspace it writes in is granted — that hook would run on the
    /// next `git` command, which is the escape the carve-out closes.
    #[tokio::test]
    async fn seatbelt_blocks_writes_into_git_metadata() {
        if !can_sandbox() {
            return;
        }
        let dir = tempfile::tempdir().expect("tempdir");
        let ws = dir.path().join("ws");
        std::fs::create_dir_all(ws.join(".git/hooks")).expect("mkdir");
        let target = ws.join(".git/hooks/pre-commit");
        let ordinary = ws.join("notes.md");

        let runner = MacSeatbeltRunner::default();
        let req = ProcessRequest {
            argv: vec![
                "sh".to_string(),
                "-c".to_string(),
                format!("echo pwned > {} ; echo fine > {}", target.display(), ordinary.display()),
            ],
            fence: Some(fence(&ws.to_string_lossy(), &[])),
            ..Default::default()
        };
        // The script's exit code is its last command's, so the refusal is
        // asserted on the file and the stderr rather than on `success()`.
        let out = runner.run(&req).await.expect("run");
        assert!(
            out.stderr_string().contains("not permitted"),
            "the .git write must be refused: {}",
            out.stderr_string()
        );
        assert!(!target.exists(), "no hook payload may land");
        assert!(ordinary.exists(), "ordinary workspace writes still work");
    }

    /// The network posture reaches a real child: with `deny_network` the
    /// sandbox refuses a loopback connect that succeeds without it.
    #[tokio::test]
    async fn seatbelt_denies_network_only_when_asked() {
        if !can_sandbox() {
            return;
        }
        // A listener makes the control case meaningful: without it both
        // runs fail and the test would pass for the wrong reason.
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let port = listener.local_addr().expect("addr").port();

        let connect = |port: u16| ProcessRequest {
            argv: vec![
                "sh".to_string(),
                "-c".to_string(),
                format!("exec 3<>/dev/tcp/127.0.0.1/{port}"),
            ],
            ..Default::default()
        };
        let dir = tempfile::tempdir().expect("tempdir");
        let runner = MacSeatbeltRunner::default();

        let mut allowed = connect(port);
        allowed.fence = Some(fence(&dir.path().to_string_lossy(), &[]));
        let out = runner.run(&allowed).await.expect("run");
        assert!(out.success(), "network is reachable by default: {out:?}");

        let mut denied = connect(port);
        denied.fence = Some(fence_no_network(&dir.path().to_string_lossy()));
        let out = runner.run(&denied).await.expect("run");
        assert!(!out.success(), "the deny must stop the connect: {out:?}");
        assert!(
            out.stderr_string().contains("not permitted"),
            "expected a denial, got: {}",
            out.stderr_string()
        );
    }

    #[test]
    fn profile_escapes_spaces_and_quotes() {
        let p = seatbelt_profile(&fence("/tmp/ws with space", &["/tmp/a\"b"]));
        assert!(p.contains("(subpath \"/tmp/ws with space\")"));
        assert!(p.contains("(subpath \"/tmp/a\\\"b\")"));
    }

    #[test]
    fn can_sandbox_is_usable_on_dev_machine() {
        assert!(can_sandbox(), "seatbelt must be usable on the dev mac");
    }

    #[test]
    fn fence_passes_through_without_seatbelt() {
        let runner = MacSeatbeltRunner::default();
        let req = argv(&["/bin/echo", "hi"]);
        let out = runner.fence(&req).unwrap();
        assert_eq!(out.argv, req.argv);
        assert!(out.fence.is_none());
    }

    #[test]
    fn fence_rewrites_argv_behind_sandbox_exec() {
        if !can_sandbox() {
            return;
        }
        let runner = MacSeatbeltRunner::default();
        let req = ProcessRequest {
            argv: vec!["/bin/echo".to_string(), "hi".to_string()],
            fence: Some(fence("/tmp/ws", &[])),
            ..Default::default()
        };
        let fenced = runner.fence(&req).unwrap();
        assert_eq!(fenced.argv[0], "/usr/bin/sandbox-exec");
        assert_eq!(fenced.argv[1], "-p");
        assert!(fenced.argv[2].contains("deny file-write*"));
        assert_eq!(&fenced.argv[3..], &["/bin/echo".to_string(), "hi".to_string()]);
        assert!(fenced.fence.is_none());
        // Group setup/teardown lives in the base runner now (`build_command`
        // puts every Unix child in its own group), so the fence no longer
        // composes any extra pre_exec hook of its own.
        assert!(fenced.pre_exec.is_none());
    }

    #[tokio::test]
    async fn seatbelt_blocks_write_outside_workspace() {
        if !can_sandbox() {
            return;
        }
        let runner = MacSeatbeltRunner::default();
        let tmp = tempfile::tempdir().unwrap();
        let ws = std::fs::canonicalize(tmp.path()).unwrap();
        let target = format!("/tmp/syscity_fence_{}", std::process::id());
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
    async fn seatbelt_allows_write_inside_workspace() {
        if !can_sandbox() {
            return;
        }
        let runner = MacSeatbeltRunner::default();
        let tmp = tempfile::tempdir().unwrap();
        let ws = std::fs::canonicalize(tmp.path()).unwrap();
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

    #[tokio::test]
    async fn seatbelt_run_collect_timeout_kills_sandboxed_grandchild() {
        if !can_sandbox() {
            return;
        }
        let runner = MacSeatbeltRunner::default();
        let tmp = tempfile::tempdir().unwrap();
        let ws = std::fs::canonicalize(tmp.path()).unwrap();
        // `sleep 5` is a descendant of the sandboxed sh; a plain kill of
        // the wrapper would orphan it and leave the output pipe open.
        let req = ProcessRequest {
            argv: vec![
                "/bin/sh".to_string(),
                "-c".to_string(),
                "echo partial-out; sleep 5".to_string(),
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
        // The group kill must reap the sandboxed grandchild promptly; if
        // the orphan still held the pipe, this await would block ~5s.
        assert!(
            started.elapsed() < Duration::from_secs(3),
            "timeout must not wait for the orphaned sleep"
        );
    }
}
