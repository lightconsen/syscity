//! Hybrid search combining vector (cosine similarity) and FTS5 (BM25) results.
//!
//! Runs both searches concurrently, delegates normalisation, deduplication and
//! fusion to [`crate::rag::hybrid::fuse_and_rerank`], then applies temporal
//! decay and MMR re-ranking in a second pass.

use tracing::warn;

use super::{
    session_search::{SessionSearch, SessionSearchQuery},
    vector::VectorMemoryService,
};
use crate::rag::hybrid::{HybridSearchConfig, HybridSearchResult, ScoredResult};

/// Run hybrid search over `vector_service` (semantic) and `session_search`
/// (FTS5), merge results, and return up to `config.max_results` entries.
///
/// Both backends are queried concurrently via `tokio::join!`. The search is
/// scoped to the provided `user_id` and `conversation_id` so results from
/// other sessions are not injected into the current prompt.
pub async fn hybrid_search(
    query: &str,
    user_id: &str,
    conversation_id: &str,
    vector_service: &VectorMemoryService,
    session_search: &SessionSearch,
    config: &HybridSearchConfig,
) -> Vec<HybridSearchResult> {
    let sub_queries = vector_service.expand(query).await;
    hybrid_search_expanded(
        query,
        &sub_queries,
        user_id,
        conversation_id,
        vector_service,
        session_search,
        config,
    )
    .await
}

/// [`hybrid_search`] with the multi-query expansion already done.
///
/// A single turn searches twice — global memory here and a knowledge-base
/// collection in [`crate::memory::MemoryManager::retrieve`] — and each search
/// used to run its own LLM expansion (~2 s each). Expanding once and handing
/// the same sub-queries to both searches removes the duplicate call.
pub async fn hybrid_search_expanded(
    query: &str,
    sub_queries: &[String],
    user_id: &str,
    conversation_id: &str,
    vector_service: &VectorMemoryService,
    session_search: &SessionSearch,
    config: &HybridSearchConfig,
) -> Vec<HybridSearchResult> {
    let fetch_limit = config.max_results * 2;
    let threshold = 0.0; // we apply min_score ourselves after merging

    // ── Launch both searches concurrently ─────────────────────────────────────
    let fts_query = SessionSearchQuery::new(query)
        .for_user(user_id)
        .for_conversation(conversation_id)
        .limit(fetch_limit);

    let (vector_res, fts_res) = tokio::join!(
        vector_service.search_expanded(sub_queries, fetch_limit, threshold, None),
        session_search.search(fts_query),
    );

    let vector_chunks = match vector_res {
        Ok(v) => v,
        Err(e) => {
            warn!("Vector search failed in hybrid_search: {}", e);
            Vec::new()
        }
    };
    let fts_results = match fts_res {
        Ok(f) => f,
        Err(e) => {
            warn!("FTS search failed in hybrid_search: {}", e);
            Vec::new()
        }
    };

    // ── Convert to domain-agnostic ScoredResult ──────────────────────
    let vector_scored: Vec<ScoredResult> = vector_chunks
        .into_iter()
        .map(|(chunk, score)| ScoredResult {
            content: chunk.text,
            score,
            source_id: Some(chunk.source_id),
            citation: format!("vector:{}", chunk.id),
        })
        .collect();

    let fts_scored: Vec<ScoredResult> = fts_results
        .into_iter()
        .map(|r| ScoredResult {
            content: r.content,
            score: r.score as f32,
            source_id: Some(r.message_id.clone()),
            citation: format!("session:{}#{}", r.conversation_id, r.message_id),
        })
        .collect();

    // ── Delegate normalisation, dedup and fusion ──────────────────────────────
    let mut results = crate::rag::hybrid::fuse_and_rerank(vector_scored, fts_scored, config);

    // ── Temporal decay ────────────────────────────────────────────────────────
    if config.temporal_decay.enabled {
        crate::rag::hybrid::apply_temporal_decay(&mut results, &config.temporal_decay);
    }

    // ── MMR re-ranking ────────────────────────────────────────────────────────
    if config.mmr.lambda > 0.0 && config.mmr.top_k > 0 {
        results = crate::rag::hybrid::mmr_rerank(results, &config.mmr);
    } else {
        results.truncate(config.max_results);
    }

    // ── Cross-encoder reranking (if configured) ────────────────────────────────
    let reranked = vector_service
        .reranker()
        .rerank(query, results.clone())
        .await;
    match reranked {
        Ok(rr) if !rr.is_empty() || results.is_empty() => results = rr,
        Ok(_) => {} // empty rerank with non-empty input: keep original
        Err(e) => warn!("Reranker failed, keeping un-reranked results: {}", e),
    }

    results
}
