//! Channel initialization functions.
//!
//! Extracted from `gateway/mod.rs`. Provides free functions for initializing
//! individual channels by type, as well as the top-level `init_channels` loop.
//! All functions take `state` / `config` explicitly instead of `&self`.

use std::sync::Arc;

// These imports and helpers are used by the external-channel initializers,
// all of which are feature-gated; a channel-free build (e.g. the mobile
// profile) has no callers for them.
#[allow(unused_imports)]
use tokio::sync::mpsc;
#[allow(unused_imports)]
use tracing::{error, info, warn};

use crate::channels::snapshot::healthy_snapshot;
use crate::channels::{Channel, ChannelExtension, ChannelType};
use crate::gateway::config::ChannelConfig;
use crate::gateway::GatewayState;

/// Register a channel background task in the unified [`TaskRegistry`].
async fn register_channel_task(
    state: &Arc<GatewayState>,
    name: &str,
    kind: &str,
    handle: tokio::task::JoinHandle<()>,
) {
    state
        .task_registry
        .insert_join(format!("channel:{}:{}", name, kind), handle)
        .await;
}

/// Resolve a channel credential: the secret store (namespace `channel`) is
/// authoritative, with the legacy plaintext `credentials` map as fallback for
/// pre-migration configs.
#[allow(dead_code)] // only called from feature-gated channel initializers
async fn channel_cred(
    secrets: &crate::secrets::SecretStoreHandle,
    name: &str,
    config: &ChannelConfig,
    key: &str,
) -> Option<String> {
    secrets
        .resolve_channel_credential(name, key, config.credentials.get(key).map(String::as_str))
        .await
        .ok()
        .flatten()
        .map(crate::secrets::SecretValue::into_inner)
}

/// Initialize all configured channels.
pub(crate) async fn init_channels(
    state: Arc<GatewayState>,
    config: &crate::gateway::config::GatewayConfig,
) -> crate::Result<()> {
    info!("Initializing {} configured channels", config.channels.len());

    for (name, channel_config) in &config.channels {
        if !channel_config.enabled {
            info!("Channel {} is disabled, skipping", name);
            continue;
        }

        // Check if channel already running
        if state.channels.channels.read().await.contains_key(name) {
            info!("Channel {} already running, skipping", name);
            continue;
        }

        init_single_channel(state.clone(), config, name, channel_config).await?;
    }

    // Discover and start WASM plugin channels
    #[cfg(feature = "plugins")]
    init_plugin_channels(state, config).await?;

    Ok(())
}

/// Discover and start WASM plugin channels.
#[cfg(feature = "plugins")]
pub(crate) async fn init_plugin_channels(
    state: Arc<GatewayState>,
    _config: &crate::gateway::config::GatewayConfig,
) -> crate::Result<()> {
    use crate::channels::plugin_host::PluginChannelRegistry;

    let plugin_dir = state.paths.extensions_dir().join("channels");
    if !plugin_dir.exists() {
        info!("Plugin channel directory does not exist, skipping: {:?}", plugin_dir);
        return Ok(());
    }

    // Create a shared inbound message channel for plugin channels and bridge it
    // into the unified inbound entry so messages from WASM plugins are not lost.
    let (plugin_inbound_tx, mut plugin_inbound_rx) = tokio::sync::mpsc::unbounded_channel();
    let state_clone = state.clone();
    let bridge_handle = tokio::spawn(async move {
        while let Some(message) = plugin_inbound_rx.recv().await {
            if let Err(e) = state_clone.pipelines.inbound_entry.send(message).await {
                warn!("Failed to submit plugin message to inbound entry: {}", e);
            }
        }
    });
    register_channel_task(&state, "plugin", "inbound", bridge_handle).await;

    let registry = PluginChannelRegistry::new(plugin_dir, plugin_inbound_tx);
    let available = registry.discover_plugins().await?;

    if available.is_empty() {
        info!("No WASM channel plugins found");
        return Ok(());
    }

    for (name, path) in &available {
        info!("Discovered WASM channel plugin '{}' at {:?}", name, path);
    }

    for (name, _path) in &available {
        // Skip if a native channel with the same name is already running
        if state.channels.channels.read().await.contains_key(name) {
            info!("Channel '{}' already running as native, skipping plugin", name);
            continue;
        }

        match registry.load_plugin(name, None).await {
            Ok(plugin) => {
                info!("Loaded WASM channel plugin '{}'", name);
                // Register in the channel map
                let channel: Arc<dyn crate::channels::Channel> = plugin.clone();
                state
                    .channels
                    .channels
                    .write()
                    .await
                    .insert(name.clone(), channel.clone());

                // Start the plugin channel in a background task so init is not
                // blocked and the task is tracked for shutdown.
                let plugin_clone = plugin.clone();
                let plugin_name = name.clone();
                let start_handle = tokio::spawn(async move {
                    if let Err(e) = plugin_clone.start().await {
                        warn!("WASM channel plugin '{}' failed: {}", plugin_name, e);
                    }
                });
                register_channel_task(&state, name, "main", start_handle).await;

                // Wire health monitoring
                if let Some(ref monitor) = state.channels.health_monitor {
                    let check_interval = std::time::Duration::from_secs(30);
                    let transport_timeout = std::time::Duration::from_secs(10);
                    let monitor_handle = monitor.monitor_channel_with_timeout(
                        name,
                        channel,
                        check_interval,
                        transport_timeout,
                    );
                    register_channel_task(&state, name, "monitor", monitor_handle).await;
                }

                // Record snapshot
                if let Some(ref store) = state.channels.snapshot_store {
                    let snap = healthy_snapshot(name, None);
                    store.store(snap).await;
                }

                info!("WASM channel plugin '{}' initialized successfully", name);
            }
            Err(e) => {
                warn!("Failed to load WASM channel plugin '{}': {}", name, e);
            }
        }
    }

    Ok(())
}

