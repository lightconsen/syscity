//! Hot-reload handlers — watch config.toml / agent / channel / plugin /
//! gateway configs and apply changes without restarting the process.
//!
//! Extracted from `gateway/mod.rs`. Wired in `Gateway::start()` via
//! `register_hot_reload_handlers(&self.state, self.config.clone(),
//! &hot_reload)`.

use std::sync::Arc;

use tracing::{error, info, warn};

use super::{AgentCommand, ChannelConfig, GatewayConfig, GatewayState};
use crate::agent::AgentConfig;
use crate::config::hot_reload::{ConfigChangeType, ConfigFileType, HotReloadManager};

/// Register hot reload handlers for config changes.
///
/// Registers handlers on the [`HotReloadManager`] for each
/// [`ConfigFileType`] (Main, Agent, Channel, Plugin, Gateway) so that file
/// changes apply to the running gateway state without restart.
pub(crate) async fn register_hot_reload_handlers(
    state: Arc<GatewayState>,
    current_config: GatewayConfig,
    hot_reload: &HotReloadManager,
) {
    // Pre-clone for handlers registered after the main handler
    // (the main handler's `move` closure will consume `state` and `current_config`)
    let state_agent = state.clone();
    let state_channel = state.clone();
    let current_config_channel = current_config.clone();
    let state_plugin = state.clone();
    let state_gateway = state.clone();
    let state_kb = state.clone();

    // Handler for main config changes (includes config.toml)
    hot_reload
        .register_handler(ConfigFileType::Main, move |_event| {
            let state = state.clone();
            let current_config = current_config.clone();
            async move {
                info!("Main config file changed - reloading configuration");

                // Reload config from disk
                let config_path = state.paths.default_config_file();
                if !config_path.exists() {
                    return Ok(());
                }

                let content = match tokio::fs::read_to_string(&config_path).await {
                    Ok(c) => c,
                    Err(e) => {
                        error!("Failed to read config.toml: {}", e);
                        return Ok(());
                    }
                };

                let new_config: GatewayConfig = match toml::from_str(&content) {
                    Ok(cfg) => cfg,
                    Err(e) => {
                        error!("Failed to parse config.toml: {}", e);
                        return Ok(());
                    }
                };

                // Get current running channels
                let current_channels: Vec<String> = {
                    let channels = state.channels.channels.read().await;
                    channels.keys().cloned().collect()
                };

                // 1. Stop removed or disabled channels
                for name in &current_channels {
                    let should_stop = match new_config.channels.get(name) {
                        None => {
                            info!("Channel '{}' removed from config, stopping...", name);
                            true
                        }
                        Some(cfg) if !cfg.enabled => {
                            info!("Channel '{}' disabled in config, stopping...", name);
                            true
                        }
                        _ => false,
                    };

                    if should_stop {
                        // Remove channel from state, then stop it outside the lock so the
                        // stop() call does not block other state access.
                        let channel = {
                            let mut channels = state.channels.channels.write().await;
                            channels.remove(name)
                        };
                        if let Some(channel) = channel {
                            if let Err(e) = channel.stop().await {
                                warn!("Failed to stop removed channel '{}': {}", name, e);
                            } else {
                                info!("✅ Stopped channel '{}'", name);
                            }
                        }
                    }
                }

                // 2. Handle new or modified channels
                for (name, new_channel_config) in &new_config.channels {
                    if !new_channel_config.enabled {
                        continue;
                    }

                    let existing = {
                        let channels = state.channels.channels.read().await;
                        channels.get(name).cloned()
                    };

                    match existing {
                        Some(_channel) => {
                            // Channel exists - check if config changed
                            let old_config = current_config.channels.get(name);
                            let config_changed = old_config
                                .map(|old| {
                                    old.credentials != new_channel_config.credentials
                                        || old.allow_from != new_channel_config.allow_from
                                        || old.block_from != new_channel_config.block_from
                                        || old.dm_policy != new_channel_config.dm_policy
                                })
                                .unwrap_or(true);

                            if config_changed {
                                info!("Channel '{}' config changed, restarting...", name);

                                // Stop the old channel before removing it from state.
                                let old_channel = {
                                    let mut channels = state.channels.channels.write().await;
                                    channels.remove(name)
                                };
                                if let Some(channel) = old_channel {
                                    if let Err(e) = channel.stop().await {
                                        warn!(
                                            "Failed to stop channel '{}' before restart: {}",
                                            name, e
                                        );
                                    }
                                }

                                // Start with new config
                                if let Err(e) = crate::gateway::init::channels::init_single_channel(
                                    state.clone(),
                                    &new_config,
                                    name,
                                    new_channel_config,
                                )
                                .await
                                {
                                    error!("Failed to restart channel '{}': {}", name, e);
                                } else {
                                    info!("✅ Restarted channel '{}' with new config", name);
                                }
                            }
                        }
                        None => {
                            // New channel - initialize it
                            info!(
                                "Hot-reloading new channel: {} ({:?})",
                                name, new_channel_config.channel_type
                            );

                            if let Err(e) = crate::gateway::init::channels::init_single_channel(
                                state.clone(),
                                &new_config,
                                name,
                                new_channel_config,
                            )
                            .await
                            {
                                error!("Failed to hot-reload channel '{}': {}", name, e);
                            } else {
                                info!("✅ Hot-reloaded channel '{}'", name);
                            }
                        }
                    }
                }

                Ok(())
            }
        })
        .await;

    // Handler for agent config changes
    {
        let state = state_agent;
        hot_reload
            .register_handler(ConfigFileType::Agent, move |event| {
                let state = state.clone();
                async move {
                    let agent_name = event
                        .path
                        .file_stem()
                        .and_then(|s| s.to_str())
                        .unwrap_or("unknown")
                        .to_string();
                    info!("Agent config changed for '{}': {:?}", agent_name, event.path);

                    let content = match tokio::fs::read_to_string(&event.path).await {
                        Ok(c) => c,
                        Err(e) => {
                            error!("Failed to read agent config {:?}: {}", event.path, e);
                            return Ok(());
                        }
                    };

                    let new_config: AgentConfig = match toml::from_str(&content) {
                        Ok(c) => c,
                        Err(e) => {
                            error!("Failed to parse agent config for '{}': {}", agent_name, e);
                            return Ok(());
                        }
                    };

                    // Send UpdateConfig to the running agent if it exists
                    let agents = state.agents.agents.read().await;
                    if let Some(handle) = agents.get(&agent_name) {
                        match handle.tx.send(AgentCommand::UpdateConfig(new_config)).await {
                            Ok(_) => {
                                info!("✅ Sent config update to agent '{}'", agent_name)
                            }
                            Err(e) => warn!(
                                "Failed to send config update to agent '{}': {}",
                                agent_name, e
                            ),
                        }
                    } else {
                        info!(
                            "Agent '{}' not currently running; config will apply on next start",
                            agent_name
                        );
                    }

                    Ok(())
                }
            })
            .await;
    }

    // Handler for channel config changes
    {
        let state = state_channel;
        let current_config = current_config_channel;
        hot_reload
            .register_handler(ConfigFileType::Channel, move |event| {
                let state = state.clone();
                let current_config = current_config.clone();
                async move {
                    let channel_name = event
                        .path
                        .file_stem()
                        .and_then(|s| s.to_str())
                        .unwrap_or("unknown")
                        .to_string();
                    info!("Channel config changed for '{}': {:?}", channel_name, event.path);

                    let content = match tokio::fs::read_to_string(&event.path).await {
                        Ok(c) => c,
                        Err(e) => {
                            error!("Failed to read channel config {:?}: {}", event.path, e);
                            return Ok(());
                        }
                    };

                    let new_channel_config: ChannelConfig = match toml::from_str(&content) {
                        Ok(c) => c,
                        Err(e) => {
                            error!("Failed to parse channel config for '{}': {}", channel_name, e);
                            return Ok(());
                        }
                    };

                    if !new_channel_config.enabled {
                        let channel = {
                            let mut channels = state.channels.channels.write().await;
                            channels.remove(&channel_name)
                        };
                        if let Some(channel) = channel {
                            if let Err(e) = channel.stop().await {
                                warn!("Failed to stop disabled channel '{}': {}", channel_name, e);
                            } else {
                                info!("✅ Stopped disabled channel '{}'", channel_name);
                            }
                        }
                        return Ok(());
                    }

                    // Stop existing channel before re-initializing with new config.
                    let old_channel = {
                        let mut channels = state.channels.channels.write().await;
                        channels.remove(&channel_name)
                    };
                    if let Some(channel) = old_channel {
                        if let Err(e) = channel.stop().await {
                            warn!("Failed to stop channel '{}' before reload: {}", channel_name, e);
                        }
                    }

                    // Re-initialize with new config
                    match crate::gateway::init::channels::init_single_channel(
                        state.clone(),
                        &current_config,
                        &channel_name,
                        &new_channel_config,
                    )
                    .await
                    {
                        Ok(_) => {
                            info!("✅ Hot-reloaded channel '{}' with updated config", channel_name)
                        }
                        Err(e) => {
                            error!("Failed to hot-reload channel '{}': {}", channel_name, e)
                        }
                    }

                    Ok(())
                }
            })
            .await;
    }

    // Handler for plugin config changes
    {
        let state = state_plugin;
        hot_reload
            .register_handler(ConfigFileType::Plugin, move |event| {
                let state = state.clone();
                async move {
                    let plugin_dir = event.path.parent().unwrap_or(&event.path).to_path_buf();
                    let plugin_id = plugin_dir
                        .file_name()
                        .and_then(|s| s.to_str())
                        .unwrap_or_default()
                        .to_string();

                    info!("Plugin file changed for '{}': {:?}", plugin_id, event.path);

                    if plugin_id.is_empty() {
                        warn!("Could not determine plugin ID from path {:?}", event.path);
                        return Ok(());
                    }

                    // Try state-preserving reload first
                    match state.infra.plugin_manager.reload_plugin(&plugin_id).await {
                        Ok(reloaded_id) => {
                            info!("✅ Reloaded plugin '{}' (preserved state)", reloaded_id);
                        }
                        Err(e) => {
                            warn!(
                                "State-preserving reload failed for '{}', falling back to \
                                 unload+load: {}",
                                plugin_id, e
                            );
                            match state.infra.plugin_manager.unload_plugin(&plugin_id).await {
                                Ok(true) => {
                                    match state.infra.plugin_manager.load_plugin(&plugin_dir).await
                                    {
                                        Ok(loaded_id) => {
                                            info!(
                                                "✅ Reloaded plugin '{}' (id={})",
                                                plugin_id, loaded_id
                                            )
                                        }
                                        Err(e) => {
                                            error!("Failed to reload plugin '{}': {}", plugin_id, e)
                                        }
                                    }
                                }
                                Ok(false) => {
                                    match state.infra.plugin_manager.load_plugin(&plugin_dir).await
                                    {
                                        Ok(loaded_id) => {
                                            info!(
                                                "✅ Loaded new plugin '{}' (id={})",
                                                plugin_id, loaded_id
                                            )
                                        }
                                        Err(e) => {
                                            warn!("Could not load plugin '{}': {}", plugin_id, e)
                                        }
                                    }
                                }
                                Err(e) => {
                                    error!("Failed to unload plugin '{}': {}", plugin_id, e)
                                }
                            }
                        }
                    }

                    Ok(())
                }
            })
            .await;
    }

    // Handler for gateway config changes
    {
        let state = state_gateway;
        hot_reload
            .register_handler(ConfigFileType::Gateway, move |event| {
                let state = state.clone();
                async move {
                    info!("Gateway config changed: {:?}", event.path);

                    // Snapshot before applying changes
                    let pre_snapshot = state.config.read().await.snapshot();

                    let content = match tokio::fs::read_to_string(&event.path).await {
                        Ok(c) => c,
                        Err(e) => {
                            error!("Failed to read gateway config {:?}: {}", event.path, e);
                            return Ok(());
                        }
                    };

                    let new_config: GatewayConfig = match toml::from_str(&content) {
                        Ok(c) => c,
                        Err(e) => {
                            error!("Failed to parse gateway config: {}", e);
                            return Ok(());
                        }
                    };

                    // Validate auth/security config before applying it.
                    if let Err(e) = super::validate_auth_config(&new_config) {
                        error!("Rejected invalid gateway hot-reload config: {}", e);
                        state
                            .auth
                            .audit_log
                            .log(
                                crate::security::runtime_audit::AuditEventType::ConfigChange,
                                "file_watcher",
                                "config",
                                false,
                                format!("Hot-reload config rejected: {}", e),
                                None,
                            )
                            .await;
                        return Ok(());
                    }

                    // Apply hot-reloadable fields (those that don't require server restart)
                    let search_config_changed = {
                        let config_guard = state.config.read().await;
                        config_guard.search != new_config.search
                    };
                    let mut config_guard = state.config.write().await;
                    let config = Arc::make_mut(&mut config_guard);
                    config.security = new_config.security;
                    config.providers = new_config.providers;
                    config.mcp = new_config.mcp;
                    config.hot_reload = new_config.hot_reload;
                    config.search = new_config.search;
                    drop(config_guard);
                    info!(
                        "✅ Applied gateway config updates (security, providers, mcp, search \
                         settings)"
                    );

                    // Rebuild just the WebSearchTool when search configuration changed so that
                    // new providers / API keys are picked up without a restart.
                    if search_config_changed {
                        info!("Search config changed, updating web_search tool...");
                        let search_providers = {
                            let cfg = state.config.read().await;
                            cfg.search.to_providers()
                        };

                        if let Some(providers) = state.tools.registry.web_search_providers() {
                            let mut guard = providers.write().await;
                            *guard = search_providers;
                            info!("✅ Updated web_search tool with new search config");
                        } else {
                            warn!(
                                "web_search providers not found in registry, cannot apply search \
                                 config hot-reload"
                            );
                        }
                    }

                    // Compute diff and log to audit
                    let post_config = state.config.read().await;
                    let changes = post_config.diff_since(&pre_snapshot);
                    drop(post_config);

                    if !changes.is_empty() {
                        let details = serde_json::to_value(&changes).unwrap_or_default();
                        state
                            .auth
                            .audit_log
                            .log(
                                crate::security::runtime_audit::AuditEventType::ConfigChange,
                                "file_watcher",
                                "config",
                                true,
                                format!("Hot-reload config change: {} field(s)", changes.len()),
                                Some(details),
                            )
                            .await;
                        info!(
                            changes = ?changes.iter().map(|c| &c.path).collect::<Vec<_>>(),
                            "Gateway config hot-reload changes"
                        );
                    }

                    Ok(())
                }
            })
            .await;
    }

    // Handler for KB config changes (kb.toml)
    {
        let state = state_kb;
        hot_reload
            .register_handler(ConfigFileType::KnowledgeBase, move |event| {
                let state = state.clone();
                async move {
                    // Extract agent ID from path: agents/{agent_id}/kb.toml
                    let agents_dir = state.paths.agents_dir();
                    let rel = match event.path.strip_prefix(&agents_dir) {
                        Ok(r) => r,
                        Err(_) => return Ok(()),
                    };
                    let agent_id = match rel.components().next() {
                        Some(c) => c.as_os_str().to_string_lossy().to_string(),
                        None => return Ok(()),
                    };

                    if event.change_type == ConfigChangeType::Deleted {
                        info!("kb.toml deleted for agent '{}' — no action needed", agent_id);
                        return Ok(());
                    }

                    info!("kb.toml changed for agent '{}', re-ingesting...", agent_id);

                    let kb_manager = state.memory.kb_manager.read().await.clone();
                    if let Some(ref manager) = kb_manager {
                        let report = manager.ingest_agent(&agent_id, false).await;
                        if report.errors.is_empty() {
                            info!(
                                "kb.toml re-ingest for '{}': {} indexed, {} skipped",
                                agent_id, report.docs_indexed, report.docs_skipped,
                            );
                        } else {
                            warn!(
                                "kb.toml re-ingest for '{}' had {} errors",
                                agent_id,
                                report.errors.len(),
                            );
                            for e in &report.errors {
                                warn!("  - {}", e);
                            }
                        }
                    } else {
                        warn!("KB manager not initialized, cannot re-ingest for '{}'", agent_id);
                    }

                    Ok(())
                }
            })
            .await;
    }

    info!("Registered hot reload handlers for all config types");
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::hot_reload::ConfigChangeEvent;
    use crate::gateway::state_tests::make_test_state;

    async fn state() -> Arc<GatewayState> {
        Arc::new(make_test_state(GatewayConfig::default()).await)
    }

    fn event(
        path: std::path::PathBuf,
        config_type: ConfigFileType,
        change_type: ConfigChangeType,
    ) -> ConfigChangeEvent {
        ConfigChangeEvent { path, config_type, change_type }
    }

    async fn write_toml<T: serde::Serialize>(
        dir: &tempfile::TempDir,
        name: &str,
        value: &T,
    ) -> std::path::PathBuf {
        let path = dir.path().join(name);
        let content = toml::to_string(value).expect("serialize toml");
        tokio::fs::write(&path, content).await.expect("write toml");
        path
    }

    #[tokio::test]
    async fn registers_all_config_type_handlers() {
        let manager = HotReloadManager::new().unwrap();
        let state = state().await;
        register_hot_reload_handlers(state.clone(), GatewayConfig::default(), &manager).await;

        // Dispatch events for each supported type; the handlers must not panic.
        let agent_tmp = tempfile::tempdir().unwrap();
        let agent_path = write_toml(&agent_tmp, "ghost.toml", &AgentConfig::default()).await;
        manager
            .dispatch_for_test(event(agent_path, ConfigFileType::Agent, ConfigChangeType::Modified))
            .await;

        let mut channel_cfg = ChannelConfig::new(crate::channels::ChannelType::Telegram);
        channel_cfg.enabled = false;
        let channel_tmp = tempfile::tempdir().unwrap();
        let channel_path = write_toml(&channel_tmp, "ghost.toml", &channel_cfg).await;
        manager
            .dispatch_for_test(event(
                channel_path,
                ConfigFileType::Channel,
                ConfigChangeType::Modified,
            ))
            .await;

        let gateway_tmp = tempfile::tempdir().unwrap();
        let gateway_path =
            write_toml(&gateway_tmp, "gateway.toml", &GatewayConfig::default()).await;
        manager
            .dispatch_for_test(event(
                gateway_path,
                ConfigFileType::Gateway,
                ConfigChangeType::Modified,
            ))
            .await;

        let agents_dir = state.paths.agents_dir();
        manager
            .dispatch_for_test(event(
                agents_dir.join("kb-agent").join("kb.toml"),
                ConfigFileType::KnowledgeBase,
                ConfigChangeType::Deleted,
            ))
            .await;

        manager
            .dispatch_for_test(event(
                agents_dir.join("kb-agent").join("kb.toml"),
                ConfigFileType::KnowledgeBase,
                ConfigChangeType::Modified,
            ))
            .await;

        manager
            .dispatch_for_test(event(
                std::path::PathBuf::from("/tmp/fake/plugins/ghost/plugin.toml"),
                ConfigFileType::Plugin,
                ConfigChangeType::Modified,
            ))
            .await;
    }

    #[tokio::test]
    async fn agent_handler_unreadable_path_ok() {
        let manager = HotReloadManager::new().unwrap();
        let state = state().await;
        register_hot_reload_handlers(state.clone(), GatewayConfig::default(), &manager).await;

        manager
            .dispatch_for_test(event(
                std::path::PathBuf::from("/tmp/does-not-exist/agent.toml"),
                ConfigFileType::Agent,
                ConfigChangeType::Modified,
            ))
            .await;
    }

    #[tokio::test]
    async fn gateway_handler_applies_search_and_audits() {
        let manager = HotReloadManager::new().unwrap();
        let state = state().await;
        register_hot_reload_handlers(state.clone(), GatewayConfig::default(), &manager).await;

        // Change search providers so the web_search rebuild + audit diff paths run.
        let mut new_config = GatewayConfig::default();
        new_config.search.providers = vec!["tavily".to_string()];
        let tmp = tempfile::tempdir().unwrap();
        let gateway_path = write_toml(&tmp, "gateway.toml", &new_config).await;

        manager
            .dispatch_for_test(event(
                gateway_path,
                ConfigFileType::Gateway,
                ConfigChangeType::Modified,
            ))
            .await;

        // The search providers field must now reflect the hot-reloaded config.
        let applied = state.config.read().await;
        assert_eq!(applied.search.providers, vec!["tavily".to_string()]);
    }

    #[tokio::test]
    async fn gateway_handler_rejects_invalid_auth_config() {
        let manager = HotReloadManager::new().unwrap();
        let state = state().await;
        register_hot_reload_handlers(state.clone(), GatewayConfig::default(), &manager).await;

        // auth_mode token without a shared token fails validation and is rejected.
        let mut new_config = GatewayConfig::default();
        new_config.security.enabled = true;
        new_config.security.auth_required = true;
        new_config.security.auth_mode = crate::gateway::protocol::AuthMode::Token;
        let tmp = tempfile::tempdir().unwrap();
        let gateway_path = write_toml(&tmp, "gateway.toml", &new_config).await;

        manager
            .dispatch_for_test(event(
                gateway_path,
                ConfigFileType::Gateway,
                ConfigChangeType::Modified,
            ))
            .await;

        // Config must be unchanged after rejection.
        let applied = state.config.read().await;
        assert_eq!(applied.security.auth_mode, crate::gateway::protocol::AuthMode::None);
    }
}
