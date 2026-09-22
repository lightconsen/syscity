//! Web fetch tool: manual redirect walking with an SSRF re-check on every hop.

use async_trait::async_trait;
use serde_json::Value;
use tracing::{debug, error, info, warn};

use super::{WebErrorCode, MAX_CONTENT_SIZE, MAX_REDIRECTS, WEB_TIMEOUT};
use crate::browser::{assert_navigation_allowed, NavigationPolicy};
use crate::tools::sdk::ToolCapabilities;
use crate::tools::{create_schema, Tool, ToolContext, ToolExecutionResult};

#[cfg(test)]
mod tests;

/// Web fetch tool for HTTP requests
#[derive(Debug)]
pub struct WebFetchTool {
    /// HTTP client (automatic redirects disabled — hops are walked manually)
    client: reqwest::Client,
    /// SSRF policy applied to the requested URL and every redirect hop.
    navigation_policy: NavigationPolicy,
}

impl Default for WebFetchTool {
    fn default() -> Self {
        Self::new()
    }
}

impl WebFetchTool {
    /// Create a new web fetch tool with the restrictive SSRF policy.
    pub fn new() -> Self {
        let client = reqwest::Client::builder()
            .timeout(WEB_TIMEOUT)
            .user_agent(
                "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/605.1.15 (KHTML, \
                 like Gecko) Version/18.0 Safari/605.1.15",
            )
            // Redirects are followed manually so each hop can be re-validated
            // against the SSRF guard before a request is issued.
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .unwrap_or_default();

        Self {
            client,
            navigation_policy: NavigationPolicy::default(),
        }
    }

    /// Override the SSRF navigation policy (for example a permissive policy
    /// for local development against loopback services).
    pub fn with_navigation_policy(mut self, policy: NavigationPolicy) -> Self {
        self.navigation_policy = policy;
        self
    }

    /// True for the redirect statuses this tool follows (301/302/303/307/308).
    ///
    /// Deliberately excludes other 3xx statuses such as 300 and 304, which
    /// carry body or cache semantics rather than a relocation target.
    fn is_followable_redirect(status: reqwest::StatusCode) -> bool {
        matches!(status.as_u16(), 301 | 302 | 303 | 307 | 308)
    }

    /// Resolve a `Location` header value against the current URL.
    ///
    /// Returns `None` when the value is unparseable or resolves outside
    /// http/https — such targets are refused instead of followed.
    fn resolve_redirect(current: &reqwest::Url, location: &str) -> Option<reqwest::Url> {
        let next = current.join(location.trim()).ok()?;
        matches!(next.scheme(), "http" | "https").then_some(next)
    }

    /// Check if content is HTML
    fn is_html(content_type: Option<&str>) -> bool {
        content_type
            .map(|ct| ct.contains("text/html") || ct.contains("application/xhtml"))
            .unwrap_or(false)
    }

    /// Simple HTML to markdown conversion.
    ///
    /// Delegates to the shared utility in `rag::ingestion::html_convert`.
    fn html_to_markdown(html: &str) -> String {
        crate::rag::ingestion::html_convert::html_to_markdown(html)
    }

    /// Truncate content if it exceeds the limit
    fn truncate_content(content: String) -> String {
        if content.len() > MAX_CONTENT_SIZE {
            // Find the nearest char boundary before MAX_CONTENT_SIZE to avoid
            // panicking on multi-byte UTF-8 characters.
            let cutoff = content.floor_char_boundary(MAX_CONTENT_SIZE);
            format!("{}\n\n[Content truncated: {} bytes total]", &content[..cutoff], content.len())
        } else {
            content
        }
    }
}

#[async_trait]
impl Tool for WebFetchTool {
    fn name(&self) -> &str {
        "web_fetch"
    }

    fn description(&self) -> &str {
        "Fetch content from a URL. Supports HTML to markdown conversion. Maximum content size: \
         100KB. HTTP error statuses are returned as normal results carrying the status code, not \
         as tool errors."
    }

    fn parameters_schema(&self) -> Value {
        create_schema(
            "Fetch content from a URL",
            serde_json::json!({
                "url": {
                    "type": "string",
                    "description": "The URL to fetch"
                }
            }),
            vec!["url"],
        )
    }

    fn capabilities(&self) -> ToolCapabilities {
        ToolCapabilities {
            read_only: true,
            requires_approval: false,
            risk_level: crate::tools::approval::RiskLevel::Medium,
            categories: vec!["network".to_string(), "web".to_string()],
            ..Default::default()
        }
    }

