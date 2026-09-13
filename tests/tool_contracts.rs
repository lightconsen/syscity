//! Tool Contract Tests
//!
//! These tests verify that all registered tools expose stable JSON schemas
//! and execution contracts. Any tool that changes its name, parameter schema
//! shape, or result structure without updating these tests signals a
//! breaking change for LLM integrations.

use std::time::Duration;

use serde_json::json;
use syscity::providers::FunctionDefinition;
use syscity::tools::*;

/// Build a minimal tool context for contract verification.
mod common;

fn test_context() -> ToolContext {
    ToolContext::new("test_user", "test_conv")
        .with_working_dir("/tmp")
        .with_timeout(Duration::from_secs(5))
}

/// Build a registry with core tools for contract testing.
fn build_test_registry() -> ToolRegistry {
    // Embedder setup: some tools resolve paths at construction.
    common::install_test_root();
    let mut registry = ToolRegistry::new();

    registry.register(Box::new(ShellTool::new()));
    registry.register(Box::new(FileReadTool::new()));
    registry.register(Box::new(FileWriteTool::new()));
    registry.register(Box::new(FileEditTool::new()));
    registry.register(Box::new(GlobTool::new()));
    registry.register(Box::new(GrepTool::new()));
    registry.register(Box::new(TimeTool::new()));
    registry.register(Box::new(TodoTool::new()));
    registry.register(Box::new(CronTool::new()));
    registry.register(Box::new(WebSearchTool::new()));
    registry.register(Box::new(WebFetchTool::new()));
    registry.register(Box::new(UpdatePlanTool::new()));
    registry.register(Box::new(ProcessTool::new()));
    registry.register(Box::new(PdfTool::new()));
    registry.register(Box::new(ImageTool::new()));
    registry.register(Box::new(ImageGenerateTool::new()));
    registry.register(Box::new(TtsTool::new()));
    registry.register(Box::new(NodesTool::new()));

    // Mark some as privileged
    registry.mark_privileged("shell");
    registry.mark_privileged("file_write");
    registry.mark_privileged("file_edit");
    registry.mark_privileged("process");
    registry.mark_privileged("image_generate");

    registry
}

// ── Tool Schema Contract ─────────────────────────────────────────────────────

/// Verify that every built-in tool exposes a valid JSON Schema object.
#[test]
fn all_builtin_tools_have_valid_json_schema() {
    let registry = build_test_registry();
    let definitions = registry.get_definitions();

    assert!(!definitions.is_empty(), "registry must have tools");

    for def in definitions {
        let schema = def.parameters;

        // Contract: schema must be a JSON object
        assert!(
            schema.is_object(),
            "Tool '{}' parameters_schema must be a JSON object, got: {}",
            def.name,
            schema
        );

        // Contract: schema must have "type": "object" at root
        let schema_type = schema.get("type");
        assert!(schema_type.is_some(), "Tool '{}' schema missing 'type' field", def.name);
        assert_eq!(
            schema_type,
            Some(&json!("object")),
            "Tool '{}' schema root type must be 'object'",
            def.name
        );
    }
}

/// Verify tool names are stable (kebab-case, no spaces, non-empty).
#[test]
fn all_builtin_tool_names_follow_contract() {
    let registry = build_test_registry();
    let definitions = registry.get_definitions();

    for def in definitions {
        let name = def.name;

        assert!(!name.is_empty(), "Tool name must not be empty");
        assert!(
            !name.contains(' '),
            "Tool '{}' name must not contain spaces (use underscores)",
            name
        );
        assert!(
            name.chars()
                .all(|c| c.is_ascii_lowercase() || c == '_' || c == '-'),
            "Tool '{}' name must be ASCII lowercase with underscores/hyphens only",
            name
        );
    }
}

/// Verify tool descriptions are non-empty.
#[test]
fn all_builtin_tool_descriptions_are_present() {
    let registry = build_test_registry();
    let definitions = registry.get_definitions();

    for def in definitions {
        assert!(
            !def.description.is_empty(),
            "Tool '{}' must have a non-empty description",
            def.name
        );
    }
}

