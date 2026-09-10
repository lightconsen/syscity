//! Storage subsystem initialization.
//!
//! Selects and initializes the storage adapter (in-memory, file, or SQLite),
//! session store, persistent audit log, and optional unified vector store.

use std::sync::Arc;

use tokio::sync::RwLock;
use tracing::{info, warn};

use crate::adapters::{FileStorage, InMemoryStorage, SqliteStorage, Storage};
use crate::agent::session_store::SessionStore;
use crate::error::SyscityError;
use crate::eval::{DecisionTraceStore, PendingBadcaseStore, TurnSampleStore};
use crate::gateway::FeedbackStore;
use crate::gateway::GatewayConfig;
#[cfg(feature = "sqlite-vec")]
use crate::rag::sqlite_vec_store::SqliteVecStore;
use crate::rag::VectorStore;
use crate::security::auth_store::AuthStore;
use crate::security::persistent_audit::PersistentAuditLog;
use crate::security::runtime_audit::AuditLogger;

/// Storage initialization result.
pub struct StorageInit {
    pub storage: Arc<RwLock<dyn Storage>>,
    pub unified_vector_store: Option<Arc<dyn VectorStore>>,
    pub sqlite_pool: Option<sqlx::SqlitePool>,
    pub session_store: Option<Arc<SessionStore>>,
    /// SQLite persistence handle for auth sessions / device pairings. `None`
    /// when the deployment is non-sqlite (auth stays in-memory).
    pub auth_store: Option<Arc<AuthStore>>,
    pub feedback_store: Option<Arc<FeedbackStore>>,
    pub pending_badcase_store: Option<Arc<PendingBadcaseStore>>,
    pub decision_trace_store: Option<Arc<DecisionTraceStore>>,
    pub sample_store: Option<Arc<TurnSampleStore>>,
    pub audit_log: Arc<PersistentAuditLog>,
    pub audit_log_dyn: Arc<dyn AuditLogger>,
}

