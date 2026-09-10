//! Late service initialization.
//!
//! Initializes services that depend on storage or other early subsystems:
//! vector memory, session search (FTS5), hot reload, cron scheduler, task
//! scheduler, and side-effect context wiring.

use std::sync::Arc;

use tokio::sync::{mpsc, Mutex};
use tracing::{debug, info, warn};

use crate::gateway::EmbeddingProviderType;
use crate::gateway::GatewayConfig;
use crate::gateway::GatewayEvent;
use crate::gateway::GatewayState;
use crate::memory::query::HydeTransformer;
use crate::memory::vector::VectorMemoryService;
use crate::memory::{MemoryManager, MemoryManagerConfig, SessionSearch};
use crate::rag::multi_query::MultiQueryConfig as RagMultiQueryConfig;
#[cfg(feature = "local-embeddings")]
use crate::rag::LocalGgufEmbeddingProvider;
use crate::rag::{
    ApiEmbeddingProvider, CachedEmbeddingProvider, CohereReranker, ContextWindowConfig,
    EmbeddingConfig, MemoryVectorStore,
};

/// Initialize vector memory, session search, and memory manager.
pub async fn init_memory_services(
    config: &GatewayConfig,
    state: &Arc<GatewayState>,
    sqlite_pool: Option<&sqlx::SqlitePool>,
    unified_vector_store: Option<Arc<dyn crate::rag::VectorStore>>,
) -> crate::Result<()> {
    if config.vector_memory.enabled {
        info!("Initializing vector memory service...");

        let embedding_provider: Option<Arc<dyn crate::rag::EmbeddingProvider>> =
            match config.vector_memory.provider {
                crate::gateway::EmbeddingProviderType::OpenAi => {
                    if let Some(ref api_key) = config.vector_memory.embedding_api_key {
                        info!("Using OpenAI embedding provider");
                        let mut provider = ApiEmbeddingProvider::new(
                            api_key.clone(),
                            config.vector_memory.embedding_model.clone(),
                            config.vector_memory.embedding_dimension,
                        );
                        if let Some(ref base_url) = config.vector_memory.api_base_url {
                            provider = provider.with_base_url(base_url.clone());
                        }
                        Some(Arc::new(provider))
                    } else {
                        warn!("OpenAI embedding provider requires an API key");
                        None
                    }
                }
                crate::gateway::EmbeddingProviderType::LocalGguf => {
                    #[cfg(feature = "local-embeddings")]
                    {
                        if let Some(ref model_path) = config.vector_memory.local_model_path {
                            info!("Using local GGUF embedding provider");
                            use crate::rag::local_embeddings::ModelSource;
                            let source = ModelSource::parse(model_path);
                            let provider = LocalGgufEmbeddingProvider::create(
                                source,
                                config.vector_memory.embedding_dimension,
                            )
                            .await;
                            if provider.is_fts_only() {
                                if let Some(reason) = provider.fts_reason() {
                                    warn!("Local GGUF provider in FTS-only mode: {}", reason);
                                } else {
                                    info!(
                                        "Local GGUF provider initialized, will load model on \
                                         first use"
                                    );
                                }
                            } else {
                                info!("GGUF model configured from {}", model_path);
                            }
                            Some(Arc::new(provider))
                        } else {
                            warn!("Local GGUF provider requires 'local_model_path' configuration");
                            None
                        }
                    }
                    #[cfg(not(feature = "local-embeddings"))]
                    {
                        warn!(
                            "Local GGUF provider requires 'local-embeddings' feature. Build with: \
                             cargo build --features local-embeddings"
                        );
                        None
                    }
                }
            };

        if let Some(embedding_provider) = embedding_provider {
            let vector_store: Arc<dyn crate::rag::VectorStore> = match unified_vector_store {
                Some(store) => {
                    info!("Using unified SQLite storage for vector store");
                    store
                }
                None => {
                    info!(
                        "Using in-memory vector store (unified storage requires 'sqlite' storage \
                         type)"
                    );
                    Arc::new(MemoryVectorStore::new(config.vector_memory.embedding_dimension))
                }
            };

            let embedding_config = EmbeddingConfig {
                model: config.vector_memory.embedding_model.clone(),
                chunk_size: config.vector_memory.embedding.chunk_size,
                chunk_overlap: config.vector_memory.embedding.chunk_overlap,
                batch_size: config.vector_memory.embedding.batch_size,
                chunk_strategy: config.vector_memory.embedding.chunk_strategy.clone(),
            };

            let cached_provider = CachedEmbeddingProvider::new(embedding_provider, 1024);
            let mut service = VectorMemoryService::new(
                Arc::new(cached_provider),
                vector_store,
                &embedding_config,
            );

            // ── Query transformer (HyDE) ─────────────────────────────────────
            let qc = &config.vector_memory.query_transformer;
            if qc.enable_hyde {
                match state.infra.model_router.create_default_provider().await {
                    Ok(provider) => {
                        let mut hyde = HydeTransformer::new(provider);
                        if let Some(ref model) = qc.hyde_model {
                            hyde = hyde.with_model(model);
                        }
                        service = service.with_query_transformer(Arc::new(hyde));
                        info!("HyDE query transformer enabled");
                    }
                    Err(e) => {
                        warn!("Failed to create LLM provider for HyDE: {}", e);
                    }
                }
            }

            // ── Cross-encoder reranker (Cohere) ──────────────────────────────
            let rc = &config.vector_memory.reranker;
            if rc.enabled {
                if let Some(ref api_key) = rc.api_key {
                    let reranker = CohereReranker::new(api_key.clone())
                        .with_model(&rc.model)
                        .with_top_k(rc.top_k);
                    service = service.with_reranker(Arc::new(reranker));
                    info!("Cohere reranker enabled (model={}, top_k={})", rc.model, rc.top_k);
                } else {
                    warn!("Reranker enabled but no api_key configured");
                }
            }

            // ── Multi-Query expansion ───────────────────────────────────────
            let mqc = &config.vector_memory.multi_query;
            if mqc.enabled && mqc.num_variations > 0 {
                // Pin the expansion LLM deterministically when configured.
                // The unconfigured fallback (`create_default_provider`) grabs
                // an arbitrary registered provider — possibly a metered one.
                let mq_provider = match &mqc.provider {
                    Some(name) => match state.infra.model_router.get_provider(name).await {
                        Some(p) => Some(p),
                        None => {
                            warn!(
                                "Multi-Query provider '{}' not found under [providers.*] — \
                                     Multi-Query disabled",
                                name
                            );
                            None
                        }
                    },
                    None => state
                        .infra
                        .model_router
                        .create_default_provider()
                        .await
                        .ok(),
                };
                if let Some(provider) = mq_provider {
                    let rag_mq_config = RagMultiQueryConfig {
                        enabled: true,
                        num_variations: mqc.num_variations,
                        ..Default::default()
                    };
                    service = service.with_multi_query(provider, mqc.model.clone(), rag_mq_config);
                    info!(
                        "Multi-Query enabled with {} variations (provider={}, model={})",
                        mqc.num_variations,
                        mqc.provider.as_deref().unwrap_or("<default>"),
                        mqc.model.as_deref().unwrap_or("<provider default>")
                    );
                }
            }

            info!(
                "Vector memory service initialized with {:?} provider",
                config.vector_memory.provider
            );
            *state.memory.vector.write().await = Some(Arc::new(service));
        } else {
            warn!("Vector memory enabled but no suitable provider available");
        }
    } else {
        info!("Vector memory service disabled");
    }

    if let Some(pool) = sqlite_pool {
        info!("Initializing session search (FTS5)...");
        let session_search = Arc::new(SessionSearch::new(pool.clone()));
        if let Err(e) = session_search.initialize().await {
            warn!("Failed to initialize session search: {}", e);
        } else {
            info!("Session search (FTS5) initialized");
            let session_search_for_mm = session_search.clone();
            *state.memory.session_search.write().await = Some(session_search.clone());

            let mm_config = build_memory_manager_config(config);

            if let Some(vector_svc) = state.memory.vector.read().await.clone() {
                info!("Initializing MemoryManager with hybrid search...");
                let store = Arc::new(
                    crate::memory::UnifiedStore::new_with_pool(pool.clone())
                        .await
                        .map_err(|e| crate::error::SyscityError::Storage {
                            context: "Failed to create UnifiedStore".into(),
                            details: e.to_string(),
                        })?,
                );
                let mm = MemoryManager::new(store.clone(), store, mm_config)
                    .with_vector_service(vector_svc)
                    .with_session_search(session_search_for_mm);
                state.memory.manager.write().await.replace(Arc::new(mm));
                info!("MemoryManager with hybrid search initialized");
            } else {
                info!("Initializing MemoryManager (vector search disabled)...");
                let store = Arc::new(
                    crate::memory::UnifiedStore::new_with_pool(pool.clone())
                        .await
                        .map_err(|e| crate::error::SyscityError::Storage {
                            context: "Failed to create UnifiedStore".into(),
                            details: e.to_string(),
                        })?,
                );
                let mm = MemoryManager::new(store.clone(), store, mm_config)
                    .with_session_search(session_search_for_mm);
                state.memory.manager.write().await.replace(Arc::new(mm));
                info!("MemoryManager initialized (without vector search)");
            }
        }
    } else {
        info!("SQLite not in use; session search and hybrid memory disabled");
    }

    Ok(())
}

