//! DM Pairing and Access Control
//!
//! Implements a reactive pairing flow where users initiate
//! pairing requests by messaging the bot. Admins approve requests by code.
//!
//! Flow:
//! 1. User sends first DM to bot
//! 2. System auto-captures user ID and generates a pairing code
//! 3. Admin lists pending requests: `syscity pairing list --channel telegram`
//! 4. Admin approves by code: `syscity pairing approve telegram ABC123`
//! 5. User is notified and can now chat
//!
//! # Example
//!
//! ```rust
//! use std::time::Duration;
//!
//! use syscity::security::pairing::{DmPolicy, PairingStore};
//!
//! # async fn example() {
//! let store = PairingStore::new();
//!
//! // User initiates pairing (called by channel handler)
//! let result = store
//!     .request_access("telegram", "123456", Some("@alice"))
//!     .await
//!     .unwrap();
//! let code = match result {
//!     syscity::security::pairing::RequestAccessResult::NewRequest { code } => code,
//!     _ => panic!("Expected new request"),
//! };
//!
//! // Admin approves the request
//! let approved = store.approve("telegram", &code, Some("admin")).await;
//! assert!(approved.is_some());
//!
//! // User is now authorized
//! assert!(store.is_authorized("telegram", "123456").await);
//! # }
//! ```

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use serde::{Deserialize, Serialize};
use tokio::sync::RwLock;
use tracing::{debug, info, warn};

// ── DM Policy
// ─────────────────────────────────────────────────────────────────

/// DM access policy for channels.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[derive(Default)]
pub enum DmPolicy {
    /// Anyone can DM the bot (no restrictions).
    #[default]
    Open,
    /// Users must request access; admin approves by code.
    Pairing,
    /// Only pre-approved users can DM (static allowlist).
    Allowlist,
}

impl std::fmt::Display for DmPolicy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DmPolicy::Open => write!(f, "open"),
            DmPolicy::Pairing => write!(f, "pairing"),
            DmPolicy::Allowlist => write!(f, "allowlist"),
        }
    }
}

impl std::str::FromStr for DmPolicy {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_lowercase().as_str() {
            "open" => Ok(DmPolicy::Open),
            "pairing" => Ok(DmPolicy::Pairing),
            "allowlist" => Ok(DmPolicy::Allowlist),
            _ => Err(format!("Invalid DM policy: {}. Expected: open, pairing, allowlist", s)),
        }
    }
}

// ── Stored entries
// ────────────────────────────────────────────────────────────

/// A pending pairing request (user-initiated).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PendingRequest {
    /// Unique pairing code (e.g., "ABC123").
    pub code: String,
    /// Channel type (e.g., "telegram", "discord").
    pub channel: String,
    /// User ID on that channel.
    pub user_id: String,
    /// Optional username/handle for display.
    pub username: Option<String>,
    /// When the request was created.
    pub created_at: SystemTime,
    /// When the request expires.
    pub expires_at: SystemTime,
    /// Number of approval attempts (for rate limiting).
    pub attempt_count: u32,
}

impl PendingRequest {
    /// Return `true` if the request has not yet expired.
    pub fn is_valid(&self) -> bool {
        SystemTime::now() < self.expires_at
    }

    /// Check if max attempts exceeded.
    pub fn is_locked(&self, max_attempts: u32) -> bool {
        self.attempt_count >= max_attempts
    }
}

/// A successfully authorized user entry.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuthorizedUser {
    /// Channel the user is authorized on.
    pub channel: String,
    /// User ID.
    pub user_id: String,
    /// Optional username/handle.
    pub username: Option<String>,
    /// When authorization was granted.
    pub authorized_at: SystemTime,
    /// When authorization expires (`None` = never).
    pub expires_at: Option<SystemTime>,
    /// Who approved the pairing (admin user ID).
    pub approved_by: Option<String>,
    /// Pairing code that was used (for audit).
    pub code_used: Option<String>,
}

impl AuthorizedUser {
    /// Return `true` if the authorization has not yet expired.
    pub fn is_valid(&self) -> bool {
        self.expires_at
            .map(|exp| SystemTime::now() < exp)
            .unwrap_or(true)
    }
}

/// Result of requesting access.
#[derive(Debug, Clone)]
pub enum RequestAccessResult {
    /// New pairing request created.
    NewRequest { code: String },
    /// Already has a pending request.
    AlreadyPending {
        code: String,
        created_at: SystemTime,
    },
    /// Already authorized.
    AlreadyAuthorized,
    /// Rate limited (too many requests).
    RateLimited { retry_after: Duration },
}

