//! Shared helpers for integration-test targets.
//!
//! Integration tests link the library **without** `cfg(test)`, so they do not
//! get the throwaway temp-dir root that unit tests fall back to. Like any
//! embedder they must install a root (`dirs::set_default_paths`) before anything
//! resolves a path — otherwise `dirs::` free functions panic by design, since
//! the process has no root and silently reading `SYSCITY_HOME`/`~` is exactly
//! what that guard exists to prevent.
//!
//! The install is single-shot per process, so this keeps one temp root alive for
//! the whole test binary.

use std::sync::Arc;

/// Install a process-wide throwaway layout root, once.
///
/// The root is laid out the way `dirs::init()` does at startup, because some
/// tools default their working directory to `<root>/workspace` and expect it to
/// exist.
pub fn install_test_root() {
    static ROOT: std::sync::OnceLock<tempfile::TempDir> = std::sync::OnceLock::new();
    let root = ROOT.get_or_init(|| tempfile::tempdir().expect("create temp root"));

    let paths = syscity::dirs::SyscityPaths::from_root(root.path());
    for dir in [
        paths.workspace_data_dir(),
        paths.data_dir(),
        paths.agents_dir(),
    ] {
        let _ = std::fs::create_dir_all(dir);
    }
    let _ = syscity::dirs::set_default_paths(Arc::new(paths));
}
