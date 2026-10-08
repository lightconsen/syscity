//! OAuth2 credential types for LLM provider authentication
//!
//! Supports multiple authentication schemes:
//! - API key (traditional, e.g. OpenAI, Anthropic)
//! - Bearer token (short-lived, e.g. some enterprise proxies)
//! - OAuth2 client credentials (Azure AD, Google, etc.)

use std::fmt;

use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};

/// Authentication credential for LLM providers.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum Credential {
    /// Simple API key (most common)
    ApiKey {
        /// The secret key
        key: String,
    },
    /// Bearer token with optional expiration
    BearerToken {
        /// The token string
        token: String,
        /// When the token expires (if known)
        #[serde(skip_serializing_if = "Option::is_none")]
        expires_at: Option<DateTime<Utc>>,
    },
    /// OAuth2 client-credentials flow
    OAuth2 {
        /// Current access token
        access_token: String,
        /// Refresh token (if available)
        #[serde(skip_serializing_if = "Option::is_none")]
        refresh_token: Option<String>,
        /// Token expiration time
        expires_at: DateTime<Utc>,
        /// OAuth2 token endpoint URL
        token_url: String,
        /// OAuth2 client ID
        client_id: String,
        /// OAuth2 client secret
        #[serde(skip_serializing_if = "Option::is_none")]
        client_secret: Option<String>,
        /// Optional scope string
        #[serde(skip_serializing_if = "Option::is_none")]
        scope: Option<String>,
    },
}

impl Credential {
    /// Create an API key credential (backward-compat helper).
    pub fn api_key(key: impl Into<String>) -> Self {
        Self::ApiKey { key: key.into() }
    }

    /// Create a bearer token credential.
    pub fn bearer_token(token: impl Into<String>) -> Self {
        Self::BearerToken {
            token: token.into(),
            expires_at: None,
        }
    }

    /// Build the Authorization header value for this credential.
    pub fn authorization_header(&self) -> String {
        match self {
            Credential::ApiKey { key } => format!("Bearer {key}"),
            Credential::BearerToken { token, .. } => format!("Bearer {token}"),
            Credential::OAuth2 { access_token, .. } => format!("Bearer {access_token}"),
        }
    }

    /// Returns true if the credential has a known expiration and is past it.
    pub fn is_expired(&self) -> bool {
        match self {
            Credential::ApiKey { .. } => false,
            Credential::BearerToken { expires_at, .. } => {
                expires_at.is_some_and(|t| Utc::now() >= t)
            }
            Credential::OAuth2 { expires_at, .. } => Utc::now() >= *expires_at,
        }
    }

    /// Returns true if the credential expires within the given margin.
    pub fn is_expiring_soon(&self, margin: Duration) -> bool {
        match self {
            Credential::ApiKey { .. } => false,
            Credential::BearerToken { expires_at, .. } => {
                expires_at.is_some_and(|t| Utc::now() + margin >= t)
            }
            Credential::OAuth2 { expires_at, .. } => Utc::now() + margin >= *expires_at,
        }
    }

    /// Refresh the credential if it supports refresh and is expired or
    /// expiring.
    ///
    /// For `OAuth2`, performs a client-credentials token refresh.
    /// For other variants this is a no-op.
    ///
    /// Returns what actually happened, so the caller can persist a refresh
    /// token the server rotated. Servers that rotate invalidate the previous
    /// token, so dropping the new one means the next process start presents a
    /// dead credential.
    pub async fn refresh_if_needed(
        &mut self,
        client: &reqwest::Client,
    ) -> crate::Result<RefreshOutcome> {
        let needs_refresh = self.is_expired() || self.is_expiring_soon(Duration::minutes(5));
        if !needs_refresh {
            return Ok(RefreshOutcome::default());
        }

        if let Credential::OAuth2 {
            refresh_token: Some(refresh),
            token_url,
            client_id,
            client_secret,
            scope,
            access_token,
            expires_at,
            ..
        } = self
        {
            let mut params = vec![
                ("grant_type", "refresh_token"),
                ("refresh_token", refresh.as_str()),
                ("client_id", client_id.as_str()),
            ];
            if let Some(secret) = client_secret {
                params.push(("client_secret", secret.as_str()));
            }
            if let Some(scope) = scope {
                params.push(("scope", scope.as_str()));
            }

            let resp = client
                .post(token_url.clone())
                .form(&params)
                .send()
                .await
                .map_err(crate::error::SyscityError::Http)?;

            if !resp.status().is_success() {
                let body = resp.text().await.unwrap_or_default();
                return Err(crate::error::SyscityError::ExternalService {
                    source: format!("OAuth2 refresh failed: {}", body),
                    cause: None,
                });
            }

            let data: TokenResponse =
                resp.json()
                    .await
                    .map_err(|e| crate::error::SyscityError::ExternalService {
                        source: format!("OAuth2 refresh response invalid: {}", e),
                        cause: None,
                    })?;

            *access_token = data.access_token;
            *expires_at = Utc::now() + Duration::seconds(data.expires_in as i64);
            let rotated = data.refresh_token;
            if let Some(new_refresh) = &rotated {
                *refresh = new_refresh.clone();
            }
            return Ok(RefreshOutcome {
                refreshed: true,
                rotated_refresh_token: rotated,
            });
        }
        Ok(RefreshOutcome::default())
    }
}

