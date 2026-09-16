//! Knowledge Base ingestion system.
//!
//! Manages per-agent document collections with automated ingestion pipeline:
//! load → chunk → embed → store, with change detection via SHA-256 checksums
//! and ingestion tracking in SQLite.

mod loader;
mod tracker;
pub(crate) mod watch;

pub mod html_convert;

use std::path::Path;
use std::sync::Arc;
use std::time::Instant;

use serde::Serialize;
use sqlx::Pool;
use sqlx::Sqlite;
use tracing::{info, warn};

pub use loader::{
    compute_checksum, detect_mime, load_dir, load_file, load_kb_config, load_source,
    KnowledgeDocument, KnowledgeSource, SourceType,
};
pub use tracker::{
    delete_record as delete_tracker_record, get_collection_stats, get_record as get_tracker_record,
    list_collections, list_records, mark_stale, upsert_record, CollectionStats, CollectionSummary,
    IngestionRecord, IngestionStatus,
};

/// Create the ingestion log's table (idempotent). The unified-database schema
/// init calls this so the table exists before anything reads it.
pub use tracker::ensure_schema as ensure_tracker_schema;
pub use watch::{KbWatchEvent, KbWatcher};

use crate::rag::chunk::{EmbeddedChunk, TextChunker};
use crate::rag::embedding::EmbeddingProvider;
use crate::rag::vector_store::VectorStore;
use crate::rag::EmbeddingConfig;

/// Configuration for the Knowledge Base ingestion system.
///
/// Mirrors the relevant subset of `GatewayConfig::vector_memory` fields needed
/// to create embedding providers. The CLI maps from `GatewayConfig` to this
/// struct.
#[derive(Debug, Clone)]
pub struct KnowledgeBaseConfig {
    pub embedding_api_key: Option<String>,
    pub embedding_model: String,
    pub embedding_dimension: usize,
    pub api_base_url: Option<String>,
    pub local_model_path: Option<String>,
    pub provider_type: KnowledgeBaseProviderType,
    pub chunk_size: usize,
    pub chunk_overlap: usize,
}

/// Embedding provider type for KB usage.
#[derive(Debug, Clone, PartialEq)]
pub enum KnowledgeBaseProviderType {
    OpenAi,
    LocalGguf,
}

/// Report from a single `ingest_source` call.
#[derive(Debug, Clone, Serialize)]
pub struct IngestReport {
    pub collection: String,
    pub total_sources: usize,
    pub docs_found: usize,
    pub docs_indexed: usize,
    pub docs_skipped: usize,
    pub total_chunks: usize,
    pub errors: Vec<String>,
    pub duration: std::time::Duration,
}

/// Report from a delete operation.
#[derive(Debug, Clone, Serialize)]
pub struct DeleteReport {
    pub collection: String,
    pub doc_id: Option<String>,
    pub chunks_deleted: usize,
}

/// High-level Knowledge Base manager.
///
/// Wraps an `EmbeddingProvider`, `VectorStore`, and SQLite `Pool` to provide
/// the ingestion pipeline for per-agent document collections.
pub struct KnowledgeBaseManager {
    embedding_provider: Arc<dyn EmbeddingProvider>,
    vector_store: Arc<dyn VectorStore>,
    pool: Pool<Sqlite>,
    chunker: TextChunker,
}

impl KnowledgeBaseManager {
    /// Create a new `KnowledgeBaseManager` with injected dependencies.
    pub fn new(
        embedding_provider: Arc<dyn EmbeddingProvider>,
        vector_store: Arc<dyn VectorStore>,
        pool: Pool<Sqlite>,
        config: &EmbeddingConfig,
    ) -> Self {
        let chunker = TextChunker::new(config.chunk_size, config.chunk_overlap);
        Self {
            embedding_provider,
            vector_store,
            pool,
            chunker,
        }
    }

    /// Create a `KnowledgeBaseManager` from config, provider, and store.
    ///
    /// Creates the chunker from the embedding config.
    pub fn from_parts(
        embedding_provider: Arc<dyn EmbeddingProvider>,
        vector_store: Arc<dyn VectorStore>,
        pool: Pool<Sqlite>,
        chunk_size: usize,
        chunk_overlap: usize,
    ) -> Self {
        let chunker = TextChunker::new(chunk_size, chunk_overlap);
        Self {
            embedding_provider,
            vector_store,
            pool,
            chunker,
        }
    }