/// Build a `MemoryManagerConfig` from gateway config, reading context window
/// settings and forwarding them when enabled.
fn build_memory_manager_config(config: &GatewayConfig) -> MemoryManagerConfig {
    let mut mm_config = MemoryManagerConfig::default();

    if config.vector_memory.context_window.enabled {
        let cwc = &config.vector_memory.context_window;
        mm_config.context_window = Some(ContextWindowConfig {
            max_tokens: cwc.max_tokens,
            reserved_for_response: cwc.reserved_for_response,
            min_chunks: cwc.min_chunks,
        });
        info!(
            "Context window budgeting enabled: max_tokens={}, reserved={}, min_chunks={}",
            cwc.max_tokens, cwc.reserved_for_response, cwc.min_chunks
        );
    }

    mm_config
}

/// Initialize hot reload manager if enabled.
pub async fn init_hot_reload(
    config: &GatewayConfig,
    state: &Arc<GatewayState>,
) -> crate::Result<()> {
    if config.hot_reload.enabled {
        info!("Initializing hot reload manager...");
        let manager = Arc::new(crate::config::hot_reload::HotReloadManager::new()?);
        info!("Hot reload manager initialized");
        *state.infra.hot_reload.write().await = Some(manager);
    } else {
        info!("Hot reload disabled");
    }
    Ok(())
}

