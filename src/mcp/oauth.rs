//! OAuth 2.0 token management — types, manager handle, background actor,
//! token persistence, and the PKCE authorization callback server.
//!
//! Sensitive credentials are split across storage tiers:
//! - `access_token` → in-memory cache only (never persisted).
//! - `refresh_token` → routed store namespace `mcp-oauth` (keyring → file).
//! - non-sensitive metadata (`token_url` / `client_id` / `expires_at`) → a
//!   `0600` sidecar JSON under `~/.syscity/mcp_tokens/{id}.json`.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use tokio::net::TcpListener;
use tokio::sync::{mpsc, oneshot, RwLock};
use tracing::{info, warn};

use crate::mcp::{McpEvent, McpManager, McpServerConfig};
use crate::secrets::{SecretId, SecretOrigin, SecretStore, SecretStoreHandle};

// ─────────────────────────────────────────────
// Token data
// ─────────────────────────────────────────────

/// OAuth 2.0 token data handed to consumers (a live access token plus the
/// context needed to refresh it). Only used in memory — see the module doc
/// for how the fields are persisted.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OAuthTokens {
    pub access_token: String,
    /// Token endpoint URL used for refreshing.
    pub token_url: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub refresh_token: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<i64>,
    /// OAuth client ID used for token refresh.
    pub client_id: String,
}

/// Non-sensitive OAuth metadata persisted to the `mcp_tokens/{id}.json`
/// sidecar. Never contains an access or refresh token.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct OAuthMetadata {
    /// Token endpoint URL used for refreshing.
    token_url: String,
    /// OAuth client ID used for token refresh.
    client_id: String,
    /// Expiry of the last known access token (seconds since epoch).
    #[serde(skip_serializing_if = "Option::is_none")]
    expires_at: Option<i64>,
}

/// Old-format token sidecar that still carries plaintext `access_token` /
/// `refresh_token` fields (pre-split-persistence, design §8.6). Every field is
/// optional so a metadata-only file also parses; the migration only acts when
/// a token field is present.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct LegacyTokenFile {
    access_token: Option<String>,
    refresh_token: Option<String>,
    token_url: Option<String>,
    client_id: Option<String>,
    expires_at: Option<i64>,
}

// ─────────────────────────────────────────────
// Actor types
// ─────────────────────────────────────────────

/// Internal state for a pending OAuth flow (inside the actor only).
struct PendingFlowState {
    cancel_tx: oneshot::Sender<()>,
}

/// A cached token. Access tokens live only in this in-memory cache; nothing
/// here is ever written to disk.
struct CachedToken {
    tokens: OAuthTokens,
}

/// Commands sent to the `OAuthManagerActor` via its mpsc channel.
pub(crate) enum OAuthCommand {
    StartFlow {
        server_id: String,
        config: Box<McpServerConfig>,
        resp_tx: oneshot::Sender<crate::Result<String>>,
    },
    CancelFlow {
        server_id: String,
    },
    GetToken {
        server_id: String,
        resp_tx: oneshot::Sender<Option<OAuthTokens>>,
    },
    HasToken {
        server_id: String,
        resp_tx: oneshot::Sender<bool>,
    },
    CallbackComplete {
        server_id: String,
        result: crate::Result<OAuthTokens>,
    },
    /// Drop the cached token and delete the persisted token data (e.g. when a
    /// server is removed).
    ClearToken {
        server_id: String,
    },
    /// Request the actor to shut down (currently unused, available for
    /// graceful teardown).
    #[allow(dead_code)]
    Shutdown,
}

// ─────────────────────────────────────────────
// OAuthManager handle
// ─────────────────────────────────────────────

/// Cloneable handle to the `OAuthManagerActor` background task.
#[derive(Clone)]
pub struct OAuthManager {
    pub(crate) cmd_tx: mpsc::UnboundedSender<OAuthCommand>,
}

impl OAuthManager {
    /// Create a new `OAuthManager` handle and the receiver half for the actor.
    pub(crate) fn new() -> (Self, mpsc::UnboundedReceiver<OAuthCommand>) {
        let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
        (Self { cmd_tx }, cmd_rx)
    }

    /// Start an OAuth 2.1 + PKCE flow. Returns the authorization URL.
    pub async fn start_flow(
        &self,
        server_id: String,
        config: McpServerConfig,
    ) -> crate::Result<String> {
        let (resp_tx, resp_rx) = oneshot::channel();
        let _ = self.cmd_tx.send(OAuthCommand::StartFlow {
            server_id,
            config: Box::new(config),
            resp_tx,
        });
        resp_rx.await.map_err(|_| {
            crate::error::SyscityError::Internal("OAuth manager dropped".to_string())
        })?
    }

    /// Cancel a pending OAuth flow.
    pub async fn cancel_flow(&self, server_id: String) {
        let _ = self.cmd_tx.send(OAuthCommand::CancelFlow { server_id });
    }

    /// Get cached OAuth tokens for a server, refreshing on demand if the
    /// cached access token is missing or expired.
    pub async fn get_token(&self, server_id: String) -> Option<OAuthTokens> {
        let (resp_tx, resp_rx) = oneshot::channel();
        let _ = self
            .cmd_tx
            .send(OAuthCommand::GetToken { server_id, resp_tx });
        resp_rx.await.ok().flatten()
    }

    /// Check if an access token can be obtained for a server (fresh cache or
    /// a refresh path).
    pub async fn has_token(&self, server_id: String) -> bool {
        let (resp_tx, resp_rx) = oneshot::channel();
        let _ = self
            .cmd_tx
            .send(OAuthCommand::HasToken { server_id, resp_tx });
        resp_rx.await.unwrap_or(false)
    }

