//! Gateway management tool
//!
//! Restart, inspect config, apply config patches, or update the gateway.
//!
//! Security: config mutations are restricted to an allowlist of safe paths.
//! The model/agent is not a trusted principal; this tool fails closed.

use std::collections::HashSet;
use std::sync::Arc;

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::Value;
use tracing::{info, warn};

use super::{Tool, ToolContext, ToolExecutionResult};
use crate::gateway::GatewayState;
use crate::tools::sdk::ToolCapabilities;

/// Gateway management tool — restart, inspect, and mutate gateway config.
pub struct GatewayTool {
    state: Arc<GatewayState>,
}

impl GatewayTool {
    pub fn new(state: Arc<GatewayState>) -> Self {
        Self { state }
    }
}

// ── Config path security ─────────────────────────────────────────────────────

/// Paths the gateway tool is allowed to mutate.  Fails closed — anything not
/// listed here is rejected.  Patterns use `*` for single-segment wildcards.
const ALLOWED_CONFIG_PATHS: &[&str] = &[
    // default_agent subfields — map to AgentConfig struct fields
    "default_agent.system_prompt",
    "default_agent.max_context_tokens",
    "default_agent.max_concurrent_tools",
    "default_agent.temperature",
    "default_agent.max_tokens",
    "default_agent.skills_prompt",
    "default_agent.max_turns",
    "default_agent.compaction_model",
    "default_agent.workspace_dir",
    "default_agent.workspace_only",
    "default_agent.reflection_config",
    // Channel require_mention patterns — match ChannelConfig.require_mention
    "channels.*.require_mention",
    "channels.*.*.require_mention",
    "channels.*.*.*.require_mention",
    "channels.*.*.*.*.require_mention",
    "channels.*.*.*.*.*.require_mention",
];

/// Protected paths that can never be changed through the gateway tool.
const BLOCKED_CONFIG_PATHS: &[&str] = &[
    "tools.exec.ask",
    "tools.exec.security",
    "tools.bash.ask",
    "tools.bash.security",
];

fn is_allowed_config_path(path: &str) -> bool {
    if BLOCKED_CONFIG_PATHS.iter().any(|p| path.starts_with(p)) {
        return false;
    }
    let segments: Vec<&str> = path.split('.').collect();
    ALLOWED_CONFIG_PATHS.iter().any(|pattern| {
        let pat: Vec<&str> = pattern.split('.').collect();
        if pat.len() > segments.len() {
            return false;
        }
        pat.iter()
            .zip(segments.iter())
            .all(|(p, s)| *p == "*" || p == s)
    })
}

fn collect_changed_paths(current: &Value, next: &Value, base: &str, out: &mut HashSet<String>) {
    if current == next {
        return;
    }
    match (current, next) {
        (Value::Array(ca), Value::Array(na)) => {
            let c_has_id = ca
                .iter()
                .all(|v| v.as_object().map(|o| o.contains_key("id")).unwrap_or(false));
            let n_has_id = na
                .iter()
                .all(|v| v.as_object().map(|o| o.contains_key("id")).unwrap_or(false));
            if c_has_id && n_has_id && !ca.is_empty() && !na.is_empty() {
                let c_ids: std::collections::HashMap<String, &Value> = ca
                    .iter()
                    .filter_map(|v| {
                        v.get("id")
                            .and_then(|id| id.as_str())
                            .map(|s| (s.to_string(), v))
                    })
                    .collect();
                let n_ids: std::collections::HashMap<String, &Value> = na
                    .iter()
                    .filter_map(|v| {
                        v.get("id")
                            .and_then(|id| id.as_str())
                            .map(|s| (s.to_string(), v))
                    })
                    .collect();
                let all_ids: HashSet<String> = c_ids.keys().chain(n_ids.keys()).cloned().collect();
                for id in all_ids {
                    collect_changed_paths(
                        c_ids.get(&id).copied().unwrap_or(&Value::Null),
                        n_ids.get(&id).copied().unwrap_or(&Value::Null),
                        &format!("{}[]", base),
                        out,
                    );
                }
            } else {
                out.insert(base.to_string());
            }
        }
        (Value::Object(co), Value::Object(no)) => {
            let keys: HashSet<String> = co.keys().chain(no.keys()).cloned().collect();
            for k in keys {
                let next = if base.is_empty() {
                    k.clone()
                } else {
                    format!("{}.{}", base, k)
                };
                collect_changed_paths(
                    co.get(&k).unwrap_or(&Value::Null),
                    no.get(&k).unwrap_or(&Value::Null),
                    &next,
                    out,
                );
            }
        }
        _ => {
            out.insert(base.to_string());
        }
    }
}

