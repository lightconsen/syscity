//! Security module for Syscity
//!
//! Provides authentication, authorization, rate limiting, and sandboxing.
// INVARIANTS-NONE: pairing expiry and audit integrity are enforced at query/write time inside the respective stores.

use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::Arc;

use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use rand::{rngs::OsRng, RngCore};
use serde::{Deserialize, Serialize};
use tokio::sync::RwLock;
use tracing::{debug, info, warn};

use crate::security::auth_store::{AuthStore, StoredSession};
use crate::security::runtime_audit::AuditEventType;

/// Unique identifier for a user
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct UserId(pub String);

impl UserId {
    /// Create a new user ID
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }
}

impl std::fmt::Display for UserId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// User information
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct User {
    /// User ID
    pub id: UserId,
    /// Display name
    pub name: String,
    /// When the user was created
    pub created_at: chrono::DateTime<chrono::Utc>,
    /// Whether the user is an admin
    pub is_admin: bool,
    /// Granted scopes (e.g. ["chat", "read", "admin"])
    #[serde(default)]
    pub scopes: Vec<String>,
    /// Additional metadata
    pub metadata: HashMap<String, String>,
}

impl User {
    /// Create a new user
    pub fn new(id: impl Into<String>, name: impl Into<String>) -> Self {
        Self {
            id: UserId::new(id),
            name: name.into(),
            created_at: chrono::Utc::now(),
            is_admin: false,
            scopes: Vec::new(),
            metadata: HashMap::new(),
        }
    }

    /// Set admin status
    pub fn admin(mut self, is_admin: bool) -> Self {
        self.is_admin = is_admin;
        self
    }
}

/// Authentication manager
#[derive(Debug, Default)]
pub struct AuthManager {
    /// Registered users
    users: Arc<RwLock<HashMap<UserId, User>>>,
    /// Active sessions
    sessions: Arc<RwLock<HashMap<String, Session>>>,
    /// Whether pairing is required for new users
    pairing_required: bool,
    /// Optional audit logger for auth events
    audit_log: Option<Arc<dyn AuditLogger>>,
    /// Optional SQLite persistence. When present the DB is the source of
    /// truth and the maps above are a write-through fast path; when absent
    /// the manager is purely in-memory (unchanged behavior).
    store: Option<Arc<AuthStore>>,
}

/// Session information
#[derive(Debug, Clone)]
pub struct Session {
    /// Session token
    pub token: String,
    /// User ID
    pub user_id: UserId,
    /// When the session was created
    pub created_at: chrono::DateTime<chrono::Utc>,
    /// When the session expires
    pub expires_at: chrono::DateTime<chrono::Utc>,
    /// Device fingerprint
    pub device_fingerprint: Option<String>,
    /// Granted scopes for this session
    pub scopes: Vec<String>,
}

impl AuthManager {
    /// Create a new auth manager
    pub fn new() -> Self {
        Self::default()
    }

    /// Require pairing for new users
    pub fn with_pairing_required(mut self, required: bool) -> Self {
        self.pairing_required = required;
        self
    }

    /// Attach an audit logger for auth events.
    pub fn with_audit_log(mut self, audit_log: Arc<dyn AuditLogger>) -> Self {
        self.audit_log = Some(audit_log);
        self
    }

    /// Attach a persistent store and restore surviving sessions into the
    /// in-memory fast path.
    ///
    /// Expired rows are pruned before loading. Only hashed token material is
    /// read back, so a restored session's `token` field carries its digest;
    /// validation still works because callers present the plaintext, which is
    /// hashed before lookup.
    pub async fn with_persistence(mut self, store: Arc<AuthStore>) -> crate::Result<Self> {
        store.init_schema().await?;

        let now_ms = chrono::Utc::now().timestamp_millis();
        match store.delete_expired_sessions(now_ms).await {
            Ok(0) => {}
            Ok(n) => debug!("Pruned {} expired session(s) on startup", n),
            Err(e) => warn!("Failed to prune expired sessions on startup: {}", e),
        }

        let loaded = store.load_sessions().await?;
        let mut restored = 0usize;
        {
            let mut sessions = self.sessions.write().await;
            for (token_hash, stored) in loaded {
                match stored.into_session(&token_hash) {
                    Some(session) if session.expires_at.timestamp_millis() > now_ms => {
                        sessions.insert(token_hash, session);
                        restored += 1;
                    }
                    // Expired or unrepresentable: skip; the row was either
                    // just pruned or is dropped on next access.
                    _ => {}
                }
            }
        }

        self.store = Some(store);
        info!("Auth manager restored {} persisted session(s)", restored);
        Ok(self)
    }

    /// Register a new user
    pub async fn register_user(&self, user: User) -> crate::Result<()> {
        let mut users = self.users.write().await;
        if users.contains_key(&user.id) {
            return Err(crate::error::SyscityError::Validation(format!(
                "User {} already exists",
                user.id
            )));
        }
        info!("Registered user: {}", user.id);
        users.insert(user.id.clone(), user);
        Ok(())
    }

    /// Get a user by ID
    pub async fn get_user(&self, user_id: &UserId) -> Option<User> {
        let users = self.users.read().await;
        users.get(user_id).cloned()
    }

    /// Check if a user exists
    pub async fn user_exists(&self, user_id: &UserId) -> bool {
        let users = self.users.read().await;
        users.contains_key(user_id)
    }

    /// Create a new session with optional scopes
    pub async fn create_session(
        &self,
        user_id: UserId,
        ttl_hours: i64,
        scopes: Option<Vec<String>>,
    ) -> crate::Result<Session> {
        // Verify user exists
        if !self.user_exists(&user_id).await {
            return Err(crate::error::SyscityError::Validation(format!(
                "User {} not found",
                user_id
            )));
        }

        // Use user's scopes if none provided
        let resolved_scopes = match scopes {
            Some(s) => s,
            None => {
                let users = self.users.read().await;
                users
                    .get(&user_id)
                    .map(|u| u.scopes.clone())
                    .unwrap_or_default()
            }
        };

        let mut token_bytes = [0u8; 32];
        OsRng.fill_bytes(&mut token_bytes);
        let token = URL_SAFE_NO_PAD.encode(token_bytes);
        let now = chrono::Utc::now();
        let session = Session {
            token: token.clone(),
            user_id: user_id.clone(),
            created_at: now,
            expires_at: now + chrono::Duration::hours(ttl_hours),
            device_fingerprint: None,
            scopes: resolved_scopes,
        };

        // Key the fast path by the token digest (never the plaintext) so the
        // in-memory map and the persisted map share one addressing scheme.
        let token_hash = auth_store::hash_session_token(&token);
        {
            let mut sessions = self.sessions.write().await;
            sessions.insert(token_hash.clone(), session.clone());
        }
        if let Some(ref store) = self.store {
            let stored = StoredSession::from_session(&session);
            if let Err(e) = store.upsert_session(&token_hash, &stored).await {
                warn!("Failed to persist session for user {}: {}", user_id, e);
            }
        }

        if let Some(ref audit) = self.audit_log {
            audit
                .log_entry(
                    AuditEventType::Login,
                    session.user_id.to_string(),
                    session.token.clone(),
                    true,
                    "Session created".to_string(),
                    Some(serde_json::json!({
                        "scopes": session.scopes,
                        "ttl_hours": ttl_hours,
                    })),
                )
                .await;
        }

        debug!("Created session for user: {} with scopes {:?}", user_id, session.scopes);
        Ok(session)
    }