/// Initialize a single channel by name and config.
pub(crate) async fn init_single_channel(
    state: Arc<GatewayState>,
    _config: &crate::gateway::config::GatewayConfig,
    name: &str,
    channel_config: &ChannelConfig,
) -> crate::Result<()> {
    info!("Initializing channel {} ({:?})", name, channel_config.channel_type);

    match channel_config.channel_type {
        ChannelType::Telegram => {
            #[cfg(feature = "telegram")]
            {
                init_telegram_channel(state.clone(), name, channel_config).await?;
            }
            #[cfg(not(feature = "telegram"))]
            {
                warn!("Telegram feature not enabled, skipping channel '{}'", name);
            }
        }
        ChannelType::Discord => {
            #[cfg(feature = "discord")]
            {
                init_discord_channel(state.clone(), name, channel_config).await?;
            }
            #[cfg(not(feature = "discord"))]
            {
                warn!("Discord feature not enabled, skipping channel '{}'", name);
            }
        }
        ChannelType::Slack => {
            #[cfg(feature = "slack")]
            {
                init_slack_channel(state.clone(), name, channel_config).await?;
            }
            #[cfg(not(feature = "slack"))]
            {
                warn!("Slack feature not enabled, skipping channel '{}'", name);
            }
        }
        ChannelType::Whatsapp => {
            #[cfg(feature = "whatsapp")]
            {
                init_whatsapp_channel(state.clone(), name, channel_config).await?;
            }
            #[cfg(not(feature = "whatsapp"))]
            {
                warn!("WhatsApp feature not enabled, skipping channel '{}'", name);
            }
        }
        ChannelType::Qq => {
            #[cfg(feature = "qq")]
            {
                init_qq_channel(state.clone(), name, channel_config).await?;
            }
            #[cfg(not(feature = "qq"))]
            {
                warn!("QQ feature not enabled, skipping channel '{}'", name);
            }
        }
        ChannelType::Feishu => {
            #[cfg(feature = "feishu")]
            {
                init_feishu_channel(state.clone(), name, channel_config).await?;
            }
            #[cfg(not(feature = "feishu"))]
            {
                warn!("Feishu/Lark feature not enabled, skipping channel '{}'", name);
            }
        }
        ChannelType::WebTerminal => {
            info!(
                "Channel '{}' (WebTerminal) uses Gateway WS/API directly, skipping adapter spawn",
                name
            );
        }
        ChannelType::Websocket => {
            info!("WebSocket channel '{}' requires external connection", name);
        }
        ChannelType::Signal => {
            #[cfg(feature = "signal")]
            {
                info!("Signal channel '{}' initialized (signal-cli daemon required)", name);
            }
            #[cfg(not(feature = "signal"))]
            {
                warn!("Signal feature not enabled, skipping channel '{}'", name);
            }
        }
        ChannelType::Imessage => {
            #[cfg(feature = "imessage")]
            {
                info!("iMessage channel '{}' initialized (BlueBubbles required)", name);
            }
            #[cfg(not(feature = "imessage"))]
            {
                warn!("iMessage feature not enabled, skipping channel '{}'", name);
            }
        }
        ChannelType::Webchat => {
            #[cfg(feature = "webchat")]
            {
                info!("WebChat channel '{}' initialized", name);
            }
            #[cfg(not(feature = "webchat"))]
            {
                warn!("WebChat feature not enabled, skipping channel '{}'", name);
            }
        }
        ChannelType::WechatMp => {
            #[cfg(feature = "wechatmp")]
            {
                init_wechatmp_channel(state.clone(), name, channel_config).await?;
            }
            #[cfg(not(feature = "wechatmp"))]
            {
                warn!("WeChat MP feature not enabled, skipping channel '{}'", name);
            }
        }
        ChannelType::Line => {
            #[cfg(feature = "line")]
            {
                init_line_channel(state.clone(), name, channel_config).await?;
            }
            #[cfg(not(feature = "line"))]
            {
                warn!("LINE feature not enabled, skipping channel '{}'", name);
            }
        }
        ChannelType::Twilio => {
            #[cfg(feature = "twilio")]
            {
                init_twilio_channel(state.clone(), name, channel_config).await?;
            }
            #[cfg(not(feature = "twilio"))]
            {
                warn!("Twilio feature not enabled, skipping channel '{}'", name);
            }
        }
        ChannelType::GoogleChat => {
            #[cfg(feature = "googlechat")]
            {
                init_googlechat_channel(state.clone(), name, channel_config).await?;
            }
            #[cfg(not(feature = "googlechat"))]
            {
                warn!("Google Chat feature not enabled, skipping channel '{}'", name);
            }
        }
        ChannelType::Teams => {
            #[cfg(feature = "teams")]
            {
                init_teams_channel(state.clone(), name, channel_config).await?;
            }
            #[cfg(not(feature = "teams"))]
            {
                warn!("Teams feature not enabled, skipping channel '{}'", name);
            }
        }
        ChannelType::Wecom => {
            #[cfg(feature = "wecom")]
            {
                init_wecom_channel(state.clone(), name, channel_config).await?;
            }
            #[cfg(not(feature = "wecom"))]
            {
                warn!("WeCom feature not enabled, skipping channel '{}'", name);
            }
        }
        ChannelType::DingTalk => {
            #[cfg(feature = "dingtalk")]
            {
                init_dingtalk_channel(state.clone(), name, channel_config).await?;
            }
            #[cfg(not(feature = "dingtalk"))]
            {
                warn!("DingTalk feature not enabled, skipping channel '{}'", name);
            }
        }
        ChannelType::Irc => {
            #[cfg(feature = "irc")]
            {
                init_irc_channel(state.clone(), name, channel_config).await?;
            }
            #[cfg(not(feature = "irc"))]
            {
                warn!("IRC feature not enabled, skipping channel '{}'", name);
            }
        }
    }

    // Record a healthy snapshot after successful channel initialization
    if let Some(ref store) = state.channels.snapshot_store {
        let snap = healthy_snapshot(name, None);
        store.store(snap).await;
    }

    // Start health monitoring if configured
    if let Some(ref monitor) = state.channels.health_monitor {
        let channels = state.channels.channels.read().await;
        if let Some(channel) = channels.get(name).cloned() {
            drop(channels);
            let check_interval = std::time::Duration::from_secs(30);
            let monitor_handle = monitor.monitor_channel(name, channel, check_interval);
            register_channel_task(&state, name, "monitor", monitor_handle).await;
            info!("Started health monitoring for channel '{}'", name);
        } else {
            warn!("Channel '{}' not found in registry for health monitoring", name);
        }
    }

    Ok(())
}

