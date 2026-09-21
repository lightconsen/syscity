//! Nodes Tool — Tailscale Device Discovery and Control
//!
//! tool for discovering and controlling paired nodes
//! via Tailscale. Supports status queries, device info, camera/screen
//! capture, notifications, and remote invocation.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tracing::warn;

use super::{Tool, ToolContext, ToolExecutionResult};
use crate::tools::process_runner::ProcessRequest;
use crate::tools::sdk::ToolCapabilities;

/// Nodes discovery and control tool
pub struct NodesTool;

impl NodesTool {
    pub fn new() -> Self {
        Self
    }
}

impl Default for NodesTool {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Debug, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
enum NodesAction {
    Status,
    Describe {
        node_id: String,
    },
    List,
    Ping {
        node_id: String,
    },
    CameraSnap {
        node_id: String,
    },
    CameraList {
        node_id: String,
    },
    ScreenRecord {
        node_id: String,
        #[serde(default)]
        duration: Option<u64>,
    },
    LocationGet {
        node_id: String,
    },
    DeviceStatus {
        node_id: String,
    },
    DeviceInfo {
        node_id: String,
    },
}

#[derive(Debug, Serialize)]
struct TailscaleNode {
    id: String,
    name: String,
    ipv4: String,
    ipv6: String,
    os: String,
    online: bool,
    last_seen: String,
    tags: Vec<String>,
}

/// Fetch Tailscale node list via tailscale CLI API
async fn fetch_tailscale_nodes() -> Vec<TailscaleNode> {
    // Try tailscale status --json
    let output = match crate::tools::process_runner::run(&ProcessRequest::argv(&[
        "tailscale",
        "status",
        "--json",
    ]))
    .await
    {
        Ok(o) if o.success() => o.stdout,
        _ => return Vec::new(),
    };

    let json: Value = match serde_json::from_slice(&output) {
        Ok(j) => j,
        Err(_) => return Vec::new(),
    };

    let mut nodes = Vec::new();

    if let Some(peer_map) = json.get("Peer") {
        if let Some(peers) = peer_map.as_object() {
            for (id, peer) in peers {
                let node = TailscaleNode {
                    id: id.clone(),
                    name: peer
                        .get("HostName")
                        .and_then(|v| v.as_str())
                        .unwrap_or(id)
                        .to_string(),
                    ipv4: peer
                        .get("TailscaleIPs")
                        .and_then(|v| v.as_array())
                        .and_then(|arr| arr.first())
                        .and_then(|ip| ip.as_str())
                        .unwrap_or("")
                        .to_string(),
                    ipv6: peer
                        .get("TailscaleIPs")
                        .and_then(|v| v.as_array())
                        .and_then(|arr| arr.get(1))
                        .and_then(|ip| ip.as_str())
                        .unwrap_or("")
                        .to_string(),
                    os: peer
                        .get("OS")
                        .and_then(|v| v.as_str())
                        .unwrap_or("unknown")
                        .to_string(),
                    online: peer
                        .get("Online")
                        .and_then(|v| v.as_bool())
                        .unwrap_or(false),
                    last_seen: peer
                        .get("LastSeen")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string(),
                    tags: peer
                        .get("Tags")
                        .and_then(|v| v.as_array())
                        .map(|arr| {
                            arr.iter()
                                .filter_map(|t| t.as_str().map(String::from))
                                .collect()
                        })
                        .unwrap_or_default(),
                };
                nodes.push(node);
            }
        }
    }

    nodes
}

#[async_trait]
impl Tool for NodesTool {
    fn name(&self) -> &str {
        "nodes"
    }

    fn description(&self) -> &str {
        "Discover and control paired Tailscale nodes. Query status, ping devices, capture \
         camera/screenshots, get location, and check device health. Requires Tailscale to be \
         installed and running."
    }

