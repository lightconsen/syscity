//! `ModelRouter` completion/streaming request flow with fallback.

use super::*;

impl ModelRouter {
    // ==================== COMPLETION (non-streaming) ====================

    /// Complete a request using the model router.
    pub async fn complete(
        &self,
        model_id: &str,
        messages: Vec<Message>,
        tools: Option<Vec<ToolDefinition>>,
    ) -> crate::Result<CompletionResponse> {
        Ok(self.complete_with_route(model_id, messages, tools).await?.0)
    }

    /// Complete a request via the router, returning the response together with
    /// the [`RouteRecord`] describing the route decision (candidate chain,
    /// chosen model, fallback). The record feeds per-turn observability.
    pub async fn complete_with_route(
        &self,
        model_id: &str,
        messages: Vec<Message>,
        tools: Option<Vec<ToolDefinition>>,
    ) -> crate::Result<(CompletionResponse, RouteRecord)> {
        let requested = model_id.to_string();
        let (provider, model_id, request) =
            self.build_request(model_id, messages, tools, false).await?;
        let providers_to_try = self
            .get_providers_to_try(&provider, &model_id, &request)
            .await;

        let (response, mut rec) = self
            .route_with_fallback(&model_id, request, providers_to_try, |provider, req| async move {
                provider.complete(req).await
            })
            .await?;

        // Merge model re-resolution (capability upgrade / unknown-model
        // fallback inside build_request) into the route reason so the
        // observation captures the full decision.
        if model_id != requested {
            Self::merge_reason(
                &mut rec,
                &format!("model re-resolved '{requested}' → '{model_id}'"),
            );
        }
        Ok((response, rec))
    }

    /// Stream a completion through the router with fallback and key rotation.
    pub async fn stream(
        &self,
        model_id: &str,
        messages: Vec<Message>,
        tools: Option<Vec<ToolDefinition>>,
    ) -> crate::Result<CompletionStream> {
        Ok(self.stream_with_route(model_id, messages, tools).await?.0)
    }

    /// Stream a completion via the router, returning the stream together with
    /// the [`RouteRecord`] describing the route decision.
    pub async fn stream_with_route(
        &self,
        model_id: &str,
        messages: Vec<Message>,
        tools: Option<Vec<ToolDefinition>>,
    ) -> crate::Result<(CompletionStream, RouteRecord)> {
        let requested = model_id.to_string();
        let (provider, model_id, request) =
            self.build_request(model_id, messages, tools, true).await?;
        let providers_to_try = self
            .get_providers_to_try(&provider, &model_id, &request)
            .await;

        let (stream, mut rec) = self
            .route_with_fallback(&model_id, request, providers_to_try, |provider, req| async move {
                provider.stream(req).await
            })
            .await?;

        if model_id != requested {
            Self::merge_reason(
                &mut rec,
                &format!("model re-resolved '{requested}' → '{model_id}'"),
            );
        }
        Ok((stream, rec))
    }

    /// Append `addition` to a route record's reason (semicolon separated).
    pub(super) fn merge_reason(rec: &mut RouteRecord, addition: &str) {
        if let Some(reason) = &mut rec.reason {
            reason.push_str(&format!("; {addition}"));
        } else {
            rec.reason = Some(addition.to_string());
        }
    }

    /// Build a CompletionRequest and resolve the model to its provider.
    /// Returns `(provider, model_id, request)`. Unknown models fall back to
    /// the global default model.
    async fn build_request(
        &self,
        model_id: &str,
        messages: Vec<Message>,
        tools: Option<Vec<ToolDefinition>>,
        stream: bool,
    ) -> crate::Result<(String, String, CompletionRequest)> {
        let (provider, resolved_model) = {
            if let Some(provider) = self.provider_for_model(model_id).await {
                // A genuine `cloud/<id>` reference keeps the cloud proxy but
                // must reach the upstream (and observability, cost, cache) as
                // the bare model id.
                let bare = match crate::model_router::parse_model_ref(model_id) {
                    (Some("cloud"), rest)
                        if provider == "cloud" && self.cloud_serves_model(rest).await =>
                    {
                        rest.to_string()
                    }
                    _ => model_id.to_string(),
                };
                (provider, bare)
            } else {
                let default_model = self.get_default_model().await;
                let provider = self
                    .provider_for_model(&default_model)
                    .await
                    .ok_or_else(|| crate::error::ConfigError::InvalidValue {
                        key: "model".to_string(),
                        message: format!("Unknown model: {model_id}"),
                    })?;
                (provider, default_model)
            }
        };

        let request = CompletionRequest {
            model: Some(resolved_model.clone()),
            messages,
            temperature: None,
            max_tokens: None,
            stream,
            tools,
            stop: None,
            extra: None,
            requires_vision: false,
            requires_tools: false,
            requires_reasoning: false,
            ..Default::default()
        };

        let (provider, resolved_model) = self
            .resolve_model_with_capabilities(&provider, &resolved_model, &request)
            .await;
        Ok((provider, resolved_model, request))
    }