/// Initialize cron scheduler if enabled.
pub async fn init_cron(config: &GatewayConfig, state: &Arc<GatewayState>) -> crate::Result<()> {
    if config.cron.enabled {
        info!("Initializing advanced cron scheduler...");
        use crate::cron::cron::{AnnounceDelivery, CronScheduler};
        let (cron_scheduler, command_rx) = CronScheduler::new();
        let cron_scheduler =
            cron_scheduler.with_store_path(state.paths.cron_dir().join("jobs.json"));
        let cron_scheduler = Arc::new(Mutex::new(cron_scheduler));

        let (announce_tx, mut announce_rx) = mpsc::channel::<AnnounceDelivery>(64);
        let (schedule_change_tx, mut schedule_change_rx) =
            mpsc::channel::<Vec<(String, Option<i64>)>>(16);
        {
            let mut scheduler = cron_scheduler.lock().await;
            scheduler.set_announce_tx(announce_tx);
            scheduler.set_schedule_change_tx(schedule_change_tx);
        }
        let event_tx_announce = state.events.tx.clone();
        let announce_handle = tokio::spawn(async move {
            while let Some(delivery) = announce_rx.recv().await {
                info!("Cron announce → {}:{}", delivery.channel, delivery.to);
                match event_tx_announce.send(GatewayEvent::CronAnnounce {
                    channel: delivery.channel,
                    to: delivery.to,
                    message: delivery.message.clone(),
                }) {
                    Ok(receiver_count) => {
                        info!("Cron announce broadcast to {} receivers", receiver_count)
                    }
                    Err(e) => warn!("Failed to broadcast cron announce: {}", e),
                }
            }
        });
        state
            .task_registry
            .insert_join("cron:announce", announce_handle)
            .await;

        let cron_scheduler_clone = Arc::clone(&cron_scheduler);
        let scheduler_handle = tokio::spawn(async move {
            let mut scheduler = cron_scheduler_clone.lock().await;
            if let Err(e) = scheduler.start(command_rx).await {
                warn!("Advanced cron scheduler failed: {}", e);
            }
        });
        state
            .task_registry
            .insert_join("cron:scheduler", scheduler_handle)
            .await;
        // 4.3: forward schedule snapshots to a platform wake bridge when one
        // is present. Without a bridge (desktop) this task never spawns, so
        // the scheduler's `schedule_change_tx` simply stays undrained — zero
        // behaviour change.
        if let Some(bridge) = state.device.bridge.read().await.clone() {
            let cron_wake_handle = tokio::spawn(async move {
                while let Some(jobs) = schedule_change_rx.recv().await {
                    let payload = serde_json::json!({
                        "jobs": jobs.iter().map(|(id, at_ms)| serde_json::json!({
                            "id": id,
                            "at_ms": at_ms,
                        })).collect::<Vec<_>>(),
                    });
                    match bridge.call(crate::device::CMD_CRON_SYNC, payload).await {
                        Ok(_) => debug!("Cron schedule synced to platform wake"),
                        Err(e) => warn!("Failed to sync cron schedule to platform wake: {}", e),
                    }
                }
            });
            state
                .task_registry
                .insert_join("cron:wake", cron_wake_handle)
                .await;
        }

        *state.scheduler.cron_scheduler.write().await = Some(cron_scheduler.clone());
        info!("Advanced cron scheduler initialized");

        crate::tools::CronTool::set_scheduler(cron_scheduler);
    } else {
        info!("Cron scheduler disabled");
    }
    Ok(())
}

