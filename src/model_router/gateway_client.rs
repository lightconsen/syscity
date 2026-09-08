//! GatewayClient — unified HTTP client for LLM provider backends
//!
//! Abstracts HTTP request building, authentication, retry logic, and
//! optional TLS fingerprint verification. Providers can delegate their
//! raw HTTP plumbing to a `GatewayClient` instead of each rolling their
//! own `reqwest` boilerplate.
//!
//! # Credential priority chain
//!
//! 1. Environment variable (`SYSCITY_PROVIDER_{NAME}_KEY`)
//! 2. Bearer / OAuth2 token from auth profile
//! 3. API key list from config
//! 4. Single API key from config

use std::sync::Arc;
use std::time::Duration;

use reqwest::header::HeaderMap;
use serde::{de::DeserializeOwned, Serialize};
use tokio::sync::RwLock;
use tracing::warn;

use crate::model_router::Credential;

/// Trait abstracting an HTTP client for a single LLM backend.
///
/// Implementors handle connection pooling, auth header injection,
/// request/response serialization, and retry/backoff.
#[async_trait::async_trait]
pub trait GatewayClient: Send + Sync {
    /// POST a JSON body and deserialize the response.
    async fn post_json<T: Serialize + Send + Sync, R: DeserializeOwned + Send>(
        &self,
        path: &str,
        body: &T,
    ) -> crate::Result<R>;

    /// POST a JSON body and return the raw text (for streaming endpoints).
    async fn post_json_text<T: Serialize + Send + Sync>(
        &self,
        path: &str,
        body: &T,
    ) -> crate::Result<String>;

    /// POST a JSON body and return the raw `Response` for byte-level
    /// streaming consumption.
    ///
    /// The caller owns the response and is responsible for consuming the
    /// byte stream (e.g. via `response.bytes_stream()`). Auth headers,
    /// extra headers, retries, and timeouts are handled internally.
    async fn post_json_streaming<T: Serialize + Send + Sync>(
        &self,
        path: &str,
        body: &T,
    ) -> crate::Result<reqwest::Response>;

    /// Update the active credential (e.g. after token refresh or key rotation).
    async fn set_credential(&self, credential: Credential);

    /// Change the request timeout.
    async fn set_timeout(&self, duration: Duration);

    /// GET and return the raw `Response` for status checking (e.g. health
    /// checks). Auth headers, extra headers, retries, and timeouts are handled
    /// internally.
    async fn get(&self, path: &str) -> crate::Result<reqwest::Response>;

    /// Return the configured base URL.
    fn base_url(&self) -> &str;
}

/// HTTP-based `GatewayClient` using `reqwest`.
pub struct HttpGatewayClient {
    pub(crate) inner: reqwest::Client,
    base_url: String,
    pub(crate) credential: Arc<RwLock<Credential>>,
    timeout: RwLock<Duration>,
    max_retries: u32,
    retry_delay: Duration,
    /// Optional per-client token-bucket rate limiter.
    rate_limiter: Option<std::sync::Arc<crate::security::RateLimiter>>,
    /// When `Some(name)`, `ApiKey` credentials use `{name}: {key}` instead of
    /// `Authorization: Bearer {key}`. `BearerToken` / `OAuth2` always use
    /// `Authorization: Bearer {token}`.
    pub(crate) api_key_header: Option<String>,
    /// Static headers injected on every request (User-Agent, version headers,
    /// etc.).
    pub(crate) extra_headers: HeaderMap,
}

impl std::fmt::Debug for HttpGatewayClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HttpGatewayClient")
            .field("base_url", &self.base_url)
            .field("max_retries", &self.max_retries)
            .field("rate_limiter", &self.rate_limiter.is_some())
            .finish()
    }
}

impl HttpGatewayClient {
    /// Create a new client.
    pub fn new(
        base_url: impl Into<String>,
        credential: Credential,
        timeout: Duration,
    ) -> crate::Result<Self> {
        let client = reqwest::Client::builder()
            .timeout(timeout)
            .build()
            .map_err(|e| crate::error::SyscityError::ExternalService {
                source: format!("Failed to build HTTP client: {}", e),
                cause: None,
            })?;

        Ok(Self {
            inner: client,
            base_url: base_url.into(),
            credential: Arc::new(RwLock::new(credential)),
            timeout: RwLock::new(timeout),
            max_retries: 3,
            retry_delay: Duration::from_millis(500),
            rate_limiter: None,
            api_key_header: None,
            extra_headers: HeaderMap::new(),
        })
    }

    /// Builder: set max retries.
    pub fn with_max_retries(mut self, n: u32) -> Self {
        self.max_retries = n;
        self
    }

    /// Builder: set retry delay.
    pub fn with_retry_delay(mut self, d: Duration) -> Self {
        self.retry_delay = d;
        self
    }

