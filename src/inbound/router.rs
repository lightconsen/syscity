//! Agent Router for the Inbound Pipeline
//!
//! Replaces the simple `resolve_agent_for_session` with a workspace-aware
//! multi-agent routing system.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use async_trait::async_trait;
use sqlx::Row;
use tokio::sync::RwLock;
use tracing::{debug, info, warn};

use crate::channels::resolver::ConversationResolver;
use crate::channels::IncomingMessage;

/// Result of routing a message to an agent.
#[derive(Debug, Clone)]
pub struct RouteResult {
    /// The agent ID that should handle this message.
    pub agent_id: String,
    /// The workspace the agent belongs to (if any).
    pub workspace_id: Option<String>,
    /// Whether the router actually persisted a session binding for this route.
    ///
    /// `true` means the router wrote (or updated) a session-to-agent binding;
    /// `false` means an existing binding, mention, or other non-binding route
    /// was used. This is useful for callers that want to distinguish "fell back
    /// to default because nothing was bound" from "used an established
    /// binding".
    pub persisted_binding: bool,
    /// Whether this route is the global default fallback.
    ///
    /// `true` only when no mention, binding, channel default, workspace
    /// default, or conversation resolver matched and the router fell back
    /// to the configured global default agent. Callers can use this to
    /// treat the route as "not found" when there is no established binding.
    pub is_fallback: bool,
}

/// What [`AgentRouter::route`] concluded.
///
/// A refusal is a first-class result rather than a borrow of `Suppress`: the
/// pipeline already uses `Suppress` for "group chat without a mention, stay
/// quiet", and a refusal must speak — the person has to be told why their
/// message went unanswered.
#[derive(Debug, Clone)]
pub enum RouteOutcome {
    /// An agent was chosen.
    Routed(RouteResult),
    /// The message named (or resolved to) an agent its sender may not use.
    Denied {
        /// The agent that was refused, for the log line and the reply.
        agent_id: String,
    },
}

impl RouteOutcome {
    /// The routed result, when there is one.
    pub fn routed(self) -> Option<RouteResult> {
        match self {
            Self::Routed(result) => Some(result),
            Self::Denied { .. } => None,
        }
    }
}

/// The id of the built-in agent every gateway spawns at startup.
///
/// It is the router's default until `agents.default` says otherwise, and the
/// one agent name that always exists: it cannot be renamed, deleted or
/// purged, so switching the default back to it is always valid.
pub const DEFAULT_AGENT_ID: &str = "default";

/// Configuration for the agent router.
#[derive(Debug, Clone)]
pub struct AgentRouterConfig {
    /// Agent ID when no other route matches, as of construction.
    ///
    /// This is only the *initial* value: [`AgentRouter::set_default_agent`]
    /// changes the live default without rebuilding the router, because the
    /// pipeline holds the router for the process lifetime and `agents.default`
    /// must take effect on the next message.
    pub default_agent_id: String,
    /// Default workspace ID when no other route matches.
    pub default_workspace_id: Option<String>,
    /// Whether to create a new agent binding on-the-fly when none exists.
    pub auto_create_binding: bool,
}

impl Default for AgentRouterConfig {
    fn default() -> Self {
        Self {
            default_agent_id: DEFAULT_AGENT_ID.to_string(),
            default_workspace_id: None,
            auto_create_binding: true,
        }
    }
}

/// Persistent storage for session-to-agent bindings.
#[async_trait]
pub trait BindingStore: Send + Sync {
    /// Load all stored bindings into memory.
    async fn load_bindings(&self) -> crate::Result<HashMap<String, (String, Option<String>)>>;
    /// Persist a single binding.
    async fn save_binding(
        &self,
        session_id: &str,
        agent_id: &str,
        workspace_id: Option<&str>,
    ) -> crate::Result<()>;
    /// Remove a persisted binding.
    async fn remove_binding(&self, session_id: &str) -> crate::Result<()>;
}

/// In-memory binding store (default, non-persistent).
#[allow(clippy::type_complexity)]
pub struct InMemoryBindingStore {
    data: Arc<RwLock<HashMap<String, (String, Option<String>)>>>,
}

impl InMemoryBindingStore {
    pub fn new() -> Self {
        Self {
            data: Arc::new(RwLock::new(HashMap::new())),
        }
    }
}

impl Default for InMemoryBindingStore {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl BindingStore for InMemoryBindingStore {
    async fn load_bindings(&self) -> crate::Result<HashMap<String, (String, Option<String>)>> {
        let data = self.data.read().await;
        Ok(data.clone())
    }

    async fn save_binding(
        &self,
        session_id: &str,
        agent_id: &str,
        workspace_id: Option<&str>,
    ) -> crate::Result<()> {
        let mut data = self.data.write().await;
        data.insert(session_id.to_string(), (agent_id.to_string(), workspace_id.map(String::from)));
        Ok(())
    }

    async fn remove_binding(&self, session_id: &str) -> crate::Result<()> {
        let mut data = self.data.write().await;
        data.remove(session_id);
        Ok(())
    }
}

/// SQLite-backed binding store for persistence across restarts.
pub struct SqliteBindingStore {
    pool: sqlx::Pool<sqlx::Sqlite>,
}

impl SqliteBindingStore {
    pub async fn new(pool: sqlx::Pool<sqlx::Sqlite>) -> crate::Result<Self> {
        let store = Self { pool };
        store.ensure_schema().await?;
        Ok(store)
    }

    async fn ensure_schema(&self) -> crate::Result<()> {
        sqlx::query(
            r#"
            CREATE TABLE IF NOT EXISTS session_bindings (
                session_id TEXT PRIMARY KEY,
                agent_id TEXT NOT NULL,
                workspace_id TEXT,
                created_at INTEGER NOT NULL DEFAULT (unixepoch())
            )
            "#,
        )
        .execute(&self.pool)
        .await
        .map_err(|e| crate::error::SyscityError::Storage {
            context: "Failed to create session_bindings table".to_string(),
            details: e.to_string(),
        })?;
        Ok(())
    }
}

#[async_trait]
impl BindingStore for SqliteBindingStore {
    async fn load_bindings(&self) -> crate::Result<HashMap<String, (String, Option<String>)>> {
        let rows = sqlx::query("SELECT session_id, agent_id, workspace_id FROM session_bindings")
            .fetch_all(&self.pool)
            .await
            .map_err(|e| crate::error::SyscityError::Storage {
                context: "Failed to load session bindings".to_string(),
                details: e.to_string(),
            })?;

        let mut bindings = HashMap::new();
        for row in rows {
            let session_id: String = match row.try_get("session_id") {
                Ok(value) => value,
                Err(e) => {
                    warn!("Skipping session_bindings row with invalid session_id: {}", e);
                    continue;
                }
            };
            if session_id.is_empty() {
                warn!("Skipping session_bindings row with empty session_id");
                continue;
            }

            let agent_id: String = match row.try_get("agent_id") {
                Ok(value) => value,
                Err(e) => {
                    warn!(
                        "Skipping session_bindings row for {}: invalid agent_id: {}",
                        session_id, e
                    );
                    continue;
                }
            };
            if agent_id.is_empty() {
                warn!("Skipping session_bindings row for {}: empty agent_id", session_id);
                continue;
            }

            let workspace_id: Option<String> = row
                .try_get("workspace_id")
                .ok()
                .filter(|s: &String| !s.is_empty());
            bindings.insert(session_id, (agent_id, workspace_id));
        }
        Ok(bindings)
    }

