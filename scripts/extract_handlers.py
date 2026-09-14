#!/usr/bin/env python3
"""Extract handler functions from gateway/mod.rs into handlers/*.rs files."""

import re
import os
from pathlib import Path

# Derived from the script's own location so it works from any checkout
# (`~/...` would not help: `pathlib.Path` does not expand a leading `~`).
REPO_ROOT = Path(__file__).resolve().parents[1]
MOD_RS = REPO_ROOT / "src/gateway/mod.rs"
HANDLERS_DIR = REPO_ROOT / "src/gateway/handlers"

HANDLER_MAP = {
    "web_terminal_html_handler": "web_ui",
    "favicon_handler": "web_ui",
    "asset_handler": "web_ui",
    "syscity_png_handler": "web_ui",
    "admin_redirect_handler": "web_ui",
    "health_handler": "health",
    "ready_handler": "health",
    "live_handler": "health",
    "metrics_handler": "health",
    "build_prometheus_metrics": "health",
    "build_health_report": "health",
    "status_handler": "health",
    "repair_status_handler": "health",
    "cost_status_handler": "health",
    "computer_screenshot_handler": "computer",
    "computer_execute_handler": "computer",
    "computer_status_handler": "computer",
    "chat_handler": "chat",
    "web_terminal_chat_handler": "chat",
    "send_message_handler": "chat",
    "get_conversation_history_handler": "chat",
    "get_last_conversation_handler": "chat",
    "list_conversations_handler": "chat",
    "canvas_ws_handler": "chat",  # Also in canvas
    "list_agents_handler": "agents",
    "create_agent_handler": "agents",
    "get_agent_handler": "agents",
    "delete_agent_handler": "agents",
    "list_channels_handler": "agents",
    "create_canvas_handler": "canvas",
    "get_canvas_handler": "canvas",
    "delete_canvas_handler": "canvas",
    "handle_canvas_websocket": "canvas",
    "list_providers_handler": "providers",
    "get_provider_health_handler": "providers",
    "switch_model_handler": "providers",
    "enable_provider_handler": "providers",
    "disable_provider_handler": "providers",
    "check_provider_handler": "providers",
    "provider_usage_handler": "providers",
    "provider_usage_by_id_handler": "providers",
    "get_fallback_chain_handler": "providers",
    "set_fallback_chain_handler": "providers",
    "list_models_handler": "providers",
    "get_default_model_handler": "providers",
    "openai_list_models_handler": "providers",
    "openai_chat_completions_handler": "openai",
    "json_rpc_error_response": "openai",
    "get_auth_profile_handler": "auth_profiles",
    "rotate_auth_profile_handler": "auth_profiles",
    "list_auth_profiles_handler": "auth_profiles",
    "memory_search_handler": "memory",
    "memory_add_handler": "memory",
    "list_memory_collections_handler": "memory",
    "list_plugins_handler": "plugins",
    "enable_plugin_handler": "plugins",
    "disable_plugin_handler": "plugins",
    "unload_plugin_handler": "plugins",
    "reload_plugin_handler": "plugins",
    "reload_plugins_handler": "plugins",
    "list_skills_handler": "skills",
    "get_skill_handler": "skills",
    "enable_skill_handler": "skills",
    "disable_skill_handler": "skills",
    "run_skill_handler": "skills",
    "list_acp_sessions_handler": "acp",
    "acp_spawn_handler": "acp",
    "terminate_acp_session_handler": "acp",
    "acp_session_message_handler": "acp",
    "acp_session_status_handler": "acp",
    "acp_session_pause_handler": "acp",
    "acp_session_resume_handler": "acp",
    "acp_session_step_handler": "acp",
    "acp_session_cancel_handler": "acp",
    "acp_session_tree_handler": "acp",
    "acp_execute_session_handler": "acp",
    "acp_execute_run_handler": "acp",
    "spawn_discovered_agent_handler": "discovery",
    "spawn_all_discovered_agents_handler": "discovery",
    "list_discovered_agents_handler": "discovery",
    "list_mcp_servers_handler": "mcp",
    "connect_mcp_server_handler": "mcp",
    "disconnect_mcp_server_handler": "mcp",
    "list_mcp_tools_handler": "mcp",
    "call_mcp_tool_handler": "mcp",
    "list_mcp_resources_handler": "mcp",
    "read_mcp_resource_handler": "mcp",
    "syscity_as_mcp_server_handler": "mcp",
    "mcp_default_timeout": "mcp",
    "list_settings_handler": "settings",
    "set_setting_handler": "settings",
    "get_setting_handler": "settings",
    "delete_setting_handler": "settings",
    "list_approvals_handler": "approvals",
    "get_approval_handler": "approvals",
    "approve_tool_handler": "approvals",
    "deny_tool_handler": "approvals",
    "list_cron_jobs_handler": "cron",
    "add_cron_job_handler": "cron",
    "remove_cron_job_handler": "cron",
    "enable_cron_job_handler": "cron",
    "disable_cron_job_handler": "cron",
    "trigger_cron_job_handler": "cron",
    "cron_job_logs_handler": "cron",
    "list_entities_handler": "admin",
    "list_threads_handler": "sessions",
    "list_turns_handler": "sessions",
    "undo_turn_handler": "sessions",
    "redo_turn_handler": "sessions",
    "get_config_handler": "config",
    "put_config_handler": "config",
    "validate_config_handler": "config",
    "list_pairing_pending_handler": "pairing",
    "list_pairing_authorized_handler": "pairing",
    "approve_pairing_handler": "pairing",
    "reject_pairing_handler": "pairing",
    "revoke_pairing_handler": "pairing",
    "add_allowlist_handler": "pairing",
    "list_gate_levels_handler": "pairing",
    "set_gate_level_handler": "pairing",
    "clear_gate_level_handler": "pairing",
    "get_mention_policy_handler": "mention",
    "set_mention_policy_handler": "mention",
    "list_mention_allowlist_handler": "mention",
    "add_mention_allowlist_handler": "mention",
    "remove_mention_allowlist_handler": "mention",
    "list_mention_blocklist_handler": "mention",
    "add_mention_blocklist_handler": "mention",
    "remove_mention_blocklist_handler": "mention",
    "list_audit_log_handler": "pairing",
}


