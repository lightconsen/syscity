//! Tests for the gateway configuration types.

use super::*;

/// Regression: the auto-generated default config written by the desktop
/// shell (and mobile hosts) must round-trip through the TOML parser. A
/// hand-written template drifted out of sync with GatewayConfig's schema
/// (missing `security.rate_limit`, `[model]` as a table instead of flat
/// keys) and silently fell back to defaults on the next start.
#[test]
fn default_config_round_trips() {
    let toml_str =
        toml::to_string_pretty(&GatewayConfig::default()).expect("serialize default config");
    // The serialized form must use the flat model keys, not a [model] table.
    assert!(toml_str.contains("\nmodel = "), "flat model key missing");
    assert!(!toml_str.contains("\n[model]\n"), "[model] table should not exist");
    // And the security section must carry the required rate_limit table.
    assert!(
        toml_str.contains("[security.rate_limit]"),
        "security.rate_limit missing from default config"
    );

    // A hand-written file with a handful of keys must load those keys and
    // default the rest — not fail and take the whole file with it.
    let partial = r#"
host = "0.0.0.0"
port = 12345
model = "some-model"

[security]
auth_mode = "token"
shared_token = "abc"
"#;
    let parsed: GatewayConfig = toml::from_str(partial).expect("a partial config must deserialize");
    assert_eq!(parsed.host, "0.0.0.0");
    assert_eq!(parsed.port, 12345);
    assert_eq!(parsed.model, "some-model", "specified fields win");
    assert_eq!(parsed.security.auth_mode, crate::gateway::protocol::AuthMode::Token);
    assert_eq!(parsed.security.shared_token.as_deref(), Some("abc"));
    // Everything the file did not mention keeps its default.
    assert_eq!(parsed.model_provider, default_model_provider());
    assert_eq!(parsed.security.local_scopes, default_local_scopes());
    assert!(parsed.security.allowed_ws_origins.is_empty());

    // A genuinely malformed file must still be rejected, so the new
    // leniency does not turn typos into silent defaults.
    assert!(toml::from_str::<GatewayConfig>("port = \"not a number\"").is_err());

    // The same for the security sub-tables: a partial `[security.cors]`
    // must fill the rest from defaults, not fail the whole parse (which
    // would silently revert every other setting to its default).
    let partial_security = "[security.cors]\nallow_credentials = true\n";
    let parsed: GatewayConfig =
        toml::from_str(partial_security).expect("a partial cors table must deserialize");
    assert!(parsed.security.cors.allow_credentials);
    assert_eq!(
        parsed.security.cors.allowed_origins,
        vec!["*".to_string()],
        "unspecified fields come from the default"
    );

    // A channel table can be sparse: the type must not require an entry to
    // spell out `credentials` and `enabled` (they default like
    // `ChannelConfig::new` fills them) — but it MUST name its type.
    let sparse_channel = "[channels.telegram]\nchannel_type = \"telegram\"\n";
    let parsed: GatewayConfig =
        toml::from_str(sparse_channel).expect("a sparse channel table must deserialize");
    let telegram = parsed
        .channels
        .get("telegram")
        .expect("the telegram channel is present");
    assert!(telegram.enabled, "enabled defaults on like ChannelConfig::new");
    assert!(telegram.credentials.is_empty(), "credentials default to empty");

    // A channel entry without a type is still rejected — guessing would
    // silently misroute messages.
    assert!(toml::from_str::<GatewayConfig>("[channels.ghost]\nenabled = true\n").is_err());

    let parsed: GatewayConfig = toml::from_str(&toml_str).expect("default config must re-parse");
    assert_eq!(parsed.model, default_model());
    assert_eq!(parsed.model_provider, default_model_provider());
    assert_eq!(parsed.security.auth_mode, crate::gateway::protocol::AuthMode::None);
    assert!(!parsed.security.auth_required);
    assert_eq!(parsed.host, "127.0.0.1");
    assert_eq!(parsed.port, 18080);
}