    /// Validate that a session has all required scopes
    pub async fn validate_scopes(&self, token: &str, required: &[&str]) -> bool {
        let token_hash = auth_store::hash_session_token(token);
        let sessions = self.sessions.read().await;
        let Some(session) = sessions.get(&token_hash) else {
            return false;
        };
        if session.expires_at <= chrono::Utc::now() {
            return false;
        }
        // Admin scope bypasses all checks
        if session.scopes.contains(&"admin".to_string()) {
            return true;
        }
        required
            .iter()
            .all(|req| session.scopes.contains(&req.to_string()))
    }

    /// Validate a session token
    ///
    /// Reads the in-memory fast path first; on a cache miss (or a stale
    /// in-memory entry) it consults the store, which is the source of truth.
    /// Expired sessions are rejected and their rows cleaned up opportunistically.
    pub async fn validate_session(&self, token: &str) -> Option<Session> {
        let token_hash = auth_store::hash_session_token(token);
        let now = chrono::Utc::now();

        let cached = {
            let sessions = self.sessions.read().await;
            sessions.get(&token_hash).cloned()
        };

        let result = match cached {
            Some(session) if session.expires_at > now => Some(session),
            Some(_) => {
                // Stale in-memory entry: evict, then fall back to the store.
                self.sessions.write().await.remove(&token_hash);
                self.load_session_from_store(&token_hash, now).await
            }
            None => self.load_session_from_store(&token_hash, now).await,
        };

        if let Some(ref audit) = self.audit_log {
            if let Some(ref session) = result {
                audit
                    .log_entry(
                        AuditEventType::TokenValidation,
                        session.user_id.to_string(),
                        token.to_string(),
                        true,
                        "Token validation succeeded".to_string(),
                        Some(serde_json::json!({
                            "scopes": session.scopes,
                        })),
                    )
                    .await;
            } else {
                audit
                    .log_entry(
                        AuditEventType::TokenValidation,
                        "unknown".to_string(),
                        token.to_string(),
                        false,
                        "Token validation failed".to_string(),
                        None,
                    )
                    .await;
            }
        }

        result
    }

    /// Load a session from the store, warming the in-memory fast path.
    ///
    /// Returns `None` when there is no store, no row, an expired row (which is
    /// deleted), or an unrepresentable timestamp.
    async fn load_session_from_store(
        &self,
        token_hash: &str,
        now: chrono::DateTime<chrono::Utc>,
    ) -> Option<Session> {
        let store = self.store.as_ref()?;
        let stored = match store.load_session(token_hash).await {
            Ok(Some(stored)) => stored,
            Ok(None) => return None,
            Err(e) => {
                warn!("Failed to load session from store: {}", e);
                return None;
            }
        };

        if stored.expires_at_ms <= now.timestamp_millis() {
            if let Err(e) = store.delete_session(token_hash).await {
                warn!("Failed to delete expired session row: {}", e);
            }
            return None;
        }

        let session = stored.into_session(token_hash)?;
        self.sessions
            .write()
            .await
            .insert(token_hash.to_string(), session.clone());
        Some(session)
    }

    /// Revoke a session
    ///
    /// Removes it from the fast path and deletes the persisted row, so a
    /// revoked session stays revoked across a restart.
    pub async fn revoke_session(&self, token: &str) -> bool {
        let token_hash = auth_store::hash_session_token(token);
        let session = {
            let mut sessions = self.sessions.write().await;
            sessions.remove(&token_hash)
        };

        if let Some(ref store) = self.store {
            if let Err(e) = store.delete_session(&token_hash).await {
                warn!("Failed to delete revoked session row: {}", e);
            }
        }

        if let Some(ref session) = session {
            if let Some(ref audit) = self.audit_log {
                audit
                    .log_entry(
                        AuditEventType::Logout,
                        session.user_id.to_string(),
                        token.to_string(),
                        true,
                        "Session revoked".to_string(),
                        None,
                    )
                    .await;
            }
        }

        session.is_some()
    }

    /// Generate a pairing code (simplified implementation)
    pub fn generate_pairing_code(&self) -> String {
        // Generate a 6-digit code
        let code: u32 = rand::random::<u32>() % 900000 + 100000;
        code.to_string()
    }
}

/// Pattern type for allowlist matching
#[derive(Debug, Clone)]
pub enum UserPattern {
    /// Exact user ID match
    Exact(String),
    /// Prefix match (e.g., `admin_` matches `admin_alice`)
    Prefix(String),
    /// Glob pattern (e.g., `user_*` matches `user_123`)
    Glob(String),
    /// Regular expression pattern
    Regex(String),
}

impl UserPattern {
    /// Check if a user ID matches this pattern
    pub fn matches(&self, user_id: &str) -> bool {
        match self {
            UserPattern::Exact(pattern) => pattern == user_id,
            UserPattern::Prefix(prefix) => user_id.starts_with(prefix),
            UserPattern::Glob(glob) => match_glob(glob, user_id),
            UserPattern::Regex(pattern) => regex::Regex::new(pattern)
                .map(|re| re.is_match(user_id))
                .unwrap_or(false),
        }
    }

    /// Human-readable description of the pattern
    pub fn description(&self) -> String {
        match self {
            UserPattern::Exact(s) => format!("exact: {}", s),
            UserPattern::Prefix(s) => format!("prefix: {}*", s),
            UserPattern::Glob(s) => format!("glob: {}", s),
            UserPattern::Regex(s) => format!("regex: {}", s),
        }
    }
}

