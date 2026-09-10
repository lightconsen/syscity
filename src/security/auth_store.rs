//! SQLite persistence for authenticated sessions and device pairings.
//!
//! Backs [`super::AuthManager`] and
//! [`super::device_pairing::DevicePairingStore`] with the shared SQLite pool
//! so login state and device pairings survive a process restart. Only
//! *hashed* token material is written to disk: the plaintext bearer token is
//! never persisted.
//!
//! Schema creation follows the ad-hoc `CREATE TABLE IF NOT EXISTS` + tolerant
//! migration style used by `crate::agent::session_store::schema` (there is no
//! separate migration framework in this repo).
//!
//! The digest is a domain-separated SHA-256 rendered as lowercase hex, and
//! digest comparison goes through [`subtle::ConstantTimeEq`] — the same
//! constant-time primitive used for the shared token in
//! `crate::gateway::middleware::shared_token_matches`.

use std::time::{Duration, UNIX_EPOCH};

use sha2::{Digest, Sha256};
use sqlx::{Pool, Sqlite};
use subtle::ConstantTimeEq;
use tracing::{debug, warn};

use crate::error::{Result, SyscityError};

use super::device_pairing::AuthorizedDevice;
use super::{Session, UserId};

/// Domain-separation prefix folded into every session-token digest.
const SESSION_HASH_PREFIX: &str = "syscity/session-token/v1:";
/// Domain-separation prefix folded into every device-token digest.
const DEVICE_HASH_PREFIX: &str = "syscity/device-token/v1:";

/// SHA-256 digest (lowercase hex) of a session bearer token.
///
/// The domain prefix keeps a session digest from ever colliding with a device
/// digest produced from the same plaintext.
pub fn hash_session_token(token: &str) -> String {
    hash_token(SESSION_HASH_PREFIX, token)
}

/// SHA-256 digest (lowercase hex) of a device bearer token.
pub fn hash_device_token(token: &str) -> String {
    hash_token(DEVICE_HASH_PREFIX, token)
}

fn hash_token(prefix: &str, token: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(prefix.as_bytes());
    hasher.update(token.as_bytes());
    hex::encode(hasher.finalize())
}

/// Constant-time comparison of two token digests.
pub fn token_hash_eq(a: &str, b: &str) -> bool {
    a.as_bytes().ct_eq(b.as_bytes()).into()
}

/// Row-shaped session record persisted in `auth_sessions`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredSession {
    /// Owning user ID.
    pub user_id: String,
    /// Creation time (Unix milliseconds, UTC).
    pub created_at_ms: i64,
    /// Expiry time (Unix milliseconds, UTC).
    pub expires_at_ms: i64,
    /// Optional device fingerprint.
    pub device_fingerprint: Option<String>,
    /// Granted scopes.
    pub scopes: Vec<String>,
}

impl StoredSession {
    /// Project a live [`Session`] into its persisted form.
    pub fn from_session(session: &Session) -> Self {
        Self {
            user_id: session.user_id.0.clone(),
            created_at_ms: session.created_at.timestamp_millis(),
            expires_at_ms: session.expires_at.timestamp_millis(),
            device_fingerprint: session.device_fingerprint.clone(),
            scopes: session.scopes.clone(),
        }
    }

    /// Rebuild a [`Session`] from a persisted row.
    ///
    /// The plaintext token is unrecoverable by design, so the returned
    /// session's `token` field carries the stored digest — used only as an
    /// opaque identity, never presented as a credential.
    pub fn into_session(self, token_hash: &str) -> Option<Session> {
        let created_at = chrono::DateTime::from_timestamp_millis(self.created_at_ms)?;
        let expires_at = chrono::DateTime::from_timestamp_millis(self.expires_at_ms)?;
        Some(Session {
            token: token_hash.to_string(),
            user_id: UserId::new(self.user_id),
            created_at,
            expires_at,
            device_fingerprint: self.device_fingerprint,
            scopes: self.scopes,
        })
    }
}

