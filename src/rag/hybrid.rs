//! Hybrid search algorithms: result fusion, MMR, temporal decay.
//!
//! This module provides the algorithmic building blocks for hybrid search:
//!
//! - [`fuse_and_rerank`] — normalise, deduplicate, and merge vector + FTS results
//! - [`mmr_rerank`] — Maximal Marginal Relevance for diversity
//! - [`apply_temporal_decay`] — exponential recency weighting
//!
//! These functions are **domain-agnostic**: they operate on [`ScoredResult`]
//! values, so any knowledge base (conversation memory, code docs, task
//! instructions, etc.) can use them without pulling in memory-specific types.

use std::collections::HashMap;

use chrono::{DateTime, NaiveDate, Utc};
use sha2::{Digest, Sha256};

// ── Public types
// ──────────────────────────────────────────────────────────────────────

/// A generic scored result from any search backend (vector or FTS).
///
/// Construct these from your backend's native result type, then pass them
/// to [`fuse_and_rerank`] for normalisation, deduplication and weighted
/// fusion.
#[derive(Debug, Clone)]
pub struct ScoredResult {
    /// Full text content of this result.
    pub content: String,
    /// Raw relevance score from the backend (higher = more relevant).
    pub score: f32,
    /// Optional backend-internal source id for cross-backend deduplication
    /// (e.g. a message id that is also the chunk's source_id in the vector
    /// store).
    pub source_id: Option<String>,
    /// Human-readable citation for this result (e.g. `"session:abc#L5"` or
    /// `"doc:rust-ownership.md"`).
    pub citation: String,
}

/// A single result from the hybrid search.
#[derive(Debug, Clone)]
pub struct HybridSearchResult {
    /// The full text content of the result.
    pub content: String,
    /// Combined hybrid score in [0, 1].
    pub score: f32,
    /// Which backend provided this result: `"vector"`, `"fts"`, or
    /// `"combined"`.
    pub source: String,
    /// Memory type to report for this result: `"semantic"`, `"session"`, or
    /// `"hybrid"`. Derived from `source` so downstream statistics can
    /// distinguish vector-only, FTS-only, and merged results.
    pub memory_type: String,
    /// Human-readable citation, e.g. `"session:abc123#L5-L12"`.
    pub citation: String,
}

/// Weights and thresholds for hybrid search.
#[derive(Debug, Clone)]
pub struct HybridSearchConfig {
    /// Weight applied to vector (semantic) scores. Default: 0.7.
    pub vector_weight: f32,
    /// Weight applied to FTS5 (BM25) scores. Default: 0.3.
    pub text_weight: f32,
    /// Maximum number of results to return. Default: 6.
    pub max_results: usize,
    /// Minimum combined score to include a result. Default: 0.35.
    pub min_score: f32,
    /// Temporal decay configuration for recency-aware scoring. Default:
    /// disabled.
    pub temporal_decay: TemporalDecayConfig,
    /// MMR configuration for diversity re-ranking. Default: lambda=0.7,
    /// top_k=5.
    pub mmr: MmrConfig,
}

impl Default for HybridSearchConfig {
    fn default() -> Self {
        Self {
            vector_weight: 0.7,
            text_weight: 0.3,
            max_results: 6,
            min_score: 0.35,
            temporal_decay: TemporalDecayConfig::default(),
            mmr: MmrConfig::default(),
        }
    }
}

/// Configuration for exponential temporal decay applied to dated memory files.
///
/// Decay formula: `score *= e^(-λ * age_days)` where `λ = ln(2) /
/// half_life_days`.
///
/// "Evergreen" files — those whose `citation` path does not contain a
/// parseable `YYYY-MM-DD` date — are exempt from decay and returned unchanged.
///
/// Disabled by default (`enabled: false`) for backward compatibility.
#[derive(Debug, Clone)]
pub struct TemporalDecayConfig {
    /// Whether temporal decay is applied. Default: `false`.
    pub enabled: bool,
    /// Exponential half-life in days. Default: 30.0.
    pub half_life_days: f32,
}

impl Default for TemporalDecayConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            half_life_days: 30.0,
        }
    }
}

