//! Browser pool for persistent browser instance caching
//!
//! Replaces per-call browser launch with a pool of long-lived browser
//! instances. Instances are lazily created per profile and evicted after idle
//! timeout.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::sync::RwLock;
use tracing::{debug, info, warn};

use super::profile::{BrowserDriver, BrowserPoolConfig, BrowserProfile};

/// Handle to a page within a browser instance
#[derive(Debug, Clone)]
pub struct PageHandle {
    /// CDP target ID
    pub target_id: String,
    /// Shared page reference
    pub page: Arc<chromiumoxide::Page>,
    /// When this page was last used
    pub last_used: Instant,
}

/// A browser instance in the pool
#[derive(Debug)]
pub struct BrowserInstance {
    /// Shared browser reference
    pub browser: Arc<chromiumoxide::Browser>,
    /// Pages managed by this instance
    pub pages: RwLock<HashMap<String, PageHandle>>,
    /// Profile used to create this instance
    pub profile: BrowserProfile,
    /// When this instance was last used
    pub last_used: RwLock<Instant>,
    /// Handler shutdown signal
    _handler_abort: tokio::task::AbortHandle,
}

impl BrowserInstance {
    /// Create a new browser instance from a profile
    #[cfg(feature = "browser")]
    pub async fn launch(profile: &BrowserProfile) -> crate::Result<Self> {
        use chromiumoxide::browser::{Browser, BrowserConfig};

        let mut builder = BrowserConfig::builder()
            .viewport(chromiumoxide::handler::viewport::Viewport {
                width: profile.viewport_width,
                height: profile.viewport_height,
                device_scale_factor: Some(1.0),
                emulating_mobile: false,
                is_landscape: profile.viewport_width > profile.viewport_height,
                has_touch: false,
            })
            .request_timeout(Duration::from_secs(30));

        if profile.headless {
            builder = builder.arg("--headless=new");
        }

        if let Some(ref path) = profile.chrome_path {
            builder = builder.chrome_executable(path.clone());
        }

        if let Some(ref ua) = profile.user_agent {
            builder = builder.arg(format!("--user-agent={}", ua));
        }

        if let Some(ref data_dir) = profile.user_data_dir {
            // The dedicated config field, not a raw arg: chromiumoxide
            // unconditionally appends its own `--user-data-dir` (the
            // `chromiumoxide-runner` temp dir) during build, which overrides
            // a duplicate raw flag — so configured persistent sessions never
            // actually used their directory.
            builder = builder.user_data_dir(data_dir);
        }

        for arg in &profile.extra_args {
            builder = builder.arg(arg.clone());
        }

        let config = builder
            .build()
            .map_err(|e| crate::error::SyscityError::ExternalService {
                source: format!("Browser configuration failed: {}", e),
                cause: None,
            })?;

        let (browser, mut handler) = Browser::launch(config).await.map_err(|e| {
            crate::error::SyscityError::ExternalService {
                source: "Failed to launch Chrome/Chromium. Is it installed?".to_string(),
                cause: Some(Box::new(e)),
            }
        })?;

        let browser = Arc::new(browser);

        // Spawn handler task
        let handler_task = tokio::spawn(async move {
            use futures::StreamExt;
            while let Some(h) = handler.next().await {
                if h.is_err() {
                    break;
                }
            }
        });

        Ok(Self {
            browser: browser.clone(),
            pages: RwLock::new(HashMap::new()),
            profile: profile.clone(),
            last_used: RwLock::new(Instant::now()),
            _handler_abort: handler_task.abort_handle(),
        })
    }

    /// Connect to an existing Chrome via CDP (Chrome MCP mode)
    #[cfg(feature = "browser")]
    pub async fn connect(cdp_url: &str, profile: &BrowserProfile) -> crate::Result<Self> {
        use chromiumoxide::browser::Browser;

        let (browser, mut handler) = Browser::connect(cdp_url).await.map_err(|e| {
            crate::error::SyscityError::ExternalService {
                source: format!("Failed to connect to Chrome at {}", cdp_url),
                cause: Some(Box::new(e)),
            }
        })?;

        let browser = Arc::new(browser);

        // Spawn handler task for the connected browser as well
        let handler_task = tokio::spawn(async move {
            use futures::StreamExt;
            while let Some(h) = handler.next().await {
                if h.is_err() {
                    break;
                }
            }
        });

        Ok(Self {
            browser: browser.clone(),
            pages: RwLock::new(HashMap::new()),
            profile: profile.clone(),
            last_used: RwLock::new(Instant::now()),
            _handler_abort: handler_task.abort_handle(),
        })
    }

