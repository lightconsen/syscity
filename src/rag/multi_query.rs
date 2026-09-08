//! Multi-Query retrieval augmentation.
//!
//! Expands a user query into multiple sub-queries using an LLM, runs them in
//! parallel, then merges the results with Reciprocal Rank Fusion (RRF).
//!
//! # Architecture
//!
//! ```text
//! User query → LLM expansion → N sub-queries
//!                                  │
//!          ┌───────────────────────┼───────────────────────┐
//!      search(var₁)           search(var₂)           search(var₃)
//!          │                       │                       │
//!          └───────────────────────┼───────────────────────┘
//!                             RRF merge
//!                                 │
//!                          final results
//! ```
//!
//! Multi-Query is orthogonal to HyDE: when both are enabled, HyDE runs
//! *inside* each sub-query's search path (via the existing `QueryTransformer`
//! on `VectorMemoryService`).

use std::collections::HashMap;

use crate::providers::{CompletionRequest, Message, Provider};

/// Multi-Query configuration.
#[derive(Debug, Clone)]
pub struct MultiQueryConfig {
    /// Enable Multi-Query expansion.
    pub enabled: bool,
    /// Number of LLM-generated sub-queries (not counting the original query).
    /// Total searches = `num_variations + 1`.
    pub num_variations: usize,
    /// Result merging strategy.
    pub merge_strategy: MergeStrategy,
}

impl Default for MultiQueryConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            num_variations: 3,
            merge_strategy: MergeStrategy::Rrf(RrfConfig { k: 60 }),
        }
    }
}

/// Reciprocal Rank Fusion configuration.
#[derive(Debug, Clone, Copy)]
pub struct RrfConfig {
    /// RRF constant k. Higher values give more weight to lower-ranked results.
    /// Default: 60.
    pub k: usize,
}

impl Default for RrfConfig {
    fn default() -> Self {
        Self { k: 60 }
    }
}

/// Result merging strategy for Multi-Query.
#[derive(Debug, Clone)]
pub enum MergeStrategy {
    /// Reciprocal Rank Fusion — position-based merging.
    Rrf(RrfConfig),
}

/// LLM prompt for expanding a query into multiple variations.
const EXPAND_PROMPT: &str = "\
You are a query expansion assistant. Given the user's original search query, \
generate {num} different versions that cover different aspects or phrasings.

Rules:
- Each version must be a complete, self-contained search query
- Cover different perspectives or terminology
- Do NOT number the queries — output one per line, no prefixes
- Keep each query concise (under 20 words)

Original query: {query}

Alternative queries:";

/// Expand a query into multiple sub-query variations using an LLM.
///
/// Returns `num_variations + 1` queries: the original query followed by LLM-
/// generated variations.  The original query is always first so its results
/// get the highest rank contribution in RRF.
pub async fn expand_query_with_llm(
    query: &str,
    num_variations: usize,
    provider: &dyn Provider,
    model: Option<&str>,
) -> crate::Result<Vec<String>> {
    if num_variations == 0 {
        return Ok(vec![query.to_string()]);
    }

    let prompt = EXPAND_PROMPT
        .replace("{num}", &num_variations.to_string())
        .replace("{query}", query);

    let request = CompletionRequest {
        messages: vec![
            Message::system(
                "You are a query expansion assistant. Generate alternative search \
                 queries that cover different aspects of the user's question.",
            ),
            Message::user(prompt),
        ],
        // `None` falls back to the provider instance's default model — make
        // sure that default is the configured one (see `create_provider`).
        model: model.map(String::from),
        temperature: Some(0.7),
        max_tokens: Some(512),
        stream: false,
        ..Default::default()
    };

    let response = provider.complete(request).await?;
    let mut queries = vec![query.to_string()];

    for line in response.message.content.lines() {
        let trimmed = line.trim();
        if !trimmed.is_empty() && queries.len() < num_variations + 1 {
            queries.push(trimmed.to_string());
        }
    }

    // Pad with copies of the original query if the LLM returned too few
    while queries.len() < num_variations + 1 {
        queries.push(query.to_string());
    }

    Ok(queries)
}

