//! PostgreSQL + pgvector vector store backend.

use async_trait::async_trait;
use sqlx::{PgPool, Row};
use tracing::info;

use crate::rag::chunk::EmbeddedChunk;
use crate::rag::vector_store::{VectorStore, VectorStoreStats};

/// PostgreSQL-backed vector store using the `pgvector` extension.
#[derive(Debug, Clone)]
pub struct PgVectorStore {
    pool: PgPool,
    dimension: usize,
    table: String,
}

impl PgVectorStore {
    /// Create a new pgvector-backed store.
    ///
    /// Enables the `vector` extension and creates the target table and index
    /// if they do not already exist.
    pub async fn new(
        pool: PgPool,
        dimension: usize,
        table: impl Into<String>,
    ) -> crate::Result<Self> {
        let table = table.into();
        let quoted = Self::quote_identifier(&table);

        sqlx::query("CREATE EXTENSION IF NOT EXISTS vector")
            .execute(&pool)
            .await
            .map_err(|e| crate::error::SyscityError::Storage {
                context: "Failed to create pgvector extension".to_string(),
                details: e.to_string(),
            })?;

        let create_sql = format!(
            "CREATE TABLE IF NOT EXISTS {} (
                id TEXT PRIMARY KEY,
                source_id TEXT NOT NULL,
                text TEXT NOT NULL,
                embedding vector({}) NOT NULL,
                position INTEGER NOT NULL,
                total_chunks INTEGER NOT NULL,
                collection TEXT,
                metadata TEXT
            )",
            quoted, dimension
        );
        sqlx::query(sqlx::AssertSqlSafe(create_sql.as_str()))
            .execute(&pool)
            .await
            .map_err(|e| crate::error::SyscityError::Storage {
                context: format!("Failed to create pgvector table {}", table),
                details: e.to_string(),
            })?;

        let safe_name = table.replace(|c: char| !c.is_alphanumeric() && c != '_', "_");
        let index_sql = format!(
            "CREATE INDEX IF NOT EXISTS idx_{}_source_id ON {}(source_id)",
            safe_name, quoted
        );
        sqlx::query(sqlx::AssertSqlSafe(index_sql.as_str()))
            .execute(&pool)
            .await
            .map_err(|e| crate::error::SyscityError::Storage {
                context: format!("Failed to create pgvector index for {}", table),
                details: e.to_string(),
            })?;

        info!("PgVectorStore initialized (table={}, dim={})", table, dimension);
        Ok(Self { pool, dimension, table })
    }

    fn quote_identifier(ident: &str) -> String {
        format!("\"{}\"", ident.replace('"', "\"\""))
    }
}

#[async_trait]
impl VectorStore for PgVectorStore {
    // Every statement below is built with `format!` because the table name is
    // configuration. The only interpolated value is the table, and it goes
    // through `quote_identifier` (which doubles any `"`), never a raw bind —
    // everything else is a bound parameter. That is the audit sqlx 0.9 asks for
    // when it refuses a non-`'static` string.
    async fn store_chunk(&self, chunk: EmbeddedChunk) -> crate::Result<()> {
        let embedding = pgvector::Vector::from(chunk.embedding.clone());
        let metadata_json = chunk
            .metadata
            .as_ref()
            .map(serde_json::to_string)
            .transpose()
            .map_err(|e| crate::error::SyscityError::Storage {
                context: "Failed to serialize metadata".to_string(),
                details: e.to_string(),
            })?;

        let sql = format!(
            "INSERT INTO {} (id, source_id, text, embedding, position, total_chunks, collection, \
             metadata)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8)
             ON CONFLICT (id) DO UPDATE SET
                 source_id = EXCLUDED.source_id,
                 text = EXCLUDED.text,
                 embedding = EXCLUDED.embedding,
                 position = EXCLUDED.position,
                 total_chunks = EXCLUDED.total_chunks,
                 collection = EXCLUDED.collection,
                 metadata = EXCLUDED.metadata",
            Self::quote_identifier(&self.table)
        );
        sqlx::query(sqlx::AssertSqlSafe(sql.as_str()))
            .bind(&chunk.id)
            .bind(&chunk.source_id)
            .bind(&chunk.text)
            .bind(embedding)
            .bind(chunk.position as i64)
            .bind(chunk.total_chunks as i64)
            .bind(&chunk.collection)
            .bind(metadata_json)
            .execute(&self.pool)
            .await
            .map_err(|e| crate::error::SyscityError::Storage {
                context: "Failed to store pgvector chunk".to_string(),
                details: e.to_string(),
            })?;
        Ok(())
    }