    /// Create a new page and add it to this instance
    #[cfg(feature = "browser")]
    pub async fn new_page(&self, url: &str) -> crate::Result<PageHandle> {
        let page = self.browser.new_page(url).await.map_err(|e| {
            crate::error::SyscityError::ExternalService {
                source: "Failed to create browser page".to_string(),
                cause: Some(Box::new(e)),
            }
        })?;

        let target_id = page.target_id().inner().to_string();
        let handle = PageHandle {
            target_id: target_id.clone(),
            page: Arc::new(page),
            last_used: Instant::now(),
        };

        {
            let mut pages = self.pages.write().await;
            pages.insert(target_id.clone(), handle.clone());
        }

        *self.last_used.write().await = Instant::now();

        Ok(handle)
    }

    /// Get a page by target ID
    pub async fn get_page(&self, target_id: &str) -> Option<PageHandle> {
        let pages = self.pages.read().await;
        pages.get(target_id).cloned()
    }

    /// The most recently used page this instance still tracks, if any.
    ///
    /// Browser state lives on the page, so a follow-up tool call wants the
    /// page the previous call left behind: opening a fresh `about:blank` per
    /// call turns a Navigate in one call and a Type in the next into two
    /// different tabs.
    pub async fn most_recent_page(&self) -> Option<PageHandle> {
        let pages = self.pages.read().await;
        pages.values().max_by_key(|h| h.last_used).cloned()
    }

    /// Mark a page as just used, so the next reuse picks it over older ones.
    pub async fn touch_page(&self, target_id: &str) {
        {
            let mut pages = self.pages.write().await;
            if let Some(handle) = pages.get_mut(target_id) {
                handle.last_used = Instant::now();
            }
        }
        *self.last_used.write().await = Instant::now();
    }

    /// Close a page by target ID — closes the CDP page and removes from map
    #[cfg(feature = "browser")]
    pub async fn close_page(&self, target_id: &str) -> crate::Result<bool> {
        let page = {
            let mut pages = self.pages.write().await;
            pages.remove(target_id)
        };

        if let Some(handle) = page {
            // Close the CDP page via JavaScript so page is actually closed in Chrome
            let close_script = "window.close();";
            if let Err(e) = handle.page.evaluate(close_script).await {
                warn!(target_id = %target_id, "Failed to close page via JS: {}", e);
            }

            *self.last_used.write().await = Instant::now();
            Ok(true)
        } else {
            Ok(false)
        }
    }

    /// Close all pages and the browser instance
    #[cfg(feature = "browser")]
    pub async fn shutdown(&self) {
        let page_ids: Vec<String> = {
            let pages = self.pages.read().await;
            pages.keys().cloned().collect()
        };

        for id in &page_ids {
            if let Err(e) = self.close_page(id).await {
                warn!(target_id = %id, "Error closing page during shutdown: {}", e);
            }
        }

        // Abort the handler task; the browser process will be terminated
        // when the Browser instance is dropped.
        self._handler_abort.abort();
    }

    /// Number of open pages
    pub async fn page_count(&self) -> usize {
        self.pages.read().await.len()
    }

    /// List all pages with their target_id, title, and url
    #[cfg(feature = "browser")]
    pub async fn list_pages(&self) -> Vec<(String, String, String)> {
        let pages = self.pages.read().await;
        let mut result = Vec::new();
        for (target_id, handle) in pages.iter() {
            let title = handle
                .page
                .get_title()
                .await
                .ok()
                .flatten()
                .unwrap_or_default();
            let url = handle.page.url().await.ok().flatten().unwrap_or_default();
            result.push((target_id.clone(), title, url));
        }
        result
    }

