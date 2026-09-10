//! Tier 2 backend — 0600/0700 atomic-write file storage.
//!
//! Each `(namespace, entity)` maps to one TOML file:
//!
//! ```text
//! ~/.syscity/secrets/{namespace}/{entity}.toml
//! ```
//!
//! The file structure is a `[secrets] kind = "value"` table. The directory is
//! `0700`, the file is `0600` (write a temp file → `set_permissions` → atomic
//! rename, so the final file never carries default permissive permissions).
//! The write pattern is absorbed from the retired `src/mcp/env_store.rs`.
//!
//! Legacy format `~/.syscity/mcp_env/{id}.toml` (`[env]` table) is read for
//! compatibility: on first startup `migrate_legacy_mcp_env()` moves it into the
//! new location and then deletes the old files.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use aes_gcm::aead::{Aead, AeadCore, OsRng};
use aes_gcm::{Aes256Gcm, Key, KeyInit, Nonce};
use base64::Engine as _;
use rand::RngCore;
use serde::{Deserialize, Serialize};
use tracing::{info, warn};
use zeroize::Zeroize;

use crate::error::SyscityError;
#[cfg(all(not(test), feature = "keyring"))]
use crate::secrets::keyring_store::{probe_keyring, with_timeout};
use crate::secrets::store::{SecretId, SecretOrigin, SecretStore, SecretStoreHandle};

/// Top-level secrets directory (`~/.syscity/secrets`).
pub fn secrets_root_dir() -> PathBuf {
    crate::dirs::syscity_dir().join("secrets")
}

/// Reject entity names that could escape the storage directory.
pub fn sanitize_entity(entity: &str) -> crate::Result<String> {
    if entity.is_empty()
        || entity == "."
        || entity == ".."
        || entity.contains('/')
        || entity.contains('\\')
        || entity.contains(std::path::MAIN_SEPARATOR)
    {
        return Err(SyscityError::Validation(format!("invalid secret entity: {entity:?}")));
    }
    Ok(entity.to_string())
}

/// A single stored value: plaintext (legacy / no master key) or AES-GCM
/// encrypted. Untagged so legacy plaintext files keep parsing in encrypted
/// mode and newly written files stay readable without a master key.
#[derive(Debug, Serialize, Deserialize)]
#[serde(untagged)]
enum SecretEntry {
    /// Plaintext value.
    Plain(String),
    /// `base64(nonce || ciphertext)` — AES-256-GCM with a random nonce.
    Encrypted { encrypted: String },
}

/// On-disk file content: `[secrets] kind = "value"`.
#[derive(Debug, Default, Serialize, Deserialize)]
struct SecretsFile {
    #[serde(default)]
    secrets: HashMap<String, SecretEntry>,
}

/// Legacy `mcp_env` file content (`[env]` table) — only read during migration.
#[derive(Debug, Serialize, Deserialize)]
struct LegacyEnvFile {
    #[serde(default)]
    env: HashMap<String, String>,
}

/// Tier 2 file backend.
///
/// When a master key is available (loaded from the keyring or the 0600
/// `.master_key` fallback) values are encrypted at rest with AES-256-GCM;
/// otherwise they are written plaintext with 0600 permissions.
#[derive(Debug, Clone)]
pub struct FileStore {
    namespace: String,
    root: PathBuf,
    master_key: Option<Arc<MasterKey>>,
}

impl FileStore {
    /// Create a file backend bound to a namespace, an explicit root, and a
    /// master key.
    ///
    /// The root and key are normally supplied by a [`SecretStoreHandle`], which
    /// owns them for the lifetime of a gateway instance. A `None` key means
    /// values are written plaintext with `0600` permissions (no key backend was
    /// available).
    pub(crate) fn with_root_and_key(
        namespace: &str,
        root: PathBuf,
        master_key: Option<Arc<MasterKey>>,
    ) -> Self {
        Self {
            namespace: namespace.to_string(),
            root,
            master_key,
        }
    }

    /// Test helper: point the root at a temp directory to keep tests hermetic.
    #[cfg(test)]
    pub(crate) fn with_root(namespace: &str, root: PathBuf) -> Self {
        Self {
            namespace: namespace.to_string(),
            root,
            master_key: None,
        }
    }

