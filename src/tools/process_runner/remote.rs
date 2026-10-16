//! Running a request somewhere other than this machine.
//!
//! The local runners enforce the write fence with the kernel — Seatbelt,
//! Landlock, AppContainer. A remote backend has no such kernel to lean on, so
//! each one has to answer the same question: **what happens to the fence?**
//!
//! Two answers are possible, and this module implements both:
//!
//! - **A container translates it.** Docker flags can express everything a
//!   [`WriteFence`] says — the granted roots become read-write bind mounts, the
//!   carve-outs inside them become read-only ones, and `deny_network` becomes
//!   `--network none`. The container is then the enforcement point instead of
//!   the local kernel, and the request keeps its meaning.
//! - **SSH refuses it.** There is nothing on the far side to translate a fence
//!   *to*, so a fenced request is rejected with the reason rather than run
//!   unfenced. Quietly dropping the fence would turn the one guarantee this
//!   project is built around into a suggestion.
//!
//! What neither can do is stated in `docs/security.md` rather than left to be
//! discovered: the escape-vector seccomp list, `pre_exec` rlimits, process-group
//! kill, and the escalation path keyed on the local fence's stderr are all
//! properties of the local runners.

use std::collections::HashMap;

use async_trait::async_trait;
use tracing::warn;

use super::{
    process_runner, CommandOutput, ProcessChild, ProcessError, ProcessRequest, ProcessRunner,
    WriteFence,
};

/// Where a request's process should actually run.
///
/// Defaults to [`ExecutionBackend::Local`], so every request that does not name
/// a backend behaves exactly as it did before this existed.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum ExecutionBackend {
    /// The platform's own runner, where the local kernel enforces the fence.
    #[default]
    Local,
    /// A container, where the fence becomes the container's own mounts.
    Docker(DockerBackend),
    /// A host reached over SSH, which cannot honour a fence at all.
    Ssh(SshBackend),
}

/// How to reach a container.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DockerBackend {
    /// The image to run in. Required — there is no sensible default for
    /// something that decides what the command can even see.
    pub image: String,
    /// Extra `docker run` flags, for a deployment that needs them: a registry,
    /// a user, a platform.
    pub extra_args: Vec<String>,
}

impl DockerBackend {
    /// A backend running in `image`.
    pub fn new(image: impl Into<String>) -> Self {
        Self {
            image: image.into(),
            extra_args: Vec::new(),
        }
    }
}

/// How to reach a remote host.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SshBackend {
    /// The host to connect to.
    pub host: String,
    /// The user to connect as, when it is not the client's default.
    pub user: Option<String>,
    /// The port to connect on.
    pub port: u16,
    /// A private key to authenticate with.
    pub key_path: Option<String>,
    /// Extra `ssh` flags.
    pub extra_args: Vec<String>,
}

impl SshBackend {
    /// A backend reaching `host` the way the ssh client is already configured
    /// to.
    pub fn new(host: impl Into<String>) -> Self {
        Self {
            host: host.into(),
            user: None,
            port: 22,
            key_path: None,
            extra_args: Vec::new(),
        }
    }
}

/// The `docker run` flags a fence becomes.
///
/// This is the whole of what a fenced remote command *means*, which is why it is
/// a pure function with its own tests rather than something buried in the
/// runner.
///
/// Order matters, and it is the same trick the macOS profile uses: a nested
/// mount shadows the one that contains it, so the carve-outs are emitted after
/// the roots they sit inside. `.git` is the one that matters — a writable
/// `.git/hooks` is code execution on the next `git` command.
pub fn fence_mount_flags(fence: &WriteFence) -> Vec<String> {
    let mut flags = Vec::new();

    // Everything the fence grants becomes a read-write bind mount. The rest of
    // the host needs no rule: a container starts from its own image, so the
    // files outside these mounts are not read-only, they are *absent* — which is
    // a stronger statement than the local fence can make.
    for root in std::iter::once(&fence.workspace_root).chain(fence.allowed_paths.iter()) {
        flags.push("--mount".to_string());
        flags.push(format!("type=bind,src={path},dst={path},rw", path = root.display()));
    }

    for protected in &fence.protected_paths {
        flags.push("--mount".to_string());
        flags.push(if protected.exists() {
            format!("type=bind,src={path},dst={path},ro", path = protected.display())
        } else {
            // One case a read-only bind mount cannot cover: docker will not bind
            // a source that does not exist, and this is precisely the path the
            // agent must not be able to *create*. An empty read-only tmpfs
            // shadows it instead, so the container can write neither the file
            // nor the directory.
            format!("type=tmpfs,dst={},ro", protected.display())
        });
    }

    if fence.deny_network {
        flags.push("--network".to_string());
        flags.push("none".to_string());
    }

    flags
}

