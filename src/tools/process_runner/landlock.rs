//! Linux: Landlock write fence applied in the child's `pre_exec` hook.

use std::sync::Arc;

use async_trait::async_trait;
use landlock::{
    AccessFs, BitFlags, CompatLevel, Compatible, LandlockStatus, PathBeneath, PathFd, Ruleset,
    RulesetAttr, RulesetCreated, RulesetCreatedAttr, RulesetError, ABI,
};

use super::namespaces;
use super::seccomp::{install_escape_seccomp, install_network_seccomp};
use super::{
    CommandOutput, ProcessChild, ProcessError, ProcessRequest, ProcessRunner, StdProcessRunner,
    WriteFence,
};

/// Linux: enforces the write fence in-place with Landlock, the kernel LSM.
///
/// Unlike macOS (which rewrites `argv` behind `sandbox-exec`), Landlock
/// restricts the child itself: the `pre_exec` hook calls
/// `landlock_restrict_self()` after fork, so there is no wrapper process (and
/// no process-group kill needed — the restricted child is the direct child).
/// Enforcement is a write fence: every handled write right is denied outside
/// `workspace_root` / `allowed_paths`. Unhandled rights (reads, exec, network)
/// stay unrestricted.
#[cfg(target_os = "linux")]
#[derive(Debug, Clone, Copy, Default)]
pub struct LandlockRunner {
    inner: StdProcessRunner,
}

#[cfg(target_os = "linux")]
impl LandlockRunner {
    /// Compose a `pre_exec` that applies the Landlock fence when the request
    /// asks for it. Hard-fails (fail-closed) when Landlock is unavailable —
    /// kernel without Landlock, unsupported write rights, or a fenced path
    /// that cannot be opened.
    pub(super) fn fence(&self, req: &ProcessRequest) -> Result<ProcessRequest, ProcessError> {
        let Some(fence) = &req.fence else {
            return Ok(req.clone());
        };
        if !landlock_available(fence) {
            return Err(ProcessError::Sandbox {
                reason: "Landlock write fence requested but not usable (kernel without Landlock, \
                         unsupported write rights, or a fenced path cannot be opened)"
                    .to_string(),
            });
        }
        let mut fenced = req.clone();
        let deny_network = fence.deny_network;
        let namespaces_posture = fence.namespaces;
        fenced.fence = None;
        let fence = fence.clone();
        let existing_pre = req.pre_exec.clone();
        fenced.pre_exec = Some(Arc::new(move || {
            if let Some(pre) = &existing_pre {
                pre()?;
            }
            // The view first: it needs capabilities in a fresh user namespace
            // and mount writes, both of which are gone once Landlock
            // restricts. Its /proc writes (the identity maps) likewise must
            // precede the restriction, which handles file writes.
            namespaces::setup(namespaces_posture, &fence)?;
            landlock_restrict(&fence)?;
            // The escape-vector deny list rides along on every fenced run:
            // Landlock fences writes but says nothing about `ptrace`, and a
            // "seccomp only when denying network" rule would leave the whole
            // process-memory attack surface open in the default posture.
            install_escape_seccomp()?;
            if deny_network {
                // After Landlock: the restrict is what sets `no_new_privs`,
                // which the kernel requires before it will take a filter.
                install_network_seccomp()?;
            }
            Ok(())
        }));
        Ok(fenced)
    }
}

#[cfg(target_os = "linux")]
#[async_trait]
impl ProcessRunner for LandlockRunner {
    async fn run(&self, req: &ProcessRequest) -> Result<CommandOutput, ProcessError> {
        let fenced = self.fence(req)?;
        self.inner.run(&fenced).await
    }

    async fn run_collect(&self, req: &ProcessRequest) -> Result<CommandOutput, ProcessError> {
        let fenced = self.fence(req)?;
        self.inner.run_collect(&fenced).await
    }

    async fn spawn(&self, req: &ProcessRequest) -> Result<Box<dyn ProcessChild>, ProcessError> {
        let fenced = self.fence(req)?;
        self.inner.spawn(&fenced).await
    }
}