    /// Clear the cached token and delete all persisted token data.
    pub async fn clear_token(&self, server_id: String) {
        let _ = self.cmd_tx.send(OAuthCommand::ClearToken { server_id });
    }
}

// ─────────────────────────────────────────────
// OAuthManagerActor — background task
// ─────────────────────────────────────────────

/// Background actor that owns the token cache and manages OAuth flows.
pub(crate) struct OAuthManagerActor {
    cmd_rx: mpsc::UnboundedReceiver<OAuthCommand>,
    cmd_tx: mpsc::UnboundedSender<OAuthCommand>,
    event_tx: Arc<RwLock<Option<mpsc::UnboundedSender<McpEvent>>>>,
    token_cache: HashMap<String, CachedToken>,
    pending_flows: HashMap<String, PendingFlowState>,
    /// Secret-store instance handle for refresh-token persistence.
    secrets: Arc<SecretStoreHandle>,
}

impl OAuthManagerActor {
    pub(crate) fn new(
        cmd_rx: mpsc::UnboundedReceiver<OAuthCommand>,
        cmd_tx: mpsc::UnboundedSender<OAuthCommand>,
        event_tx: Arc<RwLock<Option<mpsc::UnboundedSender<McpEvent>>>>,
        secrets: Arc<SecretStoreHandle>,
    ) -> Self {
        Self {
            cmd_rx,
            cmd_tx,
            event_tx,
            token_cache: HashMap::new(),
            pending_flows: HashMap::new(),
            secrets,
        }
    }

    /// Spawn the actor as a background task.
    pub(crate) fn spawn(self) {
        tokio::spawn(self.run());
    }

    /// Main run loop: process commands + periodic token refresh.
    async fn run(mut self) {
        // Access tokens are memory-only, so there is nothing to preload from
        // disk; tokens are (re)acquired on demand in `handle_get_token`.
        let mut refresh_interval = tokio::time::interval(tokio::time::Duration::from_secs(60));

        loop {
            tokio::select! {
                cmd = self.cmd_rx.recv() => {
                    match cmd {
                        None => break,
                        Some(OAuthCommand::StartFlow { server_id, config, resp_tx }) => {
                            let _ = resp_tx.send(self.handle_start_flow(&server_id, &config).await);
                        }
                        Some(OAuthCommand::CancelFlow { server_id }) => {
                            self.handle_cancel_flow(&server_id).await;
                        }
                        Some(OAuthCommand::GetToken { server_id, resp_tx }) => {
                            let _ = resp_tx.send(self.handle_get_token(&server_id).await);
                        }
                        Some(OAuthCommand::HasToken { server_id, resp_tx }) => {
                            let _ = resp_tx.send(self.handle_has_token(&server_id).await);
                        }
                        Some(OAuthCommand::CallbackComplete { server_id, result }) => {
                            self.handle_callback_complete(&server_id, result).await;
                        }
                        Some(OAuthCommand::ClearToken { server_id }) => {
                            self.handle_clear_token(&server_id).await;
                        }
                        Some(OAuthCommand::Shutdown) => break,
                    }
                }
                _ = refresh_interval.tick() => {
                    self.refresh_expiring_tokens().await;
                }
            }
        }
    }

    async fn emit_event(&self, event: McpEvent) {
        if let Some(tx) = self.event_tx.read().await.as_ref() {
            let _ = tx.send(event);
        }
    }

    async fn handle_start_flow(
        &mut self,
        server_id: &str,
        config: &McpServerConfig,
    ) -> crate::Result<String> {
        let url = config.url.as_deref().ok_or_else(|| {
            crate::error::SyscityError::Internal("Remote MCP server has no URL".to_string())
        })?;

        let http_client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(10))
            .build()
            .map_err(|e| {
                crate::error::SyscityError::Internal(format!("Failed to build HTTP client: {e}"))
            })?;

        // 1. Discover OAuth endpoints: explicit config → RFC 8414 well-known →
        //    RFC 9728 protected-resource metadata → known providers (GitHub).
        let (authorization_endpoint, token_endpoint) =
            discover_oauth_endpoints(&http_client, url, config)
                .await
                .map_err(|e| {
                    crate::error::SyscityError::Internal(format!(
                        "OAuth discovery failed for '{server_id}': {e}"
                    ))
                })?;

        // 2. Generate PKCE challenge
        let code_verifier = McpManager::generate_code_verifier();
        let code_challenge = McpManager::generate_code_challenge(&code_verifier);
        let state = McpManager::generate_random_state();

        let client_id = config
            .client_id
            .clone()
            .unwrap_or_else(|| "syscity".to_string());

        let scopes = config.scopes.clone().unwrap_or_default();

        // 3. Bind local callback listener
        let listener = TcpListener::bind("127.0.0.1:0").await.map_err(|e| {
            crate::error::SyscityError::Internal(format!("Failed to bind callback port: {e}"))
        })?;
        let callback_port = listener
            .local_addr()
            .map_err(|e| {
                crate::error::SyscityError::Internal(format!("Failed to get local addr: {e}"))
            })?
            .port();

        let redirect_uri = format!("http://127.0.0.1:{callback_port}/callback");

        // 4. Build authorization URL
        let mut auth_url = format!(
            "{authorization_endpoint}?response_type=code&client_id={}&redirect_uri={}&code_challenge={code_challenge}&code_challenge_method=S256&state={state}",
            urlencoding(&client_id),
            urlencoding(&redirect_uri),
        );
        if !scopes.is_empty() {
            auth_url.push_str(&format!("&scope={}", urlencoding(&scopes)));
        }

