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
use crate::model_router::oauth_flow::{DeviceFlow, DevicePoll, OAuthFlow};
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
    /// What to tell the user for a pending *device* flow: the code to type and
    /// where to type it. `None` for the browser flow, whose URL was handed out
    /// when the flow started.
    pub device: Option<DevicePrompt>,
    /// Why the last poll of a device flow failed, if it did.
    ///
    /// A poll that could not reach the provider says nothing about the
    /// authorization, so the flow stays pending and the reason is reported here
    /// rather than thrown: the client is the one that decides whether to keep
    /// waiting. A refusal that *does* end the flow clears `pending` and reports
    /// the provider's own reason here, which is the only place it can be read.
    pub poll_error: Option<String>,
}

/// The instructions for a pending device authorization.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DevicePrompt {
    /// The short code the user types.
    pub user_code: String,
    /// Where they type it.
    pub verification_uri: String,
    /// How often to poll, as the provider asked.
    pub interval_secs: u64,
}

/// What a device flow says about itself without being polled.
struct DeviceState {
    prompt: DevicePrompt,
    /// The provider's interval has elapsed, so a poll is due.
    due: bool,
    /// Why the last poll failed, if it did.
    last_poll_error: Option<String>,
}

/// A device flow as it was handed out, for the caller that started it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StartedDeviceFlow {
    /// Identifies the flow to `auth_status` / `auth_cancel`.
    pub flow_id: String,
    /// What to show the user.
    pub prompt: DevicePrompt,
    /// How long the code is good for.
    pub expires_in_secs: u64,
}

/// One in-flight authorization.
#[derive(Debug)]
enum PendingFlow {
    /// Browser round trip: the provider redirects back to the gateway's own
    /// callback route with a code.
    Code {
        provider: String,
        oauth: OAuthConfig,
        /// The redirect URI sent in the authorization request. The token
        /// request must repeat it verbatim — providers reject a mismatch — so
        /// it is kept rather than recomputed (recomputing would need the
        /// gateway's base again).
        redirect_uri: String,
        /// Owns the PKCE verifier; dropping the flow drops it.
        flow: OAuthFlow,
        started_at: DateTime<Utc>,
    },
    /// Device flow: there is no callback, so the flow advances only when
    /// someone polls it.
    Device {
        provider: String,
        oauth: OAuthConfig,
        device: DeviceFlow,
        device_code: String,
        prompt: DevicePrompt,
        /// Polling too fast earns `slow_down` and, at some providers, a
        /// throttle, so a poll inside the interval is answered from state
        /// instead of by asking again.
        last_polled_at: Option<DateTime<Utc>>,
        /// Why the last poll failed, kept so a throttled answer can still
        /// report it instead of going quiet for an interval.
        last_poll_error: Option<String>,
        /// The provider's own lifetime for the code, which is the authority on
        /// it — `FLOW_TTL_SECS` bounds a browser flow, whose end is our own
        /// callback, not a code the user has to type in time.
        expires_at: DateTime<Utc>,
    },
}

impl PendingFlow {
    fn provider(&self) -> &str {
        match self {
            PendingFlow::Code { provider, .. } | PendingFlow::Device { provider, .. } => provider,
        }
    }