/// Quote one argument so the far side's shell reads it as one word.
///
/// An SSH command is reassembled into a single string and parsed again by the
/// remote shell, so an argument containing a space, a quote or a semicolon would
/// otherwise become several arguments — or a second command.
fn shell_quote(arg: &str) -> String {
    format!("'{}'", arg.replace('\'', r"'\''"))
}

/// Runs a request inside a container.
pub struct DockerRunner {
    config: DockerBackend,
}

impl DockerRunner {
    /// A runner for one container configuration.
    pub fn new(config: DockerBackend) -> Self {
        Self { config }
    }

    /// Rewrite a request into the `docker run` invocation that keeps its meaning.
    ///
    /// The result is handed to the *local* runner, so a containerised run keeps
    /// the timeout, stdio and exit-status handling every other run has — the
    /// container changes where the process runs, not how it is driven.
    pub fn translate(&self, req: &ProcessRequest) -> Result<ProcessRequest, ProcessError> {
        if self.config.image.trim().is_empty() {
            return Err(ProcessError::Sandbox {
                reason: "a docker backend needs an image to run in".to_string(),
            });
        }
        if req.argv.is_empty() {
            return Err(ProcessError::EmptyArgv);
        }

        let mut argv = vec!["docker".to_string(), "run".to_string(), "--rm".to_string()];

        // Without `-i` the container's stdin is closed, and a request that
        // carries stdin would have it silently discarded.
        if req.stdin.is_some() {
            argv.push("-i".to_string());
        }

        if let Some(fence) = &req.fence {
            argv.extend(fence_mount_flags(fence));
        }

        for (key, value) in &req.env {
            argv.push("-e".to_string());
            argv.push(format!("{}={}", key, value));
        }

        if let Some(cwd) = &req.cwd {
            argv.push("-w".to_string());
            argv.push(cwd.to_string_lossy().into_owned());
        }

        argv.extend(self.config.extra_args.iter().cloned());
        argv.push(self.config.image.clone());
        argv.extend(req.argv.iter().cloned());

        if req.pre_exec.is_some() {
            // A `pre_exec` hook is an opaque closure run in the child before
            // `exec`, so there is nothing in it that a container flag could be
            // built from — docker's `--ulimit` and `--memory` would need the
            // limits as *data*, and they arrive as behaviour.
            //
            // It is dropped rather than refused, because the shell tool sets one
            // unconditionally and refusing would make this backend unusable. But
            // it is dropped loudly: a hardening that quietly goes missing is the
            // kind of thing that is only ever noticed when it was needed.
            warn!(
                "process_runner: a container cannot apply this request's pre_exec resource \
                 limits; they are not in force for this run"
            );
        }

        Ok(ProcessRequest {
            argv,
            // The fence has become the flags above. Passing it on would have the
            // *local* runner fence the docker client itself — which would stop
            // docker reading its own socket, the image and the mounts.
            fence: None,
            // The container's working directory is expressed as `-w`; keeping
            // `cwd` would have the docker client started in a directory the
            // container may not even have mounted.
            cwd: None,
            // `-e` carries exactly what the container should see; the image's own
            // environment is the image's business, not this machine's.
            env_clear: false,
            env: HashMap::new(),
            pre_exec: None,
            // The docker client is an ordinary local process. Leaving this set
            // would send the translated request straight back here.
            backend: ExecutionBackend::Local,
            ..req.clone()
        })
    }
}

#[async_trait]
impl ProcessRunner for DockerRunner {
    async fn run(&self, req: &ProcessRequest) -> Result<CommandOutput, ProcessError> {
        process_runner().run(&self.translate(req)?).await
    }

    async fn run_collect(&self, req: &ProcessRequest) -> Result<CommandOutput, ProcessError> {
        process_runner().run_collect(&self.translate(req)?).await
    }

    async fn spawn(&self, req: &ProcessRequest) -> Result<Box<dyn ProcessChild>, ProcessError> {
        process_runner().spawn(&self.translate(req)?).await
    }
}

/// Runs a request on a remote host over SSH.
pub struct SshRunner {
    config: SshBackend,
}

impl SshRunner {
    /// A runner for one remote host.
    pub fn new(config: SshBackend) -> Self {
        Self { config }
    }