/// Row-shaped authorized-device record persisted in `auth_devices`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredDevice {
    /// Device unique ID.
    pub device_id: String,
    /// Optional display name.
    pub display_name: Option<String>,
    /// Optional Ed25519 public key (base64).
    pub public_key: Option<String>,
    /// Digest of the device token (never the plaintext).
    pub token_hash: String,
    /// Authorization time (Unix milliseconds).
    pub authorized_at_ms: i64,
    /// Who approved the pairing.
    pub approved_by: Option<String>,
}

impl StoredDevice {
    /// Project a live [`AuthorizedDevice`] into its persisted form.
    pub fn from_authorized(device: &AuthorizedDevice) -> Self {
        Self {
            device_id: device.device_id.clone(),
            display_name: device.display_name.clone(),
            public_key: device.public_key.clone(),
            token_hash: device.token_hash.clone(),
            authorized_at_ms: device
                .authorized_at
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_millis() as i64)
                .unwrap_or(0),
            approved_by: device.approved_by.clone(),
        }
    }

    /// Rebuild an [`AuthorizedDevice`] from a persisted row.
    ///
    /// The plaintext token is unrecoverable by design, so `token` is left
    /// empty; authentication goes through `token_hash`.
    pub fn into_authorized(self) -> AuthorizedDevice {
        let authorized_at = UNIX_EPOCH + Duration::from_millis(self.authorized_at_ms.max(0) as u64);
        AuthorizedDevice {
            device_id: self.device_id,
            display_name: self.display_name,
            public_key: self.public_key,
            token: String::new(),
            token_hash: self.token_hash,
            authorized_at,
            approved_by: self.approved_by,
        }
    }
}

/// SQLite-backed store for auth sessions and authorized devices.
#[derive(Debug, Clone)]
pub struct AuthStore {
    pool: Pool<Sqlite>,
}

impl AuthStore {
    /// Wrap an existing pool. Call [`AuthStore::init_schema`] before use.
    pub fn new(pool: Pool<Sqlite>) -> Self {
        Self { pool }
    }

    /// Create the auth tables and indexes if they do not already exist.
    ///
    /// Idempotent: safe to call from both the auth manager and the device
    /// pairing store on the same shared pool.
    pub async fn init_schema(&self) -> Result<()> {
        sqlx::query(
            r#"
            CREATE TABLE IF NOT EXISTS auth_sessions (
                token_hash         TEXT PRIMARY KEY,
                user_id            TEXT    NOT NULL,
                created_at         INTEGER NOT NULL,
                expires_at         INTEGER NOT NULL,
                device_fingerprint TEXT,
                scopes_json        TEXT    NOT NULL DEFAULT '[]'
            )
            "#,
        )
        .execute(&self.pool)
        .await
        .map_err(|e| storage_err("create auth_sessions table", e))?;

        sqlx::query(
            r#"
            CREATE TABLE IF NOT EXISTS auth_devices (
                device_id     TEXT PRIMARY KEY,
                display_name  TEXT,
                public_key    TEXT,
                token_hash    TEXT    NOT NULL,
                authorized_at INTEGER NOT NULL,
                approved_by   TEXT
            )
            "#,
        )
        .execute(&self.pool)
        .await
        .map_err(|e| storage_err("create auth_devices table", e))?;

        for (index, table, cols) in [
            ("idx_auth_sessions_expires", "auth_sessions", "expires_at"),
            ("idx_auth_devices_token", "auth_devices", "token_hash"),
        ] {
            if let Err(e) =
                sqlx::query(&format!("CREATE INDEX IF NOT EXISTS {} ON {}({})", index, table, cols))
                    .execute(&self.pool)
                    .await
            {
                warn!("Failed to create auth store index {}: {}", index, e);
            }
        }

        debug!("Auth persistence schema initialized");
        Ok(())
    }

    // ── Sessions ─────────────────────────────────────────────────────────────

