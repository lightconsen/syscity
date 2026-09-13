//! Guard rail: constructing a public type must never panic for want of a root.
//!
//! `dirs::` free functions resolve against a process-wide root that the process
//! entry point installs (`main.rs`, the desktop shell, `Gateway::with_options`).
//! If nothing installed one, resolving a path panics by design — that guard is
//! what stops a stray code path from silently reading `SYSCITY_HOME`/`~`.
//!
//! That guard is only appropriate for code that *uses* a path. It is a poor
//! contract for a **constructor**: a library consumer (or an integration test)
//! that merely builds a tool should not have to install a root first.
//!
//! This target deliberately never installs one — it is the "bare embedder"
//! environment — and constructs everything a consumer might build. Failures are
//! collected so one run reports every offender, not just the first.
//!
//! Keep this list in step with the public constructors in `src/tools` and the
//! gateway-adjacent managers.

use std::panic::{catch_unwind, AssertUnwindSafe};

/// Run `f`, recording `name` as an offender if it panics.
fn check(offenders: &mut Vec<String>, name: &str, f: impl FnOnce()) {
    if catch_unwind(AssertUnwindSafe(f)).is_err() {
        offenders.push(name.to_string());
    }
}

#[test]
fn public_constructors_do_not_require_an_installed_root() {
    let mut offenders: Vec<String> = Vec::new();

    // ── Built-in tools (mirrors `create_default_tool_registry`) ─────────────
    use syscity::tools::*;
    check(&mut offenders, "ShellTool::new", || {
        let _ = ShellTool::new();
    });
    check(&mut offenders, "FileReadTool::new", || {
        let _ = FileReadTool::new();
    });
    check(&mut offenders, "FileWriteTool::new", || {
        let _ = FileWriteTool::new();
    });
    check(&mut offenders, "FileEditTool::new", || {
        let _ = FileEditTool::new();
    });
    check(&mut offenders, "GlobTool::new", || {
        let _ = GlobTool::new();
    });
    check(&mut offenders, "GrepTool::new", || {
        let _ = GrepTool::new();
    });
    check(&mut offenders, "TimeTool::new", || {
        let _ = TimeTool::new();
    });
    check(&mut offenders, "TodoTool::new", || {
        let _ = TodoTool::new();
    });
    check(&mut offenders, "CronTool::new", || {
        let _ = CronTool::new();
    });
    check(&mut offenders, "WebSearchTool::new", || {
        let _ = WebSearchTool::new();
    });
    check(&mut offenders, "WebFetchTool::new", || {
        let _ = WebFetchTool::new();
    });
    check(&mut offenders, "UpdatePlanTool::new", || {
        let _ = UpdatePlanTool::new();
    });
    check(&mut offenders, "ProcessTool::new", || {
        let _ = ProcessTool::new();
    });
    check(&mut offenders, "PdfTool::new", || {
        let _ = PdfTool::new();
    });
    check(&mut offenders, "ImageTool::new", || {
        let _ = ImageTool::new();
    });
    check(&mut offenders, "ImageGenerateTool::new", || {
        let _ = ImageGenerateTool::new();
    });
    check(&mut offenders, "TtsTool::new", || {
        let _ = TtsTool::new();
    });
    check(&mut offenders, "NodesTool::new", || {
        let _ = NodesTool::new();
    });

    // ── Secrets / managers ──────────────────────────────────────────────────
    //
    // Not checked: `SecretStoreHandle::new()`. That constructor *means* "the
    // process-default secrets store" and has to read the master key from that
    // root, so requiring an installed root is intrinsic to it rather than an
    // accident — and the panic message says exactly what to call instead.
    // Embedders that own their root use `new_at_root(paths.secrets_dir())`,
    // which is what the gateway does. The constructors above are different:
    // there the caller never opted into a process global, so the root is an
    // implementation detail they should not have to supply.
    // `Default` impls are constructors too. Not checked here: `ToolSandbox`'s
    // default sets `workspace_root` from `dirs::workspace_data_dir()`, so
    // `ToolContext::default()` is a *known* offender — tracked, not blessed.
    //
    // The cheap-looking fix is deliberately **not** taken: defaulting the field
    // to `current_dir()` (as the sibling `working_directory` field does) would
    // silently move the default workspace fence from `<root>/workspace` to the
    // process cwd. A default whose meaning changes in order to make a symptom
    // go away is worse than the symptom, and nothing at the call site shows it.
    // The real fix is to pass the root explicitly or resolve it lazily while
    // keeping the same value — the field is a public `PathBuf` read in ~113
    // places, so that is a real change, not a one-liner.
    check(&mut offenders, "McpManager::new", || {
        let root = tempfile::tempdir().expect("temp secrets root");
        let Ok(secrets) = syscity::secrets::SecretStoreHandle::with_root(root.path().to_path_buf())
        else {
            return; // keyring-less CI: nothing to assert about the manager
        };
        let _ = McpManager::new(std::sync::Arc::new(secrets));
    });

    assert!(
        offenders.is_empty(),
        "these public constructors panic when no path root is installed — they must take one \
         or resolve it lazily instead:\n  {}",
        offenders.join("\n  ")
    );
}