/// Simple glob-to-matcher: supports `*` (any chars) and `?` (single char).
fn match_glob(pattern: &str, text: &str) -> bool {
    let mut chars = pattern.chars().peekable();
    let mut text_chars = text.chars().peekable();

    while let Some(p) = chars.next() {
        match p {
            '*' => {
                // Skip consecutive stars
                while chars.peek() == Some(&'*') {
                    chars.next();
                }
                let next = chars.peek().copied();
                if next.is_none() {
                    return true; // trailing star matches everything
                }
                // Try to match the rest of the pattern at each position
                let rest: String = chars.collect();
                for i in 0..=text_chars.clone().count() {
                    let suffix: String = text_chars.clone().skip(i).collect();
                    if match_glob(&rest, &suffix) {
                        return true;
                    }
                }
                return false;
            }
            '?' => {
                if text_chars.next().is_none() {
                    return false;
                }
            }
            c => {
                if text_chars.next() != Some(c) {
                    return false;
                }
            }
        }
    }
    text_chars.next().is_none()
}

/// Allowlist pattern entry
#[derive(Debug, Clone)]
pub struct PatternAllowlistEntry {
    /// Pattern used for matching
    pub pattern: UserPattern,
    /// When access was granted
    pub granted_at: chrono::DateTime<chrono::Utc>,
    /// When access expires (None = never)
    pub expires_at: Option<chrono::DateTime<chrono::Utc>>,
    /// Who granted access
    pub granted_by: Option<String>,
    /// Reason for access
    pub reason: Option<String>,
}

/// Allowlist for controlling access
#[derive(Debug, Clone, Default)]
pub struct Allowlist {
    /// Allowed user IDs
    users: Arc<RwLock<HashMap<UserId, AllowlistEntry>>>,
    /// Allowed patterns
    patterns: Arc<RwLock<Vec<PatternAllowlistEntry>>>,
    /// Allowed IP addresses
    ips: Arc<RwLock<Vec<IpAddr>>>,
    /// Default allow policy
    default_allow: bool,
}

/// Allowlist entry for a user
#[derive(Debug, Clone)]
pub struct AllowlistEntry {
    /// User ID
    pub user_id: UserId,
    /// When access was granted
    pub granted_at: chrono::DateTime<chrono::Utc>,
    /// When access expires (None = never)
    pub expires_at: Option<chrono::DateTime<chrono::Utc>>,
    /// Who granted access
    pub granted_by: Option<String>,
    /// Reason for access
    pub reason: Option<String>,
}

impl Allowlist {
    /// Create a new allowlist
    pub fn new() -> Self {
        Self::default()
    }

    /// Set default allow policy
    pub fn with_default_allow(mut self, allow: bool) -> Self {
        self.default_allow = allow;
        self
    }

    /// Add a user to the allowlist
    pub async fn allow_user(
        &self,
        user_id: UserId,
        expires_at: Option<chrono::DateTime<chrono::Utc>>,
        granted_by: Option<String>,
        reason: Option<String>,
    ) {
        let mut users = self.users.write().await;
        users.insert(
            user_id.clone(),
            AllowlistEntry {
                user_id,
                granted_at: chrono::Utc::now(),
                expires_at,
                granted_by,
                reason,
            },
        );
    }

    /// Remove a user from the allowlist
    pub async fn deny_user(&self, user_id: &UserId) -> bool {
        let mut users = self.users.write().await;
        users.remove(user_id).is_some()
    }

    /// Add a pattern to the allowlist
    pub async fn allow_pattern(
        &self,
        pattern: UserPattern,
        expires_at: Option<chrono::DateTime<chrono::Utc>>,
        granted_by: Option<String>,
        reason: Option<String>,
    ) {
        let mut patterns = self.patterns.write().await;
        patterns.push(PatternAllowlistEntry {
            pattern,
            granted_at: chrono::Utc::now(),
            expires_at,
            granted_by,
            reason,
        });
    }

    /// Remove all patterns of a given description
    pub async fn deny_pattern(&self, description: &str) -> usize {
        let mut patterns = self.patterns.write().await;
        let before = patterns.len();
        patterns.retain(|p| p.pattern.description() != description);
        before - patterns.len()
    }

    /// Check if a user is allowed (exact match or pattern match)
    pub async fn is_allowed(&self, user_id: &UserId) -> bool {
        if self.default_allow {
            return true;
        }

        // Check exact match first
        let users = self.users.read().await;
        if let Some(entry) = users.get(user_id) {
            if let Some(expires) = entry.expires_at {
                return chrono::Utc::now() < expires;
            }
            return true;
        }
        drop(users);

        // Check pattern matches
        let patterns = self.patterns.read().await;
        let now = chrono::Utc::now();
        for entry in patterns.iter() {
            if let Some(expires) = entry.expires_at {
                if now >= expires {
                    continue;
                }
            }
            if entry.pattern.matches(&user_id.0) {
                return true;
            }
        }
        false
    }

    /// Add an IP to the allowlist
    pub async fn allow_ip(&self, ip: IpAddr) {
        let mut ips = self.ips.write().await;
        if !ips.contains(&ip) {
            ips.push(ip);
        }
    }

    /// Check if an IP is allowed
    pub async fn is_ip_allowed(&self, ip: &IpAddr) -> bool {
        let ips = self.ips.read().await;
        ips.is_empty() || ips.contains(ip)
    }

    /// List all allowed users
    pub async fn list_allowed_users(&self) -> Vec<AllowlistEntry> {
        let users = self.users.read().await;
        users.values().cloned().collect()
    }

    /// List all allowed patterns
    pub async fn list_allowed_patterns(&self) -> Vec<PatternAllowlistEntry> {
        let patterns = self.patterns.read().await;
        patterns.clone()
    }
}

/// Rate limiter using token bucket algorithm
#[derive(Debug, Clone)]
pub struct RateLimiter {
    /// Buckets per user
    buckets: Arc<RwLock<HashMap<UserId, TokenBucket>>>,
    /// Bucket capacity (tokens)
    capacity: u32,
    /// Refill rate (tokens per second)
    refill_rate: f64,
}

/// Token bucket for rate limiting
#[derive(Debug, Clone)]
struct TokenBucket {
    /// Current tokens
    tokens: f64,
    /// Last refill time
    last_refill: chrono::DateTime<chrono::Utc>,
    /// Capacity
    capacity: f64,
    /// Refill rate (tokens per second)
    refill_rate: f64,
}

impl TokenBucket {
    /// Create a new bucket
    fn new(capacity: f64, refill_rate: f64) -> Self {
        Self {
            tokens: capacity,
            last_refill: chrono::Utc::now(),
            capacity,
            refill_rate,
        }
    }

