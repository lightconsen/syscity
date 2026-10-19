//! Persistent storage for auth profile key state metadata
//!
//! Stores **only** key state (failure counts, cooldown timestamps, status) —
//! raw API keys remain in config and are never written to the database.
//!
//! ```rust,ignore
//! let store = AuthProfileStore::new(pool);
//! store.save_profile_state("openai", &profile).await?;
//! store.load_profile_state("openai", &mut profile).await?;
//! ```

use chrono::{DateTime, Utc};
use sqlx::{Pool, Row, Sqlite};
use tracing::{debug, warn};

use crate::model_router::auth_profile::{AuthProfile, KeyStatus};

/// SQLite-backed persistence layer for auth profile key metadata.
#[derive(Debug, Clone)]
pub struct AuthProfileStore {
    pool: Pool<Sqlite>,
}

impl AuthProfileStore {
    /// Create a new store from an existing SQLite connection pool.
    pub fn new(pool: Pool<Sqlite>) -> Self {
        Self { pool }
    }

    /// Ensure the `auth_profile_states` table exists (idempotent).
    async fn ensure_table(&self) -> crate::Result<()> {
        sqlx::query(
            r#"
            CREATE TABLE IF NOT EXISTS auth_profile_states (
                provider_name TEXT NOT NULL,
                key_label     TEXT NOT NULL,
                failure_count INTEGER NOT NULL DEFAULT 0,
                success_count INTEGER NOT NULL DEFAULT 0,
                status        TEXT NOT NULL DEFAULT 'active',
                cooldown_until TEXT,
                last_failure  TEXT,
                updated_at    TEXT NOT NULL,
                PRIMARY KEY (provider_name, key_label)
            )
            "#,
        )
        .execute(&self.pool)
        .await
        .map_err(|e| crate::error::SyscityError::Storage {
            context: "Failed to create auth_profile_states table".to_string(),
            details: e.to_string(),
        })?;

        sqlx::query(
            "CREATE INDEX IF NOT EXISTS idx_auth_profile_provider ON \
             auth_profile_states(provider_name)",
        )
        .execute(&self.pool)
        .await
        .map_err(|e| crate::error::SyscityError::Storage {
            context: "Failed to create auth_profile_states index".to_string(),
            details: e.to_string(),
        })?;

        Ok(())
    }

    /// Persist the current state of all keys in an auth profile.
    ///
    /// Overwrites any existing rows for this provider's keys.
    pub async fn save_profile_state(
        &self,
        provider: &str,
        profile: &AuthProfile,
    ) -> crate::Result<()> {
        self.ensure_table().await?;

        let now = Utc::now().to_rfc3339();
        let statuses = profile.key_statuses();

        for key in &statuses {
            let status_str = match key.status {
                KeyStatus::Active => "active",
                KeyStatus::Cooldown => "cooldown",
                KeyStatus::Disabled => "disabled",
            };

            let cooldown_str = key.cooldown_until.map(|d| d.to_rfc3339());
            let last_failure_str = key.last_failure.map(|d| d.to_rfc3339());

            sqlx::query(
                r#"
                INSERT INTO auth_profile_states
                    (provider_name, key_label, failure_count, success_count, status,
                     cooldown_until, last_failure, updated_at)
                VALUES (?, ?, ?, ?, ?, ?, ?, ?)
                ON CONFLICT(provider_name, key_label) DO UPDATE SET
                    failure_count = excluded.failure_count,
                    success_count = excluded.success_count,
                    status        = excluded.status,
                    cooldown_until= excluded.cooldown_until,
                    last_failure  = excluded.last_failure,
                    updated_at    = excluded.updated_at
                "#,
            )
            .bind(provider)
            .bind(&key.label)
            .bind(key.failure_count as i64)
            .bind(key.success_count as i64)
            .bind(status_str)
            .bind(cooldown_str)
            .bind(last_failure_str)
            .bind(&now)
            .execute(&self.pool)
            .await
            .map_err(|e| crate::error::SyscityError::Storage {
                context: format!(
                    "Failed to save auth profile state for {}:{}",
                    provider, key.label
                ),
                details: e.to_string(),
            })?;
        }

        // Clean up stale rows for keys that no longer exist in the profile
        let labels: Vec<&str> = statuses.iter().map(|k| k.label.as_str()).collect();
        if !labels.is_empty() {
            let placeholders: String = labels.iter().map(|_| "?").collect::<Vec<_>>().join(",");
            let sql = format!(
                "DELETE FROM auth_profile_states WHERE provider_name = ? AND key_label NOT IN ({})",
                placeholders
            );
            // The only interpolated text is a run of `?` placeholders; every
            // value is bound below. sqlx 0.9 makes that assertion explicit.
            let mut query = sqlx::query(sqlx::AssertSqlSafe(sql.as_str())).bind(provider);
            for label in &labels {
                query = query.bind(*label);
            }
            query.execute(&self.pool).await.map_err(|e| {
                warn!("Failed to clean up stale auth profile rows for {}: {}", provider, e);
                crate::error::SyscityError::Storage {
                    context: format!("Failed to clean up stale auth profile rows for {}", provider),
                    details: e.to_string(),
                }
            })?;
        }

        debug!("Saved auth profile state for '{}' ({} keys)", provider, statuses.len());
        Ok(())
    }