    async fn save_binding(
        &self,
        session_id: &str,
        agent_id: &str,
        workspace_id: Option<&str>,
    ) -> crate::Result<()> {
        sqlx::query(
            r#"
            INSERT INTO session_bindings (session_id, agent_id, workspace_id, created_at)
            VALUES (?1, ?2, ?3, unixepoch())
            ON CONFLICT(session_id) DO UPDATE SET
                agent_id = excluded.agent_id,
                workspace_id = excluded.workspace_id,
                created_at = excluded.created_at
            "#,
        )
        .bind(session_id)
        .bind(agent_id)
        .bind(workspace_id)
        .execute(&self.pool)
        .await
        .map_err(|e| crate::error::SyscityError::Storage {
            context: "Failed to save session binding".to_string(),
            details: e.to_string(),
        })?;
        Ok(())
    }

    async fn remove_binding(&self, session_id: &str) -> crate::Result<()> {
        sqlx::query("DELETE FROM session_bindings WHERE session_id = ?1")
            .bind(session_id)
            .execute(&self.pool)
            .await
            .map_err(|e| crate::error::SyscityError::Storage {
                context: "Failed to remove session binding".to_string(),
                details: e.to_string(),
            })?;
        Ok(())
    }
}

/// Workspace-aware multi-agent router.
///
/// Routes incoming messages to the appropriate agent based on:
/// - Session-to-agent binding table
/// - Explicit `@agent_name` mentions in message content
/// - Channel-specific defaults
/// - Workspace-specific defaults
///
/// This replaces `Gateway::resolve_agent_for_session`.
pub struct AgentRouter {
    config: AgentRouterConfig,
    /// session_id -> (agent_id, workspace_id)
    session_bindings: RwLock<HashMap<String, (String, Option<String>)>>,
    /// channel_name -> (agent_id, workspace_id)
    channel_defaults: RwLock<HashMap<String, (String, Option<String>)>>,
    /// workspace_id -> default_agent_id
    workspace_defaults: RwLock<HashMap<String, String>>,
    /// agent_id -> principals allowed to use it. An agent absent from the map
    /// is unrestricted.
    agent_principals: RwLock<HashMap<String, Vec<String>>>,
    /// Optional persistent binding store
    binding_store: Option<Arc<dyn BindingStore>>,
    /// Optional conversation resolver for supplementary routing fallback.
    conversation_resolver: Option<ConversationResolver>,
    /// Tracks whether auto-create has been disabled via config, used for
    /// logging/observability only; the actual gate is in `bind_session`.
    auto_create_binding: AtomicBool,
    /// The live global default agent.
    ///
    /// `config.default_agent_id` is the value the router was built with; this
    /// is the one routing actually reads, because `agents.default` switches it
    /// while the gateway is running. It lives here rather than in `config`
    /// because the router is held by the inbound pipeline by `Arc` for the
    /// process lifetime — a rebuild would lose every session binding — and
    /// `AgentRouterConfig` is otherwise immutable by design.
    default_agent_id: RwLock<String>,
}

impl AgentRouter {
    pub fn new(config: AgentRouterConfig) -> Self {
        let auto_create_binding = AtomicBool::new(config.auto_create_binding);
        let default_agent_id = RwLock::new(config.default_agent_id.clone());
        Self {
            config,
            session_bindings: RwLock::new(HashMap::new()),
            channel_defaults: RwLock::new(HashMap::new()),
            workspace_defaults: RwLock::new(HashMap::new()),
            agent_principals: RwLock::new(HashMap::new()),
            binding_store: None,
            conversation_resolver: None,
            auto_create_binding,
            default_agent_id,
        }
    }

    /// The agent messages fall back to when no route matches.
    pub async fn default_agent(&self) -> String {
        self.default_agent_id.read().await.clone()
    }

    /// Switch the global default agent (`agents.default`).
    ///
    /// Sessions that were already bound keep their agent: a switch changes
    /// where *unrouted* conversations go, not where existing ones point. The
    /// caller is responsible for deciding whether the new default is a real
    /// agent.
    pub async fn set_default_agent(&self, agent_id: impl Into<String>) {
        let agent_id = agent_id.into();
        let previous = {
            let mut current = self.default_agent_id.write().await;
            std::mem::replace(&mut *current, agent_id.clone())
        };
        info!("Global default agent switched from '{}' to '{}'", previous, agent_id);
    }

    /// Create a deep copy of this router, including all in-memory bindings and
    /// defaults.
    ///
    /// The optional [`ConversationResolver`] is **not** copied because it
    /// contains trait objects that are not `Clone`; the cloned router will have
    /// no conversation resolver. The persistent binding store reference is
    /// shared (cloned `Arc`).
    pub async fn deep_clone(&self) -> Self {
        let session_bindings = self.session_bindings.read().await.clone();
        let channel_defaults = self.channel_defaults.read().await.clone();
        let workspace_defaults = self.workspace_defaults.read().await.clone();
        let agent_principals = self.agent_principals.read().await.clone();

        Self {
            config: self.config.clone(),
            session_bindings: RwLock::new(session_bindings),
            channel_defaults: RwLock::new(channel_defaults),
            workspace_defaults: RwLock::new(workspace_defaults),
            agent_principals: RwLock::new(agent_principals),
            binding_store: self.binding_store.clone(),
            conversation_resolver: None,
            auto_create_binding: AtomicBool::new(self.auto_create_binding.load(Ordering::Acquire)),
            default_agent_id: RwLock::new(self.default_agent_id.read().await.clone()),
        }
    }

    /// Attach a persistent binding store.
    pub fn with_binding_store(mut self, store: Arc<dyn BindingStore>) -> Self {
        self.binding_store = Some(store);
        self
    }

