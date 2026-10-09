//! Provider OAuth authorization flows (authorization code + PKCE), run
//! gateway-side.
//!
//! The shape is a round trip through the browser:
//!
//! 1. `providers.auth_start` hands the caller an authorization URL and remembers
//!    the flow under its `state`.
//! 2. The provider redirects the browser to [`CALLBACK_PATH`] on the gateway's
//!    own origin.
//! 3. The route calls [`ProviderOAuthFlows::complete`], which exchanges the code
//!    and stores the refresh token.
//!
//! Completion arriving as an ordinary HTTP request is why there is no actor
//! here, no command channel and no spawned listener — unlike the MCP OAuth
//! manager, which needs all three because it binds an ephemeral port per flow.
//! The pending map is the whole of the state.
//!
//! What that buys: the callback lands on an origin the gateway already serves,
//! so `redirect_base` can point at a public URL. A loopback listener — the
//! obvious alternative, and what the CLI used to do — only works when the
//! browser and the gateway share a host.

use std::collections::HashMap;
use std::sync::Arc;

use chrono::{DateTime, Duration, Utc};
use tokio::sync::RwLock;

use crate::model_router::oauth_credential::Credential;
use crate::model_router::oauth_flow::OAuthFlow;
use crate::model_router::OAuthConfig;
use crate::secrets::{SecretId, SecretStoreHandle};

/// Path the gateway serves the OAuth callback on.
pub const CALLBACK_PATH: &str = "/oauth/provider/callback";

/// How long a started flow stays completable.
///
/// Enforced lazily: there is no timer task, so an abandoned flow is rejected and
/// reaped the next time anything touches it, rather than at the instant it
/// expires. What matters is that nothing *can* be completed past this window,
/// and that holds either way.
pub const FLOW_TTL_SECS: i64 = 600;

/// A flow handed out by [`ProviderOAuthFlows::start`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StartedFlow {
    /// Identifies the flow to `auth_status` / `auth_cancel`. This is the OAuth
    /// `state`, so the callback and the cancellation refer to the same value.
    pub flow_id: String,
    /// The URL to open in a browser.
    pub auth_url: String,
}

/// A provider's authorization state as seen from outside.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AuthStatus {
    /// A refresh token is stored for this provider.
    pub authorized: bool,
    /// The id of a flow waiting for the user, if any.
    pub pending: Option<String>,
}

/// One in-flight authorization.
#[derive(Debug)]
struct PendingFlow {
    provider: String,
    oauth: OAuthConfig,
    /// The redirect URI sent in the authorization request. The token request
    /// must repeat it verbatim — providers reject a mismatch — so it is kept
    /// rather than recomputed (recomputing would need the gateway's base again).
    redirect_uri: String,
    /// Owns the PKCE verifier; dropping the flow drops it.
    flow: OAuthFlow,
    started_at: DateTime<Utc>,
}

impl PendingFlow {
    fn expired(&self) -> bool {
        Utc::now() - self.started_at > Duration::seconds(FLOW_TTL_SECS)
    }
}

/// In-flight provider authorization flows, plus the credential lookups that go
/// with them.
pub struct ProviderOAuthFlows {
    secrets: Arc<SecretStoreHandle>,
    /// Keyed by OAuth `state`.
    pending: RwLock<HashMap<String, PendingFlow>>,
}

impl ProviderOAuthFlows {
    /// Create a flow manager over a secret store.
    pub fn new(secrets: Arc<SecretStoreHandle>) -> Self {
        Self {
            secrets,
            pending: RwLock::new(HashMap::new()),
        }
    }

    /// Where a provider's refresh token is stored.
    pub fn refresh_token_id(provider: &str) -> SecretId {
        SecretId::new("llm-oauth", provider, "refresh_token")
    }

    /// Begin an authorization for `provider`.
    ///
    /// `gateway_base` is the gateway's own origin (`http://host:port`), used as
    /// the redirect target unless the provider's config names one.
    pub async fn start(
        &self,
        provider: &str,
        oauth: &OAuthConfig,
        gateway_base: &str,
    ) -> StartedFlow {
        let redirect_uri = oauth.redirect_uri(gateway_base);
        let flow = OAuthFlow::new();
        let auth_url = flow.authorization_url_with_redirect(oauth, &redirect_uri);
        let flow_id = flow.state().to_string();

        let entry = PendingFlow {
            provider: provider.to_string(),
            oauth: oauth.clone(),
            redirect_uri,
            flow,
            started_at: Utc::now(),
        };

        let mut pending = self.pending.write().await;
        // Starting again supersedes — but only for this provider. The
        // superseded flow's verifier goes with it, so its state can no longer
        // be completed; without this a second `auth_start` would leave two live
        // states for one provider, and the first would still be completable.
        // Another provider's pending authorization is none of our business.
        pending.retain(|_, flow| {
            if flow.expired() {
                tracing::debug!("Reaping expired provider OAuth flow for {}", flow.provider);
                return false;
            }
            flow.provider != provider
        });
        pending.insert(flow_id.clone(), entry);

        StartedFlow { flow_id, auth_url }
    }

