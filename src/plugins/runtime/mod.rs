//! Plugin Runtime - WASM-based plugin execution
//!
//! Loads and executes plugins using Wasmtime for sandboxing.
//!
//! Structure:
//!   - `state.rs` — Type definitions (PluginSharedState, PluginState,
//!     PluginInstance, etc.)
//!   - `host_functions.rs` — 16 WASM host function definitions

#[cfg(feature = "plugins")]
mod host_functions;
mod state;

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex as StdMutex};

pub use state::PluginInstance;
#[cfg(feature = "plugins")]
use state::PluginPersistentState;
use state::{get_migrations, MigrationRecord, CURRENT_SCHEMA_VERSION};
#[cfg(feature = "plugins")]
pub use state::{PluginEvent, PluginSharedState, PluginState};
use tokio::sync::RwLock;
use tokio::sync::{mpsc, Mutex};
use tracing::{debug, info, warn};

use super::manifest::{PluginManifest, PluginPermission};
use super::metrics::PluginMetricsRegistry;

/// Shared handle to the event subscriber map.
#[cfg(feature = "plugins")]
type EventSubscribers =
    Arc<RwLock<HashMap<String, Vec<(u64, mpsc::UnboundedSender<PluginEvent>)>>>>;

/// Plugin runtime - manages plugin lifecycle
pub struct PluginRuntime {
    /// Directory holding per-plugin persisted state (`<root>/plugins/data`).
    state_root: std::path::PathBuf,
    plugins: Arc<RwLock<HashMap<String, PluginInstance>>>,
    #[cfg(feature = "plugins")]
    engine: wasmtime::Engine,
    #[cfg(feature = "plugins")]
    linker: wasmtime::Linker<PluginState>,
    #[cfg(feature = "plugins")]
    shared_state: Arc<PluginSharedState>,
    /// Event subscribers: plugin_id/wildcard → list of (subscription_id,
    /// sender)
    #[cfg(feature = "plugins")]
    event_subscribers: EventSubscribers,
    /// Monotonically increasing subscription ID counter for selective
    /// unsubscription.
    #[cfg(feature = "plugins")]
    next_sub_id: AtomicU64,
    /// Receiver side of the plugin event channel (kept so the channel stays
    /// open while the runtime is alive; the `Some` variant is moved into
    /// the dispatch task spawned in `new()`, leaving `None` here).
    /// This field is never directly read after construction; it exists solely
    /// to hold the `Arc` reference that keeps the `mpsc` channel open.
    #[cfg(feature = "plugins")]
    #[allow(dead_code)]
    event_rx: Arc<Mutex<Option<mpsc::UnboundedReceiver<PluginEvent>>>>,
    /// Handle to the spawned event dispatch task so panics are not silently
    /// ignored when the runtime is dropped.
    #[cfg(feature = "plugins")]
    event_dispatch_handle: Option<tokio::task::JoinHandle<()>>,
    /// Per-plugin metrics registry
    metrics: Arc<PluginMetricsRegistry>,
}

impl PluginRuntime {
    /// Create a new plugin runtime whose plugin state lives under `state_root`.
    pub fn new(state_root: std::path::PathBuf) -> crate::Result<Self> {
        #[cfg(feature = "plugins")]
        {
            let mut config = wasmtime::Config::default();
            config.consume_fuel(true);
            config.max_wasm_stack(512 * 1024);
            let engine = wasmtime::Engine::new(&config)
                .map_err(|e| crate::error::SyscityError::Internal(e.to_string()))?;
            let mut linker = wasmtime::Linker::new(&engine);
            let (event_tx, event_rx) = mpsc::unbounded_channel::<PluginEvent>();
            let metrics = Arc::new(PluginMetricsRegistry::new());
            let shared_state = Arc::new(PluginSharedState::with_events(event_tx, metrics.clone()));
            let event_subscribers: EventSubscribers = Arc::new(RwLock::new(HashMap::new()));
            let next_sub_id = AtomicU64::new(1);

            // Define host functions for plugins
            host_functions::define_host_functions(&mut linker)?;

            let subscribers_clone = event_subscribers.clone();

            // Spawn event dispatch task (only if tokio runtime is active)
            let event_dispatch_handle = if let Ok(handle) = tokio::runtime::Handle::try_current() {
                Some(handle.spawn(async move {
                    Self::event_dispatch_loop(subscribers_clone, event_rx).await;
                }))
            } else {
                None
            };

            Ok(Self {
                plugins: Arc::new(RwLock::new(HashMap::new())),
                engine,
                linker,
                shared_state,
                event_subscribers,
                next_sub_id,
                event_rx: Arc::new(Mutex::new(None)),
                event_dispatch_handle,
                metrics: Arc::new(PluginMetricsRegistry::new()),
                state_root,
            })
        }

        #[cfg(not(feature = "plugins"))]
        {
            Ok(Self {
                plugins: Arc::new(RwLock::new(HashMap::new())),
                metrics: Arc::new(PluginMetricsRegistry::new()),
                state_root,
            })
        }
    }

    /// Subscribe to events from a specific plugin (or `"*"` for all).
    /// Returns a subscription ID that can be passed to `unsubscribe_events`
    /// and a receiver for the events.
    #[cfg(feature = "plugins")]
    pub async fn subscribe_events(
        &self,
        pattern: &str,
    ) -> (u64, mpsc::UnboundedReceiver<PluginEvent>) {
        let (tx, rx) = mpsc::unbounded_channel();
        let sub_id = self.next_sub_id.fetch_add(1, Ordering::Relaxed);
        self.event_subscribers
            .write()
            .await
            .entry(pattern.to_string())
            .or_default()
            .push((sub_id, tx));
        (sub_id, rx)
    }

    /// Unsubscribe a specific subscription by its ID.
    /// Removes only the matching subscription while leaving others intact.
    #[cfg(feature = "plugins")]
    pub async fn unsubscribe_events(&self, sub_id: u64) {
        let mut subs = self.event_subscribers.write().await;
        subs.retain(|_, senders| {
            // Keep all senders whose ID does NOT match
            senders.retain(|(id, _)| *id != sub_id);
            !senders.is_empty()
        });
    }

