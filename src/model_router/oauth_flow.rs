//! OAuth 2.0 + PKCE initial authorization flow
//!
//! Orchestrates the full PKCE flow:
//! 1. Generate verifier + challenge
//! 2. Build authorization URL
//! 3. Exchange authorization code for tokens
//!
//! The redirect URI is always supplied by the caller: the gateway's own callback
//! route completes provider authorizations (`provider_oauth`). There used to be
//! a loopback-listener variant for the CLI, which the gateway-hosted callback
//! replaced — a listener can only work when the browser and the gateway share a
//! host, which is the case the design does not want to be limited to.
//!
//! ```rust,ignore
//! let flow = OAuthFlow::new();
//! let url = flow.authorization_url_with_redirect(&config, redirect_uri);
//! // ... the provider redirects the browser to redirect_uri ...
//! let credential = flow.exchange_code_with_redirect("code", &config, redirect_uri).await?;
//! ```

use base64::Engine as _;
use chrono::{Duration, Utc};
use serde::Deserialize;

use crate::model_router::oauth_credential::Credential;
use crate::model_router::pkce;
use crate::model_router::OAuthConfig;

/// Orchestrates an OAuth 2.0 + PKCE authorization flow.
#[derive(Debug)]
pub struct OAuthFlow {
    client: reqwest::Client,
    verifier: String,
    state: String,
}

impl OAuthFlow {
    /// Create a new flow with a fresh PKCE verifier and random state.
    pub fn new() -> Self {
        Self {
            client: reqwest::Client::new(),
            verifier: pkce::generate_verifier(),
            state: generate_state(),
        }
    }

    /// The PKCE code verifier (needed when exchanging the code).
    pub fn verifier(&self) -> &str {
        &self.verifier
    }

    /// The random state parameter (validate this in the callback).
    pub fn state(&self) -> &str {
        &self.state
    }

    /// Build the authorization URL for an explicit redirect URI.
    ///
    /// The URI is the one registered with the provider; the gateway passes its
    /// own callback (`OAuthConfig::redirect_uri`).
    pub fn authorization_url_with_redirect(
        &self,
        auth_url: &str,
        config: &OAuthConfig,
        redirect_uri: &str,
    ) -> String {
        let challenge = pkce::challenge_from_verifier(&self.verifier);

        let mut url = format!(
            "{}?response_type=code&client_id={}&redirect_uri={}&code_challenge={}&\
             code_challenge_method=S256&state={}",
            urlencoding::encode(auth_url),
            urlencoding::encode(&config.client_id),
            urlencoding::encode(redirect_uri),
            urlencoding::encode(&challenge),
            urlencoding::encode(&self.state),
        );

        if let Some(ref scope) = config.scope {
            url.push_str(&format!("&scope={}", urlencoding::encode(scope)));
        }

        url
    }

    /// Exchange an authorization code using an explicit redirect URI.
    ///
    /// It must be the same string that was sent in the authorization request —
    /// providers compare them and reject a mismatch.
    pub async fn exchange_code_with_redirect(
        &self,
        code: &str,
        config: &OAuthConfig,
        redirect_uri: &str,
    ) -> crate::Result<Credential> {
        let mut params: Vec<(&str, &str)> = vec![
            ("grant_type", "authorization_code"),
            ("code", code),
            ("client_id", config.client_id.as_str()),
            ("redirect_uri", redirect_uri),
            ("code_verifier", self.verifier.as_str()),
        ];

        if let Some(ref client_secret) = config.client_secret {
            params.push(("client_secret", client_secret.as_str()));
        }

        let resp = self
            .client
            .post(&config.token_url)
            .form(&params)
            .send()
            .await
            .map_err(|e| crate::error::SyscityError::ExternalService {
                source: format!("OAuth token exchange request failed: {}", e),
                cause: Some(Box::new(e)),
            })?;

        if !resp.status().is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(crate::error::SyscityError::ExternalService {
                source: format!("OAuth token exchange failed: {}", body),
                cause: None,
            });
        }

        let data: TokenResponse =
            resp.json()
                .await
                .map_err(|e| crate::error::SyscityError::ExternalService {
                    source: format!("OAuth token exchange response invalid: {}", e),
                    cause: Some(Box::new(e)),
                })?;

        let expires_at = Utc::now() + Duration::seconds(data.expires_in as i64);

        Ok(Credential::OAuth2 {
            access_token: data.access_token,
            refresh_token: data.refresh_token,
            expires_at,
            token_url: config.token_url.clone(),
            client_id: config.client_id.clone(),
            client_secret: config.client_secret.clone(),
            scope: config.scope.clone(),
        })
    }
}

impl Default for OAuthFlow {
    fn default() -> Self {
        Self::new()
    }
}

