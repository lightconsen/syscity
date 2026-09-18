//! Gateway Tests
//!
//! Tests for gateway configuration, routing logic, and state management.
//! These tests do not require a running server — they focus on config
//! serialization, defaults, and pure logic functions.

use syscity::channels::{ChannelType, MentionState};
use syscity::gateway::*;

// ── GatewayConfig Defaults & Serialization ───────────────────────────────────

#[test]
fn gateway_config_default_values() {
    let config = GatewayConfig::default();

    assert_eq!(config.host, "127.0.0.1");
    assert_eq!(config.port, 18080);
    assert!(config.channels.is_empty());
    assert!(config.providers.is_empty());
    // The default model lives in exactly one place since e416796; assert
    // against it rather than a literal, so a bump there does not fail here.
    assert_eq!(config.model, syscity::providers::DEFAULT_MODEL);
    assert_eq!(config.model_provider, "anthropic");
}

#[test]
fn gateway_config_serializes_to_json() {
    let config = GatewayConfig::default();
    let json = serde_json::to_value(&config).expect("GatewayConfig must serialize");

    assert!(json.get("host").is_some(), "missing 'host' field");
    assert!(json.get("port").is_some(), "missing 'port' field");
    assert!(json.get("channels").is_some(), "missing 'channels' field");
    assert!(json.get("model").is_some(), "missing 'model' field");
    assert!(json.get("security").is_some(), "missing 'security' field");
}

#[test]
fn gateway_config_roundtrips_through_json() {
    let original = GatewayConfig::default();
    let json = serde_json::to_string(&original).unwrap();
    let roundtripped: GatewayConfig =
        serde_json::from_str(&json).expect("GatewayConfig must roundtrip through JSON");

    assert_eq!(original.host, roundtripped.host);
    assert_eq!(original.port, roundtripped.port);
    assert_eq!(original.model, roundtripped.model);
}

#[test]
fn gateway_config_with_custom_model() {
    let mut config = GatewayConfig::default();
    config.model = "gpt-4o".to_string();
    config.model_provider = "openai".to_string();

    assert_eq!(config.model, "gpt-4o");
    assert_eq!(config.model_provider, "openai");
}

// ── ChannelConfig Builder & Access Control ───────────────────────────────────

#[test]
fn channel_config_new_with_type() {
    let config = ChannelConfig::new(ChannelType::Slack);

    assert_eq!(config.channel_type, ChannelType::Slack);
    assert!(config.enabled);
    assert!(config.credentials.is_empty());
    assert!(config.allow_from.is_empty());
    assert!(config.block_from.is_empty());
    assert!(config.agent_id.is_none());
}

#[test]
fn channel_config_with_dm_policy() {
    let config = ChannelConfig::new(ChannelType::Telegram)
        .with_dm_policy(syscity::security::pairing::DmPolicy::Pairing);

    assert_eq!(config.dm_policy, syscity::security::pairing::DmPolicy::Pairing);
}

#[test]
fn channel_config_with_allowlist() {
    let config = ChannelConfig::new(ChannelType::Discord)
        .with_allow_from(vec!["user1".to_string(), "user2".to_string()]);

    assert_eq!(config.allow_from.len(), 2);
    assert!(config.is_in_allowlist("user1"));
    assert!(config.is_in_allowlist("user2"));
    assert!(!config.is_in_allowlist("user3"));
}

#[test]
fn channel_config_blocklist() {
    let config =
        ChannelConfig::new(ChannelType::Whatsapp).with_allow_from(vec!["user1".to_string()]);

    // block_from is not set via builder, but we can test the method
    assert!(!config.is_blocked("user1"));
}

#[test]
fn channel_config_serializes_to_json() {
    let config = ChannelConfig::new(ChannelType::Slack)
        .with_dm_policy(syscity::security::pairing::DmPolicy::Allowlist)
        .with_allow_from(vec!["U123".to_string()]);

    let json = serde_json::to_value(&config).unwrap();
    assert_eq!(json["channel_type"], "slack");
    assert_eq!(json["enabled"], true);
    assert_eq!(json["dm_policy"], "allowlist");
}

// ── SecurityConfig Defaults ──────────────────────────────────────────────────

#[test]
fn security_config_defaults() {
    let config = SecurityConfig::default();

    assert!(config.enabled);
    assert!(!config.auth_required);
    assert!(!config.pairing_required);
    assert!(config.security_headers);
}

#[test]
fn rate_limit_config_defaults() {
    let config = RateLimitConfig::default();

    assert!(config.enabled);
    assert_eq!(config.capacity, 100);
    assert!(config.multi_tier);
    assert!(config.global.enabled);
    assert!(config.per_user.enabled);
    assert!(config.per_ip.enabled);
    assert!(!config.per_endpoint.enabled);
}

#[test]
fn tier_config_defaults() {
    let tier = TierConfig::default();

    assert!(tier.enabled);
    assert_eq!(tier.capacity, 100);
    assert_eq!(tier.window_secs, 60);
}

// ── ACP Config Defaults ──────────────────────────────────────────────────────

#[test]
fn acp_config_defaults() {
    let config = AcpConfig::default();

    assert!(config.enabled);
    assert_eq!(config.max_subagents, 10);
    assert_eq!(config.default_timeout_seconds, 300);
}

// ── CostGuard Config Defaults ────────────────────────────────────────────────

