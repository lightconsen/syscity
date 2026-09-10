//! Secret Management System
//!
//! Provides multi-source secret resolution for API keys, tokens, and
//! credentials. Supports environment variables, file-based secrets, and
//! external executables.
//!
//! # Example
//!
//! ```toml
//! [providers.anthropic]
//! api_key = { env = "ANTHROPIC_API_KEY" }
//!
//! [providers.openai]
//! api_key = "$OPENAI_API_KEY"  # shorthand syntax
//!
//! [providers.custom]
//! api_key = { source = "file", path = "/run/secrets/api_key" }
//! ```
//!
//! The storage backend (`SecretStore`) coexists with the reference resolver
//! (`SecretResolver`):
//! - `store.rs` — `SecretId` / `SecretOrigin` / `SecretStore` trait / tier routing.
//! - `keyring_store.rs` — Tier 1 OS keyring backend (feature `keyring`, opt-in).
//! - `file_store.rs` — Tier 2 file backend (absorbs the retired `mcp/env_store.rs`).
//! - `in_memory.rs` — Tier 3 zeroize memory backend.
//!
//! Default routing is the 0600 encrypted file store. The OS keyring backend is
//! opt-in: compile with `--features keyring` for keyring-primary routing with a
//! file fallback. Without it, the platform keychain is never touched.
// INVARIANTS-NONE: the file store enforces 0600 + encryption atomically at write time; nothing readable persists if a write fails.

mod file_store;
mod in_memory;
#[cfg(feature = "keyring")]
mod keyring_store;
mod mask;
mod store;

pub use file_store::{migrate_legacy_mcp_env, sanitize_entity, secrets_root_dir, FileStore};
pub use in_memory::MemoryStore;
#[cfg(feature = "keyring")]
pub use keyring_store::{probe_keyring, KeyringStore};
pub use mask::{
    is_secret_container_key, is_secret_key, mask_json_value, mask_secret,
    mask_secret_container_payload,
};
pub use store::{
    SecretId, SecretOrigin, SecretStore, SecretStoreHandle, SecretStoreTier, SecretValue, StoreRef,
    SENSITIVE_CHANNEL_CREDENTIALS,
};

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use tokio::sync::RwLock;
use tracing::{debug, warn};
use zeroize::{Zeroize, ZeroizeOnDrop};

use crate::error::{ConfigError, Result};

/// A resolved secret value with metadata
///
/// SECURITY NOTE: This struct implements custom `Debug` that redacts the secret
/// value. The secret value is also automatically zeroized in memory when the
/// struct is dropped.
#[derive(Clone, Zeroize, ZeroizeOnDrop)]
pub struct ResolvedSecret {
    /// The secret value - automatically zeroized on drop
    #[zeroize]
    pub value: String,
    /// Source of the secret (not sensitive, but kept for metadata)
    #[zeroize(skip)]
    pub source: SecretSource,
    /// When the secret was resolved (not sensitive)
    #[zeroize(skip)]
    pub resolved_at: Instant,
    /// Time-to-live for cached value (not sensitive)
    #[zeroize(skip)]
    pub ttl: Option<Duration>,
}

impl std::fmt::Debug for ResolvedSecret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ResolvedSecret")
            .field("value", &"[REDACTED]")
            .field("source", &self.source)
            .field("resolved_at", &self.resolved_at)
            .field("ttl", &self.ttl)
            .finish()
    }
}

/// Source of a secret
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SecretSource {
    /// Raw inline value (not recommended)
    Inline,
    /// From environment variable
    Env(String),
    /// From file
    File(PathBuf),
    /// From external executable
    Exec { command: String, exit_code: i32 },
}

impl std::fmt::Display for SecretSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SecretSource::Inline => write!(f, "inline"),
            SecretSource::Env(var) => write!(f, "env:{}", var),
            SecretSource::File(path) => write!(f, "file:{}", path.display()),
            SecretSource::Exec { command, .. } => write!(f, "exec:{}", command),
        }
    }
}

/// A cached value with an expiration time.
#[derive(Debug, Clone)]
struct CachedValue<T> {
    value: T,
    expires_at: Instant,
}

impl<T> CachedValue<T> {
    fn new(value: T, ttl: Duration) -> Self {
        Self {
            value,
            expires_at: Instant::now() + ttl,
        }
    }

    fn is_expired(&self) -> bool {
        Instant::now() >= self.expires_at
    }
}