/// OAuth2 token endpoint response (authorization_code flow).
#[derive(Debug, Clone, Deserialize)]
struct TokenResponse {
    access_token: String,
    #[serde(default = "default_expires_in")]
    expires_in: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    refresh_token: Option<String>,
}

fn default_expires_in() -> u64 {
    3600
}

fn generate_state() -> String {
    let bytes: Vec<u8> = (0..32).map(|_| rand::random::<u8>()).collect();
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(&bytes)
}

/// A random opaque flow identifier.
///
/// The authorization-code flow uses its CSRF `state` as the flow id because the
/// callback has to carry it; a device flow has no callback, so its flow id is
/// minted here and is deliberately *not* the device code — that value is the
/// bearer of an authorization in progress and has no business travelling to a
/// client that only needs to name the flow.
pub fn generate_flow_id() -> String {
    generate_state()
}

// ------------------------------------------------------------------
// Device authorization (RFC 8628)
// ------------------------------------------------------------------

/// What a provider hands back when a device authorization is started.
///
/// There is no PKCE verifier and no redirect: the user types `user_code` at
/// `verification_uri` on whatever device has a browser, and the client polls.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceAuthorization {
    /// The secret the token endpoint is polled with.
    pub device_code: String,
    /// The short code the user types.
    pub user_code: String,
    /// Where the user types it.
    pub verification_uri: String,
    /// How often the provider wants to be polled.
    pub interval_secs: u64,
    /// How long the user has.
    pub expires_in_secs: u64,
}

/// One poll of a device flow's token endpoint.
///
/// `Pending` and `SlowDown` are normal answers, and `Refused` is a *known*
/// ending — the user said no, or the code ran out. The three are separated from
/// an `Err` on purpose: an error means the poll could not be completed and the
/// flow's fate is unknown, so the caller must not treat it as one.
#[derive(Debug)]
pub enum DevicePoll {
    /// The user has not finished yet (RFC 8628 `authorization_pending`).
    Pending,
    /// Polling too fast: the provider wants a longer interval (`slow_down`).
    SlowDown { interval_secs: u64 },
    /// The user finished: tokens.
    Authorized(Box<Credential>),
    /// The provider ended the flow, with its own reason.
    Refused { reason: String },
}

/// Drives a device authorization.
#[derive(Debug, Clone)]
pub struct DeviceFlow {
    client: reqwest::Client,
}

impl DeviceFlow {
    pub fn new() -> Self {
        Self { client: reqwest::Client::new() }
    }

    /// Ask the provider for a device code (RFC 8628 §3.1).
    ///
    /// `device_auth_url` is a separate endpoint from `auth_url` — that is the
    /// whole reason `OAuthConfig` carries both.
    pub async fn request_code(
        &self,
        config: &OAuthConfig,
        device_auth_url: &str,
    ) -> crate::Result<DeviceAuthorization> {
        let mut params: Vec<(&str, &str)> = vec![("client_id", config.client_id.as_str())];
        if let Some(ref scope) = config.scope {
            params.push(("scope", scope.as_str()));
        }

        let resp = self
            .client
            .post(device_auth_url)
            .form(&params)
            .send()
            .await
            .map_err(|e| crate::error::SyscityError::ExternalService {
                source: format!("OAuth device authorization request failed: {e}"),
                cause: Some(Box::new(e)),
            })?;

        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        if !status.is_success() {
            return Err(crate::error::SyscityError::ExternalService {
                source: format!("OAuth device authorization failed ({status}): {body}"),
                cause: None,
            });
        }

        let data: DeviceAuthorizationResponse = serde_json::from_str(&body).map_err(|e| {
            crate::error::SyscityError::ExternalService {
                source: format!("OAuth device authorization response invalid: {e}"),
                cause: None,
            }
        })?;

        // `verification_uri` is the RFC 8628 name; Google's older
        // implementation calls it `verification_url`, and both are in the wild.
        let verification_uri =
            data.verification_uri
                .or(data.verification_url)
                .ok_or_else(|| crate::error::SyscityError::ExternalService {
                    source: "the device authorization response named no verification URI"
                        .to_string(),
                    cause: None,
                })?;

        Ok(DeviceAuthorization {
            device_code: data.device_code,
            user_code: data.user_code,
            verification_uri,
            // The RFC's default when the provider does not say.
            interval_secs: data.interval.unwrap_or(5).max(1),
            expires_in_secs: data.expires_in.unwrap_or(900),
        })
    }

