//! Web tools for Syscity
//!
//! Tools for fetching web content and searching the web.
//!
//! # Error-code contract
//!
//! Selection failures carry a machine-readable code in the result payload
//! (`data.code`) drawn from [`WebErrorCode`], so webui surfaces, retry
//! policies, and agent self-correction can branch on the code instead of
//! parsing message strings.
//!
//! # Redirect safety
//!
//! Redirects are followed manually (up to [`MAX_REDIRECTS`] hops) and every
//! hop — including the initially requested URL — is re-validated against the
//! SSRF navigation guard (`crate::browser::assert_navigation_allowed`)
//! before a request is issued. A public entry point therefore cannot bounce
//! the fetcher onto a private or blocklisted target via a redirect.
//!
//! # Non-2xx responses
//!
//! An HTTP error status is a *result*, not a tool error: the status and body
//! are surfaced in a successful payload so the model can reason about them.
//! Only transport, SSRF-policy, and configuration failures produce error
//! results.

use async_trait::async_trait;
use serde_json::Value;
use tracing::{debug, error, info, warn};

use super::{create_schema, Tool, ToolContext, ToolExecutionResult};
use crate::browser::{assert_navigation_allowed, NavigationPolicy};
use crate::tools::sdk::ToolCapabilities;

/// Maximum content size to fetch (100KB)
const MAX_CONTENT_SIZE: usize = 100 * 1024;

/// Default timeout for web requests
const WEB_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(15);

/// Per-provider request timeout. Each provider gets a strict, shorter budget
/// so that a fallback chain has time to try more than one backend before the
/// tool-level wrapper times out.
const PROVIDER_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(12);

/// Maximum number of redirects [`WebFetchTool`] follows manually.
///
/// Every hop is validated before it is issued, so this limit bounds both the
/// work done and the damage of a malicious redirect loop.
const MAX_REDIRECTS: usize = 10;

/// Stable, machine-readable outcome codes for web tool selection failures.
///
/// The wire values are part of the tool contract: webui surfaces, retry
/// policies, and agent self-correction branch on them instead of parsing
/// error strings. They ride on error results as `data.code`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WebErrorCode {
    /// The feature is configured, but a required value inside it is absent
    /// (for example an API-key-bearing search provider enabled with an empty
    /// key).
    ConfiguredMissing,
    /// Nothing is configured to serve the request (for example an empty
    /// search-provider list).
    NotConfigured,
    /// Configuration is complete, but every candidate endpoint failed —
    /// transport errors, timeouts, upstream HTTP errors, or SSRF blocks.
    Unavailable,
    /// Several candidates were tried and their failures span more than one
    /// cause class, so no single remediation applies. Inspect the
    /// per-attempt details in `data.attempts` to attribute the failures.
    Ambiguous,
}

impl WebErrorCode {
    /// Canonical SCREAMING_SNAKE_CASE wire value for this code.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ConfiguredMissing => "CONFIGURED_MISSING",
            Self::NotConfigured => "NOT_CONFIGURED",
            Self::Unavailable => "UNAVAILABLE",
            Self::Ambiguous => "AMBIGUOUS",
        }
    }

    /// Build an error tool result whose `data` carries this code merged with
    /// the fields of `extra`.
    fn error_result(self, message: impl Into<String>, extra: Value) -> ToolExecutionResult {
        let mut data = serde_json::json!({ "code": self.as_str() });
        if let (Some(target), Some(source)) = (data.as_object_mut(), extra.as_object()) {
            for (key, value) in source {
                target.insert(key.clone(), value.clone());
            }
        }
        ToolExecutionResult::error(message).with_data(data)
    }
}

/// Outcome class of a single failed attempt from the provider fallback walk.
#[derive(Debug, Clone, PartialEq, Eq)]
enum AttemptKind {
    /// Required value absent from an otherwise-enabled provider.
    MissingValue { field: &'static str },
    /// Provider configured correctly, but the endpoint did not serve results.
    Unavailable { detail: String },
}

impl AttemptKind {
    /// The stable selection-failure code this outcome maps to.
    fn code(&self) -> WebErrorCode {
        match self {
            Self::MissingValue { .. } => WebErrorCode::ConfiguredMissing,
            Self::Unavailable { .. } => WebErrorCode::Unavailable,
        }
    }

    /// Wire label for the per-attempt `outcome` field in `data.attempts`.
    fn outcome(&self) -> &'static str {
        match self {
            Self::MissingValue { .. } => "configured_missing",
            Self::Unavailable { .. } => "unavailable",
        }
    }
}

/// Record of one failed provider attempt during the fallback walk.
#[derive(Debug, Clone)]
struct ProviderAttempt {
    provider: &'static str,
    kind: AttemptKind,
}

impl ProviderAttempt {
    fn missing_value(provider: &'static str, field: &'static str) -> Self {
        Self {
            provider,
            kind: AttemptKind::MissingValue { field },
        }
    }

    fn unavailable(provider: &'static str, detail: String) -> Self {
        Self {
            provider,
            kind: AttemptKind::Unavailable { detail },
        }
    }

    /// One-line human summary used in the model-facing message.
    fn summary(&self) -> String {
        match &self.kind {
            AttemptKind::MissingValue { field } => format!("{}: missing {}", self.provider, field),
            AttemptKind::Unavailable { detail } => format!("{}: {}", self.provider, detail),
        }
    }
}

/// Classify a completed provider fallback walk into one stable code.
///
/// Attempts sharing a single cause class keep that class's code; mixed cause
/// classes collapse to [`WebErrorCode::Ambiguous`] because no single fix
/// (restore connectivity, supply the missing key) covers every attempt. An
/// empty slice maps to [`WebErrorCode::NotConfigured`] defensively; callers
/// normally short-circuit empty provider lists before walking.
fn classify_attempts(attempts: &[ProviderAttempt]) -> WebErrorCode {
    let Some(first) = attempts.first() else {
        return WebErrorCode::NotConfigured;
    };
    let shared = first.kind.code();
    if attempts.iter().all(|attempt| attempt.kind.code() == shared) {
        shared
    } else {
        WebErrorCode::Ambiguous
    }
}

/// Name of the required configuration value absent from `provider`, if any.
///
/// A provider enabled in configuration without its credential is a
/// configuration gap (`CONFIGURED_MISSING`), not an outage — detecting it
/// here avoids spending a doomed network round-trip.
fn missing_credential(provider: &SearchProvider) -> Option<&'static str> {
    match provider {
        SearchProvider::DuckDuckGo => None,
        SearchProvider::Brave { api_key }
        | SearchProvider::Tavily { api_key }
        | SearchProvider::SerpApi { api_key }
        | SearchProvider::Exa { api_key }
        | SearchProvider::Firecrawl { api_key }
        | SearchProvider::Serper { api_key }
        | SearchProvider::Bocha { api_key } => api_key.trim().is_empty().then_some("api_key"),
        #[cfg(feature = "cloud")]
        SearchProvider::Cloud { .. } => None,
        SearchProvider::Custom { url, .. } => url.trim().is_empty().then_some("url"),
    }
}

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

