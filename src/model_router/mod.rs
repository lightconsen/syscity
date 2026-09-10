//! Model Router - Multi-provider LLM support with fallback chain
//!
//! Provides:
//! - Provider-owned concrete model IDs (e.g. "deepseek-chat")
//! - Multi-provider routing (Anthropic, OpenAI, etc.)
//! - Automatic fallback on failure
//! - Health checking and load balancing
//! - Auth profile rotation with cooldown
//! - Cost-aware routing with pluggable task classification
// INVARIANTS-NONE: provider selection is stateless per request; failures carry stable codes (see resolver/web).

pub mod auth_profile;
pub mod auth_profile_store;
pub mod classifier;
pub mod config;
pub mod failure_class;
pub mod gateway_client;
pub mod model_catalog;
pub mod oauth_callback;
pub mod oauth_credential;
pub mod oauth_flow;
pub mod pkce;
pub mod router;
pub mod usage_fetcher;
pub mod usage_formatter;
pub mod usage_tracker;

// ------------------------------------------------------------------
// Re-exports from sub-modules
// ------------------------------------------------------------------

pub use auth_profile::{
    AuthProfile, AuthProfileConfig, AuthProfileManager, KeyStatus, ProfileStatus,
};
pub use auth_profile_store::AuthProfileStore;
pub use classifier::{KeywordTaskClassifier, TaskClassifierImpl};
pub use config::{
    parse_model_ref, provider_display_name, provider_preset_for_name, provider_presets,
    CircuitState, CostAwareConfig, FallbackEntry, ModelCost, ModelRouterConfig, OAuthConfig,
    ProviderConfig, ProviderHealth, ProviderHealthInfo, ProviderInfo, ProviderKey, ProviderPreset,
    ProviderType, TaskRoutingRule, TaskType,
};
pub use failure_class::FailureClass;
pub use gateway_client::{GatewayClient, HttpGatewayClient};
pub use model_catalog::{ModelCatalog, ModelCatalogEntry, ModelDiscoverySource, ModelPricing};
pub use oauth_callback::wait_for_callback;
pub use oauth_credential::Credential;
pub use oauth_flow::OAuthFlow;
pub use pkce::{challenge_from_verifier, generate_verifier};
pub use router::ModelRouter;
pub use usage_fetcher::{
    LocalBudgetFetcher, OpenAiUsageFetcher, UsageFetcher, UsageFetcherRegistry,
};
pub use usage_formatter::{
    format_provider_snapshot, format_tokens, format_usage_report, format_usage_summary_line,
    format_window, format_window_compact, FormatConfig,
};
pub use usage_tracker::{
    ProviderUsageSnapshot, ProviderUsageTracker, QuotaSource, UsageQuota, UsageTrackerConfig,
};