fn assert_config_mutation_allowed(current: &Value, raw: &str, action: &str) -> Result<(), String> {
    let parsed: Value = serde_json::from_str(raw).map_err(|e| format!("Invalid JSON: {}", e))?;
    let next = if action == "config.apply" {
        parsed
    } else {
        merge_json(current, &parsed)
    };
    let mut changed = HashSet::new();
    collect_changed_paths(current, &next, "", &mut changed);
    let disallowed: Vec<String> = changed
        .iter()
        .filter(|p| !is_allowed_config_path(p))
        .cloned()
        .collect();
    if !disallowed.is_empty() {
        return Err(format!(
            "gateway {} cannot change protected config paths: {}",
            action,
            disallowed.join(", ")
        ));
    }
    Ok(())
}

fn merge_json(base: &Value, patch: &Value) -> Value {
    match (base, patch) {
        (Value::Object(bo), Value::Object(po)) => {
            let mut merged = bo.clone();
            for (k, v) in po {
                if v.is_null() {
                    merged.remove(k);
                } else {
                    let existing = merged.get(k).unwrap_or(&Value::Null);
                    merged.insert(k.clone(), merge_json(existing, v));
                }
            }
            Value::Object(merged)
        }
        _ => patch.clone(),
    }
}

fn resolve_config_path<'a>(value: &'a Value, path: &str) -> Option<&'a Value> {
    let mut current = value;
    for segment in path.split('.') {
        match current {
            Value::Object(obj) => {
                current = obj.get(segment)?;
            }
            Value::Array(arr) => {
                if let Ok(idx) = segment.parse::<usize>() {
                    current = arr.get(idx)?;
                } else {
                    return None;
                }
            }
            _ => return None,
        }
    }
    Some(current)
}

fn compute_config_hash(config: &Value) -> String {
    crate::gateway::config_json_hash(config)
}

/// Whether the final segment of a config dot-path is a secret container, so the
/// resolved value is a container payload whose string leaves are all secret
/// (e.g. `search.keys`, `channels.tg.credentials`). `env` maps are NOT treated
/// this way — they mix secrets and non-secrets and need per-key masking.
fn path_is_container_payload(path: &str) -> bool {
    path.split('.')
        .next_back()
        .is_some_and(crate::secrets::is_secret_container_key)
}

/// Whether the final segment of a config dot-path names a secret value, so a
/// resolved scalar leaf must be masked (e.g. `search.api_key`).
fn path_is_secret_leaf(path: &str) -> bool {
    path.split('.')
        .next_back()
        .is_some_and(crate::secrets::is_secret_key)
}

// ── GatewayArgs ──────────────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
struct GatewayArgs {
    #[serde(default)]
    action: String,
    #[serde(default)]
    path: Option<String>,
    #[serde(default)]
    raw: Option<String>,
    #[serde(default)]
    base_hash: Option<String>,
    #[serde(default)]
    delay_ms: Option<u64>,
    #[serde(default)]
    reason: Option<String>,
    #[serde(default)]
    note: Option<String>,
}

// ── GatewayTool impl ─────────────────────────────────────────────────────────

#[async_trait]
impl Tool for GatewayTool {
    fn name(&self) -> &str {
        "gateway"
    }

    fn description(&self) -> &str {
        "Restart, inspect config, apply config patches, or update the gateway. Actions: restart, \
         config.get, config.schema.lookup, config.apply, config.patch, update.run. Use \
         config.schema.lookup with a targeted dot path before config edits. Use config.patch for \
         safe partial updates. Use config.apply only for full replacement. Config writes \
         hot-reload when possible."
    }