// ── ToolRegistry Contract ────────────────────────────────────────────────────

#[test]
fn tool_registry_lists_all_builtin_tools() {
    let registry = build_test_registry();
    let names = registry.list();

    // Contract: must include the core tools
    let expected_core = vec![
        "shell",
        "file_read",
        "file_write",
        "file_edit",
        "grep",
        "glob",
        "time",
        "todo",
        "cron",
        "web_search",
        "web_fetch",
        "update_plan",
        "process",
        "pdf",
        "image",
        "image_generate",
        "tts",
        "nodes",
    ];

    for tool in expected_core {
        assert!(
            names.iter().any(|n| n == tool),
            "ToolRegistry must include '{}' in default tools",
            tool
        );
    }
}

#[test]
fn tool_registry_can_retrieve_by_name() {
    let registry = build_test_registry();

    for name in registry.list() {
        assert!(
            registry.has(&name),
            "ToolRegistry must be able to retrieve '{}' via has()",
            name
        );
        assert!(
            registry.get(&name).is_some(),
            "ToolRegistry must be able to retrieve '{}' via get()",
            name
        );
    }
}

#[test]
fn tool_registry_get_available_filters_privileged_tools_for_community_trust() {
    let registry = build_test_registry();

    let trusted_ctx = test_context().with_skill_trust(SkillTrust::Trusted);
    let community_ctx = test_context().with_skill_trust(SkillTrust::Community);

    let trusted_tools = registry.get_available(&trusted_ctx);
    let community_tools = registry.get_available(&community_ctx);

    // Contract: Community trust must see fewer or equal tools
    assert!(
        community_tools.len() <= trusted_tools.len(),
        "Community trust should not see more tools than Trusted trust"
    );
}

// ── ToolExecutionResult Contract ─────────────────────────────────────────────

#[test]
fn tool_execution_result_success_contract() {
    let result = ToolExecutionResult::success("output text").with_data(json!({"key": "val"}));

    assert!(result.success);
    assert_eq!(result.output, "output text");
    assert!(result.error.is_none());
    assert_eq!(result.data, Some(json!({"key": "val"})));
}

#[test]
fn tool_execution_result_error_contract() {
    let result = ToolExecutionResult::error("something failed");

    assert!(!result.success);
    assert!(result.output.is_empty());
    assert_eq!(result.error, Some("something failed".to_string()));
}

#[test]
fn tool_execution_result_serializes_to_expected_shape() {
    let result = ToolExecutionResult::success("ok")
        .with_data(json!({"count": 42}))
        .with_execution_time(Duration::from_millis(150));

    let json = serde_json::to_value(&result).unwrap();

    assert!(json.get("success").is_some(), "missing 'success' field");
    assert!(json.get("output").is_some(), "missing 'output' field");
    assert!(json.get("error").is_some(), "missing 'error' field");
    assert!(json.get("data").is_some(), "missing 'data' field");
    assert!(json.get("execution_time").is_some(), "missing 'execution_time' field");

    assert_eq!(json["success"], true);
    assert_eq!(json["output"], "ok");
    assert_eq!(json["data"]["count"], 42);
}

#[test]
fn tool_execution_result_roundtrips_through_json() {
    let original = ToolExecutionResult::success("roundtrip test")
        .with_data(json!({"nested": {"value": true}}));

    let json = serde_json::to_string(&original).unwrap();
    let roundtripped: ToolExecutionResult =
        serde_json::from_str(&json).expect("ToolExecutionResult must roundtrip through JSON");

    assert_eq!(original.success, roundtripped.success);
    assert_eq!(original.output, roundtripped.output);
    assert_eq!(original.error, roundtripped.error);
    assert_eq!(original.data, roundtripped.data);
}

#[test]
fn tool_execution_result_to_tool_result_contract() {
    let exec = ToolExecutionResult::success("tool output");
    let tool_result = exec.to_tool_result("call_123");

    assert_eq!(tool_result.tool_call_id, "call_123");
    assert_eq!(tool_result.content, "tool output");
    assert_eq!(tool_result.is_error, Some(false));
}