/// Web search tool
#[derive(Debug)]
pub struct WebSearchTool {
    /// HTTP client
    client: reqwest::Client,
    /// Search providers to try in order (fallback).
    /// Wrapped in Arc<RwLock<>> so hot-reload can update providers without
    /// rebuilding the entire tool registry.
    providers: std::sync::Arc<tokio::sync::RwLock<Vec<SearchProvider>>>,
}

/// Search provider configuration
#[derive(Debug, Clone, Default)]
pub enum SearchProvider {
    /// DuckDuckGo (HTML scraping)
    #[default]
    DuckDuckGo,
    /// Brave Search API (requires key)
    /// https://brave.com/search/api/
    Brave { api_key: String },
    /// Custom search provider
    Custom {
        url: String,
        api_key: Option<String>,
        headers: Option<std::collections::HashMap<String, String>>,
        result_parser: Option<fn(&str, usize) -> Vec<SearchResult>>,
    },
    /// Tavily AI Search API (requires key)
    /// https://docs.tavily.com/
    Tavily { api_key: String },
    /// SerpAPI Google Search API (requires key)
    /// https://serpapi.com/
    SerpApi { api_key: String },
    /// Exa (formerly Metaphor) AI Search API (requires key)
    /// https://docs.exa.ai/
    Exa { api_key: String },
    /// Firecrawl Search API (requires key)
    /// https://docs.firecrawl.dev/
    Firecrawl { api_key: String },
    /// Serper Google Search API (requires key)
    /// https://serper.dev/
    Serper { api_key: String },
    /// Bocha AI Web Search API (requires key)
    /// https://bochaai.com/
    Bocha { api_key: String },
    /// Syscity Cloud web search (`/v1/search`, session-token auth; feature
    /// `cloud`). Normalized results identical in shape to the local providers.
    #[cfg(feature = "cloud")]
    Cloud { api_base: String },
}

impl SearchProvider {
    /// Build a provider from its config name and resolved API key.
    ///
    /// Returns `None` for unknown names (callers log and skip). This is the
    /// single name → variant mapping shared by gateway spawn, hot-reload, and
    /// the standalone eval registry.
    pub fn from_config_name(name: &str, api_key: Option<String>) -> Option<SearchProvider> {
        let key = api_key.unwrap_or_default();
        match name {
            "tavily" => Some(SearchProvider::Tavily { api_key: key }),
            "serpapi" => Some(SearchProvider::SerpApi { api_key: key }),
            "exa" => Some(SearchProvider::Exa { api_key: key }),
            "firecrawl" => Some(SearchProvider::Firecrawl { api_key: key }),
            "serper" => Some(SearchProvider::Serper { api_key: key }),
            "bocha" => Some(SearchProvider::Bocha { api_key: key }),
            "duckduckgo" => Some(SearchProvider::DuckDuckGo),
            "brave" => Some(SearchProvider::Brave { api_key: key }),
            _ => None,
        }
    }
}

impl Default for WebSearchTool {
    fn default() -> Self {
        let client = reqwest::Client::builder()
            .timeout(WEB_TIMEOUT)
            .user_agent(
                "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/605.1.15 (KHTML, \
                 like Gecko) Version/18.0 Safari/605.1.15",
            )
            .build()
            .unwrap_or_default();

        Self {
            client,
            providers: std::sync::Arc::new(tokio::sync::RwLock::new(vec![
                SearchProvider::DuckDuckGo,
            ])),
        }
    }
}

impl WebSearchTool {
    /// Create a new web search tool
    pub fn new() -> Self {
        Self::default()
    }

    /// Set a single search provider
    pub fn with_provider(mut self, provider: SearchProvider) -> Self {
        self.providers = std::sync::Arc::new(tokio::sync::RwLock::new(vec![provider]));
        self
    }

    /// Set multiple search providers to try in order
    pub fn with_providers(mut self, providers: Vec<SearchProvider>) -> Self {
        self.providers = std::sync::Arc::new(tokio::sync::RwLock::new(providers));
        self
    }

    /// Replace the provider list at runtime (used by hot-reload).
    pub async fn set_providers(&self, providers: Vec<SearchProvider>) {
        let mut guard = self.providers.write().await;
        *guard = providers;
    }

    /// Use a pre-built, shared provider list so the registry and the tool
    /// observe the same providers during hot-reload.
    pub fn with_providers_arc(
        mut self,
        providers: std::sync::Arc<tokio::sync::RwLock<Vec<SearchProvider>>>,
    ) -> Self {
        self.providers = providers;
        self
    }
}

/// Return a human-readable provider name for logging.
fn provider_name(provider: &SearchProvider) -> &'static str {
    match provider {
        SearchProvider::DuckDuckGo => "duckduckgo",
        SearchProvider::Brave { .. } => "brave",
        SearchProvider::Tavily { .. } => "tavily",
        SearchProvider::SerpApi { .. } => "serpapi",
        SearchProvider::Exa { .. } => "exa",
        SearchProvider::Firecrawl { .. } => "firecrawl",
        SearchProvider::Serper { .. } => "serper",
        SearchProvider::Bocha { .. } => "bocha",
        #[cfg(feature = "cloud")]
        SearchProvider::Cloud { .. } => "cloud",
        SearchProvider::Custom { .. } => "custom",
    }
}

impl WebSearchTool {
    /// Execute a search against a single provider.
    /// Each provider request is wrapped with its own timeout so that a slow
    /// backend does not consume the entire tool-level budget.
    async fn search_with_provider(
        &self,
        provider: &SearchProvider,
        query: &str,
        limit: usize,
    ) -> crate::Result<Vec<SearchResult>> {
        let provider_name = provider_name(provider);
        let start = std::time::Instant::now();
        let result = tokio::time::timeout(PROVIDER_TIMEOUT, async {
            match provider {
                SearchProvider::DuckDuckGo => self.search_duckduckgo(query, limit).await,
                SearchProvider::Brave { api_key } => self.search_brave(api_key, query, limit).await,
                SearchProvider::Tavily { api_key } => {
                    self.search_tavily(api_key, query, limit).await
                }
                SearchProvider::SerpApi { api_key } => {
                    self.search_serpapi(api_key, query, limit).await
                }
                SearchProvider::Exa { api_key } => self.search_exa(api_key, query, limit).await,
                SearchProvider::Firecrawl { api_key } => {
                    self.search_firecrawl(api_key, query, limit).await
                }
                SearchProvider::Serper { api_key } => {
                    self.search_serper(api_key, query, limit).await
                }
                SearchProvider::Bocha { api_key } => self.search_bocha(api_key, query, limit).await,
                #[cfg(feature = "cloud")]
                SearchProvider::Cloud { api_base } => {
                    self.search_cloud(api_base, query, limit).await
                }
                SearchProvider::Custom {
                    url,
                    api_key,
                    headers,
                    result_parser,
                } => {
                    self.search_custom(url, api_key, headers, result_parser, query, limit)
                        .await
                }
            }
        })
        .await
        .map_err(|_| {
            crate::error::SyscityError::Timeout(format!(
                "Provider '{}' search exceeded {:?}",
                provider_name, PROVIDER_TIMEOUT
            ))
        })?;

        debug!("Provider {} search completed in {:?}", provider_name, start.elapsed());
        result
    }