/// Initialize the recurring task scheduler.
pub async fn init_task_scheduler(state: &Arc<GatewayState>) -> crate::Result<()> {
    let mut task_scheduler = crate::planner::TaskScheduler::new();
    let state_for_scheduler = Arc::clone(state);
    let handler: crate::planner::scheduled_tasks::TaskHandler = Arc::new(move |task| {
        let state = Arc::clone(&state_for_scheduler);
        Box::pin(async move {
            let agents = state.agents.agents.read().await;
            if let Some(handle) = agents.values().next() {
                let msg = format!(
                    "[Scheduled] {}: {} - Actions: {}",
                    task.name,
                    task.description,
                    task.actions.len()
                );
                let incoming = crate::channels::IncomingMessage::new("scheduler", &task.id, &msg);
                if let Err(e) = handle.agent.process_message(incoming).await {
                    warn!("Scheduled task '{}' failed: {}", task.id, e);
                } else {
                    info!("Scheduled task '{}' processed successfully", task.id);
                }
            } else {
                warn!("No agent available to run scheduled task '{}'", task.id);
            }
        }) as std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>>
    });
    task_scheduler.start(handler).await?;
    *state.scheduler.task_scheduler.write().await = Some(Arc::new(Mutex::new(task_scheduler)));
    info!("TaskScheduler started");
    Ok(())
}

/// Wire side-effect executor with runtime context (memory + cron).
pub async fn init_side_effect_context(state: &Arc<GatewayState>) {
    let side_effect_ctx = crate::outbound::SideEffectContext {
        memory_manager: state.memory.manager.read().await.as_ref().cloned(),
        cron_scheduler: state.scheduler.cron_scheduler.read().await.clone(),
        webhook_client: Some(Arc::new(
            reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(10))
                .build()
                .unwrap_or_default(),
        )),
        task_registry: Some(state.task_registry.clone()),
    };
    state
        .pipelines
        .side_effect_executor
        .set_context(side_effect_ctx)
        .await;
    info!("SideEffectExecutor context wired");
}