    /// Test helper: hermetic store with an injected master key so encryption
    /// is exercised without touching the real keyring or `~/.syscity`.
    #[cfg(test)]
    pub(crate) fn with_root_encrypted(namespace: &str, root: PathBuf, key: &MasterKey) -> Self {
        Self {
            namespace: namespace.to_string(),
            root,
            master_key: Some(Arc::new(key.clone())),
        }
    }

    /// Namespace directory: `{root}/{namespace}`.
    pub fn base_dir(&self) -> PathBuf {
        self.root.join(&self.namespace)
    }

    /// Entity file path (validates the entity name first).
    pub fn path_for(&self, entity: &str) -> crate::Result<PathBuf> {
        let entity = sanitize_entity(entity)?;
        Ok(self.base_dir().join(format!("{entity}.toml")))
    }

    /// Ensure the storage directory exists with correct permissions.
    pub async fn validate_store(&self) -> crate::Result<()> {
        let dir = self.base_dir();
        tokio::fs::create_dir_all(&dir).await?;
        set_dir_perms(&dir).await?;
        Ok(())
    }

    /// Read an entity's whole map; a missing file yields an empty map.
    pub async fn get_all(&self, entity: &str) -> crate::Result<HashMap<String, String>> {
        let path = self.path_for(entity)?;
        match tokio::fs::read_to_string(&path).await {
            Ok(content) => {
                let file: SecretsFile = toml::from_str(&content)?;
                let mut out = HashMap::with_capacity(file.secrets.len());
                for (kind, entry) in file.secrets {
                    let value = match entry {
                        SecretEntry::Plain(value) => value,
                        SecretEntry::Encrypted { encrypted } => match &self.master_key {
                            Some(key) => key.decrypt(&encrypted)?,
                            None => {
                                return Err(SyscityError::Internal(format!(
                                    "secret '{kind}' in {} is encrypted but no master key is available",
                                    path.display()
                                )));
                            }
                        },
                    };
                    out.insert(kind, value);
                }
                Ok(out)
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(HashMap::new()),
            Err(e) => Err(e.into()),
        }
    }

    /// Overwrite an entity's whole map (encrypted when a master key exists).
    pub async fn set_all(&self, entity: &str, map: &HashMap<String, String>) -> crate::Result<()> {
        let path = self.path_for(entity)?;
        let mut secrets = HashMap::with_capacity(map.len());
        for (kind, value) in map {
            let entry = match &self.master_key {
                Some(key) => SecretEntry::Encrypted { encrypted: key.encrypt(value)? },
                None => SecretEntry::Plain(value.clone()),
            };
            secrets.insert(kind.clone(), entry);
        }
        write_atomically(&path, &SecretsFile { secrets }).await
    }

    /// Delete an entity's whole file (missing is not an error).
    pub async fn delete_entity(&self, entity: &str) -> crate::Result<()> {
        let path = self.path_for(entity)?;
        match tokio::fs::remove_file(&path).await {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e.into()),
        }
    }

    /// Whether an entity already has a file.
    pub async fn has_entity(&self, entity: &str) -> bool {
        match self.path_for(entity) {
            Ok(path) => tokio::fs::metadata(path)
                .await
                .map(|m| m.is_file())
                .unwrap_or(false),
            Err(_) => false,
        }
    }
}

#[async_trait::async_trait]
impl SecretStore for FileStore {
    async fn get(&self, id: &SecretId) -> crate::Result<Option<String>> {
        let map = self.get_all(&id.entity).await?;
        Ok(map.get(&id.kind).cloned())
    }

    async fn set(&self, id: &SecretId, value: &str, _origin: SecretOrigin) -> crate::Result<()> {
        let mut map = self.get_all(&id.entity).await?;
        map.insert(id.kind.clone(), value.to_string());
        self.set_all(&id.entity, &map).await
    }

    async fn delete(&self, id: &SecretId) -> crate::Result<()> {
        let mut map = self.get_all(&id.entity).await?;
        if map.remove(&id.kind).is_some() {
            if map.is_empty() {
                self.delete_entity(&id.entity).await?;
            } else {
                self.set_all(&id.entity, &map).await?;
            }
        }
        Ok(())
    }

    async fn has(&self, id: &SecretId) -> bool {
        matches!(self.get(id).await, Ok(Some(_)))
    }

    // Whole-map operations forward to the inherent methods (which take
    // precedence for concrete types); the trait versions exist so the methods
    // are reachable through `Arc<dyn SecretStore>`.
    async fn get_all(&self, entity: &str) -> crate::Result<HashMap<String, String>> {
        FileStore::get_all(self, entity).await
    }

