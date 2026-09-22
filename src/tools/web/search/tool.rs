//! `Tool` implementation for the web search tool: the provider fallback walk.

use async_trait::async_trait;
use serde_json::Value;
use tracing::{debug, info, warn};

use super::super::{
    classify_attempts, missing_credential, AttemptKind, ProviderAttempt, WebErrorCode,
};
use super::{provider_name, WebSearchTool};
use crate::tools::sdk::ToolCapabilities;
use crate::tools::{create_schema, Tool, ToolContext, ToolExecutionResult};

#[async_trait]
impl Tool for WebSearchTool {
    fn name(&self) -> &str {
        "web_search"
    }

    fn description(&self) -> &str {
        "Search the web for information. Returns a list of search results."
    }

    fn parameters_schema(&self) -> Value {
        create_schema(
            "Search the web",
            serde_json::json!({
                "query": {
                    "type": "string",
                    "description": "The search query"
                },
                "limit": {
                    "type": "integer",
                    "description": "Maximum number of results (default: 5, max: 10)",
                    "default": 5
                }
            }),
            vec!["query"],
        )
    }

    fn capabilities(&self) -> ToolCapabilities {
        ToolCapabilities {
            read_only: true,
            requires_approval: false,
            risk_level: crate::tools::approval::RiskLevel::Low,
            categories: vec!["network".to_string(), "web".to_string()],
            ..Default::default()
        }
    }

    async fn execute(
        &self,
        args: Value,
        _context: &ToolContext,
    ) -> crate::Result<ToolExecutionResult> {
        let query = args["query"].as_str().ok_or_else(|| {
            crate::error::SyscityError::Validation("Missing 'query' argument".to_string())
        })?;

        let limit = args["limit"]
            .as_u64()
            .map(|l| l as usize)
            .unwrap_or(5)
            .clamp(1, 10);

        if query.len() > 500 {
            return Ok(ToolExecutionResult::error(
                "Query too long (max 500 characters)".to_string(),
            ));
        }

        info!("Searching for: {}", query);

        let execute_start = std::time::Instant::now();
        let providers = self.providers.read().await.clone();

        // Nothing configured at all: report the stable NOT_CONFIGURED state
        // instead of a misleading empty-success.
        if providers.is_empty() {
            return Ok(WebErrorCode::NotConfigured.error_result(
                "No search provider is configured".to_string(),
                serde_json::json!({ "query": query }),
            ));
        }

        let mut failures: Vec<ProviderAttempt> = Vec::new();
        for (idx, provider) in providers.iter().enumerate() {
            let provider_name = provider_name(provider);

            // A provider enabled without its required credential is a
            // configuration gap (CONFIGURED_MISSING), not an outage — detect
            // it before spending a doomed network round-trip.
            if let Some(field) = missing_credential(provider) {
                warn!("Provider {} is configured but '{}' is absent", provider_name, field);
                failures.push(ProviderAttempt::missing_value(provider_name, field));
                continue;
            }

            info!("Trying provider {} ({}): {}", idx + 1, providers.len(), provider_name);
            match self.search_with_provider(provider, query, limit).await {
                Ok(results) if !results.is_empty() => {
                    if idx > 0 {
                        info!(
                            "Search fallback succeeded after {} provider(s): {}",
                            idx, provider_name
                        );
                    }
                    let result_count = results.len();
                    let format_start = std::time::Instant::now();
                    let formatted: Vec<String> = results
                        .iter()
                        .enumerate()
                        .map(|(i, r)| {
                            format!("{}. {}\n   URL: {}\n   {}", i + 1, r.title, r.url, r.snippet)
                        })
                        .collect();

                    let output = formatted.join("\n\n");
                    debug!("Formatted search results in {:?}", format_start.elapsed());
                    info!(
                        "web_search completed in {:?} with {} results from {}",
                        execute_start.elapsed(),
                        result_count,
                        provider_name
                    );
                    return Ok(ToolExecutionResult::success(output).with_data(serde_json::json!({
                        "query": query,
                        "result_count": result_count,
                        "provider": provider_name,
                        "results": results.iter().map(|r| {
                            serde_json::json!({
                                "title": r.title,
                                "url": r.url,
                                "snippet": r.snippet
                            })
                        }).collect::<Vec<_>>()
                    })));
                }
                Ok(_) => {
                    debug!("Provider {} returned no results", provider_name);
                }
                Err(e) => {
                    warn!("Provider {} search failed: {}", provider_name, e);
                    failures.push(ProviderAttempt::unavailable(provider_name, e.to_string()));
                }
            }
        }

        info!("web_search exhausted all providers in {:?}", execute_start.elapsed());

        // Every attempt either failed outright or came back empty-and-useless
        // for at least one provider: surface the classified selection failure.
        if !failures.is_empty() {
            let code = classify_attempts(&failures);
            let summary = failures
                .iter()
                .map(ProviderAttempt::summary)
                .collect::<Vec<_>>()
                .join("; ");
            return Ok(code.error_result(
                format!("Search failed across {} provider(s): {}", failures.len(), summary),
                serde_json::json!({
                    "query": query,
                    "attempts": failures.iter().map(|failure| serde_json::json!({
                        "provider": failure.provider,
                        "outcome": failure.kind.outcome(),
                        "detail": match &failure.kind {
                            AttemptKind::MissingValue { field } => {
                                format!("required value '{}' is absent", field)
                            }
                            AttemptKind::Unavailable { detail } => detail.clone(),
                        },
                    })).collect::<Vec<_>>()
                }),
            ));
        }

        Ok(ToolExecutionResult::success(
            "No results found: every configured search provider returned empty results. \
             You MUST tell the user the search returned nothing, and any answer you then \
             give comes from prior knowledge, not from the search — never present \
             prior-knowledge claims as search results."
                .to_string(),
        ))
    }
}