/// Secret reference - can be a raw string or a secret reference
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(untagged)]
pub enum SecretRef {
    /// Shorthand syntax: "$ENV_VAR" or just a raw value
    String(String),
    /// Explicit reference with source
    Explicit {
        /// Environment variable name
        #[serde(skip_serializing_if = "Option::is_none")]
        env: Option<String>,
        /// File path
        #[serde(skip_serializing_if = "Option::is_none")]
        file: Option<PathBuf>,
        /// External command
        #[serde(skip_serializing_if = "Option::is_none")]
        exec: Option<String>,
    },
}

impl SecretRef {
    /// Create a secret reference from an environment variable
    pub fn from_env(var: impl Into<String>) -> Self {
        SecretRef::Explicit {
            env: Some(var.into()),
            file: None,
            exec: None,
        }
    }

    /// Create a secret reference from a file path
    pub fn from_file(path: impl Into<PathBuf>) -> Self {
        SecretRef::Explicit {
            env: None,
            file: Some(path.into()),
            exec: None,
        }
    }

    /// Create a secret reference from an external command
    pub fn from_exec(command: impl Into<String>) -> Self {
        SecretRef::Explicit {
            env: None,
            file: None,
            exec: Some(command.into()),
        }
    }

    /// Check if this is an inline/raw value (no secret reference)
    pub fn is_raw_value(&self) -> bool {
        match self {
            SecretRef::String(s) => !s.starts_with('$'),
            SecretRef::Explicit { env, file, exec } => {
                env.is_none() && file.is_none() && exec.is_none()
            }
        }
    }

    /// Get the raw value if it's not a secret reference
    pub fn as_raw_value(&self) -> Option<&str> {
        match self {
            SecretRef::String(s) if !s.starts_with('$') => Some(s),
            _ => None,
        }
    }
}

impl Default for SecretRef {
    fn default() -> Self {
        SecretRef::String(String::new())
    }
}

/// Configuration for a secrets provider
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SecretProviderConfig {
    /// Provider type
    pub source: SecretProviderType,
    /// Base path for file-based providers
    #[serde(skip_serializing_if = "Option::is_none")]
    pub base_path: Option<PathBuf>,
    /// Command template for exec-based providers
    #[serde(skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
}

/// Type of secret provider
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SecretProviderType {
    /// Environment variables
    Env,
    /// File-based secrets
    File,
    /// External executable
    Exec,
}

/// Runtime snapshot of resolved secrets
#[derive(Debug, Clone)]
pub struct SecretsSnapshot {
    /// Resolved secrets by key
    secrets: HashMap<String, ResolvedSecret>,
    /// When the snapshot was created
    created_at: Instant,
    /// Default TTL for cached secrets
    default_ttl: Duration,
}

impl SecretsSnapshot {
    /// Create a new empty snapshot
    pub fn new(default_ttl: Duration) -> Self {
        Self {
            secrets: HashMap::new(),
            created_at: Instant::now(),
            default_ttl,
        }
    }

    /// Get a secret value by key
    pub fn get(&self, key: &str) -> Option<&str> {
        self.secrets.get(key).map(|s| s.value.as_str())
    }

    /// Check if the snapshot has expired
    pub fn is_expired(&self) -> bool {
        self.created_at.elapsed() > self.default_ttl
    }

    /// Get the age of the snapshot
    pub fn age(&self) -> Duration {
        self.created_at.elapsed()
    }

    /// Insert a resolved secret
    pub fn insert(&mut self, key: impl Into<String>, secret: ResolvedSecret) {
        self.secrets.insert(key.into(), secret);
    }

    /// Get all secret keys
    pub fn keys(&self) -> impl Iterator<Item = &String> {
        self.secrets.keys()
    }

    /// Get the number of secrets
    pub fn len(&self) -> usize {
        self.secrets.len()
    }

    /// Check if the snapshot is empty
    pub fn is_empty(&self) -> bool {
        self.secrets.is_empty()
    }
}

/// Secret resolver with caching and multiple sources
#[derive(Debug, Clone)]
pub struct SecretResolver {
    /// Named providers
    providers: HashMap<String, SecretProviderConfig>,
    /// Current snapshot
    snapshot: Arc<RwLock<SecretsSnapshot>>,
    /// Whether to use degraded mode (fallback to last-known-good)
    degraded_mode: bool,
    /// Whether hot-reload is enabled
    hot_reload: bool,
    /// Cache TTL for per-provider payloads and per-reference results.
    cache_ttl: Duration,
    /// Per-provider payload cache (env value, file content, exec output).
    provider_payload_cache: Arc<RwLock<HashMap<String, CachedValue<String>>>>,
    /// Per-reference result cache.
    ref_result_cache: Arc<RwLock<HashMap<String, CachedValue<String>>>>,
}