    /// Rewrite a request into an `ssh` invocation — or refuse it.
    ///
    /// **A fence is refused, never dropped.** Docker can translate one because a
    /// container has mounts to translate it into; SSH has no equivalent, and a
    /// request that ran without its fence while reporting success would be a
    /// worse answer than an error. This is the one place in this module where
    /// the honest move is to say no.
    pub fn translate(&self, req: &ProcessRequest) -> Result<ProcessRequest, ProcessError> {
        if req.fence.is_some() {
            return Err(ProcessError::Sandbox {
                reason: "ssh cannot honour a write fence: there is no far-side kernel this \
                         request's fence can be applied to. Run it locally, or on a docker \
                         backend, where the fence is enforceable."
                    .to_string(),
            });
        }
        if req.argv.is_empty() {
            return Err(ProcessError::EmptyArgv);
        }
        if self.config.host.trim().is_empty() {
            return Err(ProcessError::Sandbox {
                reason: "an ssh backend needs a host".to_string(),
            });
        }

        // The command is a single string by the time ssh sees it, and the remote
        // shell parses it again — so every argument is quoted here.
        let mut command = String::new();
        for (key, value) in &req.env {
            command.push_str(&format!("{}={} ", shell_quote(key), shell_quote(value)));
        }
        if let Some(cwd) = &req.cwd {
            command.push_str(&format!("cd {} && ", shell_quote(&cwd.to_string_lossy())));
        }
        let parts: Vec<String> = req.argv.iter().map(|arg| shell_quote(arg)).collect();
        command.push_str(&parts.join(" "));

        let mut argv = vec!["ssh".to_string()];
        for flag in [
            "-o",
            "BatchMode=yes",
            "-o",
            "ConnectTimeout=10",
            "-o",
            "StrictHostKeyChecking=accept-new",
            "-p",
        ] {
            argv.push(flag.to_string());
        }
        argv.push(self.config.port.to_string());
        if let Some(key) = &self.config.key_path {
            argv.push("-i".to_string());
            argv.push(key.clone());
        }
        argv.extend(self.config.extra_args.iter().cloned());

        let destination = match &self.config.user {
            Some(user) => format!("{}@{}", user, self.config.host),
            None => self.config.host.clone(),
        };
        argv.push(destination);
        argv.push(command);

        Ok(ProcessRequest {
            argv,
            // Nothing to fence and nothing to inherit: the request runs on a
            // machine this one has no authority over.
            fence: None,
            cwd: None,
            env_clear: false,
            env: HashMap::new(),
            pre_exec: None,
            backend: ExecutionBackend::Local,
            ..req.clone()
        })
    }
}

#[async_trait]
impl ProcessRunner for SshRunner {
    async fn run(&self, req: &ProcessRequest) -> Result<CommandOutput, ProcessError> {
        process_runner().run(&self.translate(req)?).await
    }

    async fn run_collect(&self, req: &ProcessRequest) -> Result<CommandOutput, ProcessError> {
        process_runner().run_collect(&self.translate(req)?).await
    }

