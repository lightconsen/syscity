//! Channel management commands for Syscity
//!
//! This module provides CLI commands to add, stop, remove, list, and test
//! channel configurations for the Syscity Gateway.

use std::collections::HashMap;

use clap::{Subcommand, ValueEnum};
use tracing::warn;

use crate::cli::ChannelType;
use crate::error::Result;
use crate::security::pairing::DmPolicy;

/// Channel management CLI commands
#[derive(Debug, Subcommand)]
pub enum ChannelCommands {
    /// Add/configure a channel in Gateway
    Add {
        /// Channel type to add
        #[arg(value_enum)]
        channel: ChannelType,
        /// Bot token or primary credential (env vars: TELEGRAM_BOT_TOKEN,
        /// DISCORD_BOT_TOKEN, etc.)
        #[arg(short, long)]
        token: Option<String>,
        /// Agent to route messages to (defaults to "default" agent)
        #[arg(short, long)]
        agent: Option<String>,
        /// Additional credentials as key=value pairs (e.g., --cred
        /// app_secret=xxx --cred app_id=yyy)
        #[arg(short = 'c', long = "cred", value_parser = parse_key_val)]
        credentials: Vec<(String, String)>,
    },
    /// Stop/disable a channel
    Stop {
        /// Channel type to stop
        #[arg(value_enum)]
        channel: ChannelType,
    },
    /// Remove/disconnect a channel (removes from Gateway config)
    Remove {
        /// Channel type to remove
        #[arg(value_enum)]
        channel: ChannelType,
        /// Only remove if routed to this agent (optional safety check)
        #[arg(short, long)]
        agent: Option<String>,
    },
    /// List configured channels
    List {
        /// Show all channels including disabled ones
        #[arg(short, long)]
        all: bool,
    },
    /// Check channel status
    Status {
        /// Channel type (if not provided, shows all channels)
        #[arg(value_enum)]
        channel: Option<ChannelType>,
        /// Filter by agent name
        #[arg(short, long)]
        agent: Option<String>,
    },
    /// Test channel configuration
    Test {
        /// Channel type to test
        #[arg(value_enum)]
        channel: ChannelType,
        /// Test with specific agent routing
        #[arg(short, long)]
        agent: Option<String>,
    },
}

/// Parse a key=value pair for credentials
fn parse_key_val(s: &str) -> std::result::Result<(String, String), String> {
    s.find('=')
        .map(|pos| (s[..pos].to_string(), s[pos + 1..].to_string()))
        .ok_or_else(|| format!("Invalid key=value pair: {}", s))
}

/// Get the config path for the Gateway
fn get_config_path() -> std::path::PathBuf {
    crate::dirs::default_config_file()
}

/// Load Gateway configuration
async fn load_gateway_config() -> Option<crate::gateway::GatewayConfig> {
    let config_path = get_config_path();
    if !config_path.exists() {
        return None;
    }
    match tokio::fs::read_to_string(&config_path).await {
        Ok(content) => toml::from_str(&content).ok(),
        Err(_) => None,
    }
}

/// Save Gateway configuration
///
/// Sensitive channel credentials are first mirrored into the secret store
/// (namespace `channel`) so a store copy always exists; the plaintext
/// `credentials` entries remain for backward compatibility until `secrets
/// migrate` strips them.
async fn save_gateway_config(config: &crate::gateway::GatewayConfig) -> Result<()> {
    let secrets = crate::secrets::SecretStoreHandle::new();
    for (id, channel_config) in &config.channels {
        if let Err(e) = secrets
            .persist_channel_secrets(id, &channel_config.credentials)
            .await
        {
            warn!("Failed to persist channel secrets for '{}': {}", id, e);
        }
    }
    let config_path = get_config_path();
    let config_str = toml::to_string_pretty(config)?;
    tokio::fs::write(&config_path, config_str).await?;
    Ok(())
}

/// Ensure Gateway config exists (creates default if not)
async fn ensure_gateway_config() -> Result<crate::gateway::GatewayConfig> {
    let config_path = get_config_path();
    if config_path.exists() {
        let content = tokio::fs::read_to_string(&config_path).await?;
        Ok(toml::from_str(&content).unwrap_or_default())
    } else {
        // Ensure parent directory exists
        if let Some(parent) = config_path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        Ok(crate::gateway::GatewayConfig::default())
    }
}

/// Run channel commands
pub async fn run_channel_command(command: &ChannelCommands) -> Result<()> {
    match command {
        ChannelCommands::Add {
            channel,
            token,
            agent,
            credentials,
        } => run_channel_add(*channel, token.clone(), agent.clone(), credentials.clone()).await,
        ChannelCommands::Stop { channel } => run_channel_stop(*channel).await,
        ChannelCommands::Remove { channel, agent } => {
            run_channel_remove(*channel, agent.clone()).await
        }
        ChannelCommands::List { all } => run_channel_list(*all).await,
        ChannelCommands::Status { channel, agent } => {
            run_channel_status(*channel, agent.clone()).await
        }
        ChannelCommands::Test { channel, agent } => run_channel_test(*channel, agent.clone()).await,
    }
}

/// Add/configure a channel in Gateway
async fn run_channel_add(
    channel: ChannelType,
    token: Option<String>,
    agent: Option<String>,
    extra_creds: Vec<(String, String)>,
) -> Result<()> {
    match channel {
        ChannelType::Telegram => add_telegram_channel(token, agent, extra_creds).await,
        ChannelType::Discord => add_discord_channel(token, agent, extra_creds).await,
        ChannelType::Slack => add_slack_channel(token, agent, extra_creds).await,
        ChannelType::Whatsapp => add_whatsapp_channel(token, agent, extra_creds).await,
        ChannelType::Qq => add_qq_channel(token, agent, extra_creds).await,
        ChannelType::Feishu => add_feishu_channel(token, agent, extra_creds).await,
        ChannelType::Websocket => add_websocket_channel(token, agent, extra_creds).await,
        ChannelType::Signal => add_signal_channel(token, agent, extra_creds).await,
        ChannelType::Imessage => add_imessage_channel(token, agent, extra_creds).await,
        ChannelType::Webchat => add_webchat_channel(token, agent, extra_creds).await,
        ChannelType::WechatMp => add_wechatmp_channel(token, agent, extra_creds).await,
        ChannelType::Line => add_line_channel(token, agent, extra_creds).await,
        ChannelType::Twilio => add_twilio_channel(token, agent, extra_creds).await,
        ChannelType::GoogleChat => add_googlechat_channel(token, agent, extra_creds).await,
        ChannelType::Teams => add_teams_channel(token, agent, extra_creds).await,
        ChannelType::Wecom => add_wecom_channel(token, agent, extra_creds).await,
        ChannelType::DingTalk => add_dingtalk_channel(token, agent, extra_creds).await,
        ChannelType::Irc => add_irc_channel(token, agent, extra_creds).await,
    }
}

/// Stop/disable a channel
async fn run_channel_stop(channel: ChannelType) -> Result<()> {
    let channel_name = channel.as_str();

    println!("🛑 Disabling {} channel...", channel_name);

    let mut config = match ensure_gateway_config().await {
        Ok(c) => c,
        Err(_) => {
            println!("⚠️  No Gateway configuration found");
            return Ok(());
        }
    };

    if let Some(channel_config) = config.channels.get_mut(channel_name) {
        channel_config.enabled = false;
        save_gateway_config(&config).await?;
        println!("✅ {} channel disabled", channel_name);
        println!("   The channel will be stopped when the Gateway hot-reloads the configuration.");
    } else {
        println!("⚠️  {} channel not configured", channel_name);
    }

    Ok(())
}