    /// Build the provider chain to try, including fallbacks.
    async fn get_providers_to_try(
        &self,
        provider: &str,
        model_id: &str,
        request: &CompletionRequest,
    ) -> Vec<FallbackEntry> {
        let mut providers_to_try = self.get_provider_chain(provider, model_id).await;

        for fallback in &request.fallback_models {
            if let Some(fb_provider) = self.provider_for_model(fallback).await {
                let fb_chain = self.get_provider_chain(&fb_provider, fallback).await;
                for entry in fb_chain {
                    if !providers_to_try
                        .iter()
                        .any(|e| e.provider == entry.provider && e.model == entry.model)
                    {
                        providers_to_try.push(entry);
                    }
                }
            }
        }

        providers_to_try
    }

    /// Generic routing with fallback, circuit breaker, key rotation, and retry.
    ///
    /// Handles the common fallback loop shared by [`complete`] and [`stream`],
    /// returning the successful `T` together with a [`RouteRecord`] describing
    /// the route decision (all candidates considered, the chosen model, and
    /// whether any candidate was skipped or failed first).
    async fn route_with_fallback<T, F, Fut>(
        &self,
        _model_id: &str,
        request: CompletionRequest,
        providers_to_try: Vec<FallbackEntry>,
        provider_fn: F,
    ) -> crate::Result<(T, RouteRecord)>
    where
        T: Send + 'static,
        F: Fn(Arc<dyn Provider>, CompletionRequest) -> Fut + Clone + Send + 'static,
        Fut: std::future::Future<Output = crate::Result<T>> + Send,
    {
        let mut last_error = None;
        // Full ordered list of candidate labels (including disabled /
        // circuit-open candidates that were skipped).
        let mut candidate_chain: Vec<String> = Vec::new();
        let mut failed = 0usize;
        let mut skipped = 0usize;

        for entry in providers_to_try {
            let label = Self::route_entry_label(&entry);
            if !entry.enabled {
                skipped += 1;
                candidate_chain.push(label);
                continue;
            }

            if self.is_circuit_open(&entry.provider).await {
                warn!("Circuit breaker open for provider: {}", entry.provider);
                skipped += 1;
                candidate_chain.push(label);
                continue;
            }

            let provider = {
                let providers = self.providers.read().await;
                providers.get(&entry.provider).cloned()
            };

            if let Some(provider) = provider {
                candidate_chain.push(label.clone());
                let start = std::time::Instant::now();
                let provider_clone = provider.clone();
                let request_clone = request.clone();
                let provider_fn_clone = provider_fn.clone();

                match provider_fn(provider, request.clone()).await {
                    Ok(response) => {
                        self.record_success(&entry.provider, start.elapsed()).await;
                        self.auth_profiles.record_success(&entry.provider).await;
                        let rec = RouteRecord {
                            candidate_chain,
                            chosen: label,
                            reason: Some(Self::route_reason(failed, skipped)),
                            fallback_occurred: failed > 0 || skipped > 0,
                        };
                        return Ok((response, rec));
                    }
                    Err(ref e) => {
                        let class = FailureClass::from_error(e, None);
                        warn!(
                            "Provider {} failed with {}: {}",
                            entry.provider,
                            class.description(),
                            e
                        );

                        match self
                            .handle_provider_failure(
                                &entry.provider,
                                &entry.model,
                                class,
                                e,
                                || {
                                    let p = provider_clone.clone();
                                    let r = request_clone.clone();
                                    let f = provider_fn_clone.clone();
                                    async move { f(p, r).await }
                                },
                            )
                            .await
                        {
                            Ok(response) => {
                                self.record_success(&entry.provider, start.elapsed()).await;
                                self.auth_profiles.record_success(&entry.provider).await;
                                let rec = RouteRecord {
                                    candidate_chain,
                                    chosen: label,
                                    reason: Some(Self::route_reason(failed, skipped)),
                                    fallback_occurred: failed > 0 || skipped > 0,
                                };
                                return Ok((response, rec));
                            }
                            Err(err) => {
                                last_error = Some(err);
                                failed += 1;
                            }
                        }
                    }
                }
            }
        }

        Err(last_error.unwrap_or_else(|| crate::error::SyscityError::ExternalService {
            source: "All providers failed".to_string(),
            cause: None,
        }))
    }

