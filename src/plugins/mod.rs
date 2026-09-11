//! Plugin System for Syscity
//!
//! Provides runtime extensibility:
//! - WASM-based sandboxed plugins
//! - Tool registration from plugins
//! - Channel plugins
//! - Hooks system for extending behavior
//! - Hot loading/unloading
// INVARIANTS-NONE: wasm guests are sandboxed at instantiation (fuel metering, no preopens); nothing persists between runs.

pub mod activation;
pub mod deps;
pub mod hooks;
pub mod installer;
pub mod manifest;
pub mod metrics;
pub mod provider_extension;
pub mod registry;
pub mod runtime;
pub mod sqlite_registry;
pub mod verification;

use std::path::PathBuf;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, OnceLock};

pub use activation::{ActivationPlan, ActivationPlanner, PluginTrigger};
pub use deps::{DependencyResolver, ResolvedDependency};
pub use hooks::{
    HookExecutionResult, HookHandler, HookHandlerBuilder, HookPayload, HookRegistry, HookResult,
    HookType,
};
pub use installer::PluginInstaller;
pub use manifest::{
    ExternalResource, PluginArg, PluginCapability, PluginCommand, PluginManifest, PluginPermission,
    PluginTool,
};
pub use metrics::{MetricsSnapshot, PluginMetrics, PluginMetricsRegistry};
pub use provider_extension::{PluginProvider, PluginProviderRegistry};
pub use registry::{RegistryClient, RegistryIndex, RegistryPluginEntry};
pub use runtime::{PluginInstance, PluginRuntime};
pub use sqlite_registry::{PluginDbEntry, PluginSqliteRegistry};
use tracing::{debug, info, warn};
pub use verification::{verify_manifest, VerificationResult};

use crate::cli::{DiagnosticHint, HintSeverity};
use crate::tools::ToolRegistry;

/// Callback type for registering a plugin-backed provider with the system.
pub type ProviderRegisterFn =
    Arc<dyn Fn(String, Arc<dyn crate::providers::Provider + Send + Sync>) + Send + Sync>;

/// Callback type for unregistering a plugin-backed provider.
pub type ProviderUnregisterFn = Arc<dyn Fn(String) + Send + Sync>;

/// Callback type for registering a plugin-backed channel with the system.
#[cfg(feature = "plugins")]
pub type ChannelRegisterFn =
    Arc<dyn Fn(String, Arc<dyn crate::channels::Channel + Send + Sync>) + Send + Sync>;

/// Callback type for unregistering a plugin-backed channel.
#[cfg(feature = "plugins")]
pub type ChannelUnregisterFn = Arc<dyn Fn(String) + Send + Sync>;

/// Plugin manager - high-level interface for plugin operations
pub struct PluginManager {
    runtime: Arc<PluginRuntime>,
    hook_registry: Arc<HookRegistry>,
    plugins_dir: PathBuf,
    tool_registry: OnceLock<Arc<ToolRegistry>>,
    trace_enabled: Arc<AtomicBool>,
    provider_register: OnceLock<ProviderRegisterFn>,
    provider_unregister: OnceLock<ProviderUnregisterFn>,
    /// Callbacks for registering/unregistering plugin channels
    #[cfg(feature = "plugins")]
    channel_register: OnceLock<ChannelRegisterFn>,
    #[cfg(feature = "plugins")]
    channel_unregister: OnceLock<ChannelUnregisterFn>,
    /// Message sender used to construct PluginChannel instances
    #[cfg(feature = "plugins")]
    channel_message_tx:
        OnceLock<tokio::sync::mpsc::UnboundedSender<crate::channels::IncomingMessage>>,
    /// Optional SQLite plugin registry for persistent metadata
    sqlite_registry: OnceLock<PluginSqliteRegistry>,
    /// Optional activation planner for lazy loading / dependency ordering
    activation_planner: OnceLock<ActivationPlanner>,
}