    /// Ingest documents from a knowledge source into a collection.
    ///
    /// Each document is checksummed and compared against the ingestion log.
    /// Unchanged documents are skipped. New or changed documents are chunked,
    /// embedded, and stored.
    ///
    /// If `source.collection` is set, it overrides the `collection` parameter.
    pub async fn ingest_source(
        &self,
        source: &KnowledgeSource,
        collection: &str,
        agent_dir: &Path,
        force: bool,
    ) -> IngestReport {
        let collection = source.collection.as_deref().unwrap_or(collection);
        let start = Instant::now();
        let mut report = IngestReport {
            collection: collection.to_string(),
            total_sources: 1,
            docs_found: 0,
            docs_indexed: 0,
            docs_skipped: 0,
            total_chunks: 0,
            errors: Vec::new(),
            duration: std::time::Duration::ZERO,
        };

        let docs = match load_source(source, agent_dir).await {
            Ok(docs) => docs,
            Err(e) => {
                report.errors.push(format!("Failed to load source: {}", e));
                report.duration = start.elapsed();
                return report;
            }
        };

        report.docs_found = docs.len();

        for doc in &docs {
            if !force {
                // Check if this document is already ingested with the same checksum
                match get_tracker_record(&self.pool, collection, &doc.doc_id).await {
                    Ok(Some(record)) if record.checksum == Some(doc.checksum.clone()) => {
                        report.docs_skipped += 1;
                        continue;
                    }
                    Ok(Some(_)) => {
                        // Checksum differs — content has changed, re-index
                        info!(
                            "Document '{}' in '{}' has changed, re-indexing",
                            doc.doc_id, collection
                        );
                    }
                    Ok(None) => { /* New document */ }
                    Err(e) => {
                        warn!("Failed to check tracker for '{}': {}", doc.doc_id, e);
                    }
                }
            }

            // Chunk the document
            let chunks = match self.chunker.chunk_async(&doc.content).await {
                Ok(c) => c,
                Err(e) => {
                    let err = format!("Failed to chunk '{}': {}", doc.doc_id, e);
                    warn!("{}", err);
                    report.errors.push(err);
                    // Record the failure
                    if let Err(log_err) = upsert_record(
                        &self.pool,
                        &IngestionRecord {
                            collection: collection.to_string(),
                            doc_id: doc.doc_id.clone(),
                            source_id: doc.source_id.clone(),
                            checksum: Some(doc.checksum.clone()),
                            mtime: doc.mtime,
                            chunk_count: 0,
                            status: IngestionStatus::Failed,
                            error: Some(e.to_string()),
                            indexed_at: String::new(),
                        },
                    )
                    .await
                    {
                        warn!("Failed to log ingestion failure: {}", log_err);
                    }
                    continue;
                }
            };

            // Embed chunks
            let embeddings = match self.embedding_provider.embed_batch(&chunks).await {
                Ok(e) => e,
                Err(e) => {
                    let err = format!("Failed to embed '{}': {}", doc.doc_id, e);
                    warn!("{}", err);
                    report.errors.push(err);
                    if let Err(log_err) = upsert_record(
                        &self.pool,
                        &IngestionRecord {
                            collection: collection.to_string(),
                            doc_id: doc.doc_id.clone(),
                            source_id: doc.source_id.clone(),
                            checksum: Some(doc.checksum.clone()),
                            mtime: doc.mtime,
                            chunk_count: 0,
                            status: IngestionStatus::Failed,
                            error: Some(e.to_string()),
                            indexed_at: String::new(),
                        },
                    )
                    .await
                    {
                        warn!("Failed to log embedding failure: {}", log_err);
                    }
                    continue;
                }
            };

            // Build EmbeddedChunks with collection metadata
            let total = chunks.len();
            let embedded_chunks: Vec<EmbeddedChunk> = chunks
                .into_iter()
                .zip(embeddings)
                .enumerate()
                .map(|(pos, (text, embedding))| EmbeddedChunk {
                    id: format!("{}-{}-{}", collection, doc.doc_id, pos),
                    source_id: doc.doc_id.clone(),
                    text,
                    embedding,
                    position: pos,
                    total_chunks: total,
                    collection: Some(collection.to_string()),
                    metadata: None,
                })
                .collect();

            // Store in vector store
            if let Err(e) = self.vector_store.store_chunks(embedded_chunks).await {
                let err = format!("Failed to store chunks for '{}': {}", doc.doc_id, e);
                warn!("{}", err);
                report.errors.push(err);
                if let Err(log_err) = upsert_record(
                    &self.pool,
                    &IngestionRecord {
                        collection: collection.to_string(),
                        doc_id: doc.doc_id.clone(),
                        source_id: doc.source_id.clone(),
                        checksum: Some(doc.checksum.clone()),
                        mtime: doc.mtime,
                        chunk_count: 0,
                        status: IngestionStatus::Failed,
                        error: Some(e.to_string()),
                        indexed_at: String::new(),
                    },
                )
                .await
                {
                    warn!("Failed to log storage failure: {}", log_err);
                }
                continue;
            }

            // Record success
            report.docs_indexed += 1;
            report.total_chunks += total;

            if let Err(e) = upsert_record(
                &self.pool,
                &IngestionRecord {
                    collection: collection.to_string(),
                    doc_id: doc.doc_id.clone(),
                    source_id: doc.source_id.clone(),
                    checksum: Some(doc.checksum.clone()),
                    mtime: doc.mtime,
                    chunk_count: total,
                    status: IngestionStatus::Indexed,
                    error: None,
                    indexed_at: String::new(),
                },
            )
            .await
            {
                warn!("Failed to log ingestion success: {}", e);
            }
        }

        report.duration = start.elapsed();
        info!(
            "Ingested {} docs into '{}': {} indexed, {} skipped, {} errors in {:?}",
            report.docs_found,
            collection,
            report.docs_indexed,
            report.docs_skipped,
            report.errors.len(),
            report.duration,
        );
        report
    }

