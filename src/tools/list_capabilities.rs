//! List capabilities tool — report what OS-specific capabilities are available.

use async_trait::async_trait;
use serde::Serialize;
use serde_json::Value;

use crate::tools::sdk::ToolCapabilities;
use crate::tools::{create_schema, Tool, ToolContext, ToolExecutionResult};

/// Information about a single capability set.
#[derive(Debug, Clone, Serialize)]
pub struct CapabilityInfo {
    pub id: String,
    pub name: String,
    pub description: String,
    pub available: bool,
    pub reason: Option<String>,
}

/// Tool that lists available capability sets on the host.
#[derive(Debug)]
pub struct ListCapabilitiesTool;

impl Default for ListCapabilitiesTool {
    fn default() -> Self {
        Self::new()
    }
}

impl ListCapabilitiesTool {
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl Tool for ListCapabilitiesTool {
    fn name(&self) -> &str {
        "list_capabilities"
    }

    fn description(&self) -> &str {
        "List all OS-specific capability sets available on this host. Returns which platform \
         controls (Linux Server, Desktop, etc.) are active and what tools they provide. Use when \
         the user asks 'what can you do', 'what capabilities do you have', or to self-diagnose why \
         a platform tool is missing."
    }

    fn parameters_schema(&self) -> Value {
        create_schema("List available capability sets", serde_json::json!({}), Vec::<String>::new())
    }

    fn capabilities(&self) -> ToolCapabilities {
        ToolCapabilities {
            requires_approval: false,
            risk_level: crate::tools::approval::RiskLevel::Low,
            categories: vec!["system".to_string(), "info".to_string()],
            idempotent: true,
            ..Default::default()
        }
    }

    async fn execute(
        &self,
        _args: Value,
        _context: &ToolContext,
    ) -> crate::Result<ToolExecutionResult> {
        let sets = crate::computer::platform::all_known_toolsets();

        let mut infos = Vec::new();
        for set in sets {
            let available = set.is_available();
            let reason = if available {
                None
            } else {
                let constraints = set.constraints();
                let mut reasons = Vec::new();
                if !constraints.target_os.is_empty()
                    && !constraints
                        .target_os
                        .iter()
                        .any(|os| os == std::env::consts::OS)
                {
                    reasons.push(format!("requires OS: {:?}", constraints.target_os));
                }
                if constraints.requires_gui && !has_display() {
                    reasons.push("requires GUI/display".to_string());
                }
                for svc in &constraints.requires_services {
                    reasons.push(format!("requires service: {}", svc));
                }
                if reasons.is_empty() {
                    Some("unknown constraint failure".to_string())
                } else {
                    Some(reasons.join(", "))
                }
            };

            infos.push(CapabilityInfo {
                id: set.id().to_string(),
                name: set.name().to_string(),
                description: set.description().to_string(),
                available,
                reason,
            });
        }

        let json = serde_json::to_string_pretty(&infos)
            .map_err(crate::error::SyscityError::Serialization)?;

        Ok(ToolExecutionResult::success(json).with_data(serde_json::to_value(infos)?))
    }
}

fn has_display() -> bool {
    std::env::var("DISPLAY").is_ok()
        || std::env::var("WAYLAND_DISPLAY").is_ok()
        || cfg!(target_os = "macos")
        || cfg!(target_os = "windows")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_list_capabilities_tool_creation() {
        let tool = ListCapabilitiesTool::new();
        assert_eq!(tool.name(), "list_capabilities");
        assert!(!tool.description().is_empty());
    }
}
