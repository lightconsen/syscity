//! Per-model credit multipliers, fetched from cloud `GET /v1/models`.
//!
//! The cloud proxy bills LLM calls as `max(1, ceil(tokens / 1000 ×
//! credit_multiplier))`; the multiplier is server-configured per model and
//! can change with a cloud deploy. The engine caches the mapping with a TTL
//! so the model picker can annotate cloud models (e.g. "0.8x") without a
//! round trip per render, and without hardcoding anything client-side.

use std::collections::HashMap;
use std::sync::{Arc, LazyLock};
use std::time::{Duration, Instant};

use tokio::sync::RwLock;

use crate::cloud::client::CloudClient;
use crate::cloud::config::CloudConfig;
use crate::cloud::session::get_token;

/// How long a fetched multiplier table stays fresh. Server-side changes go
/// live when this expires, no engine restart needed.
const TTL: Duration = Duration::from_secs(10 * 60);

/// `model_id → credit_multiplier`. Shared clone of the cached table.
type Multipliers = Arc<HashMap<String, f64>>;

static CACHE: LazyLock<RwLock<Option<(Instant, Multipliers)>>> =
    LazyLock::new(|| RwLock::new(None));

/// The cached multiplier table, refreshed from cloud `/v1/models` when the
/// TTL has expired. `None` when there is no data (not logged in, or the
/// first fetch failed) — callers just omit the annotation.
///
/// Failures never error out: a stale table keeps serving past its TTL so a
/// transient cloud outage doesn't blank the UI.
pub async fn credit_multipliers(cfg: &CloudConfig) -> Option<Multipliers> {
    {
        let cache = CACHE.read().await;
        if let Some((fetched_at, table)) = &*cache {
            if fetched_at.elapsed() < TTL {
                return Some(table.clone());
            }
        }
    }

    let fetched = match get_token().await {
        Some(token) => {
            let client = CloudClient::new(cfg, token);
            match client.models().await {
                Ok(v) => Some(multipliers_from_models_json(&v)),
                Err(e) => {
                    tracing::warn!("credit multiplier refresh failed, keeping cache: {e}");
                    None
                }
            }
        }
        None => None,
    };

    let mut cache = CACHE.write().await;
    if let Some(table) = fetched {
        let stale = cache
            .as_ref()
            .is_none_or(|(fetched_at, _)| fetched_at.elapsed() >= TTL);
        if stale {
            *cache = Some((Instant::now(), Arc::new(table)));
        }
    }
    cache.as_ref().map(|(_, table)| table.clone())
}

/// Extract `model_id → credit_multiplier` from an OpenAI-compatible
/// `/v1/models` response. Entries without the field are omitted (billed at
/// the implicit base rate of 1).
pub fn multipliers_from_models_json(value: &serde_json::Value) -> HashMap<String, f64> {
    let mut map = HashMap::new();
    if let Some(entries) = value.get("data").and_then(|d| d.as_array()) {
        for entry in entries {
            let Some(id) = entry.get("id").and_then(|i| i.as_str()) else {
                continue;
            };
            if let Some(mult) = entry.get("credit_multiplier").and_then(|m| m.as_f64()) {
                map.insert(id.to_string(), mult);
            }
        }
    }
    map
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn multipliers_parsed_from_data_array() {
        let v = json!({
            "object": "list",
            "data": [
                { "id": "deepseek-v4-flash", "object": "model", "credit_multiplier": 0.8 },
                { "id": "deepseek-v4-pro", "object": "model", "credit_multiplier": 1 },
                { "id": "deepseek-v4-flash-vision-exp", "object": "model", "credit_multiplier": 1.2 },
                { "id": "no-multiplier", "object": "model" },
                { "object": "model" }
            ]
        });
        let map = multipliers_from_models_json(&v);
        assert_eq!(map.get("deepseek-v4-flash"), Some(&0.8));
        assert_eq!(map.get("deepseek-v4-pro"), Some(&1.0));
        assert_eq!(map.get("deepseek-v4-flash-vision-exp"), Some(&1.2));
        assert!(!map.contains_key("no-multiplier"));
        assert_eq!(map.len(), 3);
    }

    #[test]
    fn multipliers_empty_for_unexpected_shape() {
        assert!(multipliers_from_models_json(&json!({})).is_empty());
        assert!(multipliers_from_models_json(&json!({ "data": "nope" })).is_empty());
    }
}