/// Remove/disconnect a channel (removes from Gateway config)
async fn run_channel_remove(channel: ChannelType, agent: Option<String>) -> Result<()> {
    let channel_name = channel.as_str();

    // Check if agent filter was specified
    if let Some(ref agent_name) = agent {
        println!("🗑️  Removing {} channel configuration (agent: {})...", channel_name, agent_name);
    } else {
        println!("🗑️  Removing {} channel configuration...", channel_name);
    }

    let mut config = match ensure_gateway_config().await {
        Ok(c) => c,
        Err(_) => {
            println!("⚠️  No Gateway configuration found");
            return Ok(());
        }
    };

    // Check if channel exists and verify agent if specified
    if let Some(channel_config) = config.channels.get(channel_name) {
        // If --agent was specified, verify the channel is routed to that agent
        if let Some(ref agent_name) = agent {
            let channel_agent = channel_config.agent_id.as_deref().unwrap_or("default");
            if channel_agent != agent_name {
                println!(
                    "⚠️  {} channel is routed to agent '{}' (not '{}')",
                    channel_name, channel_agent, agent_name
                );
                println!(
                    "   Use --agent {} to remove, or omit --agent to remove anyway",
                    channel_agent
                );
                return Ok(());
            }
        }

        // Remove the channel
        config.channels.remove(channel_name);
        save_gateway_config(&config).await?;

        if let Some(ref agent_name) = agent {
            println!(
                "✅ {} channel (agent: {}) removed from Gateway configuration",
                channel_name, agent_name
            );
        } else {
            println!("✅ {} channel removed from Gateway configuration", channel_name);
        }
    } else {
        println!("⚠️  {} channel was not configured", channel_name);
    }

    Ok(())
}

/// Add Telegram channel
#[cfg(feature = "telegram")]
async fn add_telegram_channel(
    token: Option<String>,
    agent: Option<String>,
    extra_creds: Vec<(String, String)>,
) -> Result<()> {
    use crate::channels::ChannelType as GatewayChannelType;
    use crate::gateway::ChannelConfig;

    let token = match token {
        Some(t) => t,
        None => std::env::var("TELEGRAM_BOT_TOKEN").map_err(|_| {
            crate::error::ConfigError::Missing(
                "TELEGRAM_BOT_TOKEN environment variable or --token argument".to_string(),
            )
        })?,
    };

    println!("🚀 Adding Telegram channel to Gateway...");

    let mut config = ensure_gateway_config().await?;

    let mut credentials = HashMap::new();
    credentials.insert("token".to_string(), token);
    for (k, v) in extra_creds {
        credentials.insert(k, v);
    }

    let channel_config = ChannelConfig {
        enabled: true,
        channel_type: GatewayChannelType::Telegram,
        credentials,
        dm_policy: DmPolicy::Open,
        require_mention: false,
        allow_from: vec![],
        block_from: vec![],
        agent_id: agent,
    };

    config
        .channels
        .insert("telegram".to_string(), channel_config);
    save_gateway_config(&config).await?;

    println!("✅ Telegram channel configured in Gateway");
    println!("   Start the Gateway to activate the channel:");
    println!("   syscity start");

    Ok(())
}

#[cfg(not(feature = "telegram"))]
async fn add_telegram_channel(
    _token: Option<String>,
    _agent: Option<String>,
    _extra_creds: Vec<(String, String)>,
) -> Result<()> {
    println!("❌ Telegram support not compiled in.");
    println!("   Build with: cargo build --features telegram");
    Ok(())
}

/// Add Discord channel
#[cfg(feature = "discord")]
async fn add_discord_channel(
    token: Option<String>,
    agent: Option<String>,
    extra_creds: Vec<(String, String)>,
) -> Result<()> {
    use crate::channels::ChannelType as GatewayChannelType;
    use crate::gateway::ChannelConfig;

    let token = match token {
        Some(t) => t,
        None => std::env::var("DISCORD_BOT_TOKEN").map_err(|_| {
            crate::error::ConfigError::Missing(
                "DISCORD_BOT_TOKEN environment variable or --token argument".to_string(),
            )
        })?,
    };

    println!("🚀 Adding Discord channel to Gateway...");

    let mut config = ensure_gateway_config().await?;

    let mut credentials = HashMap::new();
    credentials.insert("token".to_string(), token);
    for (k, v) in extra_creds {
        credentials.insert(k, v);
    }

    let channel_config = ChannelConfig {
        enabled: true,
        channel_type: GatewayChannelType::Discord,
        credentials,
        dm_policy: DmPolicy::Open,
        require_mention: false,
        allow_from: vec![],
        block_from: vec![],
        agent_id: agent,
    };

    config
        .channels
        .insert("discord".to_string(), channel_config);
    save_gateway_config(&config).await?;

    println!("✅ Discord channel configured in Gateway");
    println!("   Start the Gateway to activate the channel:");
    println!("   syscity start");

    Ok(())
}

#[cfg(not(feature = "discord"))]
async fn add_discord_channel(
    _token: Option<String>,
    _agent: Option<String>,
    _extra_creds: Vec<(String, String)>,
) -> Result<()> {
    println!("❌ Discord support not compiled in.");
    println!("   Build with: cargo build --features discord");
    Ok(())
}

/// Add Slack channel
#[cfg(feature = "slack")]
async fn add_slack_channel(
    token: Option<String>,
    agent: Option<String>,
    extra_creds: Vec<(String, String)>,
) -> Result<()> {
    use crate::channels::ChannelType as GatewayChannelType;
    use crate::gateway::ChannelConfig;

    let token = match token {
        Some(t) => t,
        None => std::env::var("SLACK_BOT_TOKEN").map_err(|_| {
            crate::error::ConfigError::Missing(
                "SLACK_BOT_TOKEN environment variable or --token argument".to_string(),
            )
        })?,
    };

    println!("🚀 Adding Slack channel to Gateway...");

    let mut config = ensure_gateway_config().await?;

    let mut credentials = HashMap::new();
    credentials.insert("bot_token".to_string(), token);

    // Also check for signing secret
    if let Ok(signing_secret) = std::env::var("SLACK_SIGNING_SECRET") {
        credentials.insert("signing_secret".to_string(), signing_secret);
    }

    for (k, v) in extra_creds {
        credentials.insert(k, v);
    }

    let channel_config = ChannelConfig {
        enabled: true,
        channel_type: GatewayChannelType::Slack,
        credentials,
        dm_policy: DmPolicy::Open,
        require_mention: false,
        allow_from: vec![],
        block_from: vec![],
        agent_id: agent,
    };

    config.channels.insert("slack".to_string(), channel_config);
    save_gateway_config(&config).await?;

    println!("✅ Slack channel configured in Gateway");
    println!("   Start the Gateway to activate the channel:");
    println!("   syscity start");

    Ok(())
}

#[cfg(not(feature = "slack"))]
async fn add_slack_channel(
    _token: Option<String>,
    _agent: Option<String>,
    _extra_creds: Vec<(String, String)>,
) -> Result<()> {
    println!("❌ Slack support not compiled in.");
    println!("   Build with: cargo build --features slack");
    Ok(())
}