    /// Background dispatch loop: receives events from the plugin channel
    /// and forwards them to matching subscribers.
    #[cfg(feature = "plugins")]
    async fn event_dispatch_loop(
        subscribers: EventSubscribers,
        mut rx: mpsc::UnboundedReceiver<PluginEvent>,
    ) {
        while let Some(event) = rx.recv().await {
            let subs = subscribers.read().await;
            // Dispatch to exact plugin_id subscribers
            if let Some(senders) = subs.get(&event.plugin_id) {
                for (_id, tx) in senders {
                    if tx.send(event.clone()).is_err() {
                        warn!("Failed to dispatch event to subscriber of '{}'", event.plugin_id);
                    }
                }
            }
            // Dispatch to wildcard subscribers
            if let Some(senders) = subs.get("*") {
                for (_id, tx) in senders {
                    if tx.send(event.clone()).is_err() {
                        warn!("Failed to dispatch event to wildcard subscriber");
                    }
                }
            }
        }
        warn!("Plugin event dispatch loop ended");
    }

    /// Save plugin state (memory + KV store) to disk (best-effort).
    #[cfg(feature = "plugins")]
    async fn save_plugin_state(
        &self,
        plugin_id: &str,
        memory: &Arc<RwLock<HashMap<String, Vec<u8>>>>,
    ) {
        let state_dir = self.state_root.join(plugin_id);
        let state_path = state_dir.join("state.json");

        let memory = memory.read().await.clone();
        let kv_store = self.shared_state.kv_store.read().await.clone();

        let persistent = PluginPersistentState {
            schema_version: CURRENT_SCHEMA_VERSION,
            memory,
            kv_store,
            migration_history: Vec::new(),
        };

        match serde_json::to_string_pretty(&persistent) {
            Ok(json) => {
                let tmp_path = state_path.with_extension("tmp");
                if let Err(e) = std::fs::create_dir_all(&state_dir) {
                    warn!("Failed to create plugin state dir for '{}': {}", plugin_id, e);
                    return;
                }
                if let Err(e) = std::fs::write(&tmp_path, &json) {
                    warn!("Failed to write plugin state for '{}': {}", plugin_id, e);
                    return;
                }
                if let Err(e) = std::fs::rename(&tmp_path, &state_path) {
                    warn!("Failed to atomically rename plugin state for '{}': {}", plugin_id, e);
                }
            }
            Err(e) => {
                warn!("Failed to serialize plugin state for '{}': {}", plugin_id, e);
            }
        }
    }

    /// Load plugin state from disk (best-effort).
    #[cfg(feature = "plugins")]
    async fn load_plugin_state(
        &self,
        plugin_id: &str,
    ) -> (Option<HashMap<String, Vec<u8>>>, Option<HashMap<String, String>>) {
        let state_path = self.state_root.join(plugin_id).join("state.json");
        if !state_path.exists() {
            return (None, None);
        }

        match tokio::fs::read_to_string(&state_path).await {
            Ok(content) => {
                match serde_json::from_str::<PluginPersistentState>(&content) {
                    Ok(mut persistent) => {
                        // Apply migrations if needed
                        let current_ver = persistent.schema_version;
                        if current_ver > CURRENT_SCHEMA_VERSION {
                            warn!(
                                "Plugin state for '{}' has schema v{} which is newer than \
                                 supported v{}. Ignoring.",
                                plugin_id, current_ver, CURRENT_SCHEMA_VERSION
                            );
                            return (None, None);
                        }

                        let migrations = get_migrations();
                        for (from, to, migrate_fn) in &migrations {
                            if persistent.schema_version == *from {
                                info!(
                                    "Migrating plugin '{}' state from v{} to v{}",
                                    plugin_id, from, to
                                );
                                match migrate_fn(&mut persistent) {
                                    Ok(()) => {
                                        persistent.migration_history.push(MigrationRecord {
                                            from_version: *from,
                                            to_version: *to,
                                            migrated_at: chrono::Utc::now().to_rfc3339(),
                                        });
                                    }
                                    Err(e) => {
                                        warn!(
                                            "Failed to migrate plugin '{}' state from v{} to v{}: \
                                             {}",
                                            plugin_id, from, to, e
                                        );
                                        return (None, None);
                                    }
                                }
                            }
                        }

                        // Extract this plugin's kv entries from the outer map
                        let kv = persistent.kv_store.get(plugin_id).cloned();
                        (Some(persistent.memory), kv)
                    }
                    Err(e) => {
                        warn!("Failed to deserialize plugin state for '{}': {}", plugin_id, e);
                        (None, None)
                    }
                }
            }
            Err(e) => {
                warn!("Failed to read plugin state for '{}': {}", plugin_id, e);
                (None, None)
            }
        }
    }

    /// Validate manifest version fields, logging warnings on issues
    /// (non-fatal).
    #[cfg(feature = "plugins")]
    fn validate_manifest_version(manifest: &PluginManifest) {
        // Validate plugin's own version string
        if let Err(e) = crate::skills::semver::Version::parse(&manifest.version) {
            warn!(
                "Plugin '{}' has invalid semver version '{}': {}",
                manifest.id, manifest.version, e
            );
        }

        // Check syscity_version constraint if present
        if let Some(ref req_str) = manifest.syscity_version {
            match req_str.parse::<crate::skills::semver::VersionReq>() {
                Ok(req) => {
                    let syscity_ver =
                        crate::skills::semver::Version::parse(env!("CARGO_PKG_VERSION"))
                            .unwrap_or_else(|_| crate::skills::semver::Version::new(0, 1, 2));
                    if !req.matches(&syscity_ver) {
                        warn!(
                            "Plugin '{}' requires syscity version '{}' but current is '{}'",
                            manifest.id, req_str, syscity_ver
                        );
                    } else {
                        debug!(
                            "Plugin '{}' syscity_version constraint '{}' satisfied",
                            manifest.id, req_str
                        );
                    }
                }
                Err(e) => {
                    warn!(
                        "Plugin '{}' has invalid syscity_version constraint '{}': {}",
                        manifest.id, req_str, e
                    );
                }
            }
        }
    }

