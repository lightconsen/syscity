//! McpManager — owns all clients (9.1, 9.2, 9.4)

use std::collections::HashMap;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;

use tokio::sync::{broadcast, mpsc, RwLock};
use tracing::{error, info, warn};

use crate::mcp::{
    token_path_for, McpClient, McpEvent, McpHealth, McpHealthStatus, McpNotification,
    McpServerConfig, McpToolDefinition, OAuthCommand, OAuthManager, OAuthManagerActor, OAuthTokens,
};
use crate::secrets::{SecretId, SecretStoreHandle};

// ─────────────────────────────────────────────
// McpConnectionMeta
// ─────────────────────────────────────────────

/// Metadata kept for each connected MCP server.
#[derive(Debug)]
pub struct McpConnectionMeta {
    pub(crate) client: Arc<RwLock<McpClient>>,
    pub(crate) config: McpServerConfig,
    pub(crate) health: Arc<RwLock<McpHealth>>,
    pub(crate) crash_count: AtomicU32,
}

impl McpConnectionMeta {
    pub(crate) fn new(client: Arc<RwLock<McpClient>>, config: McpServerConfig) -> Self {
        Self {
            client,
            config,
            health: Arc::new(RwLock::new(McpHealth::new())),
            crash_count: AtomicU32::new(0),
        }
    }
}

// ─────────────────────────────────────────────
// McpManager
// ─────────────────────────────────────────────

/// Manages all MCP server connections.  Lives in `GatewayState`.
pub struct McpManager {
    clients: Arc<RwLock<HashMap<String, McpConnectionMeta>>>,
    event_tx: Arc<RwLock<Option<mpsc::UnboundedSender<McpEvent>>>>,
    /// OAuth manager handle (token caching, refresh, flow management).
    oauth: Option<OAuthManager>,
    /// Receiver half consumed when the actor is spawned in `with_event_tx`.
    oauth_cmd_rx: Option<mpsc::UnboundedReceiver<OAuthCommand>>,
    /// Secret-store instance handle (persisted MCP env vars + OAuth refresh
    /// tokens), shared with the gateway that owns this manager.
    secrets: Arc<SecretStoreHandle>,
}

impl std::fmt::Debug for McpManager {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("McpManager")
            .field("clients", &self.clients)
            .field("event_tx", &self.event_tx)
            .field("oauth", &self.oauth.as_ref().map(|_| "OAuthManager { .. }"))
            .field("oauth_cmd_rx", &self.oauth_cmd_rx.as_ref().map(|_| "Receiver { .. }"))
            .finish()
    }
}

impl Default for McpManager {
    fn default() -> Self {
        Self::new(Arc::new(SecretStoreHandle::default()))
    }
}

impl McpManager {
    /// Create a manager bound to the given secret-store handle (used for
    /// persisted MCP env vars and OAuth refresh tokens).
    pub fn new(secrets: Arc<SecretStoreHandle>) -> Self {
        let (oauth_handle, oauth_cmd_rx) = OAuthManager::new();
        Self {
            clients: Arc::new(RwLock::new(HashMap::new())),
            event_tx: Arc::new(RwLock::new(None)),
            oauth: Some(oauth_handle),
            oauth_cmd_rx: Some(oauth_cmd_rx),
            secrets,
        }
    }

    /// Set the event sender used to emit MCP lifecycle events.
    /// Also spawns the OAuth manager actor (consumes the receiver half).
    pub async fn with_event_tx(mut self, tx: mpsc::UnboundedSender<McpEvent>) -> Self {
        *self.event_tx.write().await = Some(tx);

        // Spawn the OAuth manager actor if we have the receiver.
        if let Some(cmd_rx) = self.oauth_cmd_rx.take() {
            if let Some(ref oauth) = self.oauth {
                let actor = OAuthManagerActor::new(
                    cmd_rx,
                    oauth.cmd_tx.clone(),
                    self.event_tx.clone(),
                    self.secrets.clone(),
                );
                actor.spawn();
            }
        }

        self
    }