    /// Insert or replace a session row keyed by its token digest.
    pub async fn upsert_session(&self, token_hash: &str, session: &StoredSession) -> Result<()> {
        let scopes_json =
            serde_json::to_string(&session.scopes).unwrap_or_else(|_| "[]".to_string());
        sqlx::query(
            "INSERT INTO auth_sessions \
             (token_hash, user_id, created_at, expires_at, device_fingerprint, scopes_json) \
             VALUES (?, ?, ?, ?, ?, ?) \
             ON CONFLICT(token_hash) DO UPDATE SET \
             user_id = excluded.user_id, created_at = excluded.created_at, \
             expires_at = excluded.expires_at, \
             device_fingerprint = excluded.device_fingerprint, \
             scopes_json = excluded.scopes_json",
        )
        .bind(token_hash)
        .bind(&session.user_id)
        .bind(session.created_at_ms)
        .bind(session.expires_at_ms)
        .bind(session.device_fingerprint.as_deref())
        .bind(scopes_json)
        .execute(&self.pool)
        .await
        .map_err(|e| storage_err("upsert auth session", e))?;
        Ok(())
    }

    /// Load every persisted session as `(token_hash, record)`.
    pub async fn load_sessions(&self) -> Result<Vec<(String, StoredSession)>> {
        let rows = sqlx::query_as::<_, (String, String, i64, i64, Option<String>, String)>(
            "SELECT token_hash, user_id, created_at, expires_at, device_fingerprint, scopes_json \
             FROM auth_sessions",
        )
        .fetch_all(&self.pool)
        .await
        .map_err(|e| storage_err("load auth sessions", e))?;

        Ok(rows
            .into_iter()
            .map(
                |(token_hash, user_id, created_at, expires_at, device_fingerprint, scopes_json)| {
                    (
                        token_hash,
                        StoredSession {
                            user_id,
                            created_at_ms: created_at,
                            expires_at_ms: expires_at,
                            device_fingerprint,
                            scopes: serde_json::from_str(&scopes_json).unwrap_or_default(),
                        },
                    )
                },
            )
            .collect())
    }

    /// Load a single session by token digest.
    pub async fn load_session(&self, token_hash: &str) -> Result<Option<StoredSession>> {
        let row = sqlx::query_as::<_, (String, i64, i64, Option<String>, String)>(
            "SELECT user_id, created_at, expires_at, device_fingerprint, scopes_json \
             FROM auth_sessions WHERE token_hash = ?",
        )
        .bind(token_hash)
        .fetch_optional(&self.pool)
        .await
        .map_err(|e| storage_err("load auth session", e))?;

        Ok(row.map(|(user_id, created_at, expires_at, device_fingerprint, scopes_json)| {
            StoredSession {
                user_id,
                created_at_ms: created_at,
                expires_at_ms: expires_at,
                device_fingerprint,
                scopes: serde_json::from_str(&scopes_json).unwrap_or_default(),
            }
        }))
    }

    /// Delete a session row by token digest.
    pub async fn delete_session(&self, token_hash: &str) -> Result<()> {
        sqlx::query("DELETE FROM auth_sessions WHERE token_hash = ?")
            .bind(token_hash)
            .execute(&self.pool)
            .await
            .map_err(|e| storage_err("delete auth session", e))?;
        Ok(())
    }

    /// Delete sessions whose expiry is at or before `now_ms`. Returns the
    /// number of rows removed.
    pub async fn delete_expired_sessions(&self, now_ms: i64) -> Result<u64> {
        let result = sqlx::query("DELETE FROM auth_sessions WHERE expires_at <= ?")
            .bind(now_ms)
            .execute(&self.pool)
            .await
            .map_err(|e| storage_err("prune expired auth sessions", e))?;
        Ok(result.rows_affected())
    }

    // ── Authorized devices ───────────────────────────────────────────────────

    /// Insert or replace an authorized-device row.
    pub async fn upsert_device(&self, device: &StoredDevice) -> Result<()> {
        sqlx::query(
            "INSERT INTO auth_devices \
             (device_id, display_name, public_key, token_hash, authorized_at, approved_by) \
             VALUES (?, ?, ?, ?, ?, ?) \
             ON CONFLICT(device_id) DO UPDATE SET \
             display_name = excluded.display_name, public_key = excluded.public_key, \
             token_hash = excluded.token_hash, authorized_at = excluded.authorized_at, \
             approved_by = excluded.approved_by",
        )
        .bind(&device.device_id)
        .bind(device.display_name.as_deref())
        .bind(device.public_key.as_deref())
        .bind(&device.token_hash)
        .bind(device.authorized_at_ms)
        .bind(device.approved_by.as_deref())
        .execute(&self.pool)
        .await
        .map_err(|e| storage_err("upsert authorized device", e))?;
        Ok(())
    }