// ── Channel-specific inits ──────────────────────────────────────────

/// Initialize Telegram channel via ChannelExtension (skeleton alignment)
#[cfg(feature = "telegram")]
pub(crate) async fn init_telegram_channel(
    state: Arc<GatewayState>,
    name: &str,
    config: &ChannelConfig,
) -> crate::Result<()> {
    if let Some(token) = channel_cred(&state.secrets, name, config, "token").await {
        let telegram_config = crate::channels::telegram::TelegramConfig::new(token)
            .allow_usernames(config.allow_from.clone());

        let channel = Arc::new(crate::channels::telegram::TelegramChannel::new(telegram_config));

        // Agent routing is now handled by the InboundPipeline (AgentRouter)
        let agent_name = config.agent_id.as_deref().unwrap_or("default");
        info!(
            "Telegram channel '{}' will route via InboundPipeline (default agent: '{}')",
            name, agent_name
        );

        // Set channel default so the router knows which agent to use for Telegram
        state
            .agents
            .router
            .set_channel_default(name, agent_name.to_string(), None)
            .await;

        // Create the channel extension
        let ext = Arc::new(crate::channels::TelegramChannelExtension::new(
            channel.clone(),
            state.channels.session_channels.clone(),
        ));

        // Create inbound channel: extension -> inbound pipeline
        let (inbound_tx, mut inbound_rx) = mpsc::channel::<crate::channels::IncomingMessage>(1000);

        // Spawn extension inbound task (Telegram bot -> inbound pipeline)
        let ext_inbound = ext.clone();
        let inbound_handle = tokio::spawn(async move {
            if let Err(e) = ext_inbound.run_inbound(inbound_tx).await {
                error!("Telegram extension inbound task failed: {}", e);
            }
        });
        register_channel_task(&state, name, "inbound", inbound_handle).await;

        // Bridge inbound messages into the unified entry channel
        let state_clone = state.clone();
        let bridge_handle = tokio::spawn(async move {
            while let Some(message) = inbound_rx.recv().await {
                if let Err(e) = state_clone.pipelines.inbound_entry.send(message).await {
                    warn!("Failed to submit Telegram message to inbound entry: {}", e);
                }
            }
        });
        register_channel_task(&state, name, "bridge", bridge_handle).await;

        // Create outbound channel: reply dispatcher -> extension outbound
        let (outbound_tx, outbound_rx) = mpsc::channel::<crate::channels::OutgoingMessage>(1000);

        // Spawn extension outbound task (outbound pipeline -> Telegram)
        let ext_outbound = ext.clone();
        let outbound_handle = tokio::spawn(async move {
            if let Err(e) = ext_outbound.run_outbound(outbound_rx).await {
                error!("Telegram extension outbound task failed: {}", e);
            }
        });
        register_channel_task(&state, name, "outbound", outbound_handle).await;

        // Register a bridge with the reply dispatcher so outbound pipeline
        // messages flow into the extension's run_outbound.
        let bridge = Arc::new(crate::channels::ChannelSenderBridge::new(name, outbound_tx));
        state
            .channels
            .reply_dispatcher
            .register_channel(name, bridge)
            .await;

        // Register extension in the extension registry
        register_channel_extension(&state, ext).await;

        // Keep the raw channel in the channels map for direct access
        state
            .channels
            .channels
            .write()
            .await
            .insert(name.to_string(), channel);
        info!("✅ Telegram channel '{}' initialized via ChannelExtension", name);
    } else {
        warn!("Telegram channel '{}' missing 'token' in credentials", name);
    }
    Ok(())
}

/// Initialize Discord channel
#[cfg(feature = "discord")]
pub(crate) async fn init_discord_channel(
    state: Arc<GatewayState>,
    name: &str,
    config: &ChannelConfig,
) -> crate::Result<()> {
    if let Some(token) = channel_cred(&state.secrets, name, config, "token").await {
        // Create inbound bridge: Discord message_tx -> inbound pipeline
        let (inbound_tx, mut inbound_rx) =
            mpsc::unbounded_channel::<crate::channels::IncomingMessage>();
        let mut discord_config = crate::channels::discord::DiscordConfig::new(token);
        discord_config.message_tx = Some(inbound_tx);

        let channel = Arc::new(crate::channels::discord::DiscordChannel::new(discord_config));

        // Bridge inbound messages into the unified entry channel
        let state_clone = state.clone();
        let bridge_handle = tokio::spawn(async move {
            while let Some(msg) = inbound_rx.recv().await {
                if let Err(e) = state_clone.pipelines.inbound_entry.send(msg).await {
                    warn!("Failed to submit message to inbound entry: {}", e);
                }
            }
        });
        register_channel_task(&state, name, "bridge", bridge_handle).await;

        let channel_name = name.to_string();
        let channel_for_task = channel.clone();
        let main_handle = tokio::spawn(async move {
            if let Err(e) = channel_for_task.start().await {
                error!("Discord channel {} failed: {}", channel_name, e);
            }
        });
        register_channel_task(&state, name, "main", main_handle).await;
        state
            .channels
            .reply_dispatcher
            .register_channel(name, channel.clone())
            .await;
        state
            .channels
            .channels
            .write()
            .await
            .insert(name.to_string(), channel);
        info!("✅ Discord channel '{}' initialized", name);
    } else {
        warn!("Discord channel '{}' missing 'token' in credentials", name);
    }
    Ok(())
}