    /// Builder: attach a token-bucket rate limiter to this client.
    pub fn with_rate_limiter(
        mut self,
        limiter: std::sync::Arc<crate::security::RateLimiter>,
    ) -> Self {
        self.rate_limiter = Some(limiter);
        self
    }

    /// Builder: set a custom header name for ApiKey credentials.
    ///
    /// When set, `ApiKey` credentials produce `{name}: {key}` instead of
    /// `Authorization: Bearer {key}`. Useful for providers like Anthropic
    /// (`x-api-key`) and Gemini (`x-goog-api-key`).
    pub fn with_api_key_header(mut self, name: impl Into<String>) -> Self {
        self.api_key_header = Some(name.into());
        self
    }

    /// Builder: set static extra headers injected on every request.
    pub fn with_extra_headers(mut self, headers: HeaderMap) -> Self {
        self.extra_headers = headers;
        self
    }

    /// Return the auth header name and value for a given credential.
    ///
    /// - `ApiKey` + `api_key_header` is `Some` ⇒ `({name}, {key})`
    /// - Everything else ⇒ `("Authorization", "Bearer {token}")`
    pub(crate) fn auth_for_credential(&self, credential: &Credential) -> (&str, String) {
        match (credential, &self.api_key_header) {
            (Credential::ApiKey { key }, Some(header_name)) => (header_name.as_str(), key.clone()),
            _ => ("Authorization", credential.authorization_header()),
        }
    }

    /// Refresh the credential if it's an OAuth2 token that is expired or
    /// expiring soon.
    pub(crate) async fn refresh_credential_if_needed(&self) -> crate::Result<()> {
        let mut cred = self.credential.write().await;
        cred.refresh_if_needed(&self.inner).await
    }

    /// Build the full URL for a path.
    fn url(&self, path: &str) -> String {
        if path.starts_with("http://") || path.starts_with("https://") {
            path.to_string()
        } else {
            format!("{}{}", self.base_url.trim_end_matches('/'), path)
        }
    }

    /// Execute a request with auth, retry, and timeout handling.
    ///
    /// Retries only on transient failures: network timeouts/connection errors
    /// and 5xx server responses. 4xx client errors are returned immediately.
    async fn execute_with_retry<F, Fut>(&self, operation: F) -> crate::Result<reqwest::Response>
    where
        F: Fn() -> Fut + Send,
        Fut: std::future::Future<Output = crate::Result<reqwest::Response>> + Send,
    {
        // Token-bucket rate limit check
        if let Some(ref limiter) = self.rate_limiter {
            let user_id = crate::security::UserId::new(&self.base_url);
            match limiter.check(&user_id).await {
                crate::security::RateLimitResult::Allowed { .. } => {}
                crate::security::RateLimitResult::Denied { retry_after_secs } => {
                    return Err(crate::error::SyscityError::ExternalService {
                        source: format!("Rate limited: retry after {} seconds", retry_after_secs),
                        cause: None,
                    });
                }
            }
        }

        let mut last_error = None;

        for attempt in 0..=self.max_retries {
            match operation().await {
                Ok(resp) if resp.status().is_success() => return Ok(resp),
                Ok(resp) => {
                    let status = resp.status();
                    let body = resp.text().await.unwrap_or_default();
                    let err = crate::error::SyscityError::ExternalService {
                        source: format!("HTTP {}: {}", status, body),
                        cause: None,
                    };

                    // 4xx client errors are not retried. 501 (Not
                    // Implemented) is also permanent — a capability gap, not
                    // a transient server fault.
                    if status.is_client_error()
                        || status.as_u16() == 501
                        || attempt == self.max_retries
                    {
                        return Err(err);
                    }

                    last_error = Some(err);
                }
                Err(e) => {
                    if !is_retryable_error(&e) || attempt == self.max_retries {
                        return Err(e);
                    }
                    last_error = Some(e);
                }
            }

            if attempt < self.max_retries {
                if let Some(ref err) = last_error {
                    let delay = self.retry_delay * 2_u32.pow(attempt);
                    warn!(
                        "Request failed (attempt {}/{}), retrying in {:?}: {}",
                        attempt + 1,
                        self.max_retries + 1,
                        delay,
                        err
                    );
                    tokio::time::sleep(delay).await;
                }
            }
        }

        Err(last_error.unwrap_or_else(|| crate::error::SyscityError::ExternalService {
            source: "All retry attempts exhausted".to_string(),
            cause: None,
        }))
    }
}