#[test]
fn tool_execution_result_error_to_tool_result_includes_error_prefix() {
    let exec = ToolExecutionResult::error("bad args");
    let tool_result = exec.to_tool_result("call_456");

    assert_eq!(tool_result.tool_call_id, "call_456");
    assert!(tool_result.content.contains("Error:"));
    assert!(tool_result.content.contains("bad args"));
    assert_eq!(tool_result.is_error, Some(true));
}

// ── ToolContext Contract ─────────────────────────────────────────────────────

#[test]
fn tool_context_default_contract() {
    let ctx = ToolContext::default();

    assert!(ctx.user_id.is_empty());
    assert!(ctx.conversation_id.is_empty());
    assert!(!ctx.sandboxed());
    assert_eq!(ctx.model.skill_trust, SkillTrust::Trusted);
    assert!(ctx.allowed_paths().is_empty());
    assert!(ctx.allowed_commands().is_empty());
}

#[test]
fn tool_context_builder_contract() {
    let ctx = ToolContext::new("u1", "c1")
        .with_working_dir("/tmp")
        .with_timeout(Duration::from_secs(10))
        .allow_path("/tmp")
        .allow_command("ls")
        .with_sandboxed(true)
        .with_memory_limit(1024 * 1024)
        .with_skill_trust(SkillTrust::Community);

    assert_eq!(ctx.user_id, "u1");
    assert_eq!(ctx.conversation_id, "c1");
    assert_eq!(*ctx.working_directory(), std::path::PathBuf::from("/tmp"));
    assert_eq!(ctx.timeout(), Duration::from_secs(10));
    assert!(ctx.is_path_allowed(std::path::Path::new("/tmp")));
    assert!(ctx.is_command_allowed("ls"));
    assert!(ctx.sandboxed());
    assert_eq!(ctx.memory_limit(), Some(1024 * 1024));
    assert_eq!(ctx.model.skill_trust, SkillTrust::Community);
}

#[test]
fn tool_context_path_checking_contract() {
    let ctx = ToolContext::new("u", "c")
        .allow_path("/tmp")
        .allow_path("/home/user");

    assert!(ctx.is_path_allowed(std::path::Path::new("/tmp")));
    assert!(ctx.is_path_allowed(std::path::Path::new("/tmp/subdir")));

    let summary = ctx.resource_limits_summary();
    assert!(!summary.contains("Sandbox"));
}

#[test]
fn tool_context_command_checking_contract() {
    let ctx = ToolContext::new("u", "c")
        .allow_command("ls")
        .allow_command("cat");

    assert!(ctx.is_command_allowed("ls"));
    assert!(ctx.is_command_allowed("cat /etc/passwd")); // checks first token
    assert!(!ctx.is_command_allowed("rm"));
}

// ── SkillTrust Serialization Contract ────────────────────────────────────────

#[test]
fn skill_trust_serializes_to_snake_case() {
    let trusted = serde_json::to_value(SkillTrust::Trusted).unwrap();
    let community = serde_json::to_value(SkillTrust::Community).unwrap();

    assert_eq!(trusted, "trusted");
    assert_eq!(community, "community");
}

#[test]
fn skill_trust_roundtrips_through_json() {
    for original in [SkillTrust::Trusted, SkillTrust::Community] {
        let json = serde_json::to_string(&original).unwrap();
        let roundtripped: SkillTrust = serde_json::from_str(&json).unwrap();
        assert_eq!(original, roundtripped);
    }
}

#[test]
fn skill_trust_ordering_contract() {
    // Contract: Community < Trusted in ordering (for privilege escalation checks)
    assert!(SkillTrust::Community < SkillTrust::Trusted);
    assert!(SkillTrust::Trusted > SkillTrust::Community);
}

// ── Specific Tool Schema Contracts ───────────────────────────────────────────