impl PluginManager {
    /// Create a new plugin manager
    pub async fn new(plugins_dir: PathBuf) -> crate::Result<Self> {
        // `plugins_data_dir()` is defined as `<root>/plugins/data`, and this manager
        // is constructed with `<root>/plugins`, so the state root is derived here
        // rather than read from the process global.
        let runtime = Arc::new(PluginRuntime::new(plugins_dir.join("data"))?);
        let hook_registry = Arc::new(HookRegistry::new());

        // Ensure plugins directory exists
        if let Err(e) = tokio::fs::create_dir_all(&plugins_dir).await {
            warn!("Failed to create plugins directory {:?}: {}", plugins_dir, e);
        }

        Ok(Self {
            runtime,
            hook_registry,
            plugins_dir,
            tool_registry: OnceLock::new(),
            trace_enabled: Arc::new(AtomicBool::new(false)),
            provider_register: OnceLock::new(),
            provider_unregister: OnceLock::new(),
            #[cfg(feature = "plugins")]
            channel_register: OnceLock::new(),
            #[cfg(feature = "plugins")]
            channel_unregister: OnceLock::new(),
            #[cfg(feature = "plugins")]
            channel_message_tx: OnceLock::new(),
            sqlite_registry: OnceLock::new(),
            activation_planner: OnceLock::new(),
        })
    }

    /// Set callbacks for registering / unregistering plugin-backed providers.
    pub fn set_provider_callbacks(
        &self,
        register: ProviderRegisterFn,
        unregister: ProviderUnregisterFn,
    ) {
        self.provider_register.set(register).ok();
        self.provider_unregister.set(unregister).ok();
    }

    /// Set callbacks for registering / unregistering plugin-backed channels.
    #[cfg(feature = "plugins")]
    pub fn set_channel_callbacks(
        &self,
        register: ChannelRegisterFn,
        unregister: ChannelUnregisterFn,
    ) {
        self.channel_register.set(register).ok();
        self.channel_unregister.set(unregister).ok();
    }

    /// Set the channel message sender used to construct PluginChannel
    /// instances.
    #[cfg(feature = "plugins")]
    pub fn set_channel_message_tx(
        &self,
        tx: tokio::sync::mpsc::UnboundedSender<crate::channels::IncomingMessage>,
    ) {
        self.channel_message_tx.set(tx).ok();
    }

    /// Attach a `ToolRegistry` so that plugin tools are automatically
    /// registered on load / unregistered on unload.
    pub fn set_tool_registry(&self, registry: Arc<ToolRegistry>) {
        self.tool_registry.set(registry).ok();
    }

    /// Initialize and load all plugins
    pub async fn initialize(&self) -> crate::Result<usize> {
        info!("Initializing plugin manager...");

        let mut entries = tokio::fs::read_dir(&self.plugins_dir).await?;
        let mut count = 0;

        while let Some(entry) = entries.next_entry().await? {
            let path = entry.path();

            if path.is_dir() {
                let manifest_path = path.join("plugin.json");
                if manifest_path.exists() {
                    match self.load_plugin(&path).await {
                        Ok(plugin_id) => {
                            debug!("Auto-loaded plugin '{}'", plugin_id);
                            count += 1;
                        }
                        Err(e) => {
                            warn!("Failed to load plugin from {:?}: {}", path, e);
                        }
                    }
                }
            }
        }

        // Sync filesystem plugins into SQLite registry if available
        self.sync_filesystem_to_registry().await;

        info!("Loaded {} plugin(s)", count);
        Ok(count)
    }

    /// Load a plugin from a directory and register its tools, providers, and
    /// channels.
    pub async fn load_plugin(&self, path: &std::path::Path) -> crate::Result<String> {
        let plugin_id = self.runtime.load_plugin(path).await?;

        if let Some(plugin) = self.runtime.get_plugin(&plugin_id).await {
            self.register_plugin_tools(&plugin).await;
            #[cfg(feature = "plugins")]
            self.register_plugin_providers(&plugin).await;
            #[cfg(feature = "plugins")]
            self.register_plugin_channels(&plugin).await;

            // Sync to SQLite registry if available
            if let Some(reg) = self.sqlite_registry.get() {
                if let Err(e) = reg.register_plugin(&plugin.manifest, path, None).await {
                    warn!("Failed to register plugin '{}' in SQLite registry: {}", plugin_id, e);
                }
            }
        }

        Ok(plugin_id)
    }

    /// Unload a plugin, unregistering its tools, providers, channels, and
    /// hooks.
    pub async fn unload_plugin(&self, plugin_id: &str) -> crate::Result<bool> {
        self.deregister_plugin_tools(plugin_id).await;
        #[cfg(feature = "plugins")]
        self.deregister_plugin_channels(plugin_id).await;
        #[cfg(feature = "plugins")]
        self.deregister_plugin_providers(plugin_id).await;
        self.hook_registry.unregister_plugin(plugin_id).await;

        // Unregister from SQLite registry if available
        if let Some(reg) = self.sqlite_registry.get() {
            if let Err(e) = reg.unregister_plugin(plugin_id).await {
                warn!("Failed to unregister plugin '{}' from SQLite registry: {}", plugin_id, e);
            }
        }

        self.runtime.unload_plugin(plugin_id).await
    }

