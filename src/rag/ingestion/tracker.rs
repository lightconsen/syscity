//! Ingestion log tracker for Knowledge Base.
//!
//! Manages the `kb_ingestion_log` table which tracks which documents have been
//! ingested, their checksums (for change detection), and indexing status.

use serde::{Deserialize, Serialize};
use sqlx::{Pool, Row, Sqlite};

/// Ingestion status for a document.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum IngestionStatus {
    /// Successfully indexed.
    Indexed,
    /// Indexing failed.
    Failed,
    /// Previously indexed but source file has changed or is gone.
    Stale,
}

impl IngestionStatus {
    fn as_str(&self) -> &'static str {
        match self {
            IngestionStatus::Indexed => "indexed",
            IngestionStatus::Failed => "failed",
            IngestionStatus::Stale => "stale",
        }
    }

    fn from_str(s: &str) -> Self {
        match s {
            "failed" => IngestionStatus::Failed,
            "stale" => IngestionStatus::Stale,
            _ => IngestionStatus::Indexed,
        }
    }
}

/// A record in the `kb_ingestion_log` table.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IngestionRecord {
    pub collection: String,
    pub doc_id: String,
    pub source_id: String,
    pub checksum: Option<String>,
    pub mtime: Option<i64>,
    pub chunk_count: usize,
    pub status: IngestionStatus,
    pub error: Option<String>,
    pub indexed_at: String,
}

/// Aggregated statistics for a collection.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CollectionStats {
    pub total_docs: usize,
    pub total_chunks: usize,
    pub last_indexed_at: Option<String>,
    pub stale_count: usize,
    pub failed_count: usize,
}

/// Get a single ingestion record.
pub async fn get_record(
    pool: &Pool<Sqlite>,
    collection: &str,
    doc_id: &str,
) -> crate::Result<Option<IngestionRecord>> {
    let row = sqlx::query(
        "SELECT collection, doc_id, source_id, checksum, mtime, chunk_count, \
         status, error, indexed_at \
         FROM kb_ingestion_log WHERE collection = ? AND doc_id = ?",
    )
    .bind(collection)
    .bind(doc_id)
    .fetch_optional(pool)
    .await
    .map_err(|e| crate::error::SyscityError::Storage {
        context: "Failed to get ingestion record".to_string(),
        details: e.to_string(),
    })?;

    Ok(row.map(row_to_record))
}

/// Insert or replace an ingestion record.
pub async fn upsert_record(pool: &Pool<Sqlite>, record: &IngestionRecord) -> crate::Result<()> {
    sqlx::query(
        "INSERT OR REPLACE INTO kb_ingestion_log \
         (collection, doc_id, source_id, checksum, mtime, chunk_count, status, error, indexed_at) \
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, datetime('now'))",
    )
    .bind(&record.collection)
    .bind(&record.doc_id)
    .bind(&record.source_id)
    .bind(&record.checksum)
    .bind(record.mtime)
    .bind(record.chunk_count as i64)
    .bind(record.status.as_str())
    .bind(&record.error)
    .execute(pool)
    .await
    .map_err(|e| crate::error::SyscityError::Storage {
        context: "Failed to upsert ingestion record".to_string(),
        details: e.to_string(),
    })?;
    Ok(())
}

/// Mark a document as stale.
pub async fn mark_stale(pool: &Pool<Sqlite>, collection: &str, doc_id: &str) -> crate::Result<()> {
    sqlx::query(
        "UPDATE kb_ingestion_log SET status = 'stale', indexed_at = datetime('now') \
         WHERE collection = ? AND doc_id = ?",
    )
    .bind(collection)
    .bind(doc_id)
    .execute(pool)
    .await
    .map_err(|e| crate::error::SyscityError::Storage {
        context: "Failed to mark ingestion record as stale".to_string(),
        details: e.to_string(),
    })?;
    Ok(())
}

