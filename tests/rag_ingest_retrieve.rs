//! RAG ingest → retrieve journey.
//!
//! `src/rag/` had no integration test at all — no test in `tests/` even
//! imported `syscity::rag`. Ingestion, chunking, embedding and vector
//! retrieval are wired into the knowledge base and the memory service, so a
//! silent breakage there (a KB that ingests nothing) was invisible to CI.
//!
//! The embedder is a deterministic stand-in: `EmbeddingProvider` is a public
//! trait and both in-repo deterministic embedders are `#[cfg(test)]`-gated,
//! so an integration test brings its own (mirroring `HashProvider` in
//! `src/rag/eval.rs`). No network, no model download, no external service.

use std::sync::Arc;

use syscity::rag::{
    evaluate_retrieval, BatchEmbeddingProcessor, EmbeddingProvider, MemoryVectorStore,
    RetrievalSample, TextChunker, VectorStore,
};

const DIM: usize = 8;

/// Deterministic bag-of-bytes embedder: identical tokens map to identical
/// buckets, so a query retrieves the chunk it shares words with.
struct HashEmbedder;

fn embed(text: &str) -> Vec<f32> {
    let mut v = vec![0f32; DIM];
    for token in text.to_lowercase().split_whitespace() {
        let h: u32 = token
            .bytes()
            .fold(0u32, |a, b| a.wrapping_mul(31).wrapping_add(b as u32));
        v[(h as usize) % DIM] += 1.0;
    }
    // Normalize so cosine similarity is a plain dot product.
    let norm = v.iter().map(|x| x * x).sum::<f32>().sqrt();
    if norm > 0.0 {
        for x in &mut v {
            *x /= norm;
        }
    }
    v
}

#[async_trait::async_trait]
impl EmbeddingProvider for HashEmbedder {
    fn model_name(&self) -> &str {
        "hash-d8"
    }

    fn dimension(&self) -> usize {
        DIM
    }

    async fn embed_batch(&self, texts: &[String]) -> syscity::Result<Vec<Vec<f32>>> {
        Ok(texts.iter().map(|t| embed(t)).collect())
    }
}

#[tokio::test]
async fn ingests_documents_and_retrieves_the_relevant_one() {
    let store = MemoryVectorStore::new(DIM);
    let embedder: Arc<dyn EmbeddingProvider> = Arc::new(HashEmbedder);
    let processor = BatchEmbeddingProcessor::new(embedder.clone(), TextChunker::new(64, 8), 16);

    let chunks = processor
        .process_documents(
            vec![
                (
                    "doc-router".to_string(),
                    "syscity routes completions to a registered provider and falls back \
                     when that provider fails"
                        .to_string(),
                ),
                (
                    "doc-sandbox".to_string(),
                    "the shell tool runs inside a kernel write fence; seatbelt on macOS \
                     and landlock on linux"
                        .to_string(),
                ),
            ],
            &store,
        )
        .await
        .expect("chunk → embed → store must succeed");

    assert!(
        chunks.iter().any(|c| c.source_id == "doc-router"),
        "the router document must have been chunked and stored"
    );

    // Retrieve by a query that shares words with exactly one document.
    let query = embedder
        .embed("which provider does syscity route completions to")
        .await
        .expect("embed query");
    let hits = store
        .search_similar(&query, 5, 0.0, None)
        .await
        .expect("search");
    assert!(!hits.is_empty(), "a populated store must return hits");
    assert_eq!(
        hits[0].0.source_id,
        "doc-router",
        "the closest chunk must be the router document, got {:?}",
        hits.iter()
            .map(|(c, s)| (&c.source_id, s))
            .collect::<Vec<_>>()
    );
}

#[tokio::test]
async fn retrieval_metrics_report_the_ingested_document() {
    let store = MemoryVectorStore::new(DIM);
    let embedder: Arc<dyn EmbeddingProvider> = Arc::new(HashEmbedder);
    let processor = BatchEmbeddingProcessor::new(embedder.clone(), TextChunker::new(64, 8), 16);

    processor
        .process_documents(
            vec![(
                "doc-hooks".to_string(),
                "hooks can deny a tool call before it runs or block its result after".to_string(),
            )],
            &store,
        )
        .await
        .expect("ingest");

    let metrics = evaluate_retrieval(
        &[RetrievalSample {
            query: "what can hooks deny".to_string(),
            relevant_doc_ids: vec!["doc-hooks".to_string()],
            collection: None,
        }],
        &store,
        &*embedder,
        &[1, 5],
    )
    .await
    .expect("retrieval evaluation");

    assert_eq!(
        metrics.hit_rate_at_k[0].1, 1.0,
        "the ingested document must be found at k=1: {metrics:?}"
    );
}

#[tokio::test]
async fn an_empty_store_returns_no_hits() {
    let store = MemoryVectorStore::new(DIM);
    let embedder = HashEmbedder;
    let query = embedder.embed("anything at all").await.expect("embed");

    let hits = store
        .search_similar(&query, 5, 0.0, None)
        .await
        .expect("search on an empty store is not an error");
    assert!(hits.is_empty(), "an empty store must return no hits, got {hits:?}");
}