    /// Poll the token endpoint once (RFC 8628 §3.4).
    ///
    /// `Pending` and `SlowDown` are normal answers here, not errors: a flow that
    /// has not been approved yet has not failed. Everything else the provider
    /// can say (`access_denied`, `expired_token`, an unknown error, a malformed
    /// body) is returned as an error, because there is nothing left to wait for.
    pub async fn poll(&self, config: &OAuthConfig, device_code: &str) -> crate::Result<DevicePoll> {
        let mut params: Vec<(&str, &str)> = vec![
            ("grant_type", "urn:ietf:params:oauth:grant-type:device_code"),
            ("device_code", device_code),
            ("client_id", config.client_id.as_str()),
        ];
        if let Some(ref client_secret) = config.client_secret {
            params.push(("client_secret", client_secret.as_str()));
        }

        let resp = self
            .client
            .post(&config.token_url)
            .form(&params)
            .send()
            .await
            .map_err(|e| crate::error::SyscityError::ExternalService {
                source: format!("OAuth device token request failed: {e}"),
                cause: Some(Box::new(e)),
            })?;

        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();

        if status.is_success() {
            let data: TokenResponse = serde_json::from_str(&body).map_err(|e| {
                crate::error::SyscityError::ExternalService {
                    source: format!("OAuth token response invalid: {e}"),
                    cause: None,
                }
            })?;
            let expires_at = Utc::now() + Duration::seconds(data.expires_in as i64);
            return Ok(DevicePoll::Authorized(Box::new(Credential::OAuth2 {
                access_token: data.access_token,
                refresh_token: data.refresh_token,
                expires_at,
                token_url: config.token_url.clone(),
                client_id: config.client_id.clone(),
                client_secret: config.client_secret.clone(),
                scope: config.scope.clone(),
            })));
        }

        let Ok(error) = serde_json::from_str::<TokenErrorResponse>(&body) else {
            // A 5xx, a proxy page, a truncated body: the poll did not happen,
            // which is not the same as a flow that ended.
            return Err(crate::error::SyscityError::ExternalService {
                source: format!("OAuth device token poll failed ({status}): {body}"),
                cause: None,
            });
        };
        match error.error.as_str() {
            "authorization_pending" => Ok(DevicePoll::Pending),
            // The RFC says to add five seconds; a provider that names its own
            // interval is taken at its word.
            "slow_down" => Ok(DevicePoll::SlowDown {
                interval_secs: error.interval.unwrap_or(10),
            }),
            // `access_denied`, `expired_token`, and anything else the provider
            // spells: all of them mean there is nothing left to wait for.
            other => Ok(DevicePoll::Refused {
                reason: match error.error_description {
                    Some(description) => format!("{other}: {description}"),
                    None => other.to_string(),
                },
            }),
        }
    }
}

impl Default for DeviceFlow {
    fn default() -> Self {
        Self::new()
    }
}

/// Device authorization endpoint response (RFC 8628 §3.2).
#[derive(Debug, Deserialize)]
struct DeviceAuthorizationResponse {
    device_code: String,
    user_code: String,
    #[serde(default)]
    verification_uri: Option<String>,
    #[serde(default)]
    verification_url: Option<String>,
    #[serde(default)]
    interval: Option<u64>,
    #[serde(default)]
    expires_in: Option<u64>,
}

/// A token endpoint's error body (RFC 6749 §5.2).
#[derive(Debug, Deserialize)]
struct TokenErrorResponse {
    error: String,
    #[serde(default)]
    error_description: Option<String>,
    #[serde(default)]
    interval: Option<u64>,
}