    /// Search using DuckDuckGo
    async fn search_duckduckgo(
        &self,
        query: &str,
        limit: usize,
    ) -> crate::Result<Vec<SearchResult>> {
        // Try primary endpoint first, fall back to alternative
        let encoded = urlencoding::encode(query);
        let primary_url = format!("https://html.duckduckgo.com/html/?q={}", encoded);
        let fallback_url = format!("https://lite.duckduckgo.com/lite/?q={}", encoded);

        let response = self
            .client
            .get(&primary_url)
            .header("Accept", "text/html,application/xhtml+xml,application/xml;q=0.9,*/*;q=0.8")
            .header("Accept-Language", "zh-CN,zh;q=0.9,en;q=0.8")
            .header("Accept-Encoding", "gzip, deflate")
            .header("DNT", "1")
            .header("Connection", "keep-alive")
            .header("Upgrade-Insecure-Requests", "1")
            .header("Sec-Fetch-Dest", "document")
            .header("Sec-Fetch-Mode", "navigate")
            .header("Sec-Fetch-Site", "none")
            .header("Sec-Fetch-User", "?1")
            .timeout(std::time::Duration::from_secs(60))
            .send()
            .await;

        let response = match response {
            Ok(r) if r.status().is_success() => r,
            Ok(r) => {
                debug!("DDG primary returned HTTP {}, trying fallback", r.status());
                self.client
                    .get(&fallback_url)
                    .header("Accept", "text/html")
                    .header("Accept-Language", "zh-CN,zh;q=0.9")
                    .timeout(std::time::Duration::from_secs(60))
                    .send()
                    .await
                    .map_err(|e| {
                        crate::error::SyscityError::Internal(format!(
                            "Search request failed: {}",
                            e
                        ))
                    })?
            }
            Err(_) => {
                debug!("DDG primary connection failed, trying fallback endpoint");
                self.client
                    .get(&fallback_url)
                    .header("Accept", "text/html")
                    .header("Accept-Language", "zh-CN,zh;q=0.9")
                    .timeout(std::time::Duration::from_secs(60))
                    .send()
                    .await
                    .map_err(|e| {
                        crate::error::SyscityError::Internal(format!(
                            "Search request failed: {}",
                            e
                        ))
                    })?
            }
        };

        let html = response.text().await.map_err(|e| {
            crate::error::SyscityError::Internal(format!("Failed to read response: {}", e))
        })?;

        // Parse results from HTML
        let results = Self::parse_duckduckgo_results(&html, limit);

        Ok(results)
    }

    /// Parse DuckDuckGo HTML results
    fn parse_duckduckgo_results(html: &str, limit: usize) -> Vec<SearchResult> {
        let mut results = Vec::new();

        // Look for result containers
        for chunk in html.split("<div class=\"result\"") {
            if results.len() >= limit {
                break;
            }

            if let Some(title_start) = chunk.find("<a rel=\"nofollow\"") {
                let title_area = &chunk[title_start..];

                // Extract URL
                let url = if let Some(href_start) = title_area.find("href=\"") {
                    let href_pos = href_start + 6;
                    if let Some(href_end) = title_area[href_pos..].find("\"") {
                        let raw_url = &title_area[href_pos..href_pos + href_end];
                        // DuckDuckGo redirects through their domain
                        if raw_url.starts_with("//duckduckgo.com/l/?") {
                            if let Some(udm_start) = raw_url.find("uddg=") {
                                let encoded = &raw_url[udm_start + 5..];
                                urlencoding::decode(encoded)
                                    .map(|s| s.to_string())
                                    .unwrap_or_else(|_| raw_url.to_string())
                            } else {
                                raw_url.to_string()
                            }
                        } else {
                            raw_url.to_string()
                        }
                    } else {
                        continue;
                    }
                } else {
                    continue;
                };

                // Extract title
                let title = if let Some(tag_end) = title_area.find(">") {
                    let content_start = tag_end + 1;
                    if let Some(content_end) = title_area[content_start..].find("</a>") {
                        Self::clean_html(&title_area[content_start..content_start + content_end])
                    } else {
                        continue;
                    }
                } else {
                    continue;
                };

                // Extract snippet
                let snippet =
                    if let Some(snippet_start) = chunk.find("<a class=\"result__snippet\"") {
                        let snippet_area = &chunk[snippet_start..];
                        if let Some(tag_end) = snippet_area.find(">") {
                            let content_start = tag_end + 1;
                            if let Some(content_end) = snippet_area[content_start..].find("</a>") {
                                Self::clean_html(
                                    &snippet_area[content_start..content_start + content_end],
                                )
                            } else {
                                String::new()
                            }
                        } else {
                            String::new()
                        }
                    } else {
                        String::new()
                    };

                results.push(SearchResult { title, url, snippet });
            }
        }

        results
    }

    /// Search using Brave Search API
    async fn search_brave(
        &self,
        api_key: &str,
        query: &str,
        limit: usize,
    ) -> crate::Result<Vec<SearchResult>> {
        let url = format!(
            "https://api.search.brave.com/res/v1/web/search?q={}&count={}&offset=0",
            urlencoding::encode(query),
            limit.min(20)
        );

        let response = self
            .client
            .get(&url)
            .header("Accept", "application/json")
            .header("X-Subscription-Token", api_key)
            .send()
            .await
            .map_err(|e| {
                crate::error::SyscityError::Internal(format!("Brave search request failed: {}", e))
            })?;

        if !response.status().is_success() {
            return Err(crate::error::SyscityError::Internal(format!(
                "Brave search failed: HTTP {} - {}",
                response.status(),
                response.text().await.unwrap_or_default()
            )));
        }

        let json: serde_json::Value = response.json().await.map_err(|e| {
            crate::error::SyscityError::Internal(format!("Failed to parse Brave response: {}", e))
        })?;

        let mut results = Vec::new();

        // Parse Brave Search API response
        if let Some(web) = json.get("web").and_then(|w| w.get("results")) {
            if let Some(items) = web.as_array() {
                for item in items.iter().take(limit) {
                    let title = item
                        .get("title")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string();
                    let url = item
                        .get("url")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string();
                    let snippet = item
                        .get("description")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string();

                    if !title.is_empty() && !url.is_empty() {
                        results.push(SearchResult { title, url, snippet });
                    }
                }
            }
        }

        Ok(results)
    }