impl SecretResolver {
    /// Create a new secret resolver
    pub fn new(default_ttl: Duration) -> Self {
        Self {
            providers: HashMap::new(),
            snapshot: Arc::new(RwLock::new(SecretsSnapshot::new(default_ttl))),
            degraded_mode: false,
            hot_reload: false,
            cache_ttl: default_ttl,
            provider_payload_cache: Arc::new(RwLock::new(HashMap::new())),
            ref_result_cache: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    /// Add a named provider
    pub fn add_provider(&mut self, name: impl Into<String>, config: SecretProviderConfig) {
        self.providers.insert(name.into(), config);
    }

    /// Enable degraded mode
    pub fn set_degraded_mode(&mut self, enabled: bool) {
        self.degraded_mode = enabled;
    }

    /// Enable hot-reload
    pub fn set_hot_reload(&mut self, enabled: bool) {
        self.hot_reload = enabled;
    }

    /// Set the TTL used for the per-provider payload and per-reference caches.
    pub fn set_cache_ttl(&mut self, ttl: Duration) {
        self.cache_ttl = ttl;
    }

    /// Get the current cache TTL.
    pub fn cache_ttl(&self) -> Duration {
        self.cache_ttl
    }

    /// Resolve a secret reference to its value
    pub async fn resolve(&self, reference: &SecretRef) -> Result<String> {
        // Stable cache key derived from the reference itself.
        if let Ok(cache_key) = serde_json::to_string(reference) {
            {
                let cache = self.ref_result_cache.read().await;
                if let Some(entry) = cache.get(&cache_key) {
                    if !entry.is_expired() {
                        debug!("Returning cached resolved secret");
                        return Ok(entry.value.clone());
                    }
                }
            }

            let result = match reference {
                SecretRef::String(s) => self.resolve_string(s).await,
                SecretRef::Explicit { env, file, exec } => {
                    if let Some(var) = env {
                        self.resolve_env(var).await
                    } else if let Some(path) = file {
                        self.resolve_file(path).await
                    } else if let Some(cmd) = exec {
                        self.resolve_exec(cmd).await
                    } else {
                        Err(ConfigError::InvalidValue {
                            key: "secret".to_string(),
                            message: "Empty secret reference - no source specified".to_string(),
                        }
                        .into())
                    }
                }
            };

            if let Ok(value) = &result {
                let mut cache = self.ref_result_cache.write().await;
                cache.insert(cache_key, CachedValue::new(value.clone(), self.cache_ttl));
            }

            return result;
        }

        // Fallback if the reference cannot be serialized for caching.
        match reference {
            SecretRef::String(s) => self.resolve_string(s).await,
            SecretRef::Explicit { env, file, exec } => {
                if let Some(var) = env {
                    self.resolve_env(var).await
                } else if let Some(path) = file {
                    self.resolve_file(path).await
                } else if let Some(cmd) = exec {
                    self.resolve_exec(cmd).await
                } else {
                    Err(ConfigError::InvalidValue {
                        key: "secret".to_string(),
                        message: "Empty secret reference - no source specified".to_string(),
                    }
                    .into())
                }
            }
        }
    }

    /// Resolve a string value (handles $ENV_VAR syntax)
    async fn resolve_string(&self, value: &str) -> Result<String> {
        // Check for shorthand syntax: $ENV_VAR or ${ENV_VAR}
        if let Some(var_name) = value.strip_prefix('$') {
            let var_name = var_name.trim_start_matches('{').trim_end_matches('}');
            return self.resolve_env(var_name).await;
        }

        // Raw inline value
        Ok(value.to_string())
    }

    /// Resolve from environment variable
    async fn resolve_env(&self, var: &str) -> Result<String> {
        debug!("Resolving secret from env: {}", var);

        let cache_key = format!("env:{}", var);
        {
            let cache = self.provider_payload_cache.read().await;
            if let Some(entry) = cache.get(&cache_key) {
                if !entry.is_expired() {
                    debug!("Returning cached env payload for {}", var);
                    return Ok(entry.value.clone());
                }
            }
        }

        match std::env::var(var) {
            Ok(value) => {
                if value.is_empty() {
                    warn!("Environment variable {} is set but empty", var);
                }
                let mut cache = self.provider_payload_cache.write().await;
                cache.insert(cache_key, CachedValue::new(value.clone(), self.cache_ttl));
                Ok(value)
            }
            Err(_) => {
                // Check snapshot for last-known-good value
                let snapshot = self.snapshot.read().await;
                if let Some(cached) = snapshot.get(var) {
                    if self.degraded_mode {
                        warn!("Using cached value for {} (env var not available)", var);
                        return Ok(cached.to_string());
                    }
                }

                Err(ConfigError::Missing(format!("Environment variable {} not set", var)).into())
            }
        }
    }

    /// Resolve from file
    async fn resolve_file(&self, path: &Path) -> Result<String> {
        debug!("Resolving secret from file: {}", path.display());

        let cache_key = format!("file:{}", path.display());
        {
            let cache = self.provider_payload_cache.read().await;
            if let Some(entry) = cache.get(&cache_key) {
                if !entry.is_expired() {
                    debug!("Returning cached file payload for {}", path.display());
                    return Ok(entry.value.clone());
                }
            }
        }

        // For async file operations in tokio
        let path = path.to_path_buf();
        let content = tokio::task::spawn_blocking(move || {
            std::fs::read_to_string(&path).map_err(|e| ConfigError::FileRead { path, source: e })
        })
        .await
        .map_err(|e| crate::error::SyscityError::Internal(format!("Task join error: {}", e)))??;

        // Trim whitespace (including newlines) from file content
        let content = content.trim().to_string();
        let mut cache = self.provider_payload_cache.write().await;
        cache.insert(cache_key, CachedValue::new(content.clone(), self.cache_ttl));
        Ok(content)
    }

    /// Resolve from external command
    async fn resolve_exec(&self, command: &str) -> Result<String> {
        debug!("Resolving secret from exec: {}", command);

        let cache_key = format!("exec:{}", command);
        {
            let cache = self.provider_payload_cache.read().await;
            if let Some(entry) = cache.get(&cache_key) {
                if !entry.is_expired() {
                    debug!("Returning cached exec payload for {}", command);
                    return Ok(entry.value.clone());
                }
            }
        }

        // Parse command and args
        let parts: Vec<&str> = command.split_whitespace().collect();
        if parts.is_empty() {
            return Err(ConfigError::InvalidValue {
                key: "exec".to_string(),
                message: "Empty exec command".to_string(),
            }
            .into());
        }

        let output = tokio::process::Command::new(parts[0])
            .args(&parts[1..])
            .output()
            .await
            .map_err(|e| ConfigError::InvalidValue {
                key: "exec".to_string(),
                message: format!("Failed to execute command: {}", e),
            })?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(ConfigError::InvalidValue {
                key: "exec".to_string(),
                message: format!(
                    "Command failed with exit code {}: {}",
                    output.status.code().unwrap_or(-1),
                    stderr
                ),
            }
            .into());
        }

        let stdout = String::from_utf8(output.stdout).map_err(|e| ConfigError::InvalidValue {
            key: "exec".to_string(),
            message: format!("Invalid UTF-8 in command output: {}", e),
        })?;

        let content = stdout.trim().to_string();
        let mut cache = self.provider_payload_cache.write().await;
        cache.insert(cache_key, CachedValue::new(content.clone(), self.cache_ttl));
        Ok(content)
    }

    /// Refresh all caches, forcing the next resolutions to re-read their
    /// sources. The runtime snapshot is left intact so degraded-mode fallback
    /// values remain available until a new snapshot is built.
    pub async fn refresh(&self) {
        self.provider_payload_cache.write().await.clear();
        self.ref_result_cache.write().await.clear();
        debug!("Secret caches refreshed");
    }

    /// Refresh a specific secret reference, clearing both its per-reference
    /// cache entry and the underlying provider payload cache entries.
    pub async fn refresh_reference(&self, reference: &SecretRef) {
        if let Ok(cache_key) = serde_json::to_string(reference) {
            self.ref_result_cache.write().await.remove(&cache_key);
        }

        let mut provider_cache = self.provider_payload_cache.write().await;
        match reference {
            SecretRef::String(s) if s.starts_with('$') => {
                let var = s
                    .strip_prefix('$')
                    .unwrap_or(s)
                    .trim_start_matches('{')
                    .trim_end_matches('}');
                provider_cache.remove(&format!("env:{}", var));
            }
            SecretRef::Explicit { env, file, exec } => {
                if let Some(var) = env {
                    provider_cache.remove(&format!("env:{}", var));
                }
                if let Some(path) = file {
                    provider_cache.remove(&format!("file:{}", path.display()));
                }
                if let Some(cmd) = exec {
                    provider_cache.remove(&format!("exec:{}", cmd));
                }
            }
            _ => {}
        }
    }

    /// Build a snapshot of resolved secrets from a configuration
    pub async fn build_snapshot(
        &self,
        secrets_config: &HashMap<String, SecretRef>,
    ) -> Result<SecretsSnapshot> {
        let mut snapshot = SecretsSnapshot::new(self.snapshot.read().await.default_ttl);

        for (key, reference) in secrets_config {
            match self.resolve(reference).await {
                Ok(value) => {
                    let source = match reference {
                        SecretRef::String(s) if s.starts_with('$') => {
                            SecretSource::Env(s.trim_start_matches('$').to_string())
                        }
                        SecretRef::Explicit { env: Some(var), .. } => {
                            SecretSource::Env(var.clone())
                        }
                        SecretRef::Explicit { file: Some(path), .. } => {
                            SecretSource::File(path.clone())
                        }
                        SecretRef::Explicit { exec: Some(cmd), .. } => SecretSource::Exec {
                            command: cmd.clone(),
                            exit_code: 0,
                        },
                        _ => SecretSource::Inline,
                    };

                    snapshot.insert(
                        key.clone(),
                        ResolvedSecret {
                            value,
                            source,
                            resolved_at: Instant::now(),
                            ttl: Some(snapshot.default_ttl),
                        },
                    );
                }
                Err(e) => {
                    if !self.degraded_mode {
                        return Err(e);
                    }
                    warn!("Failed to resolve secret '{}': {}", key, e);
                }
            }
        }

        Ok(snapshot)
    }

    /// Update the runtime snapshot
    pub async fn update_snapshot(&self, snapshot: SecretsSnapshot) {
        let mut guard = self.snapshot.write().await;
        *guard = snapshot;
    }

    /// Get the current snapshot
    pub async fn get_snapshot(&self) -> SecretsSnapshot {
        self.snapshot.read().await.clone()
    }

    /// Check if the snapshot needs refreshing
    pub async fn needs_refresh(&self) -> bool {
        let snapshot = self.snapshot.read().await;
        snapshot.is_expired()
    }
}

impl Default for SecretResolver {
    fn default() -> Self {
        Self::new(Duration::from_secs(3600)) // 1 hour default TTL
    }
}

/// Helper function to resolve a secret value from various sources
pub async fn resolve_secret(reference: &SecretRef) -> Result<String> {
    let resolver = SecretResolver::default();
    resolver.resolve(reference).await
}

/// Helper function to check if a value looks like a secret reference
pub fn is_secret_reference(value: &str) -> bool {
    value.starts_with('$') || value.contains("env:") || value.contains("file:")
}

/// Resolve all secrets in a configuration map
pub async fn resolve_secrets(config: &HashMap<String, String>) -> Result<HashMap<String, String>> {
    let resolver = SecretResolver::default();
    let mut resolved = HashMap::new();

    for (key, value) in config {
        // Create SecretRef - resolver will handle whether it's an env reference or raw
        // value
        let secret_ref = SecretRef::String(value.clone());

        match resolver.resolve(&secret_ref).await {
            Ok(resolved_value) => {
                resolved.insert(key.clone(), resolved_value);
            }
            Err(_) => {
                // If resolution fails, keep the original value
                resolved.insert(key.clone(), value.clone());
            }
        }
    }

    Ok(resolved)
}

#[cfg(test)]
mod tests {
    use std::env;