        // 5. Create cancel channel
        let (cancel_tx, cancel_rx) = oneshot::channel::<()>();

        // 6. Store pending flow state
        self.pending_flows
            .insert(server_id.to_string(), PendingFlowState { cancel_tx });

        // 7. Spawn callback server task (sends CallbackComplete back to actor)
        let sv_id = server_id.to_string();
        let cmd_tx = self.cmd_tx.clone();
        tokio::spawn(async move {
            let result = run_callback_server(
                listener,
                &token_endpoint,
                &code_verifier,
                &state,
                &client_id,
                &redirect_uri,
                cancel_rx,
            )
            .await;

            let _ = cmd_tx.send(OAuthCommand::CallbackComplete { server_id: sv_id, result });
        });

        // Notify clients that a flow has started so the UI can surface the
        // authorization URL even when the connect call came from elsewhere
        // (e.g. CLI or REST).
        self.emit_event(McpEvent::AuthRequired {
            server_id: server_id.to_string(),
            auth_url: auth_url.clone(),
        })
        .await;

        Ok(auth_url)
    }

    async fn handle_cancel_flow(&mut self, server_id: &str) {
        if let Some(flow) = self.pending_flows.remove(server_id) {
            let _ = flow.cancel_tx.send(());
            self.emit_event(McpEvent::AuthFailed {
                server_id: server_id.to_string(),
                reason: "cancelled_by_user".to_string(),
            })
            .await;
        }
    }

    /// Fetch tokens for a server. Prefers a fresh in-memory access token;
    /// otherwise performs an on-demand refresh using the persisted refresh
    /// token, so a restarted process can still connect without re-authorizing.
    async fn handle_get_token(&mut self, server_id: &str) -> Option<OAuthTokens> {
        if let Some(cached) = self.token_cache.get(server_id) {
            if tokens_fresh(&cached.tokens) {
                return Some(cached.tokens.clone());
            }
            self.token_cache.remove(server_id);
        }

        let refresh_token = load_refresh_token(&self.secrets, server_id).await?;
        let meta = load_metadata(server_id).await?;

        let tokens = match self
            .refresh_via_http(server_id, &meta, &refresh_token)
            .await
        {
            Ok(t) => t,
            Err(e) => {
                warn!("On-demand OAuth refresh failed for '{server_id}': {e}");
                return None;
            }
        };

        self.token_cache
            .insert(server_id.to_string(), CachedToken { tokens: tokens.clone() });
        Some(tokens)
    }

    /// Whether an access token can be obtained: a fresh cached token, or a
    /// persisted refresh path.
    async fn handle_has_token(&mut self, server_id: &str) -> bool {
        if let Some(cached) = self.token_cache.get(server_id) {
            if tokens_fresh(&cached.tokens) {
                return true;
            }
        }
        load_refresh_token(&self.secrets, server_id).await.is_some()
            && load_metadata(server_id).await.is_some()
    }

    async fn handle_clear_token(&mut self, server_id: &str) {
        self.token_cache.remove(server_id);
        if let Err(e) = delete_refresh_token(&self.secrets, server_id).await {
            warn!("Failed to delete refresh token for '{server_id}': {e}");
        }
        let path = match token_path_for(server_id) {
            Ok(p) => p,
            Err(e) => {
                warn!("Invalid server id for token deletion '{server_id}': {e}");
                return;
            }
        };
        if let Err(e) = tokio::fs::remove_file(&path).await {
            if e.kind() != std::io::ErrorKind::NotFound {
                warn!("Failed to delete token file for '{server_id}': {e}");
            }
        }
        info!("Cleared stored OAuth token for '{server_id}'");
    }

    async fn handle_callback_complete(
        &mut self,
        server_id: &str,
        result: crate::Result<OAuthTokens>,
    ) {
        // Clean up pending flow
        self.pending_flows.remove(server_id);

        match result {
            Ok(tokens) => {
                // Split persistence: access token stays in memory only; the
                // refresh token and non-sensitive metadata go to disk.
                persist_metadata(
                    server_id,
                    &OAuthMetadata {
                        token_url: tokens.token_url.clone(),
                        client_id: tokens.client_id.clone(),
                        expires_at: tokens.expires_at,
                    },
                )
                .await
                .map_err(|e| {
                    warn!("Failed to persist OAuth metadata for '{server_id}': {e}");
                })
                .ok();
                if let Some(refresh) = &tokens.refresh_token {
                    persist_refresh_token(&self.secrets, server_id, refresh)
                        .await
                        .map_err(|e| {
                            warn!("Failed to persist refresh token for '{server_id}': {e}");
                        })
                        .ok();
                }

                self.token_cache
                    .insert(server_id.to_string(), CachedToken { tokens: tokens.clone() });

                self.emit_event(McpEvent::AuthComplete {
                    server_id: server_id.to_string(),
                })
                .await;
            }
            Err(e) => {
                warn!("OAuth flow failed for '{server_id}': {e}");
                self.emit_event(McpEvent::AuthFailed {
                    server_id: server_id.to_string(),
                    reason: e.to_string(),
                })
                .await;
            }
        }
    }

    /// Iterate the token cache and refresh any tokens expiring within 5 minutes.
    async fn refresh_expiring_tokens(&mut self) {
        let now = chrono::Utc::now().timestamp();
        let refresh_window = now + 300; // 5 minutes from now

        // Collect expiring server IDs + their token data to avoid borrow
        // conflicts.
        let expiring: Vec<(String, OAuthTokens)> = self
            .token_cache
            .iter()
            .filter(|(_, c)| {
                c.tokens.expires_at.is_some_and(|exp| exp <= refresh_window)
                    && c.tokens.refresh_token.is_some()
            })
            .map(|(id, c)| (id.clone(), c.tokens.clone()))
            .collect();

        for (server_id, tokens) in &expiring {
            let meta = OAuthMetadata {
                token_url: tokens.token_url.clone(),
                client_id: tokens.client_id.clone(),
                expires_at: tokens.expires_at,
            };
            let refresh = tokens.refresh_token.clone().unwrap_or_default();

            match self.refresh_via_http(server_id, &meta, &refresh).await {
                Ok(updated) => {
                    persist_metadata(
                        server_id,
                        &OAuthMetadata {
                            token_url: updated.token_url.clone(),
                            client_id: updated.client_id.clone(),
                            expires_at: updated.expires_at,
                        },
                    )
                    .await
                    .map_err(|e| {
                        warn!("Failed to persist refreshed metadata for '{server_id}': {e}");
                    })
                    .ok();
                    if let Some(r) = &updated.refresh_token {
                        persist_refresh_token(&self.secrets, server_id, r).await.map_err(|e| {
                            warn!("Failed to persist refreshed refresh token for '{server_id}': {e}");
                        }).ok();
                    }
                    self.token_cache
                        .insert(server_id.clone(), CachedToken { tokens: updated });

                    info!("OAuth token refreshed for '{server_id}'");

                    self.emit_event(McpEvent::TokenRefreshed { server_id: server_id.clone() })
                        .await;
                }
                Err(e) => {
                    warn!("Token refresh failed for '{server_id}': {e}");
                }
            }
        }
    }

    /// Perform a refresh-token grant over HTTP and return the refreshed tokens.
    async fn refresh_via_http(
        &self,
        server_id: &str,
        meta: &OAuthMetadata,
        refresh_token: &str,
    ) -> crate::Result<OAuthTokens> {
        let http_client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(10))
            .build()
            .map_err(|e| {
                crate::error::SyscityError::Internal(format!("Failed to build HTTP client: {e}"))
            })?;

        let body = format!(
            "grant_type=refresh_token&refresh_token={}&client_id={}",
            refresh_token, meta.client_id
        );

        let resp = http_client
            .post(&meta.token_url)
            .header("Content-Type", "application/x-www-form-urlencoded")
            .header("Accept", "application/json")
            .body(body)
            .send()
            .await
            .map_err(|e| {
                crate::error::SyscityError::Internal(format!(
                    "Token refresh request failed for '{server_id}': {e}"
                ))
            })?;

        if !resp.status().is_success() {
            let status = resp.status();
            let error_text = resp.text().await.unwrap_or_default();
            return Err(crate::error::SyscityError::Internal(format!(
                "Token refresh failed for '{server_id}': HTTP {status} - {error_text}"
            )));
        }

        let data: serde_json::Value = resp.json().await.map_err(|e| {
            crate::error::SyscityError::Internal(format!(
                "Failed to parse refresh response for '{server_id}': {e}"
            ))
        })?;

        let new_access = data["access_token"]
            .as_str()
            .map(String::from)
            .ok_or_else(|| {
                crate::error::SyscityError::Internal(format!(
                    "Missing access_token in refresh response for '{server_id}'"
                ))
            })?;
        let new_refresh = data["refresh_token"]
            .as_str()
            .map(String::from)
            .unwrap_or_else(|| refresh_token.to_string());
        let new_expires = data["expires_in"]
            .as_i64()
            .map(|secs| chrono::Utc::now().timestamp() + secs);

        Ok(OAuthTokens {
            access_token: new_access,
            token_url: meta.token_url.clone(),
            refresh_token: Some(new_refresh),
            expires_at: new_expires,
            client_id: meta.client_id.clone(),
        })
    }
}