    /// Search using Tavily AI Search API
    async fn search_tavily(
        &self,
        api_key: &str,
        query: &str,
        limit: usize,
    ) -> crate::Result<Vec<SearchResult>> {
        let start = std::time::Instant::now();
        debug!("Tavily search starting for query: {}", query);

        let body = serde_json::json!({
            "query": query,
            "search_depth": "basic",
            "max_results": limit.min(20),
        });

        let request_start = std::time::Instant::now();
        let response = self
            .client
            .post("https://api.tavily.com/search")
            .header("Authorization", format!("Bearer {}", api_key))
            .json(&body)
            .send()
            .await
            .map_err(|e| {
                crate::error::SyscityError::Internal(format!("Tavily search failed: {}", e))
            })?;
        debug!("Tavily request sent and response received in {:?}", request_start.elapsed());

        let status = response.status();
        if !status.is_success() {
            let body_text = response.text().await.unwrap_or_default();
            return Err(crate::error::SyscityError::Internal(format!(
                "Tavily search failed: HTTP {} - {}",
                status, body_text
            )));
        }

        let parse_start = std::time::Instant::now();
        let data: serde_json::Value = response.json().await.map_err(|e| {
            crate::error::SyscityError::Internal(format!("Failed to parse Tavily response: {}", e))
        })?;
        debug!("Tavily response parsed in {:?}", parse_start.elapsed());

        let mut results = Vec::new();
        if let Some(items) = data["results"].as_array() {
            for item in items.iter().take(limit) {
                results.push(SearchResult {
                    title: item["title"].as_str().unwrap_or("").to_string(),
                    url: item["url"].as_str().unwrap_or("").to_string(),
                    snippet: item["content"].as_str().unwrap_or("").to_string(),
                });
            }
        }

        debug!(
            "Tavily search completed in {:?} with {} results",
            start.elapsed(),
            results.len()
        );
        Ok(results)
    }

    /// Search using SerpAPI Google Search API
    async fn search_serpapi(
        &self,
        api_key: &str,
        query: &str,
        limit: usize,
    ) -> crate::Result<Vec<SearchResult>> {
        let url = format!(
            "https://serpapi.com/search?q={}&api_key={}&engine=google",
            urlencoding::encode(query),
            urlencoding::encode(api_key),
        );

        let response = self.client.get(&url).send().await.map_err(|e| {
            crate::error::SyscityError::Internal(format!("SerpAPI search failed: {}", e))
        })?;

        let status = response.status();
        if !status.is_success() {
            let body_text = response.text().await.unwrap_or_default();
            return Err(crate::error::SyscityError::Internal(format!(
                "SerpAPI search failed: HTTP {} - {}",
                status, body_text
            )));
        }

        let data: serde_json::Value = response.json().await.map_err(|e| {
            crate::error::SyscityError::Internal(format!("Failed to parse SerpAPI response: {}", e))
        })?;

        let mut results = Vec::new();
        if let Some(items) = data["organic_results"].as_array() {
            for item in items.iter().take(limit) {
                results.push(SearchResult {
                    title: item["title"].as_str().unwrap_or("").to_string(),
                    url: item["link"].as_str().unwrap_or("").to_string(),
                    snippet: item["snippet"].as_str().unwrap_or("").to_string(),
                });
            }
        }

        Ok(results)
    }

    /// Search using Exa (formerly Metaphor) AI Search API
    async fn search_exa(
        &self,
        api_key: &str,
        query: &str,
        limit: usize,
    ) -> crate::Result<Vec<SearchResult>> {
        let body = serde_json::json!({
            "query": query,
            "num_results": limit.min(20),
        });

        let response = self
            .client
            .post("https://api.exa.ai/search")
            .header("x-api-key", api_key)
            .json(&body)
            .send()
            .await
            .map_err(|e| {
                crate::error::SyscityError::Internal(format!("Exa search failed: {}", e))
            })?;

        let status = response.status();
        if !status.is_success() {
            let body_text = response.text().await.unwrap_or_default();
            return Err(crate::error::SyscityError::Internal(format!(
                "Exa search failed: HTTP {} - {}",
                status, body_text
            )));
        }

        let data: serde_json::Value = response.json().await.map_err(|e| {
            crate::error::SyscityError::Internal(format!("Failed to parse Exa response: {}", e))
        })?;

        let mut results = Vec::new();
        if let Some(items) = data["results"].as_array() {
            for item in items.iter().take(limit) {
                results.push(SearchResult {
                    title: item["title"].as_str().unwrap_or("").to_string(),
                    url: item["url"].as_str().unwrap_or("").to_string(),
                    snippet: item["snippet"].as_str().unwrap_or("").to_string(),
                });
            }
        }

        Ok(results)
    }

    /// Search using Firecrawl Search API
    async fn search_firecrawl(
        &self,
        api_key: &str,
        query: &str,
        limit: usize,
    ) -> crate::Result<Vec<SearchResult>> {
        let body = serde_json::json!({
            "query": query,
            "maxResults": limit.min(20),
        });

        let response = self
            .client
            .post("https://api.firecrawl.dev/v1/search")
            .header("Authorization", format!("Bearer {}", api_key))
            .json(&body)
            .send()
            .await
            .map_err(|e| {
                crate::error::SyscityError::Internal(format!("Firecrawl search failed: {}", e))
            })?;

        let status = response.status();
        if !status.is_success() {
            let body_text = response.text().await.unwrap_or_default();
            return Err(crate::error::SyscityError::Internal(format!(
                "Firecrawl search failed: HTTP {} - {}",
                status, body_text
            )));
        }

        let data: serde_json::Value = response.json().await.map_err(|e| {
            crate::error::SyscityError::Internal(format!(
                "Failed to parse Firecrawl response: {}",
                e
            ))
        })?;

        let mut results = Vec::new();
        if let Some(items) = data["data"].as_array() {
            for item in items.iter().take(limit) {
                results.push(SearchResult {
                    title: item["title"].as_str().unwrap_or("").to_string(),
                    url: item["url"].as_str().unwrap_or("").to_string(),
                    snippet: item["description"].as_str().unwrap_or("").to_string(),
                });
            }
        }

        Ok(results)
    }

