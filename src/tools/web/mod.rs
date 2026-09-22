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

mod fetch;
mod search;
/// Find a substring in `text` using ASCII case-insensitive comparison.
///
#[cfg(test)]
mod tests;

pub use fetch::WebFetchTool;
pub use search::{SearchProvider, WebSearchTool};

use serde_json::Value;

use crate::tools::ToolExecutionResult;
/// Maximum content size to fetch (100KB)
const MAX_CONTENT_SIZE: usize = 100 * 1024;

/// Default timeout for web requests
const WEB_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(15);

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