    /// Load a plugin from a directory
    pub async fn load_plugin(&self, path: &std::path::Path) -> crate::Result<String> {
        let manifest_path = path.join("plugin.json");

        if !manifest_path.exists() {
            return Err(crate::error::ConfigError::Missing(format!(
                "Plugin manifest not found at {:?}",
                manifest_path
            ))
            .into());
        }

        let manifest_content = tokio::fs::read_to_string(&manifest_path)
            .await
            .map_err(|e| crate::error::SyscityError::ExternalService {
                source: "Failed to read plugin manifest".to_string(),
                cause: Some(Box::new(e)),
            })?;

        let manifest: PluginManifest = serde_json::from_str(&manifest_content).map_err(|e| {
            crate::error::ConfigError::InvalidValue {
                key: "plugin.json".to_string(),
                message: format!("Invalid plugin manifest: {}", e),
            }
        })?;

        let plugin_id = manifest.id.clone();

        // Validate manifest version fields (non-fatal warnings)
        #[cfg(feature = "plugins")]
        Self::validate_manifest_version(&manifest);

        // Verify manifest signature (reject invalid signatures)
        #[cfg(feature = "plugins")]
        {
            let verification = crate::plugins::verification::verify_manifest(&manifest);
            crate::plugins::verification::log_verification(&plugin_id, &verification);
            if matches!(verification, crate::plugins::verification::VerificationResult::Invalid(_))
            {
                return Err(crate::error::SyscityError::Validation(format!(
                    "Plugin '{}' has an invalid manifest signature and cannot be loaded",
                    plugin_id
                )));
            }
        }

        info!("Loading plugin '{}' ({}) from {:?}", manifest.name, plugin_id, path);

        // Load config if present
        let config_path = path.join("config.json");
        let config = if config_path.exists() {
            let config_content = match tokio::fs::read_to_string(&config_path).await {
                Ok(c) => c,
                Err(e) => {
                    warn!("Failed to read plugin config from {:?}: {}", config_path, e);
                    String::new()
                }
            };
            serde_json::from_str(&config_content).unwrap_or_else(|e| {
                warn!("Failed to parse plugin config from {:?}: {}", config_path, e);
                serde_json::json!({})
            })
        } else {
            manifest.config.clone().unwrap_or(serde_json::json!({}))
        };

        #[cfg(feature = "plugins")]
        let (wasm_store, instance) = {
            // Load persisted state from disk
            let (preserved_memory, persisted_kv) = self.load_plugin_state(&plugin_id).await;
            if let Some(kv_entries) = persisted_kv {
                let mut store = self.shared_state.kv_store.write().await;
                let plugin_store = store.entry(plugin_id.clone()).or_default();
                for (k, v) in kv_entries {
                    plugin_store.insert(k, v);
                }
            }

            if let Some(ref main) = manifest.main {
                let wasm_path = path.join(main);
                if wasm_path.exists() {
                    let permissions = manifest.permissions.clone().unwrap_or_default();
                    self.load_wasm_plugin(
                        &wasm_path,
                        config.clone(),
                        &plugin_id,
                        preserved_memory,
                        permissions,
                    )
                    .await?
                } else {
                    warn!("WASM file not found: {:?}", wasm_path);
                    (None, None)
                }
            } else {
                (None, None)
            }
        };

        let instance = PluginInstance {
            manifest,
            path: path.to_path_buf(),
            enabled: true,
            config,
            #[cfg(feature = "plugins")]
            wasm_store,
            #[cfg(feature = "plugins")]
            instance,
        };

        let mut plugins = self.plugins.write().await;
        plugins.insert(plugin_id.clone(), instance);

        // Register metrics for this plugin
        self.metrics.register(&plugin_id).await;

        info!("Plugin '{}' loaded successfully", plugin_id);

        Ok(plugin_id)
    }

    #[cfg(feature = "plugins")]
    async fn load_wasm_plugin(
        &self,
        wasm_path: &std::path::Path,
        config: serde_json::Value,
        plugin_id: &str,
        preserved_memory: Option<HashMap<String, Vec<u8>>>,
        permissions: Vec<PluginPermission>,
    ) -> crate::Result<(Option<wasmtime::Store<PluginState>>, Option<wasmtime::Instance>)> {
        use wasmtime::Module;

        let wasm_bytes = tokio::fs::read(wasm_path).await.map_err(|e| {
            crate::error::SyscityError::ExternalService {
                source: "Failed to read WASM file".to_string(),
                cause: Some(Box::new(e)),
            }
        })?;

        let module = Module::new(&self.engine, &wasm_bytes).map_err(|e| {
            crate::error::SyscityError::Internal(format!("Failed to compile WASM: {}", e))
        })?;

        let state = if let Some(memory) = preserved_memory {
            PluginState::new_with_memory(
                config,
                memory,
                self.shared_state.clone(),
                plugin_id.to_string(),
                permissions,
            )
        } else {
            PluginState::new(config, self.shared_state.clone(), plugin_id.to_string(), permissions)
        };
        let mut store = wasmtime::Store::new(&self.engine, state);

        // Set initial fuel for WASM execution
        store
            .set_fuel(100_000_000)
            .map_err(|e| crate::error::SyscityError::Internal(e.to_string()))?;

        // Set up WASI context with inherited stdio
        {
            let wasi_ctx = wasmtime_wasi::WasiCtxBuilder::new()
                .inherit_stdio()
                .build_p1();
            store.data_mut().wasi_ctx = StdMutex::new(wasi_ctx);
        }

        let instance = self.linker.instantiate(&mut store, &module).map_err(|e| {
            crate::error::SyscityError::Internal(format!("Failed to instantiate WASM: {}", e))
        })?;

        // Call init function if present
        if let Ok(init) = instance.get_typed_func::<(), ()>(&mut store, "init") {
            init.call(&mut store, ()).map_err(|e| {
                crate::error::SyscityError::Internal(format!("Plugin init failed: {}", e))
            })?;
        }

        Ok((Some(store), Some(instance)))
    }

