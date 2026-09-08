//! SQLite + sqlite-vec extension vector store backend.

#![allow(unsafe_code)]

use std::os::raw::{c_char, c_int};
use std::sync::OnceLock;

use async_trait::async_trait;
use sqlx::{sqlite::SqlitePoolOptions, Pool, Row, Sqlite};
use tracing::info;

use crate::rag::chunk::EmbeddedChunk;
use crate::rag::vector_store::{VectorStore, VectorStoreStats};

/// SQLite-backed vector store using the native `sqlite-vec` extension.
#[derive(Debug, Clone)]
pub struct SqliteVecStore {
    pool: Pool<Sqlite>,
    dimension: usize,
}

/// Register the sqlite-vec extension as a SQLite auto-extension so that every
/// new connection created by SQLx (which shares the same SQLite library) has
/// the extension available.
mod sqlite_vec_ext {
    use super::*;

    type AutoExtension = unsafe extern "C" fn(
        *mut libsqlite3_sys::sqlite3,
        *mut *mut c_char,
        *const libsqlite3_sys::sqlite3_api_routines,
    ) -> c_int;

    /// Register the sqlite-vec extension as a SQLite auto-extension.
    ///
    /// # Safety
    /// This calls `sqlite3_auto_extension` with a function pointer transmuted
    /// from `sqlite_vec::sqlite3_vec_init`. The transmute is only sound because
    /// sqlite-vec's init routine has the exact SQLite auto-extension ABI that
    /// `sqlite3_auto_extension` expects. This function must not be called from
    /// multiple threads concurrently; callers rely on the `OnceLock` wrapper in
    /// [`register()`].
    unsafe fn register_vec_extension() -> Result<(), String> {
        let init: AutoExtension = std::mem::transmute(sqlite_vec::sqlite3_vec_init as *const ());
        let rc = libsqlite3_sys::sqlite3_auto_extension(Some(init));
        if rc == libsqlite3_sys::SQLITE_OK {
            Ok(())
        } else {
            Err(format!("sqlite3_auto_extension returned error code {}", rc))
        }
    }

    fn register() -> Result<(), String> {
        static RESULT: OnceLock<Result<(), String>> = OnceLock::new();
        RESULT
            .get_or_init(|| unsafe { register_vec_extension() })
            .clone()
    }

    pub fn ensure_registered() -> crate::Result<()> {
        register().map_err(|details| crate::error::SyscityError::Storage {
            context: "Failed to register sqlite-vec extension".to_string(),
            details,
        })
    }
}

impl SqliteVecStore {
    /// Create a new sqlite-vec-backed store.
    pub async fn new(path: &str, dimension: usize) -> crate::Result<Self> {
        sqlite_vec_ext::ensure_registered()?;
        let pool = SqlitePoolOptions::new()
            .max_connections(5)
            .connect(path)
            .await
            .map_err(|e| crate::error::SyscityError::Storage {
                context: format!("Failed to connect to SQLite at {}", path),
                details: e.to_string(),
            })?;

        let create_sql = format!(
            "CREATE VIRTUAL TABLE IF NOT EXISTS vec_chunks USING vec0(
                embedding float[{}] distance_metric=cosine,
                +id text,
                +source_id text,
                +text text,
                +position integer,
                +total_chunks integer,
                +metadata text
            )",
            dimension
        );
        sqlx::query(&create_sql).execute(&pool).await.map_err(|e| {
            crate::error::SyscityError::Storage {
                context: "Failed to create sqlite-vec virtual table".to_string(),
                details: e.to_string(),
            }
        })?;

        sqlx::query(
            "CREATE TABLE IF NOT EXISTS vec_chunk_collections (
                chunk_id TEXT NOT NULL,
                collection TEXT NOT NULL,
                PRIMARY KEY (chunk_id, collection)
            )",
        )
        .execute(&pool)
        .await
        .map_err(|e| crate::error::SyscityError::Storage {
            context: "Failed to create sqlite-vec collection table".to_string(),
            details: e.to_string(),
        })?;

        info!("SqliteVecStore initialized at {} (dim={})", path, dimension);
        Ok(Self { pool, dimension })
    }

    /// Create an in-memory sqlite-vec store (for testing).
    pub async fn new_in_memory(dimension: usize) -> crate::Result<Self> {
        Self::new("sqlite::memory:", dimension).await
    }
}

fn embedding_to_bytes(embedding: &[f32]) -> Vec<u8> {
    embedding.iter().flat_map(|f| f.to_le_bytes()).collect()
}

fn bytes_to_embedding(bytes: &[u8]) -> crate::Result<Vec<f32>> {
    if !bytes.len().is_multiple_of(4) {
        return Err(crate::error::SyscityError::Storage {
            context: "Invalid sqlite-vec embedding blob".to_string(),
            details: format!(
                "Embedding blob length {} is not a multiple of 4 (f32 size)",
                bytes.len()
            ),
        });
    }
    Ok(bytes
        .chunks_exact(4)
        .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]))
        .collect())
}