    /// Attach a conversation resolver for supplementary routing fallback.
    pub fn with_conversation_resolver(mut self, resolver: ConversationResolver) -> Self {
        self.conversation_resolver = Some(resolver);
        self
    }

    /// Load persisted bindings into memory.
    pub async fn load_bindings(&self) -> crate::Result<()> {
        if let Some(store) = &self.binding_store {
            match store.load_bindings().await {
                Ok(bindings) => {
                    let mut session_bindings = self.session_bindings.write().await;
                    *session_bindings = bindings;
                    info!("Loaded {} persisted session bindings", session_bindings.len());
                }
                Err(e) => {
                    warn!("Failed to load persisted bindings: {}", e);
                }
            }
        }
        Ok(())
    }

    /// Derive a stable session key from channel and user_id.
    ///
    /// normalization: `{channel}:{user_id}`
    pub fn derive_session_key(channel: &str, user_id: &str) -> String {
        format!("{}:{}", channel, user_id)
    }

    /// Route an incoming message to an agent.
    ///
    /// Resolution order:
    /// 1. Explicit `@agent_name` mention in message content
    /// 2. Existing session binding (by conversation_id)
    /// 3. Existing session binding (by derived `{channel}:{user_id}` key)
    /// 4. Channel-specific default
    /// 5. Workspace-specific default
    /// 6. Supplementary conversation resolver
    /// 7. Global default agent
    ///
    /// Every step that produces an agent is gated by
    /// [`Self::allows_principal`]: routing chooses *which* agent, authorization
    /// decides whether this sender may have it. A refusal at any step returns
    /// [`RouteOutcome::Denied`].
    pub async fn route(
        &self,
        message: &IncomingMessage,
        workspace_hint: Option<&str>,
    ) -> RouteOutcome {
        let session_id = message.conversation_id.0.clone();
        let content = message.content.trim();
        let caller = message.user_id.0.as_str();

        // 1. Check for explicit @agent_name mention
        if let Some(mentioned) = Self::extract_agent_mention(content) {
            // An `@mention` beats every other route, so it has to be gated
            // here too: without the check a person in a channel bound to their
            // own agent could type `@someone_elses_agent …` and reach it. Any
            // agent with no principal restriction stays mentionable by anyone.
            if !self.allows_principal(&mentioned, caller).await {
                info!(
                    "Refusing mention of agent '{}' by '{}' in session {}",
                    mentioned, caller, session_id
                );
                return RouteOutcome::Denied { agent_id: mentioned };
            }
            info!("Explicit agent mention: {} for session {}", mentioned, session_id);
            return RouteOutcome::Routed(RouteResult {
                agent_id: mentioned,
                workspace_id: workspace_hint.map(String::from),
                persisted_binding: false,
                is_fallback: false,
            });
        }

        // 2. Check existing session binding (by conversation_id)
        if let Some(result) = self.resolve_session_binding(&session_id).await {
            if !self.allows_principal(&result.agent_id, caller).await {
                info!(
                    "Refusing bound agent '{}' for session {}: '{}' is not an allowed principal",
                    result.agent_id, session_id, caller
                );
                return RouteOutcome::Denied { agent_id: result.agent_id };
            }
            return RouteOutcome::Routed(result);
        }

        // 2b. Check existing session binding by derived `{channel}:{user_id}` key.
        //
        // Keyed by the channel *instance*, not the channel type: two people
        // sharing one bot type (`[channels.tg_alice]` / `[channels.tg_bob]`)
        // must not resolve to each other's agent.
        let channel_name = Self::resolve_channel_name(message);
        let user_id = message.user_id.0.as_str();

        if let Some(ch) = channel_name.as_deref() {
            let derived_key = Self::derive_session_key(ch, user_id);
            if derived_key != session_id {
                if let Some(result) = self.resolve_session_binding(&derived_key).await {
                    if !self.allows_principal(&result.agent_id, caller).await {
                        info!(
                            "Refusing bound agent '{}' for session {}: '{}' is not an allowed principal",
                            result.agent_id, session_id, caller
                        );
                        return RouteOutcome::Denied { agent_id: result.agent_id };
                    }
                    debug!(
                        "Session {} resolved via derived key {} to agent {}",
                        session_id, derived_key, result.agent_id
                    );
                    return RouteOutcome::Routed(result);
                }
            }
        }

        // 3. Check channel-specific default
        if let Some(ch) = &channel_name {
            if let Some(result) = self.resolve_channel_default(ch, &session_id).await {
                if !self.allows_principal(&result.agent_id, caller).await {
                    info!(
                        "Refusing channel default agent '{}' for session {}: '{}' is not an allowed principal",
                        result.agent_id, session_id, caller
                    );
                    return RouteOutcome::Denied { agent_id: result.agent_id };
                }
                // Pin this user to the agent that just answered them. Without
                // this the derived-key lookup in step 2b is a lookup nobody
                // ever fills, and a second message from the same person in a
                // new conversation cannot be attributed to their own agent.
                self.bind_user(ch, user_id, &result.agent_id).await;
                return RouteOutcome::Routed(result);
            }
        }

        // 4. Check workspace-specific default
        if let Some(ws) = workspace_hint {
            if let Some(result) = self.resolve_workspace_default(ws, &session_id).await {
                if !self.allows_principal(&result.agent_id, caller).await {
                    info!(
                        "Refusing workspace default agent '{}' for session {}: '{}' is not an allowed principal",
                        result.agent_id, session_id, caller
                    );
                    return RouteOutcome::Denied { agent_id: result.agent_id };
                }
                return RouteOutcome::Routed(result);
            }
        }

        // 4b. Supplementary resolution via ConversationResolver (before global
        // fallback)
        if let Some(ref resolver) = self.conversation_resolver {
            let resolution = resolver.resolve(message).await;
            let is_fallback = matches!(
                resolution.source,
                crate::channels::resolver::ResolutionSource::InboundFallback
            );
            if !is_fallback {
                if !self.allows_principal(&resolution.agent_id, caller).await {
                    info!(
                        "Refusing resolver agent '{}' for session {}: '{}' is not an allowed principal",
                        resolution.agent_id, session_id, caller
                    );
                    return RouteOutcome::Denied { agent_id: resolution.agent_id };
                }
                debug!(
                    session_id,
                    agent_id = %resolution.agent_id,
                    workspace_id = ?resolution.workspace_id,
                    "Supplementary conversation resolver matched"
                );
                let mut result = RouteResult {
                    agent_id: resolution.agent_id,
                    workspace_id: resolution.workspace_id,
                    persisted_binding: true,
                    is_fallback: false,
                };
                result.persisted_binding = self.bind_session(&session_id, &result).await;
                return RouteOutcome::Routed(result);
            }
        }

        // 5. Fall back to global default
        //
        // Gated like every other route. Falling through to the default is the
        // *common* path in a single-agent deployment, so leaving it open would
        // make `allowed_users` on the default agent decorative: any stranger
        // whose message matched no binding (which is everyone, at first) would
        // reach the very agent the restriction is meant to close.
        //
        // Read once into a local: the default is switchable at runtime, and a
        // route that read it twice could name one agent in the refusal and
        // bind another.
        let default_agent = self.default_agent().await;
        if !self.allows_principal(&default_agent, caller).await {
            info!(
                "Refusing default agent '{}' for session {}: '{}' is not an allowed principal",
                default_agent, session_id, caller
            );
            return RouteOutcome::Denied { agent_id: default_agent };
        }
        debug!(
            "No binding found for session {}, using default agent {}",
            session_id, default_agent
        );
        let mut result = RouteResult {
            agent_id: default_agent,
            workspace_id: self.config.default_workspace_id.clone(),
            persisted_binding: true,
            is_fallback: true,
        };
        result.persisted_binding = self.bind_session(&session_id, &result).await;
        RouteOutcome::Routed(result)
    }

