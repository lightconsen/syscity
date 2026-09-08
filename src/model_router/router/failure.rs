//! `ModelRouter` provider creation, failure recording, and key rotation.

use super::*;

impl ModelRouter {
    /// Create a provider instance from config
    pub(super) async fn create_provider(
        &self,
        config: &ProviderConfig,
    ) -> crate::Result<Arc<dyn Provider + Send + Sync>> {
        let api_key = config.effective_key().await;
        let provider_type = config.provider_type.to_string();

        // Map legacy provider_type names to preset names
        let provider_type = match provider_type.as_str() {
            "moonshot" => "kimi",
            other => other,
        };

        use crate::providers::resolver::{resolve_from_config, ProviderOverrides};

        resolve_from_config(
            provider_type,
            Some(api_key),
            ProviderOverrides {
                // protocol: auto-detect from preset default
                base_url: config.base_url.clone(),
                // Honor the config's default_model: without this the concrete
                // provider instance falls back to the *preset* default model
                // (e.g. the openai preset's `gpt-5.4-mini`), so any
                // `model: None` request (query expansion, utility calls) would
                // send the wrong model to the configured endpoint.
                model: (!config.default_model.is_empty()).then(|| config.default_model.clone()),
                ..ProviderOverrides::default()
            },
        )
        .map(|p| p as Arc<dyn Provider + Send + Sync>)
    }

    /// Rotate the active credential for a provider in place.
    async fn rotate_provider_credential(
        &self,
        provider_name: &str,
        cooldown_secs: u64,
    ) -> crate::Result<()> {
        let provider = {
            let providers = self.providers.read().await;
            providers.get(provider_name).cloned().ok_or_else(|| {
                crate::error::ConfigError::InvalidValue {
                    key: "provider".to_string(),
                    message: format!("Unknown provider: {provider_name}"),
                }
            })?
        };

        match self
            .auth_profiles
            .rotate(provider_name, cooldown_secs)
            .await
        {
            Some(new_key) => {
                provider
                    .set_credential(Credential::api_key(new_key))
                    .await?;
                info!("Rotated credential for provider '{provider_name}'");
                Ok(())
            }
            None => Err(crate::error::SyscityError::ExternalService {
                source: format!(
                    "No available API keys for provider '{provider_name}' after rotation"
                ),
                cause: None,
            }),
        }
    }

    /// Compute the effective cooldown for a failure class.
    async fn cooldown_for_failure(&self, provider: &str, class: FailureClass) -> u64 {
        let config = self.config.read().await;
        let base = config
            .providers
            .get(provider)
            .map(|pc| pc.derived_auth_profile_config().cooldown_secs)
            .unwrap_or(60);
        drop(config);
        class.default_backoff_secs().max(base)
    }

    // ==================== RECORDING SUCCESS / FAILURE ====================

    /// Record a successful completion, updating health, auth profile and usage.
    pub(super) async fn record_completion_success(
        &self,
        provider: &str,
        latency: Duration,
        usage: Option<Usage>,
        model: &str,
    ) {
        self.record_success(provider, latency).await;
        self.auth_profiles.record_success(provider).await;
        if let Some(usage) = usage {
            self.usage_tracker.record(provider, usage, model).await;
        }
    }

    /// Record a successful request
    pub(super) async fn record_success(&self, provider: &str, latency: Duration) {
        let mut health = self.health.write().await;
        if let Some(h) = health.get_mut(provider) {
            h.successes += 1;
            h.failures = 0;
            h.state = CircuitState::Closed;

            let latency_ms = latency.as_millis() as u64;
            h.avg_latency_ms = (h.avg_latency_ms * 9 + latency_ms) / 10;
        }
    }

    /// Record a failed request with optional failure classification.
    ///
    /// Uses the classification to make smarter circuit-breaker decisions
    /// (e.g. rate-limit errors open the circuit faster).
    pub(super) async fn record_failure(&self, provider: &str, class: Option<FailureClass>) {
        let config = self.config.read().await;
        let threshold = config.circuit_breaker_threshold;
        drop(config);

        let mut health = self.health.write().await;
        if let Some(h) = health.get_mut(provider) {
            h.failures += 1;
            h.last_failure = Some(chrono::Utc::now());

            let effective_threshold = match class {
                Some(FailureClass::RateLimit) => threshold.saturating_sub(2).max(1),
                Some(FailureClass::Overloaded) => threshold.saturating_sub(1).max(1),
                _ => threshold,
            };

            // Fix (Issue 3): also transition HalfOpen → Open on failure.
            if h.failures >= effective_threshold && h.state != CircuitState::Open {
                warn!(
                    "Circuit breaker opened for provider: {provider} ({} failures, class={:?})",
                    h.failures, class
                );
                h.state = CircuitState::Open;
            }
        }
    }