/// Add WhatsApp channel
#[cfg(feature = "whatsapp")]
async fn add_whatsapp_channel(
    token: Option<String>,
    agent: Option<String>,
    extra_creds: Vec<(String, String)>,
) -> Result<()> {
    use crate::channels::ChannelType as GatewayChannelType;
    use crate::gateway::ChannelConfig;

    // WhatsApp requires both access token and phone number ID
    let access_token = match token {
        Some(t) => t,
        None => std::env::var("WHATSAPP_ACCESS_TOKEN").map_err(|_| {
            crate::error::ConfigError::Missing(
                "WHATSAPP_ACCESS_TOKEN environment variable or --token argument".to_string(),
            )
        })?,
    };

    let phone_number_id = std::env::var("WHATSAPP_PHONE_NUMBER_ID").map_err(|_| {
        crate::error::ConfigError::Missing(
            "WHATSAPP_PHONE_NUMBER_ID environment variable".to_string(),
        )
    })?;

    println!("🚀 Adding WhatsApp channel to Gateway...");

    let mut config = ensure_gateway_config().await?;

    let mut credentials = HashMap::new();
    credentials.insert("access_token".to_string(), access_token);
    credentials.insert("phone_number_id".to_string(), phone_number_id);

    // Optional: webhook verify token
    if let Ok(verify_token) = std::env::var("WHATSAPP_WEBHOOK_VERIFY_TOKEN") {
        credentials.insert("webhook_verify_token".to_string(), verify_token);
    }

    for (k, v) in extra_creds {
        credentials.insert(k, v);
    }

    let channel_config = ChannelConfig {
        enabled: true,
        channel_type: GatewayChannelType::Whatsapp,
        credentials,
        dm_policy: DmPolicy::Open,
        require_mention: false,
        allow_from: vec![],
        block_from: vec![],
        agent_id: agent,
    };

    config
        .channels
        .insert("whatsapp".to_string(), channel_config);
    save_gateway_config(&config).await?;

    println!("✅ WhatsApp channel configured in Gateway");
    println!("   Start the Gateway to activate the channel:");
    println!("   syscity start");

    Ok(())
}

#[cfg(not(feature = "whatsapp"))]
async fn add_whatsapp_channel(
    _token: Option<String>,
    _agent: Option<String>,
    _extra_creds: Vec<(String, String)>,
) -> Result<()> {
    println!("❌ WhatsApp support not compiled in.");
    println!("   Build with: cargo build --features whatsapp");
    Ok(())
}

/// Add QQ channel
#[cfg(feature = "qq")]
async fn add_qq_channel(
    token: Option<String>,
    agent: Option<String>,
    extra_creds: Vec<(String, String)>,
) -> Result<()> {
    use crate::channels::ChannelType as GatewayChannelType;
    use crate::gateway::ChannelConfig;

    let app_id = match token {
        Some(t) => t,
        None => std::env::var("QQ_APP_ID").map_err(|_| {
            crate::error::ConfigError::Missing(
                "QQ_APP_ID environment variable or --token argument".to_string(),
            )
        })?,
    };

    let app_secret = std::env::var("QQ_APP_SECRET").map_err(|_| {
        crate::error::ConfigError::Missing("QQ_APP_SECRET environment variable".to_string())
    })?;

    println!("🚀 Adding QQ channel to Gateway...");

    let mut config = ensure_gateway_config().await?;

    let mut credentials = HashMap::new();
    credentials.insert("app_id".to_string(), app_id);
    credentials.insert("app_secret".to_string(), app_secret);

    // Optional: access token if already obtained
    if let Ok(access_token) = std::env::var("QQ_ACCESS_TOKEN") {
        credentials.insert("access_token".to_string(), access_token);
    }

    for (k, v) in extra_creds {
        credentials.insert(k, v);
    }

    let channel_config = ChannelConfig {
        enabled: true,
        channel_type: GatewayChannelType::Qq,
        credentials,
        dm_policy: DmPolicy::Open,
        require_mention: false,
        allow_from: vec![],
        block_from: vec![],
        agent_id: agent,
    };

    config.channels.insert("qq".to_string(), channel_config);
    save_gateway_config(&config).await?;

    println!("✅ QQ channel configured in Gateway");
    println!("   Start the Gateway to activate the channel:");
    println!("   syscity start");

    Ok(())
}

#[cfg(not(feature = "qq"))]
async fn add_qq_channel(
    _token: Option<String>,
    _agent: Option<String>,
    _extra_creds: Vec<(String, String)>,
) -> Result<()> {
    println!("❌ QQ support not compiled in.");
    println!("   Build with: cargo build --features qq");
    Ok(())
}

/// Add Feishu/Lark channel
#[cfg(feature = "feishu")]
async fn add_feishu_channel(
    token: Option<String>,
    agent: Option<String>,
    extra_creds: Vec<(String, String)>,
) -> Result<()> {
    use crate::channels::ChannelType as GatewayChannelType;
    use crate::gateway::ChannelConfig;

    let app_id = match token {
        Some(t) => t,
        None => std::env::var("LARK_APP_ID")
            .or_else(|_| std::env::var("FEISHU_APP_ID"))
            .map_err(|_| {
                crate::error::ConfigError::Missing(
                    "LARK_APP_ID or FEISHU_APP_ID environment variable, or --token argument"
                        .to_string(),
                )
            })?,
    };

    let app_secret = std::env::var("LARK_APP_SECRET")
        .or_else(|_| std::env::var("FEISHU_APP_SECRET"))
        .map_err(|_| {
            crate::error::ConfigError::Missing(
                "LARK_APP_SECRET or FEISHU_APP_SECRET environment variable".to_string(),
            )
        })?;

    println!("🚀 Adding Feishu/Lark channel to Gateway...");

    let mut config = ensure_gateway_config().await?;

    let mut credentials = HashMap::new();
    credentials.insert("app_id".to_string(), app_id);
    credentials.insert("app_secret".to_string(), app_secret);

    // Optional: verification token for webhooks
    if let Ok(verify_token) = std::env::var("LARK_VERIFICATION_TOKEN")
        .or_else(|_| std::env::var("FEISHU_VERIFICATION_TOKEN"))
    {
        credentials.insert("verification_token".to_string(), verify_token);
    }

    // Optional: encrypt key for webhooks
    if let Ok(encrypt_key) =
        std::env::var("LARK_ENCRYPT_KEY").or_else(|_| std::env::var("FEISHU_ENCRYPT_KEY"))
    {
        credentials.insert("encrypt_key".to_string(), encrypt_key);
    }

    for (k, v) in extra_creds {
        credentials.insert(k, v);
    }

    let channel_config = ChannelConfig {
        enabled: true,
        channel_type: GatewayChannelType::Feishu,
        credentials,
        dm_policy: DmPolicy::Open,
        require_mention: false,
        allow_from: vec![],
        block_from: vec![],
        agent_id: agent,
    };

    config.channels.insert("feishu".to_string(), channel_config);
    save_gateway_config(&config).await?;

    println!("✅ Feishu/Lark channel configured in Gateway");
    println!("   Start the Gateway to activate the channel:");
    println!("   syscity start");

    Ok(())
}

#[cfg(not(feature = "feishu"))]
async fn add_feishu_channel(
    _token: Option<String>,
    _agent: Option<String>,
    _extra_creds: Vec<(String, String)>,
) -> Result<()> {
    println!("❌ Feishu/Lark support not compiled in.");
    println!("   Build with: cargo build --features feishu");
    Ok(())
}

/// Add WebSocket channel
async fn add_websocket_channel(
    _token: Option<String>,
    agent: Option<String>,
    extra_creds: Vec<(String, String)>,
) -> Result<()> {
    println!("🚀 Adding WebSocket channel to Gateway...");

    let mut config = ensure_gateway_config().await?;

    let mut credentials = HashMap::new();
    for (k, v) in extra_creds {
        credentials.insert(k, v);
    }

    // WebSocket doesn't need tokens, just the endpoint configuration
    let channel_config = crate::gateway::ChannelConfig {
        enabled: true,
        channel_type: crate::channels::ChannelType::Websocket,
        credentials,
        dm_policy: DmPolicy::Open,
        require_mention: false,
        allow_from: vec![],
        block_from: vec![],
        agent_id: agent,
    };

    config
        .channels
        .insert("websocket".to_string(), channel_config);
    save_gateway_config(&config).await?;

    println!("✅ WebSocket channel configured in Gateway");
    println!("   Start the Gateway to activate the channel:");
    println!("   syscity start");

    Ok(())
}