// ─────────────────────────────────────────────
// Token persistence
// ─────────────────────────────────────────────

/// Directory for MCP OAuth metadata (~/.syscity/mcp_tokens).
pub fn mcp_tokens_dir() -> PathBuf {
    crate::dirs::syscity_dir().join("mcp_tokens")
}

/// Path to the metadata file for a specific server. The server id is sanitized
/// so it cannot escape the tokens directory.
pub fn token_path_for(server_id: &str) -> crate::Result<PathBuf> {
    let id = crate::secrets::sanitize_entity(server_id)?;
    Ok(mcp_tokens_dir().join(format!("{id}.json")))
}

/// Whether a token bundle has a currently-usable access token (unexpired, or
/// no expiry recorded).
fn tokens_fresh(tokens: &OAuthTokens) -> bool {
    match tokens.expires_at {
        Some(exp) => chrono::Utc::now().timestamp() < exp - 60,
        None => true,
    }
}

/// The refresh-token backend — routed by the gateway's secret-store handle
/// (keyring → `~/.syscity/secrets/mcp-oauth`).
fn refresh_token_store(secrets: &SecretStoreHandle) -> Arc<dyn SecretStore> {
    secrets.route("mcp-oauth")
}

fn refresh_token_id(server_id: &str) -> SecretId {
    SecretId::new("mcp-oauth", server_id, "refresh_token")
}