def find_all_functions(lines):
    """Find all function definitions with their boundaries."""
    functions = []
    n = len(lines)
    i = 0

    while i < n:
        # Check for function signature
        line = lines[i]
        match = re.match(r'^(#[\[\]].*)?$', line.strip())
        if not match:
            match = re.match(r'^(pub\s+)?(async\s+)?fn\s+(\w+)', line)

        if not match or not re.match(r'^(pub\s+)?(async\s+)?fn\s+(\w+)', line):
            i += 1
            continue

        func_match = re.match(r'^(pub\s+)?(async\s+)?fn\s+(\w+)', line)
        if not func_match:
            i += 1
            continue

        func_name = func_match.group(3)

        # Find start (include preceding attributes and doc comments)
        start = i
        while start > 0:
            prev = lines[start - 1].rstrip('\n')
            stripped = prev.strip()
            if stripped.startswith('#[') or stripped.startswith('///') or stripped.startswith('//'):
                start -= 1
            elif stripped == '' and start > 1:
                # Check if there's a doc comment or attribute above the empty line
                above = lines[start - 2].strip()
                if above.startswith('#[') or above.startswith('///') or above.startswith('//'):
                    start -= 1
                else:
                    break
            else:
                break

        # Find opening brace
        brace_line = -1
        brace_col = -1
        for l in range(i, min(i + 20, n)):
            for c, ch in enumerate(lines[l]):
                if ch == '{':
                    brace_line = l
                    brace_col = c
                    break
            if brace_line >= 0:
                break

        if brace_line < 0:
            i += 1
            continue

        # Match braces with string/comment handling
        depth = 1
        end_line = brace_line
        end_col = brace_col

        in_block_comment = False

        for l in range(brace_line, n):
            line_text = lines[l]
            start_c = brace_col + 1 if l == brace_line else 0

            c = start_c
            while c < len(line_text):
                ch = line_text[c]

                if in_block_comment:
                    if ch == '*' and c + 1 < len(line_text) and line_text[c + 1] == '/':
                        in_block_comment = False
                        c += 2
                    else:
                        c += 1
                    continue

                # Check for string literal
                if ch == '"':
                    c += 1
                    while c < len(line_text):
                        if line_text[c] == '\\':
                            c += 2
                        elif line_text[c] == '"':
                            c += 1
                            break
                        else:
                            c += 1
                    continue

                # Check for char literal
                if ch == "'":
                    c += 1
                    while c < len(line_text):
                        if line_text[c] == '\\':
                            c += 2
                        elif line_text[c] == "'":
                            c += 1
                            break
                        else:
                            c += 1
                    continue

                # Check for raw string
                raw_match = re.match(r'r(#*)"', line_text[c:])
                if raw_match:
                    end_marker = '"' + raw_match.group(1)
                    c += len(raw_match.group(0))
                    while c < len(line_text):
                        if line_text[c:c + len(end_marker)] == end_marker:
                            c += len(end_marker)
                            break
                        c += 1
                    continue

                # Check for line comment
                if ch == '/' and c + 1 < len(line_text) and line_text[c + 1] == '/':
                    break

                # Check for block comment start
                if ch == '/' and c + 1 < len(line_text) and line_text[c + 1] == '*':
                    in_block_comment = True
                    c += 2
                    continue

                if ch == '{':
                    depth += 1
                elif ch == '}':
                    depth -= 1
                    if depth == 0:
                        end_line = l
                        end_col = c
                        break

                c += 1

            if depth == 0:
                break

        if depth == 0:
            functions.append({
                'name': func_name,
                'start': start,
                'end': end_line,
                'text': ''.join(lines[start:end_line + 1])
            })
            i = end_line + 1
        else:
            print(f"WARNING: Could not find end of function {func_name} starting at line {i + 1}")
            i += 1

    return functions