async fn add_signal_channel(
    token: Option<String>,
    agent: Option<String>,
    extra_creds: Vec<(String, String)>,
) -> Result<()> {
    println!("🚀 Adding Signal channel to Gateway...");

    let mut config = ensure_gateway_config().await?;

    let mut credentials = HashMap::new();
    if let Some(token) = token {
        credentials.insert("account".to_string(), token);
    }
    for (k, v) in extra_creds {
        credentials.insert(k, v);
    }

    let channel_config = crate::gateway::ChannelConfig {
        enabled: true,
        channel_type: crate::channels::ChannelType::Signal,
        credentials,
        dm_policy: DmPolicy::Open,
        require_mention: false,
        allow_from: vec![],
        block_from: vec![],
        agent_id: agent,
    };

    config.channels.insert("signal".to_string(), channel_config);
    save_gateway_config(&config).await?;

    println!("✅ Signal channel configured in Gateway");
    println!("   Make sure signal-cli daemon is running:");
    println!("   signal-cli daemon --http localhost:8080");
    println!("   Start the Gateway to activate the channel:");
    println!("   syscity start");

    Ok(())
}

async fn add_imessage_channel(
    _token: Option<String>,
    agent: Option<String>,
    extra_creds: Vec<(String, String)>,
) -> Result<()> {
    println!("🚀 Adding iMessage channel to Gateway...");

    let mut config = ensure_gateway_config().await?;

    let mut credentials = HashMap::new();
    for (k, v) in extra_creds {
        credentials.insert(k, v);
    }

    let channel_config = crate::gateway::ChannelConfig {
        enabled: true,
        channel_type: crate::channels::ChannelType::Imessage,
        credentials,
        dm_policy: DmPolicy::Open,
        require_mention: false,
        allow_from: vec![],
        block_from: vec![],
        agent_id: agent,
    };

    config
        .channels
        .insert("imessage".to_string(), channel_config);
    save_gateway_config(&config).await?;

    println!("✅ iMessage channel configured in Gateway");
    println!("   Make sure BlueBubbles server is running on your Mac.");
    println!("   Start the Gateway to activate the channel:");
    println!("   syscity start");

    Ok(())
}

async fn add_webchat_channel(
    _token: Option<String>,
    agent: Option<String>,
    extra_creds: Vec<(String, String)>,
) -> Result<()> {
    println!("🚀 Adding WebChat channel to Gateway...");

    let mut config = ensure_gateway_config().await?;

    let mut credentials = HashMap::new();
    for (k, v) in extra_creds {
        credentials.insert(k, v);
    }

    let channel_config = crate::gateway::ChannelConfig {
        enabled: true,
        channel_type: crate::channels::ChannelType::Webchat,
        credentials,
        dm_policy: DmPolicy::Open,
        require_mention: false,
        allow_from: vec![],
        block_from: vec![],
        agent_id: agent,
    };

    config
        .channels
        .insert("webchat".to_string(), channel_config);
    save_gateway_config(&config).await?;

    println!("✅ WebChat channel configured in Gateway");
    println!("   Access the chat interface at http://localhost:8081");
    println!("   Start the Gateway to activate the channel:");
    println!("   syscity start");

    Ok(())
}

/// Add WeChat Official Account (公众号) channel to the gateway config.
async fn add_wechatmp_channel(
    token: Option<String>,
    agent: Option<String>,
    extra_creds: Vec<(String, String)>,
) -> Result<()> {
    use crate::channels::ChannelType as GatewayChannelType;
    use crate::gateway::ChannelConfig;

    let app_id = match token {
        Some(t) => t,
        None => std::env::var("WECHAT_MP_APP_ID").map_err(|_| {
            crate::error::ConfigError::Missing(
                "WECHAT_MP_APP_ID environment variable, or --token argument".to_string(),
            )
        })?,
    };

    let app_secret = std::env::var("WECHAT_MP_APP_SECRET").map_err(|_| {
        crate::error::ConfigError::Missing("WECHAT_MP_APP_SECRET environment variable".to_string())
    })?;

    println!("🚀 Adding WeChat MP channel to Gateway...");

    let mut config = ensure_gateway_config().await?;

    let mut credentials = HashMap::new();
    credentials.insert("app_id".to_string(), app_id);
    credentials.insert("app_secret".to_string(), app_secret);

    // Optional: webhook verification token
    if let Ok(token) = std::env::var("WECHAT_MP_TOKEN") {
        credentials.insert("token".to_string(), token);
    }
    // Optional: encrypted-message key
    if let Ok(aes_key) = std::env::var("WECHAT_MP_ENCODING_AES_KEY") {
        credentials.insert("encoding_aes_key".to_string(), aes_key);
    }
    for (k, v) in extra_creds {
        credentials.insert(k, v);
    }

    let channel_config = ChannelConfig {
        enabled: true,
        channel_type: GatewayChannelType::WechatMp,
        credentials,
        dm_policy: DmPolicy::Open,
        require_mention: false,
        allow_from: vec![],
        block_from: vec![],
        agent_id: agent,
    };

    config
        .channels
        .insert("wechatmp".to_string(), channel_config);
    save_gateway_config(&config).await?;

    println!("✅ WeChat MP channel configured in Gateway");
    println!("   Configure the webhook in the MP console (URL + Token + EncodingAESKey)");
    println!("   Start the Gateway to activate the channel:");
    println!("   syscity start");

    Ok(())
}

/// Add/configure a LINE channel in Gateway
async fn add_line_channel(
    token: Option<String>,
    agent: Option<String>,
    extra_creds: Vec<(String, String)>,
) -> Result<()> {
    use crate::channels::ChannelType as GatewayChannelType;
    use crate::gateway::ChannelConfig;

    // `--token` is the channel *access* token. The channel *secret* is a
    // separate credential, so it is asked for separately rather than reusing
    // `--token`: one credential standing in for both would verify nothing and
    // fail at the first delivery instead of here.
    let access_token = match token {
        Some(t) => t,
        None => std::env::var("LINE_CHANNEL_ACCESS_TOKEN").map_err(|_| {
            crate::error::ConfigError::Missing(
                "LINE_CHANNEL_ACCESS_TOKEN environment variable, or --token argument".to_string(),
            )
        })?,
    };

    let channel_secret = std::env::var("LINE_CHANNEL_SECRET")
        .ok()
        .or_else(|| {
            extra_creds
                .iter()
                .find(|(k, _)| k == "channel_secret")
                .map(|(_, v)| v.clone())
        })
        .ok_or_else(|| {
            crate::error::ConfigError::Missing(
                "LINE_CHANNEL_SECRET environment variable, or --credential channel_secret=..."
                    .to_string(),
            )
        })?;

    println!("🚀 Adding LINE channel to Gateway...");

    let mut config = ensure_gateway_config().await?;

    let mut credentials = HashMap::new();
    credentials.insert("channel_access_token".to_string(), access_token);
    credentials.insert("channel_secret".to_string(), channel_secret);
    for (k, v) in extra_creds {
        credentials.insert(k, v);
    }

    let channel_config = ChannelConfig {
        enabled: true,
        channel_type: GatewayChannelType::Line,
        credentials,
        dm_policy: DmPolicy::Open,
        require_mention: false,
        allow_from: vec![],
        block_from: vec![],
        agent_id: agent,
    };

    config.channels.insert("line".to_string(), channel_config);
    save_gateway_config(&config).await?;

    println!("✅ LINE channel configured in Gateway");
    println!("   Point the Messaging API webhook at <gateway>/webhooks/line");
    println!("   Start the Gateway to activate the channel:");
    println!("   syscity start");

    Ok(())
}