    /// Search using Serper Google Search API
    async fn search_serper(
        &self,
        api_key: &str,
        query: &str,
        limit: usize,
    ) -> crate::Result<Vec<SearchResult>> {
        let body = serde_json::json!({
            "q": query,
            "num": limit.min(20),
        });

        let response = self
            .client
            .post("https://google.serper.dev/search")
            .header("X-API-KEY", api_key)
            .json(&body)
            .send()
            .await
            .map_err(|e| {
                crate::error::SyscityError::Internal(format!("Serper search failed: {}", e))
            })?;

        let status = response.status();
        if !status.is_success() {
            let body_text = response.text().await.unwrap_or_default();
            return Err(crate::error::SyscityError::Internal(format!(
                "Serper search failed: HTTP {} - {}",
                status, body_text
            )));
        }

        let data: serde_json::Value = response.json().await.map_err(|e| {
            crate::error::SyscityError::Internal(format!("Failed to parse Serper response: {}", e))
        })?;

        let mut results = Vec::new();
        if let Some(items) = data["organic"].as_array() {
            for item in items.iter().take(limit) {
                let title = item["title"].as_str().unwrap_or("").to_string();
                let url = item["link"].as_str().unwrap_or("").to_string();
                if !title.is_empty() && !url.is_empty() {
                    results.push(SearchResult {
                        title,
                        url,
                        snippet: item["snippet"].as_str().unwrap_or("").to_string(),
                    });
                }
            }
        }

        Ok(results)
    }

    /// Search using Bocha AI Web Search API
    async fn search_bocha(
        &self,
        api_key: &str,
        query: &str,
        limit: usize,
    ) -> crate::Result<Vec<SearchResult>> {
        let body = serde_json::json!({
            "query": query,
            "summary": true,
            "count": limit.min(50),
        });

        let response = self
            .client
            .post("https://api.bochaai.com/v1/web-search")
            .header("Authorization", format!("Bearer {}", api_key))
            .json(&body)
            .send()
            .await
            .map_err(|e| {
                crate::error::SyscityError::Internal(format!("Bocha search failed: {}", e))
            })?;

        let status = response.status();
        if !status.is_success() {
            let body_text = response.text().await.unwrap_or_default();
            return Err(crate::error::SyscityError::Internal(format!(
                "Bocha search failed: HTTP {} - {}",
                status, body_text
            )));
        }

        let data: serde_json::Value = response.json().await.map_err(|e| {
            crate::error::SyscityError::Internal(format!("Failed to parse Bocha response: {}", e))
        })?;

        let mut results = Vec::new();
        if let Some(items) = data["data"]["webPages"]["value"].as_array() {
            for item in items.iter().take(limit) {
                let title = item["name"].as_str().unwrap_or("").to_string();
                let url = item["url"].as_str().unwrap_or("").to_string();
                if !title.is_empty() && !url.is_empty() {
                    results.push(SearchResult {
                        title,
                        url,
                        snippet: item["snippet"].as_str().unwrap_or("").to_string(),
                    });
                }
            }
        }

        Ok(results)
    }

    /// Search via Syscity Cloud `/v1/search` (feature `cloud`). Requires a
    /// stored cloud session token; results are normalized the same shape as
    /// the local providers (title/url/snippet).
    #[cfg(feature = "cloud")]
    async fn search_cloud(
        &self,
        api_base: &str,
        query: &str,
        limit: usize,
    ) -> crate::Result<Vec<SearchResult>> {
        let token = crate::cloud::session::get_token().await.ok_or_else(|| {
            crate::error::SyscityError::Internal(
                "not signed in to Syscity Cloud — web search needs a cloud session".to_string(),
            )
        })?;
        let cfg = crate::cloud::config::CloudConfig {
            enabled: true,
            api_base: api_base.to_string(),
            redirect_base: String::new(),
            console_url: String::new(),
        };
        let resp = crate::cloud::client::CloudClient::new(&cfg, token)
            .search(query, limit as u32)
            .await?;
        let results = resp
            .get("results")
            .and_then(|r| r.as_array())
            .cloned()
            .unwrap_or_default();
        Ok(results
            .into_iter()
            .filter_map(|r| {
                Some(SearchResult {
                    title: r.get("title")?.as_str()?.to_string(),
                    url: r.get("url")?.as_str()?.to_string(),
                    snippet: r
                        .get("snippet")
                        .and_then(|s| s.as_str())
                        .unwrap_or_default()
                        .to_string(),
                })
            })
            .collect())
    }

    /// Search using custom provider
    async fn search_custom(
        &self,
        url: &str,
        api_key: &Option<String>,
        headers: &Option<std::collections::HashMap<String, String>>,
        parser: &Option<fn(&str, usize) -> Vec<SearchResult>>,
        query: &str,
        limit: usize,
    ) -> crate::Result<Vec<SearchResult>> {
        // Replace placeholders in URL
        let url = url.replace("{query}", &urlencoding::encode(query));
        let url = url.replace("{limit}", &limit.to_string());

        let mut request = self.client.get(&url);

        // Add API key if provided
        if let Some(key) = api_key {
            request = request.header("Authorization", format!("Bearer {}", key));
        }

        // Add custom headers if provided
        if let Some(hdrs) = headers {
            for (key, value) in hdrs {
                request = request.header(key, value);
            }
        }

        let response = request.send().await.map_err(|e| {
            crate::error::SyscityError::Internal(format!("Custom search request failed: {}", e))
        })?;

        if !response.status().is_success() {
            return Err(crate::error::SyscityError::Internal(format!(
                "Custom search failed: HTTP {}",
                response.status()
            )));
        }

        let body = response.text().await.map_err(|e| {
            crate::error::SyscityError::Internal(format!("Failed to read response: {}", e))
        })?;

        // Use custom parser if provided, otherwise try to parse as JSON
        let results = if let Some(parser_fn) = parser {
            parser_fn(&body, limit)
        } else {
            // Default JSON parsing - assumes format similar to { "results": [{ "title":
            // "...", "url": "...", "snippet": "..." }] }
            Self::parse_generic_json_results(&body, limit)
        };

        Ok(results)
    }