    async fn set_all(&self, entity: &str, map: &HashMap<String, String>) -> crate::Result<()> {
        FileStore::set_all(self, entity, map).await
    }

    async fn delete_entity(&self, entity: &str) -> crate::Result<()> {
        FileStore::delete_entity(self, entity).await
    }

    async fn has_entity(&self, entity: &str) -> bool {
        FileStore::has_entity(self, entity).await
    }
}

// ─────────────────────────────────────────────
// AES-256-GCM master key
// ─────────────────────────────────────────────

/// AES-256 master key used to encrypt file-store values at rest.
///
/// The raw bytes are zeroized on drop; the key itself is persisted in the OS
/// keyring (`syscity` / `file-master-key`) with a 0600 `.master_key` fallback
/// for headless hosts.
pub struct MasterKey([u8; 32]);

impl MasterKey {
    /// Generate a fresh random 256-bit key.
    pub(crate) fn random() -> Self {
        let mut bytes = [0u8; 32];
        rand::thread_rng().fill_bytes(&mut bytes);
        Self(bytes)
    }

    /// Encrypt `plaintext` → `base64(nonce[12] || ciphertext)` with a fresh
    /// random nonce (AES-256-GCM).
    pub fn encrypt(&self, plaintext: &str) -> crate::Result<String> {
        let cipher = Aes256Gcm::new(Key::<Aes256Gcm>::from_slice(&self.0));
        let nonce = Aes256Gcm::generate_nonce(&mut OsRng);
        let ciphertext = cipher
            .encrypt(&nonce, plaintext.as_bytes())
            .map_err(|e| SyscityError::Internal(format!("AES-GCM encryption failed: {e}")))?;
        let mut out = Vec::with_capacity(nonce.len() + ciphertext.len());
        out.extend_from_slice(&nonce);
        out.extend_from_slice(&ciphertext);
        Ok(base64::engine::general_purpose::STANDARD.encode(out))
    }

    /// Decrypt a `base64(nonce || ciphertext)` value back to plaintext.
    pub fn decrypt(&self, data: &str) -> crate::Result<String> {
        let raw = base64::engine::general_purpose::STANDARD
            .decode(data)
            .map_err(|e| {
                SyscityError::Internal(format!("encrypted secret is not valid base64: {e}"))
            })?;
        if raw.len() < NONCE_LEN {
            return Err(SyscityError::Internal(
                "encrypted secret is shorter than a nonce".to_string(),
            ));
        }
        let (nonce_bytes, ciphertext) = raw.split_at(NONCE_LEN);
        let nonce = Nonce::from_slice(nonce_bytes);
        let cipher = Aes256Gcm::new(Key::<Aes256Gcm>::from_slice(&self.0));
        let plain = cipher
            .decrypt(nonce, ciphertext)
            .map_err(|e| SyscityError::Internal(format!("AES-GCM decryption failed: {e}")))?;
        String::from_utf8(plain)
            .map_err(|e| SyscityError::Internal(format!("decrypted secret is not UTF-8: {e}")))
    }
}

impl Clone for MasterKey {
    fn clone(&self) -> Self {
        Self(self.0)
    }
}

/// The raw key is never logged — only the type name.
impl std::fmt::Debug for MasterKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("MasterKey([REDACTED])")
    }
}

impl Drop for MasterKey {
    fn drop(&mut self) {
        self.0.zeroize();
    }
}

/// AES-GCM nonce length in bytes.
const NONCE_LEN: usize = 12;