/// Add/configure a Twilio SMS channel in Gateway
async fn add_twilio_channel(
    token: Option<String>,
    agent: Option<String>,
    extra_creds: Vec<(String, String)>,
) -> Result<()> {
    use crate::channels::ChannelType as GatewayChannelType;
    use crate::gateway::ChannelConfig;

    // `--token` is the auth token. The account SID is the other half of the
    // Basic-auth pair, and the from-number is the line replies come out of —
    // both are asked for separately rather than inferred.
    let auth_token = match token {
        Some(t) => t,
        None => std::env::var("TWILIO_AUTH_TOKEN").map_err(|_| {
            crate::error::ConfigError::Missing(
                "TWILIO_AUTH_TOKEN environment variable, or --token argument".to_string(),
            )
        })?,
    };

    let lookup = |env: &str, key: &str| -> Option<String> {
        std::env::var(env).ok().or_else(|| {
            extra_creds
                .iter()
                .find(|(k, _)| k == key)
                .map(|(_, v)| v.clone())
        })
    };

    let account_sid = lookup("TWILIO_ACCOUNT_SID", "account_sid").ok_or_else(|| {
        crate::error::ConfigError::Missing(
            "TWILIO_ACCOUNT_SID environment variable, or --credential account_sid=...".to_string(),
        )
    })?;

    let from_number = lookup("TWILIO_FROM_NUMBER", "from_number").ok_or_else(|| {
        crate::error::ConfigError::Missing(
            "TWILIO_FROM_NUMBER environment variable, or --credential from_number=...".to_string(),
        )
    })?;

    // Twilio signs the URL it dialled, and the gateway cannot recover that from
    // the request — so this is not optional the way a display name would be.
    // A channel without it refuses every delivery, and it is better to say so
    // here than to have the operator discover it from a 401.
    let webhook_url = lookup("TWILIO_WEBHOOK_URL", "webhook_url").ok_or_else(|| {
        crate::error::ConfigError::Missing(
            "TWILIO_WEBHOOK_URL (the public URL configured in the Twilio console), or \
             --credential webhook_url=..."
                .to_string(),
        )
    })?;

    println!("🚀 Adding Twilio SMS channel to Gateway...");

    let mut config = ensure_gateway_config().await?;

    let mut credentials = HashMap::new();
    credentials.insert("auth_token".to_string(), auth_token);
    credentials.insert("account_sid".to_string(), account_sid);
    credentials.insert("from_number".to_string(), from_number);
    credentials.insert("webhook_url".to_string(), webhook_url);
    for (k, v) in extra_creds {
        credentials.insert(k, v);
    }

    let channel_config = ChannelConfig {
        enabled: true,
        channel_type: GatewayChannelType::Twilio,
        credentials,
        dm_policy: DmPolicy::Open,
        require_mention: false,
        allow_from: vec![],
        block_from: vec![],
        agent_id: agent,
    };

    config.channels.insert("twilio".to_string(), channel_config);
    save_gateway_config(&config).await?;

    println!("✅ Twilio SMS channel configured in Gateway");
    println!("   Point the phone number's messaging webhook at <public>/webhooks/twilio");
    println!("   Start the Gateway to activate the channel:");
    println!("   syscity start");

    Ok(())
}

/// Add/configure a Google Chat channel in Gateway
async fn add_googlechat_channel(
    token: Option<String>,
    agent: Option<String>,
    extra_creds: Vec<(String, String)>,
) -> Result<()> {
    use crate::channels::ChannelType as GatewayChannelType;
    use crate::gateway::ChannelConfig;

    let lookup = |env: &str, key: &str| -> Option<String> {
        std::env::var(env).ok().or_else(|| {
            extra_creds
                .iter()
                .find(|(k, _)| k == key)
                .map(|(_, v)| v.clone())
        })
    };

    // Every one of these is required, and for the same reason: this channel has
    // no safe default. The issuer, audience and key URL depend on the app's
    // Authentication Audience setting, and the identity allowlist is what
    // separates "Google signed this" from "Google signed this for us".
    let required = |env: &str, key: &str| -> Result<String> {
        lookup(env, key).ok_or_else(|| {
            crate::error::SyscityError::Config(crate::error::ConfigError::Missing(format!(
                "{} environment variable, or --credential {}=...",
                env, key
            )))
        })
    };

    // `--token` carries the service account's address, which is the one
    // credential an operator is most likely to have to hand.
    let client_email = match token {
        Some(t) => t,
        None => required("GOOGLE_CHAT_CLIENT_EMAIL", "client_email")?,
    };
    let private_key_pem = required("GOOGLE_CHAT_PRIVATE_KEY", "private_key_pem")?;
    let issuer = required("GOOGLE_CHAT_ISSUER", "issuer")?;
    let audience = required("GOOGLE_CHAT_AUDIENCE", "audience")?;
    let jwks_url = required("GOOGLE_CHAT_JWKS_URL", "jwks_url")?;
    let allowed = required("GOOGLE_CHAT_ALLOWED_SERVICE_ACCOUNTS", "allowed_service_accounts")?;

    println!("🚀 Adding Google Chat channel to Gateway...");

    let mut config = ensure_gateway_config().await?;

    let mut credentials = HashMap::new();
    credentials.insert("client_email".to_string(), client_email);
    credentials.insert("private_key_pem".to_string(), private_key_pem);
    credentials.insert("issuer".to_string(), issuer);
    credentials.insert("audience".to_string(), audience);
    credentials.insert("jwks_url".to_string(), jwks_url);
    credentials.insert("allowed_service_accounts".to_string(), allowed);
    for (k, v) in extra_creds {
        credentials.insert(k, v);
    }

    let channel_config = ChannelConfig {
        enabled: true,
        channel_type: GatewayChannelType::GoogleChat,
        credentials,
        dm_policy: DmPolicy::Open,
        require_mention: false,
        allow_from: vec![],
        block_from: vec![],
        agent_id: agent,
    };

    config
        .channels
        .insert("googlechat".to_string(), channel_config);
    save_gateway_config(&config).await?;

    println!("✅ Google Chat channel configured in Gateway");
    println!("   Point the Chat app's connection settings at <public>/webhooks/googlechat");
    println!("   Start the Gateway to activate the channel:");
    println!("   syscity start");

    Ok(())
}

/// Add/configure a Microsoft Teams channel in Gateway
async fn add_teams_channel(
    token: Option<String>,
    agent: Option<String>,
    extra_creds: Vec<(String, String)>,
) -> Result<()> {
    use crate::channels::ChannelType as GatewayChannelType;
    use crate::gateway::ChannelConfig;

    let lookup = |env: &str, key: &str| -> Option<String> {
        std::env::var(env).ok().or_else(|| {
            extra_creds
                .iter()
                .find(|(k, _)| k == key)
                .map(|(_, v)| v.clone())
        })
    };

    let required = |env: &str, key: &str| -> Result<String> {
        lookup(env, key).ok_or_else(|| {
            crate::error::SyscityError::Config(crate::error::ConfigError::Missing(format!(
                "{} environment variable, or --credential {}=...",
                env, key
            )))
        })
    };

    // `--token` is the app registration's secret, the way it is Twilio's auth
    // token: the credential, not the identity.
    let client_secret = match token {
        Some(t) => t,
        None => required("TEAMS_CLIENT_SECRET", "client_secret")?,
    };

    // The app id is one value with two jobs — the audience Microsoft signs
    // inbound tokens for, and the app registration replies are sent as — so it
    // is asked for once.
    let app_id = required("TEAMS_APP_ID", "app_id")?;
    let tenant_id = required("TEAMS_TENANT_ID", "tenant_id")?;

    // The public cloud's issuer and key URL are fixed, documented values, so
    // they are defaulted rather than demanded; a sovereign cloud overrides both.
    let issuer = lookup("TEAMS_ISSUER", "issuer")
        .unwrap_or_else(|| "https://api.botframework.com".to_string());
    let jwks_url = lookup("TEAMS_JWKS_URL", "jwks_url")
        .unwrap_or_else(|| "https://login.botframework.com/v1/.well-known/keys".to_string());

    println!("🚀 Adding Microsoft Teams channel to Gateway...");

    let mut config = ensure_gateway_config().await?;

    let mut credentials = HashMap::new();
    credentials.insert("app_id".to_string(), app_id);
    credentials.insert("tenant_id".to_string(), tenant_id);
    credentials.insert("client_secret".to_string(), client_secret);
    credentials.insert("issuer".to_string(), issuer);
    credentials.insert("jwks_url".to_string(), jwks_url);
    for (k, v) in extra_creds {
        credentials.insert(k, v);
    }

    let channel_config = ChannelConfig {
        enabled: true,
        channel_type: GatewayChannelType::Teams,
        credentials,
        dm_policy: DmPolicy::Open,
        require_mention: false,
        allow_from: vec![],
        block_from: vec![],
        agent_id: agent,
    };

    config.channels.insert("teams".to_string(), channel_config);
    save_gateway_config(&config).await?;

    println!("✅ Microsoft Teams channel configured in Gateway");
    println!("   Set the bot's messaging endpoint to <public>/webhooks/teams");
    println!("   Start the Gateway to activate the channel:");
    println!("   syscity start");

    Ok(())
}