    async fn spawn(&self, req: &ProcessRequest) -> Result<Box<dyn ProcessChild>, ProcessError> {
        process_runner().spawn(&self.translate(req)?).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::process_runner::{NamespacePosture, StdioMode};
    use std::path::PathBuf;

    /// A workspace with a `.git`, and a fence over it.
    fn fence_with_git(deny_network: bool) -> (tempfile::TempDir, WriteFence) {
        let dir = tempfile::tempdir().expect("a temp workspace");
        std::fs::create_dir_all(dir.path().join(".git")).expect("make .git");

        let fence = WriteFence::new(
            dir.path().to_path_buf(),
            Vec::new(),
            deny_network,
            NamespacePosture::default(),
        );
        (dir, fence)
    }

    fn request(fence: Option<WriteFence>) -> ProcessRequest {
        ProcessRequest {
            argv: vec!["echo".to_string(), "hello".to_string()],
            fence,
            ..Default::default()
        }
    }

    fn joined(flags: &[String]) -> String {
        flags.join(" ")
    }

    /// The granted root becomes a read-write mount, and the network is cut when
    /// the fence says so.
    #[test]
    fn a_fence_becomes_mounts() {
        let (dir, fence) = fence_with_git(true);
        let flags = fence_mount_flags(&fence);
        let flags = joined(&flags);

        assert!(flags.contains("--mount"), "{flags}");
        assert!(
            flags.contains(&format!("type=bind,src={0},dst={0},rw", dir.path().display())),
            "the workspace is mounted read-write: {flags}"
        );
        assert!(flags.contains("--network none"), "{flags}");
    }

    /// `.git` sits inside a granted root, and a writable `.git/hooks` is code
    /// execution on the next `git` command — so it has to be carved back out,
    /// and the carve-out has to come *after* the root it shadows.
    #[test]
    fn the_carve_outs_shadow_the_roots_they_sit_inside() {
        let (dir, fence) = fence_with_git(false);
        let flags = fence_mount_flags(&fence);
        let flags = joined(&flags);

        let git = dir.path().join(".git");
        let root_at = flags.find(&format!("dst={},rw", dir.path().display()));
        let git_at = flags.find(&format!("dst={},ro", git.display()));

        assert!(git_at.is_some(), "the carve-out is mounted read-only: {flags}");
        assert!(root_at < git_at, "a nested mount only shadows what came before it: {flags}");
    }

    /// Docker will not bind a source that does not exist — and that is exactly
    /// the path the agent must not be able to create. An empty read-only tmpfs
    /// shadows it instead.
    #[test]
    fn a_carve_out_that_does_not_exist_yet_is_shadowed_not_skipped() {
        let dir = tempfile::tempdir().expect("a temp workspace");
        // No `.git` on disk at all.
        let fence = WriteFence::new(
            dir.path().to_path_buf(),
            Vec::new(),
            false,
            NamespacePosture::default(),
        );

        let flags = joined(&fence_mount_flags(&fence));
        let git = dir.path().join(".git");

        assert!(
            flags.contains(&format!("type=tmpfs,dst={},ro", git.display())),
            "the absent carve-out is still shadowed: {flags}"
        );
        assert!(
            !flags.contains(&format!("src={},", git.display())),
            "and nothing tries to bind a path that is not there: {flags}"
        );
    }

    #[test]
    fn extra_writable_roots_are_mounted_too() {
        let (dir, mut fence) = fence_with_git(false);
        let extra = tempfile::tempdir().expect("a second root");
        fence.allowed_paths.push(extra.path().to_path_buf());

        let flags = joined(&fence_mount_flags(&fence));
        assert!(
            flags.contains(&format!("src={0},dst={0},rw", extra.path().display())),
            "{flags}"
        );
        assert!(flags.contains(&format!("src={0},dst={0},rw", dir.path().display())), "{flags}");
    }

    #[test]
    fn a_docker_request_becomes_a_docker_run() {
        let (_dir, fence) = fence_with_git(false);
        let runner = DockerRunner::new(DockerBackend::new("debian:stable"));

        let mut req = request(Some(fence));
        req.cwd = Some(PathBuf::from("/workspace"));
        req.env.insert("KEY".to_string(), "value".to_string());
        req.stdin = Some(b"input".to_vec());

        let translated = runner.translate(&req).expect("it translates");
        assert_eq!(&translated.argv[..3], ["docker", "run", "--rm"]);
        assert!(translated.argv.contains(&"-i".to_string()), "stdin is kept open");
        assert!(translated.argv.contains(&"KEY=value".to_string()));
        assert!(translated
            .argv
            .windows(2)
            .any(|w| w == ["-w", "/workspace"]));
        assert!(translated.argv.contains(&"debian:stable".to_string()));
        assert_eq!(
            &translated.argv[translated.argv.len() - 2..],
            ["echo", "hello"],
            "the command is last, after the image"
        );
    }

    /// The translated request is run by the *local* runner, so it must not carry
    /// anything that would send it back here — or have the local fence applied
    /// to the docker client, which could not read its own socket.
    #[test]
    fn the_translated_request_carries_nothing_back() {
        let (_dir, fence) = fence_with_git(true);
        let runner = DockerRunner::new(DockerBackend::new("debian:stable"));

        let mut req = request(Some(fence));
        req.pre_exec = Some(std::sync::Arc::new(|| Ok(())));

        let translated = runner.translate(&req).expect("it translates");
        assert_eq!(translated.backend, ExecutionBackend::Local);
        assert!(translated.fence.is_none(), "the fence became flags");
        assert!(translated.cwd.is_none(), "the working directory became -w");
        assert!(translated.pre_exec.is_none());
        assert!(translated.env.is_empty());
    }

    #[test]
    fn a_docker_backend_without_an_image_is_refused() {
        let runner = DockerRunner::new(DockerBackend {
            image: "  ".to_string(),
            extra_args: Vec::new(),
        });

        assert!(matches!(runner.translate(&request(None)), Err(ProcessError::Sandbox { .. })));
    }

    /// A fence is refused, never dropped. This is the whole of what makes the
    /// SSH backend safe to offer.
    #[test]
    fn ssh_refuses_a_fenced_request() {
        let (_dir, fence) = fence_with_git(false);
        let runner = SshRunner::new(SshBackend::new("build.example"));

        // `ProcessRequest` cannot be `Debug` — its `pre_exec` is an opaque
        // closure — so the error is taken out by hand rather than with
        // `expect_err`.
        let error = match runner.translate(&request(Some(fence))) {
            Err(error) => error,
            Ok(_) => panic!("a fenced request must not run over ssh"),
        };

        let rendered = error.to_string();
        assert!(rendered.contains("cannot honour a write fence"), "{rendered}");
        assert!(rendered.contains("docker"), "it says what to do instead: {rendered}");
    }

    #[test]
    fn an_unfenced_ssh_request_becomes_an_ssh_command() {
        let mut runner = SshRunner::new(SshBackend::new("build.example"));
        runner.config.user = Some("deploy".to_string());
        runner.config.port = 2222;
        runner.config.key_path = Some("/keys/id".to_string());

        let translated = runner.translate(&request(None)).expect("it translates");
        let argv = translated.argv.join(" ");

        assert_eq!(translated.argv[0], "ssh");
        assert!(argv.contains("BatchMode=yes"), "{argv}");
        assert!(argv.contains("-p 2222"), "{argv}");
        assert!(argv.contains("-i /keys/id"), "{argv}");
        assert!(argv.contains("deploy@build.example"), "{argv}");
        assert!(
            argv.ends_with("'echo' 'hello'"),
            "the command is quoted, because the far side parses it again: {argv}"
        );
        assert!(translated.fence.is_none());
        assert_eq!(translated.backend, ExecutionBackend::Local);
    }

    /// The command is reassembled into one string and parsed again by the remote
    /// shell, so an argument has to stay one argument.
    #[test]
    fn an_argument_with_a_space_stays_one_argument() {
        let runner = SshRunner::new(SshBackend::new("build.example"));
        let mut req = request(None);
        req.argv = vec!["echo".to_string(), "two words".to_string()];

        let translated = runner.translate(&req).expect("it translates");
        let command = translated.argv.last().expect("a command");

        assert_eq!(command, "'echo' 'two words'");
    }

    /// A quote or a semicolon in an argument would otherwise end it, or start a
    /// second command — on a machine this one has no authority over.
    #[test]
    fn an_argument_cannot_break_out_of_its_quotes() {
        for hostile in ["it's here", "; rm -rf /", "$(whoami)", "`id`", "a\"b"] {
            let runner = SshRunner::new(SshBackend::new("build.example"));
            let mut req = request(None);
            req.argv = vec!["echo".to_string(), hostile.to_string()];

            let translated = runner.translate(&req).expect("it translates");
            let command = translated.argv.last().expect("a command");

            // Every argument is single-quoted, and a single quote inside one is
            // closed, escaped and reopened — so the far side sees one word.
            assert!(command.starts_with("'echo' '"), "{hostile:?} became {command}");
            assert!(
                !command.contains("; rm") || command.contains("'; rm -rf /'"),
                "{hostile:?} became {command}"
            );
        }
    }

    #[test]
    fn a_working_directory_travels_with_the_command() {
        let runner = SshRunner::new(SshBackend::new("build.example"));
        let mut req = request(None);
        req.cwd = Some(PathBuf::from("/srv/app"));
        req.env.insert("MODE".to_string(), "fast".to_string());

        let translated = runner.translate(&req).expect("it translates");
        let command = translated.argv.last().expect("a command");

        assert!(command.starts_with("'MODE'='fast' "), "{command}");
        assert!(command.contains("cd '/srv/app' && "), "{command}");
        assert!(translated.cwd.is_none(), "it was folded into the command");
    }

    #[test]
    fn an_ssh_backend_without_a_host_is_refused() {
        let runner = SshRunner::new(SshBackend::new("   "));
        assert!(matches!(runner.translate(&request(None)), Err(ProcessError::Sandbox { .. })));
    }

    /// The backend has to reach the runner, or a request that named one would be
    /// run locally and report success.
    #[tokio::test]
    async fn the_request_s_selects_the_runner() {
        // A docker backend with no image refuses before running anything, which
        // is only reachable if the facade looked at the request at all.
        let mut req = request(None);
        req.backend = ExecutionBackend::Docker(DockerBackend::new(""));

        let error = crate::tools::process_runner::run(&req)
            .await
            .err()
            .expect("the docker runner should have refused it");
        assert!(matches!(error, ProcessError::Sandbox { .. }), "{error}");

        // And a local request still goes through the platform runner.
        let mut local = request(None);
        local.argv = vec!["echo".to_string(), "local".to_string()];
        local.stdio = StdioMode::Null;
        let out = crate::tools::process_runner::run(&local)
            .await
            .expect("a local request still runs");
        assert!(out.success());
    }
}