/// Whether a transport / connection-level error is worth retrying.
///
/// This function is called only for transport failures (not HTTP response
/// errors) — [`execute_with_retry`] handles 5xx server errors directly in
/// the status-code branch before reaching here.  The name focuses on the
/// "should we retry?" question rather than the error category.
fn is_retryable_error(e: &crate::error::SyscityError) -> bool {
    match e {
        crate::error::SyscityError::Http(req_err) => req_err.is_timeout() || req_err.is_connect(),
        crate::error::SyscityError::ExternalService { source, .. } => {
            let s = source.to_lowercase();
            s.contains("timeout")
                || s.contains("timed out")
                || s.contains("connection")
                || s.contains("overloaded")
                || s.contains("service unavailable")
        }
        _ => false,
    }
}

#[async_trait::async_trait]
impl GatewayClient for HttpGatewayClient {
    async fn post_json<T: Serialize + Send + Sync, R: DeserializeOwned + Send>(
        &self,
        path: &str,
        body: &T,
    ) -> crate::Result<R> {
        let url = self.url(path);
        let credential = self.credential.read().await.clone();
        let (auth_name, auth_value) = self.auth_for_credential(&credential);
        let timeout = *self.timeout.read().await;
        let extra = self.extra_headers.clone();

        let resp = self
            .execute_with_retry(|| {
                let url = url.clone();
                let auth_value = auth_value.clone();
                let extra = extra.clone();
                async move {
                    let mut builder = self
                        .inner
                        .post(&url)
                        .timeout(timeout)
                        .header(auth_name, auth_value)
                        .header("Content-Type", "application/json");

                    for (name, value) in extra.iter() {
                        builder = builder.header(name, value);
                    }

                    builder
                        .json(body)
                        .send()
                        .await
                        .map_err(crate::error::SyscityError::Http)
                }
            })
            .await?;

        resp.json::<R>()
            .await
            .map_err(|e| crate::error::SyscityError::ExternalService {
                source: format!("Failed to deserialize response: {}", e),
                cause: None,
            })
    }

    async fn post_json_text<T: Serialize + Send + Sync>(
        &self,
        path: &str,
        body: &T,
    ) -> crate::Result<String> {
        let url = self.url(path);
        let credential = self.credential.read().await.clone();
        let (auth_name, auth_value) = self.auth_for_credential(&credential);
        let timeout = *self.timeout.read().await;
        let extra = self.extra_headers.clone();

        let resp = self
            .execute_with_retry(|| {
                let url = url.clone();
                let auth_value = auth_value.clone();
                let extra = extra.clone();
                async move {
                    let mut builder = self
                        .inner
                        .post(&url)
                        .timeout(timeout)
                        .header(auth_name, auth_value)
                        .header("Content-Type", "application/json");

                    for (name, value) in extra.iter() {
                        builder = builder.header(name, value);
                    }

                    builder
                        .json(body)
                        .send()
                        .await
                        .map_err(crate::error::SyscityError::Http)
                }
            })
            .await?;

        resp.text()
            .await
            .map_err(|e| crate::error::SyscityError::ExternalService {
                source: format!("Failed to read response body: {}", e),
                cause: None,
            })
    }

    async fn post_json_streaming<T: Serialize + Send + Sync>(
        &self,
        path: &str,
        body: &T,
    ) -> crate::Result<reqwest::Response> {
        let url = self.url(path);
        let credential = self.credential.read().await.clone();
        let (auth_name, auth_value) = self.auth_for_credential(&credential);
        let extra = self.extra_headers.clone();

        self.execute_with_retry(|| {
            let url = url.clone();
            let auth_value = auth_value.clone();
            let extra = extra.clone();
            async move {
                let mut builder = self
                    .inner
                    .post(&url)
                    .header(auth_name, auth_value)
                    .header("Content-Type", "application/json")
                    .header("Accept", "text/event-stream");

                for (name, value) in extra.iter() {
                    builder = builder.header(name, value);
                }

                builder
                    .json(body)
                    .send()
                    .await
                    .map_err(crate::error::SyscityError::Http)
            }
        })
        .await
    }

    async fn get(&self, path: &str) -> crate::Result<reqwest::Response> {
        let url = self.url(path);
        let credential = self.credential.read().await.clone();
        let (auth_name, auth_value) = self.auth_for_credential(&credential);
        let timeout = *self.timeout.read().await;
        let extra = self.extra_headers.clone();

        self.execute_with_retry(|| {
            let url = url.clone();
            let auth_value = auth_value.clone();
            let extra = extra.clone();
            async move {
                let mut builder = self
                    .inner
                    .get(&url)
                    .timeout(timeout)
                    .header(auth_name, auth_value);

                for (name, value) in extra.iter() {
                    builder = builder.header(name, value);
                }

                builder
                    .send()
                    .await
                    .map_err(crate::error::SyscityError::Http)
            }
        })
        .await
    }

    async fn set_credential(&self, credential: Credential) {
        let mut cred = self.credential.write().await;
        *cred = credential;
    }