/// Add/configure a WeCom (企业微信) AI Bot channel in Gateway
async fn add_wecom_channel(
    token: Option<String>,
    agent: Option<String>,
    extra_creds: Vec<(String, String)>,
) -> Result<()> {
    use crate::channels::ChannelType as GatewayChannelType;
    use crate::gateway::ChannelConfig;

    let lookup = |env: &str, key: &str| -> Option<String> {
        std::env::var(env).ok().or_else(|| {
            extra_creds
                .iter()
                .find(|(k, _)| k == key)
                .map(|(_, v)| v.clone())
        })
    };

    let required = |env: &str, key: &str| -> Result<String> {
        lookup(env, key).ok_or_else(|| {
            crate::error::SyscityError::Config(crate::error::ConfigError::Missing(format!(
                "{} environment variable, or --credential {}=...",
                env, key
            )))
        })
    };

    // `--token` is the secret, the way it is Twilio's auth token and Teams' app
    // secret: the credential, not the identity.
    let secret = match token {
        Some(t) => t,
        None => required("WECOM_SECRET", "secret")?,
    };
    let bot_id = required("WECOM_BOT_ID", "bot_id")?;

    println!("🚀 Adding WeCom channel to Gateway...");

    let mut config = ensure_gateway_config().await?;

    let mut credentials = HashMap::new();
    credentials.insert("bot_id".to_string(), bot_id);
    credentials.insert("secret".to_string(), secret);
    // Only for a sovereign deployment, or a test peer; the public gateway is
    // the default and is what almost every deployment wants.
    if let Some(ws_url) = lookup("WECOM_WEBSOCKET_URL", "ws_url") {
        credentials.insert("ws_url".to_string(), ws_url);
    }
    for (k, v) in extra_creds {
        credentials.insert(k, v);
    }

    let channel_config = ChannelConfig {
        enabled: true,
        channel_type: GatewayChannelType::Wecom,
        credentials,
        dm_policy: DmPolicy::Open,
        require_mention: false,
        allow_from: vec![],
        block_from: vec![],
        agent_id: agent,
    };

    config.channels.insert("wecom".to_string(), channel_config);
    save_gateway_config(&config).await?;

    println!("✅ WeCom channel configured in Gateway");
    println!("   Nothing to point anywhere: the gateway connects out to WeCom itself");
    println!("   Start the Gateway to activate the channel:");
    println!("   syscity start");

    Ok(())
}

/// Add/configure a DingTalk (钉钉) Stream Mode channel in Gateway
async fn add_dingtalk_channel(
    token: Option<String>,
    agent: Option<String>,
    extra_creds: Vec<(String, String)>,
) -> Result<()> {
    use crate::channels::ChannelType as GatewayChannelType;
    use crate::gateway::ChannelConfig;

    let lookup = |env: &str, key: &str| -> Option<String> {
        std::env::var(env).ok().or_else(|| {
            extra_creds
                .iter()
                .find(|(k, _)| k == key)
                .map(|(_, v)| v.clone())
        })
    };

    let required = |env: &str, key: &str| -> Result<String> {
        lookup(env, key).ok_or_else(|| {
            crate::error::SyscityError::Config(crate::error::ConfigError::Missing(format!(
                "{} environment variable, or --credential {}=...",
                env, key
            )))
        })
    };

    // `--token` is the client secret, the way it is Twilio's auth token and
    // Teams' app secret: the credential, not the identity.
    let client_secret = match token {
        Some(t) => t,
        None => required("DINGTALK_CLIENT_SECRET", "client_secret")?,
    };
    let client_id = required("DINGTALK_CLIENT_ID", "client_id")?;

    println!("🚀 Adding DingTalk channel to Gateway...");

    let mut config = ensure_gateway_config().await?;

    let mut credentials = HashMap::new();
    credentials.insert("client_id".to_string(), client_id);
    credentials.insert("client_secret".to_string(), client_secret);
    // Only for a sovereign deployment or a test peer; the public API is the
    // default and is what almost every deployment wants.
    if let Some(endpoint) = lookup("DINGTALK_OPENAPI_ENDPOINT", "openapi_endpoint") {
        credentials.insert("openapi_endpoint".to_string(), endpoint);
    }
    for (k, v) in extra_creds {
        credentials.insert(k, v);
    }

    let channel_config = ChannelConfig {
        enabled: true,
        channel_type: GatewayChannelType::DingTalk,
        credentials,
        dm_policy: DmPolicy::Open,
        require_mention: false,
        allow_from: vec![],
        block_from: vec![],
        agent_id: agent,
    };

    config
        .channels
        .insert("dingtalk".to_string(), channel_config);
    save_gateway_config(&config).await?;

    println!("✅ DingTalk channel configured in Gateway");
    println!("   Nothing to point anywhere: the gateway connects out to DingTalk itself");
    println!("   The app's robot must have its message-receiving mode set to Stream Mode");
    println!("   Start the Gateway to activate the channel:");
    println!("   syscity start");

    Ok(())
}

/// Add/configure an IRC channel in Gateway
async fn add_irc_channel(
    token: Option<String>,
    agent: Option<String>,
    extra_creds: Vec<(String, String)>,
) -> Result<()> {
    use crate::channels::ChannelType as GatewayChannelType;
    use crate::gateway::ChannelConfig;

    let lookup = |env: &str, key: &str| -> Option<String> {
        std::env::var(env).ok().or_else(|| {
            extra_creds
                .iter()
                .find(|(k, _)| k == key)
                .map(|(_, v)| v.clone())
        })
    };

    let required = |env: &str, key: &str| -> Result<String> {
        lookup(env, key).ok_or_else(|| {
            crate::error::SyscityError::Config(crate::error::ConfigError::Missing(format!(
                "{} environment variable, or --credential {}=...",
                env, key
            )))
        })
    };

    // `--token` is the NickServ password, which is the only credential anyone
    // is likely to have to hand — the server itself usually needs none.
    let nickserv_password = token.or_else(|| lookup("IRC_NICKSERV_PASSWORD", "nickserv_password"));

    let server = required("IRC_SERVER", "server")?;
    let channel = required("IRC_CHANNEL", "channel")?;

    println!("🚀 Adding IRC channel to Gateway...");

    let mut config = ensure_gateway_config().await?;

    let mut credentials = HashMap::new();
    credentials.insert("server".to_string(), server);
    credentials.insert("channel".to_string(), channel);
    credentials.insert(
        "nickname".to_string(),
        lookup("IRC_NICKNAME", "nickname").unwrap_or_else(|| "syscity-bot".to_string()),
    );
    credentials.insert(
        "port".to_string(),
        lookup("IRC_PORT", "port").unwrap_or_else(|| "6697".to_string()),
    );
    // TLS is on unless it is switched off, matching the reference: the port the
    // channel defaults to is the TLS one, and most networks insist.
    if let Some(use_tls) = lookup("IRC_USE_TLS", "use_tls") {
        credentials.insert("use_tls".to_string(), use_tls);
    }
    if let Some(server_password) = lookup("IRC_SERVER_PASSWORD", "server_password") {
        credentials.insert("server_password".to_string(), server_password);
    }
    if let Some(password) = nickserv_password {
        credentials.insert("nickserv_password".to_string(), password);
    }
    for (k, v) in extra_creds {
        credentials.insert(k, v);
    }

    let channel_config = ChannelConfig {
        enabled: true,
        channel_type: GatewayChannelType::Irc,
        credentials,
        dm_policy: DmPolicy::Open,
        // A bot sitting in a channel should not answer everything said in it.
        // This is the one channel whose default differs, and it is the channel
        // where getting it wrong is most visible.
        require_mention: true,
        allow_from: vec![],
        block_from: vec![],
        agent_id: agent,
    };

    config.channels.insert("irc".to_string(), channel_config);
    save_gateway_config(&config).await?;

    println!("✅ IRC channel configured in Gateway");
    println!("   Nothing to point anywhere: the gateway connects out to the server");
    println!("   In a channel the bot answers when named (e.g. `syscity-bot: hello`)");
    println!("   Start the Gateway to activate the channel:");
    println!("   syscity start");

    Ok(())
}