    /// Look up a session binding by key (session_id or derived key).
    async fn resolve_session_binding(&self, key: &str) -> Option<RouteResult> {
        let bindings = self.session_bindings.read().await;
        bindings.get(key).map(|(agent_id, workspace_id)| {
            debug!("Session bound to agent {}", agent_id);
            RouteResult {
                agent_id: agent_id.clone(),
                workspace_id: workspace_id.clone(),
                persisted_binding: false,
                is_fallback: false,
            }
        })
    }

    /// The channel key a message routes by.
    ///
    /// This is the configured **instance** name when the bridge stamped one,
    /// falling back to the channel type in provenance. The distinction
    /// matters: `channel_defaults` is populated from the config section names
    /// (`init_channels` iterates `config.channels`), so looking up the type
    /// instead would miss every instance not named after its type and drop the
    /// message through to the global default agent.
    fn resolve_channel_name(message: &IncomingMessage) -> Option<String> {
        message.channel_key().map(str::to_string)
    }

    /// Look up a channel-specific default agent binding and persist it.
    async fn resolve_channel_default(
        &self,
        channel: &str,
        session_id: &str,
    ) -> Option<RouteResult> {
        let defaults = self.channel_defaults.read().await;
        let (agent_id, workspace_id) = defaults.get(channel)?;
        debug!("Channel {} default agent: {}", channel, agent_id);
        let mut result = RouteResult {
            agent_id: agent_id.clone(),
            workspace_id: workspace_id.clone(),
            persisted_binding: true,
            is_fallback: false,
        };
        drop(defaults);
        result.persisted_binding = self.bind_session(session_id, &result).await;
        Some(result)
    }

    /// Look up a workspace-specific default agent binding and persist it.
    async fn resolve_workspace_default(
        &self,
        workspace: &str,
        session_id: &str,
    ) -> Option<RouteResult> {
        let defaults = self.workspace_defaults.read().await;
        let agent_id = defaults.get(workspace)?.clone();
        debug!("Workspace {} default agent: {}", workspace, agent_id);
        drop(defaults);
        let mut result = RouteResult {
            agent_id,
            workspace_id: Some(workspace.to_string()),
            persisted_binding: true,
            is_fallback: false,
        };
        result.persisted_binding = self.bind_session(session_id, &result).await;
        Some(result)
    }

    /// Bind a session to a specific agent.
    ///
    /// Returns `true` if a binding was actually created or updated. If
    /// [`AgentRouterConfig::auto_create_binding`] is disabled, this returns
    /// `false` and no binding is stored. Empty `workspace_id` strings are
    /// normalized to `None`.
    pub async fn bind_session(&self, session_id: &str, route: &RouteResult) -> bool {
        if !self.auto_create_binding.load(Ordering::Acquire) {
            return false;
        }

        if session_id.is_empty() {
            warn!("Refusing to bind empty session_id");
            return false;
        }
        if route.agent_id.is_empty() {
            warn!("Refusing to bind empty agent_id for session {}", session_id);
            return false;
        }

        let workspace_id = route.workspace_id.clone().filter(|s| !s.is_empty());

        let mut bindings = self.session_bindings.write().await;
        bindings.insert(session_id.to_string(), (route.agent_id.clone(), workspace_id.clone()));
        drop(bindings);

        if let Some(store) = &self.binding_store {
            if let Err(e) = store
                .save_binding(session_id, &route.agent_id, workspace_id.as_deref())
                .await
            {
                warn!("Failed to persist binding for session {}: {}", session_id, e);
            }
        }

        info!(
            "Bound session {} to agent {} (workspace: {:?})",
            session_id, route.agent_id, workspace_id
        );
        true
    }

    /// Unbind a session (e.g., on `/new` command).
    pub async fn unbind_session(&self, session_id: &str) {
        let mut bindings = self.session_bindings.write().await;
        let removed = bindings.remove(session_id).is_some();
        drop(bindings);

        if removed {
            if let Some(store) = &self.binding_store {
                if let Err(e) = store.remove_binding(session_id).await {
                    warn!("Failed to remove persisted binding for session {}: {}", session_id, e);
                }
            }
            info!("Unbound session {}", session_id);
        }
    }

    /// Set whether the router should create bindings on-the-fly.
    pub fn set_auto_create_binding(&self, enabled: bool) {
        self.auto_create_binding.store(enabled, Ordering::Release);
    }

    /// Set the default agent for a channel.
    pub async fn set_channel_default(
        &self,
        channel: &str,
        agent_id: String,
        workspace_id: Option<String>,
    ) {
        let mut defaults = self.channel_defaults.write().await;
        defaults.insert(channel.to_string(), (agent_id, workspace_id));
    }

    /// Set the default agent for a workspace.
    pub async fn set_workspace_default(&self, workspace_id: &str, agent_id: String) {
        let mut defaults = self.workspace_defaults.write().await;
        defaults.insert(workspace_id.to_string(), agent_id);
    }