    /// Label for a fallback-chain entry as `"provider/model"`.
    fn route_entry_label(entry: &FallbackEntry) -> String {
        format!("{}/{}", entry.provider, entry.model)
    }

    /// Human-readable reason for a route outcome given the skip/failure counts.
    fn route_reason(failed: usize, skipped: usize) -> String {
        if failed == 0 && skipped == 0 {
            "primary".to_string()
        } else {
            format!("fallback after {failed} failed / {skipped} skipped candidate(s)")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model_router::model_catalog::ModelCatalogEntry;

    /// Provider that always fails its `complete` call (content-policy class is
    /// not retried, so the router moves to the next fallback candidate).
    #[derive(Clone)]
    struct AlwaysFailProvider;

    #[async_trait::async_trait]
    impl Provider for AlwaysFailProvider {
        fn name(&self) -> &str {
            "flaky"
        }
        fn default_model(&self) -> &str {
            "m1"
        }
        fn supports_tools(&self) -> bool {
            false
        }
        fn max_context(&self) -> usize {
            4096
        }
        async fn complete(&self, _request: CompletionRequest) -> crate::Result<CompletionResponse> {
            Err(crate::error::SyscityError::ExternalService {
                source: "OpenAI API error 400: Content policy violation".into(),
                cause: None,
            })
        }
        async fn stream(&self, _request: CompletionRequest) -> crate::Result<CompletionStream> {
            unimplemented!()
        }
        async fn health_check(&self) -> crate::Result<bool> {
            Ok(true)
        }
        async fn set_credential(
            &self,
            _credential: crate::model_router::Credential,
        ) -> crate::Result<()> {
            Ok(())
        }
    }

    #[derive(Clone)]
    struct HealthyProvider;

    #[async_trait::async_trait]
    impl Provider for HealthyProvider {
        fn name(&self) -> &str {
            "healthy"
        }
        fn default_model(&self) -> &str {
            "m1"
        }
        fn supports_tools(&self) -> bool {
            false
        }
        fn max_context(&self) -> usize {
            4096
        }
        async fn complete(&self, _request: CompletionRequest) -> crate::Result<CompletionResponse> {
            Ok(CompletionResponse {
                message: Message::assistant("ok"),
                model: "m1".to_string(),
                usage: Some(Usage::default()),
                finish_reason: Some("stop".to_string()),
            })
        }
        async fn stream(&self, _request: CompletionRequest) -> crate::Result<CompletionStream> {
            unimplemented!()
        }
        async fn health_check(&self) -> crate::Result<bool> {
            Ok(true)
        }
        async fn set_credential(
            &self,
            _credential: crate::model_router::Credential,
        ) -> crate::Result<()> {
            Ok(())
        }
    }

    #[tokio::test]
    async fn complete_with_route_records_fallback_decision() {
        let router = ModelRouter::new(crate::model_router::config::ModelRouterConfig::default());
        router
            .add_provider_instance("flaky", Arc::new(AlwaysFailProvider))
            .await
            .unwrap();
        router
            .add_provider_instance("healthy", Arc::new(HealthyProvider))
            .await
            .unwrap();
        router
            .model_catalog
            .register(ModelCatalogEntry::new("m1", "m1", "flaky"))
            .await;
        router
            .set_fallback_chain("m1", vec!["flaky".into(), "healthy".into()])
            .await
            .unwrap();

        let (response, rec) = router
            .complete_with_route("m1", vec![Message::user("hi")], None)
            .await
            .expect("should succeed after fallback");

        assert_eq!(response.message.content, "ok");
        assert_eq!(rec.chosen, "healthy/m1");
        assert_eq!(rec.candidate_chain, vec!["flaky/m1".to_string(), "healthy/m1".to_string()]);
        assert!(rec.fallback_occurred);
        let reason = rec.reason.as_deref().unwrap_or_default();
        assert!(reason.contains("fallback after 1 failed"), "reason: {reason}");

        // Plain `complete` still returns just the response (record discarded).
        let response = router
            .complete("m1", vec![Message::user("hi")], None)
            .await
            .expect("complete should succeed");
        assert_eq!(response.message.content, "ok");
    }

    /// Minimal OpenAI-compatible provider config for routing tests.
    fn pcfg(models: &[&str]) -> crate::model_router::ProviderConfig {
        use crate::model_router::{ProviderConfig, ProviderType};
        ProviderConfig {
            provider_type: ProviderType::OpenAi,
            models: models.iter().map(|s| s.to_string()).collect(),
            default_model: models.first().map(|s| s.to_string()).unwrap_or_default(),
            api_key: "test-key".to_string().into(),
            api_keys: vec![],
            auth_profile: None,
            oauth: None,
            base_url: None,
            timeout: std::time::Duration::from_secs(30),
            max_retries: 3,
            retry_delay_ms: 1000,
        }
    }

    /// `cloud/<bare>` selects the cloud proxy, and the model that reaches the
    /// upstream is the bare id (no `cloud/` leak into requests/observability).
    #[tokio::test]
    async fn build_request_routes_qualified_cloud_ref_to_cloud_with_bare_model() {
        let router = ModelRouter::new(crate::model_router::config::ModelRouterConfig::default());
        router
            .add_provider("deepseek", pcfg(&["deepseek-v4-pro"]))
            .await
            .unwrap();
        router
            .add_provider("cloud", pcfg(&["deepseek-v4-pro", "deepseek-flash"]))
            .await
            .unwrap();

        let (provider, model, request) = router
            .build_request("cloud/deepseek-v4-pro", vec![Message::user("hi")], None, false)
            .await
            .expect("qualified ref should route");
        assert_eq!(provider, "cloud");
        assert_eq!(model, "deepseek-v4-pro");
        assert_eq!(request.model.as_deref(), Some("deepseek-v4-pro"));
    }

    /// A bare duplicate id keeps resolving to the non-cloud provider (the
    /// pre-existing default), so current sessions are unaffected.
    #[tokio::test]
    async fn build_request_bare_duplicate_resolves_local() {
        let router = ModelRouter::new(crate::model_router::config::ModelRouterConfig::default());
        router
            .add_provider("deepseek", pcfg(&["deepseek-v4-pro"]))
            .await
            .unwrap();
        router
            .add_provider("cloud", pcfg(&["deepseek-v4-pro"]))
            .await
            .unwrap();

        let (provider, model, request) = router
            .build_request("deepseek-v4-pro", vec![Message::user("hi")], None, false)
            .await
            .expect("bare ref should route");
        assert_eq!(provider, "deepseek");
        assert_eq!(model, "deepseek-v4-pro");
        assert_eq!(request.model.as_deref(), Some("deepseek-v4-pro"));
    }

    /// A `cloud/` ref for a model the proxy does not serve is unknown and falls
    /// back to the global default model.
    #[tokio::test]
    async fn build_request_cloud_ghost_falls_back_to_default() {
        let mut config = crate::model_router::config::ModelRouterConfig::default();
        config.default_model = "local-default".to_string();
        let router = ModelRouter::new(config);
        router
            .add_provider("local", pcfg(&["local-default"]))
            .await
            .unwrap();
        router
            .add_provider("cloud", pcfg(&["deepseek-flash"]))
            .await
            .unwrap();

        let (provider, model, _) = router
            .build_request("cloud/ghost", vec![Message::user("hi")], None, false)
            .await
            .expect("unknown ref falls back to default");
        assert_eq!(provider, "local");
        assert_eq!(model, "local-default");
    }
}