/// List configured channels
async fn run_channel_list(all: bool) -> Result<()> {
    println!("📱 Syscity Channels");
    println!("=================");
    println!();

    // Derived from the enum rather than listed again. The hand-written version
    // this replaces had drifted: it was missing WeChat MP, so `channel list`
    // could not show a channel the gateway was actually running.
    let all_channels: Vec<(&str, &str, ChannelType)> = ChannelType::value_variants()
        .iter()
        .map(|c| (c.as_str(), c.display_name(), *c))
        .collect();

    // Load gateway config
    let config = load_gateway_config().await;

    // Show connected/configured channels
    println!("Connected Channels:");
    println!("{:<15} {:<12} {:<20}", "Channel", "Status", "Agent");
    println!("{}", "-".repeat(50));

    let mut connected_count = 0;

    for (key, name, _channel_type) in &all_channels {
        if let Some(ref cfg) = config {
            if let Some(channel) = cfg.channels.get(*key) {
                // Skip disabled channels unless --all is specified
                if !channel.enabled && !all {
                    continue;
                }

                let status = if channel.enabled {
                    "🟢 enabled"
                } else {
                    "🟡 disabled"
                };
                let agent = channel.agent_id.as_deref().unwrap_or("default");
                println!("{:<15} {:<12} {:<20}", name, status, agent);
                connected_count += 1;
            } else if all {
                // Show not configured channels only with --all
                println!("{:<15} {:<12} {:<20}", name, "🔴 not configured", "-");
            }
        } else if all {
            println!("{:<15} {:<12} {:<20}", name, "🔴 not configured", "-");
        }
    }

    if connected_count == 0 && !all {
        println!("No connected channels. Use --all to see all channels.");
    }

    println!();

    // Show compile-time features — from the same derived list, and for the
    // same reason: the `#[cfg]` pairs this replaces had drifted as well.
    println!("Compile-time features:");
    for channel in ChannelType::value_variants() {
        let (mark, state) = if channel.feature_enabled() {
            ("✅", "Compiled")
        } else {
            ("❌", "Not compiled")
        };
        println!("  {} {:<12} - {}", mark, channel.display_name(), state);
    }

    println!();
    println!("Available commands:");
    println!("  syscity channel add <channel> --token <TOKEN> [--agent <AGENT>]");
    println!("  syscity channel stop <channel>");
    println!("  syscity channel remove <channel> [--agent <AGENT>]");
    println!("  syscity channel status <channel> [--agent <AGENT>]");
    println!("  syscity channel test <channel> [--agent <AGENT>]");
    println!("  syscity channel list [--all]");
    println!();
    let names: Vec<&str> = ChannelType::value_variants()
        .iter()
        .map(|c| c.as_str())
        .collect();
    println!("Channels: {}", names.join(", "));
    println!();
    println!("Environment variables:");
    println!("  TELEGRAM_BOT_TOKEN, DISCORD_BOT_TOKEN, SLACK_BOT_TOKEN");
    println!("  WHATSAPP_ACCESS_TOKEN, WHATSAPP_PHONE_NUMBER_ID");
    println!("  QQ_APP_ID, QQ_APP_SECRET");
    println!("  LARK_APP_ID, LARK_APP_SECRET or FEISHU_APP_ID, FEISHU_APP_SECRET");

    Ok(())
}

/// Check channel status
async fn run_channel_status(
    channel: Option<ChannelType>,
    agent_filter: Option<String>,
) -> Result<()> {
    let config = load_gateway_config().await;

    // If --agent is specified without a channel, show all channels for that agent
    if channel.is_none() {
        if let Some(ref agent_name) = agent_filter {
            println!("📱 Channels for Agent: {}", agent_name);
            println!("{}", "=".repeat(30 + agent_name.len()));
            println!();

            if let Some(ref cfg) = config {
                let channels: Vec<(&str, &str)> = ChannelType::value_variants()
                    .iter()
                    .map(|c| (c.as_str(), c.display_name()))
                    .collect();

                let mut found = false;
                for (key, name) in &channels {
                    if let Some(channel_config) = cfg.channels.get(*key) {
                        let channel_agent = channel_config.agent_id.as_deref().unwrap_or("default");
                        if channel_agent == agent_name {
                            let status = if channel_config.enabled {
                                "🟢 enabled"
                            } else {
                                "🟡 disabled"
                            };
                            println!("{:12}: {}", name, status);
                            found = true;
                        }
                    }
                }

                if !found {
                    println!("No channels configured for agent '{}'", agent_name);
                }
            } else {
                println!("No Gateway configuration found.");
            }
        }
        return Ok(());
    }

    match channel {
        Some(ch) => {
            let channel_name = ch.as_str();
            let display_name = ch.display_name();

            // If --agent is specified, include it in the title
            if let Some(ref agent_name) = agent_filter {
                println!("📱 {} Status (Agent: {})", display_name, agent_name);
            } else {
                println!("📱 {} Status", display_name);
            }
            println!("{}", "=".repeat(20 + display_name.len()));

            if let Some(ref cfg) = config {
                if let Some(channel_config) = cfg.channels.get(channel_name) {
                    // If --agent specified, verify this channel is routed to that agent
                    let channel_agent = channel_config.agent_id.as_deref().unwrap_or("default");
                    if let Some(ref agent_name) = agent_filter {
                        if channel_agent != agent_name {
                            println!(
                                "⚠️  This channel is routed to agent '{}' (not '{}')",
                                channel_agent, agent_name
                            );
                            return Ok(());
                        }
                    }

                    if channel_config.enabled {
                        println!("Status: 🟢 Enabled in Gateway configuration");
                    } else {
                        println!("Status: 🟡 Disabled in Gateway configuration");
                    }

                    println!("Agent: {}", channel_agent);

                    println!(
                        "\nTo {} the channel:",
                        if channel_config.enabled {
                            "disable"
                        } else {
                            "enable"
                        }
                    );
                    if channel_config.enabled {
                        println!("  syscity channel stop {}", channel_name);
                    } else {
                        println!("  syscity channel add {}", channel_name);
                    }
                } else {
                    println!("Status: 🔴 Not configured");
                    println!("\nTo configure:");
                    println!("  syscity channel add {}", channel_name);
                }
            } else {
                println!("Status: 🔴 No Gateway configuration found");
                println!("\nThe channel will be configured when you run:");
                println!("  syscity channel add {}", channel_name);
            }
        }
        None => {
            // Show all channels
            println!("📱 Channel Status");
            println!("=================");

            if let Some(ref cfg) = config {
                let channels = [
                    ("telegram", "Telegram"),
                    ("discord", "Discord"),
                    ("slack", "Slack"),
                    ("whatsapp", "WhatsApp"),
                    ("qq", "QQ"),
                    ("feishu", "Feishu/Lark"),
                    ("websocket", "WebSocket"),
                    ("signal", "Signal"),
                    ("imessage", "iMessage"),
                    ("webchat", "WebChat"),
                ];

                println!("{:<15} {:<12} {:<20}", "Channel", "Status", "Agent");
                println!("{}", "-".repeat(50));

                for (key, name) in &channels {
                    if let Some(channel_config) = cfg.channels.get(*key) {
                        let status = if channel_config.enabled {
                            "🟢 enabled"
                        } else {
                            "🟡 disabled"
                        };
                        let agent = channel_config.agent_id.as_deref().unwrap_or("default");
                        println!("{:<15} {:<12} {:<20}", name, status, agent);
                    } else {
                        println!("{:<15} {:<12} {:<20}", name, "🔴 not configured", "-");
                    }
                }
            } else {
                println!("No Gateway configuration found.");
                println!("\nTo configure a channel, run:");
                println!("  syscity channel add <channel-name>");
            }
        }
    }
    Ok(())
}

