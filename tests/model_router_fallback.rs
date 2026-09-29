//! Model-router routing and fallback — the production-critical path that had
//! no test above unit level (`router/{health,quota,cost_aware,failure,init,
//! admin}.rs` are ~1640 lines with zero tests between them).
//!
//! Two journeys: a request finds its registered provider, and a primary that
//! fails hands the request to the next provider in the chain — with the route
//! record naming who served it.

use std::sync::Arc;

use syscity::error::SyscityError;
use syscity::model_router::{ModelCatalogEntry, ModelRouter, ModelRouterConfig};
use syscity::providers::{mock::MockProvider, Message};

/// A non-rotating, non-disabling failure (`ContentPolicy` class): the router
/// records it and advances the chain rather than retrying the same provider.
fn failing_provider() -> MockProvider {
    MockProvider::new().with_error_callback(|_msgs| {
        Some(SyscityError::ExternalService {
            source: "OpenAI API error 400: Content policy violation".to_string(),
            cause: None,
        })
    })
}

#[tokio::test]
async fn routes_to_the_registered_provider() {
    let router = ModelRouter::new(ModelRouterConfig::default());
    let primary = MockProvider::new().with_responses(vec![Message::assistant("served-by-primary")]);

    router
        .add_provider_instance("primary", Arc::new(primary.clone()))
        .await
        .expect("register provider");
    // Registration alone does not make a model routable: the catalog entry is
    // what `provider_for_model` resolves.
    router
        .model_catalog
        .register(ModelCatalogEntry::new("m1", "m1", "primary"))
        .await;

    let (response, route) = router
        .complete_with_route("m1", vec![Message::user("hi")], None)
        .await
        .expect("a registered provider must serve the request");

    assert_eq!(response.message.content, "served-by-primary");
    assert_eq!(route.chosen, "primary/m1");
    assert!(!route.fallback_occurred, "nothing should have failed: {route:?}");
    assert_eq!(primary.call_count(), 1);
}

#[tokio::test]
async fn falls_back_when_the_primary_fails() {
    let router = ModelRouter::new(ModelRouterConfig::default());
    let flaky = failing_provider();
    let healthy =
        MockProvider::new().with_responses(vec![Message::assistant("served-by-fallback")]);

    router
        .add_provider_instance("flaky", Arc::new(flaky))
        .await
        .expect("register flaky");
    router
        .add_provider_instance("healthy", Arc::new(healthy.clone()))
        .await
        .expect("register healthy");
    // The catalog must own the model before the chain can name it.
    router
        .model_catalog
        .register(ModelCatalogEntry::new("m1", "m1", "flaky"))
        .await;
    router
        .set_fallback_chain("m1", vec!["flaky".to_string(), "healthy".to_string()])
        .await
        .expect("chain registers once the model is known");

    let (response, route) = router
        .complete_with_route("m1", vec![Message::user("hi")], None)
        .await
        .expect("the healthy provider must answer after the primary failed");

    assert_eq!(response.message.content, "served-by-fallback");
    assert_eq!(route.chosen, "healthy/m1", "the fallback served it");
    assert!(route.fallback_occurred, "the record must show the fallback: {route:?}");
    assert_eq!(
        route.candidate_chain,
        vec!["flaky/m1".to_string(), "healthy/m1".to_string()],
        "the failed candidate stays in the chain for the operator"
    );
    assert_eq!(healthy.call_count(), 1, "the fallback must be called exactly once");
}

#[tokio::test]
async fn all_providers_failing_is_an_error_not_a_silent_success() {
    let router = ModelRouter::new(ModelRouterConfig::default());
    router
        .add_provider_instance("flaky", Arc::new(failing_provider()))
        .await
        .expect("register");
    router
        .model_catalog
        .register(ModelCatalogEntry::new("m1", "m1", "flaky"))
        .await;
    router
        .set_fallback_chain("m1", vec!["flaky".to_string()])
        .await
        .expect("chain");

    let err = router
        .complete_with_route("m1", vec![Message::user("hi")], None)
        .await
        .expect_err("an exhausted chain must surface an error");
    assert!(
        err.to_string().to_lowercase().contains("failed"),
        "the error must report the exhaustion: {err}"
    );
}