    fn parameters_schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "action": {
                    "type": "string",
                    "enum": ["restart", "config.get", "config.schema.lookup", "config.apply", "config.patch", "update.run"],
                    "description": "Action to perform"
                },
                "path": {
                    "type": "string",
                    "description": "Dot path for config.schema.lookup (e.g. 'agents.defaults.model')"
                },
                "raw": {
                    "type": "string",
                    "description": "JSON string for config.apply or config.patch"
                },
                "base_hash": {
                    "type": "string",
                    "description": "Hash from previous config.get for optimistic locking"
                },
                "delay_ms": {
                    "type": "integer",
                    "description": "Delay in milliseconds before restart"
                },
                "reason": {
                    "type": "string",
                    "description": "Reason for restart"
                },
                "note": {
                    "type": "string",
                    "description": "Human-readable note to deliver after restart/update"
                }
            },
            "required": ["action"]
        })
    }

    fn capabilities(&self) -> ToolCapabilities {
        ToolCapabilities {
            requires_approval: true,
            risk_level: crate::tools::approval::RiskLevel::High,
            categories: vec!["network".to_string(), "gateway".to_string()],
            idempotent: false,
            compensation: None,
            ..Default::default()
        }
    }

    fn is_available(&self, context: &ToolContext) -> bool {
        !context.sandboxed() || !context.allowed_commands().is_empty()
    }

    async fn execute(
        &self,
        args: Value,
        _context: &ToolContext,
    ) -> crate::Result<ToolExecutionResult> {
        let start = std::time::Instant::now();
        let args: GatewayArgs = match serde_json::from_value(args) {
            Ok(a) => a,
            Err(e) => {
                return Ok(ToolExecutionResult {
                    success: false,
                    output: String::new(),
                    error: Some(format!("Invalid arguments: {}", e)),
                    data: None,
                    execution_time: start.elapsed(),
                });
            }
        };

        match args.action.as_str() {
            "restart" => {
                let delay = args
                    .delay_ms
                    .map(|d| format!(" in {}ms", d))
                    .unwrap_or_default();
                let reason = args.reason.as_deref().unwrap_or("gateway tool restart");
                let note = args.note.as_deref().unwrap_or("Gateway restart scheduled");
                info!("gateway tool: restart requested{} (reason={})", delay, reason);
                self.state
                    .auth
                    .audit_log
                    .log(
                        crate::security::runtime_audit::AuditEventType::ConfigChange,
                        "admin",
                        "gateway",
                        true,
                        format!("Gateway restart scheduled: {}", reason),
                        Some(serde_json::json!({ "reason": reason, "delay_ms": args.delay_ms })),
                    )
                    .await;
                Ok(ToolExecutionResult {
                    success: true,
                    output: format!("{}.{}", note, delay),
                    error: None,
                    data: Some(serde_json::json!({
                        "scheduled": true,
                        "action": "restart",
                        "delay_ms": args.delay_ms,
                        "reason": reason,
                        "note": note,
                    })),
                    execution_time: start.elapsed(),
                })
            }

            "config.get" => {
                let config = {
                    let cfg = self.state.config.read().await;
                    match serde_json::to_value(&*cfg) {
                        Ok(v) => v,
                        Err(e) => {
                            return Ok(ToolExecutionResult {
                                success: false,
                                output: String::new(),
                                error: Some(format!("Config serialization failed: {}", e)),
                                data: None,
                                execution_time: start.elapsed(),
                            });
                        }
                    }
                };
                let hash = compute_config_hash(&config);
                // The hash is computed from the unmasked config (so `config.apply`
                // base_hash CAS stays stable), but the returned snapshot masks
                // secrets so they never reach the model.
                let masked = crate::secrets::mask_json_value(&config);
                Ok(ToolExecutionResult {
                    success: true,
                    output: format!("Current gateway config (hash: {})", &hash[..16]),
                    error: None,
                    data: Some(serde_json::json!({
                        "ok": true,
                        "config": masked,
                        "hash": hash,
                    })),
                    execution_time: start.elapsed(),
                })
            }

            "config.schema.lookup" => {
                let path = match args.path {
                    Some(p) => p,
                    None => {
                        return Ok(ToolExecutionResult {
                            success: false,
                            output: String::new(),
                            error: Some("path required for config.schema.lookup".to_string()),
                            data: None,
                            execution_time: start.elapsed(),
                        });
                    }
                };
                let config = {
                    let cfg = self.state.config.read().await;
                    match serde_json::to_value(&*cfg) {
                        Ok(v) => v,
                        Err(e) => {
                            return Ok(ToolExecutionResult {
                                success: false,
                                output: String::new(),
                                error: Some(format!("Config serialization failed: {}", e)),
                                data: None,
                                execution_time: start.elapsed(),
                            });
                        }
                    }
                };
                match resolve_config_path(&config, &path) {
                    Some(value) => {
                        // Secret values must not reach the model. The walker is
                        // always applied (it is identity for innocent config, so
                        // ancestor lookups like `mcp.servers.s` are covered
                        // too); container payloads and secret scalar leaves get
                        // stricter handling the walker cannot express.
                        let display = if path_is_container_payload(&path) {
                            crate::secrets::mask_secret_container_payload(value)
                        } else if path_is_secret_leaf(&path) {
                            match value {
                                Value::String(s) => Value::String(crate::secrets::mask_secret(s)),
                                other => crate::secrets::mask_json_value(other),
                            }
                        } else {
                            crate::secrets::mask_json_value(value)
                        };
                        Ok(ToolExecutionResult {
                            success: true,
                            output: format!("{} = {}", path, display),
                            error: None,
                            data: Some(serde_json::json!({
                                "ok": true,
                                "path": path,
                                "value": display,
                            })),
                            execution_time: start.elapsed(),
                        })
                    }
                    None => Ok(ToolExecutionResult {
                        success: false,
                        output: String::new(),
                        error: Some(format!("Path '{}' not found in config", path)),
                        data: None,
                        execution_time: start.elapsed(),
                    }),
                }
            }

            "config.apply" | "config.patch" => {
                let raw = match args.raw {
                    Some(r) => r,
                    None => {
                        return Ok(ToolExecutionResult {
                            success: false,
                            output: String::new(),
                            error: Some(format!("raw required for {}", args.action)),
                            data: None,
                            execution_time: start.elapsed(),
                        });
                    }
                };

                let (current_config_json, current_hash) = {
                    let cfg = self.state.config.read().await;
                    let json = match serde_json::to_value(&*cfg) {
                        Ok(v) => v,
                        Err(e) => {
                            return Ok(ToolExecutionResult {
                                success: false,
                                output: String::new(),
                                error: Some(format!("Config serialization failed: {}", e)),
                                data: None,
                                execution_time: start.elapsed(),
                            });
                        }
                    };
                    let hash = compute_config_hash(&json);
                    (json, hash)
                };

                if let Some(expected) = args.base_hash {
                    if expected != current_hash {
                        return Ok(ToolExecutionResult {
                            success: false,
                            output: String::new(),
                            error: Some(format!(
                                "Config hash mismatch: expected {} but current is {}. Fetch \
                                 config.get first.",
                                &expected[..expected.len().min(16)],
                                &current_hash[..16]
                            )),
                            data: Some(serde_json::json!({
                                "current_hash": current_hash,
                                "expected_hash": expected,
                            })),
                            execution_time: start.elapsed(),
                        });
                    }
                }

                if let Err(e) =
                    assert_config_mutation_allowed(&current_config_json, &raw, &args.action)
                {
                    return Ok(ToolExecutionResult {
                        success: false,
                        output: String::new(),
                        error: Some(e),
                        data: None,
                        execution_time: start.elapsed(),
                    });
                }

                let parsed: Value = match serde_json::from_str(&raw) {
                    Ok(v) => v,
                    Err(e) => {
                        return Ok(ToolExecutionResult {
                            success: false,
                            output: String::new(),
                            error: Some(format!("Invalid JSON: {}", e)),
                            data: None,
                            execution_time: start.elapsed(),
                        });
                    }
                };

                let next_config_json = if args.action == "config.patch" {
                    merge_json(&current_config_json, &parsed)
                } else {
                    parsed
                };

                let new_config: crate::gateway::GatewayConfig =
                    match serde_json::from_value(next_config_json) {
                        Ok(c) => c,
                        Err(e) => {
                            return Ok(ToolExecutionResult {
                                success: false,
                                output: String::new(),
                                error: Some(format!("Config validation failed: {}", e)),
                                data: None,
                                execution_time: start.elapsed(),
                            });
                        }
                    };

                let config_path = match self.state.config_path.as_ref() {
                    Some(p) => p.clone(),
                    None => {
                        return Ok(ToolExecutionResult {
                            success: false,
                            output: String::new(),
                            error: Some(
                                "No config file path configured — cannot persist changes"
                                    .to_string(),
                            ),
                            data: None,
                            execution_time: start.elapsed(),
                        });
                    }
                };

                let toml_str = match toml::to_string_pretty(&new_config) {
                    Ok(s) => s,
                    Err(e) => {
                        return Ok(ToolExecutionResult {
                            success: false,
                            output: String::new(),
                            error: Some(format!("TOML serialization failed: {}", e)),
                            data: None,
                            execution_time: start.elapsed(),
                        });
                    }
                };

                if let Err(e) = tokio::fs::write(&config_path, toml_str).await {
                    return Ok(ToolExecutionResult {
                        success: false,
                        output: String::new(),
                        error: Some(format!("Failed to write config file: {}", e)),
                        data: None,
                        execution_time: start.elapsed(),
                    });
                }

                {
                    let mut cfg = self.state.config.write().await;
                    *cfg = Arc::new(new_config);
                }

                let new_hash = {
                    let cfg = self.state.config.read().await;
                    let json = serde_json::to_value(&*cfg).unwrap_or_default();
                    compute_config_hash(&json)
                };

                info!("gateway tool: {} persisted to {:?}", args.action, config_path);

                self.state.auth.audit_log
                    .log(
                        crate::security::runtime_audit::AuditEventType::ConfigChange,
                        "admin",
                        "gateway",
                        true,
                        format!("Gateway {} applied", args.action),
                        Some(serde_json::json!({ "action": args.action, "path": config_path.to_string_lossy() })),
                    )
                    .await;

                Ok(ToolExecutionResult {
                    success: true,
                    output: format!(
                        "{} applied and persisted. New hash: {}.",
                        args.action,
                        &new_hash[..16]
                    ),
                    error: None,
                    data: Some(serde_json::json!({
                        "ok": true,
                        "action": args.action,
                        "new_hash": new_hash,
                        "path": config_path.to_string_lossy(),
                    })),
                    execution_time: start.elapsed(),
                })
            }

            "update.run" => {
                let note = args.note.as_deref().unwrap_or("Self-update requested");
                warn!("gateway tool: update.run is not yet implemented in Syscity");
                Ok(ToolExecutionResult {
                    success: true,
                    output: format!("{}. Self-update is not yet implemented in Syscity.", note),
                    error: None,
                    data: Some(serde_json::json!({
                        "ok": true,
                        "action": "update.run",
                        "implemented": false,
                        "note": note,
                    })),
                    execution_time: start.elapsed(),
                })
            }

            _ => Ok(ToolExecutionResult {
                success: false,
                output: String::new(),
                error: Some(format!("Unknown gateway action: {}", args.action)),
                data: None,
                execution_time: start.elapsed(),
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::time::Duration;

    use crate::channels::ChannelType;
    use crate::gateway::state_tests::make_test_state;
    use crate::gateway::GatewayConfig;
    use crate::mcp::McpServerConfig;
    use crate::model_router::{ProviderConfig, ProviderType};

    use super::*;

    fn leaked_config() -> GatewayConfig {
        let mut cfg = GatewayConfig::default();
        cfg.providers.insert(
            "openai".into(),
            ProviderConfig {
                provider_type: ProviderType::OpenAi,
                models: vec!["gpt-4o".into()],
                default_model: "gpt-4o".into(),
                api_key: "sk-plaintext-12345".to_string().into(),
                api_keys: vec![],
                auth_profile: None,
                oauth: None,
                base_url: None,
                timeout: Duration::from_secs(30),
                max_retries: 3,
                retry_delay_ms: 1000,
            },
        );
        cfg.search.api_key = "tvly-plaintext-1234".to_string();
        cfg.search
            .keys
            .insert("tavily".to_string(), "tvly-secret-99999".to_string());
        cfg.mcp.servers.insert(
            "s".into(),
            McpServerConfig {
                env: HashMap::from([
                    ("ANTHROPIC_API_KEY".to_string(), "sk-env-55555".to_string()),
                    ("HOST".to_string(), "localhost".to_string()),
                ]),
                ..McpServerConfig::default()
            },
        );
        let mut tg = crate::gateway::ChannelConfig::new(ChannelType::Telegram);
        tg.credentials
            .insert("bot_token".to_string(), "123456:ABC".to_string());
        cfg.channels.insert("tg".into(), tg);
        cfg
    }

    #[test]
    fn test_gateway_args_defaults() {
        let args: GatewayArgs = serde_json::from_value(serde_json::json!({})).unwrap();
        assert_eq!(args.action, "");
        assert!(args.path.is_none());
    }

    #[test]
    fn test_gateway_args_action() {
        let args: GatewayArgs = serde_json::from_value(serde_json::json!({
            "action": "restart"
        }))
        .unwrap();
        assert_eq!(args.action, "restart");
    }

    #[tokio::test]
    async fn config_get_masks_secrets_and_hash_matches_revision() {
        let state = Arc::new(make_test_state(leaked_config()).await);
        let tool = GatewayTool::new(state.clone());
        let ctx = ToolContext::new("user", "conv1");

        let res = tool
            .execute(serde_json::json!({"action": "config.get"}), &ctx)
            .await
            .expect("execute");
        assert!(res.success, "config.get should succeed");
        let data = res.data.expect("data");
        let config = &data["config"];

        let api_key = config["providers"]["openai"]["api_key"]
            .as_str()
            .expect("provider api_key");
        assert!(!api_key.contains("sk-plaintext"), "provider api_key must be masked");
        assert!(api_key.contains("••••"));

        let search_key = config["search"]["api_key"]
            .as_str()
            .expect("search api_key");
        assert!(!search_key.contains("tvly-plaintext"), "search api_key must be masked");
        assert!(search_key.contains("••••"));

        let keys = &config["search"]["keys"]["tavily"];
        assert!(!keys.to_string().contains("tvly-secret"), "keys container must be masked");

        let bot_token = config["channels"]["tg"]["credentials"]["bot_token"]
            .as_str()
            .expect("credentials bot_token");
        assert!(!bot_token.contains("123456"), "credentials container must be masked");

        // Non-secret fields stay readable.
        assert_eq!(config["model_provider"], "anthropic");

        // The hash (used for base_hash CAS) must match the WS revision so the
        // two surfaces agree on "current config".
        let cfg_guard = state.config.read().await;
        let ws_rev = crate::gateway::config_revision(&cfg_guard);
        assert_eq!(data["hash"].as_str().expect("hash"), ws_rev);
    }

    #[tokio::test]
    async fn schema_lookup_masks_sensitive_and_passes_innocent() {
        let state = Arc::new(make_test_state(leaked_config()).await);
        let tool = GatewayTool::new(state.clone());
        let ctx = ToolContext::new("user", "conv1");

        async fn lookup(tool: &GatewayTool, ctx: &ToolContext, path: &str) -> serde_json::Value {
            let res = tool
                .execute(serde_json::json!({"action": "config.schema.lookup", "path": path}), ctx)
                .await
                .expect("execute");
            assert!(res.success, "lookup {path} should succeed");
            res.data.expect("data")["value"].clone()
        }

        // Innocent values pass through untouched.
        assert_eq!(lookup(&tool, &ctx, "model_provider").await, "anthropic");
        assert_eq!(lookup(&tool, &ctx, "default_agent.max_tokens").await, serde_json::json!(2048));

        // Secret scalar leaf is masked.
        let api_key = lookup(&tool, &ctx, "search.api_key").await;
        let s = api_key.as_str().expect("string");
        assert!(!s.contains("tvly-plaintext"));
        assert!(s.contains("••••"));

        // Container payload masks every leaf.
        let keys = lookup(&tool, &ctx, "search.keys").await;
        assert!(!keys["tavily"].to_string().contains("tvly-secret"));
        assert!(keys["tavily"].as_str().unwrap().contains("••••"));

        let creds = lookup(&tool, &ctx, "channels.tg.credentials").await;
        assert!(!creds["bot_token"].to_string().contains("123456"));
        assert!(creds["bot_token"].as_str().unwrap().contains("••••"));

        // env map is masked per key: secret keyed values mask, HOST stays.
        let env = lookup(&tool, &ctx, "mcp.servers.s.env").await;
        assert!(!env["ANTHROPIC_API_KEY"].to_string().contains("sk-env"));
        assert_eq!(env["HOST"], "localhost");

        // Ancestor lookup is covered by the walker too.
        let server = lookup(&tool, &ctx, "mcp.servers.s").await;
        assert!(!server["env"]["ANTHROPIC_API_KEY"]
            .to_string()
            .contains("sk-env"));
        assert_eq!(server["env"]["HOST"], "localhost");
    }
}
