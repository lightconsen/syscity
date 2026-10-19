//! Hot Config Reload System
//!
//! Watches configuration files for changes and reloads them at runtime
//! without requiring a restart.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
#[cfg(feature = "hot-reload")]
use std::time::Duration;

#[cfg(feature = "hot-reload")]
use notify::{RecommendedWatcher, RecursiveMode};
#[cfg(feature = "hot-reload")]
use notify_debouncer_full::{new_debouncer, DebouncedEvent, Debouncer, RecommendedCache};
use tokio::sync::{mpsc, RwLock};
use tracing::{debug, error, info, warn};

/// Configuration file types that can be hot-reloaded
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ConfigFileType {
    /// Main application configuration
    Main,
    /// Agent configuration
    Agent,
    /// Channel configuration
    Channel,
    /// Plugin configuration
    Plugin,
    /// Gateway configuration
    Gateway,
    /// Knowledge Base kb.toml config
    KnowledgeBase,
    /// Custom config file
    Custom,
}

/// A watched configuration file
#[derive(Debug, Clone)]
pub struct WatchedConfig {
    /// File path
    pub path: PathBuf,
    /// Config type
    pub config_type: ConfigFileType,
    /// Whether the file is currently valid
    pub is_valid: bool,
}

/// Configuration change event
#[derive(Debug, Clone)]
pub struct ConfigChangeEvent {
    /// Path of the changed file
    pub path: PathBuf,
    /// Config type
    pub config_type: ConfigFileType,
    /// Type of change
    pub change_type: ConfigChangeType,
}

/// Type of configuration change
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfigChangeType {
    /// File was created
    Created,
    /// File was modified
    Modified,
    /// File was deleted
    Deleted,
    /// File was renamed
    Renamed,
}

/// Handler function for config changes
type ConfigChangeHandler = Arc<
    dyn Fn(ConfigChangeEvent) -> futures::future::BoxFuture<'static, Result<(), String>>
        + Send
        + Sync,
>;

/// Hot reload manager
pub struct HotReloadManager {
    /// Watched files
    watched_files: Arc<RwLock<HashMap<PathBuf, WatchedConfig>>>,
    /// Registered handlers
    handlers: Arc<RwLock<HashMap<ConfigFileType, Vec<ConfigChangeHandler>>>>,
    /// Channel for change events (read only by the hot-reload watcher loop)
    #[cfg_attr(not(feature = "hot-reload"), allow(dead_code))]
    change_tx: mpsc::Sender<ConfigChangeEvent>,
    change_rx: Arc<RwLock<mpsc::Receiver<ConfigChangeEvent>>>,
    /// File watcher (only available with hot-reload feature)
    #[cfg(feature = "hot-reload")]
    watcher: Arc<RwLock<Option<Debouncer<RecommendedWatcher, RecommendedCache>>>>,
}

impl HotReloadManager {
    /// Create a new hot reload manager
    pub fn new() -> crate::Result<Self> {
        let (change_tx, change_rx) = mpsc::channel(100);

        Ok(Self {
            watched_files: Arc::new(RwLock::new(HashMap::new())),
            handlers: Arc::new(RwLock::new(HashMap::new())),
            change_tx,
            change_rx: Arc::new(RwLock::new(change_rx)),
            #[cfg(feature = "hot-reload")]
            watcher: Arc::new(RwLock::new(None)),
        })
    }

    /// Start watching files
    #[cfg(feature = "hot-reload")]
    pub async fn start(&self) -> crate::Result<()> {
        let change_tx = self.change_tx.clone();
        let watched_files = self.watched_files.clone();

        // Create debouncer with 500ms delay
        let debouncer = new_debouncer(
            Duration::from_millis(500),
            None,
            move |result: Result<Vec<DebouncedEvent>, Vec<notify::Error>>| {
                match result {
                    Ok(events) => {
                        for debounced_event in events {
                            let notify_event = &debounced_event.event;
                            let change_type = match notify_event.kind {
                                notify::EventKind::Create(_) => ConfigChangeType::Created,
                                notify::EventKind::Modify(_) => ConfigChangeType::Modified,
                                notify::EventKind::Remove(_) => ConfigChangeType::Deleted,
                                _ => continue,
                            };

                            // Process each path in the event
                            for path in &notify_event.paths {
                                // Look up the config type for this path
                                let config_type = {
                                    let files = futures::executor::block_on(async {
                                        watched_files.read().await
                                    });
                                    files
                                        .get(path)
                                        .map(|f| f.config_type)
                                        .unwrap_or(ConfigFileType::Custom)
                                };

                                let event = ConfigChangeEvent {
                                    path: path.clone(),
                                    config_type,
                                    change_type,
                                };

                                if let Err(e) = change_tx.try_send(event) {
                                    warn!("Failed to send config change event: {}", e);
                                } else {
                                    info!("Config file changed: {:?}", path);
                                }
                            }
                        }
                    }
                    Err(errors) => {
                        for e in errors {
                            error!("File watcher error: {}", e);
                        }
                    }
                }
            },
        )
        .map_err(|e| crate::error::ConfigError::InvalidValue {
            key: "hot_reload".to_string(),
            message: format!("Failed to create file watcher: {}", e),
        })?;

        // Store the watcher
        {
            let mut watcher_guard = self.watcher.write().await;
            *watcher_guard = Some(debouncer);
        }

        info!("Hot reload manager started with file watching enabled");
        Ok(())
    }

    /// Start watching files (without hot-reload feature)
    #[cfg(not(feature = "hot-reload"))]
    pub async fn start(&self) -> crate::Result<()> {
        info!("Hot reload manager started (file watching disabled without hot-reload feature)");
        Ok(())
    }