    #[cfg(not(feature = "plugins"))]
    async fn load_wasm_plugin(
        &self,
        _wasm_path: &std::path::Path,
        _config: serde_json::Value,
    ) -> crate::Result<(Option<()>, Option<()>)> {
        Err(crate::error::SyscityError::Internal(
            "Plugin execution requires the `plugins` feature. Recompile Syscity with `--features \
             plugins` to enable WASM plugin support."
                .to_string(),
        ))
    }

    /// Unload a plugin
    pub async fn unload_plugin(&self, plugin_id: &str) -> crate::Result<bool> {
        let mut plugins = self.plugins.write().await;

        if let Some(plugin) = plugins.remove(plugin_id) {
            // Save plugin state to disk before dropping (best-effort)
            #[cfg(feature = "plugins")]
            {
                if let Some(store) = &plugin.wasm_store {
                    let state = store.data();
                    let memory = state.memory.clone();
                    self.save_plugin_state(plugin_id, &memory).await;
                }
            }
            // Unregister metrics for this plugin
            self.metrics.unregister(plugin_id).await;
            info!("Unloaded plugin '{}'", plugin.manifest.name);
            Ok(true)
        } else {
            Ok(false)
        }
    }

    /// Reload a plugin while preserving its runtime state (memory).
    ///
    /// Re-reads the manifest from disk so changes to `plugin.json` are picked
    /// up, then re-compiles and re-instantiates the WASM module, injecting
    /// the previously stored `PluginState::memory` into the new instance.
    pub async fn reload_plugin(&self, plugin_id: &str) -> crate::Result<String> {
        let mut plugins = self.plugins.write().await;
        let existing =
            plugins
                .get_mut(plugin_id)
                .ok_or_else(|| crate::error::ConfigError::InvalidValue {
                    key: "plugin_id".to_string(),
                    message: format!("Plugin '{}' not found", plugin_id),
                })?;

        let path = existing.path.clone();

        // Extract preserved memory before dropping the old store.
        let preserved_memory = if cfg!(feature = "plugins") {
            if let Some(store) = existing.wasm_store.take() {
                let state = store.into_data();
                let memory = state.memory.read().await.clone();
                Some(memory)
            } else {
                None
            }
        } else {
            None
        };

        drop(plugins);

        // Re-read manifest from disk to pick up edits.
        let manifest_path = path.join("plugin.json");
        let manifest_content = tokio::fs::read_to_string(&manifest_path)
            .await
            .map_err(|e| crate::error::SyscityError::ExternalService {
                source: "Failed to read plugin manifest".to_string(),
                cause: Some(Box::new(e)),
            })?;

        let manifest: PluginManifest = serde_json::from_str(&manifest_content).map_err(|e| {
            crate::error::ConfigError::InvalidValue {
                key: "plugin.json".to_string(),
                message: format!("Invalid plugin manifest: {}", e),
            }
        })?;

        // Validate manifest version fields (non-fatal warnings)
        #[cfg(feature = "plugins")]
        Self::validate_manifest_version(&manifest);

        // Load config
        let config_path = path.join("config.json");
        let config = if config_path.exists() {
            let config_content = match tokio::fs::read_to_string(&config_path).await {
                Ok(c) => c,
                Err(e) => {
                    warn!("Failed to read plugin config from {:?}: {}", config_path, e);
                    String::new()
                }
            };
            serde_json::from_str(&config_content).unwrap_or_else(|e| {
                warn!("Failed to parse plugin config from {:?}: {}", config_path, e);
                serde_json::json!({})
            })
        } else {
            manifest.config.clone().unwrap_or(serde_json::json!({}))
        };

        // Re-compile WASM with preserved memory.
        #[cfg(feature = "plugins")]
        let (wasm_store, instance) = {
            if let Some(ref main) = manifest.main {
                let wasm_path = path.join(main);
                if wasm_path.exists() {
                    let permissions = manifest.permissions.clone().unwrap_or_default();
                    self.load_wasm_plugin(
                        &wasm_path,
                        config.clone(),
                        plugin_id,
                        preserved_memory,
                        permissions,
                    )
                    .await?
                } else {
                    warn!("WASM file not found: {:?}", wasm_path);
                    (None, None)
                }
            } else {
                (None, None)
            }
        };

        #[cfg(not(feature = "plugins"))]
        let (wasm_store, instance) = (None, None);

        let new_instance = PluginInstance {
            manifest,
            path,
            enabled: true,
            config,
            #[cfg(feature = "plugins")]
            wasm_store,
            #[cfg(feature = "plugins")]
            instance,
        };

        let mut plugins = self.plugins.write().await;
        plugins.insert(plugin_id.to_string(), new_instance);

        info!("Plugin '{}' reloaded successfully", plugin_id);
        Ok(plugin_id.to_string())
    }

    /// Get a plugin instance
    pub async fn get_plugin(&self, plugin_id: &str) -> Option<PluginInstance> {
        let plugins = self.plugins.read().await;
        plugins.get(plugin_id).cloned()
    }

    /// List all loaded plugins
    pub async fn list_plugins(&self) -> Vec<PluginInstance> {
        let plugins = self.plugins.read().await;
        plugins.values().cloned().collect()
    }

    /// Enable/disable a plugin
    pub async fn set_enabled(&self, plugin_id: &str, enabled: bool) -> crate::Result<()> {
        let mut plugins = self.plugins.write().await;

        if let Some(plugin) = plugins.get_mut(plugin_id) {
            plugin.enabled = enabled;
            info!("Plugin '{}' {}", plugin_id, if enabled { "enabled" } else { "disabled" });
            Ok(())
        } else {
            Err(crate::error::ConfigError::InvalidValue {
                key: "plugin_id".to_string(),
                message: format!("Plugin '{}' not found", plugin_id),
            }
            .into())
        }
    }