/// Configuration for Maximal Marginal Relevance re-ranking.
///
/// MMR selects results that are both relevant to the query and diverse
/// relative to each other, reducing redundancy in search results.
///
/// Formula per step:
/// ```text
/// MMR(d) = λ * relevance(d, query) - (1 - λ) * max_{d' ∈ S} sim(d, d')
/// ```
/// where `S` is the set of already-selected results.
#[derive(Debug, Clone)]
pub struct MmrConfig {
    /// Trade-off between relevance and diversity.
    ///
    /// `1.0` = pure relevance ranking (no diversity benefit).
    /// `0.0` = maximum diversity, ignoring relevance.
    /// Default: `0.7`.
    pub lambda: f32,
    /// Maximum number of results to return after re-ranking. Default: 5.
    pub top_k: usize,
}

impl Default for MmrConfig {
    fn default() -> Self {
        Self { lambda: 0.7, top_k: 5 }
    }
}

// ── Internal accumulator
// ──────────────────────────────────────────────────────────────────────

#[derive(Default)]
struct Entry {
    vector_score: Option<f32>,
    fts_score: Option<f32>,
    content: String,
    citation: String,
    vector_source_id: Option<String>,
    fts_message_id: Option<String>,
}

// ── Public fusion function
// ──────────────────────────────────────────────────────────────────────

