//! The cloud model list and per-model credit multipliers, both fetched from
//! cloud `GET /v1/models` in one request.
//!
//! The cloud proxy advertises its own supported models at `/v1/models` and
//! bills LLM calls as `max(1, ceil(tokens / 1000 × credit_multiplier))`; the
//! multiplier is server-configured per model and can change with a cloud
//! deploy. The engine caches the parsed result with a TTL so the model picker
//! can (a) list the current cloud models and (b) annotate them (e.g. "0.8x")
//! without a round trip per render, and without hardcoding anything
//! client-side.

use std::collections::HashMap;
use std::sync::{Arc, LazyLock};
use std::time::{Duration, Instant};

use tokio::sync::RwLock;

use crate::cloud::client::CloudClient;
use crate::cloud::config::CloudConfig;
use crate::cloud::session::get_token;
use crate::secrets::SecretStoreHandle;

/// How long a fetched table stays fresh. Server-side changes go live when
/// this expires, no engine restart needed.
const TTL: Duration = Duration::from_secs(10 * 60);

/// `model_id → credit_multiplier`. Shared clone of the cached table.
pub type Multipliers = Arc<HashMap<String, f64>>;

/// The cloud `/v1/models` catalog: the advertised model ids (in server order,
/// deduped) plus their billing multipliers.
#[derive(Clone, Debug, Default)]
pub struct CloudModelTable {
    /// Advertised model ids, in the order the server returned them.
    pub ids: Vec<String>,
    /// `model_id → credit_multiplier`. Entries without the field are omitted
    /// (billed at the implicit base rate of 1).
    pub multipliers: Multipliers,
}

/// Cached catalog entry: the time it was fetched and the parsed table.
type CacheSlot = Option<(Instant, Arc<CloudModelTable>)>;

static CACHE: LazyLock<RwLock<CacheSlot>> = LazyLock::new(|| RwLock::new(None));

/// The cached cloud catalog as it stands right now, with no network I/O.
///
/// This exists because [`fetch_cloud_models`] blocks on a cloud round trip.
/// `models.list` is on the app's cold-start path — the UI renders nothing but a
/// spinner until it answers — while the cloud client's own bound is 10s
/// (`CloudClient::new`), larger than the 5s the web client waits. So a slow or
/// unreachable cloud used to hold the startup gate open, and the request timed
/// out client-side before the reply could arrive: the app sat on "Starting
/// syscity" with the gateway healthy the whole time.
///
/// Startup callers read this instead and refresh in the background.
pub async fn cached_cloud_models() -> Option<Arc<CloudModelTable>> {
    CACHE.read().await.as_ref().map(|(_, t)| t.clone())
}

/// Whether the cache holds nothing fresh enough to skip a refresh.
pub async fn cloud_catalog_is_stale() -> bool {
    CACHE
        .read()
        .await
        .as_ref()
        .is_none_or(|(fetched_at, _)| fetched_at.elapsed() >= TTL)
}