    use super::*;

    #[test]
    fn test_secret_ref_from_env() {
        let secret = SecretRef::from_env("TEST_VAR");
        match secret {
            SecretRef::Explicit { env, file, exec } => {
                assert_eq!(env, Some("TEST_VAR".to_string()));
                assert!(file.is_none());
                assert!(exec.is_none());
            }
            _ => panic!("Expected Explicit variant"),
        }
    }

    #[test]
    fn test_secret_ref_from_file() {
        let secret = SecretRef::from_file("/path/to/secret");
        match secret {
            SecretRef::Explicit { env, file, exec } => {
                assert!(env.is_none());
                assert_eq!(file, Some(PathBuf::from("/path/to/secret")));
                assert!(exec.is_none());
            }
            _ => panic!("Expected Explicit variant"),
        }
    }

    #[test]
    fn test_secret_ref_raw_value() {
        let secret = SecretRef::String("raw_value".to_string());
        assert!(secret.is_raw_value());
        assert_eq!(secret.as_raw_value(), Some("raw_value"));

        let secret = SecretRef::String("$ENV_VAR".to_string());
        assert!(!secret.is_raw_value());
        assert_eq!(secret.as_raw_value(), None);
    }

    #[test]
    fn test_is_secret_reference() {
        assert!(is_secret_reference("$ENV_VAR"));
        assert!(is_secret_reference("${ENV_VAR}"));
        assert!(!is_secret_reference("raw_value"));
        assert!(!is_secret_reference(""));
    }