    /// Call a tool provided by a plugin.
    ///
    /// The guest module is expected to export either:
    ///  - `call_tool(name_ptr: i32, name_len: i32, params_ptr: i32, params_len:
    ///    i32, out_ptr: i32, out_max: i32) -> i32`  (generic dispatcher), or
    ///  - `{tool_name}(params_ptr: i32, params_len: i32, out_ptr: i32, out_max:
    ///    i32) -> i32` (tool-specific function).
    ///
    /// The return value is the number of bytes written to `out_ptr`, or a
    /// negative value on error.  Both the input params and the output
    /// buffer are managed via the guest's `alloc(size: i32) -> i32` export
    /// when present.
    ///
    /// Params and results are JSON-encoded strings.
    pub async fn call_tool(
        &self,
        plugin_id: &str,
        tool_name: &str,
        params: serde_json::Value,
    ) -> crate::Result<serde_json::Value> {
        // We need a write lock so we can get `&mut Store` for WASM calls.
        let mut plugins = self.plugins.write().await;

        let plugin =
            plugins
                .get_mut(plugin_id)
                .ok_or_else(|| crate::error::ConfigError::InvalidValue {
                    key: "plugin_id".to_string(),
                    message: format!("Plugin '{}' not found", plugin_id),
                })?;

        if !plugin.enabled {
            return Err(crate::error::SyscityError::Validation(format!(
                "Plugin '{}' is disabled",
                plugin_id
            )));
        }

        #[cfg(feature = "plugins")]
        {
            let (store, instance) = match (&mut plugin.wasm_store, &plugin.instance) {
                (Some(s), Some(i)) => (s, i),
                _ => {
                    return Err(crate::error::SyscityError::Internal(format!(
                        "Plugin '{}' has no WASM module loaded",
                        plugin_id
                    )));
                }
            };

            // Enforce a 60-second timeout on WASM tool invocations as a
            // secondary safety net alongside fuel metering.
            // Note: wasmtime's synchronous f.call() blocks the tokio thread,
            // so fuel metering is the primary protection; the timeout provides
            // an additional upper bound for the caller.
            let timeout_future =
                async { Self::invoke_wasm_tool(store, instance, tool_name, params) };
            tokio::time::timeout(std::time::Duration::from_secs(60), timeout_future)
                .await
                .map_err(|_| {
                    crate::error::SyscityError::Internal(format!(
                        "Plugin '{}' tool call timed out after 60 seconds",
                        plugin_id
                    ))
                })?
        }

        #[cfg(not(feature = "plugins"))]
        Err(crate::error::SyscityError::Internal(
            "Plugin execution requires the `plugins` feature. Recompile Syscity with `--features \
             plugins` to enable WASM plugin support."
                .to_string(),
        ))
    }

    /// Low-level WASM tool invocation.
    ///
    /// Writes the tool name and JSON-encoded params into guest memory (via the
    /// guest's `alloc` export), calls either the generic `call_tool` dispatcher
    /// or a per-tool export, then reads the JSON result back from guest memory.
    #[cfg(feature = "plugins")]
    fn invoke_wasm_tool(
        store: &mut wasmtime::Store<PluginState>,
        instance: &wasmtime::Instance,
        tool_name: &str,
        params: serde_json::Value,
    ) -> crate::Result<serde_json::Value> {
        const OUT_MAX: i32 = 65_536; // 64 KiB output buffer

        let params_json = serde_json::to_string(&params)
            .map_err(|e| crate::error::SyscityError::Internal(e.to_string()))?;
        let tool_bytes = tool_name.as_bytes();
        let params_bytes = params_json.as_bytes();

        // Resolve the guest's linear memory.
        let memory = instance
            .get_export(&mut *store, "memory")
            .and_then(|e| e.into_memory())
            .ok_or_else(|| {
                crate::error::SyscityError::Internal(
                    "Plugin WASM module has no 'memory' export".to_string(),
                )
            })?;

        // Resolve the optional `alloc` export.  TypedFunc is Copy so we can
        // use it multiple times without re-borrowing.
        let alloc_fn: Option<wasmtime::TypedFunc<i32, i32>> = instance
            .get_typed_func::<i32, i32>(&mut *store, "alloc")
            .ok();

        // Allocate and write the tool name.
        let name_len = tool_bytes.len() as i32;
        let name_ptr = if let Some(ref f) = alloc_fn {
            f.call(&mut *store, name_len)
                .map_err(|e| crate::error::SyscityError::Internal(format!("alloc: {}", e)))?
        } else {
            0i32
        };
        if name_ptr != 0 {
            let data = memory.data_mut(&mut *store);
            data[name_ptr as usize..name_ptr as usize + tool_bytes.len()]
                .copy_from_slice(tool_bytes);
        }

        // Allocate and write the JSON params.
        let params_len = params_bytes.len() as i32;
        let params_ptr = if let Some(ref f) = alloc_fn {
            f.call(&mut *store, params_len)
                .map_err(|e| crate::error::SyscityError::Internal(format!("alloc: {}", e)))?
        } else {
            0i32
        };
        if params_ptr != 0 {
            let data = memory.data_mut(&mut *store);
            data[params_ptr as usize..params_ptr as usize + params_bytes.len()]
                .copy_from_slice(params_bytes);
        }

        // Allocate the output buffer.
        let out_ptr = if let Some(ref f) = alloc_fn {
            f.call(&mut *store, OUT_MAX)
                .map_err(|e| crate::error::SyscityError::Internal(format!("alloc output: {}", e)))?
        } else {
            0i32
        };

        // Try the generic `call_tool` dispatcher first.
        let written: i32 = if let Ok(f) =
            instance.get_typed_func::<(i32, i32, i32, i32, i32, i32), i32>(&mut *store, "call_tool")
        {
            f.call(&mut *store, (name_ptr, name_len, params_ptr, params_len, out_ptr, OUT_MAX))
                .map_err(|e| crate::error::SyscityError::Internal(format!("call_tool: {}", e)))?
        } else if let Ok(f) =
            instance.get_typed_func::<(i32, i32, i32, i32), i32>(&mut *store, tool_name)
        {
            // Fall back to a per-tool export.
            f.call(&mut *store, (params_ptr, params_len, out_ptr, OUT_MAX))
                .map_err(|e| {
                    crate::error::SyscityError::Internal(format!("tool '{}': {}", tool_name, e))
                })?
        } else {
            return Err(crate::error::SyscityError::Internal(format!(
                "Plugin does not export 'call_tool' or '{}' function",
                tool_name
            )));
        };

        if written < 0 {
            return Err(crate::error::SyscityError::Internal(format!(
                "Plugin tool '{}' returned error code {}",
                tool_name, written
            )));
        }

        // Read the result JSON from the output buffer.
        let result_bytes = {
            let data = memory.data(&store);
            let start = out_ptr as usize;
            let end = start + written as usize;
            data[start..end].to_vec()
        };

        let result_str = std::str::from_utf8(&result_bytes).map_err(|e| {
            crate::error::SyscityError::Internal(format!("Plugin returned invalid UTF-8: {}", e))
        })?;

        let result: serde_json::Value = serde_json::from_str(result_str)
            .unwrap_or_else(|_| serde_json::json!({ "output": result_str }));

        debug!("Plugin tool '{}' executed successfully ({} bytes)", tool_name, written);
        Ok(result)
    }