// ── Store ─────────────────────────────────────────────────────────────────────

/// Thread-safe store for pairing requests and authorized users.
#[derive(Debug, Clone)]
pub struct PairingStore {
    /// Pending requests keyed by code for O(1) lookup.
    pending: Arc<RwLock<HashMap<String, PendingRequest>>>,
    /// Reverse index: (channel, user_id) -> code for checking existing
    /// requests.
    pending_index: Arc<RwLock<HashMap<(String, String), String>>>,
    /// Authorized users keyed by (channel, user_id).
    authorized: Arc<RwLock<HashMap<(String, String), AuthorizedUser>>>,
    /// Multi-dimensional allowlist for fine-grained access control.
    allowlist: Arc<crate::security::allowlist::Allowlist>,
    /// Default TTL for pairing requests.
    default_ttl: Duration,
    /// Max pending requests per user per channel (rate limiting).
    max_requests_per_user: usize,
    /// Min time between requests from same user.
    min_request_interval: Duration,
}

impl Default for PairingStore {
    fn default() -> Self {
        Self {
            pending: Arc::new(RwLock::new(HashMap::new())),
            pending_index: Arc::new(RwLock::new(HashMap::new())),
            authorized: Arc::new(RwLock::new(HashMap::new())),
            allowlist: Arc::new(crate::security::allowlist::Allowlist::new()),
            default_ttl: Duration::from_secs(3600), // 1 hour
            max_requests_per_user: 3,
            min_request_interval: Duration::from_secs(600), // 10 minutes
        }
    }
}

impl PairingStore {
    /// Create a new empty store.
    pub fn new() -> Self {
        Self::default()
    }

    /// Create with custom TTL and rate limits.
    pub fn with_config(
        default_ttl: Duration,
        max_requests_per_user: usize,
        min_request_interval: Duration,
    ) -> Self {
        Self {
            pending: Arc::new(RwLock::new(HashMap::new())),
            pending_index: Arc::new(RwLock::new(HashMap::new())),
            authorized: Arc::new(RwLock::new(HashMap::new())),
            allowlist: Arc::new(crate::security::allowlist::Allowlist::new()),
            default_ttl,
            max_requests_per_user,
            min_request_interval,
        }
    }

    /// Get a reference to the multi-dimensional allowlist.
    pub fn allowlist(&self) -> &Arc<crate::security::allowlist::Allowlist> {
        &self.allowlist
    }

    // ── User-initiated pairing ───────────────────────────────

    /// Request access to the bot (called when unknown user sends first DM).
    ///
    /// Returns the pairing code for the user to share with admin.
    pub async fn request_access(
        &self,
        channel: &str,
        user_id: &str,
        username: Option<&str>,
    ) -> Result<RequestAccessResult, PairingError> {
        let key = (channel.to_string(), user_id.to_string());

        // Check if already authorized
        if self.is_authorized(channel, user_id).await {
            return Ok(RequestAccessResult::AlreadyAuthorized);
        }

        // Check if already has pending request
        {
            let index = self.pending_index.read().await;
            if let Some(code) = index.get(&key) {
                let pending = self.pending.read().await;
                if let Some(req) = pending.get(code) {
                    if req.is_valid() {
                        return Ok(RequestAccessResult::AlreadyPending {
                            code: code.clone(),
                            created_at: req.created_at,
                        });
                    }
                }
            }
        }

        // Check rate limits
        self.check_rate_limits(channel, user_id).await?;

        // Generate new request
        let code = generate_pairing_code();
        let now = SystemTime::now();
        let expires_at = now + self.default_ttl;

        let request = PendingRequest {
            code: code.clone(),
            channel: channel.to_string(),
            user_id: user_id.to_string(),
            username: username.map(|s| s.to_string()),
            created_at: now,
            expires_at,
            attempt_count: 0,
        };

        {
            let mut pending = self.pending.write().await;
            let mut index = self.pending_index.write().await;
            pending.insert(code.clone(), request);
            index.insert(key, code.clone());
        }

        info!(
            channel = %channel,
            user_id = %user_id,
            username = ?username,
            code = %code,
            "Pairing request created (expires in {:?})",
            self.default_ttl
        );

        Ok(RequestAccessResult::NewRequest { code })
    }