    /// Reload a plugin with state preservation.
    ///
    /// Preserves `PluginState::memory`, re-reads the manifest from disk,
    /// and re-registers tools into the `ToolRegistry`.
    pub async fn reload_plugin(&self, plugin_id: &str) -> crate::Result<String> {
        info!("Reloading plugin '{}'...", plugin_id);

        self.deregister_plugin_tools(plugin_id).await;
        #[cfg(feature = "plugins")]
        self.deregister_plugin_channels(plugin_id).await;
        #[cfg(feature = "plugins")]
        self.deregister_plugin_providers(plugin_id).await;
        self.hook_registry.unregister_plugin(plugin_id).await;

        let reloaded_id = self.runtime.reload_plugin(plugin_id).await?;

        if let Some(plugin) = self.runtime.get_plugin(&reloaded_id).await {
            self.register_plugin_tools(&plugin).await;
            #[cfg(feature = "plugins")]
            self.register_plugin_channels(&plugin).await;
            #[cfg(feature = "plugins")]
            self.register_plugin_providers(&plugin).await;
        }

        info!("Plugin '{}' reloaded successfully", reloaded_id);
        Ok(reloaded_id)
    }

    /// Enable or disable plugin trace logging.
    pub fn set_trace_enabled(&self, enabled: bool) {
        self.trace_enabled
            .store(enabled, std::sync::atomic::Ordering::Relaxed);
    }

    /// Register a plugin's tools into the `ToolRegistry`.
    async fn register_plugin_tools(&self, plugin: &PluginInstance) {
        if let Some(registry) = self.tool_registry.get() {
            for tool in plugin.manifest.get_tools() {
                let wrapper = Arc::new(PluginToolWrapper::new(
                    plugin.id().to_string(),
                    tool,
                    self.runtime.clone(),
                    self.trace_enabled.clone(),
                ));
                registry.register_dynamic(wrapper);
                info!("Registered plugin tool '{}' from plugin '{}'", tool.name, plugin.id());
            }
        }
    }

    /// Deregister a plugin's tools from the `ToolRegistry`.
    async fn deregister_plugin_tools(&self, plugin_id: &str) {
        if let Some(registry) = self.tool_registry.get() {
            if let Some(plugin) = self.runtime.get_plugin(plugin_id).await {
                for tool in plugin.manifest.get_tools() {
                    registry.deregister_dynamic(&tool.name);
                    debug!("Deregistered plugin tool '{}' from plugin '{}'", tool.name, plugin_id);
                }
            }
        }
    }

    /// Register a plugin's provider capabilities with the system.
    #[cfg(feature = "plugins")]
    async fn register_plugin_providers(&self, plugin: &PluginInstance) {
        if let Some(register) = self.provider_register.get() {
            if let Some(ref capabilities) = plugin.manifest.capabilities {
                for cap in capabilities {
                    if let PluginCapability::Provider {
                        name,
                        default_model,
                        stream_family,
                        supports_tools,
                        max_context,
                    } = cap
                    {
                        let family = PluginProvider::parse_stream_family(stream_family);
                        let provider = Arc::new(PluginProvider::new(
                            plugin.id().to_string(),
                            name.clone(),
                            default_model.clone(),
                            *supports_tools,
                            *max_context,
                            family,
                            self.runtime.clone(),
                        ));
                        register(name.clone(), provider);
                        info!(
                            "Registered plugin provider '{}' from plugin '{}'",
                            name,
                            plugin.id()
                        );
                    }
                }
            }
        }
    }

    /// Deregister a plugin's providers from the system.
    #[cfg(feature = "plugins")]
    async fn deregister_plugin_providers(&self, plugin_id: &str) {
        if let Some(unregister) = self.provider_unregister.get() {
            if let Some(plugin) = self.runtime.get_plugin(plugin_id).await {
                if let Some(ref capabilities) = plugin.manifest.capabilities {
                    for cap in capabilities {
                        if let PluginCapability::Provider { name, .. } = cap {
                            unregister(name.clone());
                            debug!(
                                "Deregistered plugin provider '{}' from plugin '{}'",
                                name, plugin_id
                            );
                        }
                    }
                }
            }
        }
    }

