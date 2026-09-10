//! Security subsystem initialization.
//!
//! Creates the authentication manager, rate limiters, and command/mention gates
//! used by the gateway request path.

use std::sync::Arc;

use tracing::info;

use crate::gateway::rate_limit::{MultiTierRateLimitConfig, MultiTierRateLimiter, TierConfig};
use crate::gateway::GatewayConfig;
use crate::security::auth_store::AuthStore;
use crate::security::mention_gate::MentionGate;
use crate::security::runtime_audit::AuditLogger;
use crate::security::AuthManager;
use crate::security::RateLimiter;

/// Security initialization result.
pub struct SecurityInit {
    pub auth_manager: Arc<AuthManager>,
    pub rate_limiter: Arc<RateLimiter>,
    pub multi_tier_rate_limiter: Arc<MultiTierRateLimiter>,
    pub command_gate: Arc<crate::tools::command_gate::CommandGate>,
    pub mention_gate: Arc<MentionGate>,
}

/// Initialize authentication, rate limiting, and command/mention gates.
///
/// When `auth_store` is present the auth manager loads surviving sessions from
/// SQLite at startup so login state survives a restart.
pub async fn init_security(
    config: &GatewayConfig,
    audit_log_dyn: Arc<dyn AuditLogger>,
    auth_store: Option<Arc<AuthStore>>,
) -> crate::Result<SecurityInit> {
    let mut auth_manager = AuthManager::new()
        .with_pairing_required(config.security.pairing_required)
        .with_audit_log(audit_log_dyn);
    if let Some(store) = auth_store {
        auth_manager = auth_manager.with_persistence(store).await?;
    }
    let auth_manager = Arc::new(auth_manager);

    let rate_limiter = Arc::new(RateLimiter::new(
        config.security.rate_limit.capacity,
        config.security.rate_limit.refill_rate,
    ));

    let multi_tier_config = MultiTierRateLimitConfig {
        global: tier_config(&config.security.rate_limit.global),
        per_user: tier_config(&config.security.rate_limit.per_user),
        per_ip: tier_config(&config.security.rate_limit.per_ip),
        per_endpoint: tier_config(&config.security.rate_limit.per_endpoint),
        shared_secret: tier_config(&config.security.rate_limit.shared_secret),
        device_token: tier_config(&config.security.rate_limit.device_token),
        hook_auth: tier_config(&config.security.rate_limit.hook_auth),
        control_plane_write: tier_config(&config.security.rate_limit.control_plane_write),
        lockout: config.security.rate_limit.lockout,
        loopback_exempt: config.security.rate_limit.loopback_exempt,
    };
    let multi_tier_rate_limiter = Arc::new(MultiTierRateLimiter::new(multi_tier_config));

    let command_gate = {
        // When no authentication is required, default to an open gate that
        // allows all commands including admin-level commands.
        let gate = if matches!(config.security.auth_mode, crate::gateway::protocol::AuthMode::None)
        {
            let gate = crate::tools::command_gate::CommandGate::open();
            gate.set_user_level("anonymous", crate::tools::command_gate::UserLevel::Admin);
            gate
        } else {
            let gate = crate::tools::command_gate::CommandGate::new();
            gate.set_user_level("web_user", crate::tools::command_gate::UserLevel::User);
            gate.set_user_level("api_user", crate::tools::command_gate::UserLevel::User);
            gate
        };
        Arc::new(gate)
    };

    let mention_gate = {
        let gate = MentionGate::new(config.security.mention_gating.policy);
        for pattern in &config.security.mention_gating.allowlist {
            gate.add_allowlist("*", pattern.clone()).await;
        }
        for pattern in &config.security.mention_gating.blocklist {
            gate.add_blocklist("*", pattern.clone()).await;
        }
        Arc::new(gate)
    };

    info!("Security components initialized");

    Ok(SecurityInit {
        auth_manager,
        rate_limiter,
        multi_tier_rate_limiter,
        command_gate,
        mention_gate,
    })
}

fn tier_config(config: &crate::gateway::TierConfig) -> TierConfig {
    TierConfig {
        enabled: config.enabled,
        capacity: config.capacity,
        window_secs: config.window_secs,
    }
}