/// List records, optionally filtered by collection and status.
///
/// When `collection` is `None`, records from all collections are returned.
pub async fn list_records(
    pool: &Pool<Sqlite>,
    collection: Option<&str>,
    status: Option<IngestionStatus>,
) -> crate::Result<Vec<IngestionRecord>> {
    let rows = match (collection, status) {
        (Some(col), Some(ref s)) => sqlx::query(
            "SELECT collection, doc_id, source_id, checksum, mtime, chunk_count, \
                 status, error, indexed_at \
                 FROM kb_ingestion_log WHERE collection = ? AND status = ? \
                 ORDER BY collection, doc_id",
        )
        .bind(col)
        .bind(s.as_str())
        .fetch_all(pool)
        .await
        .map_err(|e| crate::error::SyscityError::Storage {
            context: "Failed to list ingestion records".to_string(),
            details: e.to_string(),
        })?,
        (Some(col), None) => sqlx::query(
            "SELECT collection, doc_id, source_id, checksum, mtime, chunk_count, \
                 status, error, indexed_at \
                 FROM kb_ingestion_log WHERE collection = ? \
                 ORDER BY collection, doc_id",
        )
        .bind(col)
        .fetch_all(pool)
        .await
        .map_err(|e| crate::error::SyscityError::Storage {
            context: "Failed to list ingestion records".to_string(),
            details: e.to_string(),
        })?,
        (None, Some(ref s)) => sqlx::query(
            "SELECT collection, doc_id, source_id, checksum, mtime, chunk_count, \
                 status, error, indexed_at \
                 FROM kb_ingestion_log WHERE status = ? \
                 ORDER BY collection, doc_id",
        )
        .bind(s.as_str())
        .fetch_all(pool)
        .await
        .map_err(|e| crate::error::SyscityError::Storage {
            context: "Failed to list ingestion records".to_string(),
            details: e.to_string(),
        })?,
        (None, None) => sqlx::query(
            "SELECT collection, doc_id, source_id, checksum, mtime, chunk_count, \
                 status, error, indexed_at \
                 FROM kb_ingestion_log \
                 ORDER BY collection, doc_id",
        )
        .fetch_all(pool)
        .await
        .map_err(|e| crate::error::SyscityError::Storage {
            context: "Failed to list ingestion records".to_string(),
            details: e.to_string(),
        })?,
    };

    Ok(rows.into_iter().map(row_to_record).collect())
}

/// List all distinct collections with summary stats.
///
/// Returns one `CollectionStats` per collection, including the collection name
/// stored in `last_indexed_at` (hack: field repurposed for transport).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CollectionSummary {
    pub collection: String,
    pub total_docs: usize,
    pub total_chunks: usize,
    pub last_indexed_at: Option<String>,
    pub stale_count: usize,
    pub failed_count: usize,
}

/// Get summary stats for all collections.
pub async fn list_collections(pool: &Pool<Sqlite>) -> crate::Result<Vec<CollectionSummary>> {
    #[derive(Debug, sqlx::FromRow)]
    struct CollRow {
        collection: String,
        doc_count: i64,
        chunk_sum: Option<i64>,
        last_idx: Option<String>,
        stale: i64,
        failed: i64,
    }

    let rows: Vec<CollRow> = sqlx::query_as(
        "SELECT
            collection,
            COUNT(*) AS doc_count,
            SUM(CASE WHEN status = 'indexed' THEN chunk_count ELSE 0 END) AS chunk_sum,
            MAX(indexed_at) AS last_idx,
            SUM(CASE WHEN status = 'stale' THEN 1 ELSE 0 END) AS stale,
            SUM(CASE WHEN status = 'failed' THEN 1 ELSE 0 END) AS failed
         FROM kb_ingestion_log
         GROUP BY collection
         ORDER BY collection",
    )
    .fetch_all(pool)
    .await
    .map_err(|e| crate::error::SyscityError::Storage {
        context: "Failed to list collections".to_string(),
        details: e.to_string(),
    })?;

    Ok(rows
        .into_iter()
        .map(|r| CollectionSummary {
            collection: r.collection,
            total_docs: r.doc_count as usize,
            total_chunks: r.chunk_sum.unwrap_or(0) as usize,
            last_indexed_at: r.last_idx,
            stale_count: r.stale as usize,
            failed_count: r.failed as usize,
        })
        .collect())
}