/// Initialize Slack channel
#[cfg(feature = "slack")]
pub(crate) async fn init_slack_channel(
    state: Arc<GatewayState>,
    name: &str,
    config: &ChannelConfig,
) -> crate::Result<()> {
    if let Some(token) = channel_cred(&state.secrets, name, config, "token").await {
        // Create inbound bridge: Slack message_tx (Socket Mode) -> inbound pipeline
        let (inbound_tx, mut inbound_rx) =
            mpsc::unbounded_channel::<crate::channels::IncomingMessage>();
        let mut slack_config = crate::channels::slack::SlackConfig::new(token);
        slack_config.message_tx = Some(inbound_tx);
        if let Some(app_token) = channel_cred(&state.secrets, name, config, "app_token").await {
            slack_config.app_token = Some(app_token);
        }

        let channel = Arc::new(crate::channels::slack::SlackChannel::new(slack_config));

        // Bridge inbound messages into the unified entry channel
        let state_clone = state.clone();
        let bridge_handle = tokio::spawn(async move {
            while let Some(msg) = inbound_rx.recv().await {
                if let Err(e) = state_clone.pipelines.inbound_entry.send(msg).await {
                    warn!("Failed to submit message to inbound entry: {}", e);
                }
            }
        });
        register_channel_task(&state, name, "bridge", bridge_handle).await;

        let channel_name = name.to_string();
        let channel_for_task = channel.clone();
        let main_handle = tokio::spawn(async move {
            if let Err(e) = channel_for_task.start().await {
                error!("Slack channel {} failed: {}", channel_name, e);
            }
        });
        register_channel_task(&state, name, "main", main_handle).await;
        state
            .channels
            .reply_dispatcher
            .register_channel(name, channel.clone())
            .await;
        state
            .channels
            .channels
            .write()
            .await
            .insert(name.to_string(), channel);
        info!("✅ Slack channel '{}' initialized", name);
    } else {
        warn!("Slack channel '{}' missing 'token' in credentials", name);
    }
    Ok(())
}

/// Initialize WhatsApp channel
#[cfg(feature = "whatsapp")]
pub(crate) async fn init_whatsapp_channel(
    state: Arc<GatewayState>,
    name: &str,
    config: &ChannelConfig,
) -> crate::Result<()> {
    if let (Some(phone_id), Some(token)) = (
        config.credentials.get("phone_number_id"),
        channel_cred(&state.secrets, name, config, "access_token").await,
    ) {
        let whatsapp_config = crate::channels::whatsapp::WhatsappConfig::new(phone_id, token);

        let channel = Arc::new(crate::channels::whatsapp::WhatsappChannel::new(whatsapp_config));
        let channel_name = name.to_string();
        let channel_for_task = channel.clone();
        let main_handle = tokio::spawn(async move {
            if let Err(e) = channel_for_task.start().await {
                error!("WhatsApp channel {} failed: {}", channel_name, e);
            }
        });
        register_channel_task(&state, name, "main", main_handle).await;
        state
            .channels
            .reply_dispatcher
            .register_channel(name, channel.clone())
            .await;
        state
            .channels
            .channels
            .write()
            .await
            .insert(name.to_string(), channel);
        info!("✅ WhatsApp channel '{}' initialized", name);
    } else {
        warn!(
            "WhatsApp channel '{}' missing 'phone_number_id' or 'access_token' in credentials",
            name
        );
    }
    Ok(())
}

/// Initialize Feishu/Lark channel (outbound via ReplyDispatcher)
#[cfg(feature = "feishu")]
pub(crate) async fn init_feishu_channel(
    state: Arc<GatewayState>,
    name: &str,
    config: &ChannelConfig,
) -> crate::Result<()> {
    if let (Some(app_id), Some(app_secret)) = (
        config.credentials.get("app_id"),
        channel_cred(&state.secrets, name, config, "app_secret").await,
    ) {
        let lark_config = crate::channels::lark::LarkConfig::new(app_id, app_secret);

        let channel = Arc::new(crate::channels::lark::LarkChannel::new(lark_config));
        let channel_name = name.to_string();
        let channel_for_task = channel.clone();
        let main_handle = tokio::spawn(async move {
            if let Err(e) = channel_for_task.start().await {
                error!("Feishu channel {} failed: {}", channel_name, e);
            }
        });
        register_channel_task(&state, name, "main", main_handle).await;
        state
            .channels
            .reply_dispatcher
            .register_channel(name, channel.clone())
            .await;
        state
            .channels
            .channels
            .write()
            .await
            .insert(name.to_string(), channel);
        info!("✅ Feishu channel '{}' initialized (inbound via webhook)", name);
    } else {
        warn!("Feishu channel '{}' missing 'app_id' or 'app_secret' in credentials", name);
    }
    Ok(())
}