    /// Refill tokens based on elapsed time
    fn refill(&mut self) {
        let now = chrono::Utc::now();
        let elapsed = (now - self.last_refill).num_milliseconds() as f64 / 1000.0;
        let tokens_to_add = elapsed * self.refill_rate;

        self.tokens = (self.tokens + tokens_to_add).min(self.capacity);
        self.last_refill = now;
    }

    /// Try to consume tokens
    fn consume(&mut self, amount: f64) -> bool {
        self.refill();
        if self.tokens >= amount {
            self.tokens -= amount;
            true
        } else {
            false
        }
    }

    /// Get remaining tokens
    fn remaining(&self) -> f64 {
        self.tokens
    }
}

impl RateLimiter {
    /// Create a new rate limiter
    pub fn new(capacity: u32, refill_rate: f64) -> Self {
        Self {
            buckets: Arc::new(RwLock::new(HashMap::new())),
            capacity,
            refill_rate,
        }
    }

    /// Check if a request is allowed (consumes 1 token)
    pub async fn check(&self, user_id: &UserId) -> RateLimitResult {
        self.check_with_cost(user_id, 1.0).await
    }

    /// Check with custom cost
    pub async fn check_with_cost(&self, user_id: &UserId, cost: f64) -> RateLimitResult {
        let mut buckets = self.buckets.write().await;
        let bucket = buckets
            .entry(user_id.clone())
            .or_insert_with(|| TokenBucket::new(self.capacity as f64, self.refill_rate));

        if bucket.consume(cost) {
            RateLimitResult::Allowed {
                remaining: bucket.remaining() as u32,
                reset_after_secs: ((self.capacity as f64 - bucket.remaining()) / self.refill_rate)
                    as u64,
            }
        } else {
            RateLimitResult::Denied {
                retry_after_secs: ((cost - bucket.remaining()) / self.refill_rate) as u64,
            }
        }
    }

    /// Get current bucket state for a user
    pub async fn get_state(&self, user_id: &UserId) -> Option<RateLimitState> {
        let buckets = self.buckets.read().await;
        buckets.get(user_id).map(|b| RateLimitState {
            remaining: b.remaining() as u32,
            capacity: self.capacity,
        })
    }
}

/// Rate limit check result
#[derive(Debug, Clone)]
pub enum RateLimitResult {
    /// Request is allowed
    Allowed {
        /// Remaining tokens
        remaining: u32,
        /// Seconds until bucket is full
        reset_after_secs: u64,
    },
    /// Request is denied
    Denied {
        /// Seconds until request can be retried
        retry_after_secs: u64,
    },
}

impl RateLimitResult {
    /// Check if the request is allowed
    pub fn is_allowed(&self) -> bool {
        matches!(self, RateLimitResult::Allowed { .. })
    }
}

/// Rate limit state
#[derive(Debug, Clone)]
pub struct RateLimitState {
    /// Remaining tokens
    pub remaining: u32,
    /// Bucket capacity
    pub capacity: u32,
}

/// Rate limit headers for HTTP responses
#[derive(Debug, Clone)]
pub struct RateLimitHeaders {
    /// The maximum number of requests allowed in the current window
    pub limit: u32,
    /// The number of requests remaining in the current window
    pub remaining: u32,
    /// Unix timestamp when the rate limit resets
    pub reset: u64,
    /// Seconds until the rate limit resets (optional, for convenience)
    pub reset_after: Option<u64>,
    /// The rate limit policy (e.g., "10;w=60" for 10 requests per 60 seconds)
    pub policy: String,
}

impl RateLimitHeaders {
    /// Create headers from a rate limit result
    pub fn from_result(result: &RateLimitResult, capacity: u32, policy: impl Into<String>) -> Self {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();

        match result {
            RateLimitResult::Allowed { remaining, reset_after_secs } => Self {
                limit: capacity,
                remaining: *remaining,
                reset: now + reset_after_secs,
                reset_after: Some(*reset_after_secs),
                policy: policy.into(),
            },
            RateLimitResult::Denied { retry_after_secs } => Self {
                limit: capacity,
                remaining: 0,
                reset: now + retry_after_secs,
                reset_after: Some(*retry_after_secs),
                policy: policy.into(),
            },
        }
    }

    /// Convert to HTTP header tuples
    pub fn to_headers(&self) -> Vec<(String, String)> {
        let mut headers = vec![
            ("X-RateLimit-Limit".to_string(), self.limit.to_string()),
            ("X-RateLimit-Remaining".to_string(), self.remaining.to_string()),
            ("X-RateLimit-Reset".to_string(), self.reset.to_string()),
            ("RateLimit-Policy".to_string(), self.policy.clone()),
        ];

        if let Some(reset_after) = self.reset_after {
            headers.push(("Retry-After".to_string(), reset_after.to_string()));
            headers.push(("X-RateLimit-Reset-After".to_string(), reset_after.to_string()));
        }

        headers
    }

    /// Create headers for a successful request with remaining quota
    pub fn allowed(remaining: u32, reset: u64, policy: impl Into<String>) -> Self {
        Self {
            limit: remaining + 1,
            remaining,
            reset,
            reset_after: None,
            policy: policy.into(),
        }
    }

    /// Create headers for a rate-limited request
    pub fn denied(retry_after: u64, capacity: u32, policy: impl Into<String>) -> Self {
        Self {
            limit: capacity,
            remaining: 0,
            reset: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs()
                + retry_after,
            reset_after: Some(retry_after),
            policy: policy.into(),
        }
    }
}

/// Rate limit notification for users
#[derive(Debug, Clone)]
pub struct RateLimitNotification {
    /// Whether the request was allowed
    pub allowed: bool,
    /// Remaining requests
    pub remaining: u32,
    /// Total capacity
    pub limit: u32,
    /// Reset timestamp
    pub reset_at: chrono::DateTime<chrono::Utc>,
    /// Human-readable message
    pub message: String,
}

impl RateLimitNotification {
    /// Create a notification from rate limit headers
    pub fn from_headers(headers: &RateLimitHeaders) -> Self {
        let reset_at = chrono::DateTime::from_timestamp(headers.reset as i64, 0)
            .unwrap_or_else(chrono::Utc::now);

        let (allowed, message) = if headers.remaining == 0 {
            (
                false,
                format!(
                    "Rate limit exceeded. Please try again in {} seconds.",
                    headers.reset_after.unwrap_or(60)
                ),
            )
        } else {
            let percentage = (headers.remaining as f32 / headers.limit as f32 * 100.0) as u32;

            let msg = if percentage < 20 {
                format!(
                    "Warning: You have {} requests remaining ({}% of your quota).",
                    headers.remaining, percentage
                )
            } else {
                format!("{} of {} requests remaining.", headers.remaining, headers.limit)
            };

            (true, msg)
        };

        Self {
            allowed,
            remaining: headers.remaining,
            limit: headers.limit,
            reset_at,
            message,
        }
    }

