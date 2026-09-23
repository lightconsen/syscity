//! Tool abstractions for Syscity
//!
//! Tools are capabilities that the AI assistant can use to interact
//! with the world (execute shell commands, read files, search the web, etc.).
// INVARIANTS-NONE: tool results flow through the spill/fence/approval pipelines enforced per call; the todo store invariant lives with TodoStore in agent/.

pub mod approval;
pub mod ask_user;
pub mod escalation;
pub mod eval;
pub mod metadata;
pub mod permissions;
pub mod target_lock;

// Re-export approval types for convenience
pub use approval::{
    ApprovalDecision, ApprovalFilter, ApprovalQueue, ApprovalRequiredEvent, PendingApproval,
    PendingApprovalSummary, RiskLevel,
};
// Re-export ask_user types for convenience
pub use ask_user::{
    background_context_reason, AskEvent, AskQueue, AskRequest, AskRequiredEvent, AskResolvedEvent,
    AskUserTool, PendingQuestion,
};
// Re-export escalation helpers for convenience
pub use escalation::{declared_escalation, escalate_and_rerun, fence_denial_reason};
// Re-export permission types for convenience
pub use permissions::{EngineDecision, PermissionMode, PermissionsConfig, PermissionsRuntime};
// Re-export tool metadata for convenience
pub use metadata::ToolDescriptionMeta;

mod registrar;
mod registry;
mod types;
mod util;
mod validators;

pub use registrar::ToolRegistrar;
pub use registry::{ToolRegistry, WebSearchProviders};
pub use types::{
    BoxedTool, SharedTool, SkillTrust, Tool, ToolContext, ToolExecutionChunk, ToolExecutionResult,
    ToolId, ToolIdentity, ToolModel, ToolSandbox,
};
pub use util::create_schema;
pub use validators::{
    NameValidator, SchemaValidator, SecurityValidator, ToolValidationError, ToolValidator,
};

pub mod acp_tool;
pub mod agents_list;
pub mod browser;
pub mod canvas;
#[cfg(feature = "cloud")]
pub mod cloud_kb;
pub mod code_exec;
pub mod command_chain;
pub mod command_detector;
pub mod command_gate;
pub mod computer;
pub mod cron_tool;
pub mod delegate_tool;
pub mod file;
pub mod gateway;
pub mod grep;
pub mod heartbeat_tool;
pub mod hooks;
pub mod image;
pub mod list_capabilities;
pub mod memory;
pub mod message;
pub mod nodes;
pub mod patch;
pub mod pdf;
pub mod planner;
pub mod process;
pub mod process_runner;
pub mod report;
pub mod sandbox;
pub mod sandbox_interceptor;
pub mod screen_state;
pub mod sdk;
pub mod session;
pub mod shell;
pub mod shell_safety;
pub mod skill_tool;
pub(crate) mod spill;
pub mod stt;
pub mod svg_to_png;
pub mod time;
pub mod todo_tool;
pub mod tts;
pub mod update_plan;
pub mod web;
#[cfg(target_os = "windows")]
pub mod win_appcontainer;
pub mod write_guard;

pub use acp_tool::{AcpSessionTool, AcpSpawnTool};
pub use agents_list::AgentsListTool;
pub use browser::BrowserTool;
pub use canvas::CanvasTool;
pub use code_exec::CodeExecutionTool;
pub use cron_tool::CronTool;
pub use delegate_tool::{AgentResolver, DelegateTool};
pub use file::{FileEditTool, FileReadTool, FileWriteTool, GlobTool};
pub use gateway::GatewayTool;
pub use grep::GrepTool;
pub use heartbeat_tool::HeartbeatTool;
pub use hooks::{PostExecuteDecision, ToolHooks, ToolPolicyDecision};
pub use image::{ImageGenerateTool, ImageTool};
pub use list_capabilities::ListCapabilitiesTool;
pub use memory::{MemoryGetTool, MemorySearchTool, MemoryTool};
pub use message::MessageTool;
pub use nodes::NodesTool;
pub use patch::ApplyPatchTool;
pub use pdf::PdfTool;
pub use process::ProcessTool;
pub use process_runner::{
    CommandOutput, NamespacePosture, ProcessError, ProcessRequest, ProcessRunner, StdioMode,
};
pub use report::WriteReportTool;
pub use sandbox::{SandboxConfig, SandboxedTool};
pub use sdk::{
    CapabilityFilter, SyncResult, ToolCapabilities, ToolMetadata, ToolPack, ToolSdk, ToolSdkError,
};
pub use session::{
    SessionStatusTool, SessionsHistoryTool, SessionsListTool, SessionsSendTool, SessionsYieldTool,
};
pub use shell::ShellTool;
pub use skill_tool::SkillTool;
pub(crate) use spill::head_tail_truncate;
pub use stt::SttTool;
pub use svg_to_png::SvgToPngTool;
pub use time::TimeTool;
pub use todo_tool::{TodoState, TodoTool};
pub use tts::TtsTool;
pub use update_plan::UpdatePlanTool;
pub use web::{WebFetchTool, WebSearchTool};
pub use write_guard::{WriteGuard, WriteGuardError};