    /// Register a plugin's channel capabilities with the system.
    #[cfg(feature = "plugins")]
    async fn register_plugin_channels(&self, plugin: &PluginInstance) {
        use crate::channels::PluginChannel;

        let register = match self.channel_register.get() {
            Some(r) => r,
            None => return,
        };
        let tx = match self.channel_message_tx.get() {
            Some(t) => t.clone(),
            None => return,
        };

        if let Some(ref capabilities) = plugin.manifest.capabilities {
            for cap in capabilities {
                if let PluginCapability::Channel { channel_type: _, name } = cap {
                    let wasm_path = match plugin.manifest.main {
                        Some(ref main) => plugin.path.join(main),
                        None => {
                            warn!(
                                "Plugin '{}' declares Channel capability but no WASM main file",
                                plugin.id()
                            );
                            continue;
                        }
                    };

                    if !wasm_path.exists() {
                        warn!("Plugin '{}' channel WASM not found: {:?}", plugin.id(), wasm_path);
                        continue;
                    }

                    match PluginChannel::load(&wasm_path, plugin.config.clone(), tx.clone()).await {
                        Ok(channel) => {
                            let channel: Arc<dyn crate::channels::Channel> = Arc::new(channel);
                            register(name.clone(), channel);
                            info!(
                                "Registered plugin channel '{}' from plugin '{}'",
                                name,
                                plugin.id()
                            );
                        }
                        Err(e) => {
                            warn!("Failed to load PluginChannel from {:?}: {}", wasm_path, e);
                        }
                    }
                }
            }
        }
    }

    /// Deregister a plugin's channels from the system.
    #[cfg(feature = "plugins")]
    async fn deregister_plugin_channels(&self, plugin_id: &str) {
        if let Some(unregister) = self.channel_unregister.get() {
            if let Some(plugin) = self.runtime.get_plugin(plugin_id).await {
                if let Some(ref capabilities) = plugin.manifest.capabilities {
                    for cap in capabilities {
                        if let PluginCapability::Channel { name, .. } = cap {
                            unregister(name.clone());
                            debug!(
                                "Deregistered plugin channel '{}' from plugin '{}'",
                                name, plugin_id
                            );
                        }
                    }
                }
            }
        }
    }

    /// Get a plugin instance
    pub async fn get_plugin(&self, plugin_id: &str) -> Option<PluginInstance> {
        self.runtime.get_plugin(plugin_id).await
    }

    /// Run diagnostics on all loaded plugins.
    ///
    /// Checks: valid semver, syscity_version compatibility, WASM file
    /// existence, WASM compilation status. Returns a list of
    /// `DiagnosticHint` entries.
    pub async fn diagnose(&self) -> Vec<DiagnosticHint> {
        let mut hints = Vec::new();
        let plugins = self.runtime.list_plugins().await;

        for plugin in &plugins {
            // Check 1: valid semver version
            if let Err(e) = crate::skills::semver::Version::parse(&plugin.manifest.version) {
                hints.push(DiagnosticHint {
                    category: format!("plugin:{}", plugin.manifest.id),
                    message: format!(
                        "Plugin '{}' has invalid semver version '{}': {}",
                        plugin.manifest.name, plugin.manifest.version, e
                    ),
                    severity: HintSeverity::Warning,
                });
            }

            // Check 2: syscity_version constraint
            if let Some(ref req_str) = plugin.manifest.syscity_version {
                match req_str.parse::<crate::skills::semver::VersionReq>() {
                    Ok(req) => {
                        let syscity_ver =
                            crate::skills::semver::Version::parse(env!("CARGO_PKG_VERSION"))
                                .unwrap_or_else(|_| crate::skills::semver::Version::new(0, 1, 2));
                        if !req.matches(&syscity_ver) {
                            hints.push(DiagnosticHint {
                                category: format!("plugin:{}", plugin.manifest.id),
                                message: format!(
                                    "Plugin '{}' requires syscity_version '{}' but current is '{}'",
                                    plugin.manifest.name, req_str, syscity_ver
                                ),
                                severity: HintSeverity::Warning,
                            });
                        }
                    }
                    Err(e) => {
                        hints.push(DiagnosticHint {
                            category: format!("plugin:{}", plugin.manifest.id),
                            message: format!(
                                "Plugin '{}' has invalid syscity_version constraint '{}': {}",
                                plugin.manifest.name, req_str, e
                            ),
                            severity: HintSeverity::Warning,
                        });
                    }
                }
            }

            // Check 3: WASM file exists and compiles
            #[cfg(feature = "plugins")]
            if let Some(ref main) = plugin.manifest.main {
                let wasm_path = plugin.path.join(main);
                if !wasm_path.exists() {
                    hints.push(DiagnosticHint {
                        category: format!("plugin:{}", plugin.manifest.id),
                        message: format!(
                            "Plugin '{}' WASM file not found: {:?}",
                            plugin.manifest.name, wasm_path
                        ),
                        severity: HintSeverity::Error,
                    });
                } else if plugin.wasm_store.is_none() {
                    hints.push(DiagnosticHint {
                        category: format!("plugin:{}", plugin.manifest.id),
                        message: format!(
                            "Plugin '{}' WASM not compiled (failed at load time)",
                            plugin.manifest.name
                        ),
                        severity: HintSeverity::Warning,
                    });
                }
            }

            // Check 4: plugin has no main but has Tool capabilities
            if plugin.manifest.main.is_none() {
                let has_wasm_cap = plugin
                    .manifest
                    .capabilities
                    .as_ref()
                    .map(|caps| {
                        caps.iter().any(|c| {
                            matches!(c, crate::plugins::manifest::PluginCapability::Tools { .. })
                        })
                    })
                    .unwrap_or(false);
                if has_wasm_cap {
                    hints.push(DiagnosticHint {
                        category: format!("plugin:{}", plugin.manifest.id),
                        message: format!(
                            "Plugin '{}' declares Tools capability but no WASM main file",
                            plugin.manifest.name
                        ),
                        severity: HintSeverity::Warning,
                    });
                }
            }
        }

        hints
    }