    /// Ingest all sources defined in an agent's `kb.toml`.
    pub async fn ingest_agent(&self, agent_id: &str, force: bool) -> IngestReport {
        let agent_dir = crate::dirs::agent_dir(agent_id);
        let collection = format!("kb-{}", agent_id);

        let sources = match load_kb_config(&agent_dir) {
            Some(sources) => sources,
            None => {
                return IngestReport {
                    collection,
                    total_sources: 0,
                    docs_found: 0,
                    docs_indexed: 0,
                    docs_skipped: 0,
                    total_chunks: 0,
                    errors: vec!["No kb.toml found".to_string()],
                    duration: std::time::Duration::ZERO,
                };
            }
        };

        let mut combined = IngestReport {
            collection,
            total_sources: sources.len(),
            docs_found: 0,
            docs_indexed: 0,
            docs_skipped: 0,
            total_chunks: 0,
            errors: Vec::new(),
            duration: std::time::Duration::ZERO,
        };

        let overall_start = Instant::now();
        for source in &sources {
            let report = self
                .ingest_source(source, &combined.collection, &agent_dir, force)
                .await;
            combined.docs_found += report.docs_found;
            combined.docs_indexed += report.docs_indexed;
            combined.docs_skipped += report.docs_skipped;
            combined.total_chunks += report.total_chunks;
            combined.errors.extend(report.errors);
        }
        combined.duration = overall_start.elapsed();
        combined
    }

    /// List ingestion records, optionally filtered by collection and status.
    ///
    /// When `collection` is `None`, records from all collections are returned.
    pub async fn list(
        &self,
        collection: Option<&str>,
        status: Option<IngestionStatus>,
    ) -> crate::Result<Vec<IngestionRecord>> {
        list_records(&self.pool, collection, status).await
    }

    /// List all collections with summary stats.
    pub async fn list_collections(&self) -> crate::Result<Vec<CollectionSummary>> {
        list_collections(&self.pool).await
    }

    /// Delete all chunks in a collection and clear ingestion tracking.
    pub async fn delete(
        &self,
        collection: &str,
        doc_id: Option<&str>,
    ) -> crate::Result<DeleteReport> {
        if let Some(did) = doc_id {
            // Delete specific document — clean the vector store chunks scoped
            // to this collection (source_id maps to doc_id; the same stem can
            // exist in other per-agent collections and must survive).
            let deleted = self
                .vector_store
                .delete_by_source_in_collection(collection, did)
                .await?;
            delete_tracker_record(&self.pool, collection, did).await?;
            Ok(DeleteReport {
                collection: collection.to_string(),
                doc_id: Some(did.to_string()),
                chunks_deleted: deleted,
            })
        } else {
            // Delete entire collection
            let deleted = self.vector_store.delete_by_collection(collection).await?;
            // Clean up all tracker records for this collection
            let records = self.list(Some(collection), None).await?;
            for rec in &records {
                if let Err(e) = delete_tracker_record(&self.pool, collection, &rec.doc_id).await {
                    warn!("Failed to delete tracker record '{}': {}", rec.doc_id, e);
                }
            }
            Ok(DeleteReport {
                collection: collection.to_string(),
                doc_id: None,
                chunks_deleted: deleted,
            })
        }
    }

    /// Get collection statistics.
    pub async fn stats(&self, collection: &str) -> crate::Result<CollectionStats> {
        get_collection_stats(&self.pool, collection).await
    }

    /// Get the collection name for a given agent.
    pub fn collection_name(agent_id: &str) -> String {
        format!("kb-{}", agent_id)
    }
}