/// Build a `KnowledgeBaseManager` from gateway config: pool connect, embedding
/// provider, vector store. Pure construction — no auto-ingest, no gate.
async fn build_kb_manager(
    config: &GatewayConfig,
    sqlite_pool: Option<&sqlx::SqlitePool>,
    paths: &crate::dirs::SyscityPaths,
) -> crate::Result<Arc<crate::rag::ingestion::KnowledgeBaseManager>> {
    let dimension = config.vector_memory.embedding_dimension;

    // Provider validation comes first: it's pure config/env, so an
    // unconfigured install fails fast without touching the filesystem.
    let provider: Arc<dyn crate::rag::EmbeddingProvider> = match config.vector_memory.provider {
        EmbeddingProviderType::OpenAi => {
            let api_key = config
                .vector_memory
                .embedding_api_key
                .clone()
                .or_else(|| std::env::var("OPENAI_API_KEY").ok())
                .or_else(|| std::env::var("SYSCITY_EMBEDDING_API_KEY").ok())
                .ok_or_else(|| {
                    crate::error::SyscityError::Validation(
                        "OpenAI API key required for KB embedding — set [vector_memory] \
                         embedding_api_key in config.toml (or OPENAI_API_KEY)"
                            .into(),
                    )
                })?;
            let mut ep = ApiEmbeddingProvider::new(
                api_key,
                config.vector_memory.embedding_model.clone(),
                dimension,
            );
            if let Some(ref url) = config.vector_memory.api_base_url {
                ep = ep.with_base_url(url.clone());
            }
            Arc::new(CachedEmbeddingProvider::new(
                Arc::new(ep) as Arc<dyn crate::rag::EmbeddingProvider>,
                1024,
            )) as Arc<dyn crate::rag::EmbeddingProvider>
        }
        EmbeddingProviderType::LocalGguf => {
            // KB also accepts the local GGUF model — the same provider the
            // vector memory service uses. Requires the `local-embeddings`
            // feature and an available model; anything else fails fast here
            // with an actionable message.
            #[cfg(feature = "local-embeddings")]
            {
                let model_path =
                    config
                        .vector_memory
                        .local_model_path
                        .clone()
                        .ok_or_else(|| {
                            crate::error::SyscityError::Validation(
                                "local_gguf embeddings require [vector_memory] \
                             local_model_path in config.toml"
                                    .into(),
                            )
                        })?;
                use crate::rag::local_embeddings::ModelSource;
                let source = ModelSource::parse(&model_path);
                let provider = LocalGgufEmbeddingProvider::create(source, dimension).await;
                if provider.is_fts_only() {
                    let reason = provider
                        .fts_reason()
                        .unwrap_or("local embedding model unavailable");
                    return Err(crate::error::SyscityError::Validation(format!(
                        "KB local_gguf embeddings unavailable: {reason} — set [vector_memory] \
                         provider = \"open_ai\" with an embedding_api_key instead"
                    )));
                }
                info!("KB using local GGUF embedding provider");
                Arc::new(CachedEmbeddingProvider::new(
                    Arc::new(provider) as Arc<dyn crate::rag::EmbeddingProvider>,
                    1024,
                )) as Arc<dyn crate::rag::EmbeddingProvider>
            }
            #[cfg(not(feature = "local-embeddings"))]
            {
                return Err(crate::error::SyscityError::Validation(
                    "KB requires API-based embeddings — set [vector_memory] \
                     provider = \"open_ai\" with embedding_api_key, or rebuild with \
                     the 'local-embeddings' feature"
                        .into(),
                ));
            }
        }
    };

    let pool = match sqlite_pool {
        Some(p) => p.clone(),
        None => {
            let db_path = paths.default_memory_db();
            sqlx::sqlite::SqlitePoolOptions::new()
                .max_connections(2)
                .connect(&format!("sqlite://{}", db_path.display()))
                .await
                .map_err(|e| crate::error::SyscityError::Storage {
                    context: "Failed to connect to KB database".into(),
                    details: e.to_string(),
                })?
        }
    };

    let vec_store: Arc<dyn crate::rag::VectorStore> = {
        let db_path = paths.default_memory_db();
        Arc::new(
            crate::rag::SqliteVecStore::new(&format!("sqlite://{}", db_path.display()), dimension)
                .await?,
        )
    };

    let embedding_config = EmbeddingConfig {
        model: config.vector_memory.embedding_model.clone(),
        chunk_size: config.vector_memory.embedding.chunk_size,
        chunk_overlap: config.vector_memory.embedding.chunk_overlap,
        batch_size: config.vector_memory.embedding.batch_size,
        chunk_strategy: config.vector_memory.embedding.chunk_strategy.clone(),
    };

    Ok(Arc::new(crate::rag::ingestion::KnowledgeBaseManager::new(
        provider,
        vec_store,
        pool,
        &embedding_config,
    )))
}