    async fn emit_event(&self, event: McpEvent) {
        if let Some(tx) = self.event_tx.read().await.as_ref() {
            let _ = tx.send(event);
        }
    }

    /// Register a pre-authenticated, already-connected client.
    /// Used by the OAuth flow after token exchange completes.
    pub async fn register_client(
        &self,
        server_id: &str,
        client: Arc<RwLock<McpClient>>,
        config: McpServerConfig,
    ) -> crate::Result<()> {
        let tools = {
            let c = client.read().await;
            c.get_tools().to_vec()
        };
        let prompts = {
            let c = client.read().await;
            c.list_prompts().await.unwrap_or_default()
        };
        let resources = {
            let c = client.read().await;
            c.list_resources().await.unwrap_or_default()
        };

        let meta = McpConnectionMeta::new(client.clone(), config.clone());

        // Wire notification and progress channels.
        let (notification_tx, mut notification_rx) = mpsc::unbounded_channel::<McpNotification>();
        let (progress_tx, _progress_rx) = broadcast::channel::<McpNotification>(128);
        {
            let mut c = client.write().await;
            c.set_notification_tx(notification_tx);
            c.set_progress_tx(progress_tx);
        }

        let server_id_owned = server_id.to_string();
        let clients_for_notifications = self.clients.clone();
        let event_tx_for_notifications = self.event_tx.clone();
        tokio::spawn(async move {
            while let Some(notification) = notification_rx.recv().await {
                match notification {
                    McpNotification::ResourceUpdated { uri } => {
                        if let Some(tx) = event_tx_for_notifications.read().await.as_ref() {
                            let _ = tx.send(McpEvent::ResourceChanged {
                                server_id: server_id_owned.clone(),
                                uri,
                            });
                        }
                    }
                    McpNotification::ToolListChanged => {
                        if let Some(meta) =
                            clients_for_notifications.read().await.get(&server_id_owned)
                        {
                            let c = meta.client.read().await;
                            if c.server_capabilities()
                                .map(|c| c.supports_tools())
                                .unwrap_or(false)
                            {
                                let client_clone = meta.client.clone();
                                let sid = server_id_owned.clone();
                                tokio::spawn(async move {
                                    let mut c = client_clone.write().await;
                                    if let Err(e) = c.list_tools().await {
                                        warn!("Failed to refresh tools for '{}': {}", sid, e);
                                    }
                                });
                            }
                        }
                    }
                    _ => {}
                }
            }
        });

        self.clients
            .write()
            .await
            .insert(server_id.to_string(), meta);

        self.emit_event(McpEvent::Connected {
            server_id: server_id.to_string(),
            tools: tools.len(),
            prompts: prompts.len(),
            resources: resources.len(),
        })
        .await;

        self.start_health_monitor(server_id);

        Ok(())
    }