    // ------------------------------------------------------------------
    // Provider delegation stubs
    // ------------------------------------------------------------------

    /// Look up a plugin, check it is enabled and has a WASM module loaded,
    /// returning `(&mut Store, &Instance)`.
    ///
    /// Shared boilerplate for `call_provider_complete`, `call_provider_stream`,
    /// and `call_provider_health_check`.
    #[cfg(feature = "plugins")]
    fn get_plugin_store_instance<'a>(
        plugins: &'a mut HashMap<String, PluginInstance>,
        plugin_id: &str,
    ) -> crate::Result<(&'a mut wasmtime::Store<PluginState>, &'a wasmtime::Instance)> {
        let plugin =
            plugins
                .get_mut(plugin_id)
                .ok_or_else(|| crate::error::ConfigError::InvalidValue {
                    key: "plugin_id".to_string(),
                    message: format!("Plugin '{}' not found", plugin_id),
                })?;

        if !plugin.enabled {
            return Err(crate::error::SyscityError::Validation(format!(
                "Plugin '{}' is disabled",
                plugin_id
            )));
        }

        match (&mut plugin.wasm_store, &plugin.instance) {
            (Some(s), Some(i)) => Ok((s, i)),
            _ => Err(crate::error::SyscityError::Internal(format!(
                "Plugin '{}' has no WASM module loaded",
                plugin_id
            ))),
        }
    }

    /// Call a plugin's provider `complete` implementation.
    ///
    /// The plugin must export `provider_complete(request_ptr, request_len,
    /// out_ptr, out_max) -> i32`.
    pub async fn call_provider_complete(
        &self,
        plugin_id: &str,
        request: &serde_json::Value,
    ) -> crate::Result<serde_json::Value> {
        #[cfg(feature = "plugins")]
        {
            let mut plugins = self.plugins.write().await;
            let (store, instance) = Self::get_plugin_store_instance(&mut plugins, plugin_id)?;
            Self::invoke_wasm_provider(store, instance, "provider_complete", request)
        }

        #[cfg(not(feature = "plugins"))]
        Err(crate::error::SyscityError::Internal(
            "Plugin execution requires the `plugins` feature. Recompile Syscity with `--features \
             plugins` to enable WASM plugin support."
                .to_string(),
        ))
    }

    /// Call a plugin's provider `stream` implementation.
    ///
    /// The plugin must export `provider_stream(request_ptr, request_len,
    /// out_ptr, out_max) -> i32`. Returns a JSON array of CompletionChunk
    /// objects.
    pub async fn call_provider_stream(
        &self,
        plugin_id: &str,
        request: &serde_json::Value,
    ) -> crate::Result<serde_json::Value> {
        #[cfg(feature = "plugins")]
        {
            let mut plugins = self.plugins.write().await;
            let (store, instance) = Self::get_plugin_store_instance(&mut plugins, plugin_id)?;
            Self::invoke_wasm_provider(store, instance, "provider_stream", request)
        }

        #[cfg(not(feature = "plugins"))]
        Err(crate::error::SyscityError::Internal(
            "Plugin execution requires the `plugins` feature. Recompile Syscity with `--features \
             plugins` to enable WASM plugin support."
                .to_string(),
        ))
    }

    /// Call a plugin's provider `health_check` implementation.
    ///
    /// The plugin must export `provider_health_check(out_ptr, out_max) -> i32`.
    pub async fn call_provider_health_check(
        &self,
        plugin_id: &str,
    ) -> crate::Result<serde_json::Value> {
        #[cfg(feature = "plugins")]
        {
            let mut plugins = self.plugins.write().await;
            let (store, instance) = Self::get_plugin_store_instance(&mut plugins, plugin_id)?;
            Self::invoke_wasm_provider(
                store,
                instance,
                "provider_health_check",
                &serde_json::json!({}),
            )
        }

        #[cfg(not(feature = "plugins"))]
        Err(crate::error::SyscityError::Internal(
            "Plugin execution requires the `plugins` feature. Recompile Syscity with `--features \
             plugins` to enable WASM plugin support."
                .to_string(),
        ))
    }

    /// Low-level WASM provider invocation.
    ///
    /// Note: WASM linear memory allocated via `alloc` is reclaimed when the
    /// store/instance is dropped (no explicit `dealloc` is needed — the WASM
    /// linear memory is owned by the store). This is consistent with
    /// `invoke_wasm_tool` which follows the same pattern.
    #[cfg(feature = "plugins")]
    fn invoke_wasm_provider(
        store: &mut wasmtime::Store<PluginState>,
        instance: &wasmtime::Instance,
        export_name: &str,
        request: &serde_json::Value,
    ) -> crate::Result<serde_json::Value> {
        const OUT_MAX: i32 = 256_000; // 256 KiB output buffer

        let request_json = serde_json::to_string(request)
            .map_err(|e| crate::error::SyscityError::Internal(e.to_string()))?;
        let request_bytes = request_json.as_bytes();

        let memory = instance
            .get_export(&mut *store, "memory")
            .and_then(|e| e.into_memory())
            .ok_or_else(|| {
                crate::error::SyscityError::Internal(
                    "Plugin WASM module has no 'memory' export".to_string(),
                )
            })?;

        let alloc_fn: Option<wasmtime::TypedFunc<i32, i32>> = instance
            .get_typed_func::<i32, i32>(&mut *store, "alloc")
            .ok();

        let req_len = request_bytes.len() as i32;
        let req_ptr = if let Some(ref f) = alloc_fn {
            f.call(&mut *store, req_len)
                .map_err(|e| crate::error::SyscityError::Internal(format!("alloc: {}", e)))?
        } else {
            0i32
        };
        if req_ptr != 0 {
            let data = memory.data_mut(&mut *store);
            data[req_ptr as usize..req_ptr as usize + request_bytes.len()]
                .copy_from_slice(request_bytes);
        }

        let out_ptr = if let Some(ref f) = alloc_fn {
            f.call(&mut *store, OUT_MAX)
                .map_err(|e| crate::error::SyscityError::Internal(format!("alloc output: {}", e)))?
        } else {
            0i32
        };

        let written: i32 = if let Ok(f) =
            instance.get_typed_func::<(i32, i32, i32, i32), i32>(&mut *store, export_name)
        {
            f.call(&mut *store, (req_ptr, req_len, out_ptr, OUT_MAX))
                .map_err(|e| {
                    crate::error::SyscityError::Internal(format!("{}: {}", export_name, e))
                })?
        } else {
            return Err(crate::error::SyscityError::Internal(format!(
                "Plugin does not export '{}' function",
                export_name
            )));
        };

        if written < 0 {
            return Err(crate::error::SyscityError::Internal(format!(
                "Plugin provider '{}' returned error code {}",
                export_name, written
            )));
        }

        let result_bytes = {
            let data = memory.data(&store);
            let start = out_ptr as usize;
            let end = start + written as usize;
            data[start..end].to_vec()
        };

        let result_str = std::str::from_utf8(&result_bytes).map_err(|e| {
            crate::error::SyscityError::Internal(format!("Plugin returned invalid UTF-8: {}", e))
        })?;

        let result: serde_json::Value = serde_json::from_str(result_str)
            .unwrap_or_else(|_| serde_json::json!({ "output": result_str }));

        debug!("Plugin provider '{}' executed successfully ({} bytes)", export_name, written);
        Ok(result)
    }

    /// Shutdown all plugins
    pub async fn shutdown(&self) -> crate::Result<()> {
        let mut plugins = self.plugins.write().await;

        for (id, plugin) in plugins.drain() {
            // Save plugin state to disk before shutting down (best-effort)
            #[cfg(feature = "plugins")]
            {
                if let Some(store) = &plugin.wasm_store {
                    let state = store.data();
                    let memory = state.memory.clone();
                    self.save_plugin_state(&id, &memory).await;
                }
            }
            info!("Shutting down plugin '{}'", id);
        }

        Ok(())
    }
}