    /// Record a failed *health probe*. Unlike [`record_failure`], this does
    /// NOT refresh `last_failure` while the breaker is already Open: the
    /// Open→HalfOpen cooldown must keep running so real traffic can trial the
    /// provider after `circuit_breaker_reset_secs` even while probes keep
    /// failing. (Probe failures used to pin `last_failure`, making the
    /// HalfOpen transition unreachable and locking the provider out until a
    /// probe itself succeeded — a permanent lockout for any provider whose
    /// probe endpoint is broken but whose real endpoint works.)
    pub(super) async fn record_probe_failure(&self, provider: &str) {
        let config = self.config.read().await;
        let threshold = config.circuit_breaker_threshold;
        drop(config);

        let mut health = self.health.write().await;
        if let Some(h) = health.get_mut(provider) {
            h.failures += 1;
            if h.state != CircuitState::Open {
                h.last_failure = Some(chrono::Utc::now());
                if h.failures >= threshold {
                    warn!(
                        "Circuit breaker opened for provider: {provider} ({} probe failures)",
                        h.failures
                    );
                    h.state = CircuitState::Open;
                }
            }
        }
    }

    /// Handle a provider failure, applying key rotation/disable and a single
    /// retry when appropriate.
    pub(super) async fn handle_provider_failure<T, F, Fut>(
        &self,
        provider_name: &str,
        model: &str,
        class: FailureClass,
        error: &crate::error::SyscityError,
        retry_once: F,
    ) -> crate::Result<T>
    where
        F: FnOnce() -> Fut,
        Fut: std::future::Future<Output = crate::Result<T>>,
    {
        if class == FailureClass::ModelNotFound {
            self.model_catalog.suppress(provider_name, model).await;
            warn!("Auto-suppressed model {provider_name}:{model}");
        }

        if class.should_disable_key() {
            let cooldown = self.cooldown_for_failure(provider_name, class).await;
            if let Err(disable_err) = self
                .rotate_provider_credential(provider_name, cooldown)
                .await
            {
                error!("Key disable/rotation failed for provider {provider_name}: {disable_err}");
            }
            self.record_failure(provider_name, Some(class)).await;
            return Err(crate::error::SyscityError::ExternalService {
                source: format!("Provider {provider_name} auth disabled: {error}"),
                cause: None,
            });
        }

        if class.should_rotate_key() {
            let cooldown = self.cooldown_for_failure(provider_name, class).await;
            match self
                .rotate_provider_credential(provider_name, cooldown)
                .await
            {
                Ok(()) => match retry_once().await {
                    Ok(response) => return Ok(response),
                    Err(e2) => {
                        let class2 = FailureClass::from_error(&e2, None);
                        error!("Provider {provider_name} failed after key rotation: {e2}");
                        self.record_failure(provider_name, Some(class2)).await;
                        return Err(e2);
                    }
                },
                Err(rotate_err) => {
                    error!("Key rotation failed for provider {provider_name}: {rotate_err}");
                    self.record_failure(provider_name, Some(class)).await;
                    return Err(rotate_err);
                }
            }
        }

        self.record_failure(provider_name, Some(class)).await;
        Err(crate::error::SyscityError::ExternalService {
            source: format!("Provider {provider_name} failed: {error}"),
            cause: None,
        })
    }

    // ==================== AUTH PROFILE MANAGEMENT ====================

    /// Get auth profile status for a provider
    pub async fn get_auth_profile_status(&self, provider_name: &str) -> Option<ProfileStatus> {
        self.auth_profiles.get_status(provider_name).await
    }

    /// Get auth profile status for all providers
    pub async fn list_auth_profiles(&self) -> Vec<ProfileStatus> {
        self.auth_profiles.all_statuses().await
    }

    /// Manually rotate the auth key for a provider
    pub async fn rotate_auth_key(&self, provider_name: &str) -> crate::Result<String> {
        let provider = {
            let providers = self.providers.read().await;
            providers.get(provider_name).cloned().ok_or_else(|| {
                crate::error::ConfigError::InvalidValue {
                    key: "provider".to_string(),
                    message: format!("Unknown provider: {provider_name}"),
                }
            })?
        };

        match self.auth_profiles.rotate(provider_name, 60).await {
            Some(new_key) => {
                provider
                    .set_credential(Credential::api_key(new_key.clone()))
                    .await?;
                info!("Manually rotated auth key for provider '{provider_name}'");
                Ok(new_key)
            }
            None => Err(crate::error::SyscityError::ExternalService {
                source: format!(
                    "No available API keys for provider '{provider_name}' after rotation"
                ),
                cause: None,
            }),
        }
    }
}