/// Contract: shell tool must accept "command" and optionally "working_dir"
#[test]
fn shell_tool_schema_contract() {
    let registry = build_test_registry();
    let shell = registry.get("shell").expect("shell tool must exist");
    let schema = shell.parameters_schema();

    let props = schema
        .get("properties")
        .expect("schema must have properties");
    assert!(props.get("command").is_some(), "shell tool schema must have 'command' property");
}

/// Contract: file_read tool must accept "path"
#[test]
fn file_read_tool_schema_contract() {
    let registry = build_test_registry();
    let tool = registry
        .get("file_read")
        .expect("file_read tool must exist");
    let schema = tool.parameters_schema();

    let props = schema
        .get("properties")
        .expect("schema must have properties");
    assert!(props.get("path").is_some(), "file_read tool schema must have 'path' property");
}

/// Contract: time tool must accept "action"
#[test]
fn time_tool_schema_contract() {
    let registry = build_test_registry();
    let tool = registry.get("time").expect("time tool must exist");
    let schema = tool.parameters_schema();

    let props = schema
        .get("properties")
        .expect("schema must have properties");
    assert!(props.get("action").is_some(), "time tool schema must have 'action' property");

    let required = schema.get("required").expect("schema must have required");
    let req_arr = required.as_array().expect("required must be array");
    assert!(req_arr.contains(&json!("action")), "time tool must require 'action'");
}

/// Contract: todo tool schema takes a whole-snapshot "todos" array
#[test]
fn todo_tool_schema_contract() {
    let registry = build_test_registry();
    let tool = registry.get("todo").expect("todo tool must exist");
    let schema = tool.parameters_schema();

    let props = schema
        .get("properties")
        .expect("schema must have properties");
    assert!(props.get("todos").is_some(), "todo must have 'todos' property");
    let required = schema
        .get("required")
        .expect("schema must have required list");
    let req_arr = required.as_array().expect("required must be array");
    assert!(req_arr.contains(&json!("todos")), "todo must require 'todos'");
}

/// Contract: cron tool schema must have "action"
#[test]
fn cron_tool_schema_contract() {
    let registry = build_test_registry();
    let tool = registry.get("cron").expect("cron tool must exist");
    let schema = tool.parameters_schema();

    let props = schema
        .get("properties")
        .expect("schema must have properties");
    assert!(props.get("action").is_some(), "cron must have 'action' property");
}

/// Contract: grep tool schema must have "pattern" and "path"
#[test]
fn grep_tool_schema_contract() {
    let registry = build_test_registry();
    let tool = registry.get("grep").expect("grep tool must exist");
    let schema = tool.parameters_schema();

    let props = schema
        .get("properties")
        .expect("schema must have properties");
    assert!(props.get("pattern").is_some(), "grep must have 'pattern'");
    assert!(props.get("path").is_some(), "grep must have 'path'");
}

/// Contract: web_search tool schema must have "query"
#[test]
fn web_search_tool_schema_contract() {
    let registry = build_test_registry();
    let tool = registry
        .get("web_search")
        .expect("web_search tool must exist");
    let schema = tool.parameters_schema();

    let props = schema
        .get("properties")
        .expect("schema must have properties");
    assert!(props.get("query").is_some(), "web_search must have 'query' property");
}

/// Contract: process tool schema must have "action"
#[test]
fn process_tool_schema_contract() {
    let registry = build_test_registry();
    let tool = registry.get("process").expect("process tool must exist");
    let schema = tool.parameters_schema();

    let props = schema
        .get("properties")
        .expect("schema must have properties");
    assert!(props.get("action").is_some(), "process must have 'action' property");
}

/// Contract: pdf tool schema must have "content"
#[test]
fn pdf_tool_schema_contract() {
    let registry = build_test_registry();
    let tool = registry.get("pdf").expect("pdf tool must exist");
    let schema = tool.parameters_schema();

    let props = schema
        .get("properties")
        .expect("schema must have properties");
    assert!(props.get("content").is_some(), "pdf must have 'content' property");
}

// ── Sandbox Contract ─────────────────────────────────────────────────────────