/// Delete a specific ingestion record.
pub async fn delete_record(
    pool: &Pool<Sqlite>,
    collection: &str,
    doc_id: &str,
) -> crate::Result<bool> {
    let result = sqlx::query("DELETE FROM kb_ingestion_log WHERE collection = ? AND doc_id = ?")
        .bind(collection)
        .bind(doc_id)
        .execute(pool)
        .await
        .map_err(|e| crate::error::SyscityError::Storage {
            context: "Failed to delete ingestion record".to_string(),
            details: e.to_string(),
        })?;
    Ok(result.rows_affected() > 0)
}

/// Get aggregated statistics for a collection.
pub async fn get_collection_stats(
    pool: &Pool<Sqlite>,
    collection: &str,
) -> crate::Result<CollectionStats> {
    let total: (i64,) =
        sqlx::query_as("SELECT COUNT(*) FROM kb_ingestion_log WHERE collection = ?")
            .bind(collection)
            .fetch_one(pool)
            .await
            .map_err(|e| crate::error::SyscityError::Storage {
                context: "Failed to count collection records".to_string(),
                details: e.to_string(),
            })?;

    let chunks: (Option<i64>,) = sqlx::query_as(
        "SELECT SUM(chunk_count) FROM kb_ingestion_log WHERE collection = ? AND status = 'indexed'",
    )
    .bind(collection)
    .fetch_one(pool)
    .await
    .map_err(|e| crate::error::SyscityError::Storage {
        context: "Failed to sum collection chunks".to_string(),
        details: e.to_string(),
    })?;

    let stale: (i64,) = sqlx::query_as(
        "SELECT COUNT(*) FROM kb_ingestion_log WHERE collection = ? AND status = 'stale'",
    )
    .bind(collection)
    .fetch_one(pool)
    .await
    .map_err(|e| crate::error::SyscityError::Storage {
        context: "Failed to count stale records".to_string(),
        details: e.to_string(),
    })?;

    let failed: (i64,) = sqlx::query_as(
        "SELECT COUNT(*) FROM kb_ingestion_log WHERE collection = ? AND status = 'failed'",
    )
    .bind(collection)
    .fetch_one(pool)
    .await
    .map_err(|e| crate::error::SyscityError::Storage {
        context: "Failed to count failed records".to_string(),
        details: e.to_string(),
    })?;

    let last: Option<(String,)> = sqlx::query_as(
        "SELECT indexed_at FROM kb_ingestion_log WHERE collection = ? \
         ORDER BY indexed_at DESC LIMIT 1",
    )
    .bind(collection)
    .fetch_optional(pool)
    .await
    .map_err(|e| crate::error::SyscityError::Storage {
        context: "Failed to get last indexed time".to_string(),
        details: e.to_string(),
    })?;

    Ok(CollectionStats {
        total_docs: total.0 as usize,
        total_chunks: chunks.0.unwrap_or(0) as usize,
        last_indexed_at: last.map(|r| r.0),
        stale_count: stale.0 as usize,
        failed_count: failed.0 as usize,
    })
}