#[cfg(test)]
mod tests {
    use wiremock::matchers::{body_string_contains, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::*;

    /// The redirect URI a caller would hand in; the exchange must echo it back
    /// verbatim, so the tests pass the same string to both calls.
    const REDIRECT_URI: &str = "https://gw.example.com/oauth/provider/callback";

    fn test_oauth_config(token_url: String, client_secret: Option<String>) -> OAuthConfig {
        OAuthConfig {
            client_id: "test-client".to_string(),
            auth_url: Some("http://example.com/auth".to_string()),
            device_auth_url: None,
            token_url,
            scope: None,
            client_secret,
            redirect_base: None,
            refresh_token: None,
        }
    }

    #[tokio::test]
    async fn exchange_code_includes_client_secret() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/token"))
            .and(body_string_contains("client_secret=super-secret"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "access_token": "access-123",
                "refresh_token": "refresh-456",
                "expires_in": 3600,
                "token_type": "Bearer",
            })))
            .expect(1)
            .mount(&server)
            .await;

        let config =
            test_oauth_config(format!("{}/token", server.uri()), Some("super-secret".to_string()));
        let flow = OAuthFlow::new();
        let credential = flow
            .exchange_code_with_redirect("auth-code", &config, REDIRECT_URI)
            .await
            .unwrap();

        match credential {
            Credential::OAuth2 {
                client_secret,
                access_token,
                refresh_token,
                ..
            } => {
                assert_eq!(access_token, "access-123");
                assert_eq!(refresh_token, Some("refresh-456".to_string()));
                assert_eq!(client_secret, Some("super-secret".to_string()));
            }
            other => panic!("expected OAuth2 credential, got {:?}", other),
        }
    }

    #[tokio::test]
    async fn exchange_code_omits_client_secret_when_not_configured() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/token"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "access_token": "access-789",
                "expires_in": 3600,
                "token_type": "Bearer",
            })))
            .expect(1)
            .mount(&server)
            .await;

        let config = test_oauth_config(format!("{}/token", server.uri()), None);
        let flow = OAuthFlow::new();
        let credential = flow
            .exchange_code_with_redirect("auth-code", &config, REDIRECT_URI)
            .await
            .unwrap();

        match credential {
            Credential::OAuth2 { client_secret, .. } => {
                assert_eq!(client_secret, None);
            }
            other => panic!("expected OAuth2 credential, got {:?}", other),
        }
    }

    // ── Device flow (RFC 8628) ──────────────────────────────────────────────

    fn device_config(device_url: String, token_url: String) -> OAuthConfig {
        OAuthConfig {
            client_id: "test-client".to_string(),
            auth_url: None,
            device_auth_url: Some(device_url),
            token_url,
            scope: Some("openid".to_string()),
            client_secret: None,
            redirect_base: None,
            refresh_token: None,
        }
    }

    /// Google's implementation of the device flow predates the RFC's field name
    /// and calls it `verification_url`; a provider that speaks only that must
    /// still be usable.
    #[tokio::test]
    async fn device_code_accepts_the_verification_url_spelling() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/device"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "device_code": "dev",
                "user_code": "CODE-1",
                "verification_url": "https://provider.example/activate",
                "expires_in": 600,
            })))
            .mount(&server)
            .await;

        let config = device_config(format!("{}/device", server.uri()), "unused".to_string());
        let authorization = DeviceFlow::new()
            .request_code(&config, &format!("{}/device", server.uri()))
            .await
            .unwrap();

        assert_eq!(authorization.verification_uri, "https://provider.example/activate");
        // No `interval` in the response: the RFC's default of five seconds.
        assert_eq!(authorization.interval_secs, 5);
        assert_eq!(authorization.expires_in_secs, 600);
    }

    /// A device authorization endpoint that answers with an error is not a
    /// hang: it fails, with the provider's message.
    #[tokio::test]
    async fn device_code_request_reports_a_refusal() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/device"))
            .respond_with(ResponseTemplate::new(400).set_body_string("client not allowed"))
            .mount(&server)
            .await;

        let config = device_config(format!("{}/device", server.uri()), "unused".to_string());
        let err = DeviceFlow::new()
            .request_code(&config, &format!("{}/device", server.uri()))
            .await
            .unwrap_err();

        assert!(format!("{err}").contains("client not allowed"), "{err}");
    }

    #[tokio::test]
    async fn device_poll_maps_the_rfc_error_codes() {
        for error in [
            "authorization_pending",
            "slow_down",
            "access_denied",
            "expired_token",
        ] {
            let server = MockServer::start().await;
            Mock::given(method("POST"))
                .and(path("/token"))
                .and(body_string_contains("device_code"))
                .respond_with(ResponseTemplate::new(400).set_body_json(serde_json::json!({
                    "error": error,
                    "error_description": "because",
                })))
                .mount(&server)
                .await;

            let config = device_config("unused".to_string(), format!("{}/token", server.uri()));
            let poll = DeviceFlow::new().poll(&config, "dev-code").await.unwrap();
            match (error, poll) {
                ("authorization_pending", DevicePoll::Pending) => {}
                ("slow_down", DevicePoll::SlowDown { interval_secs }) => {
                    assert_eq!(interval_secs, 10, "the RFC's five seconds on top of five")
                }
                ("access_denied" | "expired_token", DevicePoll::Refused { reason }) => {
                    assert!(reason.contains(error) && reason.contains("because"), "{reason}");
                }
                (error, poll) => panic!("{error} mapped to {poll:?}"),
            }
        }
    }

    /// A malformed or failed response is *not* a refusal: the flow's fate is
    /// unknown, and the caller must not report it as decided.
    #[tokio::test]
    async fn device_poll_separates_a_broken_response_from_a_refusal() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/token"))
            .respond_with(ResponseTemplate::new(502).set_body_string("<html>bad gateway</html>"))
            .mount(&server)
            .await;

        let config = device_config("unused".to_string(), format!("{}/token", server.uri()));
        let err = DeviceFlow::new()
            .poll(&config, "dev-code")
            .await
            .unwrap_err();

        assert!(format!("{err}").contains("502"), "{err}");
    }
}
