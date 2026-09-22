//! Per-provider search adapters: the fallback walk dispatches into these.

use tracing::debug;

use super::super::SearchResult;
use super::{provider_name, SearchProvider, WebSearchTool};

/// Per-provider request timeout. Each provider gets a strict, shorter budget
/// so that a fallback chain has time to try more than one backend before the
/// tool-level wrapper times out.
const PROVIDER_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(12);

impl WebSearchTool {
    /// Execute a search against a single provider.
    /// Each provider request is wrapped with its own timeout so that a slow
    /// backend does not consume the entire tool-level budget.
    pub(super) async fn search_with_provider(
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
        let secrets = self.secrets.as_ref().ok_or_else(|| {
            crate::error::SyscityError::Internal(
                "cloud web search is not wired to the secret store".to_string(),
            )
        })?;
        let token = crate::cloud::session::get_token(secrets)
            .await
            .ok_or_else(|| {
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
        let resp = crate::cloud::client::CloudClient::new(&cfg, token, secrets.clone())
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
}