    /// Load previously persisted state into an auth profile.
    ///
    /// Matches rows by `key_label`.  Keys that have no saved state are left
    /// untouched (they start with defaults from config).
    pub async fn load_profile_state(
        &self,
        provider: &str,
        profile: &mut AuthProfile,
    ) -> crate::Result<()> {
        self.ensure_table().await?;

        let rows = sqlx::query(
            r#"
            SELECT key_label, failure_count, success_count, status,
                   cooldown_until, last_failure
            FROM auth_profile_states
            WHERE provider_name = ?
            "#,
        )
        .bind(provider)
        .fetch_all(&self.pool)
        .await
        .map_err(|e| crate::error::SyscityError::Storage {
            context: format!("Failed to load auth profile state for {}", provider),
            details: e.to_string(),
        })?;

        let row_count = rows.len();
        for row in rows {
            let label: String = row.try_get("key_label").unwrap_or_default();
            let failure_count: i64 = row.try_get("failure_count").unwrap_or(0);
            let success_count: i64 = row.try_get("success_count").unwrap_or(0);
            let status_str: String = row.try_get("status").unwrap_or_default();
            let cooldown_until_str: Option<String> = row.try_get("cooldown_until").ok();
            let last_failure_str: Option<String> = row.try_get("last_failure").ok();

            if let Some(entry) = profile.key_entry_mut(&label) {
                entry.failure_count = failure_count as u32;
                entry.success_count = success_count as u64;
                entry.status = parse_status(&status_str);
                entry.cooldown_until = cooldown_until_str
                    .and_then(|s| DateTime::parse_from_rfc3339(&s).ok())
                    .map(|d| d.with_timezone(&Utc));
                entry.last_failure = last_failure_str
                    .and_then(|s| DateTime::parse_from_rfc3339(&s).ok())
                    .map(|d| d.with_timezone(&Utc));
            } else {
                warn!(
                    "Auth profile state for {} contains unknown key label '{}' — ignoring",
                    provider, label
                );
            }
        }

        debug!("Loaded auth profile state for '{}' ({} rows)", provider, row_count);
        Ok(())
    }

    /// Delete all persisted state for a provider.
    pub async fn delete_profile_state(&self, provider: &str) -> crate::Result<()> {
        if let Err(e) = self.ensure_table().await {
            warn!("Failed to ensure auth_profile_states table before delete: {}", e);
        }

        sqlx::query("DELETE FROM auth_profile_states WHERE provider_name = ?")
            .bind(provider)
            .execute(&self.pool)
            .await
            .map_err(|e| crate::error::SyscityError::Storage {
                context: format!("Failed to delete auth profile state for {}", provider),
                details: e.to_string(),
            })?;

        Ok(())
    }
}

fn parse_status(s: &str) -> KeyStatus {
    match s {
        "disabled" => KeyStatus::Disabled,
        "cooldown" => KeyStatus::Cooldown,
        _ => KeyStatus::Active,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model_router::auth_profile::AuthProfile;

    async fn in_memory_store() -> AuthProfileStore {
        let pool = sqlx::SqlitePool::connect("sqlite::memory:")
            .await
            .expect("in-memory pool");
        AuthProfileStore::new(pool)
    }

    #[tokio::test]
    async fn test_save_and_load_profile_state() {
        let store = in_memory_store().await;

        let mut profile = AuthProfile::with_keys(
            "openai",
            vec![
                ("key1".to_string(), "primary"),
                ("key2".to_string(), "secondary"),
            ],
            60,
            3,
        );

        // Simulate a failure on primary key
        profile.rotate(30);
        assert_eq!(profile.current_key(), Some("key2"));

        // Save state
        store.save_profile_state("openai", &profile).await.unwrap();

        // Create a fresh profile from same config
        let mut restored = AuthProfile::with_keys(
            "openai",
            vec![
                ("key1".to_string(), "primary"),
                ("key2".to_string(), "secondary"),
            ],
            60,
            3,
        );

        // Load state
        store
            .load_profile_state("openai", &mut restored)
            .await
            .unwrap();

        // Verify primary key has failure state restored
        let statuses = restored.key_statuses();
        let primary = statuses.iter().find(|k| k.label == "primary").unwrap();
        assert_eq!(primary.failure_count, 1);
        assert_eq!(primary.status, KeyStatus::Cooldown);
        assert!(primary.cooldown_until.is_some());

        let secondary = statuses.iter().find(|k| k.label == "secondary").unwrap();
        assert_eq!(secondary.failure_count, 0);
        assert_eq!(secondary.status, KeyStatus::Active);
    }

    #[tokio::test]
    async fn test_delete_profile_state() {
        let store = in_memory_store().await;

        let mut profile =
            AuthProfile::with_keys("openai", vec![("key1".to_string(), "primary")], 60, 3);
        profile.rotate(30);

        store.save_profile_state("openai", &profile).await.unwrap();
        store.delete_profile_state("openai").await.unwrap();

        let mut restored =
            AuthProfile::with_keys("openai", vec![("key1".to_string(), "primary")], 60, 3);
        store
            .load_profile_state("openai", &mut restored)
            .await
            .unwrap();

        let statuses = restored.key_statuses();
        assert_eq!(statuses[0].failure_count, 0);
        assert_eq!(statuses[0].status, KeyStatus::Active);
    }
}