    /// Create a simple notification
    pub fn simple(remaining: u32, limit: u32) -> Self {
        Self {
            allowed: remaining > 0,
            remaining,
            limit,
            reset_at: chrono::Utc::now() + chrono::Duration::minutes(1),
            message: format!("{} of {} requests remaining.", remaining, limit),
        }
    }

    /// Format as a user-friendly message
    pub fn to_message(&self) -> String {
        self.message.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_allowlist() {
        let allowlist = Allowlist::new();
        let user_id = UserId::new("user1");

        assert!(!allowlist.is_allowed(&user_id).await);

        allowlist
            .allow_user(user_id.clone(), None, None, None)
            .await;
        assert!(allowlist.is_allowed(&user_id).await);

        allowlist.deny_user(&user_id).await;
        assert!(!allowlist.is_allowed(&user_id).await);
    }

    #[tokio::test]
    async fn test_rate_limiter() {
        let limiter = RateLimiter::new(10, 1.0); // 10 tokens, 1 per second
        let user_id = UserId::new("user1");

        // Should allow first 10 requests
        for _ in 0..10 {
            assert!(limiter.check(&user_id).await.is_allowed());
        }

        // 11th request should be denied
        assert!(!limiter.check(&user_id).await.is_allowed());
    }

    #[test]
    fn test_token_bucket() {
        let mut bucket = TokenBucket::new(10.0, 1.0);
        assert!(bucket.consume(5.0));
        assert_eq!(bucket.remaining(), 5.0);

        assert!(bucket.consume(5.0));
        assert_eq!(bucket.remaining(), 0.0);

        assert!(!bucket.consume(1.0));
    }

    #[tokio::test]
    async fn test_auth_manager() {
        let auth = AuthManager::new();
        let user = User::new("user1", "Test User");

        assert!(!auth.user_exists(&user.id).await);

        auth.register_user(user.clone()).await.unwrap();
        assert!(auth.user_exists(&user.id).await);

        let session = auth
            .create_session(user.id.clone(), 24, None)
            .await
            .unwrap();
        assert!(auth.validate_session(&session.token).await.is_some());
    }

    #[tokio::test]
    async fn test_auth_manager_audit_login_logout() {
        use crate::security::runtime_audit::{AuditEventType, RuntimeAuditLog};

        let audit_log = Arc::new(RuntimeAuditLog::with_capacity(100));
        let auth = AuthManager::new().with_audit_log(audit_log.clone());
        let user = User::new("audited_user", "Audited User");

        auth.register_user(user.clone()).await.unwrap();

        let session = auth
            .create_session(user.id.clone(), 24, None)
            .await
            .unwrap();

        let logins = audit_log.filter(AuditEventType::Login).await;
        assert_eq!(logins.len(), 1);
        assert_eq!(logins[0].actor, "audited_user");
        assert!(logins[0].allowed);

        assert!(auth.revoke_session(&session.token).await);

        let logouts = audit_log.filter(AuditEventType::Logout).await;
        assert_eq!(logouts.len(), 1);
        assert_eq!(logouts[0].actor, "audited_user");
        assert!(logouts[0].allowed);
    }

    #[tokio::test]
    async fn test_auth_manager_audit_token_validation() {
        use crate::security::runtime_audit::{AuditEventType, RuntimeAuditLog};

        let audit_log = Arc::new(RuntimeAuditLog::with_capacity(100));
        let auth = AuthManager::new().with_audit_log(audit_log.clone());
        let user = User::new("token_user", "Token User");

        auth.register_user(user.clone()).await.unwrap();

        // Failed validation before session exists
        assert!(auth.validate_session("invalid-token").await.is_none());

        let session = auth
            .create_session(user.id.clone(), 24, None)
            .await
            .unwrap();

        // Successful validation
        assert!(auth.validate_session(&session.token).await.is_some());

        let validations = audit_log.filter(AuditEventType::TokenValidation).await;
        assert_eq!(validations.len(), 2);
        assert!(!validations[0].allowed);
        assert_eq!(validations[0].actor, "unknown");
        assert!(validations[1].allowed);
        assert_eq!(validations[1].actor, "token_user");
    }

    // ── Persistence (T1: sessions + device pairings survive restart) ────────

    use crate::security::device_pairing::{DeviceAccessResult, DevicePairingStore};

    /// Open a file-backed SQLite pool so writes are visible across "restart"
    /// (a fresh `AuthManager` / `DevicePairingStore` over the same database).
    async fn persistence_pool(dir: &std::path::Path) -> sqlx::SqlitePool {
        let db_path = dir.join("auth-persistence-test.db");
        let url = format!("sqlite://{}?mode=rwc", db_path.display());
        sqlx::sqlite::SqlitePoolOptions::new()
            .max_connections(1)
            .connect(&url)
            .await
            .expect("open file-backed sqlite pool")
    }

    /// Request + approve a pairing, returning the plaintext device token.
    async fn pair_device(store: &DevicePairingStore, device_id: &str) -> String {
        let code = match store
            .request_access(device_id, Some("Test Device"), None)
            .await
        {
            DeviceAccessResult::PairingRequired { code } => code,
            other => panic!("expected PairingRequired, got {:?}", other),
        };
        store
            .approve(&code, Some("admin"))
            .await
            .expect("approve returns a device token")
    }

    #[tokio::test]
    async fn session_survives_restart() {
        let dir = tempfile::tempdir().unwrap();
        let pool = persistence_pool(dir.path()).await;
        let store = Arc::new(AuthStore::new(pool.clone()));

        let user = User::new("persist_user", "Persist User");
        let token = {
            let auth = AuthManager::new()
                .with_persistence(store.clone())
                .await
                .unwrap();
            auth.register_user(user.clone()).await.unwrap();
            auth.create_session(user.id.clone(), 24, Some(vec!["chat".to_string()]))
                .await
                .unwrap()
                .token
        };

        // Fresh manager over the same database — as if the process restarted.
        let auth = AuthManager::new()
            .with_persistence(store.clone())
            .await
            .unwrap();
        let restored = auth.validate_session(&token).await;
        assert!(restored.is_some(), "session should survive restart");
        assert_eq!(restored.unwrap().user_id, user.id);
        assert!(auth.validate_scopes(&token, &["chat"]).await);
    }

    #[tokio::test]
    async fn expired_session_rejected_after_reload() {
        let dir = tempfile::tempdir().unwrap();
        let pool = persistence_pool(dir.path()).await;
        let store = Arc::new(AuthStore::new(pool.clone()));
        let user = User::new("expired_user", "Expired User");

        let token = {
            let auth = AuthManager::new()
                .with_persistence(store.clone())
                .await
                .unwrap();
            auth.register_user(user.clone()).await.unwrap();
            // TTL of 0 hours → already expired at persistence time.
            auth.create_session(user.id.clone(), 0, None)
                .await
                .unwrap()
                .token
        };

        let auth = AuthManager::new()
            .with_persistence(store.clone())
            .await
            .unwrap();
        assert!(
            auth.validate_session(&token).await.is_none(),
            "expired session must not validate after reload"
        );
    }

    #[tokio::test]
    async fn expired_row_is_rejected_even_when_present() {
        let dir = tempfile::tempdir().unwrap();
        let pool = persistence_pool(dir.path()).await;
        let store = Arc::new(AuthStore::new(pool.clone()));
        // A manager that never loaded the store: the row must still be
        // rejected on the store fallback path.
        let auth = AuthManager::new()
            .with_persistence(store.clone())
            .await
            .unwrap();

        let token = "manually-planted-token";
        store
            .upsert_session(
                &auth_store::hash_session_token(token),
                &StoredSession {
                    user_id: "planted".to_string(),
                    created_at_ms: 0,
                    expires_at_ms: 1, // long past
                    device_fingerprint: None,
                    scopes: vec![],
                },
            )
            .await
            .unwrap();

        assert!(
            auth.validate_session(token).await.is_none(),
            "a present-but-expired row must be rejected"
        );
        // The rejected row is cleaned up opportunistically.
        assert_eq!(store.load_sessions().await.unwrap().len(), 0);
    }

    #[tokio::test]
    async fn revoked_session_rejected_after_reload() {
        let dir = tempfile::tempdir().unwrap();
        let pool = persistence_pool(dir.path()).await;
        let store = Arc::new(AuthStore::new(pool.clone()));
        let user = User::new("revoked_user", "Revoked User");

        let token = {
            let auth = AuthManager::new()
                .with_persistence(store.clone())
                .await
                .unwrap();
            auth.register_user(user.clone()).await.unwrap();
            let token = auth
                .create_session(user.id.clone(), 24, None)
                .await
                .unwrap()
                .token;
            assert!(auth.revoke_session(&token).await);
            token
        };

        let auth = AuthManager::new()
            .with_persistence(store.clone())
            .await
            .unwrap();
        assert!(
            auth.validate_session(&token).await.is_none(),
            "revoked session must not validate after reload"
        );
        assert_eq!(store.load_sessions().await.unwrap().len(), 0);
    }

    #[tokio::test]
    async fn device_pairing_survives_restart() {
        let dir = tempfile::tempdir().unwrap();
        let pool = persistence_pool(dir.path()).await;
        let store = Arc::new(AuthStore::new(pool.clone()));

        let token = {
            let devices = DevicePairingStore::new()
                .with_persistence(store.clone())
                .await
                .unwrap();
            pair_device(&devices, "dev-persist").await
        };

        let devices = DevicePairingStore::new()
            .with_persistence(store.clone())
            .await
            .unwrap();
        assert_eq!(
            devices.validate_token(&token).await,
            Some("dev-persist".to_string()),
            "device pairing should survive restart"
        );
        assert!(devices
            .list_authorized()
            .await
            .iter()
            .any(|d| d.device_id == "dev-persist"));
    }

    #[tokio::test]
    async fn revoked_device_rejected_after_reload() {
        let dir = tempfile::tempdir().unwrap();
        let pool = persistence_pool(dir.path()).await;
        let store = Arc::new(AuthStore::new(pool.clone()));

        let token = {
            let devices = DevicePairingStore::new()
                .with_persistence(store.clone())
                .await
                .unwrap();
            let token = pair_device(&devices, "dev-revoke").await;
            assert!(devices.revoke("dev-revoke").await);
            token
        };

        let devices = DevicePairingStore::new()
            .with_persistence(store.clone())
            .await
            .unwrap();
        assert!(devices.validate_token(&token).await.is_none());
        assert!(devices.list_authorized().await.is_empty());
    }

    #[tokio::test]
    async fn no_plaintext_token_persisted() {
        let dir = tempfile::tempdir().unwrap();
        let pool = persistence_pool(dir.path()).await;
        let store = Arc::new(AuthStore::new(pool.clone()));
        let user = User::new("noplain_user", "NoPlain User");

        let auth = AuthManager::new()
            .with_persistence(store.clone())
            .await
            .unwrap();
        auth.register_user(user.clone()).await.unwrap();
        let session_token = auth
            .create_session(user.id.clone(), 24, None)
            .await
            .unwrap()
            .token;

        let devices = DevicePairingStore::new()
            .with_persistence(store.clone())
            .await
            .unwrap();
        let device_token = pair_device(&devices, "dev-noplain").await;

        // Raw read-back: the plaintext must not appear in any stored column.
        let session_rows: Vec<(String, String, Option<String>, String)> = sqlx::query_as(
            "SELECT token_hash, user_id, device_fingerprint, scopes_json FROM auth_sessions",
        )
        .fetch_all(&pool)
        .await
        .unwrap();
        assert_eq!(session_rows.len(), 1);
        for (hash, user_id, fp, scopes) in &session_rows {
            assert_ne!(hash, &session_token);
            assert!(!hash.contains(&session_token));
            assert_ne!(user_id, &session_token);
            assert_ne!(fp.as_deref(), Some(session_token.as_str()));
            assert!(!scopes.contains(&session_token));
            // The stored digest is exactly the expected hash.
            assert_eq!(hash, &auth_store::hash_session_token(&session_token));
        }

        let device_rows: Vec<(String, String, Option<String>, Option<String>)> = sqlx::query_as(
            "SELECT device_id, token_hash, display_name, approved_by FROM auth_devices",
        )
        .fetch_all(&pool)
        .await
        .unwrap();
        assert_eq!(device_rows.len(), 1);
        for (device_id, hash, display_name, approved_by) in &device_rows {
            assert_ne!(device_id, &device_token);
            assert_ne!(hash, &device_token);
            assert!(!hash.contains(&device_token));
            assert_ne!(display_name.as_deref(), Some(device_token.as_str()));
            assert_ne!(approved_by.as_deref(), Some(device_token.as_str()));
            assert_eq!(hash, &auth_store::hash_device_token(&device_token));
        }
    }
}

// Device fingerprinting for security tracking

/// Secret scanning for detecting sensitive data leaks
pub mod secrets {
    use std::sync::LazyLock;