    async fn search_similar(
        &self,
        query_embedding: &[f32],
        limit: usize,
        threshold: f32,
        collection: Option<&str>,
    ) -> crate::Result<Vec<(EmbeddedChunk, f32)>> {
        let embedding = pgvector::Vector::from(query_embedding.to_vec());
        let max_distance = 1.0f64 - threshold as f64;

        let sql = format!(
            "SELECT id, source_id, text, embedding, position, total_chunks, collection, metadata,
                    embedding <=> $1 AS distance
             FROM {}
             WHERE embedding <=> $1 <= $2
               AND ($4::text IS NULL OR collection = $4)
             ORDER BY embedding <=> $1
             LIMIT $3",
            Self::quote_identifier(&self.table)
        );
        let rows = sqlx::query(sqlx::AssertSqlSafe(sql.as_str()))
            .bind(embedding)
            .bind(max_distance)
            .bind(limit as i64)
            .bind(collection)
            .fetch_all(&self.pool)
            .await
            .map_err(|e| crate::error::SyscityError::Storage {
                context: "Failed to search pgvector".to_string(),
                details: e.to_string(),
            })?;

        let mut results = Vec::new();
        for row in rows {
            let distance: f64 =
                row.try_get("distance")
                    .map_err(|e| crate::error::SyscityError::Storage {
                        context: "Failed to read pgvector distance".to_string(),
                        details: e.to_string(),
                    })?;
            let embedding_col: pgvector::Vector =
                row.try_get("embedding")
                    .map_err(|e| crate::error::SyscityError::Storage {
                        context: "Failed to read pgvector embedding".to_string(),
                        details: e.to_string(),
                    })?;
            let metadata: Option<String> =
                row.try_get("metadata")
                    .map_err(|e| crate::error::SyscityError::Storage {
                        context: "Failed to read pgvector metadata column".to_string(),
                        details: e.to_string(),
                    })?;
            let collection: Option<String> =
                row.try_get("collection")
                    .map_err(|e| crate::error::SyscityError::Storage {
                        context: "Failed to read pgvector collection column".to_string(),
                        details: e.to_string(),
                    })?;
            let chunk = EmbeddedChunk {
                id: row
                    .try_get("id")
                    .map_err(|e| crate::error::SyscityError::Storage {
                        context: "Failed to read pgvector id".to_string(),
                        details: e.to_string(),
                    })?,
                source_id: row.try_get("source_id").map_err(|e| {
                    crate::error::SyscityError::Storage {
                        context: "Failed to read pgvector source_id".to_string(),
                        details: e.to_string(),
                    }
                })?,
                text: row
                    .try_get("text")
                    .map_err(|e| crate::error::SyscityError::Storage {
                        context: "Failed to read pgvector text".to_string(),
                        details: e.to_string(),
                    })?,
                embedding: embedding_col.into(),
                position: row.try_get::<i64, _>("position").map_err(|e| {
                    crate::error::SyscityError::Storage {
                        context: "Failed to read pgvector position".to_string(),
                        details: e.to_string(),
                    }
                })? as usize,
                total_chunks: row.try_get::<i64, _>("total_chunks").map_err(|e| {
                    crate::error::SyscityError::Storage {
                        context: "Failed to read pgvector total_chunks".to_string(),
                        details: e.to_string(),
                    }
                })? as usize,
                collection,
                metadata: metadata
                    .map(|m| serde_json::from_str(&m))
                    .transpose()
                    .map_err(|e| crate::error::SyscityError::Storage {
                        context: "Failed to deserialize pgvector metadata".to_string(),
                        details: e.to_string(),
                    })?,
            };
            results.push((chunk, (1.0f64 - distance) as f32));
        }
        Ok(results)
    }

    async fn delete_by_source(&self, source_id: &str) -> crate::Result<usize> {
        let sql =
            format!("DELETE FROM {} WHERE source_id = $1", Self::quote_identifier(&self.table));
        let result = sqlx::query(sqlx::AssertSqlSafe(sql.as_str()))
            .bind(source_id)
            .execute(&self.pool)
            .await
            .map_err(|e| crate::error::SyscityError::Storage {
                context: "Failed to delete by source".to_string(),
                details: e.to_string(),
            })?;
        Ok(result.rows_affected() as usize)
    }

    async fn delete_by_source_in_collection(
        &self,
        collection: &str,
        source_id: &str,
    ) -> crate::Result<usize> {
        let sql = format!(
            "DELETE FROM {} WHERE collection = $1 AND source_id = $2",
            Self::quote_identifier(&self.table)
        );
        let result = sqlx::query(sqlx::AssertSqlSafe(sql.as_str()))
            .bind(collection)
            .bind(source_id)
            .execute(&self.pool)
            .await
            .map_err(|e| crate::error::SyscityError::Storage {
                context: "Failed to delete by source in collection".to_string(),
                details: e.to_string(),
            })?;
        Ok(result.rows_affected() as usize)
    }