/// The file-store master key for a given storage root.
///
/// Source priority: OS keyring (feature `keyring`, only when `use_keyring` is
/// set) → 0600 `.master_key` file under `root`. Returns `Ok(None)` when no
/// backend is available (plaintext fallback).
///
/// This is called exactly once per [`SecretStoreHandle`] at construction; the
/// handle owns the result so no process-global cache is required.
pub(crate) fn load_or_create_master_key(
    root: &Path,
    use_keyring: bool,
) -> crate::Result<Option<MasterKey>> {
    let _ = use_keyring;

    // 1. OS keyring (preferred, feature `keyring` only). The probe is
    // cooldown-throttled and never hangs; the keyring read/write below are
    // additionally time-bounded so a dark-wake hang falls back to the 0600
    // file instead of blocking startup.
    #[cfg(all(feature = "keyring", not(test)))]
    {
        if use_keyring && probe_keyring() {
            if let Some(key) = read_master_key_keyring_bounded()? {
                return Ok(Some(key));
            }
            let key = MasterKey::random();
            if write_master_key_keyring_bounded(&key)? {
                return Ok(Some(key));
            }
            // Keyring write timed out or failed — fall through to the file path.
        }
    }

    // 2. 0600 `.master_key` file (headless hosts / default file storage).
    if let Some(key) = read_master_key_file(root)? {
        return Ok(Some(key));
    }
    let key = MasterKey::random();
    match write_master_key_file(root, &key) {
        Ok(()) => Ok(Some(key)),
        Err(e) => {
            warn!("Cannot persist master key; secrets stored unencrypted: {e}");
            Ok(None)
        }
    }
}

/// Keyring account name for the file-store master key.
#[cfg(all(not(test), feature = "keyring"))]
const MASTER_KEY_ACCOUNT: &str = "file-master-key";

#[cfg(all(not(test), feature = "keyring"))]
fn read_master_key_keyring() -> crate::Result<Option<MasterKey>> {
    let entry = keyring::Entry::new("syscity", MASTER_KEY_ACCOUNT).map_err(|e| {
        SyscityError::Internal(format!("keyring entry unavailable (file-master-key): {e}"))
    })?;
    match entry.get_password() {
        Ok(raw) => decode_master_key(&raw).map(Some),
        Err(keyring::Error::NoEntry) => Ok(None),
        Err(e) => Err(SyscityError::Internal(format!("keyring read error (file-master-key): {e}"))),
    }
}

#[cfg(all(not(test), feature = "keyring"))]
fn write_master_key_keyring(key: &MasterKey) -> crate::Result<()> {
    let entry = keyring::Entry::new("syscity", MASTER_KEY_ACCOUNT).map_err(|e| {
        SyscityError::Internal(format!("keyring entry unavailable (file-master-key): {e}"))
    })?;
    entry
        .set_password(&encode_master_key(key))
        .map_err(|e| SyscityError::Internal(format!("keyring write error (file-master-key): {e}")))
}

/// Bounded keyring master-key read. A timeout (macOS dark wake) is treated as
/// absent so the 0600 file fallback is used instead of hanging startup.
#[cfg(all(not(test), feature = "keyring"))]
fn read_master_key_keyring_bounded() -> crate::Result<Option<MasterKey>> {
    match with_timeout(read_master_key_keyring) {
        Some(result) => result,
        None => Ok(None),
    }
}

/// Bounded keyring master-key write; `Ok(false)` when it did not finish in
/// time, letting the caller fall through to the 0600 file fallback.
#[cfg(all(not(test), feature = "keyring"))]
fn write_master_key_keyring_bounded(key: &MasterKey) -> crate::Result<bool> {
    let key = key.clone();
    match with_timeout(move || write_master_key_keyring(&key)) {
        Some(result) => result.map(|()| true),
        None => Ok(false),
    }
}

/// Path of the headless master-key fallback file under an explicit root.
fn master_key_path(root: &Path) -> PathBuf {
    root.join(".master_key")
}

fn read_master_key_file(root: &Path) -> crate::Result<Option<MasterKey>> {
    let path = master_key_path(root);
    match std::fs::read_to_string(&path) {
        Ok(content) => match decode_master_key(content.trim()) {
            Ok(key) => Ok(Some(key)),
            Err(e) => {
                warn!("Master key file {} is unreadable: {e}", path.display());
                Ok(None)
            }
        },
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e.into()),
    }
}

fn write_master_key_file(root: &Path, key: &MasterKey) -> crate::Result<()> {
    let path = master_key_path(root);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
        #[cfg(unix)]
        std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700))?;
    }
    std::fs::write(&path, encode_master_key(key))?;
    #[cfg(unix)]
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
    Ok(())
}

fn encode_master_key(key: &MasterKey) -> String {
    base64::engine::general_purpose::STANDARD.encode(key.0)
}

fn decode_master_key(raw: &str) -> crate::Result<MasterKey> {
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(raw)
        .map_err(|e| SyscityError::Internal(format!("master key is not valid base64: {e}")))?;
    let key: [u8; 32] = bytes
        .try_into()
        .map_err(|_| SyscityError::Internal("master key must be 32 bytes".to_string()))?;
    Ok(MasterKey(key))
}