/// Normalise, deduplicate, and fuse two scored result sets (vector and FTS)
/// using the weighted combination formula from `config`.
///
/// Returns results sorted descending by combined score, **before** MMR
/// re-ranking and temporal decay are applied.  Call [`mmr_rerank`] and
/// [`apply_temporal_decay`] afterwards if desired.
///
/// # Example
///
/// ```rust
/// use syscity::rag::hybrid::{ScoredResult, HybridSearchConfig, fuse_and_rerank};
///
/// let vec_results = vec![
///     ScoredResult { content: "Rust ownership".into(), score: 0.9, source_id: None, citation: "doc:1".into() },
/// ];
/// let fts_results = vec![
///     ScoredResult { content: "Rust borrowing".into(), score: 0.8, source_id: None, citation: "doc:2".into() },
/// ];
/// let config = HybridSearchConfig { min_score: 0.0, ..HybridSearchConfig::default() };
/// let merged = fuse_and_rerank(vec_results, fts_results, &config);
/// assert!(!merged.is_empty());
/// ```
pub fn fuse_and_rerank(
    vector_results: Vec<ScoredResult>,
    fts_results: Vec<ScoredResult>,
    config: &HybridSearchConfig,
) -> Vec<HybridSearchResult> {
    // ── Collect raw scores ────────────────────────────────────────────────────
    let vector_pairs: Vec<(f32, String)> = vector_results
        .iter()
        .map(|r| (r.score, normalized_content_key(&r.content)))
        .collect();
    let fts_pairs: Vec<(f32, String)> = fts_results
        .iter()
        .map(|r| (r.score, normalized_content_key(&r.content)))
        .collect();

    // ── Normalise independently ───────────────────────────────────────────────
    let vector_norm = normalise(&vector_pairs);
    let fts_norm = normalise(&fts_pairs);

    // ── Accumulate entries keyed by content fingerprint ───────────────────────
    let mut entries: HashMap<String, Entry> = HashMap::new();

    for r in vector_results {
        let key = normalized_content_key(&r.content);
        let norm = *vector_norm.get(&key).unwrap_or(&0.0);
        let e = entries.entry(key.clone()).or_default();
        e.vector_score = Some(norm);
        if e.content.is_empty() {
            e.content = r.content.clone();
            e.citation = r.citation.clone();
            e.vector_source_id = r.source_id;
        }
    }

    for r in fts_results {
        let key = normalized_content_key(&r.content);
        let norm = *fts_norm.get(&key).unwrap_or(&0.0);
        let e = entries.entry(key.clone()).or_default();
        e.fts_score = Some(norm);
        if e.content.is_empty() {
            e.content = r.content.clone();
            e.citation = r.citation.clone();
            e.fts_message_id = r.source_id;
        }
    }

    // ── Secondary source-id deduplication ─────────────────────────────────────
    // If a vector chunk and an FTS result share the same source/message id,
    // merge the FTS entry into the vector entry even if their text differs.
    let mut source_key_to_content_key: HashMap<String, String> = HashMap::new();
    for (content_key, entry) in &entries {
        if let Some(ref src) = entry.vector_source_id {
            source_key_to_content_key.insert(format!("src:{}", src), content_key.clone());
        }
    }
    let mut merges: Vec<(String, String)> = Vec::new();
    for (content_key, entry) in &entries {
        if entry.vector_score.is_some() {
            continue;
        }
        if let Some(ref msg_id) = entry.fts_message_id {
            if let Some(vector_key) = source_key_to_content_key.get(&format!("src:{}", msg_id)) {
                if vector_key != content_key {
                    merges.push((content_key.clone(), vector_key.clone()));
                }
            }
        }
    }
    for (fts_key, vector_key) in merges {
        if let Some(fts_entry) = entries.remove(&fts_key) {
            if let Some(vector_entry) = entries.get_mut(&vector_key) {
                if vector_entry.fts_score.is_none() {
                    vector_entry.fts_score = fts_entry.fts_score;
                } else if let Some(fts_score) = fts_entry.fts_score {
                    if fts_score > vector_entry.fts_score.unwrap_or(0.0) {
                        vector_entry.fts_score = Some(fts_score);
                    }
                }
            }
        }
    }

    // ── Merge and filter ──────────────────────────────────────────────────────
    let mut merged: Vec<HybridSearchResult> = entries
        .into_values()
        .filter_map(|e| {
            let vs = e.vector_score.unwrap_or(0.0);
            let fs = e.fts_score.unwrap_or(0.0);
            let combined = config.vector_weight * vs + config.text_weight * (1.0 - fs);

            if combined < config.min_score || e.content.is_empty() {
                return None;
            }

            let source = match (e.vector_score.is_some(), e.fts_score.is_some()) {
                (true, true) => "combined",
                (true, false) => "vector",
                _ => "fts",
            };

            let memory_type = match source {
                "vector" => "semantic",
                "fts" => "session",
                _ => "hybrid",
            };

            Some(HybridSearchResult {
                content: e.content,
                score: combined,
                source: source.to_string(),
                memory_type: memory_type.to_string(),
                citation: e.citation,
            })
        })
        .collect();

    // Sort descending by score, then truncate.
    merged.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
    });

    // Apply temporal decay if enabled.
    if config.temporal_decay.enabled {
        apply_temporal_decay(&mut merged, &config.temporal_decay);
    }

    // Apply MMR re-ranking if configured.
    if config.mmr.lambda > 0.0 && config.mmr.top_k > 0 {
        merged = mmr_rerank(merged, &config.mmr);
    } else {
        merged.truncate(config.max_results);
    }

    merged
}

// ── Normalisation
// ─────────────────────────────────────────────────────────────────────

/// Normalise a slice of (score, key) pairs so that the maximum score maps to
/// 1.0. Returns a `HashMap<key, normalised_score>`.
fn normalise(pairs: &[(f32, String)]) -> HashMap<String, f32> {
    if pairs.is_empty() {
        return HashMap::new();
    }

    let min = pairs.iter().map(|(s, _)| *s).fold(f32::INFINITY, f32::min);
    let max = pairs
        .iter()
        .map(|(s, _)| *s)
        .fold(f32::NEG_INFINITY, f32::max);

    // Handle NaN or flat input.
    if max.is_nan() || min.is_nan() || max <= min {
        return pairs.iter().map(|(_, k)| (k.clone(), 0.0)).collect();
    }

    // Min-max normalization to [0, 1] — caller inverts for FTS5 (lower = better).
    let range = max - min;
    pairs
        .iter()
        .map(|(s, k)| (k.clone(), (s - min) / range))
        .collect()
}