    /// List all plugins
    pub async fn list_plugins(&self) -> Vec<PluginInstance> {
        self.runtime.list_plugins().await
    }

    /// Enable/disable a plugin
    pub async fn set_enabled(&self, plugin_id: &str, enabled: bool) -> crate::Result<()> {
        self.runtime.set_enabled(plugin_id, enabled).await
    }

    /// Get the hook registry
    pub fn hook_registry(&self) -> &Arc<HookRegistry> {
        &self.hook_registry
    }

    /// Get the plugin runtime
    pub fn runtime(&self) -> &Arc<PluginRuntime> {
        &self.runtime
    }

    /// Execute a hook
    pub async fn execute_hook(
        &self,
        hook_type: HookType,
        payload: HookPayload,
    ) -> HookExecutionResult {
        self.hook_registry.execute(hook_type, payload).await
    }

    /// Register a hook handler
    pub async fn register_hook(&self, handler: HookHandler) {
        self.hook_registry.register(handler).await;
    }

    /// Shutdown all plugins
    pub async fn shutdown(&self) -> crate::Result<()> {
        info!("Shutting down plugin manager...");
        self.runtime.shutdown().await
    }

    /// Get the metrics registry.
    pub fn metrics(&self) -> &Arc<PluginMetricsRegistry> {
        self.runtime.metrics()
    }

    /// Set the SQLite plugin registry for persistent metadata storage.
    pub async fn set_sqlite_registry(&self, pool: sqlx::sqlite::SqlitePool) {
        let registry = PluginSqliteRegistry::new(pool);
        // Create the table (best-effort)
        if let Err(e) = registry.create_table().await {
            warn!("Failed to create plugin_registry table: {}", e);
        }
        self.sqlite_registry.set(registry).ok();
    }

    /// Set the activation planner for dependency-based loading.
    pub fn set_activation_planner(&self) {
        let planner = ActivationPlanner::new(self.plugins_dir.clone());
        self.activation_planner.set(planner).ok();
    }

    /// Initialize plugins using the activation planner for dependency ordering.
    ///
    /// Falls back to flat scanning if no activation planner is set.
    pub async fn initialize_with_planner(&self) -> crate::Result<usize> {
        if let Some(planner) = self.activation_planner.get() {
            let plan = planner.plan_activation().await?;

            // Warn about cycle and missing deps
            if !plan.cycles.is_empty() {
                warn!("Detected {} plugin dependency cycle(s)", plan.cycles.len());
                for cycle in &plan.cycles {
                    warn!("  Cycle: {}", cycle.join(" -> "));
                }
            }
            for (plugin_id, dep_name, constraint) in &plan.missing_deps {
                warn!(
                    "Plugin '{}' has missing dependency '{}' ({})",
                    plugin_id, dep_name, constraint
                );
            }

            let mut count = 0;
            for plugin_id in &plan.load_order {
                let path = self.plugins_dir.join(plugin_id);
                if !path.is_dir() {
                    continue;
                }
                match self.load_plugin(&path).await {
                    Ok(id) => {
                        debug!("Activation planner loaded plugin '{}'", id);
                        count += 1;
                    }
                    Err(e) => {
                        warn!("Failed to load plugin '{}': {}", plugin_id, e);
                    }
                }
            }
            info!("Loaded {} plugin(s) via activation planner", count);
            Ok(count)
        } else {
            // Fall back to flat scan
            self.initialize().await
        }
    }