    /// Restrict an agent to a set of principals.
    ///
    /// This is **use** authorization, not authentication: the caller's id
    /// comes from whatever the channel established (a Telegram user id, an
    /// iMessage handle, or the gateway's single identity under a shared
    /// token). An empty list removes the restriction — the agent is then
    /// usable by anyone who can reach the gateway, exactly as before this
    /// existed. `"*"` is the explicit spelling of the same thing.
    pub async fn set_agent_principals(&self, agent_id: &str, principals: Vec<String>) {
        let mut map = self.agent_principals.write().await;
        if principals.is_empty() || principals.iter().any(|p| p == "*") {
            map.remove(agent_id);
        } else {
            map.insert(agent_id.to_string(), principals);
        }
    }

    /// Whether `principal` may use `agent_id`.
    ///
    /// An agent with no configured list is unrestricted, which is what keeps
    /// every existing deployment working unchanged.
    ///
    /// Public so the WebSocket surfaces can ask before binding a session:
    /// `route` refuses a disallowed caller, but a binding written ahead of that
    /// would keep a denied caller pinned to someone else's agent.
    pub async fn allows_principal(&self, agent_id: &str, principal: &str) -> bool {
        let map = self.agent_principals.read().await;
        match map.get(agent_id) {
            None => true,
            Some(allowed) => allowed.iter().any(|p| p == principal),
        }
    }

    /// Whether `agent_id` has no `allowed_users` at all.
    ///
    /// Distinct from [`allows_principal`](Self::allows_principal), which is
    /// also true for a restricted agent that happens to name this caller. A
    /// *shared* agent — one the deployment left open to everyone — is what a
    /// one-agent credential may reach beside its own agent; a personal agent
    /// restricted to somebody else is not, and neither is one restricted to
    /// this caller alone (that is still a private agent, not a shared one).
    pub async fn is_unrestricted(&self, agent_id: &str) -> bool {
        !self.agent_principals.read().await.contains_key(agent_id)
    }

    /// The agents that carry an `allowed_users` restriction.
    ///
    /// The complement of [`is_unrestricted`](Self::is_unrestricted), as one
    /// snapshot: a caller that must decide per row (a session listing walks
    /// many ids) should read this once rather than await a map lock per row.
    pub async fn restricted_agents(&self) -> std::collections::HashSet<String> {
        self.agent_principals.read().await.keys().cloned().collect()
    }

    /// Bind a user on a channel to an agent, for every session they open.
    ///
    /// This is what makes "one agent per human employee" hold in a deployment
    /// where several people share one bot: the router's derived
    /// `{channel}:{user_id}` lookup only matches if something writes that key.
    /// Call this once per `(channel, user_id)` — [`Self::route`] does it on the
    /// first message a user sends to a channel that has no explicit default.
    ///
    /// Idempotent and safe to call for a user who is already bound: the row is
    /// rewritten with the same value. A no-op when the instance or user is
    /// empty, so a half-configured channel cannot create a `":user"` binding
    /// that silently swallows every channel.
    pub async fn bind_user(&self, channel: &str, user_id: &str, agent_id: &str) {
        if channel.is_empty() || user_id.is_empty() || agent_id.is_empty() {
            return;
        }
        let key = Self::derive_session_key(channel, user_id);
        let route = RouteResult {
            agent_id: agent_id.to_string(),
            workspace_id: None,
            persisted_binding: true,
            is_fallback: false,
        };
        self.bind_session(&key, &route).await;
    }

    /// Whether a derived `{channel}:{user_id}` binding already exists.
    pub async fn has_user_binding(&self, channel: &str, user_id: &str) -> bool {
        let key = Self::derive_session_key(channel, user_id);
        self.session_bindings.read().await.contains_key(&key)
    }

    /// Reverse a derived key: `"{channel}:{user_id}"` → `(channel, user_id)`.
    ///
    /// Splits on the **first** colon, so a user id that itself contains one
    /// (a fully-qualified Matrix/Teams id, say) round-trips intact. Only keys
    /// [`derive_session_key`](Self::derive_session_key) produced are reversed;
    /// a conversation-shaped session id has no channel prefix and returns
    /// `None`.
    pub fn split_session_key(key: &str) -> Option<(&str, &str)> {
        let (channel, user_id) = key.split_once(':')?;
        if channel.is_empty() || user_id.is_empty() {
            return None;
        }
        Some((channel, user_id))
    }

    /// List all active session bindings.
    pub async fn list_bindings(&self) -> HashMap<String, (String, Option<String>)> {
        let bindings = self.session_bindings.read().await;
        bindings.clone()
    }

    /// Resolve an agent by session ID only (used when the full IncomingMessage
    /// is not yet available, e.g. from the message queue processor).
    ///
    /// Checks session bindings and falls back to the global default.
    pub async fn resolve_by_session(&self, session_id: &str) -> RouteResult {
        // 1. Check existing session binding
        {
            let bindings = self.session_bindings.read().await;
            if let Some((agent_id, workspace_id)) = bindings.get(session_id) {
                debug!("Session {} bound to agent {}", session_id, agent_id);
                return RouteResult {
                    agent_id: agent_id.clone(),
                    workspace_id: workspace_id.clone(),
                    persisted_binding: false,
                    is_fallback: false,
                };
            }
        }

        // 2. Fall back to global default
        let default_agent = self.default_agent().await;
        debug!(
            "No binding found for session {}, using default agent {}",
            session_id, default_agent
        );
        let mut result = RouteResult {
            agent_id: default_agent,
            workspace_id: self.config.default_workspace_id.clone(),
            persisted_binding: true,
            is_fallback: true,
        };
        result.persisted_binding = self.bind_session(session_id, &result).await;
        result
    }