    async fn execute(
        &self,
        args: Value,
        _context: &ToolContext,
    ) -> crate::Result<ToolExecutionResult> {
        let url = args["url"].as_str().ok_or_else(|| {
            crate::error::SyscityError::Validation("Missing 'url' argument".to_string())
        })?;

        info!("Fetching URL: {}", url);

        // Validate URL
        let parsed_url = reqwest::Url::parse(url)
            .map_err(|e| crate::error::SyscityError::Validation(format!("Invalid URL: {}", e)))?;

        // Only allow HTTP and HTTPS. This is argument-level validation and
        // intentionally sits outside the provider-selection error taxonomy.
        if parsed_url.scheme() != "http" && parsed_url.scheme() != "https" {
            return Ok(ToolExecutionResult::error(format!(
                "Unsupported URL scheme: {}",
                parsed_url.scheme()
            )));
        }

        // Walk redirects manually: every hop — starting with the requested
        // URL — passes the SSRF navigation guard before a request is issued.
        let mut current = parsed_url;
        let mut completed_hops = 0usize;
        let response = loop {
            if let Err(blocked) =
                assert_navigation_allowed(current.as_str(), &self.navigation_policy).await
            {
                warn!("SSRF guard blocked {}: {}", current, blocked);
                return Ok(WebErrorCode::Unavailable.error_result(
                    format!(
                        "Fetch blocked by SSRF guard at {} (hop {}): {}",
                        current,
                        completed_hops + 1,
                        blocked
                    ),
                    serde_json::json!({
                        "url": url,
                        "blocked_target": current.as_str(),
                        "hop": completed_hops + 1
                    }),
                ));
            }

            let response = match self.client.get(current.clone()).send().await {
                Ok(resp) => resp,
                Err(e) => {
                    error!("Failed to fetch URL: {}", e);
                    return Ok(WebErrorCode::Unavailable.error_result(
                        format!("Failed to fetch URL: {}", e),
                        serde_json::json!({ "url": url }),
                    ));
                }
            };

            if !Self::is_followable_redirect(response.status()) {
                break response;
            }

            completed_hops += 1;
            if completed_hops > MAX_REDIRECTS {
                return Ok(WebErrorCode::Unavailable.error_result(
                    format!("Exceeded {} redirects while fetching {}", MAX_REDIRECTS, url),
                    serde_json::json!({ "url": url, "hops": completed_hops }),
                ));
            }

            let location = response
                .headers()
                .get(reqwest::header::LOCATION)
                .and_then(|value| value.to_str().ok())
                .map(str::to_string);
            let next = location.and_then(|loc| Self::resolve_redirect(&current, &loc));
            match next {
                Some(next) => {
                    debug!("Following redirect {} -> {}", current, next);
                    current = next;
                }
                None => {
                    return Ok(WebErrorCode::Unavailable.error_result(
                        format!(
                            "Redirect from {} carries a missing or disallowed Location header",
                            current
                        ),
                        serde_json::json!({ "url": url, "hop": completed_hops }),
                    ));
                }
            }
        };

        // Get content type (clone to avoid borrow issues)
        let content_type: Option<String> = response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .map(|s| s.to_string());

        debug!("Content-Type: {:?}", content_type);

        let status = response.status();

        // Get content
        let bytes = match response.bytes().await {
            Ok(b) => b,
            Err(e) => {
                error!("Failed to read response body: {}", e);
                return Ok(WebErrorCode::Unavailable.error_result(
                    format!("Failed to read response: {}", e),
                    serde_json::json!({ "url": url, "status": status.as_u16() }),
                ));
            }
        };

        // Convert to string
        let content = String::from_utf8_lossy(&bytes).to_string();

        // Convert HTML to markdown if needed
        let final_content = if Self::is_html(content_type.as_deref()) {
            debug!("Converting HTML to markdown");
            Self::html_to_markdown(&content)
        } else {
            content
        };

        // Truncate if needed
        let truncated = Self::truncate_content(final_content);

        info!(
            "Fetched {} bytes from {} (HTTP {}, {} redirect hop(s))",
            bytes.len(),
            url,
            status,
            completed_hops
        );

        // An HTTP error status is a result, not a tool error: surface the
        // status and body so the model can reason about them.
        if !status.is_success() {
            return Ok(ToolExecutionResult::success(format!(
                "HTTP {} from {}\n\n{}",
                status, current, truncated
            ))
            .with_data(serde_json::json!({
                "url": url,
                "final_url": current.as_str(),
                "status": status.as_u16(),
                "http_error": true,
                "content_type": content_type,
                "size": bytes.len(),
                "redirects": completed_hops
            })));
        }

        Ok(ToolExecutionResult::success(truncated).with_data(serde_json::json!({
            "url": url,
            "final_url": current.as_str(),
            "status": status.as_u16(),
            "content_type": content_type,
            "size": bytes.len(),
            "redirects": completed_hops
        })))
    }
}