    /// Admin approves a pending request by code.
    ///
    /// Returns the authorized user info if found and approved.
    pub async fn approve(
        &self,
        channel: &str,
        code: &str,
        approved_by: Option<&str>,
    ) -> Option<AuthorizedUser> {
        let mut pending = self.pending.write().await;
        let mut index = self.pending_index.write().await;
        let mut authorized = self.authorized.write().await;

        let request = pending.get_mut(code)?;

        // Verify channel matches
        if request.channel != channel {
            warn!(
                code = %code,
                expected_channel = %channel,
                actual_channel = %request.channel,
                "Channel mismatch for pairing code"
            );
            return None;
        }

        // Check if expired
        if !request.is_valid() {
            warn!(code = %code, "Pairing code expired");
            // Clean up expired request
            let key = (request.channel.clone(), request.user_id.clone());
            pending.remove(code);
            index.remove(&key);
            return None;
        }

        // Increment attempt count
        request.attempt_count += 1;

        // Create authorized user
        let now = SystemTime::now();
        let user = AuthorizedUser {
            channel: request.channel.clone(),
            user_id: request.user_id.clone(),
            username: request.username.clone(),
            authorized_at: now,
            expires_at: None, // permanent unless revoked
            approved_by: approved_by.map(|s| s.to_string()),
            code_used: Some(code.to_string()),
        };

        // Move from pending to authorized
        let key = (request.channel.clone(), request.user_id.clone());
        pending.remove(code);
        index.remove(&key);
        authorized.insert(key, user.clone());

        info!(
            channel = %user.channel,
            user_id = %user.user_id,
            code = %code,
            "User approved and authorized"
        );

        Some(user)
    }

    /// Reject/deny a pending request by code.
    pub async fn reject(&self, channel: &str, code: &str) -> Option<PendingRequest> {
        let mut pending = self.pending.write().await;
        let mut index = self.pending_index.write().await;

        // Check the request exists and matches channel before removing
        let request = pending.get(code).cloned()?;

        if request.channel != channel {
            return None;
        }

        let key = (request.channel.clone(), request.user_id.clone());
        pending.remove(code);
        index.remove(&key);

        info!(
            channel = %channel,
            code = %code,
            user_id = %request.user_id,
            "Pairing request rejected"
        );

        Some(request)
    }

    // ── Access checks ─────────────────────────────────────────────────────────

    /// Return `true` if `user_id` is currently authorized on `channel`.
    /// Checks both the direct authorized list and the multi-dimensional
    /// allowlist.
    pub async fn is_authorized(&self, channel: &str, user_id: &str) -> bool {
        // First check direct authorized list (pairing flow)
        let key = (channel.to_string(), user_id.to_string());
        {
            let auth = self.authorized.read().await;
            if auth.get(&key).map(|u| u.is_valid()).unwrap_or(false) {
                return true;
            }
        }

        // Then check multi-dimensional allowlist
        crate::security::allowlist::is_user_allowed(&self.allowlist, channel, user_id, None).await
    }

    /// Check if user has a pending request.
    pub async fn has_pending(&self, channel: &str, user_id: &str) -> Option<PendingRequest> {
        let key = (channel.to_string(), user_id.to_string());
        let index = self.pending_index.read().await;
        let code = index.get(&key)?;
        let pending = self.pending.read().await;
        pending.get(code).cloned()
    }

    /// Get pending request by code.
    pub async fn get_pending_by_code(&self, code: &str) -> Option<PendingRequest> {
        let pending = self.pending.read().await;
        pending.get(code).cloned()
    }

    // ── Listing ───────────────────────────────────────────────────────────────

    /// List all pending requests for a channel.
    pub async fn list_pending(&self, channel: &str) -> Vec<PendingRequest> {
        let pending = self.pending.read().await;
        pending
            .values()
            .filter(|r| r.channel == channel && r.is_valid())
            .cloned()
            .collect()
    }

    /// List all currently valid authorizations.
    pub async fn list_authorized(&self) -> Vec<AuthorizedUser> {
        let auth = self.authorized.read().await;
        auth.values().filter(|u| u.is_valid()).cloned().collect()
    }

    /// List authorized users filtered by channel.
    pub async fn list_authorized_for_channel(&self, channel: &str) -> Vec<AuthorizedUser> {
        let auth = self.authorized.read().await;
        auth.values()
            .filter(|u| u.channel == channel && u.is_valid())
            .cloned()
            .collect()
    }

    // ── Revocation ────────────────────────────────────────────────────────────