    async fn set_timeout(&self, duration: Duration) {
        let mut t = self.timeout.write().await;
        *t = duration;
    }

    fn base_url(&self) -> &str {
        &self.base_url
    }
}

#[cfg(test)]
mod tests {
    use serde::Serialize;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::*;

    #[derive(Serialize)]
    struct TestBody {
        msg: String,
    }

    #[test]
    fn test_url_building() {
        let client = HttpGatewayClient::new(
            "https://api.example.com/v1",
            Credential::api_key("test"),
            Duration::from_secs(30),
        )
        .unwrap();
        assert_eq!(client.url("/chat/completions"), "https://api.example.com/v1/chat/completions");
        assert_eq!(client.url("https://other.com/path"), "https://other.com/path");
    }

    #[test]
    fn test_builder_chain() {
        let client = HttpGatewayClient::new(
            "https://api.example.com",
            Credential::api_key("test"),
            Duration::from_secs(30),
        )
        .unwrap()
        .with_max_retries(5)
        .with_retry_delay(Duration::from_secs(1));

        assert_eq!(client.max_retries, 5);
        assert_eq!(client.retry_delay, Duration::from_secs(1));
    }

    #[test]
    fn test_rate_limiter_builder() {
        let limiter = std::sync::Arc::new(crate::security::RateLimiter::new(10, 1.0));
        let client = HttpGatewayClient::new(
            "https://api.example.com",
            Credential::api_key("test"),
            Duration::from_secs(30),
        )
        .unwrap()
        .with_rate_limiter(limiter);

        assert!(client.rate_limiter.is_some());
    }

    #[tokio::test]
    async fn set_timeout_is_applied_to_requests() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/slow"))
            .respond_with(ResponseTemplate::new(200).set_delay(Duration::from_millis(200)))
            .mount(&server)
            .await;

        let client = HttpGatewayClient::new(
            server.uri(),
            Credential::api_key("test"),
            Duration::from_secs(30),
        )
        .unwrap();
        client.set_timeout(Duration::from_millis(50)).await;

        let result = client
            .post_json::<TestBody, serde_json::Value>("/slow", &TestBody { msg: "hi".to_string() })
            .await;

        match result {
            Err(crate::error::SyscityError::Http(e)) if e.is_timeout() => {}
            other => panic!("expected timeout Http error, got {:?}", other),
        }
    }

    #[tokio::test]
    async fn client_error_is_not_retried() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/chat"))
            .respond_with(ResponseTemplate::new(400).set_body_string("bad request"))
            .expect(1)
            .mount(&server)
            .await;

        let client = HttpGatewayClient::new(
            server.uri(),
            Credential::api_key("test"),
            Duration::from_secs(30),
        )
        .unwrap()
        .with_max_retries(3)
        .with_retry_delay(Duration::from_millis(10));

        let result = client
            .post_json::<TestBody, serde_json::Value>("/chat", &TestBody { msg: "hi".to_string() })
            .await;

        assert!(result.is_err());
    }

    #[tokio::test]
    async fn not_implemented_is_not_retried() {
        // 501 is a permanent capability gap (e.g. a cloud relay without
        // streaming support) — retrying just adds latency before the same
        // answer.
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/chat"))
            .respond_with(
                ResponseTemplate::new(501)
                    .set_body_string("{\"error\":\"streaming not supported\"}"),
            )
            .expect(1)
            .mount(&server)
            .await;

        let client = HttpGatewayClient::new(
            server.uri(),
            Credential::api_key("test"),
            Duration::from_secs(30),
        )
        .unwrap()
        .with_max_retries(3)
        .with_retry_delay(Duration::from_millis(10));

        let result = client
            .post_json::<TestBody, serde_json::Value>("/chat", &TestBody { msg: "hi".to_string() })
            .await;

        match result {
            Err(crate::error::SyscityError::ExternalService { source, .. }) => {
                assert!(source.starts_with("HTTP 501"), "source: {source}");
            }
            other => panic!("expected ExternalService 501 error, got {:?}", other),
        }
    }

    #[tokio::test]
    async fn server_error_is_retried() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/chat"))
            .respond_with(ResponseTemplate::new(500))
            .up_to_n_times(1)
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/chat"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(serde_json::json!({ "ok": true })),
            )
            .expect(1)
            .mount(&server)
            .await;

        let client = HttpGatewayClient::new(
            server.uri(),
            Credential::api_key("test"),
            Duration::from_secs(30),
        )
        .unwrap()
        .with_max_retries(3)
        .with_retry_delay(Duration::from_millis(10));

        let result = client
            .post_json::<TestBody, serde_json::Value>("/chat", &TestBody { msg: "hi".to_string() })
            .await;

        assert!(result.is_ok());
    }
}