/// ABI-1 write rights: the irreducible write fence, hard-required (fail-closed
/// on kernels without Landlock).
#[cfg(target_os = "linux")]
pub(super) fn landlock_required_rights() -> BitFlags<AccessFs> {
    AccessFs::from_write(ABI::V1)
}

/// Write rights added by later ABIs, best-effort (silently dropped on kernels
/// too old to express them): rename/link protection (Refer, ABI 2) and
/// truncate (Truncate, ABI 3).
#[cfg(target_os = "linux")]
pub(super) fn landlock_optional_rights() -> BitFlags<AccessFs> {
    AccessFs::Refer | AccessFs::Truncate
}

/// The full handled-rights mask; also the access granted on fenced paths.
#[cfg(target_os = "linux")]
pub(super) fn landlock_write_rights() -> BitFlags<AccessFs> {
    landlock_required_rights() | landlock_optional_rights()
}

/// Build a ruleset handling the write fence's access rights.
#[cfg(target_os = "linux")]
pub(super) fn landlock_ruleset() -> Result<RulesetCreated, RulesetError> {
    Ruleset::default()
        .set_compatibility(CompatLevel::HardRequirement)
        .handle_access(landlock_required_rights())?
        .set_compatibility(CompatLevel::BestEffort)
        .handle_access(landlock_optional_rights())?
        .create()
}

/// Create a ruleset and add a write rule for the workspace and each allowed
/// path. The caller either discards it (availability probe) or restricts the
/// calling process with it.
#[cfg(target_os = "linux")]
pub(super) fn landlock_build(fence: &WriteFence) -> std::io::Result<RulesetCreated> {
    let mut ruleset = landlock_ruleset()
        .map_err(|e| std::io::Error::other(format!("landlock unavailable: {e}")))?;
    for path in std::iter::once(&fence.workspace_root).chain(fence.allowed_paths.iter()) {
        let fd = PathFd::new(path)
            .map_err(|e| std::io::Error::other(format!("cannot open '{}': {e}", path.display())))?;
        let rule = PathBeneath::new(fd, landlock_write_rights());
        ruleset = ruleset.add_rule(rule).map_err(|e| {
            std::io::Error::other(format!("landlock rule for '{}': {e}", path.display()))
        })?;
    }
    // `/dev/null` is granted exactly the way the Seatbelt profile allows it
    // literally: the ubiquitous `2>/dev/null` idiom is ordinary work, not a
    // fence escape (a bit bucket accepts nothing but bits). The namespace
    // view, when present, re-binds `/dev` read-write on top of the read-only
    // root so the mount layer allows what Landlock grants here.
    let fd = PathFd::new("/dev/null")
        .map_err(|e| std::io::Error::other(format!("cannot open /dev/null: {e}")))?;
    let rule = PathBeneath::new(fd, landlock_write_rights());
    ruleset = ruleset
        .add_rule(rule)
        .map_err(|e| std::io::Error::other(format!("landlock rule for /dev/null: {e}")))?;
    Ok(ruleset)
}

/// Parent-side gate: ensures the workspace directory exists (so the child's
/// rule-open succeeds) and that a ruleset handling the write rights can be
/// created for every fenced path. Does NOT call `restrict_self` (irreversible);
/// enforcement happens in the child.
#[cfg(target_os = "linux")]
pub(super) fn landlock_available(fence: &WriteFence) -> bool {
    let _ = std::fs::create_dir_all(&fence.workspace_root);
    landlock_build(fence).is_ok()
}

/// Apply the write fence to the calling process. Runs in the `pre_exec` hook
/// after fork: `landlock_restrict_self` confines this process and its
/// descendants, and because the restriction is irrevocable (and `no_new_privs`
/// is set), the exec'd command inherits the fence.
#[cfg(target_os = "linux")]
pub(super) fn landlock_restrict(fence: &WriteFence) -> std::io::Result<()> {
    let ruleset = landlock_build(fence)?;
    let status = ruleset
        .restrict_self()
        .map_err(|e| std::io::Error::other(format!("landlock restrict_self: {e}")))?;
    match status.landlock {
        LandlockStatus::Available { .. } => Ok(()),
        other => Err(std::io::Error::other(format!("landlock not enforced: {other:?}"))),
    }
}