    /// Revoke access for `user_id` on `channel`.
    ///
    /// Returns `true` if an entry was removed.
    pub async fn revoke(&self, channel: &str, user_id: &str) -> bool {
        let key = (channel.to_string(), user_id.to_string());
        let mut auth = self.authorized.write().await;
        let removed = auth.remove(&key).is_some();
        if removed {
            info!(channel, user_id, "User access revoked");
        }
        removed
    }

    // ── Allowlist (static) support ────────────────────────────────────────────

    /// Add a user directly to authorized list (for allowlist policy).
    pub async fn add_to_allowlist(
        &self,
        channel: &str,
        user_id: &str,
        username: Option<&str>,
        added_by: Option<&str>,
    ) -> AuthorizedUser {
        let key = (channel.to_string(), user_id.to_string());
        let user = AuthorizedUser {
            channel: channel.to_string(),
            user_id: user_id.to_string(),
            username: username.map(|s| s.to_string()),
            authorized_at: SystemTime::now(),
            expires_at: None,
            approved_by: added_by.map(|s| s.to_string()),
            code_used: None,
        };

        let mut auth = self.authorized.write().await;
        auth.insert(key, user.clone());

        info!(
            channel = %channel,
            user_id = %user_id,
            "User added to allowlist"
        );

        user
    }

    // ── Maintenance ───────────────────────────────────────────────────────────

    /// Remove expired pending codes and expired authorizations.
    pub async fn sweep_expired(&self) {
        let mut pending = self.pending.write().await;
        let mut index = self.pending_index.write().await;
        let before = pending.len();

        // Remove expired entries from both maps
        let expired_codes: Vec<String> = pending
            .values()
            .filter(|r| !r.is_valid())
            .map(|r| r.code.clone())
            .collect();

        for code in &expired_codes {
            if let Some(req) = pending.remove(code) {
                let key = (req.channel.clone(), req.user_id.clone());
                index.remove(&key);
            }
        }

        let removed = before - pending.len();
        drop(pending);
        drop(index);

        let mut auth = self.authorized.write().await;
        auth.retain(|_, u| u.is_valid());

        debug!("Pairing store sweep: removed {} expired pending requests", removed);
    }

    // ── Internal helpers ──────────────────────────────────────────────────────

    async fn check_rate_limits(&self, channel: &str, user_id: &str) -> Result<(), PairingError> {
        // Count existing pending requests for this user
        let pending = self.pending.read().await;
        let user_request_count = pending
            .values()
            .filter(|r| r.channel == channel && r.user_id == user_id && r.is_valid())
            .count();

        if user_request_count >= self.max_requests_per_user {
            return Err(PairingError::RateLimited {
                message: format!("Maximum {} pending requests reached", self.max_requests_per_user),
                retry_after: self.min_request_interval,
            });
        }

        Ok(())
    }
}

// ── Error type
// ────────────────────────────────────────────────────────────────

/// Errors that can occur during pairing operations.
#[derive(Debug, Clone)]
pub enum PairingError {
    /// Rate limit exceeded.
    RateLimited {
        message: String,
        retry_after: Duration,
    },
    /// Internal error.
    Internal(String),
}

impl std::fmt::Display for PairingError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PairingError::RateLimited { message, .. } => write!(f, "Rate limited: {}", message),
            PairingError::Internal(msg) => write!(f, "Internal error: {}", msg),
        }
    }
}

impl std::error::Error for PairingError {}

// ── Code generation
// ───────────────────────────────────────────────────────────

