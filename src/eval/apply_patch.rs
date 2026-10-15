//! Optimistic-concurrency apply for optimizer patches.
//!
//! `apply_optimizer_patch` mutates the live gateway config for one dot-path
//! using the exact same CAS machinery as the WS `config.set` handler:
//! `config_revision` → write-lock re-check → `apply_config_path` →
//! `persist_config_atomic` → push the update to the running default agent.
//! A conflicting revision aborts the patch with no mutation, so concurrent
//! manual `config.set` writes and optimizer runs never clobber each other.

use std::sync::Arc;

use serde_json::json;
use tracing::warn;

use crate::gateway::apply_config::apply_config_path;
use crate::gateway::handlers::config::persist_config_atomic;
use crate::gateway::ws::push_default_agent_update;
use crate::gateway::{config_revision, GatewayState};

/// A single scalar patch the optimizer wants to apply.
#[derive(Debug, Clone, PartialEq)]
pub struct OptimizerPatch {
    /// Dot-path into the gateway config (e.g. `default_agent.temperature`).
    pub path: String,
    /// Value to apply — must match the path's expected JSON type.
    pub value: serde_json::Value,
}

/// Result of attempting an optimistic apply.
#[derive(Debug, Clone, PartialEq)]
pub enum PatchOutcome {
    /// Applied and pushed; carries the new config revision.
    Applied { new_revision: String },
    /// The config changed since `base_revision` was read — nothing mutated.
    Conflict { current: String },
    /// The path is not one `apply_config_path` knows.
    UnknownPath,
}

/// CAS-apply a patch to the live gateway config and push the update to the
/// running default agent. No-op (returns [`PatchOutcome::Conflict`]) when the
/// config revision no longer matches `base_revision`.
pub async fn apply_optimizer_patch(
    state: &Arc<GatewayState>,
    patch: &OptimizerPatch,
    base_revision: &str,
) -> PatchOutcome {
    // Cheap read-side CAS fast-fail before taking the write lock.
    {
        let cfg = state.config.read().await;
        let current = config_revision(&cfg);
        if current != base_revision {
            return PatchOutcome::Conflict { current };
        }
    }

    let mut guard = state.config.write().await;
    let current = config_revision(&guard);
    if current != base_revision {
        return PatchOutcome::Conflict { current };
    }

    let config = Arc::make_mut(&mut guard);
    if !apply_config_path(config, &patch.path, &patch.value) {
        return PatchOutcome::UnknownPath;
    }

    // Persist while still holding the write lock so a concurrent writer cannot
    // overwrite our update before it is serialized (mirrors `config.set`).
    if let Some(config_path) = state.config_path.clone() {
        if let Err(e) = persist_config_atomic(config, &config_path, None).await {
            warn!("Optimizer applied {} but failed to persist config: {}", patch.path, e);
        }
    }
    let new_revision = config_revision(config);
    drop(guard);

    push_default_agent_update(state).await;
    PatchOutcome::Applied { new_revision }
}

/// JSON snapshot of a successful patch, for decision-trace evidence.
pub fn applied_evidence(
    run_id: &str,
    from: f64,
    to: f64,
    base_revision: &str,
    new_revision: &str,
) -> serde_json::Value {
    json!({
        "run_id": run_id,
        "from": from,
        "to": to,
        "base_revision": base_revision,
        "new_revision": new_revision,
    })
}

/// JSON snapshot of a rejected patch (CAS conflict), for decision-trace
/// evidence.
pub fn conflict_evidence(run_id: &str, current_revision: &str) -> serde_json::Value {
    json!({
        "run_id": run_id,
        "reason": "revision_conflict",
        "current_revision": current_revision,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gateway::state_tests::make_test_state;
    use crate::gateway::GatewayConfig;

    #[tokio::test]
    async fn applies_and_changes_revision() {
        let state = Arc::new(make_test_state(GatewayConfig::default()).await);
        let base = {
            let cfg = state.config.read().await;
            config_revision(&cfg)
        };
        let patch = OptimizerPatch {
            path: "default_agent.temperature".to_string(),
            value: json!(0.9),
        };
        let outcome = apply_optimizer_patch(&state, &patch, &base).await;
        match outcome {
            PatchOutcome::Applied { new_revision } => {
                assert_ne!(new_revision, base, "revision must change after apply");
            }
            other => panic!("expected Applied, got {:?}", other),
        }
        let cfg = state.config.read().await;
        assert!((cfg.default_agent.temperature - 0.9).abs() < 1e-6);
    }

    #[tokio::test]
    async fn stale_revision_conflicts_without_mutation() {
        let state = Arc::new(make_test_state(GatewayConfig::default()).await);
        // A stale base (the config is still at its default revision).
        let patch = OptimizerPatch {
            path: "default_agent.max_tokens".to_string(),
            value: json!(4096),
        };
        let outcome = apply_optimizer_patch(&state, &patch, "stale-revision").await;
        assert!(matches!(outcome, PatchOutcome::Conflict { .. }));
        let cfg = state.config.read().await;
        assert_eq!(cfg.default_agent.max_tokens, 2048, "must not mutate on conflict");
    }

    #[tokio::test]
    async fn unknown_path_rejected() {
        let state = Arc::new(make_test_state(GatewayConfig::default()).await);
        let base = {
            let cfg = state.config.read().await;
            config_revision(&cfg)
        };
        let patch = OptimizerPatch {
            path: "no.such.path".to_string(),
            value: json!(1),
        };
        assert_eq!(apply_optimizer_patch(&state, &patch, &base).await, PatchOutcome::UnknownPath);
    }
}