    #[tokio::test]
    async fn test_resolve_env() {
        env::set_var("TEST_SECRET_VAR", "test_value");

        let resolver = SecretResolver::default();
        let result = resolver.resolve_env("TEST_SECRET_VAR").await;

        assert!(result.is_ok());
        assert_eq!(result.unwrap(), "test_value");

        env::remove_var("TEST_SECRET_VAR");
    }

    #[tokio::test]
    async fn test_resolve_string_shorthand() {
        env::set_var("TEST_SHORTHAND", "shorthand_value");

        let resolver = SecretResolver::default();
        let result = resolver.resolve_string("$TEST_SHORTHAND").await;

        assert!(result.is_ok());
        assert_eq!(result.unwrap(), "shorthand_value");

        env::remove_var("TEST_SHORTHAND");
    }

    #[tokio::test]
    async fn test_resolve_string_raw() {
        let resolver = SecretResolver::default();
        let result = resolver.resolve_string("raw_value").await;

        assert!(result.is_ok());
        assert_eq!(result.unwrap(), "raw_value");
    }

    #[tokio::test]
    async fn test_resolve_file() {
        // Create a temporary file with a secret
        let temp_dir = std::env::temp_dir();
        let secret_path = temp_dir.join("test_secret.txt");
        std::fs::write(&secret_path, "file_secret_value\n").unwrap();

        let resolver = SecretResolver::default();
        let result = resolver.resolve_file(&secret_path).await;

        assert!(result.is_ok());
        assert_eq!(result.unwrap(), "file_secret_value");

        // Cleanup
        std::fs::remove_file(&secret_path).unwrap();
    }