    /// Which provider a `state` belongs to, if it is still pending.
    ///
    /// The callback route reads this *before* [`ProviderOAuthFlows::complete`],
    /// because completing consumes the state: a failed exchange still needs to
    /// be attributed to a provider to be worth reporting.
    pub async fn provider_of_state(&self, state: &str) -> Option<String> {
        self.pending
            .read()
            .await
            .get(state)
            .map(|flow| flow.provider.clone())
    }

    /// Complete a flow: validate its state, exchange the code, store the refresh
    /// token, and return the provider that is now authorized.
    pub async fn complete(&self, state: &str, code: &str) -> crate::Result<String> {
        // Removed before the exchange, not after: the state is single-use, and
        // a replay must find nothing even if the exchange is still in flight.
        let entry = self.pending.write().await.remove(state);

        let Some(entry) = entry else {
            return Err(crate::error::SyscityError::Validation(
                "unknown or already-used OAuth state".to_string(),
            ));
        };
        if entry.expired() {
            return Err(crate::error::SyscityError::Validation(format!(
                "the authorization flow for '{}' expired after {FLOW_TTL_SECS}s; start it again",
                entry.provider
            )));
        }

        let credential = entry
            .flow
            .exchange_code_with_redirect(code, &entry.oauth, &entry.redirect_uri)
            .await?;

        let refresh_token = match credential {
            Credential::OAuth2 { refresh_token, .. } => refresh_token,
            other => {
                return Err(crate::error::SyscityError::Validation(format!(
                    "the token exchange for '{}' returned {:?}, not an OAuth2 credential",
                    entry.provider,
                    other.authorization_header().split(' ').next().unwrap_or("")
                )))
            }
        };

        // A provider that issues no refresh token cannot be kept authorized:
        // access tokens are deliberately memory-only, so there would be nothing
        // left after a restart.
        let Some(refresh_token) = refresh_token else {
            return Err(crate::error::SyscityError::Validation(format!(
                "'{}' returned no refresh token; this provider cannot stay authorized \
                 across restarts",
                entry.provider
            )));
        };

        let id = Self::refresh_token_id(&entry.provider);
        self.secrets.choose(&id).set(&id, &refresh_token).await?;

        Ok(entry.provider)
    }

    /// Drop expired flows and report how many were reaped.
    pub async fn reap_expired(&self) -> usize {
        let mut pending = self.pending.write().await;
        let before = pending.len();
        pending.retain(|_, flow| {
            if flow.expired() {
                tracing::debug!("Reaping expired provider OAuth flow for {}", flow.provider);
                false
            } else {
                true
            }
        });
        before - pending.len()
    }

    /// Whether a refresh token is stored for `provider`.
    pub async fn is_authorized(&self, provider: &str) -> bool {
        let id = Self::refresh_token_id(provider);
        let store = self.secrets.choose(&id);
        store.has(&id).await
    }

    /// A provider's authorization state, reaping any expired flow on the way.
    pub async fn status(&self, provider: &str) -> AuthStatus {
        let pending = {
            let mut guard = self.pending.write().await;
            guard.retain(|_, flow| !flow.expired());
            guard
                .iter()
                .find(|(_, flow)| flow.provider == provider)
                .map(|(state, _)| state.clone())
        };

        AuthStatus {
            authorized: self.is_authorized(provider).await,
            pending,
        }
    }

    /// Abandon any pending flow for `provider`. Returns whether one was pending.
    pub async fn cancel(&self, provider: &str) -> bool {
        let mut pending = self.pending.write().await;
        let before = pending.len();
        pending.retain(|_, flow| flow.provider != provider);
        pending.len() != before
    }
}