    fn parameters_schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "action": {
                    "type": "string",
                    "enum": ["status", "describe", "list", "ping", "camera_snap", "camera_list", "screen_record", "location_get", "device_status", "device_info"],
                    "description": "Node action"
                },
                "node_id": {
                    "type": "string",
                    "description": "Node ID or hostname (required for most actions)"
                },
                "duration": {
                    "type": "integer",
                    "description": "Duration in seconds for screen_record",
                    "default": 5
                }
            },
            "required": ["action"]
        })
    }

    fn capabilities(&self) -> ToolCapabilities {
        ToolCapabilities {
            read_only: true,
            requires_approval: false,
            risk_level: crate::tools::approval::RiskLevel::Medium,
            categories: vec!["system".to_string(), "network".to_string()],
            ..Default::default()
        }
    }

    fn is_available(&self, _context: &ToolContext) -> bool {
        // Drives the `tailscale` CLI; no mobile equivalent (§4.4).
        !cfg!(mobile_os)
    }

    async fn execute(
        &self,
        args: Value,
        _context: &ToolContext,
    ) -> crate::Result<ToolExecutionResult> {
        let start = std::time::Instant::now();
        let action: NodesAction = match serde_json::from_value(args) {
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

        // Check if tailscale is available
        let tailscale_available =
            crate::tools::process_runner::run(&ProcessRequest::argv(&["tailscale", "version"]))
                .await
                .map(|o| o.success())
                .unwrap_or(false);

        if !tailscale_available {
            return Ok(ToolExecutionResult {
                success: false,
                output: String::new(),
                error: Some(
                    "Tailscale is not installed or not in PATH. Install from https://tailscale.com"
                        .to_string(),
                ),
                data: None,
                execution_time: start.elapsed(),
            });
        }

        match action {
            NodesAction::Status => {
                let nodes = fetch_tailscale_nodes().await;
                let online = nodes.iter().filter(|n| n.online).count();

                Ok(ToolExecutionResult {
                    success: true,
                    output: format!("{} node(s), {} online", nodes.len(), online),
                    error: None,
                    data: Some(serde_json::json!({
                        "total": nodes.len(),
                        "online": online,
                        "nodes": nodes,
                    })),
                    execution_time: start.elapsed(),
                })
            }
            NodesAction::Describe { node_id } => {
                let nodes = fetch_tailscale_nodes().await;
                match nodes
                    .into_iter()
                    .find(|n| n.id == node_id || n.name == node_id)
                {
                    Some(node) => Ok(ToolExecutionResult {
                        success: true,
                        output: format!(
                            "Node {}: {} ({}, {})",
                            node.name,
                            if node.online { "online" } else { "offline" },
                            node.ipv4,
                            node.os
                        ),
                        error: None,
                        data: Some(serde_json::to_value(node).unwrap_or_default()),
                        execution_time: start.elapsed(),
                    }),
                    None => Ok(ToolExecutionResult {
                        success: false,
                        output: String::new(),
                        error: Some(format!("Node {} not found", node_id)),
                        data: None,
                        execution_time: start.elapsed(),
                    }),
                }
            }
            NodesAction::List => {
                let nodes = fetch_tailscale_nodes().await;
                let summary: Vec<_> = nodes
                    .iter()
                    .map(|n| {
                        serde_json::json!({
                            "id": n.id,
                            "name": n.name,
                            "online": n.online,
                            "os": n.os,
                            "ipv4": n.ipv4,
                        })
                    })
                    .collect();

                Ok(ToolExecutionResult {
                    success: true,
                    output: format!("{} node(s) found", nodes.len()),
                    error: None,
                    data: Some(serde_json::json!({ "nodes": summary })),
                    execution_time: start.elapsed(),
                })
            }
            NodesAction::Ping { node_id } => {
                let output = crate::tools::process_runner::run(&ProcessRequest::argv(&[
                    "tailscale",
                    "ping",
                    "-c",
                    "3",
                    &node_id,
                ]))
                .await;

                match output {
                    Ok(o) => {
                        let stdout = o.stdout_string();
                        let stderr = o.stderr_string();
                        let success = o.success();

                        Ok(ToolExecutionResult {
                            success,
                            output: if success {
                                stdout.to_string()
                            } else {
                                stderr.to_string()
                            },
                            error: if success {
                                None
                            } else {
                                Some(stderr.to_string())
                            },
                            data: Some(serde_json::json!({
                                "node_id": node_id,
                                "exit_code": o.exit_code(),
                                "signal": o.signal,
                            })),
                            execution_time: start.elapsed(),
                        })
                    }
                    Err(e) => Ok(ToolExecutionResult {
                        success: false,
                        output: String::new(),
                        error: Some(format!("Ping failed: {}", e)),
                        data: None,
                        execution_time: start.elapsed(),
                    }),
                }
            }
            NodesAction::CameraSnap { node_id } => {
                // Camera snap requires a remote agent running on the node
                // This is a stub that documents the capability
                warn!("Camera snap requested for node {} — requires remote agent", node_id);
                Ok(ToolExecutionResult {
                    success: false,
                    output: String::new(),
                    error: Some(
                        "Camera snap requires a remote agent on the target node. Not yet \
                         implemented."
                            .to_string(),
                    ),
                    data: Some(
                        serde_json::json!({ "node_id": node_id, "capability": "camera_snap" }),
                    ),
                    execution_time: start.elapsed(),
                })
            }
            NodesAction::CameraList { node_id } => {
                warn!("Camera list requested for node {} — requires remote agent", node_id);
                Ok(ToolExecutionResult {
                    success: false,
                    output: String::new(),
                    error: Some(
                        "Camera list requires a remote agent on the target node. Not yet \
                         implemented."
                            .to_string(),
                    ),
                    data: Some(serde_json::json!({ "node_id": node_id })),
                    execution_time: start.elapsed(),
                })
            }
            NodesAction::ScreenRecord { node_id, duration } => {
                warn!("Screen record requested for node {} — requires remote agent", node_id);
                Ok(ToolExecutionResult {
                    success: false,
                    output: String::new(),
                    error: Some(
                        "Screen recording requires a remote agent on the target node. Not yet \
                         implemented."
                            .to_string(),
                    ),
                    data: Some(serde_json::json!({
                        "node_id": node_id,
                        "duration": duration.unwrap_or(5),
                    })),
                    execution_time: start.elapsed(),
                })
            }
            NodesAction::LocationGet { node_id } => {
                // Try to get location from tailscale status --json (may include geo info)
                let output = crate::tools::process_runner::run(&ProcessRequest::argv(&[
                    "tailscale",
                    "status",
                    "--json",
                ]))
                .await;

                match output {
                    Ok(o) if o.success() => {
                        let _json: Value = serde_json::from_slice(&o.stdout).unwrap_or_default();
                        // Tailscale doesn't expose location directly; this is a placeholder
                        Ok(ToolExecutionResult {
                            success: false,
                            output: String::new(),
                            error: Some(
                                "Location tracking requires Tailscale's geo features or a remote \
                                 agent."
                                    .to_string(),
                            ),
                            data: Some(serde_json::json!({ "node_id": node_id })),
                            execution_time: start.elapsed(),
                        })
                    }
                    _ => Ok(ToolExecutionResult {
                        success: false,
                        output: String::new(),
                        error: Some("Failed to get node location".to_string()),
                        data: None,
                        execution_time: start.elapsed(),
                    }),
                }
            }
            NodesAction::DeviceStatus { node_id } => {
                let nodes = fetch_tailscale_nodes().await;
                match nodes
                    .into_iter()
                    .find(|n| n.id == node_id || n.name == node_id)
                {
                    Some(node) => Ok(ToolExecutionResult {
                        success: true,
                        output: format!(
                            "Node {} is {}",
                            node.name,
                            if node.online { "online" } else { "offline" }
                        ),
                        error: None,
                        data: Some(serde_json::to_value(node).unwrap_or_default()),
                        execution_time: start.elapsed(),
                    }),
                    None => Ok(ToolExecutionResult {
                        success: false,
                        output: String::new(),
                        error: Some(format!("Node {} not found", node_id)),
                        data: None,
                        execution_time: start.elapsed(),
                    }),
                }
            }
            NodesAction::DeviceInfo { node_id } => {
                let nodes = fetch_tailscale_nodes().await;
                match nodes
                    .into_iter()
                    .find(|n| n.id == node_id || n.name == node_id)
                {
                    Some(node) => Ok(ToolExecutionResult {
                        success: true,
                        output: format!(
                            "{}: {} running {}, IP {}",
                            node.name,
                            if node.online { "online" } else { "offline" },
                            node.os,
                            node.ipv4
                        ),
                        error: None,
                        data: Some(serde_json::to_value(node).unwrap_or_default()),
                        execution_time: start.elapsed(),
                    }),
                    None => Ok(ToolExecutionResult {
                        success: false,
                        output: String::new(),
                        error: Some(format!("Node {} not found", node_id)),
                        data: None,
                        execution_time: start.elapsed(),
                    }),
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_nodes_action_parsing() {
        let action: NodesAction = serde_json::from_value(serde_json::json!({
            "action": "status"
        }))
        .unwrap();
        assert!(matches!(action, NodesAction::Status));

        let action: NodesAction = serde_json::from_value(serde_json::json!({
            "action": "describe",
            "node_id": "node1"
        }))
        .unwrap();
        assert!(matches!(action, NodesAction::Describe { node_id } if node_id == "node1"));

        let action: NodesAction = serde_json::from_value(serde_json::json!({
            "action": "camera_snap",
            "node_id": "node1"
        }))
        .unwrap();
        assert!(matches!(action, NodesAction::CameraSnap { node_id } if node_id == "node1"));

        let action: NodesAction = serde_json::from_value(serde_json::json!({
            "action": "screen_record",
            "node_id": "node1",
            "duration": 10
        }))
        .unwrap();
        assert!(
            matches!(action, NodesAction::ScreenRecord { node_id, duration } if node_id == "node1" && duration == Some(10))
        );
    }

    #[test]
    fn test_tailscale_node_serialization() {
        let node = TailscaleNode {
            id: "n1".to_string(),
            name: "my-node".to_string(),
            ipv4: "100.64.1.1".to_string(),
            ipv6: "fd7a::1".to_string(),
            os: "linux".to_string(),
            online: true,
            last_seen: "2024-01-01".to_string(),
            tags: vec!["tag1".to_string()],
        };
        let json = serde_json::to_value(node).unwrap();
        assert_eq!(json["id"], "n1");
        assert_eq!(json["name"], "my-node");
        assert_eq!(json["online"], true);
    }

    #[test]
    fn test_nodes_tool_name_and_schema() {
        let tool = NodesTool::new();
        assert_eq!(tool.name(), "nodes");
        let schema = tool.parameters_schema();
        assert!(schema.get("properties").is_some());
    }

    #[tokio::test]
    async fn test_nodes_tool_no_tailscale() {
        let tool = NodesTool::new();
        let ctx = ToolContext::new("user", "conv");
        let result = tool
            .execute(serde_json::json!({ "action": "status" }), &ctx)
            .await
            .unwrap();

        assert!(!result.success);
        assert!(result.error.unwrap().contains("Tailscale"));
    }

    #[tokio::test]
    async fn test_nodes_tool_invalid_args() {
        let tool = NodesTool::new();
        let ctx = ToolContext::new("user", "conv");
        let result = tool.execute(serde_json::json!({}), &ctx).await.unwrap();

        assert!(!result.success);
        assert!(result.error.unwrap().contains("Invalid arguments"));
    }
}