/// Fast-path/lazy accessor for the WS `kb.*` handlers: reuse the
/// daemon-initialized manager when present, otherwise build one on demand
/// (independent of the `auto_ingest_on_startup` gate). The error is a
/// human-readable reason (surfaced as `KB_NOT_CONFIGURED` / the UI's setup
/// hint) when the embedding provider isn't usable.
pub(crate) async fn ensure_kb_manager(
    config: &GatewayConfig,
    state: &Arc<GatewayState>,
) -> Result<Arc<crate::rag::ingestion::KnowledgeBaseManager>, String> {
    if let Some(m) = state.memory.kb_manager.read().await.clone() {
        return Ok(m);
    }
    let manager = build_kb_manager(config, None, &state.paths)
        .await
        .map_err(|e| e.to_string())?;
    *state.memory.kb_manager.write().await = Some(manager.clone());
    Ok(manager)
}

/// Initialize the Knowledge Base manager for daemon-side auto-ingest.
pub async fn init_kb_manager(
    config: &GatewayConfig,
    state: &Arc<GatewayState>,
    sqlite_pool: Option<&sqlx::SqlitePool>,
    _unified_vector_store: Option<Arc<dyn crate::rag::VectorStore>>,
) -> crate::Result<()> {
    if !config.knowledge_base.auto_ingest_on_startup {
        info!("KB auto-ingest disabled");
        return Ok(());
    }

    info!("Initializing Knowledge Base manager...");

    let manager = build_kb_manager(config, sqlite_pool, &state.paths).await?;

    *state.memory.kb_manager.write().await = Some(manager.clone());

    // Auto-ingest stale/new documents on startup
    info!("KB: auto-ingesting stale/new documents...");
    let agents_dir = state.paths.agents_dir();
    if agents_dir.exists() {
        let mut read_dir = tokio::fs::read_dir(&agents_dir).await?;
        while let Some(entry) = read_dir.next_entry().await? {
            let path = entry.path();
            if !path.is_dir() {
                continue;
            }
            let agent_id = match path.file_name().and_then(|n| n.to_str()) {
                Some(id) => id.to_string(),
                None => continue,
            };
            let kb_toml = path.join("kb.toml");
            if !kb_toml.exists() {
                continue;
            }
            info!("KB: auto-ingesting '{}'...", agent_id);
            let report = manager.ingest_agent(&agent_id, false).await;
            info!(
                "KB: '{}' ingested: {} indexed, {} skipped, {} errors in {:?}",
                agent_id,
                report.docs_indexed,
                report.docs_skipped,
                report.errors.len(),
                report.duration,
            );
        }
    }

    info!("KB manager initialized with auto-ingest complete");
    Ok(())
}

/// Convenience helper that initializes all late services in dependency order.
pub async fn init_late_services(
    config: &GatewayConfig,
    state: &Arc<GatewayState>,
    sqlite_pool: Option<&sqlx::SqlitePool>,
    unified_vector_store: Option<Arc<dyn crate::rag::VectorStore>>,
) -> crate::Result<()> {
    init_memory_services(config, state, sqlite_pool, unified_vector_store.clone()).await?;
    init_kb_manager(config, state, sqlite_pool, unified_vector_store).await?;
    init_hot_reload(config, state).await?;
    init_cron(config, state).await?;
    init_task_scheduler(state).await?;
    init_side_effect_context(state).await;
    Ok(())
}