    /// Watch a configuration file
    pub async fn watch_file(
        &self,
        path: impl AsRef<Path>,
        config_type: ConfigFileType,
    ) -> crate::Result<()> {
        let path = path.as_ref().to_path_buf();

        // Check if file exists
        if !path.exists() {
            warn!("Cannot watch non-existent file: {:?}", path);
            return Err(
                crate::error::ConfigError::Missing(format!("File not found: {:?}", path)).into()
            );
        }

        // Add to watched files
        let watched_config = WatchedConfig {
            path: path.clone(),
            config_type,
            is_valid: true,
        };

        {
            let mut files = self.watched_files.write().await;
            files.insert(path.clone(), watched_config);
        }

        // Register with file watcher if hot-reload feature is enabled
        #[cfg(feature = "hot-reload")]
        {
            let mut watcher_guard = self.watcher.write().await;
            if let Some(ref mut debouncer) = *watcher_guard {
                // `Debouncer::watcher()` is a no-op in notify-debouncer-full 0.7;
                // the debouncer forwards `watch` itself.
                if let Err(e) = debouncer.watch(&path, RecursiveMode::NonRecursive) {
                    warn!("Failed to add file to watcher: {:?} - {}", path, e);
                } else {
                    debug!("Added file to notify watcher: {:?}", path);
                }
            }
        }

        info!("Watching config file: {:?} ({:?})", path, config_type);
        Ok(())
    }

    /// Unwatch a file
    pub async fn unwatch_file(&self, path: impl AsRef<Path>) -> crate::Result<bool> {
        let path = path.as_ref();
        let mut files = self.watched_files.write().await;

        if files.remove(path).is_some() {
            info!("Stopped watching config file: {:?}", path);
            Ok(true)
        } else {
            Ok(false)
        }
    }

    /// Register a handler for config changes
    pub async fn register_handler<F, Fut>(&self, config_type: ConfigFileType, handler: F)
    where
        F: Fn(ConfigChangeEvent) -> Fut + Send + Sync + 'static,
        Fut: std::future::Future<Output = Result<(), String>> + Send + 'static,
    {
        let mut handlers = self.handlers.write().await;
        let handler_list = handlers.entry(config_type).or_default();

        handler_list.push(Arc::new(move |event| Box::pin(handler(event))));

        debug!("Registered handler for {:?}", config_type);
    }

    /// Process configuration changes
    pub async fn run(&self) -> crate::Result<()> {
        let mut rx = self.change_rx.write().await;

        while let Some(event) = rx.recv().await {
            info!("Processing config change: {:?} ({:?})", event.path, event.change_type);

            // Get handlers for this config type
            let handlers = {
                let handlers = self.handlers.read().await;
                handlers.get(&event.config_type).cloned()
            };

            if let Some(handlers) = handlers {
                for handler in handlers {
                    match handler(event.clone()).await {
                        Ok(_) => {
                            debug!("Handler succeeded for {:?}", event.path);
                        }
                        Err(e) => {
                            error!("Handler failed for {:?}: {}", event.path, e);
                        }
                    }
                }
            }
        }

        Ok(())
    }

    /// Test-only: synchronously invoke the handlers registered for the event's
    /// config type, bypassing the file-watcher channel. Mirrors the core of
    /// [`Self::run`] so handler bodies can be exercised deterministically.
    #[cfg(test)]
    pub async fn dispatch_for_test(&self, event: ConfigChangeEvent) {
        let handlers = {
            let handlers = self.handlers.read().await;
            handlers.get(&event.config_type).cloned()
        };
        if let Some(handlers) = handlers {
            for handler in handlers {
                if let Err(e) = handler(event.clone()).await {
                    error!("Handler failed for {:?}: {}", event.path, e);
                }
            }
        }
    }

    /// Stop watching files
    pub async fn stop(&self) -> crate::Result<()> {
        let mut files = self.watched_files.write().await;
        files.clear();

        #[cfg(feature = "hot-reload")]
        {
            let mut watcher = self.watcher.write().await;
            *watcher = None;
        }

        info!("Hot reload manager stopped");
        Ok(())
    }

    /// List all watched files
    pub async fn list_watched(&self) -> Vec<WatchedConfig> {
        let files = self.watched_files.read().await;
        files.values().cloned().collect()
    }

    /// Check if a file is being watched
    pub async fn is_watched(&self, path: impl AsRef<Path>) -> bool {
        let files = self.watched_files.read().await;
        files.contains_key(path.as_ref())
    }
}

impl Default for HotReloadManager {
    fn default() -> Self {
        #[allow(clippy::expect_used)]
        Self::new().expect("Failed to create HotReloadManager")
    }
}

/// Builder for hot reload setup
pub struct HotReloadBuilder {
    config_paths: Vec<(PathBuf, ConfigFileType)>,
}

impl HotReloadBuilder {
    /// Create a new builder
    pub fn new() -> Self {
        Self { config_paths: vec![] }
    }

    /// Add a config file to watch
    pub fn watch(mut self, path: impl AsRef<Path>, config_type: ConfigFileType) -> Self {
        self.config_paths
            .push((path.as_ref().to_path_buf(), config_type));
        self
    }

    /// Build and start the hot reload manager
    pub async fn build(self) -> crate::Result<HotReloadManager> {
        let manager = HotReloadManager::new()?;
        manager.start().await?;

        for (path, config_type) in self.config_paths {
            if let Err(e) = manager.watch_file(&path, config_type).await {
                warn!("Failed to watch {:?}: {}", path, e);
            }
        }

        Ok(manager)
    }
}

impl Default for HotReloadBuilder {
    fn default() -> Self {
        Self::new()
    }
}
