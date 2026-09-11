//! Integration Tests — Direct Tool::execute() Invocation
//!
//! Each file tests a category of tools without going through Gateway/WebSocket.

pub use std::sync::Arc;
pub use std::time::Duration;

pub use serde_json::json;
pub use syscity::tools::web::SearchProvider;
pub use syscity::tools::{
    AcpSessionTool, AcpSpawnTool, ApplyPatchTool, BrowserTool, CanvasTool, CodeExecutionTool,
    CronTool, DelegateTool, FileEditTool, FileReadTool, FileWriteTool, GlobTool, GrepTool,
    ImageGenerateTool, ImageTool, McpConnectionTool, McpManager, MemoryGetTool, MemorySearchTool,
    MemoryTool, NodesTool, PdfTool, ProcessTool, SessionStatusTool, SessionsHistoryTool,
    SessionsListTool, SessionsSendTool, SessionsYieldTool, ShellTool, TimeTool, TodoTool, Tool,
    ToolContext, TtsTool, UpdatePlanTool, WebFetchTool, WebSearchTool,
};

/// Install a process-wide path root, the way any embedder must.
///
/// These tests drive tools directly and so bypass gateway startup, which is
/// where a root is normally installed. `dirs::` free functions panic without
/// one. The install is single-shot per process, so keep one temp root alive.
pub fn install_test_root() {
    static ROOT: std::sync::OnceLock<tempfile::TempDir> = std::sync::OnceLock::new();
    let root = ROOT.get_or_init(|| tempfile::tempdir().expect("create temp root"));
    let paths = syscity::dirs::SyscityPaths::from_root(root.path());
    // Lay the root out the way `dirs::init()` does at startup: tools default
    // their working directory to `<root>/workspace`, so it has to exist.
    for dir in [
        paths.workspace_data_dir(),
        paths.data_dir(),
        paths.agents_dir(),
    ] {
        let _ = std::fs::create_dir_all(dir);
    }
    let _ = syscity::dirs::set_default_paths(Arc::new(paths));
}

/// Create a test ToolContext with a unique conversation_id to avoid cross-test
/// pollution.
pub fn test_context() -> ToolContext {
    install_test_root();
    ToolContext::new("test_user", format!("test-session-{}", std::process::id()))
        .with_timeout(Duration::from_secs(10))
        .with_workspace_only(false)
}

mod acp_tests;
#[cfg(feature = "browser")]
mod browser_tests;
mod chrome_connector_smoke;
mod computer_adapter_e2e_tests;
mod computer_modules_tests;
mod delegate_mcp_plan_tests;
mod execution_tests;
mod file_tests;
mod media_tests;
mod memory_tests;
mod message_tool_tests;
mod network_tests;
mod task_time_tests;
#[cfg(feature = "vision")]
mod vision_tests;
mod web_tests;