/// What a [`Credential::refresh_if_needed`] call actually did.
///
/// `Debug` is written by hand: the derived one would print a live refresh
/// token, and this crate's rule is that debug output never carries a secret
/// (`docs/secret-storage.md` §1.4.8).
#[derive(Default, Clone, PartialEq, Eq)]
pub struct RefreshOutcome {
    /// True when a token exchange actually happened.
    pub refreshed: bool,
    /// The new refresh token, when the server returned one.
    pub rotated_refresh_token: Option<String>,
}

impl fmt::Debug for RefreshOutcome {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RefreshOutcome")
            .field("refreshed", &self.refreshed)
            .field(
                "rotated_refresh_token",
                &self.rotated_refresh_token.as_ref().map(|_| "[REDACTED]"),
            )
            .finish()
    }
}

impl fmt::Display for Credential {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Credential::ApiKey { key } => {
                let masked = if key.len() > 8 {
                    format!("{}****", &key[..4])
                } else {
                    "****".to_string()
                };
                write!(f, "ApiKey({})", masked)
            }
            Credential::BearerToken { token, expires_at } => {
                let masked = if token.len() > 8 {
                    format!("{}****", &token[..4])
                } else {
                    "****".to_string()
                };
                write!(
                    f,
                    "BearerToken({}{})",
                    masked,
                    expires_at
                        .map(|t| format!(" exp={}", t))
                        .unwrap_or_default()
                )
            }
            Credential::OAuth2 {
                access_token,
                expires_at,
                client_id,
                ..
            } => {
                let masked = if access_token.len() > 8 {
                    format!("{}****", &access_token[..4])
                } else {
                    "****".to_string()
                };
                write!(f, "OAuth2(client={} token={} exp={})", client_id, masked, expires_at)
            }
        }
    }
}

/// OAuth2 token endpoint response.
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