/// SHA-256 fingerprint of `text` used for dedup.
///
/// The text is normalised (lowercase, whitespace collapsed) before hashing so
/// that semantically identical results that differ only in case or spacing are
/// merged. The original text is preserved in the returned result for display.
fn normalized_content_key(text: &str) -> String {
    let normalized = text
        .to_lowercase()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    let hash = Sha256::digest(normalized.as_bytes());
    hex::encode(hash)
}

// ── Temporal decay
// ────────────────────────────────────────────────────────────────────────

/// Apply exponential temporal decay to `results` in-place, then re-sort
/// descending by score.
///
/// Only results whose `citation` contains a `YYYY-MM-DD` date string (e.g.
/// `"vector:memory/2025-01-15.md"`) are decayed; all others are left
/// unchanged (evergreen).
///
/// This function is a no-op when `config.enabled == false`.
pub fn apply_temporal_decay(results: &mut [HybridSearchResult], config: &TemporalDecayConfig) {
    if !config.enabled {
        return;
    }

    let lambda = std::f32::consts::LN_2 / config.half_life_days;
    let now: DateTime<Utc> = Utc::now();

    for result in results.iter_mut() {
        if let Some(date) = parse_date_from_citation(&result.citation) {
            let Some(midnight) = date.and_hms_opt(0, 0, 0) else {
                continue;
            };
            let age_days = (now - midnight.and_utc()).num_days() as f32;
            let decay = (-lambda * age_days.max(0.0)).exp();
            result.score *= decay;
        }
        // Evergreen: no date found → no decay.
    }

    // Re-sort descending after decay has shifted scores.
    results.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
}

// ── MMR Re-ranking
// ────────────────────────────────────────────────────────────────────────

/// Re-rank `results` using Maximal Marginal Relevance.
///
/// Requires that `results` are already sorted by relevance score (descending).
/// Uses word-level Jaccard similarity as the inter-document similarity
/// measure, making it embedding-free and fast.
pub fn mmr_rerank(
    candidates: Vec<HybridSearchResult>,
    config: &MmrConfig,
) -> Vec<HybridSearchResult> {
    if candidates.is_empty() {
        return candidates;
    }

    let n = candidates.len();
    let mut selected: Vec<usize> = Vec::with_capacity(config.top_k);
    let mut remaining: Vec<usize> = (0..n).collect();

    while selected.len() < config.top_k && !remaining.is_empty() {
        let mut best_idx_in_remaining: usize = 0;
        let mut best_score = f32::NEG_INFINITY;

        for (rem_pos, &cand_idx) in remaining.iter().enumerate() {
            let relevance = candidates[cand_idx].score;

            // Max similarity to any already-selected document.
            let max_sim = selected
                .iter()
                .map(|&sel_idx| {
                    jaccard_similarity(&candidates[cand_idx].content, &candidates[sel_idx].content)
                })
                .fold(0.0_f32, f32::max);

            let mmr_score = config.lambda * relevance - (1.0 - config.lambda) * max_sim;

            if mmr_score > best_score {
                best_score = mmr_score;
                best_idx_in_remaining = rem_pos;
            }
        }

        let chosen = remaining.remove(best_idx_in_remaining);
        selected.push(chosen);
    }

    selected
        .into_iter()
        .map(|i| candidates[i].clone())
        .collect()
}

// ── Helpers
// ────────────────────────────────────────────────────────────────────────

/// Word-level Jaccard similarity: |A ∩ B| / |A ∪ B|.
///
/// Operates on the set of unique words (lowercased, split on whitespace).
fn jaccard_similarity(a: &str, b: &str) -> f32 {
    use std::collections::HashSet;

    let words_a: HashSet<&str> = a.split_whitespace().collect();
    let words_b: HashSet<&str> = b.split_whitespace().collect();

    let intersection = words_a.intersection(&words_b).count();
    let union = words_a.union(&words_b).count();

    if union == 0 {
        return 1.0; // Both empty → identical.
    }

    intersection as f32 / union as f32
}