/// Initialize WeChat Official Account (公众号) channel.
///
/// Inbound arrives via the encrypted webhook in `gateway/webhooks.rs`; this
/// only registers the outbound (customer-service message) channel.
#[cfg(feature = "wechatmp")]
pub(crate) async fn init_wechatmp_channel(
    state: Arc<GatewayState>,
    name: &str,
    config: &ChannelConfig,
) -> crate::Result<()> {
    use crate::channels::wechatmp::{WechatMpChannel, WechatMpConfig};

    if let (Some(app_id), Some(app_secret), Some(token), Some(encoding_aes_key)) = (
        config.credentials.get("app_id"),
        channel_cred(&state.secrets, name, config, "app_secret").await,
        channel_cred(&state.secrets, name, config, "token").await,
        channel_cred(&state.secrets, name, config, "encoding_aes_key").await,
    ) {
        let mp_config = WechatMpConfig {
            app_id: app_id.clone(),
            app_secret,
            token,
            encoding_aes_key,
        };
        let channel = Arc::new(WechatMpChannel::new(mp_config));

        let channel_name = name.to_string();
        let channel_for_task = channel.clone();
        let main_handle = tokio::spawn(async move {
            if let Err(e) = channel_for_task.start().await {
                error!("WeChat MP channel {} failed: {}", channel_name, e);
            }
        });
        register_channel_task(&state, name, "main", main_handle).await;
        state
            .channels
            .reply_dispatcher
            .register_channel(name, channel.clone())
            .await;
        state
            .channels
            .channels
            .write()
            .await
            .insert(name.to_string(), channel);
        info!("✅ WeChat MP channel '{}' initialized (inbound via encrypted webhook)", name);
    } else {
        warn!(
            "WeChat MP channel '{}' missing 'app_id'/'app_secret'/'token'/'encoding_aes_key' \
             in credentials",
            name
        );
    }
    Ok(())
}

/// Initialize LINE channel.
///
/// Inbound arrives via the signed webhook in `gateway/webhooks.rs`; this only
/// registers the outbound (push) channel.
#[cfg(feature = "line")]
pub(crate) async fn init_line_channel(
    state: Arc<GatewayState>,
    name: &str,
    config: &ChannelConfig,
) -> crate::Result<()> {
    use crate::channels::line::{LineChannel, LineConfig};

    if let (Some(access_token), Some(channel_secret)) = (
        channel_cred(&state.secrets, name, config, "channel_access_token").await,
        channel_cred(&state.secrets, name, config, "channel_secret").await,
    ) {
        let channel = Arc::new(LineChannel::new(LineConfig::new(access_token, channel_secret)));

        let channel_name = name.to_string();
        let channel_for_task = channel.clone();
        let main_handle = tokio::spawn(async move {
            if let Err(e) = channel_for_task.start().await {
                error!("LINE channel {} failed: {}", channel_name, e);
            }
        });
        register_channel_task(&state, name, "main", main_handle).await;
        state
            .channels
            .reply_dispatcher
            .register_channel(name, channel.clone())
            .await;
        state
            .channels
            .channels
            .write()
            .await
            .insert(name.to_string(), channel);
        info!("✅ LINE channel '{}' initialized (inbound via signed webhook)", name);
    } else {
        warn!(
            "LINE channel '{}' missing 'channel_access_token'/'channel_secret' in credentials",
            name
        );
    }
    Ok(())
}

/// Initialize Twilio SMS channel.
///
/// Inbound arrives via the signed webhook in `gateway/webhooks.rs`; this only
/// registers the outbound (Messages API) channel.
#[cfg(feature = "twilio")]
pub(crate) async fn init_twilio_channel(
    state: Arc<GatewayState>,
    name: &str,
    config: &ChannelConfig,
) -> crate::Result<()> {
    use crate::channels::twilio::{TwilioChannel, TwilioConfig};

    if let (Some(account_sid), Some(auth_token), Some(from_number), Some(webhook_url)) = (
        channel_cred(&state.secrets, name, config, "account_sid").await,
        channel_cred(&state.secrets, name, config, "auth_token").await,
        channel_cred(&state.secrets, name, config, "from_number").await,
        channel_cred(&state.secrets, name, config, "webhook_url").await,
    ) {
        let channel = Arc::new(TwilioChannel::new(TwilioConfig::new(
            account_sid,
            auth_token,
            from_number,
            webhook_url,
        )));

        let channel_name = name.to_string();
        let channel_for_task = channel.clone();
        let main_handle = tokio::spawn(async move {
            if let Err(e) = channel_for_task.start().await {
                error!("Twilio channel {} failed: {}", channel_name, e);
            }
        });
        register_channel_task(&state, name, "main", main_handle).await;
        state
            .channels
            .reply_dispatcher
            .register_channel(name, channel.clone())
            .await;
        state
            .channels
            .channels
            .write()
            .await
            .insert(name.to_string(), channel);
        info!("✅ Twilio SMS channel '{}' initialized (inbound via signed webhook)", name);
    } else {
        warn!(
            "Twilio channel '{}' missing \
             'account_sid'/'auth_token'/'from_number'/'webhook_url' in credentials",
            name
        );
    }
    Ok(())
}