#[test]
fn provider_for_model_finds_owning_provider() {
    let mut config = GatewayConfig::default();
    config.providers.insert(
        "deepseek".to_string(),
        crate::model_router::ProviderConfig {
            provider_type: crate::model_router::ProviderType::OpenAi,
            models: vec!["deepseek-chat".to_string(), "deepseek-reasoner".to_string()],
            default_model: "deepseek-chat".to_string(),
            api_key: String::new().into(),
            api_keys: Vec::new(),
            auth_profile: None,
            oauth: None,
            base_url: None,
            timeout: std::time::Duration::from_secs(30),
            max_retries: 3,
            retry_delay_ms: 1000,
        },
    );
    assert_eq!(config.provider_for_model("deepseek-chat"), Some("deepseek"));
    assert_eq!(config.provider_for_model("deepseek-reasoner"), Some("deepseek"));
    assert_eq!(config.provider_for_model("gpt-4o"), None);
}

// ── agent_overrides ───────────────────────────────────────────────────────

#[test]
fn apply_agent_overrides_merges_non_none_fields() {
    let mut config = GatewayConfig::default();
    config.agent_overrides.insert(
        "coder".to_string(),
        AgentOverrides {
            temperature: Some(0.2),
            max_tokens: Some(4096),
            system_prompt: Some("You are a code reviewer".to_string()),
            ..Default::default()
        },
    );

    let mut base = AgentConfig::default();
    config.apply_agent_overrides("coder", &mut base);

    assert_eq!(base.temperature, 0.2);
    assert_eq!(base.max_tokens, 4096);
    assert_eq!(base.system_prompt, "You are a code reviewer");
    // Unset fields keep the base value.
    assert_eq!(base.max_concurrent_tools, AgentConfig::default().max_concurrent_tools);
}

#[test]
fn apply_agent_overrides_no_entry_is_noop() {
    let config = GatewayConfig::default();
    let mut base = AgentConfig::default();
    config.apply_agent_overrides("ghost", &mut base);
    assert_eq!(base.temperature, AgentConfig::default().temperature);
}

#[test]
fn apply_agent_override_field_roundtrip() {
    let mut config = GatewayConfig::default();

    assert!(config
        .apply_agent_override_field("coder", "temperature", &serde_json::json!(0.5))
        .unwrap());
    assert_eq!(config.agent_overrides.get("coder").unwrap().temperature, Some(0.5));

    // Same value again → no change.
    assert!(!config
        .apply_agent_override_field("coder", "temperature", &serde_json::json!(0.5))
        .unwrap());

    // null clears the field; the now-empty entry is dropped.
    assert!(config
        .apply_agent_override_field("coder", "temperature", &serde_json::Value::Null)
        .unwrap());
    assert!(!config.agent_overrides.contains_key("coder"));
}

#[test]
fn apply_agent_override_field_empty_prompt_clears() {
    let mut config = GatewayConfig::default();
    config
        .apply_agent_override_field("coder", "system_prompt", &serde_json::json!("hi"))
        .unwrap();
    assert_eq!(
        config
            .agent_overrides
            .get("coder")
            .unwrap()
            .system_prompt
            .as_deref(),
        Some("hi")
    );
    config
        .apply_agent_override_field("coder", "system_prompt", &serde_json::json!(""))
        .unwrap();
    // Clearing the only override drops the whole entry.
    assert!(!config.agent_overrides.contains_key("coder"));
}

#[test]
fn apply_agent_override_field_rejects_unknown_field() {
    let mut config = GatewayConfig::default();
    let res = config.apply_agent_override_field("coder", "bogus", &serde_json::json!(1));
    assert!(res.is_err());
}

#[test]
fn clear_agent_overrides_resets_whole_agent() {
    let mut config = GatewayConfig::default();
    config
        .apply_agent_override_field("coder", "max_tokens", &serde_json::json!(512))
        .unwrap();
    assert!(config.agent_overrides.contains_key("coder"));
    config.clear_agent_overrides("coder");
    assert!(!config.agent_overrides.contains_key("coder"));
}