    /// Switch focus to a page by target_id (bring to front)
    #[cfg(feature = "browser")]
    pub async fn switch_page(&self, target_id: &str) -> crate::Result<bool> {
        let page = {
            let pages = self.pages.read().await;
            pages.get(target_id).cloned()
        };

        if let Some(handle) = page {
            handle.page.activate().await.map_err(|e| {
                crate::error::SyscityError::ExternalService {
                    source: "Failed to activate page".to_string(),
                    cause: Some(Box::new(e)),
                }
            })?;
            *self.last_used.write().await = Instant::now();
            Ok(true)
        } else {
            Ok(false)
        }
    }

    /// Find a page target_id by title (fuzzy match)
    #[cfg(feature = "browser")]
    pub async fn find_page_by_title(&self, title: &str) -> Option<String> {
        let pages = self.pages.read().await;
        for (target_id, handle) in pages.iter() {
            if let Ok(Some(page_title)) = handle.page.get_title().await {
                if page_title.contains(title) {
                    return Some(target_id.clone());
                }
            }
        }
        None
    }
}

/// Pool of browser instances keyed by profile name
#[derive(Debug)]
pub struct BrowserPool {
    instances: Arc<RwLock<HashMap<String, Arc<BrowserInstance>>>>,
    config: BrowserPoolConfig,
    profiles: Arc<RwLock<HashMap<String, BrowserProfile>>>,
    cleanup_shutdown: Option<Arc<tokio::sync::Notify>>,
    _cleanup_task: Option<tokio::task::AbortHandle>,
}

impl BrowserPool {
    /// Start the background idle-eviction task.
    fn spawn_cleanup(
        instances: Arc<RwLock<HashMap<String, Arc<BrowserInstance>>>>,
        idle_timeout_secs: u64,
        cleanup_interval_secs: u64,
    ) -> (Option<tokio::task::AbortHandle>, Option<Arc<tokio::sync::Notify>>) {
        let runtime_handle = tokio::runtime::Handle::try_current();
        if runtime_handle.is_err() {
            return (None, None);
        }
        let idle_timeout = Duration::from_secs(idle_timeout_secs);
        let interval = Duration::from_secs(cleanup_interval_secs);

        let shutdown = Arc::new(tokio::sync::Notify::new());
        let shutdown_clone = shutdown.clone();

        let task = tokio::spawn(async move {
            let mut ticker = tokio::time::interval(interval);
            loop {
                tokio::select! {
                    _ = ticker.tick() => {
                        Self::evict_idle(&instances, idle_timeout).await;
                    }
                    _ = shutdown_clone.notified() => {
                        debug!("Cleanup task shutting down");
                        break;
                    }
                }
            }
        });
        (Some(task.abort_handle()), Some(shutdown))
    }

    /// Create a new empty browser pool
    pub fn new(config: BrowserPoolConfig) -> Self {
        let instances: Arc<RwLock<HashMap<String, Arc<BrowserInstance>>>> =
            Arc::new(RwLock::new(HashMap::new()));
        let mut profiles_map = HashMap::new();
        profiles_map
            .insert(config.default_profile.clone(), BrowserProfile::new(&config.default_profile));
        let profiles = Arc::new(RwLock::new(profiles_map));

        let (cleanup_handle, shutdown_notify) = Self::spawn_cleanup(
            instances.clone(),
            config.idle_timeout_secs,
            config.cleanup_interval_secs,
        );

        Self {
            instances,
            config,
            profiles,
            cleanup_shutdown: shutdown_notify,
            _cleanup_task: cleanup_handle,
        }
    }

    /// Get the pool configuration
    pub fn config(&self) -> &BrowserPoolConfig {
        &self.config
    }

    /// Create a pool with pre-configured profiles
    pub fn with_profiles(config: BrowserPoolConfig, extra_profiles: Vec<BrowserProfile>) -> Self {
        let mut profiles_map = HashMap::new();
        profiles_map
            .insert(config.default_profile.clone(), BrowserProfile::new(&config.default_profile));
        for profile in extra_profiles {
            profiles_map.insert(profile.name.clone(), profile);
        }
        let profiles = Arc::new(RwLock::new(profiles_map));
        let instances: Arc<RwLock<HashMap<String, Arc<BrowserInstance>>>> =
            Arc::new(RwLock::new(HashMap::new()));

        let (cleanup_handle, shutdown_notify) = Self::spawn_cleanup(
            instances.clone(),
            config.idle_timeout_secs,
            config.cleanup_interval_secs,
        );

        Self {
            instances,
            config,
            profiles,
            cleanup_shutdown: shutdown_notify,
            _cleanup_task: cleanup_handle,
        }
    }