    use regex::Regex;

    #[allow(clippy::unwrap_used)]
    static DEFAULT_PATTERNS: LazyLock<Vec<SecretPattern>> = LazyLock::new(|| {
        vec![
            // API Keys
            SecretPattern {
                name: "OpenAI API Key",
                regex: Regex::new(r"sk-[a-zA-Z0-9]{48}").unwrap(),
                severity: Severity::Critical,
                description: "OpenAI API key detected",
            },
            SecretPattern {
                name: "Anthropic API Key",
                regex: Regex::new(r"sk-ant-[a-zA-Z0-9_-]{40,}").unwrap(),
                severity: Severity::Critical,
                description: "Anthropic API key detected",
            },
            SecretPattern {
                name: "AWS Access Key ID",
                regex: Regex::new(r"AKIA[0-9A-Z]{16}").unwrap(),
                severity: Severity::Critical,
                description: "AWS Access Key ID detected",
            },
            SecretPattern {
                name: "AWS Secret Access Key",
                regex: Regex::new(r"[0-9a-zA-Z/+]{40}").unwrap(),
                severity: Severity::Critical,
                description: "Potential AWS Secret Key detected",
            },
            // Private Keys
            SecretPattern {
                name: "RSA Private Key",
                regex: Regex::new(r"-----BEGIN (RSA )?PRIVATE KEY-----").unwrap(),
                severity: Severity::Critical,
                description: "RSA private key detected",
            },
            SecretPattern {
                name: "SSH Private Key",
                regex: Regex::new(r"-----BEGIN OPENSSH PRIVATE KEY-----").unwrap(),
                severity: Severity::Critical,
                description: "SSH private key detected",
            },
            SecretPattern {
                name: "PGP Private Key",
                regex: Regex::new(r"-----BEGIN PGP PRIVATE KEY BLOCK-----").unwrap(),
                severity: Severity::Critical,
                description: "PGP private key detected",
            },
            // Database URLs
            SecretPattern {
                name: "Database Connection String",
                regex: Regex::new(r"(postgres|mysql|mongodb)://[^:]+:[^@]+@").unwrap(),
                severity: Severity::High,
                description: "Database connection string with password detected",
            },
            // Tokens
            SecretPattern {
                name: "Bearer Token",
                regex: Regex::new(r"(?i)bearer\s+[a-zA-Z0-9_\-\.]{20,}").unwrap(),
                severity: Severity::High,
                description: "Bearer token detected",
            },
            SecretPattern {
                name: "GitHub Token",
                regex: Regex::new(r"gh[pousr]_[A-Za-z0-9_]{36,}").unwrap(),
                severity: Severity::Critical,
                description: "GitHub personal access token detected",
            },
            SecretPattern {
                name: "Slack Token",
                regex: Regex::new(r"xox[baprs]-[0-9a-zA-Z\-]{10,48}").unwrap(),
                severity: Severity::Critical,
                description: "Slack API token detected",
            },
            SecretPattern {
                name: "Discord Token",
                regex: Regex::new(r"[MN][A-Za-z\d]{23}\.[\w-]{6}\.[\w-]{27}").unwrap(),
                severity: Severity::Critical,
                description: "Discord bot token detected",
            },
            // Generic patterns
            SecretPattern {
                name: "Generic API Key",
                regex: Regex::new(r"(?i)(api[_-]?key|apikey)\s*[=:]\s*[a-zA-Z0-9_-]{16,}").unwrap(),
                severity: Severity::Medium,
                description: "Potential API key detected",
            },
            SecretPattern {
                name: "Generic Secret",
                regex: Regex::new(r"(?i)(secret|password|passwd|pwd)\s*[=:]\s*[^\s]{8,}").unwrap(),
                severity: Severity::Medium,
                description: "Potential password/secret detected",
            },
            SecretPattern {
                name: "JWT Token",
                regex: Regex::new(r"eyJ[a-zA-Z0-9_-]*\.eyJ[a-zA-Z0-9_-]*\.[a-zA-Z0-9_-]*").unwrap(),
                severity: Severity::High,
                description: "JWT token detected",
            },
        ]
    });
    #[derive(Debug, Clone)]
    pub struct SecretPattern {
        /// Pattern name
        pub name: &'static str,
        /// Regex pattern for detection
        pub regex: Regex,
        /// Severity level
        pub severity: Severity,
        /// Description of the secret type
        pub description: &'static str,
    }