    /// Connect to a server and return its discovered tools.
    pub async fn connect(
        &self,
        server_id: &str,
        config: McpServerConfig,
    ) -> crate::Result<Vec<McpToolDefinition>> {
        let mut config = config;
        // Reconnect-after-restart: pull persisted tokens from the secret store
        // so the server spawns with them without the user re-entering them.
        // Inline (submitted) env wins over stored via entry().or_insert().
        if let Ok(stored) = self.secrets.route("mcp-env").get_all(server_id).await {
            for (k, v) in stored {
                config.resolved_env.entry(k).or_insert(v);
            }
        }
        let mut client = McpClient::new()
            .with_timeout(config.timeout_secs)
            .with_secrets(self.secrets.clone());
        client.connect(config.clone()).await?;

        let tools = client.get_tools().to_vec();
        let prompts = client.list_prompts().await.unwrap_or_default();
        let resources = client.list_resources().await.unwrap_or_default();

        let client_arc = Arc::new(RwLock::new(client));
        let meta = McpConnectionMeta::new(client_arc.clone(), config.clone());

        // Wire notification and progress channels.
        let (notification_tx, mut notification_rx) = mpsc::unbounded_channel::<McpNotification>();
        let (progress_tx, _progress_rx) = broadcast::channel::<McpNotification>(128);
        {
            let mut c = client_arc.write().await;
            c.set_notification_tx(notification_tx);
            c.set_progress_tx(progress_tx);
        }

        let server_id_owned = server_id.to_string();
        let clients_for_notifications = self.clients.clone();
        let event_tx_for_notifications = self.event_tx.clone();
        tokio::spawn(async move {
            while let Some(notification) = notification_rx.recv().await {
                match notification {
                    McpNotification::ResourceUpdated { uri } => {
                        if let Some(tx) = event_tx_for_notifications.read().await.as_ref() {
                            let _ = tx.send(McpEvent::ResourceChanged {
                                server_id: server_id_owned.clone(),
                                uri,
                            });
                        }
                    }
                    McpNotification::ToolListChanged => {
                        // Refresh tool list in the background.
                        if let Some(meta) =
                            clients_for_notifications.read().await.get(&server_id_owned)
                        {
                            let c = meta.client.read().await;
                            if c.server_capabilities()
                                .map(|c| c.supports_tools())
                                .unwrap_or(false)
                            {
                                // list_tools requires &mut self; spawn a short task with a clone of
                                // the Arc.
                                let client_clone = meta.client.clone();
                                let sid = server_id_owned.clone();
                                tokio::spawn(async move {
                                    let mut c = client_clone.write().await;
                                    if let Err(e) = c.list_tools().await {
                                        warn!("Failed to refresh tools for '{}': {}", sid, e);
                                    }
                                });
                            }
                        }
                    }
                    McpNotification::ResourceListChanged => {
                        // Nothing automatic to do; consumers can re-list on
                        // demand.
                    }
                    _ => {}
                }
            }
        });

        self.clients
            .write()
            .await
            .insert(server_id.to_string(), meta);

        self.emit_event(McpEvent::Connected {
            server_id: server_id.to_string(),
            tools: tools.len(),
            prompts: prompts.len(),
            resources: resources.len(),
        })
        .await;

        // Start health monitor for this connection.
        self.start_health_monitor(server_id);

        Ok(tools)
    }

    /// Disconnect a server.
    pub async fn disconnect(&self, server_id: &str) -> crate::Result<()> {
        let removed = self.clients.write().await.remove(server_id);
        if let Some(meta) = removed {
            meta.client.write().await.disconnect().await?;
            self.emit_event(McpEvent::Disconnected {
                server_id: server_id.to_string(),
                reason: "manual_disconnect".to_string(),
            })
            .await;
        }
        Ok(())
    }

    /// Get the `Arc<RwLock<McpClient>>` for a server.
    pub async fn get_client(&self, server_id: &str) -> Option<Arc<RwLock<McpClient>>> {
        self.clients
            .read()
            .await
            .get(server_id)
            .map(|m| m.client.clone())
    }

    /// Get the health record for a server.
    pub async fn get_health(&self, server_id: &str) -> Option<Arc<RwLock<McpHealth>>> {
        self.clients
            .read()
            .await
            .get(server_id)
            .map(|m| m.health.clone())
    }

    /// List connected server IDs.
    pub async fn list_servers(&self) -> Vec<String> {
        self.clients.read().await.keys().cloned().collect()
    }

