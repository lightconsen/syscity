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
        config: &OAuthConfig,
        redirect_uri: &str,
    ) -> String {
        let challenge = pkce::challenge_from_verifier(&self.verifier);

        let mut url = format!(
            "{}?response_type=code&client_id={}&redirect_uri={}&code_challenge={}&\
             code_challenge_method=S256&state={}",
            urlencoding::encode(&config.auth_url),
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
            auth_url: "http://example.com/auth".to_string(),
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
}