/// Generate a 6-character alphanumeric pairing code (unambiguous).
/// Uses characters that are easy to distinguish: no 0/O, 1/I/l.
fn generate_pairing_code() -> String {
    const ALPHABET: &[u8] = b"ABCDEFGHJKLMNPQRSTUVWXYZ23456789";
    const LEN: usize = 6;

    let mut code = String::with_capacity(LEN);
    for _ in 0..LEN {
        let idx = rand::random_range(0..ALPHABET.len());
        code.push(ALPHABET[idx] as char);
    }
    code
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_request_access_and_approve() {
        let store = PairingStore::new();

        // User requests access
        let result = store
            .request_access("telegram", "123456", Some("@alice"))
            .await;
        assert!(matches!(result, Ok(RequestAccessResult::NewRequest { .. })));

        let code = match result.unwrap() {
            RequestAccessResult::NewRequest { code } => code,
            _ => panic!("Expected NewRequest"),
        };

        // Not authorized yet
        assert!(!store.is_authorized("telegram", "123456").await);

        // Admin approves
        let approved = store.approve("telegram", &code, Some("admin")).await;
        assert!(approved.is_some());

        let user = approved.unwrap();
        assert_eq!(user.user_id, "123456");
        assert_eq!(user.username, Some("@alice".to_string()));
        assert_eq!(user.approved_by, Some("admin".to_string()));

        // Now authorized
        assert!(store.is_authorized("telegram", "123456").await);
    }

    #[tokio::test]
    async fn test_already_authorized() {
        let store = PairingStore::new();

        // First request
        let result = store.request_access("telegram", "123456", None).await;
        let code = match result.unwrap() {
            RequestAccessResult::NewRequest { code } => code,
            _ => panic!("Expected NewRequest"),
        };
        store.approve("telegram", &code, None).await;

        // Second request from same user
        let result = store.request_access("telegram", "123456", None).await;
        assert!(matches!(result, Ok(RequestAccessResult::AlreadyAuthorized)));
    }

    #[tokio::test]
    async fn test_already_pending() {
        let store = PairingStore::new();

        // First request
        let result = store.request_access("telegram", "123456", None).await;
        let code = match result.unwrap() {
            RequestAccessResult::NewRequest { code } => code,
            _ => panic!("Expected NewRequest"),
        };

        // Second request from same user (before approval)
        let result = store.request_access("telegram", "123456", None).await;
        assert!(matches!(
            result,
            Ok(RequestAccessResult::AlreadyPending { code: ref c, .. }) if c == &code
        ));
    }

    #[tokio::test]
    async fn test_list_pending() {
        let store = PairingStore::new();

        store
            .request_access("telegram", "111", Some("@user1"))
            .await
            .unwrap();
        store
            .request_access("telegram", "222", Some("@user2"))
            .await
            .unwrap();
        store.request_access("discord", "333", None).await.unwrap();

        let pending_telegram = store.list_pending("telegram").await;
        assert_eq!(pending_telegram.len(), 2);

        let pending_discord = store.list_pending("discord").await;
        assert_eq!(pending_discord.len(), 1);
    }

    #[tokio::test]
    async fn test_reject_pending() {
        let store = PairingStore::new();

        let result = store.request_access("telegram", "123456", None).await;
        let code = match result.unwrap() {
            RequestAccessResult::NewRequest { code } => code,
            _ => panic!("Expected NewRequest"),
        };

        let rejected = store.reject("telegram", &code).await;
        assert!(rejected.is_some());

        // Not authorized after rejection
        assert!(!store.is_authorized("telegram", "123456").await);

        // Pending list empty
        let pending = store.list_pending("telegram").await;
        assert!(pending.is_empty());
    }

    #[tokio::test]
    async fn test_allowlist() {
        let store = PairingStore::new();

        // Add directly to allowlist (no pairing flow)
        let user = store
            .add_to_allowlist("telegram", "123456", Some("@alice"), Some("admin"))
            .await;

        assert_eq!(user.user_id, "123456");
        assert!(store.is_authorized("telegram", "123456").await);
    }

    #[tokio::test]
    async fn test_revoke() {
        let store = PairingStore::new();

        let _user = store
            .add_to_allowlist("telegram", "123456", None, None)
            .await;
        assert!(store.is_authorized("telegram", "123456").await);

        store.revoke("telegram", "123456").await;
        assert!(!store.is_authorized("telegram", "123456").await);
    }

    #[tokio::test]
    async fn test_dm_policy_from_str() {
        assert_eq!("open".parse::<DmPolicy>().unwrap(), DmPolicy::Open);
        assert_eq!("pairing".parse::<DmPolicy>().unwrap(), DmPolicy::Pairing);
        assert_eq!("allowlist".parse::<DmPolicy>().unwrap(), DmPolicy::Allowlist);
        assert!("invalid".parse::<DmPolicy>().is_err());
    }

    #[tokio::test]
    async fn test_dm_policy_display() {
        assert_eq!(DmPolicy::Open.to_string(), "open");
        assert_eq!(DmPolicy::Pairing.to_string(), "pairing");
        assert_eq!(DmPolicy::Allowlist.to_string(), "allowlist");
    }

    #[tokio::test]
    async fn test_code_format() {
        let store = PairingStore::new();

        let result = store.request_access("telegram", "123", None).await;
        let code = match result.unwrap() {
            RequestAccessResult::NewRequest { code } => code,
            _ => panic!("Expected NewRequest"),
        };

        // 6 characters
        assert_eq!(code.len(), 6);

        // No ambiguous characters
        assert!(!code.contains('0'));
        assert!(!code.contains('O'));
        assert!(!code.contains('1'));
        assert!(!code.contains('I'));
    }
}