    /// Parse generic JSON search results
    fn parse_generic_json_results(json: &str, limit: usize) -> Vec<SearchResult> {
        let mut results = Vec::new();

        if let Ok(value) = serde_json::from_str::<serde_json::Value>(json) {
            // Try common result paths
            let results_array = value
                .get("results")
                .and_then(|v| v.as_array())
                .or_else(|| value.get("items").and_then(|v| v.as_array()))
                .or_else(|| value.get("data").and_then(|v| v.as_array()));

            if let Some(items) = results_array {
                for item in items.iter().take(limit) {
                    let title = item
                        .get("title")
                        .or_else(|| item.get("name"))
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string();
                    let url = item
                        .get("url")
                        .or_else(|| item.get("link"))
                        .or_else(|| item.get("href"))
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string();
                    let snippet = item
                        .get("snippet")
                        .or_else(|| item.get("description"))
                        .or_else(|| item.get("summary"))
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string();

                    if !title.is_empty() && !url.is_empty() {
                        results.push(SearchResult { title, url, snippet });
                    }
                }
            }
        }

        results
    }

    /// Clean HTML entities and tags from text
    fn clean_html(html: &str) -> String {
        // First, strip actual HTML tags (but not entity-encoded ones)
        let mut result = String::new();
        let mut in_tag = false;
        for ch in html.chars() {
            match ch {
                '<' => in_tag = true,
                '>' if in_tag => in_tag = false,
                _ if !in_tag => result.push(ch),
                _ => {}
            }
        }

        // Then decode HTML entities
        result = result.replace("&amp;", "&");
        result = result.replace("&lt;", "<");
        result = result.replace("&gt;", ">");
        result = result.replace("&quot;", "\"");
        result = result.replace("&#39;", "'");
        result = result.replace("&nbsp;", " ");

        result.trim().to_string()
    }
}

/// Search result
#[derive(Debug, Clone)]
pub struct SearchResult {
    /// Result title
    pub title: String,
    /// Result URL
    pub url: String,
    /// Result snippet
    pub snippet: String,
}

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

/// Find a substring in `text` using ASCII case-insensitive comparison.
///
#[cfg(test)]
mod tests {
    // Tests assert against fallible bind/send/read results; unwrapping keeps
    // the failure-path assertions readable (same allowance as
    // `sandbox_interceptor`).
    #![allow(clippy::unwrap_used)]

    use super::*;
    use crate::browser::NavigationPolicy;

    /// Canned per-path raw HTTP responses served by [`spawn_http_server`].
    type Routes = std::collections::HashMap<String, String>;

    /// Build a raw HTTP/1.1 response with correct Content-Length.
    fn http_response(status_line: &str, headers: &[(&str, &str)], body: &str) -> String {
        let mut response = format!("{status_line}\r\n");
        for (name, value) in headers {
            response.push_str(&format!("{name}: {value}\r\n"));
        }
        response.push_str(&format!("Content-Length: {}\r\n", body.len()));
        response.push_str("Connection: close\r\n\r\n");
        response.push_str(body);
        response
    }

    /// Serve canned raw HTTP responses on an ephemeral loopback port.
    ///
    /// Each accepted connection is handled on its own task: read the request
    /// head, look up the path, write the canned response, then close. A
    /// `{port}` placeholder inside a canned response is replaced with the
    /// server's own port at serve time (redirects need absolute targets).
    async fn spawn_http_server(routes: Routes) -> u16 {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let routes = std::sync::Arc::new(routes);
        tokio::spawn(async move {
            loop {
                match listener.accept().await {
                    Ok((socket, _)) => {
                        let routes = std::sync::Arc::clone(&routes);
                        tokio::spawn(serve_connection(socket, routes, port));
                    }
                    Err(err) => {
                        debug!("test server accept failed, stopping: {}", err);
                        break;
                    }
                }
            }
        });
        port
    }