def main():
    with open(MOD_RS, 'r') as f:
        lines = f.readlines()

    print(f"Read {len(lines)} lines from mod.rs")

    # Find all functions
    functions = find_all_functions(lines)
    print(f"Found {len(functions)} functions")

    # Separate handlers from core functions
    handlers = {}
    core_lines_to_keep = set()

    for func in functions:
        name = func['name']
        if name in HANDLER_MAP:
            module = HANDLER_MAP[name]
            if module not in handlers:
                handlers[module] = []
            handlers[module].append(func)
            print(f"  Handler: {name} -> {module} (lines {func['start'] + 1}-{func['end'] + 1})")
        else:
            # Core function - keep in mod.rs
            for l in range(func['start'], func['end'] + 1):
                core_lines_to_keep.add(l)
            print(f"  Core: {name} (lines {func['start'] + 1}-{func['end'] + 1})")

    # Lines that are not part of any function are also kept
    all_func_lines = set()
    for func in functions:
        for l in range(func['start'], func['end'] + 1):
            all_func_lines.add(l)

    for l in range(len(lines)):
        if l not in all_func_lines:
            core_lines_to_keep.add(l)

    # Write new mod.rs (only core lines)
    new_mod_lines = []
    for l in range(len(lines)):
        if l in core_lines_to_keep:
            new_mod_lines.append(lines[l])

    # Write handler files
    os.makedirs(HANDLERS_DIR, exist_ok=True)

    IMPORTS = """#![allow(unused_imports)]

use axum::{
    extract::{Path, Query, State, WebSocketUpgrade},
    http::{header, StatusCode},
    response::{Html, IntoResponse, Json},
    routing::{delete, get, post},
    Router,
};
use futures_util::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;
use tokio::io::{AsyncBufReadExt, AsyncSeekExt};
use tokio::net::TcpListener;
use tokio::sync::{broadcast, mpsc, RwLock};
use tower_http::cors::CorsLayer;
use tracing::{debug, error, info, warn};

use crate::acp::AcpControlPlane;
use crate::agent::{Agent, AgentConfig};
use crate::canvas::{CanvasEvent, CanvasManager};
use crate::channels::{Channel, ChannelExtension, ChannelType};
use crate::config::hot_reload::{ConfigFileType, HotReloadManager};
use crate::inbound::*;
use crate::memory::vector::{
    ApiEmbeddingProvider, CachedEmbeddingProvider, EmbeddingConfig, LocalGgufEmbeddingProvider,
    MemoryVectorStore, VectorMemoryService,
};
use crate::model_router::ModelRouter;
use crate::plugins::PluginManager;
use crate::security::pairing::DmPolicy;
use crate::tools::approval::{ApprovalDecision, ApprovalFilter, ApprovalQueue};
use crate::tools::mcp::{McpManager, McpSettings, McpToolWrapper};
use crate::tools::ToolRegistry;
use crate::gateway::GatewayState;

"""

    for module, funcs in sorted(handlers.items()):
        filepath = HANDLERS_DIR / f"{module}.rs"
        with open(filepath, 'w') as f:
            f.write(IMPORTS)
            for func in funcs:
                f.write(func['text'])
                f.write('\n')
        print(f"Wrote {len(funcs)} functions to {filepath}")

    # Create handlers/mod.rs
    mod_rs_content = ""
    for module in sorted(handlers.keys()):
        mod_rs_content += f"pub mod {module};\n"
    mod_rs_content += "\n"
    for module in sorted(handlers.keys()):
        mod_rs_content += f"pub use {module}::*;\n"

    with open(HANDLERS_DIR / "mod.rs", 'w') as f:
        f.write(mod_rs_content)
    print(f"Wrote handlers/mod.rs with {len(handlers)} modules")

    # Write new mod.rs
    with open(MOD_RS, 'w') as f:
        # Find where to insert `pub mod handlers;` - after existing `pub mod` declarations
        insert_pos = None
        for i, line in enumerate(new_mod_lines):
            if line.strip() == 'pub mod ws;':
                insert_pos = i + 1
                break

        if insert_pos is not None:
            new_mod_lines.insert(insert_pos, 'pub mod handlers;\n')
            new_mod_lines.insert(insert_pos + 1, 'use handlers::*;\n')

        f.writelines(new_mod_lines)

    print(f"\nNew mod.rs: {len(new_mod_lines)} lines (was {len(lines)})")
    print("Done!")


if __name__ == '__main__':
    main()