/// Test channel configuration
async fn run_channel_test(channel: ChannelType, agent: Option<String>) -> Result<()> {
    let channel_name = channel.display_name();

    println!("🧪 Testing {} configuration...", channel_name);

    // Show agent information
    if let Some(ref agent_name) = agent {
        println!("  Agent filter: {}", agent_name);
    }

    // Check compile-time feature
    let feature_enabled = channel.feature_enabled();

    if feature_enabled {
        println!("  ✅ Feature compiled in");
    } else {
        println!("  ❌ Feature not compiled in");
    }

    // Check environment variables
    println!("\n  Environment variables:");
    match channel {
        ChannelType::Telegram => {
            if std::env::var("TELEGRAM_BOT_TOKEN").is_ok() {
                println!("    ✅ TELEGRAM_BOT_TOKEN set");
            } else {
                println!("    ⚠️  TELEGRAM_BOT_TOKEN not set");
            }
        }
        ChannelType::Discord => {
            if std::env::var("DISCORD_BOT_TOKEN").is_ok() {
                println!("    ✅ DISCORD_BOT_TOKEN set");
            } else {
                println!("    ⚠️  DISCORD_BOT_TOKEN not set");
            }
        }
        ChannelType::Slack => {
            if std::env::var("SLACK_BOT_TOKEN").is_ok() {
                println!("    ✅ SLACK_BOT_TOKEN set");
            } else {
                println!("    ⚠️  SLACK_BOT_TOKEN not set");
            }
            if std::env::var("SLACK_SIGNING_SECRET").is_ok() {
                println!("    ✅ SLACK_SIGNING_SECRET set");
            } else {
                println!("    ⚠️  SLACK_SIGNING_SECRET not set (optional)");
            }
        }
        ChannelType::Whatsapp => {
            if std::env::var("WHATSAPP_ACCESS_TOKEN").is_ok() {
                println!("    ✅ WHATSAPP_ACCESS_TOKEN set");
            } else {
                println!("    ⚠️  WHATSAPP_ACCESS_TOKEN not set");
            }
            if std::env::var("WHATSAPP_PHONE_NUMBER_ID").is_ok() {
                println!("    ✅ WHATSAPP_PHONE_NUMBER_ID set");
            } else {
                println!("    ⚠️  WHATSAPP_PHONE_NUMBER_ID not set");
            }
        }
        ChannelType::Qq => {
            if std::env::var("QQ_APP_ID").is_ok() {
                println!("    ✅ QQ_APP_ID set");
            } else {
                println!("    ⚠️  QQ_APP_ID not set");
            }
            if std::env::var("QQ_APP_SECRET").is_ok() {
                println!("    ✅ QQ_APP_SECRET set");
            } else {
                println!("    ⚠️  QQ_APP_SECRET not set");
            }
        }
        ChannelType::Feishu => {
            if std::env::var("LARK_APP_ID").is_ok() || std::env::var("FEISHU_APP_ID").is_ok() {
                println!("    ✅ LARK_APP_ID/FEISHU_APP_ID set");
            } else {
                println!("    ⚠️  LARK_APP_ID or FEISHU_APP_ID not set");
            }
            if std::env::var("LARK_APP_SECRET").is_ok()
                || std::env::var("FEISHU_APP_SECRET").is_ok()
            {
                println!("    ✅ LARK_APP_SECRET/FEISHU_APP_SECRET set");
            } else {
                println!("    ⚠️  LARK_APP_SECRET or FEISHU_APP_SECRET not set");
            }
        }
        ChannelType::Websocket => {
            println!("    ℹ️  WebSocket requires no environment variables");
        }
        ChannelType::Signal => {
            if std::env::var("SIGNAL_ACCOUNT").is_ok() {
                println!("    ✅ SIGNAL_ACCOUNT set");
            } else {
                println!("    ⚠️  SIGNAL_ACCOUNT not set");
            }
        }
        ChannelType::Imessage => {
            println!("    ℹ️  iMessage requires BlueBubbles server running on macOS");
        }
        ChannelType::Webchat => {
            println!("    ℹ️  WebChat requires no environment variables");
        }
        ChannelType::WechatMp => {
            println!("    ℹ️  WeChat MP uses the official-account console credentials (AppID/Secret/Token/EncodingAESKey)");
        }
        ChannelType::Line => {
            println!("    ℹ️  LINE uses the Messaging API credentials (channel access token + channel secret)");
        }
        ChannelType::Twilio => {
            println!("    ℹ️  Twilio uses the account SID + auth token, and needs the public webhook URL the console was given");
        }
        ChannelType::GoogleChat => {
            println!("    ℹ️  Google Chat needs a service account, the app's issuer/audience/JWKS, and the allowed service-account identities");
        }
        ChannelType::Teams => {
            println!("    ℹ️  Teams needs the bot's app registration (app id + secret + tenant) and the Bot Framework issuer/JWKS");
        }
        ChannelType::Wecom => {
            println!("    ℹ️  WeCom needs an AI Bot's Bot ID and Secret; it holds a WebSocket out, so it needs no public URL");
        }
        ChannelType::DingTalk => {
            println!("    ℹ️  DingTalk needs the app's Client ID and Client Secret, with the robot's message mode set to Stream");
        }
        ChannelType::Irc => {
            println!("    ℹ️  IRC needs a server, a nickname and a channel to join; TLS on port 6697 is the default");
        }
    }

    // Check Gateway config
    println!("\n  Gateway configuration:");
    let config_key = channel.as_str();

    if let Some(ref cfg) = load_gateway_config().await {
        if let Some(channel_config) = cfg.channels.get(config_key) {
            if channel_config.enabled {
                println!("    ✅ Channel enabled in config");
            } else {
                println!("    🟡 Channel disabled in config");
            }
            // Show agent information
            let configured_agent = channel_config.agent_id.as_deref().unwrap_or("default");
            println!("    📎 Routed to agent: {}", configured_agent);

            // Check if agent filter matches
            if let Some(ref filter_agent) = agent {
                if configured_agent != filter_agent {
                    println!(
                        "    ⚠️  Warning: Configured agent '{}' doesn't match filter '{}'",
                        configured_agent, filter_agent
                    );
                }
            }
        } else {
            println!("    🔴 Channel not configured");
        }
    } else {
        println!("    🔴 No Gateway configuration found");
    }

    println!("\n  To configure and start:");
    if let Some(ref agent_name) = agent {
        println!("    syscity channel add {} --agent {}", config_key, agent_name);
    } else {
        println!("    syscity channel add {}", config_key);
    }
    println!("    syscity start");

    Ok(())
}