#[async_trait]
impl VectorStore for SqliteVecStore {
    async fn store_chunk(&self, chunk: EmbeddedChunk) -> crate::Result<()> {
        if chunk.embedding.len() != self.dimension {
            return Err(crate::error::SyscityError::Storage {
                context: "Embedding dimension mismatch".to_string(),
                details: format!(
                    "expected dimension {}, got {}",
                    self.dimension,
                    chunk.embedding.len()
                ),
            });
        }

        let embedding_bytes = embedding_to_bytes(&chunk.embedding);
        let metadata_json = chunk
            .metadata
            .as_ref()
            .map(serde_json::to_string)
            .transpose()
            .map_err(|e| crate::error::SyscityError::Storage {
                context: "Failed to serialize metadata".to_string(),
                details: e.to_string(),
            })?;

        sqlx::query(
            "INSERT OR REPLACE INTO vec_chunks
             (id, source_id, text, embedding, position, total_chunks, metadata)
             VALUES (?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(&chunk.id)
        .bind(&chunk.source_id)
        .bind(&chunk.text)
        .bind(&embedding_bytes)
        .bind(chunk.position as i64)
        .bind(chunk.total_chunks as i64)
        .bind(metadata_json)
        .execute(&self.pool)
        .await
        .map_err(|e| crate::error::SyscityError::Storage {
            context: "Failed to store sqlite-vec chunk".to_string(),
            details: e.to_string(),
        })?;

        if let Some(collection) = &chunk.collection {
            sqlx::query(
                "INSERT OR REPLACE INTO vec_chunk_collections (chunk_id, collection) VALUES (?, ?)",
            )
            .bind(&chunk.id)
            .bind(collection)
            .execute(&self.pool)
            .await
            .map_err(|e| crate::error::SyscityError::Storage {
                context: "Failed to store sqlite-vec collection".to_string(),
                details: e.to_string(),
            })?;
        }

        Ok(())
    }

    async fn search_similar(
        &self,
        query_embedding: &[f32],
        limit: usize,
        threshold: f32,
        collection: Option<&str>,
    ) -> crate::Result<Vec<(EmbeddedChunk, f32)>> {
        if query_embedding.len() != self.dimension {
            return Err(crate::error::SyscityError::Storage {
                context: "Query embedding dimension mismatch".to_string(),
                details: format!(
                    "expected dimension {}, got {}",
                    self.dimension,
                    query_embedding.len()
                ),
            });
        }

        let query_bytes = embedding_to_bytes(query_embedding);
        let max_distance = 1.0f64 - threshold as f64;

        let rows = if let Some(collection) = collection {
            // The KNN scan must live in its own subquery: sqlite-vec cannot
            // extract the `k` constraint from a `LIMIT ?` that belongs to a
            // query joining another table, and fails with "A LIMIT or 'k = ?'
            // constraint is required on vec0 knn queries". The subquery keeps
            // the plain (supported) KNN shape; the collection filter is
            // applied by the outer join.
            sqlx::query(
                "SELECT v.rowid, v.id, v.source_id, v.text, v.embedding, v.position,
                        v.total_chunks, v.metadata, v.distance
                 FROM (SELECT rowid, id, source_id, text, embedding, position,
                              total_chunks, metadata, distance
                       FROM vec_chunks
                       WHERE embedding MATCH ? AND distance <= ?
                       ORDER BY distance
                       LIMIT ?) v
                 JOIN vec_chunk_collections c ON v.id = c.chunk_id
                 WHERE c.collection = ?
                 ORDER BY v.distance",
            )
            .bind(&query_bytes)
            .bind(max_distance)
            .bind(limit as i64)
            .bind(collection)
            .fetch_all(&self.pool)
            .await
            .map_err(|e| crate::error::SyscityError::Storage {
                context: "Failed to search sqlite-vec".to_string(),
                details: e.to_string(),
            })?
        } else {
            sqlx::query(
                "SELECT rowid, id, source_id, text, embedding, position, total_chunks, metadata, \
                 distance
                 FROM vec_chunks
                 WHERE embedding MATCH ? AND distance <= ?
                 ORDER BY distance
                 LIMIT ?",
            )
            .bind(&query_bytes)
            .bind(max_distance)
            .bind(limit as i64)
            .fetch_all(&self.pool)
            .await
            .map_err(|e| crate::error::SyscityError::Storage {
                context: "Failed to search sqlite-vec".to_string(),
                details: e.to_string(),
            })?
        };

        let mut results = Vec::new();
        for row in rows {
            let distance: f64 =
                row.try_get("distance")
                    .map_err(|e| crate::error::SyscityError::Storage {
                        context: "Failed to read sqlite-vec distance".to_string(),
                        details: e.to_string(),
                    })?;
            let metadata: Option<String> =
                row.try_get("metadata")
                    .map_err(|e| crate::error::SyscityError::Storage {
                        context: "Failed to read sqlite-vec metadata column".to_string(),
                        details: e.to_string(),
                    })?;
            let chunk = EmbeddedChunk {
                id: row
                    .try_get("id")
                    .map_err(|e| crate::error::SyscityError::Storage {
                        context: "Failed to read sqlite-vec id".to_string(),
                        details: e.to_string(),
                    })?,
                source_id: row.try_get("source_id").map_err(|e| {
                    crate::error::SyscityError::Storage {
                        context: "Failed to read sqlite-vec source_id".to_string(),
                        details: e.to_string(),
                    }
                })?,
                text: row
                    .try_get("text")
                    .map_err(|e| crate::error::SyscityError::Storage {
                        context: "Failed to read sqlite-vec text".to_string(),
                        details: e.to_string(),
                    })?,
                embedding: {
                    let blob: Vec<u8> = row.try_get("embedding").map_err(|e| {
                        crate::error::SyscityError::Storage {
                            context: "Failed to read sqlite-vec embedding".to_string(),
                            details: e.to_string(),
                        }
                    })?;
                    let embedding = bytes_to_embedding(&blob)?;
                    if embedding.len() != self.dimension {
                        return Err(crate::error::SyscityError::Storage {
                            context: "sqlite-vec embedding dimension mismatch".to_string(),
                            details: format!(
                                "expected dimension {}, got {}",
                                self.dimension,
                                embedding.len()
                            ),
                        });
                    }
                    embedding
                },
                position: row.try_get::<i64, _>("position").map_err(|e| {
                    crate::error::SyscityError::Storage {
                        context: "Failed to read sqlite-vec position".to_string(),
                        details: e.to_string(),
                    }
                })? as usize,
                total_chunks: row.try_get::<i64, _>("total_chunks").map_err(|e| {
                    crate::error::SyscityError::Storage {
                        context: "Failed to read sqlite-vec total_chunks".to_string(),
                        details: e.to_string(),
                    }
                })? as usize,
                collection: None,
                metadata: metadata
                    .map(|m| serde_json::from_str(&m))
                    .transpose()
                    .map_err(|e| crate::error::SyscityError::Storage {
                        context: "Failed to deserialize sqlite-vec metadata".to_string(),
                        details: e.to_string(),
                    })?,
            };
            results.push((chunk, (1.0f64 - distance) as f32));
        }
        Ok(results)
    }

    async fn delete_by_source(&self, source_id: &str) -> crate::Result<usize> {
        // Must clean vec_chunk_collections first to avoid orphaned rows.
        sqlx::query(
            "DELETE FROM vec_chunk_collections WHERE chunk_id IN \
             (SELECT id FROM vec_chunks WHERE source_id = ?)",
        )
        .bind(source_id)
        .execute(&self.pool)
        .await
        .map_err(|e| crate::error::SyscityError::Storage {
            context: "Failed to delete chunk collections by source".to_string(),
            details: e.to_string(),
        })?;

        let result = sqlx::query("DELETE FROM vec_chunks WHERE source_id = ?")
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
        // Delete the chunks first (the membership subquery is still intact in
        // that same statement), then clean up the now-orphaned membership rows.
        let result = sqlx::query(
            "DELETE FROM vec_chunks WHERE source_id = ?1 AND id IN \
             (SELECT chunk_id FROM vec_chunk_collections WHERE collection = ?2)",
        )
        .bind(source_id)
        .bind(collection)
        .execute(&self.pool)
        .await
        .map_err(|e| crate::error::SyscityError::Storage {
            context: "Failed to delete by source in collection".to_string(),
            details: e.to_string(),
        })?;

        sqlx::query(
            "DELETE FROM vec_chunk_collections WHERE collection = ?1 AND chunk_id NOT IN \
             (SELECT id FROM vec_chunks)",
        )
        .bind(collection)
        .execute(&self.pool)
        .await
        .map_err(|e| crate::error::SyscityError::Storage {
            context: "Failed to clean chunk collections after scoped source delete".to_string(),
            details: e.to_string(),
        })?;

        Ok(result.rows_affected() as usize)
    }

    async fn delete_by_collection(&self, collection: &str) -> crate::Result<usize> {
        // Delete from vec_chunks for chunks belonging to this collection.
        let result = sqlx::query(
            "DELETE FROM vec_chunks WHERE id IN \
             (SELECT chunk_id FROM vec_chunk_collections WHERE collection = ?)",
        )
        .bind(collection)
        .execute(&self.pool)
        .await
        .map_err(|e| crate::error::SyscityError::Storage {
            context: "Failed to delete by collection from vec_chunks".to_string(),
            details: e.to_string(),
        })?;

        // Clean the collection table itself.
        sqlx::query("DELETE FROM vec_chunk_collections WHERE collection = ?")
            .bind(collection)
            .execute(&self.pool)
            .await
            .map_err(|e| crate::error::SyscityError::Storage {
                context: "Failed to delete by collection from vec_chunk_collections".to_string(),
                details: e.to_string(),
            })?;

        Ok(result.rows_affected() as usize)
    }

    async fn stats(&self) -> crate::Result<VectorStoreStats> {
        let total_vectors: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM vec_chunks")
            .fetch_one(&self.pool)
            .await
            .map_err(|e| crate::error::SyscityError::Storage {
                context: "Failed to count vectors".to_string(),
                details: e.to_string(),
            })?;
        let total_sources: (i64,) =
            sqlx::query_as("SELECT COUNT(DISTINCT source_id) FROM vec_chunks")
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
        sqlx::query("DELETE FROM vec_chunk_collections")
            .execute(&self.pool)
            .await
            .map_err(|e| crate::error::SyscityError::Storage {
                context: "Failed to clear sqlite-vec collection table".to_string(),
                details: e.to_string(),
            })?;
        sqlx::query("DELETE FROM vec_chunks")
            .execute(&self.pool)
            .await
            .map_err(|e| crate::error::SyscityError::Storage {
                context: "Failed to clear sqlite-vec table".to_string(),
                details: e.to_string(),
            })?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rag::chunk::EmbeddedChunk;

    #[tokio::test]
    async fn test_sqlite_vec_store_store_and_search() -> crate::Result<()> {
        let store = SqliteVecStore::new_in_memory(3).await?;
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
        Ok(())
    }

    #[tokio::test]
    async fn test_sqlite_vec_store_collection_search() -> crate::Result<()> {
        // Regression: the collection-filtered search joins
        // `vec_chunk_collections`; a direct KNN + JOIN query fails on
        // sqlite-vec ("A LIMIT or 'k = ?' constraint is required on vec0 knn
        // queries"). The query must use the subquery form.
        let store = SqliteVecStore::new_in_memory(2).await?;
        for (id, source, collection, embedding) in [
            ("c1", "doc-a", Some("col-a"), vec![1.0, 0.0]),
            ("c2", "doc-b", Some("col-b"), vec![0.9, 0.1]),
            ("c3", "doc-c", None, vec![1.0, 0.0]),
        ] {
            store
                .store_chunk(EmbeddedChunk {
                    id: id.to_string(),
                    source_id: source.to_string(),
                    text: id.to_string(),
                    embedding,
                    position: 0,
                    total_chunks: 1,
                    collection: collection.map(String::from),
                    metadata: None,
                })
                .await?;
        }

        let results = store
            .search_similar(&[1.0, 0.0], 10, 0.5, Some("col-a"))
            .await?;
        assert_eq!(results.len(), 1, "only col-a chunks must match");
        assert_eq!(results[0].0.id, "c1");

        // The other collection must not leak its chunks.
        let results_b = store
            .search_similar(&[1.0, 0.0], 10, 0.5, Some("col-b"))
            .await?;
        assert_eq!(results_b.len(), 1);
        assert_eq!(results_b[0].0.id, "c2");
        Ok(())
    }

    #[tokio::test]
    async fn test_sqlite_vec_store_delete_by_source() -> crate::Result<()> {
        let store = SqliteVecStore::new_in_memory(2).await?;
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
        Ok(())
    }

    #[tokio::test]
    async fn test_sqlite_vec_store_stats_and_clear() -> crate::Result<()> {
        let store = SqliteVecStore::new_in_memory(4).await?;
        store
            .store_chunk(EmbeddedChunk {
                id: "c1".to_string(),
                source_id: "s1".to_string(),
                text: "a".to_string(),
                embedding: vec![0.0; 4],
                position: 0,
                total_chunks: 1,
                collection: None,
                metadata: None,
            })
            .await?;
        store
            .store_chunk(EmbeddedChunk {
                id: "c2".to_string(),
                source_id: "s2".to_string(),
                text: "b".to_string(),
                embedding: vec![0.0; 4],
                position: 0,
                total_chunks: 1,
                collection: None,
                metadata: None,
            })
            .await?;

        let stats = store.stats().await?;
        assert_eq!(stats.total_vectors, 2);
        assert_eq!(stats.total_sources, 2);
        assert_eq!(stats.dimension, 4);

        store.clear().await?;
        let stats = store.stats().await?;
        assert_eq!(stats.total_vectors, 0);
        Ok(())
    }
}