/// Initialize the storage adapter, shared SQLite pool, session store, and audit
/// log.
pub async fn init_storage(config: &GatewayConfig) -> crate::Result<StorageInit> {
    #[allow(clippy::type_complexity)]
    let (storage, unified_vector_store, sqlite_pool): (
        Arc<RwLock<dyn Storage>>,
        Option<Arc<dyn VectorStore>>,
        Option<sqlx::SqlitePool>,
    ) =
        match config.storage.storage_type.as_str() {
            "sqlite" => {
                let db_path = config
                    .storage
                    .database_url
                    .as_ref()
                    .map(|s| std::path::PathBuf::from(s.strip_prefix("sqlite:").unwrap_or(s)))
                    .unwrap_or_else(|| crate::dirs::syscity_dir().join("data").join("syscity.db"));
                if let Some(parent) = db_path.parent() {
                    if let Err(e) = tokio::fs::create_dir_all(parent).await {
                        warn!("Failed to create SQLite directory {:?}: {}", parent, e);
                    }
                }
                if !db_path.exists() {
                    if let Err(e) = tokio::fs::File::create(&db_path).await {
                        warn!("Failed to create SQLite file {:?}: {}", db_path, e);
                    }
                }
                let db_url = format!("sqlite:///{}", db_path.display());
                info!("Connecting to SQLite storage at: {}", db_url);
                let pool = sqlx::SqlitePool::connect(&db_url).await.map_err(|e| {
                    SyscityError::Storage {
                        context: "Failed to connect to SQLite".into(),
                        details: e.to_string(),
                    }
                })?;
                let sqlite_storage = SqliteStorage::new(pool.clone());
                #[cfg(feature = "sqlite-vec")]
                let vector_store: Option<Arc<dyn VectorStore>> =
                    match SqliteVecStore::new(&db_url, config.vector_memory.embedding_dimension)
                        .await
                    {
                        Ok(store) => {
                            info!("Using sqlite-vec vector store at {}", db_url);
                            Some(Arc::new(store))
                        }
                        Err(e) => {
                            warn!(
                                "Failed to initialize sqlite-vec vector store: {}. Falling back \
                                 to in-memory vector store.",
                                e
                            );
                            None
                        }
                    };
                #[cfg(not(feature = "sqlite-vec"))]
                let vector_store: Option<Arc<dyn VectorStore>> = None;
                let storage: Arc<RwLock<dyn Storage>> = Arc::new(RwLock::new(sqlite_storage));
                (storage, vector_store, Some(pool))
            }
            "file" => {
                let base_path = config.storage.base_path.as_deref().unwrap_or("./data");
                let storage = Arc::new(RwLock::new(FileStorage::new(base_path)?));
                (storage, None, None)
            }
            _ => {
                let storage = Arc::new(RwLock::new(InMemoryStorage::new()));
                (storage, None, None)
            }
        };

    let session_store: Option<Arc<SessionStore>> = if let Some(ref pool) = sqlite_pool {
        match SessionStore::from_pool(pool.clone()).await {
            Ok(store) => {
                info!("SessionStore initialized for persistent chat history");
                Some(Arc::new(store))
            }
            Err(e) => {
                warn!("Failed to initialize SessionStore: {}. Chat history will not persist.", e);
                None
            }
        }
    } else {
        None
    };

    // Auth sessions / device pairings share the same database as the rest of
    // the harness. Schema is created when the store is attached to the auth
    // manager / device pairing store at gateway startup.
    let auth_store: Option<Arc<AuthStore>> = sqlite_pool
        .as_ref()
        .map(|pool| Arc::new(AuthStore::new(pool.clone())));

    let audit_log: Arc<PersistentAuditLog> = if let Some(ref pool) = sqlite_pool {
        Arc::new(PersistentAuditLog::with_pool(pool.clone()))
    } else {
        Arc::new(PersistentAuditLog::new())
    };
    let audit_log_dyn: Arc<dyn AuditLogger> = audit_log.clone();

    // Feedback + pending-badcase stores share the same SQLite pool so all
    // harness tables live in one database.
    let feedback_store: Option<Arc<FeedbackStore>> = match sqlite_pool.as_ref() {
        Some(pool) => match FeedbackStore::from_pool(pool.clone()).await {
            Ok(store) => Some(Arc::new(store)),
            Err(e) => {
                warn!("Failed to initialize FeedbackStore: {}", e);
                None
            }
        },
        None => None,
    };
    let pending_badcase_store: Option<Arc<PendingBadcaseStore>> = match sqlite_pool.as_ref() {
        Some(pool) => match PendingBadcaseStore::from_pool(pool.clone()).await {
            Ok(store) => Some(Arc::new(store)),
            Err(e) => {
                warn!("Failed to initialize PendingBadcaseStore: {}", e);
                None
            }
        },
        None => None,
    };
    let decision_trace_store: Option<Arc<DecisionTraceStore>> = match sqlite_pool.as_ref() {
        Some(pool) => match DecisionTraceStore::from_pool(pool.clone()).await {
            Ok(store) => Some(Arc::new(store)),
            Err(e) => {
                warn!("Failed to initialize DecisionTraceStore: {}", e);
                None
            }
        },
        None => None,
    };
    let sample_store: Option<Arc<TurnSampleStore>> = match sqlite_pool.as_ref() {
        Some(pool) => match TurnSampleStore::from_pool(pool.clone()).await {
            Ok(store) => Some(Arc::new(store)),
            Err(e) => {
                warn!("Failed to initialize TurnSampleStore: {}", e);
                None
            }
        },
        None => None,
    };

    Ok(StorageInit {
        storage,
        unified_vector_store,
        sqlite_pool,
        session_store,
        auth_store,
        feedback_store,
        pending_badcase_store,
        decision_trace_store,
        sample_store,
        audit_log,
        audit_log_dyn,
    })
}