    /// Handle one connection of the canned-response test server.
    async fn serve_connection(
        mut socket: tokio::net::TcpStream,
        routes: std::sync::Arc<Routes>,
        port: u16,
    ) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let mut buffer = Vec::new();
        let mut chunk = [0u8; 1024];
        loop {
            match socket.read(&mut chunk).await {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    buffer.extend_from_slice(&chunk[..n]);
                    if buffer.windows(4).any(|window| window == b"\r\n\r\n") {
                        break;
                    }
                }
            }
        }
        let request = String::from_utf8_lossy(&buffer);
        let path = request.split_whitespace().nth(1).unwrap_or("/").to_string();
        let path = path.split('?').next().unwrap_or("/").to_string();
        let response = routes.get(&path).cloned().unwrap_or_else(|| {
            "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_string()
        });
        let response = response.replace("{port}", &port.to_string());
        if let Err(err) = socket.write_all(response.as_bytes()).await {
            debug!("test server write failed: {}", err);
        }
        if let Err(err) = socket.shutdown().await {
            debug!("test server shutdown failed: {}", err);
        }
    }

    #[test]
    fn test_web_fetch_tool_creation() {
        let tool = WebFetchTool::new();
        assert_eq!(tool.name(), "web_fetch");
        assert!(!tool.description().is_empty());
    }

    #[test]
    fn test_web_search_tool_creation() {
        let tool = WebSearchTool::new();
        assert_eq!(tool.name(), "web_search");
        assert!(!tool.description().is_empty());
    }

    #[test]
    fn test_html_to_markdown() {
        let html = r#"<h1>Title</h1><p>This is <strong>bold</strong> and <em>italic</em>.</p>"#;
        let markdown = WebFetchTool::html_to_markdown(html);
        assert!(markdown.contains("# Title"));
        assert!(markdown.contains("**bold**"));
        assert!(markdown.contains("_italic_"));
    }

    #[test]
    fn test_is_html() {
        assert!(WebFetchTool::is_html(Some("text/html")));
        assert!(WebFetchTool::is_html(Some("text/html; charset=utf-8")));
        assert!(WebFetchTool::is_html(Some("application/xhtml+xml")));
        assert!(!WebFetchTool::is_html(Some("text/plain")));
        assert!(!WebFetchTool::is_html(Some("application/json")));
        assert!(!WebFetchTool::is_html(None));
    }

    #[test]
    fn test_truncate_content() {
        let long_content = "a".repeat(MAX_CONTENT_SIZE + 100);
        let truncated = WebFetchTool::truncate_content(long_content);
        assert!(truncated.contains("truncated"));
        assert!(truncated.len() <= MAX_CONTENT_SIZE + 100);
    }

    #[test]
    fn test_parse_duckduckgo_results() {
        let html = r#"
            <div class="result">
                <a rel="nofollow" href="http://example.com">Test Title</a>
                <a class="result__snippet">Test snippet here</a>
            </div>
            <div class="result">
                <a rel="nofollow" href="http://example2.com">Second Title</a>
                <a class="result__snippet">Second snippet</a>
            </div>
        "#;

        let results = WebSearchTool::parse_duckduckgo_results(html, 10);
        assert_eq!(results.len(), 2);
        assert_eq!(results[0].title, "Test Title");
        assert_eq!(results[0].url, "http://example.com");
        assert_eq!(results[0].snippet, "Test snippet here");
    }

    #[test]
    fn test_clean_html() {
        assert_eq!(WebSearchTool::clean_html("Hello &amp; World"), "Hello & World");
        assert_eq!(WebSearchTool::clean_html("&lt;tag&gt;"), "<tag>");
        assert_eq!(WebSearchTool::clean_html("<b>Bold</b>"), "Bold");
    }

    // ── Error-code taxonomy ─────────────────────────────────────────────────

    #[test]
    fn test_error_code_wire_values() {
        assert_eq!(WebErrorCode::ConfiguredMissing.as_str(), "CONFIGURED_MISSING");
        assert_eq!(WebErrorCode::NotConfigured.as_str(), "NOT_CONFIGURED");
        assert_eq!(WebErrorCode::Unavailable.as_str(), "UNAVAILABLE");
        assert_eq!(WebErrorCode::Ambiguous.as_str(), "AMBIGUOUS");
    }

    #[test]
    fn test_error_result_carries_code_merged_with_extra() {
        let result =
            WebErrorCode::Unavailable.error_result("boom", serde_json::json!({ "url": "u" }));
        assert!(!result.success);
        assert_eq!(result.data.as_ref().unwrap()["code"], "UNAVAILABLE");
        assert_eq!(result.data.as_ref().unwrap()["url"], "u");
    }

    fn attempt(provider: &'static str, kind: AttemptKind) -> ProviderAttempt {
        match kind {
            AttemptKind::MissingValue { field } => ProviderAttempt::missing_value(provider, field),
            AttemptKind::Unavailable { detail } => ProviderAttempt::unavailable(provider, detail),
        }
    }

    #[test]
    fn test_classify_attempts_single_causes() {
        assert_eq!(
            classify_attempts(&[attempt(
                "brave",
                AttemptKind::MissingValue { field: "api_key" }
            )]),
            WebErrorCode::ConfiguredMissing
        );
        assert_eq!(
            classify_attempts(&[attempt(
                "duckduckgo",
                AttemptKind::Unavailable { detail: "timeout".into() }
            )]),
            WebErrorCode::Unavailable
        );
    }

    #[test]
    fn test_classify_attempts_shared_class_keeps_code() {
        let attempts = vec![
            attempt("tavily", AttemptKind::MissingValue { field: "api_key" }),
            attempt("serper", AttemptKind::MissingValue { field: "api_key" }),
        ];
        assert_eq!(classify_attempts(&attempts), WebErrorCode::ConfiguredMissing);

        let attempts = vec![
            attempt("duckduckgo", AttemptKind::Unavailable { detail: "dns".into() }),
            attempt("exa", AttemptKind::Unavailable { detail: "5xx".into() }),
        ];
        assert_eq!(classify_attempts(&attempts), WebErrorCode::Unavailable);
    }

    #[test]
    fn test_classify_attempts_mixed_classes_are_ambiguous() {
        let attempts = vec![
            attempt("brave", AttemptKind::MissingValue { field: "api_key" }),
            attempt("duckduckgo", AttemptKind::Unavailable { detail: "refused".into() }),
        ];
        assert_eq!(classify_attempts(&attempts), WebErrorCode::Ambiguous);
    }

    #[test]
    fn test_classify_attempts_empty_is_not_configured() {
        assert_eq!(classify_attempts(&[]), WebErrorCode::NotConfigured);
    }

    #[test]
    fn test_missing_credential_detection() {
        assert_eq!(missing_credential(&SearchProvider::DuckDuckGo), None);
        assert_eq!(
            missing_credential(&SearchProvider::Brave { api_key: String::new() }),
            Some("api_key")
        );
        assert_eq!(
            missing_credential(&SearchProvider::Tavily { api_key: "  ".to_string() }),
            Some("api_key")
        );
        assert_eq!(
            missing_credential(&SearchProvider::Serper { api_key: "key".to_string() }),
            None
        );
        assert_eq!(
            missing_credential(&SearchProvider::Custom {
                url: "  ".to_string(),
                api_key: None,
                headers: None,
                result_parser: None
            }),
            Some("url")
        );
    }

    // ── Redirect helpers ────────────────────────────────────────────────────

    #[test]
    fn test_is_followable_redirect() {
        use reqwest::StatusCode;
        assert!(WebFetchTool::is_followable_redirect(StatusCode::MOVED_PERMANENTLY));
        assert!(WebFetchTool::is_followable_redirect(StatusCode::FOUND));
        assert!(WebFetchTool::is_followable_redirect(StatusCode::SEE_OTHER));
        assert!(WebFetchTool::is_followable_redirect(StatusCode::TEMPORARY_REDIRECT));
        assert!(WebFetchTool::is_followable_redirect(StatusCode::PERMANENT_REDIRECT));
        // 300 and 304 are 3xx but carry no followable relocation target here.
        assert!(!WebFetchTool::is_followable_redirect(StatusCode::MULTIPLE_CHOICES));
        assert!(!WebFetchTool::is_followable_redirect(StatusCode::NOT_MODIFIED));
        assert!(!WebFetchTool::is_followable_redirect(StatusCode::OK));
    }

    #[test]
    fn test_resolve_redirect_joins_relative_targets() {
        let base = reqwest::Url::parse("https://example.com/a/b").unwrap();
        assert_eq!(
            WebFetchTool::resolve_redirect(&base, "/c")
                .unwrap()
                .as_str(),
            "https://example.com/c"
        );
        assert_eq!(
            WebFetchTool::resolve_redirect(&base, "d").unwrap().as_str(),
            "https://example.com/a/d"
        );
        assert_eq!(
            WebFetchTool::resolve_redirect(&base, "https://other.org/x")
                .unwrap()
                .as_str(),
            "https://other.org/x"
        );
    }

    #[test]
    fn test_resolve_redirect_rejects_disallowed_targets() {
        let base = reqwest::Url::parse("https://example.com/").unwrap();
        assert!(WebFetchTool::resolve_redirect(&base, "ftp://example.com/x").is_none());
        assert!(WebFetchTool::resolve_redirect(&base, "file:///etc/passwd").is_none());
        // Genuinely unparseable Location values are refused, not followed.
        assert!(WebFetchTool::resolve_redirect(&base, "http://[").is_none());
    }

    // ── Fetch: SSRF guard and redirect revalidation ─────────────────────────

    fn permissive_fetcher() -> WebFetchTool {
        WebFetchTool::new().with_navigation_policy(NavigationPolicy::permissive())
    }

    async fn run_fetch(tool: &WebFetchTool, url: String) -> ToolExecutionResult {
        tool.execute(serde_json::json!({ "url": url }), &ToolContext::default())
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn fetch_follows_redirect_chain_to_final_body() {
        let port = spawn_http_server(
            [
                ("/a".to_string(), http_response("HTTP/1.1 302 Found", &[("Location", "/b")], "")),
                (
                    "/b".to_string(),
                    http_response(
                        "HTTP/1.1 200 OK",
                        &[("Content-Type", "text/plain")],
                        "final body",
                    ),
                ),
            ]
            .into_iter()
            .collect(),
        )
        .await;

        let result = run_fetch(&permissive_fetcher(), format!("http://127.0.0.1:{port}/a")).await;
        assert!(result.success, "expected success, got {:?}", result.error);
        assert!(result.output.contains("final body"));
        let data = result.data.unwrap();
        assert_eq!(data["status"], 200);
        assert_eq!(data["redirects"], 1);
        assert_eq!(data["final_url"], format!("http://127.0.0.1:{port}/b"));
    }

    #[tokio::test]
    async fn fetch_returns_non_2xx_as_success_result() {
        let port = spawn_http_server(
            [(
                "/missing".to_string(),
                http_response(
                    "HTTP/1.1 404 Not Found",
                    &[("Content-Type", "text/plain")],
                    "nothing here",
                ),
            )]
            .into_iter()
            .collect(),
        )
        .await;

        let result =
            run_fetch(&permissive_fetcher(), format!("http://127.0.0.1:{port}/missing")).await;
        assert!(result.success, "non-2xx must be a successful tool result");
        assert!(result.error.is_none());
        assert!(result.output.contains("HTTP 404 Not Found"));
        assert!(result.output.contains("nothing here"));
        let data = result.data.unwrap();
        assert_eq!(data["status"], 404);
        assert_eq!(data["http_error"], true);
    }

    #[tokio::test]
    async fn fetch_redirect_loop_reports_unavailable() {
        let loop_response = http_response("HTTP/1.1 302 Found", &[("Location", "/y")], "");
        let port = spawn_http_server(
            [
                ("/x".to_string(), loop_response.clone()),
                ("/y".to_string(), loop_response),
            ]
            .into_iter()
            .collect(),
        )
        .await;

        let result = run_fetch(&permissive_fetcher(), format!("http://127.0.0.1:{port}/x")).await;
        assert!(!result.success, "redirect loops must fail");
        assert!(result
            .error
            .as_deref()
            .unwrap_or_default()
            .contains("redirect"));
        assert_eq!(result.data.unwrap()["code"], "UNAVAILABLE");
    }

    #[tokio::test]
    async fn fetch_revalidates_every_redirect_hop_against_guard() {
        // The initial target (127.0.0.1) passes this policy; the redirect
        // target hostname "localhost" does not. If the guard only ran on the
        // first hop, the second request would go through.
        let policy = NavigationPolicy {
            allow_private: true,
            allowed_hostnames: Vec::new(),
            blocked_hostnames: vec!["localhost".to_string()],
        };
        let port = spawn_http_server(
            [(
                "/go".to_string(),
                http_response(
                    "HTTP/1.1 302 Found",
                    &[("Location", "http://localhost:{port}/end")],
                    "",
                ),
            )]
            .into_iter()
            .collect(),
        )
        .await;

        let tool = WebFetchTool::new().with_navigation_policy(policy);
        let result = run_fetch(&tool, format!("http://127.0.0.1:{port}/go")).await;
        assert!(!result.success, "hop 2 must be blocked by the SSRF guard");
        let message = result.error.as_deref().unwrap_or_default();
        assert!(message.contains("SSRF guard"), "unexpected message: {}", message);
        assert!(message.contains("localhost"), "unexpected message: {}", message);
        assert_eq!(result.data.unwrap()["code"], "UNAVAILABLE");
    }

    #[tokio::test]
    async fn fetch_blocks_private_target_on_first_hop() {
        let port = spawn_http_server(std::collections::HashMap::new()).await;
        // Default policy is restrictive: private targets are refused before
        // any request is issued.
        let result = run_fetch(&WebFetchTool::new(), format!("http://127.0.0.1:{port}/a")).await;
        assert!(!result.success);
        let message = result.error.as_deref().unwrap_or_default();
        assert!(message.contains("SSRF guard"), "unexpected message: {}", message);
        assert_eq!(result.data.unwrap()["code"], "UNAVAILABLE");
    }

    // ── Search: selection-failure classification ────────────────────────────

    /// Bind an ephemeral port and immediately drop it so connections are
    /// deterministically refused.
    async fn dead_port() -> u16 {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);
        port
    }

    async fn run_search(tool: &WebSearchTool) -> ToolExecutionResult {
        tool.execute(serde_json::json!({ "query": "syscity" }), &ToolContext::default())
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn search_empty_provider_list_reports_not_configured() {
        let tool = WebSearchTool::new().with_providers(Vec::new());
        let result = run_search(&tool).await;
        assert!(!result.success);
        assert_eq!(result.data.unwrap()["code"], "NOT_CONFIGURED");
    }

    #[tokio::test]
    async fn search_provider_without_key_reports_configured_missing() {
        let tool =
            WebSearchTool::new().with_provider(SearchProvider::Brave { api_key: String::new() });
        let result = run_search(&tool).await;
        assert!(!result.success);
        let data = result.data.unwrap();
        assert_eq!(data["code"], "CONFIGURED_MISSING");
        assert_eq!(data["attempts"][0]["provider"], "brave");
        assert_eq!(data["attempts"][0]["outcome"], "configured_missing");
    }

    #[tokio::test]
    async fn search_dead_endpoint_reports_unavailable() {
        let port = dead_port().await;
        let tool = WebSearchTool::new().with_provider(SearchProvider::Custom {
            url: format!("http://127.0.0.1:{}/{{query}}", port),
            api_key: None,
            headers: None,
            result_parser: None,
        });
        let result = run_search(&tool).await;
        assert!(!result.success);
        let data = result.data.unwrap();
        assert_eq!(data["code"], "UNAVAILABLE");
        assert_eq!(data["attempts"][0]["outcome"], "unavailable");
    }

    #[tokio::test]
    async fn search_mixed_failure_classes_report_ambiguous() {
        let port = dead_port().await;
        let tool = WebSearchTool::new().with_providers(vec![
            SearchProvider::Brave { api_key: String::new() },
            SearchProvider::Custom {
                url: format!("http://127.0.0.1:{}/{{query}}", port),
                api_key: None,
                headers: None,
                result_parser: None,
            },
        ]);
        let result = run_search(&tool).await;
        assert!(!result.success);
        let data = result.data.unwrap();
        assert_eq!(data["code"], "AMBIGUOUS");
        assert_eq!(data["attempts"].as_array().unwrap().len(), 2);
    }
}