    /// Sync filesystem plugins into the SQLite registry.
    async fn sync_filesystem_to_registry(&self) {
        let Some(registry) = self.sqlite_registry.get() else {
            return;
        };

        let mut entries = match tokio::fs::read_dir(&self.plugins_dir).await {
            Ok(e) => e,
            Err(_) => return,
        };

        while let Some(entry) = entries.next_entry().await.ok().flatten() {
            let path = entry.path();
            if !path.is_dir() {
                continue;
            }
            let manifest_path = path.join("plugin.json");
            if !manifest_path.exists() {
                continue;
            }

            let content = match tokio::fs::read_to_string(&manifest_path).await {
                Ok(c) => c,
                Err(_) => continue,
            };
            let manifest: PluginManifest = match serde_json::from_str(&content) {
                Ok(m) => m,
                Err(_) => continue,
            };

            if let Err(e) = registry.register_plugin(&manifest, &path, None).await {
                warn!("Failed to sync plugin '{}' to registry: {}", manifest.id, e);
            }
        }
    }

    /// Get a plugin from the SQLite registry by ID.
    pub async fn get_registry_plugin(&self, id: &str) -> Option<PluginDbEntry> {
        match self.sqlite_registry.get() {
            Some(reg) => reg.get_plugin(id).await.ok().flatten(),
            None => None,
        }
    }

    /// List all plugins from the SQLite registry.
    pub async fn list_registry_plugins(&self) -> Vec<PluginDbEntry> {
        match self.sqlite_registry.get() {
            Some(reg) => reg.list_plugins().await.unwrap_or_default(),
            None => vec![],
        }
    }

    /// Create a sample plugin template
    pub async fn create_template(&self, name: &str, description: &str) -> crate::Result<PathBuf> {
        let plugin_dir = self.plugins_dir.join(name);
        tokio::fs::create_dir_all(&plugin_dir).await?;

        // Create manifest
        let manifest = PluginManifest {
            id: format!("com.example.{}", name),
            name: name.to_string(),
            version: "0.1.0".to_string(),
            syscity_version: None,
            description: description.to_string(),
            author: Some("Your Name".to_string()),
            main: None,
            capabilities: Some(vec![PluginCapability::Hooks {
                hooks: vec!["before_tool_execute".to_string()],
            }]),
            permissions: Some(vec![PluginPermission::Memory]),
            config: Some(serde_json::json!({
                "example_setting": "value"
            })),
            triggers: None,
            dependencies: None,
            repository: None,
            registry: None,
            signature: None,
            signer_public_key: None,
            external_resources: None,
        };

        let manifest_json = serde_json::to_string_pretty(&manifest)?;
        tokio::fs::write(plugin_dir.join("plugin.json"), manifest_json).await?;

        // Create config.json
        let config = serde_json::json!({
            "example_setting": "value"
        });
        tokio::fs::write(plugin_dir.join("config.json"), serde_json::to_string_pretty(&config)?)
            .await?;

        // Create README
        let readme = format!(
            r#"# {}

{}

## Installation

Place this directory in `{}`

## Configuration

Edit `config.json` to customize settings.

## Capabilities

- Hooks: before_tool_execute

## Permissions

- Memory
"#,
            name,
            description,
            self.plugins_dir.display()
        );
        tokio::fs::write(plugin_dir.join("README.md"), readme).await?;

        info!("Created plugin template at {:?}", plugin_dir);
        Ok(plugin_dir)
    }

    /// Install a plugin from a remote registry.
    pub async fn install_plugin(
        &self,
        name: &str,
        registry_url: Option<&str>,
    ) -> crate::Result<()> {
        let installer = PluginInstaller::new(self.plugins_dir.clone());
        installer.install(name, registry_url).await
    }

    /// Uninstall a plugin (remove from disk).
    pub async fn uninstall_plugin(&self, name: &str) -> crate::Result<()> {
        let installer = PluginInstaller::new(self.plugins_dir.clone());
        installer.uninstall(name).await
    }

    /// Search for plugins in a remote registry.
    pub async fn search_registry(
        &self,
        query: &str,
        registry_url: Option<&str>,
    ) -> crate::Result<Vec<registry::RegistryPluginEntry>> {
        let url = registry_url.unwrap_or("https://plugins.syscity.dev");
        let client = registry::RegistryClient::new(url);
        client.search(query).await
    }
}

#[cfg(test)]
mod tests {
    use tempfile::tempdir;