/// Invariant checks owned by this module, registered by
/// [`crate::core::invariants::register_builtins`].
pub(crate) fn provider_oauth_invariant_checks() -> Vec<crate::core::invariants::Invariant> {
    use crate::core::invariants::{Invariant, SKIP_PREFIX};

    vec![Invariant {
        id: "model_router/llm_oauth_persists_no_access_token",
        module: "model_router",
        description:
            "a provider OAuth access token never reaches disk; only the refresh token is persisted",
        check: || {
            Box::pin(async move {
                // The namespace `refresh_token_id` writes to, and the one
                // `SecretStoreHandle::choose` keeps access tokens out of.
                let dir = crate::secrets::secrets_root_dir().join("llm-oauth");
                let mut entries = match tokio::fs::read_dir(&dir).await {
                    Ok(entries) => entries,
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                        return Err(format!(
                            "{SKIP_PREFIX}no provider OAuth credentials stored at {} yet",
                            dir.display()
                        ));
                    }
                    Err(e) => return Err(format!("cannot read {}: {e}", dir.display())),
                };

                let mut offenders = Vec::new();
                while let Some(entry) = entries.next_entry().await.map_err(|e| e.to_string())? {
                    let path = entry.path();
                    if path.extension().and_then(|e| e.to_str()) != Some("toml") {
                        continue;
                    }
                    let text = tokio::fs::read_to_string(&path)
                        .await
                        .map_err(|e| format!("cannot read {}: {e}", path.display()))?;
                    // These files hold `[secrets] <kind> = "…"`, so an access
                    // token that reached disk would show up as one of these keys.
                    if text.contains("access_token") || text.contains("expires_at") {
                        offenders.push(path.display().to_string());
                    }
                }

                if offenders.is_empty() {
                    Ok(())
                } else {
                    Err(format!(
                        "access-token material is persisted at {} — access tokens are memory-only \
                         (docs/secret-storage.md §1.4.4), so something now writes them",
                        offenders.join(", ")
                    ))
                }
            })
        },
    }]
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{body_string_contains, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    /// A hermetic flow manager: no keyring, no `~/.syscity`.
    fn flows(name: &str) -> (ProviderOAuthFlows, std::path::PathBuf) {
        let root = std::env::temp_dir()
            .join(format!("syscity_provider_oauth_{}_{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let handle = SecretStoreHandle::with_root(root.clone()).expect("isolated handle");
        (ProviderOAuthFlows::new(Arc::new(handle)), root)
    }

    fn oauth_config(token_url: String) -> OAuthConfig {
        OAuthConfig {
            client_id: "test-client".to_string(),
            auth_url: "https://provider.example/authorize".to_string(),
            token_url,
            scope: Some("openid".to_string()),
            client_secret: None,
            redirect_base: None,
            refresh_token: None,
        }
    }

    fn token_endpoint(server: &MockServer, body: serde_json::Value) -> Mock {
        Mock::given(method("POST"))
            .and(path("/token"))
            .and(body_string_contains("grant_type=authorization_code"))
            .respond_with(ResponseTemplate::new(200).set_body_json(body))
    }

    #[tokio::test]
    async fn start_builds_a_gateway_hosted_redirect() {
        let (flows, root) = flows("start");
        let config = oauth_config("https://provider.example/token".to_string());

        let started = flows.start("grok", &config, "http://127.0.0.1:18080").await;

        // The redirect must point at the gateway's own callback route, not a
        // loopback port: that is what makes a public `redirect_base` possible.
        let expected =
            urlencoding::encode("http://127.0.0.1:18080/oauth/provider/callback").to_string();
        assert!(started.auth_url.contains(&expected), "{}", started.auth_url);
        assert!(started.auth_url.contains("code_challenge_method=S256"));
        assert_eq!(started.auth_url.matches("state=").count(), 1);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn start_honours_redirect_base_override() {
        let (flows, root) = flows("redirect_base");
        let mut config = oauth_config("https://provider.example/token".to_string());
        config.redirect_base = Some("https://gw.example.com/".to_string());

        let started = flows.start("grok", &config, "http://127.0.0.1:18080").await;

        // Trailing slash trimmed; the gateway base is ignored.
        let expected =
            urlencoding::encode("https://gw.example.com/oauth/provider/callback").to_string();
        assert!(started.auth_url.contains(&expected), "{}", started.auth_url);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn complete_stores_the_refresh_token() {
        let server = MockServer::start().await;
        token_endpoint(
            &server,
            serde_json::json!({
                "access_token": "at",
                "refresh_token": "rt-value",
                "expires_in": 3600,
            }),
        )
        .expect(1)
        .mount(&server)
        .await;

        let (flows, root) = flows("complete");
        let config = oauth_config(format!("{}/token", server.uri()));
        let started = flows.start("grok", &config, "http://127.0.0.1:18080").await;

        let provider = flows.complete(&started.flow_id, "auth-code").await.unwrap();
        assert_eq!(provider, "grok");

        // Stored where the resolver will look for it, and nowhere else.
        let id = ProviderOAuthFlows::refresh_token_id("grok");
        let store = flows.secrets.choose(&id);
        assert_eq!(store.get(&id).await.unwrap().as_deref(), Some("rt-value"));
        assert!(flows.is_authorized("grok").await);
        assert!(!flows.is_authorized("other").await);

        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn complete_rejects_an_unknown_or_replayed_state() {
        let (flows, root) = flows("replay");
        let err = flows
            .complete("never-issued", "auth-code")
            .await
            .unwrap_err();
        assert!(format!("{err}").contains("unknown or already-used"), "{err}");

        // And a second completion of a *real* flow is refused: the state is
        // single-use, so a leaked callback cannot be replayed.
        let server = MockServer::start().await;
        token_endpoint(
            &server,
            serde_json::json!({
                "access_token": "at",
                "refresh_token": "rt-value",
                "expires_in": 3600,
            }),
        )
        .mount(&server)
        .await;
        let config = oauth_config(format!("{}/token", server.uri()));
        let started = flows.start("grok", &config, "http://127.0.0.1:18080").await;

        assert!(flows.complete(&started.flow_id, "code-1").await.is_ok());
        let replay = flows
            .complete(&started.flow_id, "code-1")
            .await
            .unwrap_err();
        assert!(format!("{replay}").contains("unknown or already-used"), "{replay}");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn complete_refuses_a_provider_that_issues_no_refresh_token() {
        let server = MockServer::start().await;
        token_endpoint(&server, serde_json::json!({ "access_token": "at", "expires_in": 3600 }))
            .mount(&server)
            .await;

        let (flows, root) = flows("no_refresh");
        let config = oauth_config(format!("{}/token", server.uri()));
        let started = flows.start("grok", &config, "http://127.0.0.1:18080").await;

        let err = flows.complete(&started.flow_id, "code").await.unwrap_err();
        // Without a refresh token there is nothing durable to keep, so the flow
        // must fail loudly instead of appearing to succeed.
        assert!(format!("{err}").contains("no refresh token"), "{err}");
        assert!(!flows.is_authorized("grok").await);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn starting_again_supersedes_the_previous_flow() {
        let (flows, root) = flows("supersede");
        let config = oauth_config("https://provider.example/token".to_string());

        let first = flows.start("grok", &config, "http://127.0.0.1:18080").await;
        let second = flows.start("grok", &config, "http://127.0.0.1:18080").await;

        assert_ne!(first.flow_id, second.flow_id);
        assert_eq!(flows.status("grok").await.pending, Some(second.flow_id));
        // The superseded flow's verifier is gone, so its state cannot complete.
        assert!(flows.complete(&first.flow_id, "code").await.is_err());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn starting_a_flow_leaves_other_providers_alone() {
        let (flows, root) = flows("cross_provider");
        let config = oauth_config("https://provider.example/token".to_string());

        let first = flows.start("grok", &config, "http://127.0.0.1:18080").await;
        let second = flows.start("kimi", &config, "http://127.0.0.1:18080").await;

        // Superseding is per provider: authorizing kimi must not have thrown
        // away grok's pending flow.
        assert_eq!(
            flows.status("grok").await.pending,
            Some(first.flow_id),
            "another provider's flow was disturbed"
        );
        assert_eq!(flows.status("kimi").await.pending, Some(second.flow_id));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn cancel_drops_the_pending_flow() {
        let (flows, root) = flows("cancel");
        let config = oauth_config("https://provider.example/token".to_string());

        assert!(!flows.cancel("grok").await, "nothing pending yet");
        let started = flows.start("grok", &config, "http://127.0.0.1:18080").await;
        assert!(flows.cancel("grok").await);

        assert_eq!(flows.status("grok").await.pending, None);
        assert!(flows.complete(&started.flow_id, "code").await.is_err());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn expired_flows_are_reaped_and_cannot_complete() {
        let (flows, root) = flows("expiry");
        let config = oauth_config("https://provider.example/token".to_string());
        let started = flows.start("grok", &config, "http://127.0.0.1:18080").await;

        // Age the flow past its window rather than waiting ten minutes.
        {
            let mut pending = flows.pending.write().await;
            let flow = pending.get_mut(&started.flow_id).expect("flow is pending");
            flow.started_at = Utc::now() - Duration::seconds(FLOW_TTL_SECS + 1);
        }

        assert_eq!(flows.reap_expired().await, 1);
        assert_eq!(flows.reap_expired().await, 0);
        assert_eq!(flows.status("grok").await.pending, None);
        let err = flows.complete(&started.flow_id, "code").await.unwrap_err();
        assert!(format!("{err}").contains("unknown or already-used"), "{err}");
        let _ = std::fs::remove_dir_all(&root);
    }
}