    /// Register a profile
    pub async fn register_profile(&self, profile: BrowserProfile) {
        let mut profiles = self.profiles.write().await;
        profiles.insert(profile.name.clone(), profile);
    }

    /// Get or create a browser instance for a profile
    #[cfg(feature = "browser")]
    pub async fn get_or_create(&self, profile_name: &str) -> crate::Result<Arc<BrowserInstance>> {
        // Fast path: instance already exists
        {
            let instances = self.instances.read().await;
            if let Some(instance) = instances.get(profile_name) {
                *instance.last_used.write().await = Instant::now();
                return Ok(instance.clone());
            }
        }

        // Slow path: need to create instance
        let profile = {
            let profiles = self.profiles.read().await;
            profiles
                .get(profile_name)
                .cloned()
                .unwrap_or_else(|| BrowserProfile::new(profile_name))
        };

        info!(profile = %profile_name, "Creating new browser instance");

        let instance = match &profile.driver {
            BrowserDriver::Managed => BrowserInstance::launch(&profile).await?,
            BrowserDriver::ChromeMcp { cdp_url } => {
                BrowserInstance::connect(cdp_url, &profile).await?
            }
        };

        let instance = Arc::new(instance);

        {
            let mut instances = self.instances.write().await;
            instances.insert(profile_name.to_string(), instance.clone());
        }

        Ok(instance)
    }

    /// Create a new page in a browser instance for the given profile
    #[cfg(feature = "browser")]
    pub async fn new_page(&self, profile_name: &str, url: &str) -> crate::Result<PageHandle> {
        let instance = self.get_or_create(profile_name).await?;
        instance.new_page(url).await
    }

    /// Close a specific page by profile and target ID
    #[cfg(feature = "browser")]
    pub async fn close_page(&self, profile_name: &str, target_id: &str) -> crate::Result<bool> {
        let instances = self.instances.read().await;
        if let Some(instance) = instances.get(profile_name) {
            instance.close_page(target_id).await
        } else {
            Ok(false)
        }
    }

    /// Close all pages for a profile and remove the instance
    #[cfg(feature = "browser")]
    pub async fn close_profile(&self, profile_name: &str) {
        let instance = {
            let mut instances = self.instances.write().await;
            instances.remove(profile_name)
        };

        if let Some(inst) = instance {
            info!(profile = %profile_name, "Shutting down browser instance");
            inst.shutdown().await;
        } else {
            debug!(profile = %profile_name, "No browser instance to shut down");
        }
    }

    /// Shut down all browser instances in the pool.
    #[cfg(feature = "browser")]
    pub async fn shutdown(&self) {
        let profile_names: Vec<String> = {
            let instances = self.instances.read().await;
            instances.keys().cloned().collect()
        };
        for name in profile_names {
            self.close_profile(&name).await;
        }
    }

    /// List active profiles and their page counts
    pub async fn status(&self) -> Vec<(String, usize)> {
        let instances = self.instances.read().await;
        let mut result = Vec::new();
        for (name, instance) in instances.iter() {
            result.push((name.clone(), instance.page_count().await));
        }
        result
    }

    /// List all pages for a profile
    #[cfg(feature = "browser")]
    pub async fn list_pages(
        &self,
        profile_name: &str,
    ) -> crate::Result<Vec<(String, String, String)>> {
        let instances = self.instances.read().await;
        if let Some(instance) = instances.get(profile_name) {
            Ok(instance.list_pages().await)
        } else {
            Ok(Vec::new())
        }
    }

    /// Switch to a page by target_id
    #[cfg(feature = "browser")]
    pub async fn switch_page(&self, profile_name: &str, target_id: &str) -> crate::Result<bool> {
        let instances = self.instances.read().await;
        if let Some(instance) = instances.get(profile_name) {
            instance.switch_page(target_id).await
        } else {
            Ok(false)
        }
    }