/// Initialize Google Chat channel.
///
/// Inbound arrives via the bearer-token webhook in `gateway/webhooks.rs`. The
/// two halves are set up separately because they have separate credentials: the
/// verification side needs the issuer, audience and key URL, and the outbound
/// side needs a service account. A deployment can legitimately have one without
/// the other, and — more to the point — a missing outbound key must not quietly
/// take the inbound check down with it.
#[cfg(feature = "googlechat")]
pub(crate) async fn init_googlechat_channel(
    state: Arc<GatewayState>,
    name: &str,
    config: &ChannelConfig,
) -> crate::Result<()> {
    use crate::channels::googlechat::{GoogleChatChannel, GoogleChatConfig};

    let (Some(issuer), Some(audience), Some(jwks_url)) = (
        channel_cred(&state.secrets, name, config, "issuer").await,
        channel_cred(&state.secrets, name, config, "audience").await,
        channel_cred(&state.secrets, name, config, "jwks_url").await,
    ) else {
        warn!(
            "Google Chat channel '{}' missing one of 'issuer'/'audience'/'jwks_url' — deliveries \
             cannot be verified and will be refused",
            name
        );
        return Ok(());
    };

    // The identity allowlist is read rather than defaulted: an absent setting
    // has to mean "nobody", because a token whose signature verifies but whose
    // identity is unlisted must not be acted on.
    let allowed_service_accounts =
        channel_cred(&state.secrets, name, config, "allowed_service_accounts")
            .await
            .unwrap_or_default()
            .split(',')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .collect::<Vec<_>>();

    if allowed_service_accounts.is_empty() {
        warn!(
            "Google Chat channel '{}' has no 'allowed_service_accounts' — every delivery will be \
             refused until one is configured",
            name
        );
    }

    // Registered before the outbound half is even looked at: this is what the
    // webhook handler needs, and it has to outlive a request because it caches
    // the issuer's key set.
    state.channels.webhook_verifiers.write().await.insert(
        name.to_string(),
        Arc::new(crate::gateway::jwt_verify::JwtVerifier::new(&issuer, &audience, &jwks_url)),
    );
    info!("Google Chat channel '{}': deliveries verified against issuer {}", name, issuer);

    let (Some(client_email), Some(private_key_pem)) = (
        channel_cred(&state.secrets, name, config, "client_email").await,
        channel_cred(&state.secrets, name, config, "private_key_pem").await,
    ) else {
        warn!(
            "Google Chat channel '{}' has no service account ('client_email'/'private_key_pem'), \
             so replies cannot be posted",
            name
        );
        return Ok(());
    };

    let channel = Arc::new(GoogleChatChannel::new(GoogleChatConfig {
        issuer,
        audience,
        jwks_url,
        allowed_service_accounts,
        client_email,
        private_key_pem,
    }));

    let channel_name = name.to_string();
    let channel_for_task = channel.clone();
    let main_handle = tokio::spawn(async move {
        if let Err(e) = channel_for_task.start().await {
            error!("Google Chat channel {} failed: {}", channel_name, e);
        }
    });
    register_channel_task(&state, name, "main", main_handle).await;
    state
        .channels
        .reply_dispatcher
        .register_channel(name, channel.clone())
        .await;
    state
        .channels
        .channels
        .write()
        .await
        .insert(name.to_string(), channel);
    info!(
        "✅ Google Chat channel '{}' initialized (inbound via bearer-token webhook)",
        name
    );

    Ok(())
}

/// Initialize Microsoft Teams channel.
///
/// Inbound arrives via the Bot Framework webhook in `gateway/webhooks.rs`. As
/// with Google Chat, the two halves are set up separately because they have
/// separate credentials — verification needs the app id, the issuer and the key
/// URL, sending needs the tenant and the app secret — and a deployment can
/// legitimately have one without the other. A missing secret must not quietly
/// take the inbound check down with it, which is why the outbound half is looked
/// at last.
#[cfg(feature = "teams")]
pub(crate) async fn init_teams_channel(
    state: Arc<GatewayState>,
    name: &str,
    config: &ChannelConfig,
) -> crate::Result<()> {
    use crate::channels::teams::{TeamsChannel, TeamsConfig};

    let (Some(app_id), Some(issuer), Some(jwks_url)) = (
        channel_cred(&state.secrets, name, config, "app_id").await,
        channel_cred(&state.secrets, name, config, "issuer").await,
        channel_cred(&state.secrets, name, config, "jwks_url").await,
    ) else {
        warn!(
            "Teams channel '{}' missing one of 'app_id'/'issuer'/'jwks_url' — activities \
             cannot be verified and will be refused",
            name
        );
        return Ok(());
    };

    // Registered before the outbound half is even looked at: this is what the
    // webhook handler needs, and it has to outlive a request because it caches
    // the issuer's key set. The app id is the audience, which is the same value
    // the outbound half sends as.
    state.channels.webhook_verifiers.write().await.insert(
        name.to_string(),
        Arc::new(crate::gateway::jwt_verify::JwtVerifier::new(&issuer, &app_id, &jwks_url)),
    );
    info!("Teams channel '{}': activities verified against issuer {}", name, issuer);

    let (Some(tenant_id), Some(client_secret)) = (
        channel_cred(&state.secrets, name, config, "tenant_id").await,
        channel_cred(&state.secrets, name, config, "client_secret").await,
    ) else {
        warn!(
            "Teams channel '{}' has no app registration secret ('tenant_id'/'client_secret'), \
             so replies cannot be posted",
            name
        );
        return Ok(());
    };

    let channel = Arc::new(TeamsChannel::new(TeamsConfig {
        issuer,
        app_id,
        jwks_url,
        tenant_id,
        client_secret,
    }));

    let channel_name = name.to_string();
    let channel_for_task = channel.clone();
    let main_handle = tokio::spawn(async move {
        if let Err(e) = channel_for_task.start().await {
            error!("Teams channel {} failed: {}", channel_name, e);
        }
    });
    register_channel_task(&state, name, "main", main_handle).await;
    state
        .channels
        .reply_dispatcher
        .register_channel(name, channel.clone())
        .await;
    state
        .channels
        .channels
        .write()
        .await
        .insert(name.to_string(), channel);
    info!("✅ Teams channel '{}' initialized (inbound via Bot Framework webhook)", name);

    Ok(())
}