#[test]
fn sandboxed_tool_wraps_inner_tool() {
    let inner = ShellTool::new();
    let config = SandboxConfig::default();
    let sandboxed = SandboxedTool::new(inner, config);

    // Contract: sandboxed tool delegates name to inner tool
    assert_eq!(sandboxed.name(), "shell");
}

#[test]
fn sandbox_config_default_contract() {
    let config = SandboxConfig::default();

    assert!(config.allow_file_access);
    assert!(!config.allow_network_access);
    assert!(config.allowed_paths.is_empty());
    assert!(config.blocked_paths.is_empty());
    assert_eq!(config.timeout, Duration::from_secs(60));
}

#[test]
fn sandbox_config_path_checking_contract() {
    let config = SandboxConfig {
        allow_file_access: true,
        allowed_paths: vec![std::path::PathBuf::from("/tmp")],
        blocked_paths: vec![std::path::PathBuf::from("/etc")],
        ..Default::default()
    };

    assert!(config
        .check_path(std::path::Path::new("/tmp/file.txt"))
        .is_ok());
    assert!(config
        .check_path(std::path::Path::new("/etc/passwd"))
        .is_err());
    assert!(config
        .check_path(std::path::Path::new("/home/user"))
        .is_err());
}

#[test]
fn sandbox_config_blocks_file_access_when_disabled() {
    let config = SandboxConfig {
        allow_file_access: false,
        ..Default::default()
    };

    assert!(config
        .check_path(std::path::Path::new("/tmp/file"))
        .is_err());
}

// ── FunctionDefinition Contract ──────────────────────────────────────────────

#[test]
fn function_definition_serializes_to_openai_compatible_shape() {
    let def = FunctionDefinition {
        name: "test_tool".to_string(),
        description: "A test tool".to_string(),
        parameters: json!({
            "type": "object",
            "properties": {
                "input": {"type": "string"}
            },
            "required": ["input"]
        }),
    };

    let json = serde_json::to_value(&def).unwrap();

    assert_eq!(json["name"], "test_tool");
    assert_eq!(json["description"], "A test tool");
    assert!(json.get("parameters").is_some());
    assert_eq!(json["parameters"]["type"], "object");
}

// ── Privilege System Contract ────────────────────────────────────────────────

#[test]
fn privilege_system_contract() {
    let mut registry = ToolRegistry::new();
    registry.register(Box::new(ShellTool::new()));
    registry.register(Box::new(FileReadTool::new()));

    registry.mark_privileged("shell");

    assert!(registry.is_privileged("shell"));
    assert!(!registry.is_privileged("file_read"));
    assert!(!registry.is_privileged("nonexistent"));
}

#[test]
fn circuit_breaker_contract() {
    let registry = ToolRegistry::new();

    assert!(!registry.is_degraded("any_tool"));
    assert!(registry.degraded_tools().is_empty());

    registry.record_failure("test_tool");
    registry.record_failure("test_tool");
    assert!(!registry.is_degraded("test_tool"));

    registry.record_failure("test_tool");
    assert!(registry.is_degraded("test_tool"));
    assert_eq!(registry.degraded_tools(), vec!["test_tool"]);

    registry.reset_failure("test_tool");
    assert!(!registry.is_degraded("test_tool"));
}

// ── Dynamic Tool Registration Contract ───────────────────────────────────────

#[test]
fn dynamic_tool_registration_contract() {
    let registry = ToolRegistry::new();

    registry.register_dynamic(std::sync::Arc::new(TimeTool::new()));

    assert!(registry.has("time"));
    // get() only covers static tools; dynamic tools are found via has() and list()
    assert!(registry.list().contains(&"time".to_string()));
}

#[test]
fn deregister_prefix_contract() {
    let registry = ToolRegistry::new();

    registry.register_dynamic(std::sync::Arc::new(TimeTool::new()));
    registry.register_dynamic(std::sync::Arc::new(ShellTool::new()));

    registry.deregister_prefix("shell");

    assert!(!registry.has("shell"));
    assert!(registry.has("time"));
}