    /// Extract an explicit `@agent_name` mention from message content.
    ///
    /// Looks for the first `@agent_name` token anywhere in the message.
    /// The agent name may be followed by punctuation, which is stripped.
    fn extract_agent_mention(content: &str) -> Option<String> {
        for word in content.split_whitespace() {
            if let Some(name) = word.strip_prefix('@') {
                let name =
                    name.trim_end_matches(|c: char| !c.is_alphanumeric() && c != '_' && c != '-');
                if !name.is_empty()
                    && name
                        .chars()
                        .all(|c| c.is_alphanumeric() || c == '_' || c == '-')
                {
                    return Some(name.to_string());
                }
            }
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::channels::InputProvenance;

    #[tokio::test]
    async fn test_default_route() {
        let router = AgentRouter::new(AgentRouterConfig::default());
        let msg = IncomingMessage::new("u1", "s1", "hello");
        let route = router.route(&msg, None).await.routed().expect("routed");
        assert_eq!(route.agent_id, "default");
        assert!(route.persisted_binding);
        assert!(route.is_fallback);
    }

    #[tokio::test]
    async fn test_explicit_mention() {
        let router = AgentRouter::new(AgentRouterConfig::default());
        let msg = IncomingMessage::new("u1", "s1", "@coder write some rust");
        let route = router.route(&msg, None).await.routed().expect("routed");
        assert_eq!(route.agent_id, "coder");
        assert!(!route.persisted_binding);
    }

    #[tokio::test]
    async fn test_explicit_mention_not_at_start() {
        let router = AgentRouter::new(AgentRouterConfig::default());
        let msg = IncomingMessage::new("u1", "s1", "hi @coder can you help");
        let route = router.route(&msg, None).await.routed().expect("routed");
        assert_eq!(route.agent_id, "coder");
        assert!(!route.persisted_binding);
    }

    #[tokio::test]
    async fn test_explicit_mention_with_punctuation() {
        let router = AgentRouter::new(AgentRouterConfig::default());
        let msg = IncomingMessage::new("u1", "s1", "@coder, please review");
        let route = router.route(&msg, None).await.routed().expect("routed");
        assert_eq!(route.agent_id, "coder");
        assert!(!route.persisted_binding);
    }

    #[tokio::test]
    async fn test_session_binding_persists() {
        let router = AgentRouter::new(AgentRouterConfig::default());
        let msg = IncomingMessage::new("u1", "s1", "hello");

        // First message creates binding
        let route1 = router.route(&msg, None).await.routed().expect("routed");
        assert!(route1.persisted_binding);

        // Second message uses existing binding
        let msg2 = IncomingMessage::new("u1", "s1", "again");
        let route2 = router.route(&msg2, None).await.routed().expect("routed");
        assert!(!route2.persisted_binding);
        assert!(!route2.is_fallback);
        assert_eq!(route2.agent_id, route1.agent_id);
    }

    #[tokio::test]
    async fn test_workspace_default() {
        let router = AgentRouter::new(AgentRouterConfig::default());
        router
            .set_workspace_default("dev", "coder".to_string())
            .await;

        let msg = IncomingMessage::new("u1", "s1", "hello");
        let route = router
            .route(&msg, Some("dev"))
            .await
            .routed()
            .expect("routed");
        assert_eq!(route.agent_id, "coder");
        assert_eq!(route.workspace_id, Some("dev".to_string()));
    }

    #[tokio::test]
    async fn test_unbind_session() {
        let router = AgentRouter::new(AgentRouterConfig::default());
        let msg = IncomingMessage::new("u1", "s1", "hello");

        let _route1 = router.route(&msg, None).await.routed().expect("routed");
        router.unbind_session("s1").await;

        let msg2 = IncomingMessage::new("u1", "s1", "again");
        let route2 = router.route(&msg2, None).await.routed().expect("routed");
        // After unbind, a new binding is created (may be different agent)
        assert!(route2.persisted_binding);
    }

    #[tokio::test]
    async fn test_auto_create_binding_disabled_returns_false() {
        let router = AgentRouter::new(AgentRouterConfig {
            auto_create_binding: false,
            ..Default::default()
        });
        let msg = IncomingMessage::new("u1", "s1", "hello");

        let route = router.route(&msg, None).await.routed().expect("routed");
        assert_eq!(route.agent_id, "default");
        assert!(!route.persisted_binding);
        assert!(route.is_fallback);

        let bindings = router.list_bindings().await;
        assert!(bindings.is_empty());
    }

    #[tokio::test]
    async fn test_bind_session_rejects_empty_ids() {
        let router = AgentRouter::new(AgentRouterConfig::default());
        let route_with_empty_agent = RouteResult {
            agent_id: "".to_string(),
            workspace_id: None,
            persisted_binding: false,
            is_fallback: false,
        };
        assert!(
            !router.bind_session("s1", &route_with_empty_agent).await,
            "empty agent_id should not bind"
        );

        let route_ok = RouteResult {
            agent_id: "default".to_string(),
            workspace_id: None,
            persisted_binding: false,
            is_fallback: false,
        };
        assert!(!router.bind_session("", &route_ok).await, "empty session_id should not bind");
        assert!(router.list_bindings().await.is_empty());
    }

    #[tokio::test]
    async fn test_bind_session_valid() {
        let router = AgentRouter::new(AgentRouterConfig::default());
        let route = RouteResult {
            agent_id: "coder".to_string(),
            workspace_id: Some("dev".to_string()),
            persisted_binding: false,
            is_fallback: false,
        };
        assert!(router.bind_session("s1", &route).await);
        let bindings = router.list_bindings().await;
        assert_eq!(bindings.get("s1"), Some(&("coder".to_string(), Some("dev".to_string()))));
    }

    #[tokio::test]
    async fn test_bind_session_normalizes_empty_workspace() {
        let router = AgentRouter::new(AgentRouterConfig::default());
        let route = RouteResult {
            agent_id: "coder".to_string(),
            workspace_id: Some("".to_string()),
            persisted_binding: false,
            is_fallback: false,
        };
        assert!(router.bind_session("s1", &route).await);
        let bindings = router.list_bindings().await;
        assert_eq!(bindings.get("s1"), Some(&("coder".to_string(), None)));
    }

    /// Two instances of the *same* channel type must route to the agents their
    /// own config section names — the reason messages carry the instance.
    #[tokio::test]
    async fn test_channel_instance_routes_to_its_own_default() {
        let router = AgentRouter::new(AgentRouterConfig::default());
        router
            .set_channel_default("tg_alice", "alice".to_string(), None)
            .await;
        router
            .set_channel_default("tg_bob", "bob".to_string(), None)
            .await;

        let from_alice = IncomingMessage::new("u-alice", "s-alice", "hi")
            .with_provenance(InputProvenance::ExternalUser {
                channel: "telegram".to_string(),
                is_direct: true,
            })
            .with_channel_instance("tg_alice");
        let from_bob = IncomingMessage::new("u-bob", "s-bob", "hi")
            .with_provenance(InputProvenance::ExternalUser {
                channel: "telegram".to_string(),
                is_direct: true,
            })
            .with_channel_instance("tg_bob");

        assert_eq!(
            router
                .route(&from_alice, None)
                .await
                .routed()
                .expect("routed")
                .agent_id,
            "alice"
        );
        assert_eq!(
            router
                .route(&from_bob, None)
                .await
                .routed()
                .expect("routed")
                .agent_id,
            "bob"
        );
    }

    /// Without an instance, the channel *type* is still the lookup key, so a
    /// section named after its type keeps working.
    #[tokio::test]
    async fn test_channel_type_is_the_fallback_key() {
        let router = AgentRouter::new(AgentRouterConfig::default());
        router
            .set_channel_default("telegram", "tg-agent".to_string(), None)
            .await;

        let msg =
            IncomingMessage::new("u1", "s1", "hi").with_provenance(InputProvenance::ExternalUser {
                channel: "telegram".to_string(),
                is_direct: true,
            });
        assert_eq!(
            router
                .route(&msg, None)
                .await
                .routed()
                .expect("routed")
                .agent_id,
            "tg-agent"
        );
    }

    /// The channel default pins the sender, so their *next* conversation on the
    /// same channel resolves by the derived key instead of falling through.
    #[tokio::test]
    async fn test_channel_default_pins_the_user_for_later_sessions() {
        let router = AgentRouter::new(AgentRouterConfig::default());
        router
            .set_channel_default("tg_alice", "alice".to_string(), None)
            .await;

        let first = IncomingMessage::new("u-alice", "s-1", "hi")
            .with_provenance(InputProvenance::ExternalUser {
                channel: "telegram".to_string(),
                is_direct: true,
            })
            .with_channel_instance("tg_alice");
        assert_eq!(
            router
                .route(&first, None)
                .await
                .routed()
                .expect("routed")
                .agent_id,
            "alice"
        );
        assert!(router.has_user_binding("tg_alice", "u-alice").await);

        let second = IncomingMessage::new("u-alice", "s-2", "again")
            .with_provenance(InputProvenance::ExternalUser {
                channel: "telegram".to_string(),
                is_direct: true,
            })
            .with_channel_instance("tg_alice");
        // Derived-key hit: not a fallback, and still alice's agent.
        let route = router.route(&second, None).await.routed().expect("routed");
        assert_eq!(route.agent_id, "alice");
        assert!(!route.is_fallback);
    }

    /// `bind_user` is what makes the derived-key lookup reachable at all.
    #[tokio::test]
    async fn test_bind_user_reaches_derived_key() {
        let router = AgentRouter::new(AgentRouterConfig::default());
        router.bind_user("tg_bob", "u-bob", "bob").await;

        let msg = IncomingMessage::new("u-bob", "s-fresh", "hi")
            .with_provenance(InputProvenance::ExternalUser {
                channel: "telegram".to_string(),
                is_direct: true,
            })
            .with_channel_instance("tg_bob");
        let route = router.route(&msg, None).await.routed().expect("routed");
        assert_eq!(route.agent_id, "bob");
        assert!(!route.is_fallback);
    }

    /// A half-configured channel must not create a `":user"` binding that would
    /// swallow every other channel's traffic.
    #[tokio::test]
    async fn test_bind_user_ignores_empty_parts() {
        let router = AgentRouter::new(AgentRouterConfig::default());
        router.bind_user("", "u1", "alice").await;
        router.bind_user("tg", "", "alice").await;
        router.bind_user("tg", "u1", "").await;
        assert!(router.list_bindings().await.is_empty());
    }

    #[test]
    fn test_split_session_key() {
        assert_eq!(AgentRouter::split_session_key("tg_alice:u1"), Some(("tg_alice", "u1")));
        // A user id containing a colon survives: split on the first one only.
        assert_eq!(
            AgentRouter::split_session_key("matrix:@alice:example.org"),
            Some(("matrix", "@alice:example.org"))
        );
        assert_eq!(AgentRouter::split_session_key("conversation-shaped"), None);
        assert_eq!(AgentRouter::split_session_key(":u1"), None);
        assert_eq!(AgentRouter::split_session_key("tg:"), None);
    }

    // ── Who may use an agent ─────────────────────────────────────────────

    /// The message that reaches a channel's default agent, from `user` on the
    /// channel instance `channel`.
    fn on_channel(user: &str, session: &str, channel: &str) -> IncomingMessage {
        IncomingMessage::new(user, session, "hi")
            .with_provenance(InputProvenance::ExternalUser {
                channel: "telegram".to_string(),
                is_direct: true,
            })
            .with_channel_instance(channel)
    }

    /// An unconfigured agent is usable by anyone — the behaviour every
    /// existing deployment relies on.
    #[tokio::test]
    async fn an_agent_with_no_principal_list_is_open() {
        let router = AgentRouter::new(AgentRouterConfig::default());
        router
            .set_channel_default("tg_alice", "alice".to_string(), None)
            .await;

        let route = router
            .route(&on_channel("whoever", "s1", "tg_alice"), None)
            .await
            .routed()
            .expect("an unrestricted agent routes");
        assert_eq!(route.agent_id, "alice");
    }

    /// The channel default is the first gate: a stranger on someone's channel
    /// is refused rather than handed that person's agent.
    #[tokio::test]
    async fn a_stranger_is_refused_the_channel_default() {
        let router = AgentRouter::new(AgentRouterConfig::default());
        router
            .set_channel_default("tg_alice", "alice".to_string(), None)
            .await;
        router
            .set_agent_principals("alice", vec!["u-alice".to_string()])
            .await;

        assert!(matches!(
            router.route(&on_channel("u-stranger", "s2", "tg_alice"), None).await,
            RouteOutcome::Denied { ref agent_id } if agent_id == "alice"
        ));

        let route = router
            .route(&on_channel("u-alice", "s1", "tg_alice"), None)
            .await
            .routed()
            .expect("the allowed principal routes");
        assert_eq!(route.agent_id, "alice");
    }

    /// The global fallback is a route like any other. In a single-agent
    /// deployment it is the *only* route, so it must be gated: otherwise a
    /// restriction on the default agent would be the one restriction no
    /// stranger ever meets.
    #[tokio::test]
    async fn the_global_default_fallback_is_gated() {
        let router = AgentRouter::new(AgentRouterConfig {
            default_agent_id: "solo".into(),
            ..Default::default()
        });
        router
            .set_agent_principals("solo", vec!["u-owner".to_string()])
            .await;

        // No binding of any kind exists, so this falls all the way through.
        assert!(matches!(
            router.route(&on_channel("u-stranger", "s2", "tg_chan"), None).await,
            RouteOutcome::Denied { ref agent_id } if agent_id == "solo"
        ));

        let route = router
            .route(&on_channel("u-owner", "s1", "tg_chan"), None)
            .await
            .routed()
            .expect("the owner routes");
        assert_eq!(route.agent_id, "solo");
    }

    /// The `@mention` path beats every other route, so it needs the same gate:
    /// without it a person in a channel bound to their own agent could type
    /// `@someone_elses_agent …` and reach it.
    #[tokio::test]
    async fn an_agent_mention_is_gated_too() {
        let router = AgentRouter::new(AgentRouterConfig::default());
        router
            .set_agent_principals("bob", vec!["u-bob".to_string()])
            .await;

        let intrusion = IncomingMessage::new("u-alice", "s1", "@bob please help")
            .with_provenance(InputProvenance::ExternalUser {
                channel: "telegram".to_string(),
                is_direct: true,
            })
            .with_channel_instance("tg_alice");
        assert!(matches!(
            router.route(&intrusion, None).await,
            RouteOutcome::Denied { ref agent_id } if agent_id == "bob"
        ));

        let allowed = IncomingMessage::new("u-bob", "s2", "@bob please help")
            .with_provenance(InputProvenance::ExternalUser {
                channel: "telegram".to_string(),
                is_direct: true,
            })
            .with_channel_instance("tg_bob");
        assert_eq!(
            router
                .route(&allowed, None)
                .await
                .routed()
                .expect("bob may reach bob")
                .agent_id,
            "bob"
        );
    }

    /// A binding from an earlier, unrestricted configuration must not become a
    /// permanent way in: the derived-key lookup is gated like the rest.
    #[tokio::test]
    async fn a_bound_agent_is_rechecked_against_the_caller() {
        let router = AgentRouter::new(AgentRouterConfig::default());
        router.bind_user("tg_alice", "u-alice", "alice").await;
        router
            .set_agent_principals("alice", vec!["u-alice".to_string()])
            .await;

        // Someone else on the same channel instance inherits the derived key
        // only if the key is theirs; it is not, so they fall through to the
        // global default rather than to alice's agent.
        let other = router
            .route(&on_channel("u-bob", "s9", "tg_alice"), None)
            .await
            .routed()
            .expect("falls back to the global default");
        assert_eq!(other.agent_id, AgentRouterConfig::default().default_agent_id);
        assert!(other.is_fallback);

        // The owner still resolves through the binding.
        let owner = router
            .route(&on_channel("u-alice", "s10", "tg_alice"), None)
            .await
            .routed()
            .expect("the owner routes");
        assert_eq!(owner.agent_id, "alice");
    }

    /// An empty list (or `"*"`) lifts the restriction rather than locking the
    /// agent away from everyone.
    #[tokio::test]
    async fn an_empty_or_wildcard_list_removes_the_restriction() {
        let router = AgentRouter::new(AgentRouterConfig::default());
        router
            .set_channel_default("tg_alice", "alice".to_string(), None)
            .await;

        router.set_agent_principals("alice", vec![]).await;
        assert!(router.allows_principal("alice", "anyone").await);

        router
            .set_agent_principals("alice", vec!["u-alice".to_string()])
            .await;
        assert!(!router.allows_principal("alice", "anyone").await);

        router
            .set_agent_principals("alice", vec!["*".to_string()])
            .await;
        assert!(router.allows_principal("alice", "anyone").await);
    }

    /// A clone carries the restrictions, so a router copied for a test or a
    /// secondary pipeline does not silently lose its gates.
    #[tokio::test]
    async fn a_clone_keeps_the_principal_restrictions() {
        let router = AgentRouter::new(AgentRouterConfig::default());
        router
            .set_agent_principals("alice", vec!["u-alice".to_string()])
            .await;
        let clone = router.deep_clone().await;
        assert!(clone.allows_principal("alice", "u-alice").await);
        assert!(!clone.allows_principal("alice", "u-bob").await);
    }

    /// `agents.default` must change routing, not just configuration: after the
    /// switch, a message that matches nothing resolves to the new agent.
    #[tokio::test]
    async fn a_switched_default_takes_over_the_global_fallback() {
        let router = AgentRouter::new(AgentRouterConfig::default());
        assert_eq!(router.default_agent().await, DEFAULT_AGENT_ID);

        router.set_default_agent("secretary").await;

        let route = router
            .route(&IncomingMessage::new("u1", "s1", "hello"), None)
            .await
            .routed()
            .expect("routed");
        assert_eq!(route.agent_id, "secretary");
        assert!(route.is_fallback);
        assert!(route.persisted_binding, "the fallback binds the session");
    }

    /// The switch reaches the by-session path too — `chat.send` reports the
    /// routing decision it just made from `resolve_by_session`.
    #[tokio::test]
    async fn a_switched_default_is_used_by_resolve_by_session() {
        let router = AgentRouter::new(AgentRouterConfig::default());
        router.set_default_agent("secretary").await;
        let route = router.resolve_by_session("s-new").await;
        assert_eq!(route.agent_id, "secretary");
        assert!(route.is_fallback);
    }

    /// A switch does not relocate conversations that were already routed:
    /// rebinding them would move a person's history out from under them.
    #[tokio::test]
    async fn a_switch_leaves_existing_bindings_alone() {
        let router = AgentRouter::new(AgentRouterConfig::default());
        let first = router
            .route(&IncomingMessage::new("u1", "s1", "hello"), None)
            .await
            .routed()
            .expect("routed");
        assert_eq!(first.agent_id, DEFAULT_AGENT_ID);

        router.set_default_agent("secretary").await;

        let again = router
            .route(&IncomingMessage::new("u1", "s1", "again"), None)
            .await
            .routed()
            .expect("routed");
        assert_eq!(again.agent_id, DEFAULT_AGENT_ID);
        assert!(!again.is_fallback, "served by the existing binding");
    }

    /// The switched default is still gated: a restriction on the new default
    /// agent applies to the fallback like it does to every other route.
    #[tokio::test]
    async fn a_switched_default_is_still_gated() {
        let router = AgentRouter::new(AgentRouterConfig::default());
        router.set_default_agent("secretary").await;
        router
            .set_agent_principals("secretary", vec!["u-owner".to_string()])
            .await;

        assert!(matches!(
            router
                .route(&IncomingMessage::new("u-stranger", "s2", "hi"), None)
                .await,
            RouteOutcome::Denied { ref agent_id } if agent_id == "secretary"
        ));
        let route = router
            .route(&IncomingMessage::new("u-owner", "s3", "hi"), None)
            .await
            .routed()
            .expect("the owner routes");
        assert_eq!(route.agent_id, "secretary");
    }

    /// A clone inherits the live default, so a router copied for a secondary
    /// pipeline does not silently revert to the built-in agent.
    #[tokio::test]
    async fn a_clone_keeps_the_switched_default() {
        let router = AgentRouter::new(AgentRouterConfig::default());
        router.set_default_agent("secretary").await;
        let clone = router.deep_clone().await;
        assert_eq!(clone.default_agent().await, "secretary");
    }
}