/// Persist a refresh token for a server.
async fn persist_refresh_token(
    secrets: &SecretStoreHandle,
    server_id: &str,
    refresh: &str,
) -> crate::Result<()> {
    refresh_token_store(secrets)
        .set(&refresh_token_id(server_id), refresh, SecretOrigin::SystemGenerated)
        .await
}

/// Load a persisted refresh token for a server.
async fn load_refresh_token(secrets: &SecretStoreHandle, server_id: &str) -> Option<String> {
    refresh_token_store(secrets)
        .get(&refresh_token_id(server_id))
        .await
        .ok()
        .flatten()
}

/// Delete a persisted refresh token for a server (missing is not an error).
async fn delete_refresh_token(secrets: &SecretStoreHandle, server_id: &str) -> crate::Result<()> {
    refresh_token_store(secrets)
        .delete(&refresh_token_id(server_id))
        .await
}

/// Write non-sensitive OAuth metadata to the sidecar JSON (atomic + `0600`).
async fn persist_metadata(server_id: &str, meta: &OAuthMetadata) -> crate::Result<()> {
    persist_metadata_to(&mcp_tokens_dir(), server_id, meta).await
}

/// Write metadata to an explicit directory — used by the live path and by the
/// legacy-token migration, which must rewrite the very sidecar it scanned.
async fn persist_metadata_to(
    dir: &Path,
    server_id: &str,
    meta: &OAuthMetadata,
) -> crate::Result<()> {
    let id = crate::secrets::sanitize_entity(server_id)?;
    let path = dir.join(format!("{id}.json"));
    tokio::fs::create_dir_all(dir).await?;
    set_dir_perms(dir).await?;

    let json = serde_json::to_string(meta)?;
    let tmp = path.with_extension("json.tmp");
    tokio::fs::write(&tmp, json).await?;
    set_file_perms(&tmp).await?;
    tokio::fs::rename(&tmp, &path).await?;
    Ok(())
}

/// Read non-sensitive OAuth metadata from the sidecar JSON, if present.
async fn load_metadata(server_id: &str) -> Option<OAuthMetadata> {
    let path = token_path_for(server_id).ok()?;
    let data = tokio::fs::read_to_string(&path).await.ok()?;
    serde_json::from_str(&data).ok()
}

/// One-time migration of legacy `mcp_tokens/{id}.json` sidecars that still
/// carry plaintext `access_token` / `refresh_token` fields written by an older
/// format, into the split persistence model (design §8.6): the refresh token
/// moves into the routed `mcp-oauth` store and the access token is dropped
/// (memory-only). Idempotent — metadata-only sidecars are skipped.
///
/// Atomic and rollback-safe: the refresh token is persisted to the store
/// *before* the sidecar is rewritten, so a failure at any step leaves the
/// plaintext file intact and the refresh path is never lost.
pub async fn migrate_legacy_mcp_tokens(secrets: &SecretStoreHandle) -> crate::Result<()> {
    let store = refresh_token_store(secrets);
    migrate_legacy_mcp_tokens_with_store(mcp_tokens_dir(), store).await
}

async fn migrate_legacy_mcp_tokens_with_store(
    dir: PathBuf,
    store: Arc<dyn SecretStore>,
) -> crate::Result<()> {
    if tokio::fs::metadata(&dir).await.is_err() || !dir.is_dir() {
        return Ok(());
    }

    let mut migrated = 0usize;
    let mut failed = 0usize;

    let mut entries = match tokio::fs::read_dir(&dir).await {
        Ok(e) => e,
        Err(e) => {
            warn!("Cannot read legacy mcp_tokens dir {}: {e}", dir.display());
            return Ok(());
        }
    };

    while let Ok(Some(entry)) = entries.next_entry().await {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        let Some(server_id) = path.file_stem().and_then(|s| s.to_str()).map(String::from) else {
            continue;
        };

        let content = match tokio::fs::read_to_string(&path).await {
            Ok(c) => c,
            Err(_) => continue,
        };
        let legacy: LegacyTokenFile = match serde_json::from_str(&content) {
            Ok(f) => f,
            Err(e) => {
                warn!("Skipping legacy mcp_tokens {} (unreadable): {e}", path.display());
                failed += 1;
                continue;
            }
        };

        // Current-format metadata-only sidecar — nothing to do.
        if legacy.access_token.is_none() && legacy.refresh_token.is_none() {
            continue;
        }

        // Move the refresh token into the store *before* touching the file.
        if let Some(refresh) = legacy.refresh_token {
            let id = refresh_token_id(&server_id);
            let already = store.get(&id).await.ok().flatten().is_some();
            if !already {
                if let Err(e) = store
                    .set(&id, &refresh, SecretOrigin::SystemGenerated)
                    .await
                {
                    warn!("Failed to migrate refresh token for '{server_id}': {e}");
                    failed += 1;
                    continue;
                }
            }
        }

        // Rewrite the sidecar as metadata-only; this removes the plaintext
        // token fields from disk (atomic + 0600).
        let meta = OAuthMetadata {
            token_url: legacy.token_url.unwrap_or_default(),
            client_id: legacy.client_id.unwrap_or_default(),
            expires_at: legacy.expires_at,
        };
        if let Err(e) = persist_metadata_to(&dir, &server_id, &meta).await {
            warn!("Failed to rewrite legacy mcp_tokens {}: {e}", path.display());
            failed += 1;
            continue;
        }
        migrated += 1;
    }

    info!("mcp_tokens legacy migration: {migrated} migrated, {failed} failed");
    Ok(())
}

#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;

#[cfg(unix)]
async fn set_dir_perms(dir: &Path) -> crate::Result<()> {
    tokio::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700)).await?;
    Ok(())
}