#[test]
fn agent_overrides_serialize_roundtrip() {
    let mut config = GatewayConfig::default();
    config
        .apply_agent_override_field("coder", "max_context_tokens", &serde_json::json!(8192))
        .unwrap();

    let toml_str = toml::to_string_pretty(&config).unwrap();
    let parsed: GatewayConfig = toml::from_str(&toml_str).unwrap();
    assert_eq!(
        parsed
            .agent_overrides
            .get("coder")
            .unwrap()
            .max_context_tokens,
        Some(8192)
    );
}

/// `tui.theme` survives a full serialization round-trip, and a config file
/// written before the field existed still parses (defaults to `Auto`).
#[test]
fn tui_theme_roundtrips_and_defaults_for_old_files() {
    let mut config = GatewayConfig::default();
    config.tui.theme = ThemeSetting::Light;
    let toml_str = toml::to_string_pretty(&config).unwrap();
    let parsed: GatewayConfig = toml::from_str(&toml_str).unwrap();
    assert_eq!(parsed.tui.theme, ThemeSetting::Light);

    // A bare `[tui]` (say, an old file someone half-edited) is fine too.
    let from_scratch: GatewayConfig = toml::from_str("[tui]\n").unwrap();
    assert_eq!(from_scratch.tui.theme, ThemeSetting::Auto);

    // An unknown value name is a hard parse error, not a silent default.
    let bad = toml::from_str::<GatewayConfig>("[tui]\ntheme = \"neon\"\n");
    assert!(bad.is_err());
}

/// Build two configs with identical content but HashMaps populated in
/// different insertion orders (channels, search keys, agent models). Their
/// revisions must be identical — the CAS fingerprint cannot depend on
/// HashMap iteration order.
#[test]
fn config_revision_stable_across_insertion_order() {
    let mut a = GatewayConfig::default();
    for (name, ty) in [("a", "telegram"), ("b", "slack"), ("c", "discord")] {
        let mut ch = ChannelConfig::new(match ty {
            "telegram" => crate::channels::ChannelType::Telegram,
            "slack" => crate::channels::ChannelType::Slack,
            _ => crate::channels::ChannelType::Discord,
        });
        ch.agent_id = Some(name.to_string());
        a.channels.insert(name.to_string(), ch);
    }
    a.search.keys.insert("k1".into(), "v1".into());
    a.search.keys.insert("k2".into(), "v2".into());
    a.agent_models.insert("m1".into(), "gpt-4o".into());
    a.agent_models.insert("m2".into(), "claude".into());

    let mut b = GatewayConfig::default();
    for (name, ty) in [("c", "discord"), ("b", "slack"), ("a", "telegram")] {
        let mut ch = ChannelConfig::new(match ty {
            "telegram" => crate::channels::ChannelType::Telegram,
            "slack" => crate::channels::ChannelType::Slack,
            _ => crate::channels::ChannelType::Discord,
        });
        ch.agent_id = Some(name.to_string());
        b.channels.insert(name.to_string(), ch);
    }
    b.search.keys.insert("k2".into(), "v2".into());
    b.search.keys.insert("k1".into(), "v1".into());
    b.agent_models.insert("m2".into(), "claude".into());
    b.agent_models.insert("m1".into(), "gpt-4o".into());

    assert_eq!(config_revision(&a), config_revision(&b));
}

#[test]
fn config_revision_changes_when_config_changes() {
    let mut config = GatewayConfig::default();
    let before = config_revision(&config);
    config.default_agent.temperature = 0.5;
    let after = config_revision(&config);
    assert_ne!(before, after);
}

#[test]
fn config_revision_matches_config_json_hash() {
    let config = GatewayConfig::default();
    let value = serde_json::to_value(&config).unwrap();
    assert_eq!(config_revision(&config), config_json_hash(&value));
}