/// Initialize WeCom (企业微信) AI Bot channel.
///
/// The first channel with no webhook route: the gateway holds a WebSocket to
/// WeCom and the same socket carries messages both ways. What is new is
/// therefore only the transport — inbound goes through
/// [`dispatch_channel_message_to`](crate::gateway::webhooks::dispatch_channel_message_to),
/// the tail every other direct-message channel uses, so the access check, the
/// session keying and the `session_channels` mapping that reply addressing
/// depends on are shared rather than reimplemented.
///
/// The bridge is a task of its own rather than a closure inside the connection
/// loop, because the loop has to survive a reconnect and the bridge has to
/// survive it too — and because both need to be in the `TaskRegistry` for
/// shutdown to join them.
#[cfg(feature = "wecom")]
pub(crate) async fn init_wecom_channel(
    state: Arc<GatewayState>,
    name: &str,
    config: &ChannelConfig,
) -> crate::Result<()> {
    use crate::channels::wecom::{WeComChannel, WeComConfig};

    let (Some(bot_id), Some(secret)) = (
        channel_cred(&state.secrets, name, config, "bot_id").await,
        channel_cred(&state.secrets, name, config, "secret").await,
    ) else {
        warn!(
            "WeCom channel '{}' missing 'bot_id'/'secret' — it cannot authenticate, so it is \
             not started",
            name
        );
        return Ok(());
    };

    let (inbound_tx, mut inbound_rx) =
        mpsc::unbounded_channel::<crate::channels::IncomingMessage>();

    let mut wecom_config = WeComConfig::new(bot_id, secret);
    wecom_config.message_tx = Some(inbound_tx);
    if let Some(url) = channel_cred(&state.secrets, name, config, "ws_url").await {
        wecom_config.ws_url = url;
    }

    let channel = Arc::new(WeComChannel::new(wecom_config));

    // Bridge: every message the socket delivers goes through the shared tail,
    // which is what applies access control, honours `/new`, and records which
    // channel owns the session so the agent's reply can be routed back.
    let state_for_bridge = state.clone();
    let channel_name = name.to_string();
    let bridge_handle = tokio::spawn(async move {
        while let Some(msg) = inbound_rx.recv().await {
            // The mention state travels with the message because it is what
            // tells the inbound stage whether an unmentioned group message
            // should be suppressed — and a group reported as a direct message
            // has its bot answer everything in it.
            crate::gateway::webhooks::dispatch_channel_message_to(
                &state_for_bridge,
                &channel_name,
                &msg.user_id.0,
                &msg.conversation_id.0,
                &msg.content,
                msg.mention,
            )
            .await;
        }
    });
    register_channel_task(&state, name, "bridge", bridge_handle).await;

    let name_for_task = name.to_string();
    let channel_for_task = channel.clone();
    let main_handle = tokio::spawn(async move {
        if let Err(e) = channel_for_task.start().await {
            error!("WeCom channel {} failed: {}", name_for_task, e);
        }
    });
    register_channel_task(&state, name, "main", main_handle).await;

    state
        .channels
        .reply_dispatcher
        .register_channel(name, channel.clone())
        .await;
    state
        .channels
        .channels
        .write()
        .await
        .insert(name.to_string(), channel);
    info!("✅ WeCom channel '{}' initialized (long-lived WebSocket)", name);

    Ok(())
}

/// Initialize DingTalk (钉钉) Stream Mode channel.
///
/// The second connection channel, and the same shape as WeCom's: the transport
/// is new here and nothing above it is. What differs is only the handshake —
/// Stream Mode opens no socket until an HTTP call hands one over — and that
/// inbound has to be acknowledged, which the connection loop does before the
/// message is handed on.
#[cfg(feature = "dingtalk")]
pub(crate) async fn init_dingtalk_channel(
    state: Arc<GatewayState>,
    name: &str,
    config: &ChannelConfig,
) -> crate::Result<()> {
    use crate::channels::dingtalk::{DingTalkChannel, DingTalkConfig};

    let (Some(client_id), Some(client_secret)) = (
        channel_cred(&state.secrets, name, config, "client_id").await,
        channel_cred(&state.secrets, name, config, "client_secret").await,
    ) else {
        warn!(
            "DingTalk channel '{}' missing 'client_id'/'client_secret' — it cannot authenticate, \
             so it is not started",
            name
        );
        return Ok(());
    };

    let (inbound_tx, mut inbound_rx) =
        mpsc::unbounded_channel::<crate::channels::IncomingMessage>();

    let mut dingtalk_config = DingTalkConfig::new(client_id, client_secret);
    dingtalk_config.message_tx = Some(inbound_tx);
    if let Some(endpoint) = channel_cred(&state.secrets, name, config, "openapi_endpoint").await {
        dingtalk_config.openapi_endpoint = endpoint;
    }

    let channel = Arc::new(DingTalkChannel::new(dingtalk_config));

    // Bridge: every message the connection delivers goes through the shared
    // tail, which is what applies access control, honours `/new`, and records
    // which channel owns the session so the agent's reply can be routed back.
    let state_for_bridge = state.clone();
    let channel_name = name.to_string();
    let bridge_handle = tokio::spawn(async move {
        while let Some(msg) = inbound_rx.recv().await {
            crate::gateway::webhooks::dispatch_channel_message_to(
                &state_for_bridge,
                &channel_name,
                &msg.user_id.0,
                &msg.conversation_id.0,
                &msg.content,
                msg.mention,
            )
            .await;
        }
    });
    register_channel_task(&state, name, "bridge", bridge_handle).await;

    let name_for_task = name.to_string();
    let channel_for_task = channel.clone();
    let main_handle = tokio::spawn(async move {
        if let Err(e) = channel_for_task.start().await {
            error!("DingTalk channel {} failed: {}", name_for_task, e);
        }
    });
    register_channel_task(&state, name, "main", main_handle).await;

    state
        .channels
        .reply_dispatcher
        .register_channel(name, channel.clone())
        .await;
    state
        .channels
        .channels
        .write()
        .await
        .insert(name.to_string(), channel);
    info!("✅ DingTalk channel '{}' initialized (Stream Mode)", name);

    Ok(())
}