    /// Severity levels for secret detection
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
    pub enum Severity {
        /// Critical - API keys, private keys
        Critical,
        /// High - Database passwords, auth tokens
        High,
        /// Medium - Config secrets, session IDs
        Medium,
        /// Low - Less sensitive patterns
        Low,
    }

    impl std::fmt::Display for Severity {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            match self {
                Severity::Critical => write!(f, "CRITICAL"),
                Severity::High => write!(f, "HIGH"),
                Severity::Medium => write!(f, "MEDIUM"),
                Severity::Low => write!(f, "LOW"),
            }
        }
    }

    /// Detected secret
    #[derive(Debug, Clone)]
    pub struct DetectedSecret {
        /// Pattern name
        pub pattern: String,
        /// Severity level
        pub severity: Severity,
        /// Line number where found
        pub line_number: usize,
        /// Matched content (redacted for display)
        pub redacted: String,
        /// Full description
        pub description: String,
        /// The original matched text (for precise replacement in redaction)
        pub original: String,
    }

    impl DetectedSecret {
        /// Redact sensitive content for safe display
        pub fn redact(content: &str) -> String {
            if content.len() <= 8 {
                "***".to_string()
            } else {
                format!("{}...{}", &content[..4], &content[content.len() - 4..])
            }
        }
    }

    /// Secret scanner
    #[derive(Debug, Clone)]
    pub struct SecretScanner {
        patterns: Vec<SecretPattern>,
    }

    impl Default for SecretScanner {
        fn default() -> Self {
            Self::with_default_patterns()
        }
    }

    impl SecretScanner {
        /// Create a new scanner with default patterns
        pub fn with_default_patterns() -> Self {
            Self {
                patterns: DEFAULT_PATTERNS.clone(),
            }
        }
        /// Create an empty scanner
        pub fn empty() -> Self {
            Self { patterns: vec![] }
        }

        /// Add a custom pattern
        pub fn add_pattern(&mut self, pattern: SecretPattern) {
            self.patterns.push(pattern);
        }