    use super::*;

    #[tokio::test]
    async fn new_creates_empty_manager() {
        let tmp = tempdir().unwrap();
        let manager = PluginManager::new(tmp.path().to_path_buf()).await.unwrap();
        let plugins = manager.list_plugins().await;
        assert!(plugins.is_empty());
    }

    #[tokio::test]
    async fn create_template_creates_manifest() {
        let tmp = tempdir().unwrap();
        let manager = PluginManager::new(tmp.path().to_path_buf()).await.unwrap();

        let path = manager
            .create_template("test-plugin", "A test plugin")
            .await
            .unwrap();
        assert!(path.join("plugin.json").exists());
        assert!(path.join("config.json").exists());
        assert!(path.join("README.md").exists());
    }

    #[tokio::test]
    async fn initialize_loads_plugins_from_disk() {
        let tmp = tempdir().unwrap();
        let manager = PluginManager::new(tmp.path().to_path_buf()).await.unwrap();

        // Create two plugin templates
        manager
            .create_template("plugin-a", "Plugin A")
            .await
            .unwrap();
        manager
            .create_template("plugin-b", "Plugin B")
            .await
            .unwrap();

        let count = manager.initialize().await.unwrap();
        assert_eq!(count, 2);

        let plugins = manager.list_plugins().await;
        assert_eq!(plugins.len(), 2);
    }

    #[tokio::test]
    async fn load_and_unload_plugin() {
        let tmp = tempdir().unwrap();
        let manager = PluginManager::new(tmp.path().to_path_buf()).await.unwrap();

        let path = manager
            .create_template("load-test", "Load test")
            .await
            .unwrap();
        let id = manager.load_plugin(&path).await.unwrap();
        assert_eq!(id, "com.example.load-test");

        let plugins = manager.list_plugins().await;
        assert_eq!(plugins.len(), 1);
        assert_eq!(plugins[0].name(), "load-test");

        let unloaded = manager.unload_plugin(&id).await.unwrap();
        assert!(unloaded);

        let plugins = manager.list_plugins().await;
        assert!(plugins.is_empty());
    }

    #[tokio::test]
    async fn get_plugin_returns_some_when_exists() {
        let tmp = tempdir().unwrap();
        let manager = PluginManager::new(tmp.path().to_path_buf()).await.unwrap();

        let path = manager
            .create_template("get-test", "Get test")
            .await
            .unwrap();
        let id = manager.load_plugin(&path).await.unwrap();

        let plugin = manager.get_plugin(&id).await;
        assert!(plugin.is_some());
        assert_eq!(plugin.unwrap().name(), "get-test");

        let missing = manager.get_plugin("nonexistent").await;
        assert!(missing.is_none());
    }

    #[tokio::test]
    async fn set_enabled_toggles_plugin() {
        let tmp = tempdir().unwrap();
        let manager = PluginManager::new(tmp.path().to_path_buf()).await.unwrap();

        let path = manager
            .create_template("toggle-test", "Toggle test")
            .await
            .unwrap();
        let id = manager.load_plugin(&path).await.unwrap();

        let plugin = manager.get_plugin(&id).await.unwrap();
        assert!(plugin.enabled);

        manager.set_enabled(&id, false).await.unwrap();
        let plugin = manager.get_plugin(&id).await.unwrap();
        assert!(!plugin.enabled);

        manager.set_enabled(&id, true).await.unwrap();
        let plugin = manager.get_plugin(&id).await.unwrap();
        assert!(plugin.enabled);
    }