    #[tokio::test]
    async fn test_secret_ref_serialization() {
        // Test string variant
        let secret = SecretRef::String("$ENV_VAR".to_string());
        let json = serde_json::to_string(&secret).unwrap();
        assert_eq!(json, r#""$ENV_VAR""#);

        // Test explicit variant
        let secret = SecretRef::Explicit {
            env: Some("MY_VAR".to_string()),
            file: None,
            exec: None,
        };
        let json = serde_json::to_string(&secret).unwrap();
        assert!(json.contains("env"));
        assert!(json.contains("MY_VAR"));
    }

    #[tokio::test]
    async fn test_secret_ref_deserialization() {
        // Test string variant
        let json = r#""$ENV_VAR""#;
        let secret: SecretRef = serde_json::from_str(json).unwrap();
        assert_eq!(secret, SecretRef::String("$ENV_VAR".to_string()));

        // Test explicit variant
        let json = r#"{"env": "MY_VAR"}"#;
        let secret: SecretRef = serde_json::from_str(json).unwrap();
        assert_eq!(
            secret,
            SecretRef::Explicit {
                env: Some("MY_VAR".to_string()),
                file: None,
                exec: None,
            }
        );
    }

    #[tokio::test]
    async fn test_per_ref_cache_env() {
        let var = "TEST_PER_REF_CACHE_ENV";
        env::set_var(var, "old_value");

        let mut resolver = SecretResolver::new(Duration::from_secs(3600));
        resolver.set_cache_ttl(Duration::from_secs(3600));
        let reference = SecretRef::from_env(var);

        let first = resolver.resolve(&reference).await.unwrap();
        assert_eq!(first, "old_value");

        env::set_var(var, "new_value");
        let second = resolver.resolve(&reference).await.unwrap();
        assert_eq!(second, "old_value", "per-reference cache should return old value");

        resolver.refresh().await;
        let third = resolver.resolve(&reference).await.unwrap();
        assert_eq!(third, "new_value", "after refresh the new value should be read");

        env::remove_var(var);
    }

    #[tokio::test]
    async fn test_provider_payload_cache_env() {
        let var = "TEST_PROVIDER_PAYLOAD_CACHE_ENV";
        env::set_var(var, "old_value");

        let mut resolver = SecretResolver::new(Duration::from_secs(3600));
        resolver.set_cache_ttl(Duration::from_secs(3600));

        let shorthand = SecretRef::String(format!("${}", var));
        let explicit = SecretRef::from_env(var);

        let first = resolver.resolve(&shorthand).await.unwrap();
        assert_eq!(first, "old_value");

        env::set_var(var, "new_value");
        let second = resolver.resolve(&explicit).await.unwrap();
        assert_eq!(
            second, "old_value",
            "provider payload cache should be shared between references"
        );

        resolver.refresh_reference(&explicit).await;
        let third = resolver.resolve(&explicit).await.unwrap();
        assert_eq!(third, "new_value");

        env::remove_var(var);
    }

    #[tokio::test]
    async fn test_file_cache() {
        let temp_dir = std::env::temp_dir();
        let path = temp_dir.join("test_secret_cache_file.txt");
        std::fs::write(&path, "old_file_secret\n").unwrap();

        let mut resolver = SecretResolver::new(Duration::from_secs(3600));
        resolver.set_cache_ttl(Duration::from_secs(3600));
        let reference = SecretRef::from_file(&path);

        let first = resolver.resolve(&reference).await.unwrap();
        assert_eq!(first, "old_file_secret");

        std::fs::write(&path, "new_file_secret\n").unwrap();
        let second = resolver.resolve(&reference).await.unwrap();
        assert_eq!(second, "old_file_secret", "file content should be cached");

        resolver.refresh().await;
        let third = resolver.resolve(&reference).await.unwrap();
        assert_eq!(third, "new_file_secret");

        std::fs::remove_file(&path).unwrap();
    }

    #[tokio::test]
    async fn test_exec_cache() {
        let var = "TEST_EXEC_CACHE_VAR";
        env::set_var(var, "old_exec_secret");

        let mut resolver = SecretResolver::new(Duration::from_secs(3600));
        resolver.set_cache_ttl(Duration::from_secs(3600));
        let reference = SecretRef::from_exec(format!("printenv {}", var));

        let first = resolver.resolve(&reference).await.unwrap();
        assert_eq!(first, "old_exec_secret");

        env::set_var(var, "new_exec_secret");
        let second = resolver.resolve(&reference).await.unwrap();
        assert_eq!(second, "old_exec_secret", "exec output should be cached");

        resolver.refresh().await;
        let third = resolver.resolve(&reference).await.unwrap();
        assert_eq!(third, "new_exec_secret");

        env::remove_var(var);
    }

    #[tokio::test]
    async fn test_refresh_reference_is_selective() {
        let var_a = "TEST_REFRESH_SELECTIVE_A";
        let var_b = "TEST_REFRESH_SELECTIVE_B";
        env::set_var(var_a, "old_a");
        env::set_var(var_b, "old_b");

        let mut resolver = SecretResolver::new(Duration::from_secs(3600));
        resolver.set_cache_ttl(Duration::from_secs(3600));
        let ref_a = SecretRef::from_env(var_a);
        let ref_b = SecretRef::from_env(var_b);

        assert_eq!(resolver.resolve(&ref_a).await.unwrap(), "old_a");
        assert_eq!(resolver.resolve(&ref_b).await.unwrap(), "old_b");

        env::set_var(var_a, "new_a");
        env::set_var(var_b, "new_b");

        resolver.refresh_reference(&ref_a).await;
        assert_eq!(resolver.resolve(&ref_a).await.unwrap(), "new_a");
        assert_eq!(resolver.resolve(&ref_b).await.unwrap(), "old_b");

        env::remove_var(var_a);
        env::remove_var(var_b);
    }

    #[test]
    fn test_cache_ttl_accessor() {
        let mut resolver = SecretResolver::new(Duration::from_secs(60));
        assert_eq!(resolver.cache_ttl(), Duration::from_secs(60));
        resolver.set_cache_ttl(Duration::from_secs(120));
        assert_eq!(resolver.cache_ttl(), Duration::from_secs(120));
    }
}