        /// Scan text for secrets
        pub fn scan(&self, text: &str) -> Vec<DetectedSecret> {
            let mut findings = Vec::new();

            for (line_num, line) in text.lines().enumerate() {
                for pattern in &self.patterns {
                    for mat in pattern.regex.find_iter(line) {
                        let raw = mat.as_str();
                        findings.push(DetectedSecret {
                            pattern: pattern.name.to_string(),
                            severity: pattern.severity,
                            line_number: line_num + 1,
                            redacted: DetectedSecret::redact(raw),
                            description: pattern.description.to_string(),
                            original: raw.to_string(),
                        });
                    }
                }
            }

            findings
        }

        /// Scan a single line
        pub fn scan_line(&self, line: &str, line_number: usize) -> Vec<DetectedSecret> {
            let mut findings = Vec::new();

            for pattern in &self.patterns {
                for mat in pattern.regex.find_iter(line) {
                    let raw = mat.as_str();
                    findings.push(DetectedSecret {
                        pattern: pattern.name.to_string(),
                        severity: pattern.severity,
                        line_number,
                        redacted: DetectedSecret::redact(raw),
                        description: pattern.description.to_string(),
                        original: raw.to_string(),
                    });
                }
            }

            findings
        }

        /// Check if text contains any secrets
        pub fn contains_secrets(&self, text: &str) -> bool {
            !self.scan(text).is_empty()
        }

        /// Get all patterns
        pub fn patterns(&self) -> &[SecretPattern] {
            &self.patterns
        }
    }

    /// Scan result summary
    #[derive(Debug, Clone)]
    pub struct ScanSummary {
        /// Total secrets found
        pub total: usize,
        /// By severity
        pub by_severity: std::collections::HashMap<Severity, usize>,
        /// Unique patterns found
        pub unique_patterns: Vec<String>,
    }

    impl From<Vec<DetectedSecret>> for ScanSummary {
        fn from(secrets: Vec<DetectedSecret>) -> Self {
            let mut by_severity: std::collections::HashMap<Severity, usize> =
                std::collections::HashMap::new();
            let mut unique: std::collections::HashSet<String> = std::collections::HashSet::new();

            for secret in &secrets {
                *by_severity.entry(secret.severity).or_insert(0) += 1;
                unique.insert(secret.pattern.clone());
            }

            Self {
                total: secrets.len(),
                by_severity,
                unique_patterns: unique.into_iter().collect(),
            }
        }
    }

    /// Quick scan function
    pub fn scan_text(text: &str) -> Vec<DetectedSecret> {
        let scanner = SecretScanner::default();
        scanner.scan(text)
    }

    /// Quick check function
    pub fn contains_secrets(text: &str) -> bool {
        let scanner = SecretScanner::default();
        !scanner.scan(text).is_empty()
    }
}

#[cfg(test)]
mod secret_tests {
    use super::secrets::*;

    #[test]
    fn test_detect_openai_key() {
        let scanner = SecretScanner::with_default_patterns();
        let text = "sk-abcdefghijklmnopqrstuvwxyz123456789012345678901234567";
        let findings = scanner.scan(text);
        assert!(!findings.is_empty());
        assert!(findings.iter().any(|f| f.pattern == "OpenAI API Key"));
    }

    #[test]
    fn test_detect_aws_key() {
        let scanner = SecretScanner::with_default_patterns();
        let text = "AKIAIOSFODNN7EXAMPLE";
        let findings = scanner.scan(text);
        assert!(!findings.is_empty());
        assert!(findings.iter().any(|f| f.pattern == "AWS Access Key ID"));
    }

    #[test]
    fn test_detect_private_key() {
        let scanner = SecretScanner::with_default_patterns();
        let text = "-----BEGIN RSA PRIVATE KEY-----\nMIIEpAIBAAKCAQEA...";
        let findings = scanner.scan(text);
        assert!(!findings.is_empty());
        assert!(findings.iter().any(|f| f.pattern == "RSA Private Key"));
    }

    #[test]
    fn test_detect_jwt() {
        let scanner = SecretScanner::with_default_patterns();
        let text = "eyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9.\
                    eyJzdWIiOiIxMjM0NTY3ODkwIiwibmFtZSI6IkpvaG4gRG9lIiwiaWF0IjoxNTE2MjM5MDIyfQ.\
                    SflKxwRJSMeKKF2QT4fwpMeJf36POk6yJV_adQssw5c";
        let findings = scanner.scan(text);
        assert!(!findings.is_empty());
        assert!(findings.iter().any(|f| f.pattern == "JWT Token"));
    }

    #[test]
    fn test_no_false_positives() {
        let scanner = SecretScanner::with_default_patterns();
        let text = "This is just regular text without any secrets.";
        let findings = scanner.scan(text);
        assert!(findings.is_empty());
    }

    #[test]
    fn test_redaction() {
        let secret = "sk-abcdefghijklmnopqrstuvwxyz123456";
        let redacted = super::secrets::DetectedSecret::redact(secret);
        // For long strings, redact shows first 4 and last 4 chars with ... in between
        assert!(redacted.starts_with("sk-a"));
        assert!(redacted.ends_with("3456"));
        assert!(redacted.contains("..."));
        assert!(!redacted.contains("abcdefghijklmnop"));
    }
}

/// SQLite persistence for authenticated sessions and device pairings
pub mod auth_store;

/// Security audit module
pub mod audit;

/// Runtime audit log for security-relevant events
pub mod runtime_audit;

/// Persistent SQLite-backed audit log
pub mod persistent_audit;

/// DM pairing and access control
pub mod pairing;

/// Device pairing for WebSocket-native protocol
pub mod device_pairing;

/// Mention gating for controlling agent responses to mentions
pub mod mention_gate;

/// Sliding window rate limiter for per-user, per-endpoint rate limiting
pub mod sliding_window;

/// Dynamic security penetration testing
pub mod pentest;

/// Tailscale authentication
pub mod tailscale;

/// Trusted proxy authentication
pub mod trusted_proxy;

/// Multi-dimensional allowlist
pub mod allowlist;

/// PII detection and content filtering
pub mod pii;

/// Content filter for tool outputs
pub mod content_filter;

// Re-export SecurityValidator and validation types from tools module for use in
// security tests
// Re-export content filter types
pub use content_filter::{ContentFilter, ContentFilterOutcome, FilterAction};
// Re-export PII types
pub use pii::{DataClassification, DetectedPii, FilterResult, PiiDetector, PiiPattern};
// Re-export audit logger trait
pub use runtime_audit::AuditLogger;
// Re-export trusted proxy types
pub use trusted_proxy::{
    TrustedProxyAuthenticator, TrustedProxyConfig, TrustedProxyError, TrustedProxyUser,
};

pub use crate::tools::{SecurityValidator, ToolValidationError, ToolValidator};