    #[tokio::test]
    async fn set_enabled_unknown_plugin_fails() {
        let tmp = tempdir().unwrap();
        let manager = PluginManager::new(tmp.path().to_path_buf()).await.unwrap();

        let result = manager.set_enabled("nonexistent", false).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn reload_plugin_updates_manifest() {
        let tmp = tempdir().unwrap();
        let manager = PluginManager::new(tmp.path().to_path_buf()).await.unwrap();

        let path = manager
            .create_template("reload-test", "Reload test")
            .await
            .unwrap();
        let id = manager.load_plugin(&path).await.unwrap();

        // Modify manifest on disk
        let manifest_path = path.join("plugin.json");
        let mut manifest: PluginManifest = {
            let content = tokio::fs::read_to_string(&manifest_path).await.unwrap();
            serde_json::from_str(&content).unwrap()
        };
        manifest.description = "Updated description".to_string();
        tokio::fs::write(&manifest_path, serde_json::to_string_pretty(&manifest).unwrap())
            .await
            .unwrap();

        let reloaded_id = manager.reload_plugin(&id).await.unwrap();
        assert_eq!(reloaded_id, id);

        let plugin = manager.get_plugin(&id).await.unwrap();
        assert_eq!(plugin.manifest.description, "Updated description");
    }

    #[tokio::test]
    async fn reload_unknown_plugin_fails() {
        let tmp = tempdir().unwrap();
        let manager = PluginManager::new(tmp.path().to_path_buf()).await.unwrap();

        let result = manager.reload_plugin("nonexistent").await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn hook_registry_is_empty_by_default() {
        let tmp = tempdir().unwrap();
        let manager = PluginManager::new(tmp.path().to_path_buf()).await.unwrap();

        let hooks = manager.hook_registry().list_hooks().await;
        assert!(hooks.is_empty());
    }

    #[tokio::test]
    async fn shutdown_clears_plugins() {
        let tmp = tempdir().unwrap();
        let manager = PluginManager::new(tmp.path().to_path_buf()).await.unwrap();

        let path = manager
            .create_template("shutdown-test", "Shutdown test")
            .await
            .unwrap();
        manager.load_plugin(&path).await.unwrap();

        manager.shutdown().await.unwrap();
        let plugins = manager.list_plugins().await;
        assert!(plugins.is_empty());
    }

    #[tokio::test]
    async fn unload_unknown_plugin_returns_false() {
        let tmp = tempdir().unwrap();
        let manager = PluginManager::new(tmp.path().to_path_buf()).await.unwrap();

        let result = manager.unload_plugin("nonexistent").await.unwrap();
        assert!(!result);
    }
}

/// Plugin tool wrapper - adapts plugin tools to Syscity's Tool trait
use crate::tools::{Tool, ToolContext, ToolExecutionResult};

pub(crate) struct PluginToolWrapper {
    plugin_id: String,
    tool_name: String,
    description: String,
    parameters: serde_json::Value,
    runtime: Arc<PluginRuntime>,
    trace_enabled: Arc<std::sync::atomic::AtomicBool>,
}

impl PluginToolWrapper {
    pub(crate) fn new(
        plugin_id: String,
        tool: &PluginTool,
        runtime: Arc<PluginRuntime>,
        trace_enabled: Arc<std::sync::atomic::AtomicBool>,
    ) -> Self {
        Self {
            plugin_id,
            tool_name: tool.name.clone(),
            description: tool.description.clone(),
            parameters: tool.parameters.clone(),
            runtime,
            trace_enabled,
        }
    }
}

#[async_trait::async_trait]
impl Tool for PluginToolWrapper {
    fn name(&self) -> &str {
        &self.tool_name
    }

    fn description(&self) -> &str {
        &self.description
    }

    fn parameters_schema(&self) -> serde_json::Value {
        self.parameters.clone()
    }

    async fn execute(
        &self,
        args: serde_json::Value,
        _context: &ToolContext,
    ) -> crate::Result<ToolExecutionResult> {
        let start = std::time::Instant::now();
        let plugin_id = self.plugin_id.clone();

        if self
            .trace_enabled
            .load(std::sync::atomic::Ordering::Relaxed)
        {
            debug!(
                "[trace] Plugin tool '{}' from '{}' called with args: {}",
                self.tool_name, plugin_id, args
            );
        }

        // Record tool call metric
        if let Some(metrics) = self.runtime.metrics().get(&plugin_id).await {
            metrics.record_tool_call();
            metrics.touch();
        }

        let result = self
            .runtime
            .call_tool(&plugin_id, &self.tool_name, args)
            .await;

        // Record tool error metric on failure
        if let Err(e) = &result {
            if let Some(metrics) = self.runtime.metrics().get(&plugin_id).await {
                metrics.record_tool_error();
                metrics.set_last_error(e.to_string());
            }
        }

        if self
            .trace_enabled
            .load(std::sync::atomic::Ordering::Relaxed)
        {
            debug!(
                "[trace] Plugin tool '{}' from '{}' result: {:?} (elapsed: {:?})",
                self.tool_name,
                plugin_id,
                result.is_ok(),
                start.elapsed()
            );
        }

        match result {
            Ok(output) => Ok(ToolExecutionResult {
                success: true,
                output: output.to_string(),
                error: None,
                data: Some(output),
                execution_time: start.elapsed(),
            }),
            Err(e) => Ok(ToolExecutionResult {
                success: false,
                output: String::new(),
                error: Some(e.to_string()),
                data: None,
                execution_time: start.elapsed(),
            }),
        }
    }
}
