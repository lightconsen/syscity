//! Cloud device binding (P2-9): a stable local device identity registered
//! with the cloud account.
//!
//! The device id is a UUID minted on first use and persisted under the config
//! dir (`cloud/device_id`) so it survives restarts and re-binds idempotently.
//! The `device_token` returned by the cloud on bind is stored in the secret
//! store (it is a credential); it is groundwork for future cloud sync.

#[cfg(test)]
use std::cell::RefCell;
use std::path::PathBuf;
use std::sync::Arc;

use uuid::Uuid;

use crate::cloud::client::CloudClient;
use crate::cloud::config::CloudConfig;
use crate::secrets::{SecretId, SecretOrigin, SecretStoreHandle};

/// Secret-store entity for the device token.
pub const ENTITY_DEVICE_TOKEN: &str = "device_token";

/// Test hook: redirect the device-id file so tests stay hermetic.
#[cfg(test)]
thread_local! {
    static DEVICE_ID_PATH_OVERRIDE: RefCell<Option<PathBuf>> = const { RefCell::new(None) };
}

fn device_id_path() -> PathBuf {
    #[cfg(test)]
    if let Some(path) = DEVICE_ID_PATH_OVERRIDE.with(|c| c.borrow().clone()) {
        return path;
    }
    crate::dirs::config_dir().join("cloud").join("device_id")
}

fn token_id() -> SecretId {
    SecretId::secret(crate::cloud::session::CLOUD_NS, ENTITY_DEVICE_TOKEN)
}

/// The stable local device id, minted on first use (persisted under the
/// config dir so it survives restarts).
pub fn local_device_id() -> String {
    if let Ok(id) = std::fs::read_to_string(device_id_path()) {
        let id = id.trim();
        if !id.is_empty() {
            return id.to_string();
        }
    }
    let id = Uuid::new_v4().to_string();
    if let Some(parent) = device_id_path().parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = std::fs::write(device_id_path(), &id);
    id
}

/// The stored device token, if this device was bound to the account.
pub async fn device_token(secrets: &SecretStoreHandle) -> Option<String> {
    secrets
        .choose(&token_id())
        .get(&token_id())
        .await
        .ok()
        .flatten()
}

/// Whether this device has been bound to the account.
pub async fn bound(secrets: &SecretStoreHandle) -> bool {
    device_token(secrets).await.is_some()
}

/// Register this device with the cloud account (idempotent). Requires a
/// logged-in session; on success the returned `device_token` is persisted.
/// Best-effort: callers log failures but do not fail the surrounding flow.
pub async fn bind(cfg: &CloudConfig, secrets: Arc<SecretStoreHandle>) -> crate::Result<()> {
    let token = crate::cloud::session::get_token(&secrets)
        .await
        .ok_or_else(|| {
            crate::error::SyscityError::Internal(
                "not signed in to Syscity Cloud — cannot bind device".to_string(),
            )
        })?;
    let client = CloudClient::new(cfg, token, secrets.clone());
    let resp = client
        .bind_device(&local_device_id(), &default_display_name(), None)
        .await?;
    if let Some(device_token) = resp.get("device_token").and_then(|v| v.as_str()) {
        secrets
            .choose(&token_id())
            .set(&token_id(), device_token, SecretOrigin::SystemGenerated)
            .await?;
    }
    Ok(())
}

/// A human-readable display name for this device (host + OS).
fn default_display_name() -> String {
    let host = std::env::var("HOSTNAME")
        .ok()
        .or_else(|| std::env::var("COMPUTERNAME").ok())
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| "syscity".to_string());
    format!("{} ({})", std::env::consts::OS, host)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn with_temp_device_id(f: impl FnOnce()) {
        let dir = std::env::temp_dir().join(format!("syscity_device_id_{}", Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("device_id");
        DEVICE_ID_PATH_OVERRIDE.with(|c| *c.borrow_mut() = Some(path.clone()));
        // Run the body, then drop the override so other tests read the real
        // config dir (they never call local_device_id, but stay hermetic).
        f();
        DEVICE_ID_PATH_OVERRIDE.with(|c| *c.borrow_mut() = None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn mints_and_persists_a_stable_id() {
        with_temp_device_id(|| {
            let first = local_device_id();
            assert_eq!(first.len(), 36, "minted a v4 UUID");
            // Persisted to disk and stable across calls.
            assert_eq!(local_device_id(), first);
            let on_disk = std::fs::read_to_string(device_id_path()).unwrap();
            assert_eq!(on_disk.trim(), first);
        });
    }

    #[test]
    fn reuses_an_existing_id() {
        with_temp_device_id(|| {
            std::fs::write(device_id_path(), "my-device\n").unwrap();
            assert_eq!(local_device_id(), "my-device");
        });
    }
}