#[test]
fn cost_guard_config_defaults_unlimited() {
    let config = CostGuardConfig::default();

    assert_eq!(config.daily_limit_cents, 0);
    assert_eq!(config.hourly_action_limit, 0);
}

// ── Storage Config Defaults ──────────────────────────────────────────────────

#[test]
fn storage_config_defaults() {
    let config = StorageConfig::default();

    assert_eq!(config.storage_type, "sqlite");
    assert!(config.base_path.is_none());
    assert!(config.database_url.is_none());
}

// ── Vector Memory Config Defaults ────────────────────────────────────────────

#[test]
fn vector_memory_config_defaults() {
    let config = VectorMemoryConfig::default();

    assert!(!config.enabled);
    assert_eq!(config.embedding_model, "text-embedding-3-small");
    assert_eq!(config.embedding_dimension, 1536);
}

// ── Plugin Config Defaults ───────────────────────────────────────────────────

#[test]
fn plugin_config_defaults() {
    let config = PluginConfig::default();

    assert!(config.enabled);
    assert!(config.auto_load);
    assert!(config.plugin_dir.is_none());
}

// ── Hot Reload Config Defaults ───────────────────────────────────────────────

#[test]
fn hot_reload_config_defaults() {
    let config = HotReloadConfig::default();

    assert!(config.enabled);
    assert!(config.watch_config);
    assert!(config.watch_agents);
    assert!(config.watch_plugins);
    assert_eq!(config.debounce_seconds, 2);
}

// ── Cron Config Defaults ─────────────────────────────────────────────────────

#[test]
fn cron_config_defaults() {
    let config = CronConfig::default();

    assert!(config.enabled);
    assert_eq!(config.check_interval_seconds, 60);
}

// ── MentionState Logic ───────────────────────────────────────────────────────

#[test]
fn mention_state_should_process_dm_always() {
    assert!(MentionState::DirectMessage.should_process(false));
    assert!(MentionState::DirectMessage.should_process(true));
}

#[test]
fn mention_state_should_process_mentioned_always() {
    assert!(MentionState::Mentioned.should_process(false));
    assert!(MentionState::Mentioned.should_process(true));
}

#[test]
fn mention_state_should_process_not_mentioned_depends_on_requirement() {
    assert!(MentionState::NotMentioned.should_process(false));
    assert!(!MentionState::NotMentioned.should_process(true));
}

// ── GatewayEvent Serialization ───────────────────────────────────────────────

#[test]
fn gateway_event_message_received_serializes() {
    let event = GatewayEvent::MessageReceived {
        channel: "telegram".to_string(),
        user_id: "user_1".to_string(),
        content: "Hello".to_string(),
        timestamp: chrono::Utc::now(),
    };

    let json = serde_json::to_value(&event).unwrap();
    assert!(json.get("MessageReceived").is_some());
}

#[test]
fn gateway_event_agent_status_serializes() {
    let event = GatewayEvent::AgentStatus {
        agent_id: "agent_1".to_string(),
        status: AgentStatus::Idle,
    };

    let json = serde_json::to_value(&event).unwrap();
    assert!(json.get("AgentStatus").is_some());
}

#[test]
fn agent_status_variants_serialize() {
    let statuses = vec![
        AgentStatus::Idle,
        AgentStatus::Processing { session_id: "s1".to_string() },
        AgentStatus::Error("fail".to_string()),
        AgentStatus::Shutdown,
    ];

    for status in statuses {
        let json = serde_json::to_value(&status).unwrap();
        assert!(json.is_object() || json.is_string(), "AgentStatus must serialize to JSON");
    }
}

// ── RepairRecord ─────────────────────────────────────────────────────────────

#[test]
fn repair_record_new() {
    let record = RepairRecord::new("agent_1");

    assert_eq!(record.target, "agent_1");
    assert_eq!(record.restart_count, 0);
    assert!(record.last_restart_at.is_none());
    assert!(!record.abandoned);
}

#[test]
fn repair_state_new() {
    let state = RepairState::new();

    // Should be empty initially
    assert!(!state.loop_running.load(std::sync::atomic::Ordering::SeqCst));
}

// ── EmbeddingProviderType ────────────────────────────────────────────────────

#[test]
fn embedding_provider_type_default() {
    assert_eq!(EmbeddingProviderType::default(), EmbeddingProviderType::OpenAi);
}

#[test]
fn embedding_provider_type_serializes_to_snake_case() {
    let openai = serde_json::to_value(EmbeddingProviderType::OpenAi).unwrap();
    let local = serde_json::to_value(EmbeddingProviderType::LocalGguf).unwrap();

    assert_eq!(openai.as_str().unwrap(), "open_ai");
    assert_eq!(local.as_str().unwrap(), "local_gguf");
}

// ── Full Config File Simulation ──────────────────────────────────────────────

#[test]
fn gateway_config_toml_roundtrip() {
    let config = GatewayConfig::default();

    let toml_str = toml::to_string(&config).expect("GatewayConfig must serialize to TOML");
    let roundtripped: GatewayConfig =
        toml::from_str(&toml_str).expect("GatewayConfig must deserialize from TOML");

    assert_eq!(config.host, roundtripped.host);
    assert_eq!(config.port, roundtripped.port);
    assert_eq!(config.model, roundtripped.model);
}
