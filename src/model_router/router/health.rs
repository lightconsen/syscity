//! `ModelRouter` health checks, circuit breaker, and capability routing.

use super::*;

impl ModelRouter {
    // ==================== HEALTH CHECKS ====================

    /// Start the health check background task
    pub fn start_health_checks(self: Arc<Self>) {
        let token = self.shutdown_token.clone();
        let registry = self.task_registry.clone();
        let handle = tokio::spawn(async move {
            let interval_secs = {
                let config = self.config.read().await;
                config.health_check_interval_secs
            };
            let mut interval = tokio::time::interval(Duration::from_secs(interval_secs));
            loop {
                tokio::select! {
                    biased;
                    _ = token.cancelled() => {
                        info!("Model router health checks received shutdown signal, exiting");
                        break;
                    }
                    _ = interval.tick() => {}
                }
                self.run_health_checks().await;
            }
        });

        if let Some(registry) = registry {
            tokio::spawn(async move {
                registry
                    .insert_join("model_router:health_check", handle)
                    .await;
            });
        }
    }

    /// Run periodic health checks
    async fn run_health_checks(&self) {
        let provider_names: Vec<String> = {
            let providers = self.providers.read().await;
            providers.keys().cloned().collect()
        };
        let reset_secs = {
            let config = self.config.read().await;
            config.circuit_breaker_reset_secs
        };

        for name in provider_names {
            // First handle circuit-breaker state transitions (Open → HalfOpen).
            {
                let mut health = self.health.write().await;
                if let Some(h) = health.get_mut(&name) {
                    h.last_health_check = Some(chrono::Utc::now());

                    if h.state == CircuitState::Open {
                        if let Some(last_failure) = h.last_failure {
                            let elapsed = chrono::Utc::now() - last_failure;
                            if elapsed.num_seconds() >= reset_secs as i64 {
                                info!("Circuit breaker half-open for provider: {}", name);
                                h.state = CircuitState::HalfOpen;
                            }
                        }
                    }
                }
            }

            let provider = {
                let providers = self.providers.read().await;
                providers.get(&name).cloned()
            };

            if let Some(provider) = provider {
                let start = std::time::Instant::now();
                match provider.health_check().await {
                    Ok(true) => self.record_success(&name, start.elapsed()).await,
                    Ok(false) => {
                        debug!("Health probe reported unhealthy for {}", name);
                        self.record_probe_failure(&name).await;
                    }
                    Err(e) => {
                        debug!("Health probe failed for {}: {}", name, e);
                        self.record_probe_failure(&name).await;
                    }
                }
            }
        }
    }

    /// Resolve a model, upgrading to a capability-compatible model if needed.
    /// Returns `(provider, model_id)`.
    pub(super) async fn resolve_model_with_capabilities(
        &self,
        provider: &str,
        model_id: &str,
        request: &CompletionRequest,
    ) -> (String, String) {
        if !request.requires_vision && !request.requires_tools && !request.requires_reasoning {
            return (provider.to_string(), model_id.to_string());
        }

        let entry = self.model_catalog.get(provider, model_id).await;

        let compatible = entry.is_some_and(|e| {
            (!request.requires_vision || e.supports_vision)
                && (!request.requires_tools || e.supports_tools)
                && (!request.requires_reasoning || e.supports_reasoning)
        });

        if compatible {
            return (provider.to_string(), model_id.to_string());
        }

        // Search catalog for cheapest compatible model
        let candidates = self.model_catalog.list().await;
        let mut best: Option<(&ModelCatalogEntry, f64)> = None;

        for c in &candidates {
            if (!request.requires_vision || c.supports_vision)
                && (!request.requires_tools || c.supports_tools)
                && (!request.requires_reasoning || c.supports_reasoning)
            {
                let cost = c
                    .pricing
                    .as_ref()
                    .map(|p| p.input_per_1k + p.output_per_1k)
                    .unwrap_or(f64::MAX);
                if best.is_none_or(|(_, best_cost)| cost < best_cost) {
                    best = Some((c, cost));
                }
            }
        }

        if let Some((entry, _)) = best {
            info!(
                "Capability routing: upgraded '{}' (provider={}) to '{}' (provider={}) for \
                 vision={} tools={} reasoning={}",
                model_id,
                provider,
                entry.id,
                entry.provider,
                request.requires_vision,
                request.requires_tools,
                request.requires_reasoning,
            );
            return (entry.provider.clone(), entry.id.clone());
        }

        (provider.to_string(), model_id.to_string())
    }

    // ==================== PROVIDER CHAIN ====================

    /// Get the ordered list of providers to try for a model, falling back to
    /// the provider that owns the model when no chain is configured.
    pub(super) async fn get_provider_chain(
        &self,
        provider: &str,
        model_id: &str,
    ) -> Vec<FallbackEntry> {
        let chains = self.fallback_chains.read().await;

        if let Some(chain) = chains.get(model_id) {
            return chain.clone();
        }

        vec![FallbackEntry {
            provider: provider.to_string(),
            model: model_id.to_string(),
            enabled: true,
            health_score: 100,
        }]
    }

    /// Check if circuit breaker is open for a provider.
    ///
    /// This is an **optimistic** check — a concurrent health-check task may
    /// transition the state from `Open → HalfOpen` between when this returns
    /// `false` and when the actual provider call completes.  The downstream
    /// [`route_with_fallback`] handles that safely by recording the failure
    /// and re-opening the circuit (see [`record_failure`]).
    pub(super) async fn is_circuit_open(&self, provider: &str) -> bool {
        let health = self.health.read().await;
        health
            .get(provider)
            .is_some_and(|h| h.state == CircuitState::Open)
    }

    // ==================== HEALTH STATUS ====================

    /// Get health status for all providers
    pub async fn get_health_status(&self) -> HashMap<String, ProviderHealth> {
        let health = self.health.read().await;
        health
            .iter()
            .map(|(k, v)| {
                (
                    k.clone(),
                    ProviderHealth {
                        state: v.state,
                        failures: v.failures,
                        successes: v.successes,
                        last_failure: v.last_failure,
                        avg_latency_ms: v.avg_latency_ms,
                        last_health_check: v.last_health_check,
                    },
                )
            })
            .collect()
    }
}