/// Initialize IRC channel.
///
/// The third connection channel, and the least exotic of the three: a plain TCP
/// socket carrying lines rather than a WebSocket. The shape is the same, so only
/// the transport differs — and inbound goes through the shared tail unchanged.
#[cfg(feature = "irc")]
pub(crate) async fn init_irc_channel(
    state: Arc<GatewayState>,
    name: &str,
    config: &ChannelConfig,
) -> crate::Result<()> {
    use crate::channels::irc::{IrcChannel, IrcConfig};

    let (Some(server), Some(channel_name)) = (
        channel_cred(&state.secrets, name, config, "server").await,
        channel_cred(&state.secrets, name, config, "channel").await,
    ) else {
        warn!(
            "IRC channel '{}' missing 'server'/'channel' — there is nowhere to connect to, so it \
             is not started",
            name
        );
        return Ok(());
    };

    let nickname = channel_cred(&state.secrets, name, config, "nickname")
        .await
        .unwrap_or_else(|| "syscity-bot".to_string());

    let (inbound_tx, mut inbound_rx) =
        mpsc::unbounded_channel::<crate::channels::IncomingMessage>();

    let mut irc_config = IrcConfig::new(server, nickname, channel_name);
    irc_config.message_tx = Some(inbound_tx);
    if let Some(port) = channel_cred(&state.secrets, name, config, "port").await {
        match port.parse() {
            Ok(port) => irc_config.port = port,
            Err(e) => warn!("IRC channel '{}': ignoring an unusable port: {}", name, e),
        }
    }
    if let Some(use_tls) = channel_cred(&state.secrets, name, config, "use_tls").await {
        irc_config.use_tls =
            matches!(use_tls.trim().to_ascii_lowercase().as_str(), "1" | "true" | "yes");
    }
    irc_config.server_password =
        channel_cred(&state.secrets, name, config, "server_password").await;
    irc_config.nickserv_password =
        channel_cred(&state.secrets, name, config, "nickserv_password").await;

    let channel = Arc::new(IrcChannel::new(irc_config));

    let state_for_bridge = state.clone();
    let name_for_bridge = name.to_string();
    let bridge_handle = tokio::spawn(async move {
        while let Some(msg) = inbound_rx.recv().await {
            crate::gateway::webhooks::dispatch_channel_message_to(
                &state_for_bridge,
                &name_for_bridge,
                &msg.user_id.0,
                &msg.conversation_id.0,
                &msg.content,
                msg.mention,
            )
            .await;
        }
    });
    register_channel_task(&state, name, "bridge", bridge_handle).await;

    let name_for_task = name.to_string();
    let channel_for_task = channel.clone();
    let main_handle = tokio::spawn(async move {
        if let Err(e) = channel_for_task.start().await {
            error!("IRC channel {} failed: {}", name_for_task, e);
        }
    });
    register_channel_task(&state, name, "main", main_handle).await;

    state
        .channels
        .reply_dispatcher
        .register_channel(name, channel.clone())
        .await;
    state
        .channels
        .channels
        .write()
        .await
        .insert(name.to_string(), channel);
    info!("✅ IRC channel '{}' initialized (long-lived socket)", name);

    Ok(())
}

/// Initialize QQ channel
#[cfg(feature = "qq")]
pub(crate) async fn init_qq_channel(
    state: Arc<GatewayState>,
    name: &str,
    config: &ChannelConfig,
) -> crate::Result<()> {
    if let (Some(app_id), Some(app_secret), Some(bot_qq)) = (
        config.credentials.get("app_id"),
        channel_cred(&state.secrets, name, config, "app_secret").await,
        config.credentials.get("bot_qq"),
    ) {
        // Create inbound bridge: QQ WebSocket -> inbound pipeline
        let (inbound_tx, mut inbound_rx) =
            mpsc::unbounded_channel::<crate::channels::IncomingMessage>();
        let mut qq_config = crate::channels::qq::QqConfig::new(app_id, app_secret, bot_qq);
        qq_config.message_tx = Some(inbound_tx);

        let channel = Arc::new(crate::channels::qq::QqChannel::new(qq_config));

        // Bridge inbound messages into the unified entry channel
        let state_clone = state.clone();
        let bridge_handle = tokio::spawn(async move {
            while let Some(msg) = inbound_rx.recv().await {
                if let Err(e) = state_clone.pipelines.inbound_entry.send(msg).await {
                    warn!("Failed to submit message to inbound entry: {}", e);
                }
            }
        });
        register_channel_task(&state, name, "bridge", bridge_handle).await;

        let channel_name = name.to_string();
        let channel_for_task = channel.clone();
        let main_handle = tokio::spawn(async move {
            if let Err(e) = channel_for_task.start().await {
                error!("QQ channel {} failed: {}", channel_name, e);
            }
        });
        register_channel_task(&state, name, "main", main_handle).await;
        state
            .channels
            .reply_dispatcher
            .register_channel(name, channel.clone())
            .await;
        state
            .channels
            .channels
            .write()
            .await
            .insert(name.to_string(), channel);
        info!("✅ QQ channel '{}' initialized", name);
    } else {
        warn!(
            "QQ channel '{}' missing required credentials (app_id, app_secret, bot_qq)",
            name
        );
    }
    Ok(())
}

/// Register a channel extension in the extension registry.
#[allow(dead_code)] // only called from feature-gated channel initializers
async fn register_channel_extension(state: &GatewayState, ext: Arc<dyn ChannelExtension>) {
    let mut registry = state.channels.extensions.write().await;
    registry.register(ext);
}