    /// Find a page by title and return its target_id
    #[cfg(feature = "browser")]
    pub async fn find_page_by_title(
        &self,
        profile_name: &str,
        title: &str,
    ) -> crate::Result<Option<String>> {
        let instances = self.instances.read().await;
        if let Some(instance) = instances.get(profile_name) {
            Ok(instance.find_page_by_title(title).await)
        } else {
            Ok(None)
        }
    }

    /// Evict idle instances
    async fn evict_idle(
        instances: &Arc<RwLock<HashMap<String, Arc<BrowserInstance>>>>,
        idle_timeout: Duration,
    ) {
        let to_evict: Vec<String> = {
            let instances = instances.read().await;
            let mut evict = Vec::new();
            for (name, instance) in instances.iter() {
                let last_used = *instance.last_used.read().await;
                if last_used.elapsed() > idle_timeout && instance.page_count().await == 0 {
                    evict.push(name.clone());
                }
            }
            evict
        };

        for name in to_evict {
            debug!(profile = %name, "Evicting idle browser instance");
            let inst = {
                let mut instances = instances.write().await;
                instances.remove(&name)
            };
            if let Some(i) = inst {
                i.shutdown().await;
            }
        }
    }
}

impl Drop for BrowserPool {
    fn drop(&mut self) {
        // Signal the cleanup task to shut down gracefully before aborting
        if let Some(notify) = self.cleanup_shutdown.take() {
            notify.notify_waiters();
        }
        if let Some(handle) = self._cleanup_task.take() {
            handle.abort();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_browser_pool_create() {
        let config = BrowserPoolConfig::default();
        let pool = BrowserPool::new(config);
        // Should not panic
        drop(pool);
    }

    #[test]
    fn test_browser_pool_with_profiles() {
        let profiles = vec![
            BrowserProfile::new("default"),
            BrowserProfile::headed("headed"),
        ];
        let config = BrowserPoolConfig::default();
        let pool = BrowserPool::with_profiles(config, profiles);
        drop(pool);
    }

    #[test]
    fn test_browser_profile_default_in_pool() {
        let config = BrowserPoolConfig::default();
        let pool = BrowserPool::new(config);
        // The default profile should be registered
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let profiles = pool.profiles.read().await;
            assert!(profiles.contains_key("default"));
        });
    }

    #[tokio::test]
    async fn test_browser_pool_register_profile() {
        let config = BrowserPoolConfig::default();
        let pool = BrowserPool::new(config);
        let profile = BrowserProfile::headed("test-headed");
        pool.register_profile(profile).await;
        let profiles = pool.profiles.read().await;
        assert!(profiles.contains_key("test-headed"));
    }

    #[tokio::test]
    async fn test_browser_pool_status_empty() {
        let config = BrowserPoolConfig::default();
        let pool = BrowserPool::new(config);
        let status = pool.status().await;
        assert!(status.is_empty());
    }

    #[tokio::test]
    #[cfg(feature = "browser")]
    async fn test_browser_pool_list_pages_empty_profile() {
        let config = BrowserPoolConfig::default();
        let pool = BrowserPool::new(config);
        let pages = pool.list_pages("nonexistent").await.unwrap();
        assert!(pages.is_empty());
    }

    #[tokio::test]
    #[cfg(feature = "browser")]
    async fn test_browser_pool_switch_page_not_found() {
        let config = BrowserPoolConfig::default();
        let pool = BrowserPool::new(config);
        let result = pool.switch_page("nonexistent", "target-1").await.unwrap();
        assert!(!result);
    }

    #[tokio::test]
    #[cfg(feature = "browser")]
    async fn test_browser_pool_find_page_empty() {
        let config = BrowserPoolConfig::default();
        let pool = BrowserPool::new(config);
        let result = pool.find_page_by_title("nonexistent", "foo").await.unwrap();
        assert!(result.is_none());
    }

    #[tokio::test]
    #[cfg(feature = "browser")]
    async fn test_browser_pool_close_page_not_found() {
        let config = BrowserPoolConfig::default();
        let pool = BrowserPool::new(config);
        let result = pool.close_page("nonexistent", "target-1").await.unwrap();
        assert!(!result);
    }
}