    /// Load every persisted authorized device.
    pub async fn load_devices(&self) -> Result<Vec<StoredDevice>> {
        let rows = sqlx::query_as::<
            _,
            (String, Option<String>, Option<String>, String, i64, Option<String>),
        >(
            "SELECT device_id, display_name, public_key, token_hash, authorized_at, approved_by \
             FROM auth_devices",
        )
        .fetch_all(&self.pool)
        .await
        .map_err(|e| storage_err("load authorized devices", e))?;

        Ok(rows
            .into_iter()
            .map(
                |(
                    device_id,
                    display_name,
                    public_key,
                    token_hash,
                    authorized_at_ms,
                    approved_by,
                )| StoredDevice {
                    device_id,
                    display_name,
                    public_key,
                    token_hash,
                    authorized_at_ms,
                    approved_by,
                },
            )
            .collect())
    }

    /// Delete an authorized device row.
    pub async fn delete_device(&self, device_id: &str) -> Result<()> {
        sqlx::query("DELETE FROM auth_devices WHERE device_id = ?")
            .bind(device_id)
            .execute(&self.pool)
            .await
            .map_err(|e| storage_err("delete authorized device", e))?;
        Ok(())
    }
}

fn storage_err(context: &str, e: sqlx::Error) -> SyscityError {
    SyscityError::Storage {
        context: format!("Failed to {}", context),
        details: e.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hashes_are_deterministic_and_domain_separated() {
        let session = hash_session_token("abc");
        assert_eq!(session, hash_session_token("abc"));
        // Session and device digests for the same plaintext must differ.
        assert_ne!(session, hash_device_token("abc"));
        // Lowercase hex SHA-256.
        assert_eq!(session.len(), 64);
        assert!(session.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn token_hash_eq_matches_only_equal_digests() {
        let a = hash_session_token("token-a");
        let b = hash_session_token("token-b");
        assert!(token_hash_eq(&a, &a));
        assert!(!token_hash_eq(&a, &b));
    }

    #[test]
    fn session_round_trip_preserves_fields_but_not_plaintext() {
        let session = Session {
            token: "plaintext-token".to_string(),
            user_id: UserId::new("user-1"),
            created_at: chrono::Utc::now(),
            expires_at: chrono::Utc::now() + chrono::Duration::hours(1),
            device_fingerprint: Some("fp".to_string()),
            scopes: vec!["chat".to_string()],
        };
        let stored = StoredSession::from_session(&session);
        let restored = stored.into_session("digest").expect("valid timestamps");
        assert_eq!(restored.user_id, session.user_id);
        assert_eq!(restored.scopes, session.scopes);
        assert_eq!(restored.device_fingerprint, session.device_fingerprint);
        // The plaintext token is not carried into the persisted form.
        assert_eq!(restored.token, "digest");
    }

    #[test]
    fn device_round_trip_clears_plaintext_token() {
        let device = AuthorizedDevice {
            device_id: "dev-1".to_string(),
            display_name: Some("Laptop".to_string()),
            public_key: None,
            token: "dt_plaintext".to_string(),
            token_hash: hash_device_token("dt_plaintext"),
            authorized_at: UNIX_EPOCH + Duration::from_secs(1_700_000_000),
            approved_by: Some("admin".to_string()),
        };
        let stored = StoredDevice::from_authorized(&device);
        assert_eq!(stored.token_hash, device.token_hash);
        assert!(!stored.token_hash.contains("dt_plaintext"));
        let restored = stored.into_authorized();
        assert_eq!(restored.device_id, "dev-1");
        assert!(restored.token.is_empty());
        assert_eq!(restored.token_hash, device.token_hash);
    }
}