    fn expired(&self) -> bool {
        match self {
            PendingFlow::Code { started_at, .. } => {
                Utc::now() - *started_at > Duration::seconds(FLOW_TTL_SECS)
            }
            // The provider's own lifetime for the device code, which is the
            // authority on it: polling a code it has expired is an error, not
            // a longer wait.
            PendingFlow::Device { expires_at, .. } => Utc::now() > *expires_at,
        }
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

    /// Begin a browser authorization for `provider`.
    ///
    /// `gateway_base` is the gateway's own origin (`http://host:port`), used as
    /// the redirect target unless the provider's config names one.
    pub async fn start(
        &self,
        provider: &str,
        oauth: &OAuthConfig,
        gateway_base: &str,
    ) -> crate::Result<StartedFlow> {
        let Some(auth_url) = oauth.auth_url.as_deref() else {
            return Err(crate::error::SyscityError::Validation(format!(
                "provider '{provider}' declares no auth_url: it offers the device flow, which \
                 needs no redirect — start it in device mode"
            )));
        };

        let redirect_uri = oauth.redirect_uri(gateway_base);
        let flow = OAuthFlow::new();
        let authorization_url =
            flow.authorization_url_with_redirect(auth_url, oauth, &redirect_uri);
        let flow_id = flow.state().to_string();

        let entry = PendingFlow::Code {
            provider: provider.to_string(),
            oauth: oauth.clone(),
            redirect_uri,
            flow,
            started_at: Utc::now(),
        };

        self.supersede_and_insert(provider, flow_id.clone(), entry)
            .await;

        Ok(StartedFlow {
            flow_id,
            auth_url: authorization_url,
        })
    }

    /// Begin a *device* authorization (RFC 8628) for `provider`.
    ///
    /// This is the login that needs no browser on the gateway's host: the
    /// provider is asked for a code, the user types it wherever they do have a
    /// browser, and [`ProviderOAuthFlows::status`] polls for the result.
    pub async fn start_device(
        &self,
        provider: &str,
        oauth: &OAuthConfig,
        device_auth_url: &str,
    ) -> crate::Result<StartedDeviceFlow> {
        let device = DeviceFlow::new();
        let authorization = device.request_code(oauth, device_auth_url).await?;

        let prompt = DevicePrompt {
            user_code: authorization.user_code,
            verification_uri: authorization.verification_uri,
            interval_secs: authorization.interval_secs,
        };
        let flow_id = crate::model_router::oauth_flow::generate_flow_id();
        let entry = PendingFlow::Device {
            provider: provider.to_string(),
            oauth: oauth.clone(),
            device,
            device_code: authorization.device_code,
            prompt: prompt.clone(),
            last_polled_at: None,
            last_poll_error: None,
            expires_at: Utc::now() + Duration::seconds(authorization.expires_in_secs as i64),
        };

        self.supersede_and_insert(provider, flow_id.clone(), entry)
            .await;

        Ok(StartedDeviceFlow {
            flow_id,
            prompt,
            expires_in_secs: authorization.expires_in_secs,
        })
    }

    /// Replace any pending flow for `provider` with `entry`, dropping expired
    /// flows on the way.
    ///
    /// Starting again supersedes — but only for this provider. A superseded
    /// browser flow's verifier goes with it, so its state can no longer be
    /// completed; without this a second `auth_start` would leave two live states
    /// for one provider, and the first would still be completable. Another
    /// provider's pending authorization is none of our business.
    async fn supersede_and_insert(&self, provider: &str, flow_id: String, entry: PendingFlow) {
        let mut pending = self.pending.write().await;
        pending.retain(|_, flow| {
            if flow.expired() {
                tracing::debug!("Reaping expired provider OAuth flow for {}", flow.provider());
                return false;
            }
            flow.provider() != provider
        });
        pending.insert(flow_id, entry);
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
            .map(|flow| flow.provider().to_string())
    }

    /// Complete a browser flow: validate its state, exchange the code, store the
    /// refresh token, and return the provider that is now authorized.
    pub async fn complete(&self, state: &str, code: &str) -> crate::Result<String> {
        // Removed before the exchange, not after: the state is single-use, and
        // a replay must find nothing even if the exchange is still in flight.
        let entry = self.pending.write().await.remove(state);

        let Some(entry) = entry else {
            return Err(crate::error::SyscityError::Validation(
                "unknown or already-used OAuth state".to_string(),
            ));
        };

        let PendingFlow::Code {
            provider,
            oauth,
            redirect_uri,
            flow,
            started_at,
        } = entry
        else {
            // A device flow has no redirect and cannot be completed by one; it
            // is put back so polling can still finish it.
            let provider = entry.provider().to_string();
            self.pending.write().await.insert(state.to_string(), entry);
            return Err(crate::error::SyscityError::Validation(format!(
                "the pending flow for '{provider}' is a device authorization: it completes by \
                 polling, not by a redirect"
            )));
        };

        if Utc::now() - started_at > Duration::seconds(FLOW_TTL_SECS) {
            return Err(crate::error::SyscityError::Validation(format!(
                "the authorization flow for '{provider}' expired after {FLOW_TTL_SECS}s; \
                 start it again"
            )));
        }

        let credential = flow
            .exchange_code_with_redirect(code, &oauth, &redirect_uri)
            .await?;

        self.store_refresh_token(&provider, credential).await?;
        Ok(provider)
    }

    /// Store the refresh token out of an issued credential.
    ///
    /// A provider that issues none cannot be kept authorized: access tokens are
    /// deliberately memory-only, so there would be nothing left after a restart.
    /// Failing loudly here is what stops the flow from *appearing* to succeed.
    async fn store_refresh_token(
        &self,
        provider: &str,
        credential: Credential,
    ) -> crate::Result<()> {
        let refresh_token = match credential {
            Credential::OAuth2 { refresh_token, .. } => refresh_token,
            other => {
                return Err(crate::error::SyscityError::Validation(format!(
                    "the token exchange for '{provider}' returned {:?}, not an OAuth2 credential",
                    other.authorization_header().split(' ').next().unwrap_or("")
                )))
            }
        };

        let Some(refresh_token) = refresh_token else {
            return Err(crate::error::SyscityError::Validation(format!(
                "'{provider}' returned no refresh token; this provider cannot stay authorized \
                 across restarts"
            )));
        };

        let id = Self::refresh_token_id(provider);
        self.secrets.choose(&id).set(&id, &refresh_token).await
    }

    /// Drop expired flows and report how many were reaped.
    pub async fn reap_expired(&self) -> usize {
        let mut pending = self.pending.write().await;
        let before = pending.len();
        pending.retain(|_, flow| {
            if flow.expired() {
                tracing::debug!("Reaping expired provider OAuth flow for {}", flow.provider());
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
    ///
    /// For a device flow this is also the poll (RFC 8628 §3.4): the flow has no
    /// callback, so being asked about is what advances it. Polling here rather
    /// than in a spawned task is what keeps the flow's lifetime the client's
    /// business — there is no task to cancel, and nothing keeps polling after
    /// the client stops asking.
    pub async fn status(&self, provider: &str) -> AuthStatus {
        let pending = {
            let mut guard = self.pending.write().await;
            guard.retain(|_, flow| !flow.expired());
            guard
                .iter()
                .find(|(_, flow)| flow.provider() == provider)
                .map(|(state, _)| state.clone())
        };

        let Some(flow_id) = pending else {
            return AuthStatus {
                authorized: self.is_authorized(provider).await,
                ..Default::default()
            };
        };

        let Some(state) = self.device_state(&flow_id).await else {
            // A browser flow: it completes through the callback route, so there
            // is nothing to do but report it.
            return AuthStatus {
                authorized: self.is_authorized(provider).await,
                pending: Some(flow_id),
                device: None,
                poll_error: None,
            };
        };
        let prompt = state.prompt;

        // Not due yet: the provider asked to be polled this often, and the last
        // answer stands until it is.
        if !state.due {
            return AuthStatus {
                authorized: false,
                pending: Some(flow_id),
                device: Some(prompt),
                poll_error: state.last_poll_error,
            };
        }

        let waiting = |prompt: DevicePrompt, poll_error| AuthStatus {
            authorized: false,
            pending: Some(flow_id.clone()),
            device: Some(prompt),
            poll_error,
        };

        match self.poll_device(&flow_id).await {
            // Not yet: the user has not finished at the verification URI.
            Ok(DevicePoll::Pending) => {
                self.note_poll_error(&flow_id, None).await;
                waiting(prompt, None)
            }
            Ok(DevicePoll::SlowDown { interval_secs }) => {
                waiting(DevicePrompt { interval_secs, ..prompt }, None)
            }
            Ok(DevicePoll::Authorized(credential)) => {
                let stored = self.store_refresh_token(provider, *credential).await;
                // Either way the flow is over: it was answered.
                self.pending.write().await.remove(&flow_id);
                AuthStatus {
                    authorized: stored.is_ok(),
                    pending: None,
                    device: None,
                    poll_error: stored.err().map(|e| e.to_string()),
                }
            }
            // A known ending — the user refused, or the code expired. The flow
            // goes with it; reporting it as still pending would leave the client
            // waiting for something that will not come.
            Ok(DevicePoll::Refused { reason }) => {
                self.pending.write().await.remove(&flow_id);
                AuthStatus {
                    authorized: false,
                    pending: None,
                    device: None,
                    poll_error: Some(reason),
                }
            }
            // The poll could not be completed, which says nothing about the
            // authorization: the flow stays, and the client decides whether to
            // keep waiting.
            Err(reason) => {
                self.note_poll_error(&flow_id, Some(reason.clone())).await;
                waiting(prompt, Some(reason))
            }
        }
    }

    /// What a device flow says about itself without being polled: the prompt,
    /// whether a poll is due, and why the last one failed.
    async fn device_state(&self, flow_id: &str) -> Option<DeviceState> {
        let mut guard = self.pending.write().await;
        let Some(PendingFlow::Device {
            prompt,
            last_polled_at,
            last_poll_error,
            ..
        }) = guard.get_mut(flow_id)
        else {
            return None;
        };

        // A client polling faster than the provider asked is answered from
        // state: `slow_down` is a warning, and at some providers a throttle.
        let due = last_polled_at
            .map(|at| Utc::now() - at >= Duration::seconds(prompt.interval_secs as i64))
            .unwrap_or(true);
        if due {
            *last_polled_at = Some(Utc::now());
        }
        Some(DeviceState {
            prompt: prompt.clone(),
            due,
            last_poll_error: last_poll_error.clone(),
        })
    }

    /// Record the outcome of a poll, so a throttled answer can report it.
    async fn note_poll_error(&self, flow_id: &str, error: Option<String>) {
        let mut guard = self.pending.write().await;
        if let Some(PendingFlow::Device { last_poll_error, .. }) = guard.get_mut(flow_id) {
            *last_poll_error = error;
        }
    }

    /// Poll a device flow's token endpoint once.
    ///
    /// `Err` is "the poll could not be completed", which is not one of the
    /// outcomes [`DevicePoll`] names.
    async fn poll_device(&self, flow_id: &str) -> Result<DevicePoll, String> {
        let flow = {
            let guard = self.pending.read().await;
            match guard.get(flow_id) {
                Some(PendingFlow::Device { device, oauth, device_code, .. }) => {
                    Some((device.clone(), oauth.clone(), device_code.clone()))
                }
                _ => None,
            }
        };
        let Some((device, oauth, device_code)) = flow else {
            return Ok(DevicePoll::Pending);
        };

        device
            .poll(&oauth, &device_code)
            .await
            .map_err(|e| e.to_string())
    }

    /// Abandon any pending flow for `provider`. Returns whether one was pending.
    pub async fn cancel(&self, provider: &str) -> bool {
        let mut pending = self.pending.write().await;
        let before = pending.len();
        pending.retain(|_, flow| flow.provider() != provider);
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
            auth_url: Some("https://provider.example/authorize".to_string()),
            device_auth_url: None,
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

    /// A provider that offers the device flow: no browser endpoint at all.
    fn device_config(device_url: String, token_url: String) -> OAuthConfig {
        OAuthConfig {
            device_auth_url: Some(device_url),
            auth_url: None,
            ..oauth_config(token_url)
        }
    }

    fn device_endpoint(server: &MockServer) -> Mock {
        Mock::given(method("POST"))
            .and(path("/device"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "device_code": "dev-code",
                "user_code": "ABCD-EFGH",
                "verification_uri": "https://provider.example/activate",
                "expires_in": 900,
                "interval": 5,
            })))
    }

    /// The device token endpoint, answering with a token-endpoint *error body*
    /// (`authorization_pending` and friends are answers, not HTTP failures).
    fn device_poll(server: &MockServer, error: &str) -> Mock {
        // The form body is URL-encoded, so the grant type arrives with %3A for
        // its colons and is matched on the parameter that survives that.
        Mock::given(method("POST"))
            .and(path("/token"))
            .and(body_string_contains("device_code"))
            .respond_with(ResponseTemplate::new(400).set_body_json(serde_json::json!({
                "error": error,
            })))
    }

    /// Age a device flow's last poll, so the next `status` is due immediately
    /// rather than after the interval the test does not want to wait out.
    async fn due_now(flows: &ProviderOAuthFlows, flow_id: &str) {
        let mut pending = flows.pending.write().await;
        match pending.get_mut(flow_id).expect("pending") {
            PendingFlow::Device { last_polled_at, .. } => {
                *last_polled_at = Some(Utc::now() - Duration::seconds(60))
            }
            other => panic!("expected a device flow, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn start_builds_a_gateway_hosted_redirect() {
        let (flows, root) = flows("start");
        let config = oauth_config("https://provider.example/token".to_string());

        let started = flows
            .start("grok", &config, "http://127.0.0.1:18080")
            .await
            .unwrap();

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

        let started = flows
            .start("grok", &config, "http://127.0.0.1:18080")
            .await
            .unwrap();

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
        let started = flows
            .start("grok", &config, "http://127.0.0.1:18080")
            .await
            .unwrap();

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
        let started = flows
            .start("grok", &config, "http://127.0.0.1:18080")
            .await
            .unwrap();

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
        let started = flows
            .start("grok", &config, "http://127.0.0.1:18080")
            .await
            .unwrap();

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

        let first = flows
            .start("grok", &config, "http://127.0.0.1:18080")
            .await
            .unwrap();
        let second = flows
            .start("grok", &config, "http://127.0.0.1:18080")
            .await
            .unwrap();

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

        let first = flows
            .start("grok", &config, "http://127.0.0.1:18080")
            .await
            .unwrap();
        let second = flows
            .start("kimi", &config, "http://127.0.0.1:18080")
            .await
            .unwrap();

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
        let started = flows
            .start("grok", &config, "http://127.0.0.1:18080")
            .await
            .unwrap();
        assert!(flows.cancel("grok").await);

        assert_eq!(flows.status("grok").await.pending, None);
        assert!(flows.complete(&started.flow_id, "code").await.is_err());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn expired_flows_are_reaped_and_cannot_complete() {
        let (flows, root) = flows("expiry");
        let config = oauth_config("https://provider.example/token".to_string());
        let started = flows
            .start("grok", &config, "http://127.0.0.1:18080")
            .await
            .unwrap();

        // Age the flow past its window rather than waiting ten minutes.
        {
            let mut pending = flows.pending.write().await;
            match pending.get_mut(&started.flow_id).expect("flow is pending") {
                PendingFlow::Code { started_at, .. } => {
                    *started_at = Utc::now() - Duration::seconds(FLOW_TTL_SECS + 1)
                }
                other => panic!("expected a browser flow, got {other:?}"),
            }
        }

        assert_eq!(flows.reap_expired().await, 1);
        assert_eq!(flows.reap_expired().await, 0);
        assert_eq!(flows.status("grok").await.pending, None);
        let err = flows.complete(&started.flow_id, "code").await.unwrap_err();
        assert!(format!("{err}").contains("unknown or already-used"), "{err}");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// The device flow's whole point: a login that needs no browser *here*. The
    /// provider hands out a code, and we never build a redirect.
    #[tokio::test]
    async fn device_start_hands_out_a_code_to_type() {
        let server = MockServer::start().await;
        device_endpoint(&server).expect(1).mount(&server).await;

        let (flows, root) = flows("device_start");
        let config =
            device_config(format!("{}/device", server.uri()), format!("{}/token", server.uri()));

        let started = flows
            .start_device("grok", &config, config.device_auth_url.as_deref().unwrap())
            .await
            .unwrap();

        assert_eq!(started.prompt.user_code, "ABCD-EFGH");
        assert_eq!(started.prompt.verification_uri, "https://provider.example/activate");
        assert_eq!(started.prompt.interval_secs, 5);
        assert_eq!(started.expires_in_secs, 900);
        assert!(!started.flow_id.is_empty());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn device_status_reports_the_prompt_without_polling_faster_than_asked() {
        let server = MockServer::start().await;
        device_endpoint(&server).mount(&server).await;
        // One poll is owed. A client asking every two seconds must not turn that
        // into a request per ask: the provider asked for five.
        device_poll(&server, "authorization_pending")
            .expect(1)
            .mount(&server)
            .await;

        let (flows, root) = flows("device_pending");
        let config =
            device_config(format!("{}/device", server.uri()), format!("{}/token", server.uri()));
        let started = flows
            .start_device("grok", &config, config.device_auth_url.as_deref().unwrap())
            .await
            .unwrap();

        for _ in 0..3 {
            let status = flows.status("grok").await;
            assert!(!status.authorized);
            assert_eq!(status.pending.as_deref(), Some(started.flow_id.as_str()));
            let prompt = status.device.expect("the code has to reach the user");
            assert_eq!(prompt.user_code, "ABCD-EFGH");
            assert_eq!(prompt.verification_uri, "https://provider.example/activate");
            assert_eq!(status.poll_error, None);
        }

        // The mock's `expect(1)` is asserted when the server drops.
        drop(server);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn device_status_stores_the_refresh_token_when_the_user_finishes() {
        let server = MockServer::start().await;
        device_endpoint(&server).mount(&server).await;
        device_poll(&server, "authorization_pending")
            .up_to_n_times(1)
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/token"))
            .and(body_string_contains(
                "grant_type=urn%3Aietf%3Aparams%3Aoauth%3Agrant-type%3Adevice_code",
            ))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "access_token": "at",
                "refresh_token": "rt-device",
                "expires_in": 3600,
            })))
            .expect(1)
            .mount(&server)
            .await;

        let (flows, root) = flows("device_authorized");
        let config =
            device_config(format!("{}/device", server.uri()), format!("{}/token", server.uri()));
        let started = flows
            .start_device("grok", &config, config.device_auth_url.as_deref().unwrap())
            .await
            .unwrap();

        assert!(!flows.status("grok").await.authorized, "the user has not finished yet");

        due_now(&flows, &started.flow_id).await;
        let status = flows.status("grok").await;
        assert!(status.authorized);
        assert_eq!(status.pending, None, "an answered flow is not pending");
        assert_eq!(status.poll_error, None);

        // Stored where the resolver looks, and only the refresh token is.
        let id = ProviderOAuthFlows::refresh_token_id("grok");
        let store = flows.secrets.choose(&id);
        assert_eq!(store.get(&id).await.unwrap().as_deref(), Some("rt-device"));

        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn device_refusal_ends_the_flow_and_reports_the_reason() {
        let server = MockServer::start().await;
        device_endpoint(&server).mount(&server).await;
        device_poll(&server, "access_denied")
            .expect(1)
            .mount(&server)
            .await;

        let (flows, root) = flows("device_denied");
        let config =
            device_config(format!("{}/device", server.uri()), format!("{}/token", server.uri()));
        let started = flows
            .start_device("grok", &config, config.device_auth_url.as_deref().unwrap())
            .await
            .unwrap();

        let status = flows.status("grok").await;
        // The flow is over: leaving it pending would have the client waiting for
        // an answer the provider has already given.
        assert_eq!(status.pending, None);
        assert!(!status.authorized);
        assert!(
            status
                .poll_error
                .as_deref()
                .unwrap_or_default()
                .contains("access_denied"),
            "{:?}",
            status.poll_error
        );
        assert!(flows.complete(&started.flow_id, "code").await.is_err());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn a_failed_device_poll_leaves_the_flow_alone() {
        let server = MockServer::start().await;
        device_endpoint(&server).mount(&server).await;
        // A 500 is not an answer about the authorization, so nothing may be
        // concluded from it — least of all that the login failed.
        Mock::given(method("POST"))
            .and(path("/token"))
            .respond_with(ResponseTemplate::new(500).set_body_string("upstream is unwell"))
            .mount(&server)
            .await;

        let (flows, root) = flows("device_unreachable");
        let config =
            device_config(format!("{}/device", server.uri()), format!("{}/token", server.uri()));
        let started = flows
            .start_device("grok", &config, config.device_auth_url.as_deref().unwrap())
            .await
            .unwrap();

        let status = flows.status("grok").await;
        assert_eq!(status.pending.as_deref(), Some(started.flow_id.as_str()));
        assert!(status.device.is_some(), "the user still needs the code");
        assert!(!status.authorized);
        let error = status.poll_error.unwrap_or_default();
        assert!(error.contains("500") || error.contains("unwell"), "{error}");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// A device flow has no redirect, so the callback route must not be able to
    /// end it — and must not consume it either.
    #[tokio::test]
    async fn a_device_flow_cannot_be_completed_by_a_redirect() {
        let server = MockServer::start().await;
        device_endpoint(&server).mount(&server).await;

        let (flows, root) = flows("device_callback");
        let config =
            device_config(format!("{}/device", server.uri()), format!("{}/token", server.uri()));
        let started = flows
            .start_device("grok", &config, config.device_auth_url.as_deref().unwrap())
            .await
            .unwrap();

        let err = flows.complete(&started.flow_id, "code").await.unwrap_err();
        assert!(format!("{err}").contains("device authorization"), "{err}");
        assert_eq!(
            flows.status("grok").await.pending.as_deref(),
            Some(started.flow_id.as_str()),
            "the flow must survive an attempt to complete it the wrong way"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// A provider with no authorization endpoint and no device endpoint has
    /// nothing to start; both modes say so rather than building a URL that
    /// cannot work.
    #[tokio::test]
    async fn a_provider_with_no_endpoint_is_refused() {
        let (flows, root) = flows("no_endpoints");
        let config = OAuthConfig {
            auth_url: None,
            ..oauth_config("https://provider.example/token".to_string())
        };

        let err = flows
            .start("grok", &config, "http://127.0.0.1:18080")
            .await
            .unwrap_err();
        assert!(format!("{err}").contains("no auth_url"), "{err}");
        let _ = std::fs::remove_dir_all(&root);
    }
}