/// Extract the first `YYYY-MM-DD` date from a citation string.
///
/// Returns `None` for evergreen files that carry no date.
fn parse_date_from_citation(citation: &str) -> Option<NaiveDate> {
    // Scan for a 10-char substring matching `YYYY-MM-DD`.
    // Use char_indices to safely handle multi-byte UTF-8 characters.
    let indices: Vec<usize> = citation.char_indices().map(|(i, _)| i).collect();
    if indices.len() < 10 {
        return None;
    }
    for i in 0..=indices.len().saturating_sub(10) {
        let start = indices[i];
        let end = indices[i + 9]; // 9 chars later = 10-char slice start..end+1
        if end + 1 > citation.len() {
            break;
        }
        let slice = &citation[start..=end];
        if let Ok(date) = NaiveDate::parse_from_str(slice, "%Y-%m-%d") {
            return Some(date);
        }
    }
    None
}

// ── Internal helpers used by tests
// ────────────────────────────────────────────────────────────────────────

#[cfg(test)]
pub(crate) fn make_result(content: &str, score: f32) -> HybridSearchResult {
    HybridSearchResult {
        content: content.to_string(),
        score,
        source: "vector".to_string(),
        memory_type: "semantic".to_string(),
        citation: format!("doc:{}", content),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── Normalise tests ──────────────────────────────────────────────────────

    #[test]
    fn test_normalise_basic() {
        let pairs = vec![
            (2.0_f32, "a".to_string()),
            (1.0_f32, "b".to_string()),
            (0.0_f32, "c".to_string()),
        ];
        let norm = normalise(&pairs);
        assert!((norm["a"] - 1.0).abs() < 1e-6);
        assert!((norm["b"] - 0.5).abs() < 1e-6);
        assert!((norm["c"]).abs() < 1e-6);
    }

    #[test]
    fn test_normalise_all_zero() {
        let pairs = vec![(0.0_f32, "x".to_string()), (0.0_f32, "y".to_string())];
        let norm = normalise(&pairs);
        assert_eq!(norm["x"], 0.0);
        assert_eq!(norm["y"], 0.0);
    }

    #[test]
    fn test_content_key_is_deterministic() {
        let k1 = normalized_content_key("hello world");
        let k2 = normalized_content_key("hello world");
        assert_eq!(k1, k2);
    }

    #[test]
    fn test_content_key_differs_for_different_text() {
        let k1 = normalized_content_key("hello");
        let k2 = normalized_content_key("world");
        assert_ne!(k1, k2);
    }

    #[test]
    fn test_content_key_no_512_truncation() {
        let prefix = "a".repeat(512);
        let a = format!("{}-alpha", prefix);
        let b = format!("{}-beta", prefix);
        assert_ne!(
            normalized_content_key(&a),
            normalized_content_key(&b),
            "two strings with identical 512-char prefixes must not share a content key"
        );
    }

    #[test]
    fn test_content_key_normalizes_case_and_whitespace() {
        let k1 = normalized_content_key("Hello   World");
        let k2 = normalized_content_key("hello world");
        assert_eq!(k1, k2, "case and whitespace differences should produce the same key");
    }

    // ── Config tests ─────────────────────────────────────────────────────────

    #[test]
    fn test_config_defaults() {
        let cfg = HybridSearchConfig::default();
        assert!((cfg.vector_weight + cfg.text_weight - 1.0).abs() < 1e-6);
        assert_eq!(cfg.max_results, 6);
        assert!(cfg.min_score > 0.0);
    }

    #[test]
    fn test_hybrid_search_config_includes_temporal_decay_and_mmr() {
        let cfg = HybridSearchConfig::default();
        assert!(!cfg.temporal_decay.enabled);
        assert!((cfg.temporal_decay.half_life_days - 30.0).abs() < 1e-6);
        assert!((cfg.mmr.lambda - 0.7).abs() < 1e-6);
        assert_eq!(cfg.mmr.top_k, 5);
    }

    #[test]
    fn test_hybrid_search_config_with_custom_temporal_decay() {
        let cfg = HybridSearchConfig {
            temporal_decay: TemporalDecayConfig {
                enabled: true,
                half_life_days: 7.0,
            },
            mmr: MmrConfig { lambda: 0.5, top_k: 3 },
            ..HybridSearchConfig::default()
        };

        assert!(cfg.temporal_decay.enabled);
        assert!((cfg.temporal_decay.half_life_days - 7.0).abs() < 1e-6);
        assert!((cfg.mmr.lambda - 0.5).abs() < 1e-6);
        assert_eq!(cfg.mmr.top_k, 3);
    }

    // ── Temporal decay tests ──────────────────────────────────────────────────

    #[test]
    fn test_temporal_decay_disabled_is_noop() {
        let config = TemporalDecayConfig {
            enabled: false,
            half_life_days: 30.0,
        };

        let mut results = vec![HybridSearchResult {
            content: "old content".to_string(),
            score: 0.8,
            source: "vector".to_string(),
            memory_type: "semantic".to_string(),
            citation: "vector:memory/2020-01-01.md".to_string(),
        }];

        apply_temporal_decay(&mut results, &config);
        assert!((results[0].score - 0.8).abs() < 1e-6, "Score should be unchanged when disabled");
    }

    #[test]
    fn test_temporal_decay_reduces_old_scores() {
        let config = TemporalDecayConfig {
            enabled: true,
            half_life_days: 30.0,
        };

        let mut results = vec![HybridSearchResult {
            content: "old content".to_string(),
            score: 1.0,
            source: "vector".to_string(),
            memory_type: "semantic".to_string(),
            citation: "vector:memory/2000-01-01.md".to_string(),
        }];

        apply_temporal_decay(&mut results, &config);
        assert!(results[0].score < 0.01, "Score for 25-year-old memory should approach 0");
    }

    #[test]
    fn test_temporal_decay_spares_evergreen_files() {
        let config = TemporalDecayConfig {
            enabled: true,
            half_life_days: 30.0,
        };

        let mut results = vec![HybridSearchResult {
            content: "evergreen content".to_string(),
            score: 0.9,
            source: "vector".to_string(),
            memory_type: "semantic".to_string(),
            citation: "vector:MEMORY.md".to_string(),
        }];

        apply_temporal_decay(&mut results, &config);
        assert!(
            (results[0].score - 0.9).abs() < 1e-6,
            "Evergreen file score should not be decayed"
        );
    }

    #[test]
    fn test_temporal_decay_sorts_descending() {
        let config = TemporalDecayConfig {
            enabled: true,
            half_life_days: 30.0,
        };

        let fresh_date = chrono::Utc::now().format("%Y-%m-%d").to_string();
        let mut results = vec![
            HybridSearchResult {
                content: "old".to_string(),
                score: 0.9,
                source: "fts".to_string(),
                memory_type: "session".to_string(),
                citation: format!("vector:memory/2000-01-01.md"),
            },
            HybridSearchResult {
                content: "fresh".to_string(),
                score: 0.7,
                source: "vector".to_string(),
                memory_type: "semantic".to_string(),
                citation: format!("vector:memory/{}.md", fresh_date),
            },
        ];

        apply_temporal_decay(&mut results, &config);

        assert_eq!(results[0].content, "fresh", "Fresh result should rank first after decay");
    }

    #[test]
    fn test_parse_date_from_citation_finds_date() {
        let date = parse_date_from_citation("vector:memory/2025-03-15.md");
        assert!(date.is_some());
        let d = date.unwrap();
        assert_eq!(d.to_string(), "2025-03-15");
    }

    #[test]
    fn test_parse_date_from_citation_returns_none_for_evergreen() {
        assert!(parse_date_from_citation("vector:MEMORY.md").is_none());
        assert!(parse_date_from_citation("session:abc123#5").is_none());
    }

    #[test]
    fn test_temporal_decay_config_defaults() {
        let cfg = TemporalDecayConfig::default();
        assert!(!cfg.enabled, "Decay should be disabled by default");
        assert!((cfg.half_life_days - 30.0).abs() < 1e-6);
    }

    #[test]
    fn test_apply_temporal_decay_preserves_score_order_for_same_age() {
        let config = TemporalDecayConfig {
            enabled: true,
            half_life_days: 30.0,
        };

        let today = chrono::Utc::now().format("%Y-%m-%d").to_string();
        let mut results = vec![
            HybridSearchResult {
                content: "first".to_string(),
                score: 0.9,
                source: "vector".to_string(),
                memory_type: "semantic".to_string(),
                citation: format!("vector:memory/{}.md", today),
            },
            HybridSearchResult {
                content: "second".to_string(),
                score: 0.7,
                source: "vector".to_string(),
                memory_type: "semantic".to_string(),
                citation: format!("vector:memory/{}.md", today),
            },
        ];

        apply_temporal_decay(&mut results, &config);

        assert_eq!(results[0].content, "first");
        assert_eq!(results[1].content, "second");
        assert!(results[0].score > results[1].score);
        assert!(results[0].score <= 0.9);
        assert!(results[1].score <= 0.7);
    }

    #[test]
    fn test_apply_temporal_decay_half_life_correctness() {
        let config = TemporalDecayConfig {
            enabled: true,
            half_life_days: 30.0,
        };

        let old_date = (chrono::Utc::now() - chrono::Duration::days(30))
            .format("%Y-%m-%d")
            .to_string();
        let mut results = vec![HybridSearchResult {
            content: "old".to_string(),
            score: 1.0,
            source: "vector".to_string(),
            memory_type: "semantic".to_string(),
            citation: format!("vector:memory/{}.md", old_date),
        }];

        apply_temporal_decay(&mut results, &config);

        assert!(
            results[0].score > 0.45 && results[0].score < 0.55,
            "Score after one half-life should be ~0.5, got {}",
            results[0].score
        );
    }

    #[test]
    fn test_apply_temporal_decay_multiple_half_lives() {
        let config = TemporalDecayConfig {
            enabled: true,
            half_life_days: 30.0,
        };

        let old_date = (chrono::Utc::now() - chrono::Duration::days(90))
            .format("%Y-%m-%d")
            .to_string();
        let mut results = vec![HybridSearchResult {
            content: "very old".to_string(),
            score: 1.0,
            source: "vector".to_string(),
            memory_type: "semantic".to_string(),
            citation: format!("vector:memory/{}.md", old_date),
        }];

        apply_temporal_decay(&mut results, &config);

        assert!(
            results[0].score > 0.10 && results[0].score < 0.15,
            "Score after 3 half-lives should be ~0.125, got {}",
            results[0].score
        );
    }

    // ── MMR tests ─────────────────────────────────────────────────────────

    #[test]
    fn test_mmr_empty_input() {
        let results = mmr_rerank(vec![], &MmrConfig::default());
        assert!(results.is_empty());
    }

    #[test]
    fn test_mmr_top_k_limits_output() {
        let results = vec![
            make_result("a b c", 0.9),
            make_result("d e f", 0.8),
            make_result("g h i", 0.7),
            make_result("j k l", 0.6),
        ];
        let cfg = MmrConfig { lambda: 0.7, top_k: 2 };
        let reranked = mmr_rerank(results, &cfg);
        assert_eq!(reranked.len(), 2);
    }

    #[test]
    fn test_mmr_promotes_diversity() {
        let results = vec![
            make_result("rust ownership borrow move copy", 0.9),
            make_result("rust ownership borrow move copy clone", 0.85),
            make_result("python async await coroutine", 0.7),
        ];
        let cfg = MmrConfig { lambda: 0.5, top_k: 2 };
        let reranked = mmr_rerank(results, &cfg);
        assert!(reranked[0]
            .content
            .starts_with("rust ownership borrow move copy"));
        assert_eq!(reranked.len(), 2);
        let has_diverse = reranked.iter().any(|r| r.content.starts_with("python"));
        assert!(has_diverse, "MMR should promote the diverse result over the near-duplicate");
    }

    #[test]
    fn test_mmr_pure_relevance_with_lambda_one() {
        let results = vec![
            make_result("aaa bbb ccc", 0.9),
            make_result("ddd eee fff", 0.8),
            make_result("ggg hhh iii", 0.7),
        ];
        let cfg = MmrConfig { lambda: 1.0, top_k: 3 };
        let reranked = mmr_rerank(results, &cfg);
        assert_eq!(reranked[0].score, 0.9);
        assert_eq!(reranked[1].score, 0.8);
        assert_eq!(reranked[2].score, 0.7);
    }

    #[test]
    fn test_jaccard_similarity_identical() {
        assert!((jaccard_similarity("hello world", "hello world") - 1.0).abs() < 1e-6);
    }

    #[test]
    fn test_jaccard_similarity_disjoint() {
        assert!((jaccard_similarity("hello world", "foo bar")).abs() < 1e-6);
    }

    #[test]
    fn test_jaccard_similarity_partial_overlap() {
        let sim = jaccard_similarity("a b c", "b c d");
        assert!((sim - 0.5).abs() < 1e-6);
    }

    #[test]
    fn test_mmr_config_defaults() {
        let cfg = MmrConfig::default();
        assert!((cfg.lambda - 0.7).abs() < 1e-6);
        assert_eq!(cfg.top_k, 5);
    }

    #[test]
    fn test_mmr_with_zero_lambda_pure_diversity() {
        let results = vec![
            make_result("rust programming language", 0.9),
            make_result("rust programming tutorial", 0.85),
            make_result("python scripting", 0.5),
        ];
        let cfg = MmrConfig { lambda: 0.0, top_k: 2 };
        let reranked = mmr_rerank(results, &cfg);

        assert_eq!(reranked.len(), 2);
        assert!(reranked[0].content.contains("rust"));
        let has_diverse = reranked.iter().any(|r| r.content.contains("python"));
        assert!(has_diverse, "Should pick diverse Python result with lambda=0");
    }

    // ── fuse_and_rerank tests ───────────────────────────────────────────────

    #[test]
    fn test_fuse_and_rerank_basic() {
        let cfg = HybridSearchConfig {
            min_score: 0.0,
            ..HybridSearchConfig::default()
        };
        let vec_results = vec![ScoredResult {
            content: "rust ownership".into(),
            score: 0.9,
            source_id: None,
            citation: "vec:1".into(),
        }];
        let fts_results = vec![ScoredResult {
            content: "rust borrowing".into(),
            score: 0.8,
            source_id: None,
            citation: "fts:1".into(),
        }];
        let merged = fuse_and_rerank(vec_results, fts_results, &cfg);
        assert!(!merged.is_empty());
    }

    #[test]
    fn test_fuse_and_rerank_empty_inputs() {
        let merged = fuse_and_rerank(vec![], vec![], &HybridSearchConfig::default());
        assert!(merged.is_empty());
    }

    #[test]
    fn test_fuse_and_rerank_dedup_by_content() {
        let cfg = HybridSearchConfig {
            min_score: 0.0,
            ..HybridSearchConfig::default()
        };
        let vec_results = vec![ScoredResult {
            content: "same content".into(),
            score: 0.9,
            source_id: None,
            citation: "vec:1".into(),
        }];
        let fts_results = vec![ScoredResult {
            content: "same content".into(),
            score: 0.8,
            source_id: None,
            citation: "fts:1".into(),
        }];
        let merged = fuse_and_rerank(vec_results, fts_results, &cfg);
        assert_eq!(merged.len(), 1, "identical content should be deduped");
        assert_eq!(merged[0].source, "combined");
    }

    // ── HybridSearchResult test ──────────────────────────────────────────────

    #[test]
    fn test_hybrid_search_result_memory_type() {
        let semantic = HybridSearchResult {
            content: "vector result".into(),
            score: 0.9,
            source: "vector".into(),
            memory_type: "semantic".into(),
            citation: "vector:abc".into(),
        };
        let session = HybridSearchResult {
            content: "fts result".into(),
            score: 0.8,
            source: "fts".into(),
            memory_type: "session".into(),
            citation: "session:abc#1".into(),
        };
        let hybrid = HybridSearchResult {
            content: "combined result".into(),
            score: 0.85,
            source: "combined".into(),
            memory_type: "hybrid".into(),
            citation: "hybrid:abc".into(),
        };

        assert_eq!(semantic.memory_type, "semantic");
        assert_eq!(session.memory_type, "session");
        assert_eq!(hybrid.memory_type, "hybrid");
    }
}