    async fn delete_by_collection(&self, collection: &str) -> crate::Result<usize> {
        let sql =
            format!("DELETE FROM {} WHERE collection = $1", Self::quote_identifier(&self.table));
        let result = sqlx::query(sqlx::AssertSqlSafe(sql.as_str()))
            .bind(collection)
            .execute(&self.pool)
            .await
            .map_err(|e| crate::error::SyscityError::Storage {
                context: "Failed to delete by collection".to_string(),
                details: e.to_string(),
            })?;
        Ok(result.rows_affected() as usize)
    }

    async fn stats(&self) -> crate::Result<VectorStoreStats> {
        let total_sql = format!("SELECT COUNT(*) FROM {}", Self::quote_identifier(&self.table));
        let total_vectors: (i64,) = sqlx::query_as(sqlx::AssertSqlSafe(total_sql.as_str()))
            .fetch_one(&self.pool)
            .await
            .map_err(|e| crate::error::SyscityError::Storage {
                context: "Failed to count vectors".to_string(),
                details: e.to_string(),
            })?;

        let sources_sql = format!(
            "SELECT COUNT(DISTINCT source_id) FROM {}",
            Self::quote_identifier(&self.table)
        );
        let total_sources: (i64,) = sqlx::query_as(sqlx::AssertSqlSafe(sources_sql.as_str()))
            .fetch_one(&self.pool)
            .await
            .map_err(|e| crate::error::SyscityError::Storage {
                context: "Failed to count sources".to_string(),
                details: e.to_string(),
            })?;

        Ok(VectorStoreStats {
            total_vectors: total_vectors.0 as usize,
            total_sources: total_sources.0 as usize,
            dimension: self.dimension,
        })
    }

    async fn clear(&self) -> crate::Result<()> {
        let sql = format!("DELETE FROM {}", Self::quote_identifier(&self.table));
        sqlx::query(sqlx::AssertSqlSafe(sql.as_str()))
            .execute(&self.pool)
            .await
            .map_err(|e| crate::error::SyscityError::Storage {
                context: "Failed to clear pgvector table".to_string(),
                details: e.to_string(),
            })?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rag::chunk::EmbeddedChunk;

    async fn pg_pool() -> Option<PgPool> {
        let url = std::env::var("TEST_PGVECTOR_URL").unwrap_or_else(|_| {
            "postgres://postgres:postgres@localhost:5432/syscity_test".to_string()
        });
        match PgPool::connect(&url).await {
            Ok(pool) => Some(pool),
            Err(_) => None,
        }
    }

    #[tokio::test]
    async fn test_pgvector_store_store_and_search() -> crate::Result<()> {
        let Some(pool) = pg_pool().await else {
            return Ok(());
        };
        let store = PgVectorStore::new(pool, 3, "test_pgvector_chunks").await?;
        store.clear().await?;

        let chunk = EmbeddedChunk {
            id: "c1".to_string(),
            source_id: "doc1".to_string(),
            text: "hello world".to_string(),
            embedding: vec![1.0, 0.0, 0.0],
            position: 0,
            total_chunks: 1,
            collection: None,
            metadata: None,
        };
        store.store_chunk(chunk).await?;

        let results = store.search_similar(&[1.0, 0.0, 0.0], 5, 0.0, None).await?;
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].0.id, "c1");
        assert!((results[0].1 - 1.0).abs() < 0.001);

        let results = store.search_similar(&[0.0, 1.0, 0.0], 5, 0.5, None).await?;
        assert!(results.is_empty());

        store.clear().await?;
        Ok(())
    }

    #[tokio::test]
    async fn test_pgvector_store_delete_by_source() -> crate::Result<()> {
        let Some(pool) = pg_pool().await else {
            return Ok(());
        };
        let store = PgVectorStore::new(pool, 2, "test_pgvector_delete").await?;
        store.clear().await?;

        store
            .store_chunk(EmbeddedChunk {
                id: "c1".to_string(),
                source_id: "doc-a".to_string(),
                text: "a".to_string(),
                embedding: vec![1.0, 0.0],
                position: 0,
                total_chunks: 2,
                collection: None,
                metadata: None,
            })
            .await?;
        store
            .store_chunk(EmbeddedChunk {
                id: "c2".to_string(),
                source_id: "doc-a".to_string(),
                text: "b".to_string(),
                embedding: vec![0.0, 1.0],
                position: 1,
                total_chunks: 2,
                collection: None,
                metadata: None,
            })
            .await?;
        store
            .store_chunk(EmbeddedChunk {
                id: "c3".to_string(),
                source_id: "doc-b".to_string(),
                text: "c".to_string(),
                embedding: vec![1.0, 1.0],
                position: 0,
                total_chunks: 1,
                collection: None,
                metadata: None,
            })
            .await?;

        let deleted = store.delete_by_source("doc-a").await?;
        assert_eq!(deleted, 2);

        let stats = store.stats().await?;
        assert_eq!(stats.total_vectors, 1);
        store.clear().await?;
        Ok(())
    }
}