    /// Attempt exponential-backoff reconnect for a disconnected server (9.4).
    pub async fn reconnect_with_backoff(
        &self,
        server_id: &str,
        config: McpServerConfig,
    ) -> crate::Result<Vec<McpToolDefinition>> {
        let max_attempts = config.max_reconnect_attempts.max(1) as usize;
        let base_delays: &[u64] = &[5, 10, 20, 40, 80];
        let delays: Vec<u64> = base_delays
            .iter()
            .cycle()
            .take(max_attempts)
            .copied()
            .collect();

        for (attempt, &secs) in delays.iter().enumerate() {
            warn!(
                "Reconnecting to MCP server '{}' in {}s (attempt {}/{}) …",
                server_id,
                secs,
                attempt + 1,
                delays.len()
            );
            tokio::time::sleep(tokio::time::Duration::from_secs(secs)).await;
            match self.connect(server_id, config.clone()).await {
                Ok(tools) => {
                    info!("Reconnected to MCP server '{}'", server_id);
                    if let Some(meta) = self.clients.read().await.get(server_id) {
                        meta.crash_count.store(0, Ordering::SeqCst);
                    }
                    self.emit_event(McpEvent::Recovered {
                        server_id: server_id.to_string(),
                        attempt: (attempt + 1) as u32,
                    })
                    .await;
                    return Ok(tools);
                }
                Err(e) => {
                    warn!("Reconnect attempt failed for '{}': {}", server_id, e);
                }
            }
        }
        Err(crate::error::SyscityError::Internal(format!(
            "Failed to reconnect to MCP server '{}' after {} attempts",
            server_id,
            delays.len()
        )))
    }

    /// Spawn a background health monitor for a single server connection.
    fn start_health_monitor(&self, server_id: &str) -> tokio::task::JoinHandle<()> {
        let server_id = server_id.to_string();
        let clients = self.clients.clone();
        let event_tx = self.event_tx.clone();
        let secrets = self.secrets.clone();

        tokio::spawn(async move {
            loop {
                let interval_secs = {
                    let guard = clients.read().await;
                    guard
                        .get(&server_id)
                        .map(|m| m.config.health_check_interval_secs.max(5))
                        .unwrap_or(0)
                };
                if interval_secs == 0 {
                    break;
                }
                tokio::time::sleep(tokio::time::Duration::from_secs(interval_secs)).await;

                let reconnect_config = {
                    let mut guard = clients.write().await;
                    let Some(meta) = guard.get_mut(&server_id) else {
                        break;
                    };

                    let client = meta.client.read().await;
                    let healthy = client.is_connected() && !client.has_child_exited();
                    drop(client);

                    let mut health = meta.health.write().await;
                    if healthy {
                        health.status = McpHealthStatus::Healthy;
                        health.last_heartbeat = chrono::Utc::now();
                        health.consecutive_failures = 0;
                        None
                    } else {
                        health.consecutive_failures += 1;
                        health.status = if health.consecutive_failures == 1 {
                            McpHealthStatus::Degraded
                        } else {
                            McpHealthStatus::Unhealthy
                        };
                        let failures = health.consecutive_failures;
                        drop(health);
                        let auto_reconnect = meta.config.auto_reconnect;
                        let config = meta.config.clone();
                        drop(guard);

                        warn!(
                            "MCP server '{}' health check failed ({} consecutive)",
                            server_id, failures
                        );

                        if failures >= 2 && auto_reconnect {
                            Some(config)
                        } else {
                            None
                        }
                    }
                };

                if let Some(config) = reconnect_config {
                    if let Some(tx) = event_tx.read().await.as_ref() {
                        let _ = tx.send(McpEvent::Disconnected {
                            server_id: server_id.clone(),
                            reason: "health_check_failed".to_string(),
                        });
                    }

                    warn!("MCP server '{}' marked unhealthy; attempting recovery", server_id);

                    let _ = clients.write().await.remove(&server_id);

                    let manager = McpManager {
                        clients: clients.clone(),
                        event_tx: event_tx.clone(),
                        oauth: None,
                        oauth_cmd_rx: None,
                        secrets: secrets.clone(),
                    };
                    if let Err(e) = manager.reconnect_with_backoff(&server_id, config).await {
                        error!("MCP server '{}' recovery failed: {}", server_id, e);
                    }
                    break;
                }
            }
        })
    }

    /// Extract the origin (scheme + host) from a URL.
    pub(crate) fn origin_from_url(url: &str) -> String {
        if let Some(rest) = url.strip_prefix("https://") {
            let end = rest.find('/').unwrap_or(rest.len());
            format!("https://{}", &rest[..end])
        } else if let Some(rest) = url.strip_prefix("http://") {
            let end = rest.find('/').unwrap_or(rest.len());
            format!("http://{}", &rest[..end])
        } else {
            url.to_string()
        }
    }