/// Merge multiple ranked result sets using Reciprocal Rank Fusion (RRF).
///
/// RRF score = Σ 1 / (k + rank_i(d))
///
/// where `rank_i(d)` is the 1-based position of document `d` in result set `i`.
/// Documents not present in a result set contribute 0.
///
/// The merged results are sorted by RRF score descending. Items are
/// deduplicated by identity (using `Eq` + `Hash` on the item type).
pub fn merge_results<T, IdFn>(
    result_sets: Vec<Vec<(T, f32)>>,
    config: &RrfConfig,
    max_results: usize,
    id_fn: IdFn,
) -> Vec<(T, f32)>
where
    T: Clone,
    IdFn: Fn(&T) -> String,
{
    if result_sets.is_empty() {
        return Vec::new();
    }

    if result_sets.len() == 1 {
        // Single result set — return as-is (up to max_results)
        let mut sets = result_sets;
        return sets.swap_remove(0).into_iter().take(max_results).collect();
    }

    let k = config.k as f64;
    let mut scores: HashMap<String, (T, f64)> = HashMap::new();
    let mut insertion_order: Vec<String> = Vec::new();

    for results in result_sets.iter() {
        for (rank, (item, _score)) in results.iter().enumerate() {
            let id = id_fn(item);
            let rrf_contribution = 1.0 / (k + (rank + 1) as f64);

            let entry = scores.get_mut(&id);
            if let Some((_, existing_score)) = entry {
                *existing_score += rrf_contribution;
            } else {
                scores.insert(id.clone(), (item.clone(), rrf_contribution));
                insertion_order.push(id.clone());
            }
        }
    }

    // Sort by RRF score descending, then by insertion order for stability
    let mut sorted: Vec<(T, f64)> = insertion_order
        .into_iter()
        .filter_map(|id| {
            let (item, score) = scores.remove(&id)?;
            Some((item, score))
        })
        .collect();

    sorted.sort_by(|(_, a), (_, b)| b.partial_cmp(a).unwrap_or(std::cmp::Ordering::Equal));

    // Convert f64 scores back to f32 and limit
    sorted
        .into_iter()
        .take(max_results)
        .map(|(item, score)| (item, score as f32))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::providers::mock::MockProvider;
    use std::sync::Arc;

    // ── RRF merge tests ──────────────────────────────────────────────────────

    #[test]
    fn test_rrf_merge_empty_sets() {
        let config = RrfConfig { k: 60 };
        let result = merge_results::<String, _>(vec![], &config, 10, |s| s.clone());
        assert!(result.is_empty());
    }

    #[test]
    fn test_rrf_merge_single_set() {
        let config = RrfConfig { k: 60 };
        let results = vec![vec![("doc1".to_string(), 0.9), ("doc2".to_string(), 0.8)]];
        let merged = merge_results(results, &config, 10, |s| s.clone());
        assert_eq!(merged.len(), 2);
        assert_eq!(merged[0].0, "doc1");
        assert_eq!(merged[1].0, "doc2");
    }

    #[test]
    fn test_rrf_merge_two_sets() {
        let config = RrfConfig { k: 60 };

        // Set A: doc1 ranks 1, doc2 ranks 2
        let set_a = vec![
            ("doc1".to_string(), 0.9),
            ("doc2".to_string(), 0.8),
            ("doc3".to_string(), 0.7),
        ];

        // Set B: doc3 ranks 1, doc1 ranks 2
        let set_b = vec![
            ("doc3".to_string(), 0.9),
            ("doc1".to_string(), 0.8),
            ("doc4".to_string(), 0.7),
        ];

        let merged = merge_results(vec![set_a, set_b], &config, 10, |s| s.clone());

        // doc1 appears in both sets (rank 1 + rank 2) → highest RRF score
        // doc3 appears in both sets (rank 3 + rank 1) → second highest
        // doc2 appears only in set A → lower
        // doc4 appears only in set B → lower
        assert_eq!(merged.len(), 4);
        assert_eq!(merged[0].0, "doc1");
        assert_eq!(merged[1].0, "doc3");
    }

    #[test]
    fn test_rrf_merge_respects_max_results() {
        let config = RrfConfig { k: 60 };
        let set_a = vec![
            ("doc1".to_string(), 0.9),
            ("doc2".to_string(), 0.8),
            ("doc3".to_string(), 0.7),
        ];
        let merged = merge_results(vec![set_a], &config, 2, |s| s.clone());
        assert_eq!(merged.len(), 2);
    }

    #[test]
    fn test_rrf_k_value_effect() {
        // With k=0, only top-ranked results matter
        let config_small = RrfConfig { k: 1 };
        let set_a = vec![("doc1".to_string(), 0.9), ("doc2".to_string(), 0.8)];
        let set_b = vec![("doc2".to_string(), 0.9), ("doc1".to_string(), 0.8)];
        let merged = merge_results(vec![set_a, set_b], &config_small, 10, |s| s.clone());
        // With small k, ranking contribution is sharper
        assert_eq!(merged.len(), 2);
        // Both appear in each other's sets, so order depends on scores
        // Just verify both are present
        let ids: Vec<&str> = merged.iter().map(|(s, _)| s.as_str()).collect();
        assert!(ids.contains(&"doc1"));
        assert!(ids.contains(&"doc2"));
    }

    #[test]
    fn test_rrf_merge_deduplication_across_sets() {
        let config = RrfConfig { k: 60 };
        // Same document appears in two different result sets
        let set_a = vec![("doc1".to_string(), 0.9)];
        let set_b = vec![("doc1".to_string(), 0.8)];
        let merged = merge_results(vec![set_a, set_b], &config, 10, |s| s.clone());
        assert_eq!(merged.len(), 1, "duplicates across sets should be merged");
    }

    // ── expand_query_with_llm tests ──────────────────────────────────────────

    #[tokio::test]
    async fn test_expand_zero_variations() {
        let mock = Arc::new(MockProvider::new().with_callback(|_| Message::assistant("unused")));
        let queries = expand_query_with_llm("test query", 0, mock.as_ref(), None)
            .await
            .unwrap();
        assert_eq!(queries.len(), 1);
        assert_eq!(queries[0], "test query");
    }

    #[tokio::test]
    async fn test_expand_with_variations() {
        let mock = Arc::new(MockProvider::new().with_callback(|_| {
            Message::assistant("deployment steps\nmonitoring setup\nalert rules")
        }));
        let queries = expand_query_with_llm("how to deploy", 3, mock.as_ref(), None)
            .await
            .unwrap();
        assert_eq!(queries.len(), 4);
        assert_eq!(queries[0], "how to deploy");
        assert_eq!(queries[1], "deployment steps");
        assert_eq!(queries[2], "monitoring setup");
        assert_eq!(queries[3], "alert rules");
    }

    #[tokio::test]
    async fn test_expand_pads_on_insufficient_output() {
        let mock = Arc::new(MockProvider::new().with_callback(|_| Message::assistant("only one")));
        let queries = expand_query_with_llm("test", 3, mock.as_ref(), None)
            .await
            .unwrap();
        assert_eq!(queries.len(), 4);
        // Should pad with copies of the original
        assert_eq!(queries[0], "test");
        assert_eq!(queries[1], "only one");
        assert_eq!(queries[2], "test");
        assert_eq!(queries[3], "test");
    }

    #[tokio::test]
    async fn test_expand_handles_empty_lines() {
        let mock = Arc::new(
            MockProvider::new()
                .with_callback(|_| Message::assistant("\n\nvariant a\n\nvariant b\n\n")),
        );
        let queries = expand_query_with_llm("test", 2, mock.as_ref(), None)
            .await
            .unwrap();
        assert_eq!(queries.len(), 3);
        assert_eq!(queries[0], "test");
        assert_eq!(queries[1], "variant a");
        assert_eq!(queries[2], "variant b");
    }
}