/// Convert a SQLite row to an `IngestionRecord`.
fn row_to_record(row: sqlx::sqlite::SqliteRow) -> IngestionRecord {
    IngestionRecord {
        collection: row.get("collection"),
        doc_id: row.get("doc_id"),
        source_id: row.get("source_id"),
        checksum: row.get("checksum"),
        mtime: row.get("mtime"),
        chunk_count: row.get::<i64, _>("chunk_count") as usize,
        status: IngestionStatus::from_str(row.get("status")),
        error: row.get("error"),
        indexed_at: row.get("indexed_at"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn test_pool() -> Pool<Sqlite> {
        let pool = sqlx::sqlite::SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .unwrap();
        // Create the table
        sqlx::query(
            "CREATE TABLE IF NOT EXISTS kb_ingestion_log (
                collection TEXT NOT NULL,
                doc_id TEXT NOT NULL,
                source_id TEXT NOT NULL,
                checksum TEXT,
                mtime INTEGER,
                chunk_count INTEGER DEFAULT 0,
                status TEXT NOT NULL DEFAULT 'indexed',
                error TEXT,
                indexed_at TEXT NOT NULL DEFAULT (datetime('now')),
                PRIMARY KEY (collection, doc_id)
            )",
        )
        .execute(&pool)
        .await
        .unwrap();
        pool
    }

    #[tokio::test]
    async fn test_upsert_and_get_record() {
        let pool = test_pool().await;
        let record = IngestionRecord {
            collection: "test-col".to_string(),
            doc_id: "doc1".to_string(),
            source_id: "/path/to/doc1.md".to_string(),
            checksum: Some("abc123".to_string()),
            mtime: Some(1000),
            chunk_count: 5,
            status: IngestionStatus::Indexed,
            error: None,
            indexed_at: String::new(),
        };

        upsert_record(&pool, &record).await.unwrap();

        let fetched = get_record(&pool, "test-col", "doc1").await.unwrap();
        assert!(fetched.is_some());
        let fetched = fetched.unwrap();
        assert_eq!(fetched.doc_id, "doc1");
        assert_eq!(fetched.collection, "test-col");
        assert_eq!(fetched.checksum, Some("abc123".to_string()));
        assert_eq!(fetched.chunk_count, 5);
    }

    #[tokio::test]
    async fn test_mark_stale() {
        let pool = test_pool().await;
        let record = IngestionRecord {
            collection: "test-col".to_string(),
            doc_id: "doc1".to_string(),
            source_id: "source".to_string(),
            checksum: None,
            mtime: None,
            chunk_count: 1,
            status: IngestionStatus::Indexed,
            error: None,
            indexed_at: String::new(),
        };
        upsert_record(&pool, &record).await.unwrap();

        mark_stale(&pool, "test-col", "doc1").await.unwrap();

        let fetched = get_record(&pool, "test-col", "doc1")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(fetched.status, IngestionStatus::Stale);
    }

    #[tokio::test]
    async fn test_list_records() {
        let pool = test_pool().await;
        for i in 0..3 {
            let record = IngestionRecord {
                collection: "test-col".to_string(),
                doc_id: format!("doc{}", i),
                source_id: format!("source{}", i),
                checksum: None,
                mtime: None,
                chunk_count: 1,
                status: IngestionStatus::Indexed,
                error: None,
                indexed_at: String::new(),
            };
            upsert_record(&pool, &record).await.unwrap();
        }

        let records = list_records(&pool, Some("test-col"), None).await.unwrap();
        assert_eq!(records.len(), 3);
    }

    #[tokio::test]
    async fn test_list_records_all_collections() {
        let pool = test_pool().await;
        for col in &["col-a", "col-b"] {
            let record = IngestionRecord {
                collection: col.to_string(),
                doc_id: "doc1".to_string(),
                source_id: "source".to_string(),
                checksum: None,
                mtime: None,
                chunk_count: 1,
                status: IngestionStatus::Indexed,
                error: None,
                indexed_at: String::new(),
            };
            upsert_record(&pool, &record).await.unwrap();
        }

        let records = list_records(&pool, None, None).await.unwrap();
        assert_eq!(records.len(), 2);
    }

    #[tokio::test]
    async fn test_list_collections() {
        let pool = test_pool().await;
        for i in 0..2 {
            let record = IngestionRecord {
                collection: "test-col".to_string(),
                doc_id: format!("doc{}", i),
                source_id: "source".to_string(),
                checksum: None,
                mtime: None,
                chunk_count: 2,
                status: IngestionStatus::Indexed,
                error: None,
                indexed_at: String::new(),
            };
            upsert_record(&pool, &record).await.unwrap();
        }

        let collections = list_collections(&pool).await.unwrap();
        assert_eq!(collections.len(), 1);
        assert_eq!(collections[0].collection, "test-col");
        assert_eq!(collections[0].total_docs, 2);
        assert_eq!(collections[0].total_chunks, 4);
    }

    #[tokio::test]
    async fn test_delete_record() {
        let pool = test_pool().await;
        let record = IngestionRecord {
            collection: "test-col".to_string(),
            doc_id: "doc1".to_string(),
            source_id: "source".to_string(),
            checksum: None,
            mtime: None,
            chunk_count: 1,
            status: IngestionStatus::Indexed,
            error: None,
            indexed_at: String::new(),
        };
        upsert_record(&pool, &record).await.unwrap();

        let deleted = delete_record(&pool, "test-col", "doc1").await.unwrap();
        assert!(deleted);

        let fetched = get_record(&pool, "test-col", "doc1").await.unwrap();
        assert!(fetched.is_none());
    }

    #[tokio::test]
    async fn test_get_collection_stats() {
        let pool = test_pool().await;
        for i in 0..3 {
            let record = IngestionRecord {
                collection: "test-col".to_string(),
                doc_id: format!("doc{}", i),
                source_id: format!("source{}", i),
                checksum: None,
                mtime: None,
                chunk_count: 2,
                status: IngestionStatus::Indexed,
                error: None,
                indexed_at: String::new(),
            };
            upsert_record(&pool, &record).await.unwrap();
        }

        let stats = get_collection_stats(&pool, "test-col").await.unwrap();
        assert_eq!(stats.total_docs, 3);
        assert_eq!(stats.total_chunks, 6);
        assert_eq!(stats.stale_count, 0);
        assert_eq!(stats.failed_count, 0);
    }

    #[test]
    fn test_ingestion_status_roundtrip() {
        assert_eq!(IngestionStatus::from_str("indexed"), IngestionStatus::Indexed);
        assert_eq!(IngestionStatus::from_str("failed"), IngestionStatus::Failed);
        assert_eq!(IngestionStatus::from_str("stale"), IngestionStatus::Stale);
        assert_eq!(IngestionStatus::Indexed.as_str(), "indexed");
        assert_eq!(IngestionStatus::Failed.as_str(), "failed");
        assert_eq!(IngestionStatus::Stale.as_str(), "stale");
    }
}

/// Create the `kb_ingestion_log` table if it does not exist (idempotent).
///
/// The DDL lives here, beside the only code that reads and writes the table,
/// so the table and its owner stay together. The unified-database schema init
/// (`memory::db`) calls this rather than carrying a copy of the definition —
/// that copy is how the table came to be created by one module and used by
/// another.
pub async fn ensure_schema(pool: &Pool<Sqlite>) -> crate::Result<()> {
    sqlx::query(
        r#"
        CREATE TABLE IF NOT EXISTS kb_ingestion_log (
            collection TEXT NOT NULL,
            doc_id TEXT NOT NULL,
            source_id TEXT NOT NULL,
            checksum TEXT,
            mtime INTEGER,
            chunk_count INTEGER DEFAULT 0,
            status TEXT NOT NULL DEFAULT 'indexed',
            error TEXT,
            indexed_at TEXT NOT NULL DEFAULT (datetime('now')),
            PRIMARY KEY (collection, doc_id)
        )
        "#,
    )
    .execute(pool)
    .await
    .map_err(|e| crate::error::SyscityError::Storage {
        context: "Failed to create kb_ingestion_log table".to_string(),
        details: e.to_string(),
    })?;
    Ok(())
}