    /// Generate a PKCE code verifier (random bytes → base64url no-pad).
    pub(crate) fn generate_code_verifier() -> String {
        use base64::engine::general_purpose::URL_SAFE_NO_PAD;
        use base64::Engine;
        use rand::rngs::OsRng;
        use rand::RngCore;

        let mut bytes = vec![0u8; 64];
        OsRng.fill_bytes(&mut bytes);
        URL_SAFE_NO_PAD.encode(&bytes)
    }

    /// Generate a PKCE code challenge (SHA-256 → base64url no-pad).
    pub(crate) fn generate_code_challenge(verifier: &str) -> String {
        use base64::engine::general_purpose::URL_SAFE_NO_PAD;
        use base64::Engine;
        use sha2::{Digest, Sha256};

        let hash = Sha256::digest(verifier.as_bytes());
        URL_SAFE_NO_PAD.encode(hash)
    }

    /// Generate a random state parameter.
    pub(crate) fn generate_random_state() -> String {
        use base64::engine::general_purpose::URL_SAFE_NO_PAD;
        use base64::Engine;
        use rand::rngs::OsRng;
        use rand::RngCore;

        let mut bytes = vec![0u8; 32];
        OsRng.fill_bytes(&mut bytes);
        URL_SAFE_NO_PAD.encode(&bytes)
    }
}

// ─────────────────────────────────────────────
// OAuth delegation
// ─────────────────────────────────────────────

impl McpManager {
    /// Load stored OAuth tokens for a server (delegates to OAuthManager actor,
    /// which refreshes on demand).
    pub async fn load_stored_token(&self, server_id: &str) -> Option<OAuthTokens> {
        match &self.oauth {
            Some(oauth) => oauth.get_token(server_id.to_string()).await,
            None => {
                // No OAuth actor running. Access tokens are memory-only and
                // cannot be recovered from disk, so there is nothing to hand
                // out (only reachable for temporary reconnect managers).
                warn!("load_stored_token called with no OAuth manager");
                None
            }
        }
    }

    /// Check if an access token can be obtained for a server (fresh cache or a
    /// persisted refresh path).
    pub async fn has_stored_token(&self, server_id: &str) -> bool {
        match &self.oauth {
            Some(oauth) => oauth.has_token(server_id.to_string()).await,
            None => false,
        }
    }

    /// Start the OAuth 2.1 + PKCE authorization flow for a remote MCP server.
    ///
    /// Returns the authorization URL the frontend should open in a browser.
    pub async fn start_oauth_flow(
        &self,
        server_id: &str,
        config: &McpServerConfig,
    ) -> crate::Result<String> {
        match &self.oauth {
            Some(oauth) => {
                oauth
                    .start_flow(server_id.to_string(), config.clone())
                    .await
            }
            None => {
                Err(crate::error::SyscityError::Internal("OAuth manager not available".to_string()))
            }
        }
    }

    /// Cancel a pending OAuth authorization flow.
    pub async fn cancel_oauth(&self, server_id: &str) {
        if let Some(oauth) = &self.oauth {
            oauth.cancel_flow(server_id.to_string()).await;
        }
    }

    /// Clear stored OAuth tokens for a server (memory cache + persisted token
    /// data). Called when a server is removed so a re-added server cannot
    /// reuse stale tokens.
    pub async fn clear_oauth_token(&self, server_id: &str) {
        if let Some(oauth) = &self.oauth {
            oauth.clear_token(server_id.to_string()).await;
        } else if let Ok(path) = token_path_for(server_id) {
            let _ = tokio::fs::remove_file(path).await;
            let _ = self
                .secrets
                .route("mcp-oauth")
                .delete(&SecretId::new("mcp-oauth", server_id, "refresh_token"))
                .await;
        }
    }
}
