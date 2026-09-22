//! Web search tool: provider configuration and result parsing.

use super::{SearchResult, WEB_TIMEOUT};

mod providers;
#[cfg(test)]
mod tests;
mod tool;

/// Web search tool
#[derive(Debug)]
pub struct WebSearchTool {
    /// HTTP client
    client: reqwest::Client,
    /// Search providers to try in order (fallback).
    /// Wrapped in Arc<RwLock<>> so hot-reload can update providers without
    /// rebuilding the entire tool registry.
    providers: std::sync::Arc<tokio::sync::RwLock<Vec<SearchProvider>>>,
    /// Secret-store handle for the cloud search provider's session token.
    /// `None` until wired by the gateway (`with_secrets`).
    secrets: Option<std::sync::Arc<crate::secrets::SecretStoreHandle>>,
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
            secrets: None,
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

    /// Attach the secret-store handle used by the cloud search provider.
    pub fn with_secrets(
        mut self,
        secrets: std::sync::Arc<crate::secrets::SecretStoreHandle>,
    ) -> Self {
        self.secrets = Some(secrets);
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