#[cfg(not(unix))]
async fn set_dir_perms(_dir: &Path) -> crate::Result<()> {
    Ok(())
}

#[cfg(unix)]
async fn set_file_perms(path: &Path) -> crate::Result<()> {
    tokio::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).await?;
    Ok(())
}

#[cfg(not(unix))]
async fn set_file_perms(_path: &Path) -> crate::Result<()> {
    Ok(())
}

/// Minimal percent-encoding for OAuth URL parameters.
fn urlencoding(input: &str) -> String {
    let mut result = String::with_capacity(input.len());
    for byte in input.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                result.push(byte as char);
            }
            b' ' => result.push_str("%20"),
            _ => result.push_str(&format!("%{byte:02X}")),
        }
    }
    result
}

// ─────────────────────────────────────────────
// OAuth endpoint discovery
// ─────────────────────────────────────────────

/// An `(authorization_endpoint, token_endpoint)` pair.
type DiscoveredEndpoints = (String, String);

/// Discover OAuth endpoints for a remote MCP server.
///
/// Strategy (in order of preference):
/// 1. Explicit `auth_url` / `token_url` from the server config.
/// 2. RFC 8414 well-known metadata at the server origin
///    (`/.well-known/oauth-authorization-server`).
/// 3. RFC 9728 protected-resource metadata: the server advertises a
///    `resource_metadata` URL via the `WWW-Authenticate` challenge; that
///    document lists `authorization_servers`, each probed for RFC 8414 / OIDC
///    metadata.
/// 4. A registry of known providers that publish no discovery document at all
///    (e.g. GitHub's OAuth endpoints).
async fn discover_oauth_endpoints(
    http: &reqwest::Client,
    server_url: &str,
    config: &McpServerConfig,
) -> Result<DiscoveredEndpoints, String> {
    // 1. Explicit config wins.
    if let (Some(auth), Some(token)) = (config.auth_url.as_deref(), config.token_url.as_deref()) {
        return Ok((auth.to_string(), token.to_string()));
    }

    let origin = McpManager::origin_from_url(server_url);

    // 2. RFC 8414 well-known document at the server origin.
    let well_known = format!("{origin}/.well-known/oauth-authorization-server");
    if let Some(endpoints) = fetch_metadata_endpoints(http, &well_known).await {
        return Ok(endpoints);
    }

    // 3. RFC 9728 protected-resource metadata.
    let mut auth_servers: Vec<String> = Vec::new();
    if let Some(resource_meta_url) = fetch_resource_metadata_url(http, server_url).await {
        auth_servers = fetch_authorization_servers(http, &resource_meta_url).await;
        for issuer in &auth_servers {
            let rfc8414 = format!("{issuer}/.well-known/oauth-authorization-server");
            if let Some(endpoints) = fetch_metadata_endpoints(http, &rfc8414).await {
                return Ok(endpoints);
            }
            let oidc = format!("{issuer}/.well-known/openid-configuration");
            if let Some(endpoints) = fetch_metadata_endpoints(http, &oidc).await {
                return Ok(endpoints);
            }
        }
    }

    // 4. Known-provider registry, matched against the advertised authorization
    //    servers first, then the server origin.
    let issuers = auth_servers
        .iter()
        .map(|s| s.as_str())
        .chain(std::iter::once(origin.as_str()));
    for issuer in issuers {
        if let Some(endpoints) = known_provider_endpoints(issuer) {
            return Ok(endpoints);
        }
    }

    Err(format!(
        "the server exposes no OAuth metadata ({well_known} is unavailable and no \
         supported resource-metadata discovery was found); set auth_url and token_url \
         in the server config to use it directly"
    ))
}

/// Fetch an RFC 8414 / OIDC discovery document and extract the OAuth endpoints.
async fn fetch_metadata_endpoints(
    http: &reqwest::Client,
    metadata_url: &str,
) -> Option<DiscoveredEndpoints> {
    let resp = http.get(metadata_url).send().await.ok()?;
    if !resp.status().is_success() {
        return None;
    }
    let doc: serde_json::Value = resp.json().await.ok()?;
    let authorization_endpoint = doc.get("authorization_endpoint")?.as_str()?;
    let token_endpoint = doc.get("token_endpoint")?.as_str()?;
    Some((authorization_endpoint.to_string(), token_endpoint.to_string()))
}

/// Send an unauthenticated request to the MCP endpoint and return the
/// `resource_metadata` URL advertised in the `WWW-Authenticate` challenge
/// (RFC 9728). Probes with a POST `initialize` first, then a plain GET.
async fn fetch_resource_metadata_url(http: &reqwest::Client, server_url: &str) -> Option<String> {
    let request = http
        .post(server_url)
        .header("Accept", "application/json, text/event-stream")
        .json(&serde_json::json!({
            "jsonrpc": "2.0",
            "id": 0,
            "method": "initialize",
            "params": {
                "protocolVersion": "2025-06-18",
                "capabilities": {},
                "clientInfo": { "name": "syscity", "version": env!("CARGO_PKG_VERSION") },
            }
        }));
    let resp = request.send().await.ok()?;
    if let Some(meta) = www_authenticate_resource_metadata(&resp) {
        return Some(meta);
    }

    let resp = http.get(server_url).send().await.ok()?;
    www_authenticate_resource_metadata(&resp)
}

/// Extract the `resource_metadata` parameter from a response's
/// `WWW-Authenticate` header if present.
fn www_authenticate_resource_metadata(resp: &reqwest::Response) -> Option<String> {
    let header = resp.headers().get("www-authenticate")?.to_str().ok()?;
    extract_resource_metadata(header)
}