// Re-exported from the top-level mcp module for backward compatibility.
pub use crate::mcp::{McpConnectionTool, McpManager};

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use async_trait::async_trait;
    use serde_json::Value;

    use super::*;

    #[test]
    fn test_tool_id() {
        let id = ToolId::new("test_tool");
        assert_eq!(id.0, "test_tool");
    }

    #[test]
    fn test_tool_context() {
        let ctx = ToolContext::new("user1", "conv1")
            .with_timeout(Duration::from_secs(60))
            .allow_path("/tmp")
            .allow_command("ls");

        assert_eq!(ctx.user_id, "user1");
        assert_eq!(ctx.timeout(), Duration::from_secs(60));
        assert!(ctx.is_command_allowed("ls"));
        assert!(!ctx.is_command_allowed("rm"));
    }

    #[test]
    fn test_tool_execution_result() {
        let success = ToolExecutionResult::success("Done!");
        assert!(success.success);
        assert_eq!(success.output, "Done!");

        let error = ToolExecutionResult::error("Failed!");
        assert!(!error.success);
        assert_eq!(error.error, Some("Failed!".to_string()));
    }

    #[test]
    fn test_tool_registry() {
        let registry = ToolRegistry::new();
        assert!(registry.list().is_empty());
        assert!(!registry.has("test"));
    }

    #[test]
    fn test_create_schema() {
        let schema = create_schema(
            "A test tool",
            serde_json::json!({
                "name": { "type": "string" },
                "count": { "type": "integer" }
            }),
            vec!["name"],
        );

        assert_eq!(schema["type"], "object");
        assert_eq!(schema["description"], "A test tool");
        assert_eq!(schema["required"], serde_json::json!(["name"]));
    }

    // ToolRegistrar tests

    #[test]
    fn test_tool_registrar_creation() {
        let registrar = ToolRegistrar::new();
        assert!(registrar.list().is_empty());
    }

    #[test]
    fn test_name_validator_valid() {
        struct ValidTool;

        #[async_trait]
        impl Tool for ValidTool {
            fn name(&self) -> &str {
                "valid_tool"
            }
            fn description(&self) -> &str {
                "A valid test tool"
            }
            fn parameters_schema(&self) -> Value {
                create_schema("Test", serde_json::json!({}), Vec::<String>::new())
            }
            async fn execute(
                &self,
                _args: Value,
                _ctx: &ToolContext,
            ) -> crate::Result<ToolExecutionResult> {
                Ok(ToolExecutionResult::success("ok"))
            }
        }

        let validator = NameValidator;
        assert!(validator.validate(&ValidTool).is_ok());
    }

    #[test]
    fn test_name_validator_invalid() {
        struct InvalidTool;

        #[async_trait]
        impl Tool for InvalidTool {
            fn name(&self) -> &str {
                "123_invalid"
            }
            fn description(&self) -> &str {
                "A test tool"
            }
            fn parameters_schema(&self) -> Value {
                create_schema("Test", serde_json::json!({}), Vec::<String>::new())
            }
            async fn execute(
                &self,
                _args: Value,
                _ctx: &ToolContext,
            ) -> crate::Result<ToolExecutionResult> {
                Ok(ToolExecutionResult::success("ok"))
            }
        }

        let validator = NameValidator;
        let result = validator.validate(&InvalidTool);
        assert!(result.is_err());
        assert!(matches!(result.unwrap_err(), ToolValidationError::InvalidName(_)));
    }

    #[test]
    fn test_security_validator_path_traversal() {
        let validator = SecurityValidator;

        // Valid paths
        assert!(validator
            .check_path_traversal("/home/user/file.txt")
            .is_ok());
        assert!(validator.check_path_traversal("./file.txt").is_ok());

        // Invalid paths with traversal
        assert!(validator.check_path_traversal("../etc/passwd").is_err());
        assert!(validator
            .check_path_traversal("foo/../../../etc/passwd")
            .is_err());
    }

    #[test]
    fn test_security_validator_command_injection() {
        let validator = SecurityValidator;

        // Valid commands
        assert!(validator.check_command_injection("ls -la").is_ok());
        assert!(validator.check_command_injection("cat file.txt").is_ok());

        // Invalid commands with injection
        assert!(validator.check_command_injection("ls; rm -rf /").is_err());
        assert!(validator
            .check_command_injection("cat file | grep test")
            .is_err());
        assert!(validator.check_command_injection("echo $(whoami)").is_err());
    }

    #[test]
    fn test_security_validator_input_validation() {
        let validator = SecurityValidator;

        // Valid input
        let valid_args = serde_json::json!({
            "path": "/home/user/file.txt",
            "content": "hello world"
        });
        assert!(validator.validate_input("test", &valid_args).is_ok());

        // Invalid input with path traversal
        let invalid_args = serde_json::json!({
            "path": "../../../etc/passwd",
            "content": "malicious"
        });
        assert!(validator.validate_input("test", &invalid_args).is_err());

        // Invalid input with command injection
        let cmd_inject_args = serde_json::json!({
            "command": "ls; rm -rf /"
        });
        assert!(validator.validate_input("test", &cmd_inject_args).is_err());
    }

    // ── Path sandbox penetration tests ───────────────────────────────────────

    use tempfile::tempdir;

    fn workspace_context(root: &std::path::Path) -> ToolContext {
        ToolContext::new("user", "conv1")
            .with_workspace_root(root)
            .with_workspace_only(true)
    }

    #[test]
    fn test_path_allowed_relative_within_workspace() {
        let tmp = tempdir().unwrap();
        let ws = tmp.path().join("workspace");
        std::fs::create_dir(&ws).unwrap();
        let ctx = workspace_context(&ws);
        assert!(ctx.is_path_allowed(std::path::Path::new("notes.txt")));
    }

    #[test]
    fn test_path_allowed_relative_traversal_outside_workspace() {
        let tmp = tempdir().unwrap();
        let ws = tmp.path().join("workspace");
        std::fs::create_dir(&ws).unwrap();
        let outside = tmp.path().join("outside.txt");
        std::fs::write(&outside, "secret").unwrap();
        let ctx = workspace_context(&ws);
        assert!(!ctx.is_path_allowed(std::path::Path::new("../outside.txt")));
    }

    #[test]
    fn test_path_allowed_dotdot_combo_traversal() {
        let tmp = tempdir().unwrap();
        let ws = tmp.path().join("workspace");
        std::fs::create_dir_all(ws.join("subdir")).unwrap();
        let outside = tmp.path().join("outside.txt");
        std::fs::write(&outside, "secret").unwrap();
        let ctx = workspace_context(&ws);
        assert!(!ctx.is_path_allowed(std::path::Path::new("subdir/../../outside.txt")));
    }

    #[test]
    fn test_path_allowed_absolute_outside_workspace() {
        let tmp = tempdir().unwrap();
        let ws = tmp.path().join("workspace");
        std::fs::create_dir(&ws).unwrap();
        let ctx = workspace_context(&ws);
        assert!(!ctx.is_path_allowed(std::path::Path::new("/etc/passwd")));
    }

    #[test]
    fn test_path_allowed_absolute_inside_workspace() {
        let tmp = tempdir().unwrap();
        let ws = tmp.path().join("workspace");
        std::fs::create_dir(&ws).unwrap();
        let inside = ws.join("file.txt");
        std::fs::write(&inside, "ok").unwrap();
        let ctx = workspace_context(&ws);
        assert!(ctx.is_path_allowed(&inside));
    }

    #[cfg(unix)]
    #[test]
    fn test_path_allowed_symlink_escape() {
        use std::os::unix::fs::symlink;
        let tmp = tempdir().unwrap();
        let ws = tmp.path().join("workspace");
        std::fs::create_dir(&ws).unwrap();
        let outside = tmp.path().join("outside");
        std::fs::create_dir(&outside).unwrap();
        let secret = outside.join("secret.txt");
        std::fs::write(&secret, "secret").unwrap();
        symlink(&outside, ws.join("link")).unwrap();
        let ctx = workspace_context(&ws);
        assert!(!ctx.is_path_allowed(std::path::Path::new("link/secret.txt")));
    }

    #[test]
    fn test_path_allowed_tilde_expansion_outside_workspace() {
        let tmp = tempdir().unwrap();
        let ws = tmp.path().join("workspace");
        std::fs::create_dir(&ws).unwrap();
        let ctx = workspace_context(&ws);
        // `~` resolves to the user's home directory, which is outside the workspace.
        assert!(!ctx.is_path_allowed(std::path::Path::new("~/anything.txt")));
    }

    #[test]
    fn test_path_allowed_with_allowlist() {
        let tmp = tempdir().unwrap();
        let allowed = tmp.path().join("allowed");
        std::fs::create_dir(&allowed).unwrap();
        let ctx = ToolContext::new("user", "conv1")
            .with_workspace_only(false)
            .allow_path(&allowed);
        let inside = allowed.join("file.txt");
        assert!(ctx.is_path_allowed(&inside));
        assert!(!ctx.is_path_allowed(std::path::Path::new("/etc/passwd")));
    }

    // ── Skill trust boundary tests ───────────────────────────────────────────

    struct ReadTool;

    #[async_trait]
    impl Tool for ReadTool {
        fn name(&self) -> &str {
            "read"
        }

        fn description(&self) -> &str {
            "Read a file"
        }

        fn parameters_schema(&self) -> Value {
            create_schema(
                "Read a file",
                serde_json::json!({"path": {"type": "string"}}),
                vec!["path"],
            )
        }

        async fn execute(
            &self,
            _args: Value,
            _ctx: &ToolContext,
        ) -> crate::Result<ToolExecutionResult> {
            Ok(ToolExecutionResult::success("ok"))
        }
    }

    #[test]
    fn test_skill_trust_boundary_excludes_privileged_tools() {
        let mut registry = ToolRegistry::new();
        registry.register(Box::new(ShellTool::new()));
        registry.register(Box::new(ReadTool));
        registry.mark_privileged("shell");

        let trusted_ctx = ToolContext::new("user", "conv1").with_skill_trust(SkillTrust::Trusted);
        let community_ctx =
            ToolContext::new("user", "conv1").with_skill_trust(SkillTrust::Community);

        let trusted: Vec<String> = registry
            .get_available(&trusted_ctx)
            .into_iter()
            .map(|d| d.name)
            .collect();
        let community: Vec<String> = registry
            .get_available(&community_ctx)
            .into_iter()
            .map(|d| d.name)
            .collect();

        assert!(trusted.contains(&"shell".to_string()), "Trusted context should see shell");
        assert!(trusted.contains(&"read".to_string()), "Trusted context should see read");
        assert!(
            !community.contains(&"shell".to_string()),
            "Community context must not see shell"
        );
        assert!(community.contains(&"read".to_string()), "Community context should see read");
    }

    #[test]
    fn test_skill_trust_backward_compatible() {
        let mut registry = ToolRegistry::new();
        registry.register(Box::new(ShellTool::new()));
        registry.register(Box::new(ReadTool));
        registry.mark_privileged("shell");

        // No user_context/tool_policy: SkillTrust alone governs availability.
        let community_ctx =
            ToolContext::new("user", "conv1").with_skill_trust(SkillTrust::Community);
        let available: Vec<String> = registry
            .get_available(&community_ctx)
            .into_iter()
            .map(|d| d.name)
            .collect();

        assert!(!available.contains(&"shell".to_string()));
        assert!(available.contains(&"read".to_string()));
    }

    // ── Tool gating tests ────────────────────────────────────────────────────

    #[test]
    fn test_plugin_allowlist_gating() {
        let registry = ToolRegistry::new();
        registry.register_dynamic(std::sync::Arc::new(ReadTool));
        registry.register_dynamic(std::sync::Arc::new(PluginTool));
        registry.register_dynamic(std::sync::Arc::new(BlockedPluginTool));

        let ctx =
            ToolContext::new("user", "conv1").with_plugin_allowlist(vec!["allowed__".to_string()]);

        let available: Vec<String> = registry
            .get_available(&ctx)
            .into_iter()
            .map(|d| d.name)
            .collect();

        assert!(available.contains(&"read".to_string()));
        assert!(available.contains(&"allowed__foo".to_string()));
        assert!(!available.contains(&"blocked__bar".to_string()));
    }

    struct PluginTool;

    #[async_trait]
    impl Tool for PluginTool {
        fn name(&self) -> &str {
            "allowed__foo"
        }

        fn description(&self) -> &str {
            "An allowed plugin tool"
        }

        fn parameters_schema(&self) -> Value {
            create_schema("Plugin", serde_json::json!({}), Vec::<String>::new())
        }

        async fn execute(
            &self,
            _args: Value,
            _ctx: &ToolContext,
        ) -> crate::Result<ToolExecutionResult> {
            Ok(ToolExecutionResult::success("plugin ok"))
        }
    }

    struct BlockedPluginTool;

    #[async_trait]
    impl Tool for BlockedPluginTool {
        fn name(&self) -> &str {
            "blocked__bar"
        }

        fn description(&self) -> &str {
            "A blocked plugin tool"
        }

        fn parameters_schema(&self) -> Value {
            create_schema("Blocked", serde_json::json!({}), Vec::<String>::new())
        }

        async fn execute(
            &self,
            _args: Value,
            _ctx: &ToolContext,
        ) -> crate::Result<ToolExecutionResult> {
            Ok(ToolExecutionResult::success("blocked"))
        }
    }

    // ── execute()/execute_call() integration tests ───────────────────────────

    #[tokio::test]
    async fn test_execute_simple_tool() {
        let mut registry = ToolRegistry::new();
        registry.register(Box::new(ReadTool));

        let ctx = ToolContext::new("user", "conv1");
        let result = registry
            .execute("read", serde_json::json!({"path": "/tmp/test"}), &ctx)
            .await;

        assert!(result.is_some(), "execute should return Some for known tools");
        let inner = result.unwrap();
        assert!(inner.is_ok(), "ReadTool should succeed");
        assert_eq!(inner.unwrap().output, "ok");
    }

    #[tokio::test]
    async fn test_execute_unknown_tool() {
        let registry = ToolRegistry::new();
        let ctx = ToolContext::new("user", "conv1");
        let result = registry
            .execute("nonexistent", serde_json::json!({}), &ctx)
            .await;

        assert!(result.is_none(), "execute should return None for unknown tools");
    }

    #[tokio::test]
    async fn test_execute_blocked_tool() {
        let mut registry = ToolRegistry::new();
        registry.register(Box::new(ReadTool));

        // Block the tool by prefix
        registry.deregister_prefix("read");

        let ctx = ToolContext::new("user", "conv1");
        let result = registry
            .execute("read", serde_json::json!({"path": "/tmp/test"}), &ctx)
            .await;

        assert!(result.is_none(), "execute should return None for blocked tools");
    }

    #[tokio::test]
    async fn test_execute_no_cache_skips_cache() {
        let mut registry = ToolRegistry::new();
        registry.register(Box::new(ReadTool));

        let ctx = ToolContext::new("user", "conv1");
        // Execute once to warm cache
        let r1 = registry
            .execute("read", serde_json::json!({"path": "/tmp/test"}), &ctx)
            .await;
        assert!(r1.is_some());
        assert!(r1.unwrap().is_ok());

        // execute_no_cache should bypass cache
        let r2 = registry
            .execute_no_cache("read", serde_json::json!({"path": "/tmp/test"}), &ctx)
            .await;
        assert!(r2.is_some());
        assert!(r2.unwrap().is_ok());
    }

    #[tokio::test]
    async fn test_execute_call_simple() {
        let mut registry = ToolRegistry::new();
        registry.register(Box::new(ReadTool));

        let call = crate::providers::FunctionCall {
            name: "read".to_string(),
            arguments: "{\"path\": \"/tmp/test\"}".to_string(),
        };
        let ctx = ToolContext::new("user", "conv1");

        let result = registry.execute_call(&call, &ctx).await;
        assert!(result.is_ok());
        assert_eq!(result.unwrap().output, "ok");
    }

    // ── Post-execute hook pipeline ────────────────────────────────────────

    struct DataTool;

    #[async_trait]
    impl Tool for DataTool {
        fn name(&self) -> &str {
            "data_tool"
        }

        fn description(&self) -> &str {
            "A tool returning structured data"
        }

        fn parameters_schema(&self) -> Value {
            create_schema("Data", serde_json::json!({}), Vec::<String>::new())
        }

        async fn execute(
            &self,
            _args: Value,
            _ctx: &ToolContext,
        ) -> crate::Result<ToolExecutionResult> {
            Ok(ToolExecutionResult::success("raw output").with_data(serde_json::json!({"k": 1})))
        }
    }

    #[tokio::test]
    async fn test_post_execute_block_on_execute_call() {
        let mut registry = ToolRegistry::new();
        registry.register(Box::new(ReadTool));
        registry.set_hooks(ToolHooks::new().post_execute(|_, _, _, _ctx, _| async move {
            PostExecuteDecision::Block("write to workspace instead".to_string())
        }));

        let call = crate::providers::FunctionCall {
            name: "read".to_string(),
            arguments: "{}".to_string(),
        };
        let ctx = ToolContext::new("user", "conv1");
        let result = registry.execute_call(&call, &ctx).await.unwrap();

        // Blocked: original output is confiscated, error carries the feedback.
        assert!(!result.success);
        assert_eq!(result.error.as_deref(), Some("write to workspace instead"));
        assert!(result.output.is_empty());
        // What the model sees via to_tool_result:
        let tool_result = result.to_tool_result("c1");
        assert_eq!(tool_result.is_error, Some(true));
        assert_eq!(tool_result.content, "Error: write to workspace instead");
    }

    #[tokio::test]
    async fn test_post_execute_replace_output_preserves_data() {
        let mut registry = ToolRegistry::new();
        registry.register(Box::new(DataTool));
        registry.set_hooks(ToolHooks::new().post_execute(|_, _, _, _ctx, _| async move {
            PostExecuteDecision::ReplaceOutput("preview…".to_string())
        }));

        let ctx = ToolContext::new("user", "conv1");
        let result = registry
            .execute("data_tool", serde_json::json!({}), &ctx)
            .await
            .unwrap()
            .unwrap();

        assert!(result.success);
        assert_eq!(result.output, "preview…");
        assert_eq!(result.data, Some(serde_json::json!({"k": 1})));
    }

    #[tokio::test]
    async fn test_post_execute_block_on_streaming() {
        let mut registry = ToolRegistry::new();
        registry.register(Box::new(StreamingTool));
        registry.set_hooks(ToolHooks::new().post_execute(|_, _, _, _ctx, _| async move {
            PostExecuteDecision::Block("blocked after stream".to_string())
        }));

        let call = crate::providers::FunctionCall {
            name: "streaming_test".to_string(),
            arguments: "{}".to_string(),
        };
        let ctx = ToolContext::new("user", "conv1");
        let result = registry
            .execute_call_streaming(&call, &ctx, |_chunk| async move {})
            .await
            .unwrap();

        assert!(!result.success);
        assert_eq!(result.error.as_deref(), Some("blocked after stream"));
    }

    #[tokio::test]
    async fn test_post_execute_absent_passes_through() {
        let mut registry = ToolRegistry::new();
        registry.register(Box::new(DataTool));

        let ctx = ToolContext::new("user", "conv1");
        let result = registry
            .execute("data_tool", serde_json::json!({}), &ctx)
            .await
            .unwrap()
            .unwrap();

        assert!(result.success);
        assert_eq!(result.output, "raw output");
        assert_eq!(result.data, Some(serde_json::json!({"k": 1})));
    }

    // ── Spill stage ───────────────────────────────────────────────────────

    struct BigTool;

    #[async_trait]
    impl Tool for BigTool {
        fn name(&self) -> &str {
            "big"
        }

        fn description(&self) -> &str {
            "A tool returning an oversized output"
        }

        fn parameters_schema(&self) -> Value {
            create_schema("Big", serde_json::json!({}), Vec::<String>::new())
        }

        async fn execute(
            &self,
            _args: Value,
            _ctx: &ToolContext,
        ) -> crate::Result<ToolExecutionResult> {
            Ok(ToolExecutionResult::success("x".repeat(500)))
        }
    }

    fn tmp_workspace() -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("syscity_spill_reg_{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn spill_files(dir: &std::path::Path) -> Vec<std::path::PathBuf> {
        let spill_dir = dir.join(".syscity/spill");
        match std::fs::read_dir(&spill_dir) {
            Ok(rd) => rd.filter_map(|e| e.ok().map(|e| e.path())).collect(),
            Err(_) => Vec::new(),
        }
    }

    #[tokio::test]
    async fn test_spill_oversized_output_via_execute_call() {
        let tmp = tmp_workspace();
        let mut registry = ToolRegistry::new().with_spill_threshold(Some(64));
        registry.register(Box::new(BigTool));

        let ctx = ToolContext::new("user", "conv1").with_workspace_root(tmp.clone());
        let call = crate::providers::FunctionCall {
            name: "big".to_string(),
            arguments: "{}".to_string(),
        };
        let result = registry.execute_call(&call, &ctx).await.unwrap();

        assert!(result.success);
        assert!(result.output.contains(".syscity/spill/"), "preview points at spill file");
        assert!(result.output.contains("file_read"), "retrieval hint present");
        assert!(result.output.len() <= 64 + 260, "bounded: {}", result.output.len());
        assert_eq!(result.data.as_ref().unwrap()["spill"]["total_bytes"], serde_json::json!(500));

        let files = spill_files(&tmp);
        assert_eq!(files.len(), 1);
        let on_disk = std::fs::read_to_string(&files[0]).unwrap();
        assert_eq!(on_disk.len(), 500, "full output preserved on disk");

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[tokio::test]
    async fn test_spill_skipped_under_threshold() {
        let tmp = tmp_workspace();
        let mut registry = ToolRegistry::new(); // default 32KB threshold
        registry.register(Box::new(BigTool));

        let ctx = ToolContext::new("user", "conv1").with_workspace_root(tmp.clone());
        let call = crate::providers::FunctionCall {
            name: "big".to_string(),
            arguments: "{}".to_string(),
        };
        let result = registry.execute_call(&call, &ctx).await.unwrap();

        assert_eq!(result.output, "x".repeat(500), "untouched under threshold");
        assert!(spill_files(&tmp).is_empty());

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[tokio::test]
    async fn test_spill_skipped_after_hook_block() {
        let tmp = tmp_workspace();
        let mut registry = ToolRegistry::new().with_spill_threshold(Some(64));
        registry.register(Box::new(BigTool));
        registry.set_hooks(ToolHooks::new().post_execute(|_, _, _, _ctx, _| async move {
            PostExecuteDecision::Block("confiscated".to_string())
        }));

        let ctx = ToolContext::new("user", "conv1").with_workspace_root(tmp.clone());
        let call = crate::providers::FunctionCall {
            name: "big".to_string(),
            arguments: "{}".to_string(),
        };
        let result = registry.execute_call(&call, &ctx).await.unwrap();

        assert!(!result.success);
        assert_eq!(result.error.as_deref(), Some("confiscated"));
        assert!(spill_files(&tmp).is_empty(), "blocked results are never spilled");

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[tokio::test]
    async fn test_file_read_spills_unless_targeting_spill_dir() {
        let tmp = tmp_workspace();
        let mut registry = ToolRegistry::new().with_spill_threshold(Some(64));
        registry.register(Box::new(FileReadBigTool));

        let ctx = ToolContext::new("user", "conv1").with_workspace_root(tmp.clone());

        // A large read of a non-spill path now spills like any other tool.
        let spill_arg = crate::providers::FunctionCall {
            name: "file_read".to_string(),
            arguments: r#"{"path":"/somewhere/else/big.txt"}"#.to_string(),
        };
        let result = registry.execute_call(&spill_arg, &ctx).await.unwrap();
        assert!(result.output.len() < 500, "large file_read spills (preview bounded)");
        assert!(result.output.contains(".syscity/spill/"), "preview points at spill file");
        assert_eq!(spill_files(&tmp).len(), 1);

        // Re-reading a spilled artifact under the spill dir is exempt — the
        // whole content comes back and no second spill file is written.
        let spill_path = spill_files(&tmp)[0].clone();
        let re_arg = crate::providers::FunctionCall {
            name: "file_read".to_string(),
            arguments: serde_json::json!({ "path": spill_path.display().to_string() }).to_string(),
        };
        let result = registry.execute_call(&re_arg, &ctx).await.unwrap();
        assert_eq!(result.output, "y".repeat(500), "spill-dir read returns whole content");
        assert_eq!(spill_files(&tmp).len(), 1, "no re-spill for spill-dir reads");

        let _ = std::fs::remove_dir_all(&tmp);
    }

    struct FileReadBigTool;

    #[async_trait]
    impl Tool for FileReadBigTool {
        fn name(&self) -> &str {
            "file_read"
        }

        fn description(&self) -> &str {
            "Stand-in for the real file_read tool"
        }

        fn parameters_schema(&self) -> Value {
            create_schema("FileRead", serde_json::json!({}), Vec::<String>::new())
        }

        async fn execute(
            &self,
            _args: Value,
            _ctx: &ToolContext,
        ) -> crate::Result<ToolExecutionResult> {
            Ok(ToolExecutionResult::success("y".repeat(500)))
        }
    }

    // ── Streaming tool tests ─────────────────────────────────────────────────

    struct StreamingTool;

    #[async_trait]
    impl Tool for StreamingTool {
        fn name(&self) -> &str {
            "streaming_test"
        }

        fn description(&self) -> &str {
            "A tool that streams output"
        }

        fn parameters_schema(&self) -> Value {
            create_schema("Stream", serde_json::json!({}), Vec::<String>::new())
        }

        fn capabilities(&self) -> crate::tools::sdk::ToolCapabilities {
            crate::tools::sdk::ToolCapabilities {
                streaming: true,
                ..Default::default()
            }
        }

        async fn execute(
            &self,
            _args: Value,
            _ctx: &ToolContext,
        ) -> crate::Result<ToolExecutionResult> {
            Ok(ToolExecutionResult::success("final"))
        }
    }

    #[tokio::test]
    async fn test_tool_execute_stream_default() {
        let tool = StreamingTool;
        let ctx = ToolContext::new("user", "conv1");
        let mut stream = tool.execute_stream(serde_json::json!({}), &ctx);

        let chunks: Vec<ToolExecutionChunk> = tokio_stream::StreamExt::collect(&mut stream).await;
        assert_eq!(chunks.len(), 1);
        assert!(matches!(chunks[0], ToolExecutionChunk::Output(ref s) if s == "final"));
    }

    #[tokio::test]
    async fn test_registry_execute_call_streaming() {
        let mut registry = ToolRegistry::new();
        registry.register(Box::new(StreamingTool));

        let call = crate::providers::FunctionCall {
            name: "streaming_test".to_string(),
            arguments: "{}".to_string(),
        };
        let ctx = ToolContext::new("user", "conv1");

        let mut chunks = Vec::new();
        let result = registry
            .execute_call_streaming(&call, &ctx, |chunk| {
                chunks.push(chunk.clone());
                async move {}
            })
            .await;

        assert!(result.is_ok(), "streaming execution should succeed");
        let result = result.unwrap();
        assert_eq!(result.output, "final");
        assert!(chunks
            .iter()
            .any(|c| matches!(c, ToolExecutionChunk::Output(s) if s == "final")));
    }

    #[test]
    fn test_tool_execution_chunk_serialization() {
        let chunk = ToolExecutionChunk::Output("hello".to_string());
        let json = serde_json::to_string(&chunk).unwrap();
        assert!(json.contains("\"kind\":\"output\""));
        assert!(json.contains("\"payload\":\"hello\""));

        let decoded: ToolExecutionChunk = serde_json::from_str(&json).unwrap();
        assert!(matches!(decoded, ToolExecutionChunk::Output(s) if s == "hello"));
    }

    #[test]
    fn test_parse_tool_args_clean_json() {
        let registry = ToolRegistry::new();
        let result = registry.parse_tool_args(r#"{"cmd": "echo hello"}"#, "test_tool");
        assert!(result.is_ok());
        assert_eq!(result.unwrap()["cmd"], "echo hello");
    }

    #[test]
    fn test_parse_tool_args_trailing_text() {
        let registry = ToolRegistry::new();
        // DeepSeek appends text after JSON
        let result =
            registry.parse_tool_args(r#"{"cmd": "echo hello"} some trailing text"#, "test_tool");
        assert!(result.is_ok(), "should handle trailing text: {:?}", result);
        assert_eq!(result.unwrap()["cmd"], "echo hello");
    }

    #[test]
    fn test_parse_tool_args_multiple_objects() {
        let registry = ToolRegistry::new();
        // LLM produces two consecutive JSON objects
        let result = registry.parse_tool_args(r#"{"cmd": "first"} {"cmd": "second"}"#, "test_tool");
        assert!(result.is_ok(), "should handle multiple objects: {:?}", result);
        // Should return only the first value
        assert_eq!(result.unwrap()["cmd"], "first");
    }

    #[test]
    fn test_parse_tool_args_empty() {
        let registry = ToolRegistry::new();
        let result = registry.parse_tool_args("", "test_tool");
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), serde_json::json!({}));
    }

    #[test]
    fn test_parse_tool_args_whitespace_only() {
        let registry = ToolRegistry::new();
        let result = registry.parse_tool_args("   ", "test_tool");
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), serde_json::json!({}));
    }

    #[test]
    fn test_parse_tool_args_invalid_json() {
        let registry = ToolRegistry::new();
        let result = registry.parse_tool_args("not valid json", "test_tool");
        assert!(result.is_err());
    }
}