#[cfg(feature = "plugins")]
impl Drop for PluginRuntime {
    fn drop(&mut self) {
        if let Some(handle) = self.event_dispatch_handle.take() {
            handle.abort();
        }
    }
}

impl PluginRuntime {
    /// Get the metrics registry for this runtime.
    pub fn metrics(&self) -> &Arc<PluginMetricsRegistry> {
        &self.metrics
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_manifest(id: &str) -> PluginManifest {
        PluginManifest {
            id: id.to_string(),
            name: format!("Test Plugin {}", id),
            version: "1.0.0".to_string(),
            syscity_version: None,
            description: "A test plugin".to_string(),
            author: None,
            main: None,
            capabilities: None,
            permissions: None,
            config: None,
            triggers: None,
            dependencies: None,
            repository: None,
            registry: None,
            signature: None,
            signer_public_key: None,
            external_resources: None,
        }
    }

    fn test_instance(id: &str) -> PluginInstance {
        PluginInstance {
            manifest: test_manifest(id),
            path: std::path::PathBuf::from("/tmp/test-plugin"),
            enabled: true,
            config: serde_json::json!({}),
            #[cfg(feature = "plugins")]
            wasm_store: None,
            #[cfg(feature = "plugins")]
            instance: None,
        }
    }

    #[test]
    fn test_plugin_runtime_new() {
        let runtime = PluginRuntime::new(std::env::temp_dir().join("syscity-plugin-tests"));
        assert!(runtime.is_ok());
    }

    #[test]
    fn test_plugin_instance_getters() {
        let instance = test_instance("com.test.plugin");
        assert_eq!(instance.id(), "com.test.plugin");
        assert_eq!(instance.name(), "Test Plugin com.test.plugin");
    }

    #[test]
    fn test_plugin_instance_clone_drops_wasm() {
        let instance = test_instance("com.test.plugin");
        let cloned = instance.clone();
        assert_eq!(cloned.id(), "com.test.plugin");
        assert_eq!(cloned.enabled, true);
        #[cfg(feature = "plugins")]
        {
            assert!(cloned.wasm_store.is_none());
            assert!(cloned.instance.is_none());
        }
    }

    #[tokio::test]
    async fn test_load_plugin_from_directory() {
        let temp_dir = tempfile::tempdir().unwrap();
        let manifest = serde_json::json!({
            "id": "com.test.loader",
            "name": "Loader Test",
            "version": "0.1.0",
            "description": "Testing load_plugin"
        });
        tokio::fs::write(
            temp_dir.path().join("plugin.json"),
            serde_json::to_string_pretty(&manifest).unwrap(),
        )
        .await
        .unwrap();

        let runtime =
            PluginRuntime::new(std::env::temp_dir().join("syscity-plugin-tests")).unwrap();
        let plugin_id = runtime.load_plugin(temp_dir.path()).await.unwrap();
        assert_eq!(plugin_id, "com.test.loader");

        let plugin = runtime.get_plugin("com.test.loader").await.unwrap();
        assert_eq!(plugin.name(), "Loader Test");
    }

    #[tokio::test]
    async fn test_load_plugin_missing_manifest() {
        let temp_dir = tempfile::tempdir().unwrap();
        let runtime =
            PluginRuntime::new(std::env::temp_dir().join("syscity-plugin-tests")).unwrap();

        let result = runtime.load_plugin(temp_dir.path()).await;
        assert!(result.is_err());
        let err_str = result.unwrap_err().to_string();
        assert!(err_str.contains("Plugin manifest not found"));
    }

    #[tokio::test]
    async fn test_get_plugin_not_found() {
        let runtime =
            PluginRuntime::new(std::env::temp_dir().join("syscity-plugin-tests")).unwrap();
        let plugin = runtime.get_plugin("nonexistent").await;
        assert!(plugin.is_none());
    }

    #[tokio::test]
    async fn test_list_plugins_empty() {
        let runtime =
            PluginRuntime::new(std::env::temp_dir().join("syscity-plugin-tests")).unwrap();
        let plugins = runtime.list_plugins().await;
        assert!(plugins.is_empty());
    }

    #[tokio::test]
    async fn test_list_plugins_after_load() {
        let temp_dir = tempfile::tempdir().unwrap();
        let manifest = serde_json::json!({
            "id": "com.test.list",
            "name": "List Test",
            "version": "0.1.0",
            "description": "Testing list_plugins"
        });
        tokio::fs::write(
            temp_dir.path().join("plugin.json"),
            serde_json::to_string_pretty(&manifest).unwrap(),
        )
        .await
        .unwrap();

        let runtime =
            PluginRuntime::new(std::env::temp_dir().join("syscity-plugin-tests")).unwrap();
        runtime.load_plugin(temp_dir.path()).await.unwrap();

        let plugins = runtime.list_plugins().await;
        assert_eq!(plugins.len(), 1);
        assert_eq!(plugins[0].id(), "com.test.list");
    }

    #[tokio::test]
    async fn test_set_enabled() {
        let temp_dir = tempfile::tempdir().unwrap();
        let manifest = serde_json::json!({
            "id": "com.test.toggle",
            "name": "Toggle Test",
            "version": "0.1.0",
            "description": "Testing set_enabled"
        });
        tokio::fs::write(
            temp_dir.path().join("plugin.json"),
            serde_json::to_string_pretty(&manifest).unwrap(),
        )
        .await
        .unwrap();

        let runtime =
            PluginRuntime::new(std::env::temp_dir().join("syscity-plugin-tests")).unwrap();
        runtime.load_plugin(temp_dir.path()).await.unwrap();

        // Disable
        runtime.set_enabled("com.test.toggle", false).await.unwrap();
        let plugin = runtime.get_plugin("com.test.toggle").await.unwrap();
        assert!(!plugin.enabled);

        // Enable
        runtime.set_enabled("com.test.toggle", true).await.unwrap();
        let plugin = runtime.get_plugin("com.test.toggle").await.unwrap();
        assert!(plugin.enabled);
    }

    #[tokio::test]
    async fn test_set_enabled_not_found() {
        let runtime =
            PluginRuntime::new(std::env::temp_dir().join("syscity-plugin-tests")).unwrap();
        let result = runtime.set_enabled("nonexistent", false).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_unload_plugin() {
        let temp_dir = tempfile::tempdir().unwrap();
        let manifest = serde_json::json!({
            "id": "com.test.unload",
            "name": "Unload Test",
            "version": "0.1.0",
            "description": "Testing unload_plugin"
        });
        tokio::fs::write(
            temp_dir.path().join("plugin.json"),
            serde_json::to_string_pretty(&manifest).unwrap(),
        )
        .await
        .unwrap();

        let runtime =
            PluginRuntime::new(std::env::temp_dir().join("syscity-plugin-tests")).unwrap();
        runtime.load_plugin(temp_dir.path()).await.unwrap();
        assert!(runtime.get_plugin("com.test.unload").await.is_some());

        let removed = runtime.unload_plugin("com.test.unload").await.unwrap();
        assert!(removed);
        assert!(runtime.get_plugin("com.test.unload").await.is_none());
    }

    #[tokio::test]
    async fn test_unload_plugin_not_found() {
        let runtime =
            PluginRuntime::new(std::env::temp_dir().join("syscity-plugin-tests")).unwrap();
        let removed = runtime.unload_plugin("nonexistent").await.unwrap();
        assert!(!removed);
    }

    #[tokio::test]
    async fn test_shutdown() {
        let temp_dir = tempfile::tempdir().unwrap();
        let manifest = serde_json::json!({
            "id": "com.test.shutdown",
            "name": "Shutdown Test",
            "version": "0.1.0",
            "description": "Testing shutdown"
        });
        tokio::fs::write(
            temp_dir.path().join("plugin.json"),
            serde_json::to_string_pretty(&manifest).unwrap(),
        )
        .await
        .unwrap();

        let runtime =
            PluginRuntime::new(std::env::temp_dir().join("syscity-plugin-tests")).unwrap();
        runtime.load_plugin(temp_dir.path()).await.unwrap();

        let result = runtime.shutdown().await;
        assert!(result.is_ok());

        // After shutdown, plugin list should be empty
        let plugins = runtime.list_plugins().await;
        assert!(plugins.is_empty());
    }

    #[test]
    fn test_plugin_state_new() {
        let shared_state = Arc::new(PluginSharedState::new());
        let state = PluginState::new(
            serde_json::json!({"key": "value"}),
            shared_state,
            "test.plugin".to_string(),
            vec![],
        );
        assert_eq!(state.config["key"], "value");
    }

    #[test]
    fn test_plugin_state_new_with_memory() {
        let shared_state = Arc::new(PluginSharedState::new());
        let mut memory = HashMap::new();
        memory.insert("data".to_string(), vec![1, 2, 3]);
        let state = PluginState::new_with_memory(
            serde_json::json!({}),
            memory,
            shared_state,
            "test.plugin".to_string(),
            vec![],
        );
        let rt = tokio::runtime::Runtime::new().unwrap();
        let stored = rt.block_on(async {
            let m = state.memory.read().await;
            m.get("data").cloned()
        });
        assert_eq!(stored, Some(vec![1, 2, 3]));
    }
}