/// Extract the `resource_metadata` value from a `WWW-Authenticate` header,
/// e.g. `Bearer ..., resource_metadata="https://example.com/metadata"`.
fn extract_resource_metadata(www_authenticate: &str) -> Option<String> {
    let needle = "resource_metadata=";
    let idx = www_authenticate.find(needle)?;
    let after = &www_authenticate[idx + needle.len()..];
    let trimmed = after.trim_start();
    if let Some(rest) = trimmed.strip_prefix('"') {
        let end = rest.find('"')?;
        Some(rest[..end].to_string())
    } else {
        let end = after.find(',').unwrap_or(after.len());
        Some(after[..end].trim().to_string())
    }
}

/// Fetch an RFC 9728 protected-resource metadata document and return the
/// listed authorization server issuers.
async fn fetch_authorization_servers(
    http: &reqwest::Client,
    resource_meta_url: &str,
) -> Vec<String> {
    let Ok(resp) = http.get(resource_meta_url).send().await else {
        return Vec::new();
    };
    if !resp.status().is_success() {
        return Vec::new();
    }
    let Ok(doc) = resp.json::<serde_json::Value>().await else {
        return Vec::new();
    };
    doc.get("authorization_servers")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default()
}

/// Hardcoded OAuth endpoints for providers that publish no discovery document.
/// Keyed by hostname (scheme and path are ignored).
fn known_provider_endpoints(issuer: &str) -> Option<DiscoveredEndpoints> {
    let host = issuer
        .strip_prefix("https://")
        .or_else(|| issuer.strip_prefix("http://"))
        .unwrap_or(issuer)
        .split('/')
        .next()
        .unwrap_or(issuer);
    match host {
        "github.com" | "api.github.com" => Some((
            "https://github.com/login/oauth/authorize".to_string(),
            "https://github.com/login/oauth/access_token".to_string(),
        )),
        _ => None,
    }
}

// ─────────────────────────────────────────────
// Callback server
// ─────────────────────────────────────────────

/// Run a mini HTTP server handling the OAuth redirect callback,
/// exchanging the authorization code for tokens.
async fn run_callback_server(
    listener: TcpListener,
    token_url: &str,
    code_verifier: &str,
    expected_state: &str,
    client_id: &str,
    redirect_uri: &str,
    cancel_rx: oneshot::Receiver<()>,
) -> crate::Result<OAuthTokens> {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt};

    let accept = Box::pin(listener.accept());
    let cancel = Box::pin(cancel_rx);

    let (stream, _) = tokio::select! {
        result = accept => result.map_err(|e| {
            crate::error::SyscityError::Internal(format!("Callback server accept failed: {e}"))
        })?,
        _ = cancel => {
            return Err(crate::error::SyscityError::Internal(
                "OAuth flow cancelled".to_string(),
            ));
        }
    };

    let (reader, mut writer) = tokio::io::split(stream);
    let mut lines = tokio::io::BufReader::new(reader).lines();

    let request_line = lines.next_line().await.ok().flatten().unwrap_or_default();

    let path = request_line.split_whitespace().nth(1).unwrap_or("/");

    // Drain remaining request headers
    while let Ok(Some(line)) = lines.next_line().await {
        if line.is_empty() {
            break;
        }
    }

    // Parse query parameters
    let params: HashMap<String, String> = path
        .split('?')
        .nth(1)
        .unwrap_or("")
        .split('&')
        .filter_map(|pair| {
            let mut parts = pair.splitn(2, '=');
            let key = parts.next()?.to_string();
            let value = parts.next().unwrap_or("").to_string();
            Some((key, value))
        })
        .collect();

    let code = params.get("code").ok_or_else(|| {
        crate::error::SyscityError::Internal("Missing code in OAuth callback".to_string())
    })?;

    let state = params.get("state").ok_or_else(|| {
        crate::error::SyscityError::Internal("Missing state in OAuth callback".to_string())
    })?;

    if state != expected_state {
        let body = "Invalid state parameter. Authorization failed.";
        let _ = writer
            .write_all(
                format!(
                    "HTTP/1.1 400 Bad Request\r\nContent-Length: {}\r\nContent-Type: text/plain\r\n\r\n{body}",
                    body.len()
                )
                .as_bytes(),
            )
            .await;
        return Err(crate::error::SyscityError::Internal(
            "State mismatch in OAuth callback".to_string(),
        ));
    }

    // Exchange code for tokens
    let http_client = reqwest::Client::new();
    let token_body = format!(
        "grant_type=authorization_code&code={code}&redirect_uri={redirect_uri}&client_id={client_id}&code_verifier={code_verifier}"
    );

    let token_response = http_client
        .post(token_url)
        .header("Content-Type", "application/x-www-form-urlencoded")
        .header("Accept", "application/json")
        .body(token_body)
        .send()
        .await
        .map_err(|e| crate::error::SyscityError::Internal(format!("Token exchange failed: {e}")))?;

    if !token_response.status().is_success() {
        let status = token_response.status();
        let error_text = token_response.text().await.unwrap_or_default();
        return Err(crate::error::SyscityError::Internal(format!(
            "Token exchange failed: HTTP {status} - {error_text}"
        )));
    }

    let token_data: serde_json::Value = token_response.json().await.map_err(|e| {
        crate::error::SyscityError::Internal(format!("Failed to parse token response: {e}"))
    })?;

    let access_token = token_data["access_token"]
        .as_str()
        .ok_or_else(|| {
            crate::error::SyscityError::Internal(
                "Missing access_token in token response".to_string(),
            )
        })?
        .to_string();

    let refresh_token = token_data["refresh_token"].as_str().map(String::from);
    let expires_at = token_data["expires_in"]
        .as_i64()
        .map(|secs| chrono::Utc::now().timestamp() + secs);

    let body = "<html><body><h1>Authorization complete!</h1><p>You may close this window and return to Syscity.</p></body></html>";
    let _ = writer
        .write_all(
            format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nContent-Type: text/html; charset=utf-8\r\n\r\n{body}",
                body.len()
            )
            .as_bytes(),
        )
        .await;

    Ok(OAuthTokens {
        access_token,
        token_url: token_url.to_string(),
        refresh_token,
        expires_at,
        client_id: client_id.to_string(),
    })
}