/// The cached cloud catalog, refreshed from cloud `/v1/models` when the TTL
/// has expired. `None` when there is no data (not logged in, or the first
/// fetch failed) — callers fall back to their static seed.
///
/// Failures never error out: a stale table keeps serving past its TTL so a
/// transient cloud outage doesn't blank the UI.
///
/// **Blocks on the network.** Do not call this from a path the UI is waiting
/// on at startup — use [`cached_cloud_models`] there and refresh off-path.
pub async fn fetch_cloud_models(
    cfg: &CloudConfig,
    secrets: &Arc<SecretStoreHandle>,
) -> Option<Arc<CloudModelTable>> {
    {
        let cache = CACHE.read().await;
        if let Some((fetched_at, table)) = &*cache {
            if fetched_at.elapsed() < TTL {
                return Some(table.clone());
            }
        }
    }

    let fetched = match get_token(secrets).await {
        Some(token) => {
            let client = CloudClient::new(cfg, token, secrets.clone());
            match client.models().await {
                Ok(v) => Some(parse_cloud_models_json(&v)),
                Err(e) => {
                    tracing::warn!("cloud model refresh failed, keeping cache: {e}");
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

/// The cached billing multiplier table only. Thin wrapper over
/// [`fetch_cloud_models`] for callers that don't need the model id list.
pub async fn credit_multipliers(
    cfg: &CloudConfig,
    secrets: &Arc<SecretStoreHandle>,
) -> Option<Multipliers> {
    fetch_cloud_models(cfg, secrets)
        .await
        .map(|t| t.multipliers.clone())
}

/// Parse the cloud catalog from an OpenAI-compatible `/v1/models` response:
/// the advertised ids (order-preserving, deduped) and their multipliers.
pub fn parse_cloud_models_json(value: &serde_json::Value) -> CloudModelTable {
    let mut ids = Vec::new();
    let mut seen = std::collections::HashSet::new();
    let mut multipliers = HashMap::new();
    if let Some(entries) = value.get("data").and_then(|d| d.as_array()) {
        for entry in entries {
            let Some(id) = entry.get("id").and_then(|i| i.as_str()) else {
                continue;
            };
            if !id.is_empty() && seen.insert(id.to_string()) {
                ids.push(id.to_string());
            }
            if let Some(mult) = entry.get("credit_multiplier").and_then(|m| m.as_f64()) {
                multipliers.insert(id.to_string(), mult);
            }
        }
    }
    CloudModelTable {
        ids,
        multipliers: Arc::new(multipliers),
    }
}

/// Extract `model_id → credit_multiplier` from an OpenAI-compatible
/// `/v1/models` response. Entries without the field are omitted (billed at
/// the implicit base rate of 1).
pub fn multipliers_from_models_json(value: &serde_json::Value) -> HashMap<String, f64> {
    parse_cloud_models_json(value).multipliers.as_ref().clone()
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

    #[test]
    fn catalog_collects_ids_in_order_and_deduped() {
        let v = json!({
            "object": "list",
            "data": [
                { "id": "deepseek-flash", "object": "model", "credit_multiplier": 1 },
                { "id": "deepseek-v4-pro", "object": "model", "credit_multiplier": 1.5 },
                { "id": "deepseek-flash", "object": "model" },
                { "id": "no-multiplier", "object": "model" },
                { "object": "model" },
                { "id": "", "object": "model" }
            ]
        });
        let table = parse_cloud_models_json(&v);
        // Order preserved, duplicates and empty/missing ids dropped.
        assert_eq!(table.ids, vec!["deepseek-flash", "deepseek-v4-pro", "no-multiplier"]);
        assert_eq!(table.multipliers.get("deepseek-v4-pro"), Some(&1.5));
        assert_eq!(table.multipliers.len(), 2);
    }

    #[test]
    fn catalog_empty_for_unexpected_shape() {
        for v in [json!({}), json!({ "data": "nope" }), json!([])] {
            let table = parse_cloud_models_json(&v);
            assert!(table.ids.is_empty());
            assert!(table.multipliers.is_empty());
        }
    }

    /// The startup path must answer from the cache and never wait on the
    /// network. A stale table is still served — that is the whole point: the
    /// app renders model setup from what it already knows and refreshes behind
    /// the scenes, instead of blocking the first frame on a WAN round trip.
    #[tokio::test]
    async fn cached_read_serves_stale_and_reports_staleness() {
        let model = |ids: &[&str]| {
            Arc::new(CloudModelTable {
                ids: ids.iter().map(|s| s.to_string()).collect(),
                multipliers: Arc::new(HashMap::new()),
            })
        };

        *CACHE.write().await = None;
        assert!(cached_cloud_models().await.is_none(), "empty cache holds nothing");
        assert!(cloud_catalog_is_stale().await, "empty cache wants a refresh");

        *CACHE.write().await = Some((Instant::now(), model(&["cloud/a"])));
        assert!(!cloud_catalog_is_stale().await, "fresh entry needs no refresh");
        let fresh = cached_cloud_models().await.expect("fresh entry served");
        assert_eq!(fresh.ids, vec!["cloud/a".to_string()]);

        // Expired by more than the TTL: still served, but flagged stale.
        *CACHE.write().await = Some((Instant::now() - TTL * 2, model(&["cloud/b"])));
        let stale = cached_cloud_models()
            .await
            .expect("stale entry still served");
        assert_eq!(stale.ids, vec!["cloud/b".to_string()]);
        assert!(cloud_catalog_is_stale().await, "expired entry wants a refresh");

        *CACHE.write().await = None;
    }
}

/// Read the catalog cache for invariant checks: `(fetched_at, table)` when a
/// slot is populated, `None` when it is empty. Exposes the internals rather
/// than duplicating them, so the check and the cache cannot drift.
pub async fn catalog_cache_snapshot() -> Option<(std::time::Instant, Arc<CloudModelTable>)> {
    CACHE.read().await.clone()
}