// ─────────────────────────────────────────────
// Atomic write + permissions
// ─────────────────────────────────────────────

/// Write a temp file → tighten permissions → atomic rename.
async fn write_atomically(path: &Path, file: &SecretsFile) -> crate::Result<()> {
    let dir = path
        .parent()
        .ok_or_else(|| SyscityError::Internal("secret store path has no parent".to_string()))?;
    tokio::fs::create_dir_all(dir).await?;
    set_dir_perms(dir).await?;

    let content = toml::to_string(file)?;
    let tmp = path.with_extension("toml.tmp");
    tokio::fs::write(&tmp, content).await?;
    set_file_perms(&tmp).await?;
    tokio::fs::rename(&tmp, &path).await?;
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

// ─────────────────────────────────────────────
// Legacy format migration (~/.syscity/mcp_env → ~/.syscity/secrets/mcp-env)
// ─────────────────────────────────────────────

/// Migrate the old `~/.syscity/mcp_env/{id}.toml` (`[env]` table) once into
/// `~/.syscity/secrets/mcp-env/{id}.toml` (`[secrets]` table), then delete the
/// old directory.
///
/// The migration must be atomic and rollback-safe: each file is written to its
/// new location before the old file is removed; on any failure the current
/// state is kept and a warning is logged. The old directory is removed once
/// empty.
pub async fn migrate_legacy_mcp_env(secrets: &SecretStoreHandle) -> crate::Result<()> {
    let store = secrets.file_store("mcp-env");
    migrate_legacy_mcp_env_with_store(crate::dirs::syscity_dir().join("mcp_env"), &store).await
}

async fn migrate_legacy_mcp_env_with_store(
    old_dir: PathBuf,
    store: &FileStore,
) -> crate::Result<()> {
    if tokio::fs::metadata(&old_dir).await.is_err() {
        return Ok(());
    }
    if !old_dir.is_dir() {
        return Ok(());
    }

    let mut migrated = 0usize;
    let mut failed = 0usize;

    let mut entries = match tokio::fs::read_dir(&old_dir).await {
        Ok(e) => e,
        Err(e) => {
            warn!("Cannot read legacy mcp_env dir {}: {e}", old_dir.display());
            return Ok(());
        }
    };

    while let Ok(Some(entry)) = entries.next_entry().await {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("toml") {
            continue;
        }
        let Some(id) = path.file_stem().and_then(|s| s.to_str()).map(String::from) else {
            continue;
        };

        let legacy: LegacyEnvFile = match tokio::fs::read_to_string(&path).await {
            Ok(content) => match toml::from_str(&content) {
                Ok(f) => f,
                Err(e) => {
                    warn!("Skipping legacy mcp_env {} (unreadable): {e}", path.display());
                    failed += 1;
                    continue;
                }
            },
            Err(_) => continue,
        };

        // Only delete the old file after the new location was written.
        match store.set_all(&id, &legacy.env).await {
            Ok(()) => {
                if let Err(e) = tokio::fs::remove_file(&path).await {
                    if e.kind() != std::io::ErrorKind::NotFound {
                        warn!("Migrated {} but failed to remove legacy file: {e}", path.display());
                    }
                }
                migrated += 1;
            }
            Err(e) => {
                warn!("Failed to migrate legacy mcp_env {}: {e}", path.display());
                failed += 1;
            }
        }
    }

    // Remove the whole old directory once it is empty.
    let remaining = match tokio::fs::read_dir(&old_dir).await {
        Ok(mut e) => {
            let mut n = 0usize;
            while let Ok(Some(_)) = e.next_entry().await {
                n += 1;
            }
            n
        }
        Err(_) => 0,
    };
    if remaining == 0 {
        let _ = tokio::fs::remove_dir(&old_dir).await;
    }

    info!("mcp_env legacy migration: {migrated} migrated, {failed} failed");
    Ok(())
}

// ─────────────────────────────────────────────
// Tests
// ─────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::secrets::store::{SecretId, SecretOrigin};

    /// Unique temp dir per test so concurrent tests do not wipe each other's
    /// state (all tests share the process id, so a name suffix is required).
    fn temp_root(name: &str) -> PathBuf {
        let dir = std::env::temp_dir()
            .join(format!("syscity_file_store_test_{}_{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    // Point the store root at the temp dir via an override for tests.
    // We can't easily monkeypatch secrets_root_dir(), so we exercise the
    // public path through a real ~/.syscity/secrets ... instead use the
    // path_for/validate_store on a store whose base we compute from
    // path_for — but that always goes to the real dir. For hermetic tests we
    // write through FileStore but target the temp dir by constructing paths
    // manually via the low-level helpers below.
    async fn write_at(
        dir: &Path,
        entity: &str,
        map: &HashMap<String, String>,
    ) -> crate::Result<()> {
        let path = dir.join(format!("{entity}.toml"));
        tokio::fs::create_dir_all(dir).await?;
        set_dir_perms(dir).await?;
        let secrets = map
            .iter()
            .map(|(k, v)| (k.clone(), SecretEntry::Plain(v.clone())))
            .collect();
        let content = toml::to_string(&SecretsFile { secrets })?;
        let tmp = path.with_extension("toml.tmp");
        tokio::fs::write(&tmp, content).await?;
        set_file_perms(&tmp).await?;
        tokio::fs::rename(&tmp, &path).await?;
        Ok(())
    }

    async fn read_map(dir: &Path, entity: &str) -> HashMap<String, String> {
        let content = tokio::fs::read_to_string(dir.join(format!("{entity}.toml")))
            .await
            .unwrap_or_default();
        toml::from_str::<SecretsFile>(&content)
            .map(|f| {
                f.secrets
                    .into_iter()
                    .map(|(k, entry)| {
                        let value = match entry {
                            SecretEntry::Plain(v) => v,
                            SecretEntry::Encrypted { .. } => String::new(),
                        };
                        (k, value)
                    })
                    .collect()
            })
            .unwrap_or_default()
    }

    #[test]
    fn test_sanitize_entity() {
        assert!(sanitize_entity("github").is_ok());
        assert!(sanitize_entity("github-main").is_ok());
        assert!(sanitize_entity("").is_err());
        assert!(sanitize_entity(".").is_err());
        assert!(sanitize_entity("..").is_err());
        assert!(sanitize_entity("../x").is_err());
        assert!(sanitize_entity("a/b").is_err());
        assert!(sanitize_entity("a\\b").is_err());
    }

    #[tokio::test]
    async fn test_secrets_file_roundtrip() {
        let dir = temp_root("roundtrip");
        let mut map = HashMap::new();
        map.insert("refresh_token".to_string(), "rt_abc".to_string());
        map.insert("client_id".to_string(), "cid".to_string());
        write_at(&dir, "myserver", &map).await.unwrap();

        let loaded = read_map(&dir, "myserver").await;
        assert_eq!(loaded["refresh_token"], "rt_abc");
        assert_eq!(loaded["client_id"], "cid");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn test_permissions() {
        use std::os::unix::fs::PermissionsExt;
        let dir = temp_root("perms");
        let mut map = HashMap::new();
        map.insert("k".to_string(), "v".to_string());
        write_at(&dir, "sec", &map).await.unwrap();

        let dir_meta = tokio::fs::metadata(&dir).await.unwrap();
        assert_eq!(dir_meta.permissions().mode() & 0o777, 0o700);

        let file_meta = tokio::fs::metadata(dir.join("sec.toml")).await.unwrap();
        assert_eq!(file_meta.permissions().mode() & 0o777, 0o600);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn test_secret_store_trait_via_file_store() {
        let root = temp_root("trait").join("root");
        let store = FileStore::with_root("mcp-oauth", root.clone());
        let id = SecretId::new("mcp-oauth", "myserver", "refresh_token");

        store
            .set(&id, "rt_secret", SecretOrigin::UserEntered)
            .await
            .unwrap();
        assert!(store.has(&id).await);
        assert_eq!(store.get(&id).await.unwrap(), Some("rt_secret".to_string()));
        assert!(store.has_entity("myserver").await);
        assert_eq!(store.get_all("myserver").await.unwrap()["refresh_token"], "rt_secret");

        store.delete(&id).await.unwrap();
        assert!(!store.has(&id).await);

        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn test_legacy_mcp_env_migration() {
        // Simulate a legacy ~/.syscity/mcp_env with an [env] table.
        let root = temp_root("migration");
        let old_dir = root.join("mcp_env");
        tokio::fs::create_dir_all(&old_dir).await.unwrap();
        let legacy = "[env]\nGITHUB_PERSONAL_ACCESS_TOKEN = \"ghp_x\"\n";
        tokio::fs::write(old_dir.join("github.toml"), legacy)
            .await
            .unwrap();

        // Migrate into the same temp-rooted store that the read uses, so the
        // test never touches the real ~/.syscity/secrets directory.
        let store = FileStore::with_root("mcp-env", root.join("secrets"));
        migrate_legacy_mcp_env_with_store(old_dir.clone(), &store)
            .await
            .unwrap();

        let loaded = store.get_all("github").await.unwrap();
        assert_eq!(loaded["GITHUB_PERSONAL_ACCESS_TOKEN"], "ghp_x");
        // Old file + directory are gone.
        assert!(!old_dir.exists());

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn test_master_key_roundtrip() {
        let key = MasterKey::random();
        let enc = key.encrypt("super-secret").unwrap();
        assert_ne!(enc, "super-secret");
        assert!(enc.contains('=') || enc.contains('+') || enc.contains('/'));
        assert_eq!(key.decrypt(&enc).unwrap(), "super-secret");
    }

    #[test]
    fn test_master_key_wrong_key_fails() {
        let a = MasterKey::random();
        let b = MasterKey::random();
        let enc = a.encrypt("value").unwrap();
        assert!(b.decrypt(&enc).is_err());
    }

    #[test]
    fn test_master_key_roundtrip_unicode() {
        let key = MasterKey::random();
        let enc = key.encrypt("中文密钥 123").unwrap();
        assert_eq!(key.decrypt(&enc).unwrap(), "中文密钥 123");
    }

    #[test]
    fn test_decode_master_key_rejects_bad_input() {
        assert!(decode_master_key("").is_err());
        assert!(decode_master_key("aGVsbG8=").is_err()); // 5 bytes, not 32
        assert!(decode_master_key("not base64!!").is_err());
    }

    #[tokio::test]
    async fn test_encrypted_store_roundtrip() {
        let root = temp_root("enc_roundtrip");
        let key = MasterKey::random();
        let store = FileStore::with_root_encrypted("channel", root.join("secrets"), &key);
        let id = SecretId::new("channel", "whatsapp", "access_token");

        store
            .set(&id, "at_secret", SecretOrigin::UserEntered)
            .await
            .unwrap();
        assert_eq!(store.get(&id).await.unwrap(), Some("at_secret".to_string()));

        // The on-disk file must not contain the plaintext value.
        let content = tokio::fs::read_to_string(store.path_for("whatsapp").unwrap())
            .await
            .unwrap();
        assert!(!content.contains("at_secret"));
        assert!(content.contains("encrypted"));

        // A fresh store with the same key can read it back.
        let store2 = FileStore::with_root_encrypted("channel", root.join("secrets"), &key);
        assert_eq!(store2.get(&id).await.unwrap(), Some("at_secret".to_string()));

        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn test_encrypted_read_requires_master_key() {
        let root = temp_root("enc_nokey");
        let key = MasterKey::random();
        let store = FileStore::with_root_encrypted("channel", root.join("secrets"), &key);
        store
            .set(
                &SecretId::new("channel", "whatsapp", "access_token"),
                "at_secret",
                SecretOrigin::UserEntered,
            )
            .await
            .unwrap();

        // A store without a master key cannot read the encrypted value.
        let store_no_key = FileStore::with_root("channel", root.join("secrets"));
        let err = store_no_key.get_all("whatsapp").await.unwrap_err();
        assert!(err.to_string().contains("encrypted"));

        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn test_legacy_plaintext_file_reads_in_encrypted_mode() {
        // A file written in plaintext mode must still parse when a master key
        // is present (untagged SecretEntry keeps legacy files readable).
        let root = temp_root("enc_legacy");
        let mut map = HashMap::new();
        map.insert("token".to_string(), "legacy_plain".to_string());
        // The store scopes files under `{root}/{namespace}`, so write the legacy
        // plaintext file into the "channel" subdirectory.
        write_at(&root.join("channel"), "telegram", &map)
            .await
            .unwrap();

        let key = MasterKey::random();
        let store = FileStore::with_root_encrypted("channel", root.clone(), &key);
        let loaded = store.get_all("telegram").await.unwrap();
        assert_eq!(loaded["token"], "legacy_plain");

        let _ = std::fs::remove_dir_all(&root);
    }
}