// ─────────────────────────────────────────────
// Tests
// ─────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_token_path_for_sanitizes() {
        assert!(token_path_for("github").is_ok());
        assert!(token_path_for("github-main").is_ok());
        assert!(token_path_for("").is_err());
        assert!(token_path_for("..").is_err());
        assert!(token_path_for("../x").is_err());
        assert!(token_path_for("a/b").is_err());
    }

    #[test]
    fn test_metadata_serialization_roundtrip() {
        let meta = OAuthMetadata {
            token_url: "https://example.com/token".to_string(),
            client_id: "syscity".to_string(),
            expires_at: Some(1_700_000_000),
        };
        let json = serde_json::to_string(&meta).unwrap();
        let back: OAuthMetadata = serde_json::from_str(&json).unwrap();
        assert_eq!(back.token_url, meta.token_url);
        assert_eq!(back.client_id, meta.client_id);
        assert_eq!(back.expires_at, meta.expires_at);
    }

    #[test]
    fn test_metadata_drops_unknown_fields() {
        // Old-format files carry access_token/refresh_token; the metadata
        // parser must ignore them rather than fail.
        let json = r#"{
            "access_token": "at_secret",
            "refresh_token": "rt_secret",
            "token_url": "https://example.com/token",
            "client_id": "syscity",
            "expires_at": 1700000000
        }"#;
        let meta: OAuthMetadata = serde_json::from_str(json).unwrap();
        assert_eq!(meta.client_id, "syscity");
    }

    #[tokio::test]
    async fn test_migrate_legacy_mcp_tokens() {
        use crate::secrets::FileStore;

        let root =
            std::env::temp_dir().join(format!("syscity_oauth_migrate_test_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let tokens_dir = root.join("mcp_tokens");
        tokio::fs::create_dir_all(&tokens_dir).await.unwrap();

        // Legacy sidecar carrying plaintext token fields.
        let legacy = r#"{
            "access_token": "at_secret",
            "refresh_token": "rt_secret",
            "token_url": "https://example.com/token",
            "client_id": "syscity",
            "expires_at": 1700000000
        }"#;
        tokio::fs::write(tokens_dir.join("github.json"), legacy)
            .await
            .unwrap();

        // A metadata-only sidecar that must be left untouched.
        let meta_only = r#"{"token_url":"https://example.com/token","client_id":"syscity"}"#;
        tokio::fs::write(tokens_dir.join("google.json"), meta_only)
            .await
            .unwrap();

        let store = Arc::new(FileStore::with_root("mcp-oauth", root.join("secrets")));
        migrate_legacy_mcp_tokens_with_store(tokens_dir.clone(), store.clone())
            .await
            .unwrap();

        // Refresh token moved into the store.
        let stored = store
            .get(&SecretId::new("mcp-oauth", "github", "refresh_token"))
            .await
            .unwrap();
        assert_eq!(stored.as_deref(), Some("rt_secret"));

        // Sidecar rewritten as metadata-only: plaintext token fields gone.
        let rewritten = tokio::fs::read_to_string(tokens_dir.join("github.json"))
            .await
            .unwrap();
        let meta: serde_json::Value = serde_json::from_str(&rewritten).unwrap();
        assert!(meta.get("access_token").is_none());
        assert!(meta.get("refresh_token").is_none());
        assert_eq!(meta["client_id"], "syscity");

        // Metadata-only sidecar untouched.
        let untouched = tokio::fs::read_to_string(tokens_dir.join("google.json"))
            .await
            .unwrap();
        assert!(untouched.contains("\"token_url\""));

        // Idempotent: a second run neither errors nor drops the stored token.
        migrate_legacy_mcp_tokens_with_store(tokens_dir.clone(), store.clone())
            .await
            .unwrap();
        let stored = store
            .get(&SecretId::new("mcp-oauth", "github", "refresh_token"))
            .await
            .unwrap();
        assert_eq!(stored.as_deref(), Some("rt_secret"));

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn test_tokens_fresh() {
        let now = chrono::Utc::now().timestamp();
        let fresh = OAuthTokens {
            access_token: "at".to_string(),
            token_url: "https://example.com/token".to_string(),
            refresh_token: Some("rt".to_string()),
            expires_at: Some(now + 1000),
            client_id: "syscity".to_string(),
        };
        assert!(tokens_fresh(&fresh));

        let expired = OAuthTokens {
            expires_at: Some(now - 1000),
            ..fresh.clone()
        };
        assert!(!tokens_fresh(&expired));

        // No expiry recorded → treated as fresh.
        let no_expiry = OAuthTokens { expires_at: None, ..fresh };
        assert!(tokens_fresh(&no_expiry));
    }
}
