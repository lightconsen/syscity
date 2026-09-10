//! Syscity Cloud knowledge base tool (feature `cloud`): list, query and upload
//! documents in a cloud knowledge base (§3.7). Requires a cloud session token.

use async_trait::async_trait;
use serde_json::{json, Value};

use crate::cloud::client::CloudClient;
use crate::cloud::config::CloudConfig;
use crate::cloud::session;
use crate::error::SyscityError;
use crate::tools::approval::RiskLevel;
use crate::tools::sdk::ToolCapabilities;
use crate::tools::types::{Tool, ToolContext, ToolExecutionResult};

/// Tool that talks to the Syscity Cloud knowledge base API.
pub struct CloudKbTool {
    cfg: CloudConfig,
    secrets: std::sync::Arc<crate::secrets::SecretStoreHandle>,
}

impl CloudKbTool {
    pub fn new(
        cfg: CloudConfig,
        secrets: std::sync::Arc<crate::secrets::SecretStoreHandle>,
    ) -> Self {
        Self { cfg, secrets }
    }

    async fn client(&self) -> crate::Result<CloudClient> {
        let token = session::get_token(&self.secrets).await.ok_or_else(|| {
            SyscityError::Internal(
                "not signed in to Syscity Cloud — knowledge base needs a cloud session".to_string(),
            )
        })?;
        Ok(CloudClient::new(&self.cfg, token, self.secrets.clone()))
    }
}

#[async_trait]
impl Tool for CloudKbTool {
    fn name(&self) -> &str {
        "cloud_kb"
    }

    fn description(&self) -> &str {
        "Access the user's Syscity Cloud knowledge base (RAG). Operations: \
         list (no args), query (kb_id, query, top_k), upload (kb_id, filename, \
         content). Requires a cloud login."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "operation": { "type": "string", "enum": ["list", "query", "upload"] },
                "kb_id": { "type": "string", "description": "Knowledge base id (from list)" },
                "query": { "type": "string", "description": "Query for semantic retrieval" },
                "top_k": { "type": "integer", "default": 5 },
                "filename": { "type": "string", "description": "Upload target filename" },
                "content": { "type": "string", "description": "Upload document content" },
            },
            "required": ["operation"],
        })
    }

    fn capabilities(&self) -> ToolCapabilities {
        ToolCapabilities {
            requires_approval: false,
            risk_level: RiskLevel::Low,
            categories: vec!["knowledge".to_string(), "cloud".to_string()],
            ..Default::default()
        }
    }

    async fn execute(
        &self,
        args: Value,
        _context: &ToolContext,
    ) -> crate::Result<ToolExecutionResult> {
        let start = std::time::Instant::now();
        let client = self.client().await?;
        let op = args
            .get("operation")
            .and_then(|v| v.as_str())
            .unwrap_or("list");

        let output = match op {
            "list" => {
                let resp = client.list_kbs().await?;
                let kbs = resp
                    .get("knowledge_bases")
                    .and_then(|k| k.as_array())
                    .cloned()
                    .unwrap_or_default();
                let mut out = String::new();
                for k in kbs {
                    let id = k.get("id").and_then(|i| i.as_str()).unwrap_or("");
                    let name = k.get("name").and_then(|n| n.as_str()).unwrap_or("");
                    out.push_str(&format!("- {id}: {name}\n"));
                }
                if out.is_empty() {
                    out = "No knowledge bases.".to_string();
                }
                out
            }
            "query" => {
                let kb_id = args.get("kb_id").and_then(|v| v.as_str()).ok_or_else(|| {
                    SyscityError::Internal("kb_id required for query".to_string())
                })?;
                let query = args
                    .get("query")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| SyscityError::Internal("query required".to_string()))?;
                let top_k = args.get("top_k").and_then(|v| v.as_u64()).unwrap_or(5) as usize;
                let resp = client.kb_query(kb_id, query, top_k).await?;
                let hits = resp
                    .get("hits")
                    .and_then(|h| h.as_array())
                    .cloned()
                    .unwrap_or_default();
                let mut out = String::new();
                for h in hits {
                    let content = h.get("content").and_then(|c| c.as_str()).unwrap_or("");
                    let source = h.get("source").and_then(|s| s.as_str()).unwrap_or("");
                    let score = h.get("score").and_then(|s| s.as_f64()).unwrap_or(0.0);
                    out.push_str(&format!("[{source} · {:.0}%]\n{content}\n\n", score * 100.0));
                }
                if out.is_empty() {
                    out = "No matching results.".to_string();
                }
                out
            }
            "upload" => {
                let kb_id = args.get("kb_id").and_then(|v| v.as_str()).ok_or_else(|| {
                    SyscityError::Internal("kb_id required for upload".to_string())
                })?;
                let filename = args
                    .get("filename")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| SyscityError::Internal("filename required".to_string()))?;
                let content = args
                    .get("content")
                    .and_then(|v| v.as_str())
                    .unwrap_or_default();
                let resp = client
                    .kb_upload(kb_id, filename, content.as_bytes(), "text/plain")
                    .await?;
                let n = resp
                    .get("documents")
                    .and_then(|d| d.as_array())
                    .map(|a| a.len())
                    .unwrap_or(0);
                format!("Uploaded {n} document(s) to {kb_id}")
            }
            other => {
                return Err(SyscityError::Internal(format!("unknown cloud_kb operation {other}")))
            }
        };

        Ok(ToolExecutionResult {
            success: true,
            output,
            error: None,
            data: None,
            execution_time: start.elapsed(),
        })
    }
}
