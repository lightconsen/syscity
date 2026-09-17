//! Agent listing tool
//!
//! List all available agent personalities/types.

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::Value;
use tokio::sync::RwLock;

use super::{Tool, ToolContext, ToolExecutionResult};
use crate::tools::sdk::ToolCapabilities;

/// List available agent personalities.
pub struct AgentsListTool {
    agent_registry: Arc<RwLock<crate::agent::AgentRegistry>>,
}

impl AgentsListTool {
    pub fn new(agent_registry: Arc<RwLock<crate::agent::AgentRegistry>>) -> Self {
        Self { agent_registry }
    }
}

#[async_trait]
impl Tool for AgentsListTool {
    fn name(&self) -> &str {
        "agents_list"
    }

    fn description(&self) -> &str {
        "List all available agent personalities/types that can be used for subagent spawning."
    }

    fn parameters_schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {},
        })
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
        let start = std::time::Instant::now();
        let registry = self.agent_registry.read().await;
        let agents = registry.list();

        let agent_info: Vec<_> = agents
            .iter()
            .filter_map(|id| {
                registry.get(id).map(|p| {
                    serde_json::json!({
                        "id": id,
                        "name": p.display_name(),
                    })
                })
            })
            .collect();

        Ok(ToolExecutionResult {
            success: true,
            output: format!("Found {} agent personality(ies)", agent_info.len()),
            error: None,
            data: Some(serde_json::json!({ "agents": agent_info })),
            execution_time: start.elapsed(),
        })
    }
}