#[cfg(test)]
mod tests {
    use wiremock::matchers::{body_string_contains, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::*;

    #[test]
    fn test_api_key_authorization_header() {
        let cred = Credential::api_key("sk-test");
        assert_eq!(cred.authorization_header(), "Bearer sk-test");
    }

    #[test]
    fn test_bearer_token_authorization_header() {
        let cred = Credential::BearerToken {
            token: "tok-123".to_string(),
            expires_at: None,
        };
        assert_eq!(cred.authorization_header(), "Bearer tok-123");
    }

    #[test]
    fn test_oauth2_authorization_header() {
        let cred = Credential::OAuth2 {
            access_token: "at-xyz".to_string(),
            refresh_token: Some("rt-abc".to_string()),
            expires_at: Utc::now() + Duration::hours(1),
            token_url: "https://example.com/token".to_string(),
            client_id: "client-1".to_string(),
            client_secret: Some("secret".to_string()),
            scope: None,
        };
        assert_eq!(cred.authorization_header(), "Bearer at-xyz");
    }

    #[test]
    fn test_api_key_never_expires() {
        let cred = Credential::api_key("sk-test");
        assert!(!cred.is_expired());
        assert!(!cred.is_expiring_soon(Duration::minutes(1)));
    }

    #[test]
    fn test_bearer_token_expiration() {
        let past = Utc::now() - Duration::minutes(1);
        let cred = Credential::BearerToken {
            token: "tok".to_string(),
            expires_at: Some(past),
        };
        assert!(cred.is_expired());
        assert!(cred.is_expiring_soon(Duration::minutes(5)));

        let future = Utc::now() + Duration::hours(1);
        let cred2 = Credential::BearerToken {
            token: "tok".to_string(),
            expires_at: Some(future),
        };
        assert!(!cred2.is_expired());
        assert!(!cred2.is_expiring_soon(Duration::minutes(5)));
    }

    #[test]
    fn test_oauth2_expiration() {
        let past = Utc::now() - Duration::minutes(1);
        let cred = Credential::OAuth2 {
            access_token: "at".to_string(),
            refresh_token: None,
            expires_at: past,
            token_url: "https://example.com/token".to_string(),
            client_id: "c".to_string(),
            client_secret: None,
            scope: None,
        };
        assert!(cred.is_expired());

        let near_future = Utc::now() + Duration::minutes(2);
        let cred2 = Credential::OAuth2 {
            access_token: "at".to_string(),
            refresh_token: None,
            expires_at: near_future,
            token_url: "https://example.com/token".to_string(),
            client_id: "c".to_string(),
            client_secret: None,
            scope: None,
        };
        assert!(!cred2.is_expired());
        assert!(cred2.is_expiring_soon(Duration::minutes(5)));
    }

    #[test]
    fn test_credential_display_masks_secrets() {
        let cred = Credential::api_key("sk-very-long-secret-key");
        let s = format!("{}", cred);
        assert!(s.contains("sk-v****"));
        assert!(!s.contains("secret-key"));
    }

    #[test]
    fn test_credential_refresh_no_op_for_api_key() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let client = reqwest::Client::new();
            let mut cred = Credential::api_key("sk-test");
            let outcome = cred.refresh_if_needed(&client).await.unwrap();
            // An API key has nothing to refresh — and reports no rotation,
            // which is what keeps the write-back from firing on every request.
            assert_eq!(outcome, RefreshOutcome::default());
        });
    }

    /// An OAuth2 credential that is already expired, pointed at `token_url`.
    ///
    /// Expired on purpose: `refresh_if_needed` only acts when the token is gone
    /// or within five minutes of going.
    fn expired_oauth2(token_url: String, refresh_token: &str) -> Credential {
        Credential::OAuth2 {
            access_token: String::new(),
            refresh_token: Some(refresh_token.to_string()),
            expires_at: Utc::now() - Duration::seconds(1),
            token_url,
            client_id: "test-client".to_string(),
            client_secret: None,
            scope: None,
        }
    }

    #[tokio::test]
    async fn refresh_reports_a_rotated_refresh_token() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/token"))
            .and(body_string_contains("grant_type=refresh_token"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "access_token": "new-access",
                "refresh_token": "rotated-refresh",
                "expires_in": 3600,
            })))
            .expect(1)
            .mount(&server)
            .await;

        let mut cred = expired_oauth2(format!("{}/token", server.uri()), "original-refresh");
        let outcome = cred
            .refresh_if_needed(&reqwest::Client::new())
            .await
            .unwrap();

        assert!(outcome.refreshed);
        assert_eq!(outcome.rotated_refresh_token.as_deref(), Some("rotated-refresh"));

        match &cred {
            Credential::OAuth2 {
                access_token, refresh_token, ..
            } => {
                assert_eq!(access_token, "new-access");
                assert_eq!(refresh_token.as_deref(), Some("rotated-refresh"));
            }
            other => panic!("expected an OAuth2 credential, got {other:?}"),
        }
    }

    /// Servers that do not rotate send no `refresh_token`. Nothing must be
    /// reported as rotated, or every request would rewrite the stored token.
    #[tokio::test]
    async fn refresh_without_rotation_reports_nothing_to_persist() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/token"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "access_token": "new-access",
                "expires_in": 3600,
            })))
            .expect(1)
            .mount(&server)
            .await;

        let mut cred = expired_oauth2(format!("{}/token", server.uri()), "original-refresh");
        let outcome = cred
            .refresh_if_needed(&reqwest::Client::new())
            .await
            .unwrap();

        assert!(outcome.refreshed);
        assert_eq!(outcome.rotated_refresh_token, None);

        match &cred {
            Credential::OAuth2 { refresh_token, .. } => {
                assert_eq!(refresh_token.as_deref(), Some("original-refresh"));
            }
            other => panic!("expected an OAuth2 credential, got {other:?}"),
        }
    }

    #[test]
    fn refresh_outcome_debug_does_not_print_the_token() {
        let outcome = RefreshOutcome {
            refreshed: true,
            rotated_refresh_token: Some("super-secret-refresh".to_string()),
        };
        let rendered = format!("{outcome:?}");
        assert!(!rendered.contains("super-secret-refresh"), "{rendered}");
        assert!(rendered.contains("REDACTED"), "{rendered}");
    }
}
