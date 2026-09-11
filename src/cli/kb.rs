//! Knowledge Base management commands for Syscity CLI.
//!
//! Provides CLI access to KB ingestion: ingest agent documents, list indexed
//! documents, and delete collections.

use std::sync::Arc;

use clap::Subcommand;

use crate::error::Result;
use crate::rag::embedding::EmbeddingProvider;
use crate::rag::ingestion::{KnowledgeBaseManager, KnowledgeSource, SourceType};
use crate::rag::VectorStore;

/// Knowledge Base subcommands.
#[derive(Debug, Subcommand)]
pub enum KbCommands {
    /// Ingest documents for an agent's knowledge base.
    Ingest {
        /// Agent ID (e.g., "sre", "coder")
        agent: String,
        /// Force re-indexing of all documents (alias: --rebuild)
        #[arg(short, long)]
        force: bool,
        /// Ad-hoc single file or URL to ingest (bypasses kb.toml)
        #[arg(short, long)]
        source: Option<String>,
    },
    /// List indexed documents.
    List {
        /// Agent ID (e.g., "sre", "coder"). Omit to show all collections.
        agent: Option<String>,
        /// Filter by status: indexed, failed, stale
        #[arg(short, long)]
        status: Option<String>,
    },
    /// Delete indexed documents or entire collections.
    Delete {
        /// Agent ID (alternative to --collection)
        agent: Option<String>,
        /// Collection name (alternative to --agent). E.g. "kb-sre"
        #[arg(short, long)]
        collection: Option<String>,
        /// Specific document ID to delete. If omitted, deletes all.
        #[arg(short, long)]
        doc: Option<String>,
    },
    /// Show knowledge base health for a collection.
    Status {
        /// Agent ID (e.g., "sre", "coder")
        agent: String,
    },
    /// Watch KB source files and auto-re-ingest on change.
    Watch {
        /// Agent ID to watch (e.g., "sre", "coder"). Omit to watch all.
        agent: Option<String>,
        /// Run as a background daemon task (requires gateway).
        #[arg(long)]
        daemon: bool,
    },
}

/// Run a KB command.
pub async fn run_kb_command(command: &KbCommands) -> Result<()> {
    match command {
        KbCommands::Ingest { agent, force, source } => {
            cmd_ingest(agent, *force, source.as_deref()).await
        }
        KbCommands::List { agent, status } => cmd_list(agent.as_deref(), status.as_deref()).await,
        KbCommands::Delete { agent, collection, doc } => {
            cmd_delete(agent.as_deref(), collection.as_deref(), doc.as_deref()).await
        }
        KbCommands::Status { agent } => cmd_status(agent).await,
        KbCommands::Watch { agent, daemon } => cmd_watch(agent.as_deref(), *daemon).await,
    }
}

/// Create a KnowledgeBaseManager with embedding provider and vector store.
async fn create_kb_manager() -> Result<KnowledgeBaseManager> {
    use sqlx::sqlite::SqlitePoolOptions;

    // ── Embedding provider config ──────────────────────────────────────────
    let api_key = std::env::var("OPENAI_API_KEY")
        .or_else(|_| std::env::var("SYSCITY_EMBEDDING_API_KEY"))
        .map_err(|_| {
            crate::error::SyscityError::Validation(
                "OPENAI_API_KEY or SYSCITY_EMBEDDING_API_KEY must be set".to_string(),
            )
        })?;
    let model = std::env::var("SYSCITY_EMBEDDING_MODEL")
        .unwrap_or_else(|_| "text-embedding-3-small".to_string());
    let dimension: usize = std::env::var("SYSCITY_EMBEDDING_DIMENSION")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(1536);
    let base_url = std::env::var("SYSCITY_EMBEDDING_BASE_URL").ok();

    // ── Database ───────────────────────────────────────────────────────────
    let db_path = crate::dirs::default_memory_db();
    let pool = SqlitePoolOptions::new()
        .max_connections(2)
        .connect(&format!("sqlite://{}", db_path.display()))
        .await
        .map_err(|e| crate::error::SyscityError::Storage {
            context: "Failed to connect to database".to_string(),
            details: e.to_string(),
        })?;

    // ── Embedding provider ────────────────────────────────────────────────
    let mut provider =
        crate::rag::ApiEmbeddingProvider::new(api_key.clone(), model.clone(), dimension);
    if let Some(url) = &base_url {
        provider = provider.with_base_url(url.clone());
    }
    let provider_arc: Arc<dyn EmbeddingProvider> = Arc::new(provider);
    let embedding_provider: Arc<dyn EmbeddingProvider> =
        Arc::new(crate::rag::CachedEmbeddingProvider::new(provider_arc, 1024));

    // ── Vector store ──────────────────────────────────────────────────────
    let vec_store: Arc<dyn VectorStore> = Arc::new(
        crate::rag::SqliteVecStore::new(&format!("sqlite://{}", db_path.display()), dimension)
            .await?,
    );

    let config = crate::rag::EmbeddingConfig {
        model,
        chunk_size: 512,
        chunk_overlap: 50,
        batch_size: 32,
        chunk_strategy: Default::default(),
    };

    Ok(KnowledgeBaseManager::new(embedding_provider, vec_store, pool, &config))
}

/// Handle `kb ingest` command.
async fn cmd_ingest(agent: &str, force: bool, source: Option<&str>) -> Result<()> {
    let manager = create_kb_manager().await?;

    let report = if let Some(source_str) = source {
        // Ad-hoc single source — bypass kb.toml
        let collection = KnowledgeBaseManager::collection_name(agent);
        let agent_dir = crate::dirs::agent_dir(agent);
        let is_url = source_str.starts_with("http://") || source_str.starts_with("https://");
        let source_type = if is_url {
            SourceType::Url { url: source_str.to_string() }
        } else {
            SourceType::File { path: source_str.to_string() }
        };
        let kb_source = KnowledgeSource {
            id: None,
            name: source_str.to_string(),
            source_type,
            pattern: None,
            collection: None,
            chunk_strategy: None,
        };
        manager
            .ingest_source(&kb_source, &collection, &agent_dir, force)
            .await
    } else {
        manager.ingest_agent(agent, force).await
    };

    println!("Knowledge Base ingestion report for '{}':", agent);
    println!("  Sources:       {}", report.total_sources);
    println!("  Docs found:    {}", report.docs_found);
    println!("  Docs indexed:  {}", report.docs_indexed);
    println!("  Docs skipped:  {}", report.docs_skipped);
    println!("  Total chunks:  {}", report.total_chunks);
    println!("  Duration:      {:?}", report.duration);

    if !report.errors.is_empty() {
        println!("\n  Errors ({}):", report.errors.len());
        for err in &report.errors {
            println!("    - {}", err);
        }
    }

    Ok(())
}

/// Handle `kb list` command.
async fn cmd_list(agent: Option<&str>, status: Option<&str>) -> Result<()> {
    let manager = create_kb_manager().await?;

    use crate::rag::ingestion::IngestionStatus;
    let filter = status.map(|s| match s.to_lowercase().as_str() {
        "failed" => IngestionStatus::Failed,
        "stale" => IngestionStatus::Stale,
        _ => IngestionStatus::Indexed,
    });

    if let Some(agent_id) = agent {
        // Single agent/collection
        let collection = KnowledgeBaseManager::collection_name(agent_id);
        let records = manager.list(Some(&collection), filter).await?;
        let stats = manager.stats(&collection).await?;

        println!("Collection: {} ({})", collection, agent_id);
        println!("  Total docs:     {}", stats.total_docs);
        println!("  Total chunks:   {}", stats.total_chunks);
        if let Some(ref last) = stats.last_indexed_at {
            println!("  Last indexed:   {}", last);
        }
        println!("  Stale:          {}", stats.stale_count);
        println!("  Failed:         {}", stats.failed_count);
        println!();

        if records.is_empty() {
            println!("  No documents found.");
        } else {
            println!("  Documents:");
            for rec in &records {
                let status_str = match rec.status {
                    IngestionStatus::Indexed => "OK",
                    IngestionStatus::Failed => "FAIL",
                    IngestionStatus::Stale => "STALE",
                };
                println!(
                    "    [{:5}] {} ({} chunks, {})",
                    status_str, rec.doc_id, rec.chunk_count, rec.indexed_at
                );
            }
        }
    } else {
        // List all collections
        let collections = manager.list_collections().await?;
        if collections.is_empty() {
            println!("  No collections found.");
        } else {
            println!(
                "  {:<20} {:>6} {:>8} {:>6} {:>6}  Last Indexed",
                "Collection", "Docs", "Chunks", "Stale", "Failed"
            );
            println!("  {}", "-".repeat(70));
            for c in &collections {
                let last = c.last_indexed_at.as_deref().unwrap_or("-");
                println!(
                    "  {:<20} {:>6} {:>8} {:>6} {:>6}  {}",
                    c.collection, c.total_docs, c.total_chunks, c.stale_count, c.failed_count, last
                );
            }
        }
    }

    Ok(())
}

/// Handle `kb delete` command.
async fn cmd_delete(
    agent: Option<&str>,
    collection: Option<&str>,
    doc: Option<&str>,
) -> Result<()> {
    let collection = match collection {
        Some(c) => c.to_string(),
        None => {
            let agent = agent.ok_or_else(|| {
                crate::error::SyscityError::Validation(
                    "Either --agent or --collection must be provided".to_string(),
                )
            })?;
            KnowledgeBaseManager::collection_name(agent)
        }
    };

    let manager = create_kb_manager().await?;
    let report = manager.delete(&collection, doc).await?;
    println!("Deleted {} chunks from '{}'", report.chunks_deleted, report.collection);
    if let Some(did) = &report.doc_id {
        println!("  Document: {}", did);
    }

    Ok(())
}

/// Handle `kb status` command.
async fn cmd_status(agent: &str) -> Result<()> {
    let manager = create_kb_manager().await?;
    let collection = KnowledgeBaseManager::collection_name(agent);
    let stats = manager.stats(&collection).await?;

    println!("Knowledge Base status for '{}':", agent);
    println!("  Collection:     {}", collection);
    println!("  Total docs:     {}", stats.total_docs);
    println!("  Total chunks:   {}", stats.total_chunks);
    if let Some(ref last) = stats.last_indexed_at {
        println!("  Last indexed:   {}", last);
    }
    println!("  Stale:          {}", stats.stale_count);
    println!("  Failed:         {}", stats.failed_count);

    if stats.stale_count > 0 || stats.failed_count > 0 {
        println!("\n  Issues found: {}", stats.stale_count + stats.failed_count);
        if stats.stale_count > 0 {
            println!("    - {} stale document(s) — re-ingest recommended", stats.stale_count);
        }
        if stats.failed_count > 0 {
            println!("    - {} failed document(s)", stats.failed_count);
        }
    } else {
        println!("\n  All documents indexed successfully.");
    }

    Ok(())
}

/// Handle `kb watch` command.
async fn cmd_watch(agent: Option<&str>, daemon: bool) -> Result<()> {
    if daemon {
        return cmd_watch_daemon(agent).await;
    }
    cmd_watch_foreground(agent).await
}

/// Handle foreground `kb watch`.
async fn cmd_watch_foreground(agent: Option<&str>) -> Result<()> {
    let manager = create_kb_manager().await?;
    let manager = Arc::new(manager);

    let mut watcher = crate::rag::ingestion::watch::KbWatcher::new(crate::dirs::paths())?;

    let agents: Vec<String> = if let Some(a) = agent {
        watcher.add_agent(a)?;
        vec![a.to_string()]
    } else {
        watcher.add_all_agents()?
    };

    if agents.is_empty() {
        println!("No agents with kb.toml found to watch.");
        return Ok(());
    }

    println!("Watching {} agent(s) for KB changes:", agents.len());
    for a in &agents {
        println!("  - {}", a);
    }
    println!("Press Ctrl+C to stop.");

    let mut rx = watcher.event_rx;

    // Handle Ctrl+C
    let shutdown = Arc::new(tokio::sync::Notify::new());
    let shutdown_clone = shutdown.clone();
    tokio::spawn(async move {
        let _ = tokio::signal::ctrl_c().await;
        shutdown_clone.notify_one();
    });

    loop {
        tokio::select! {
            Some(event) = rx.recv() => {
                match event {
                    crate::rag::ingestion::watch::KbWatchEvent::SourceFileChanged { agent, source_path } => {
                        println!("[{}] Source changed: {}", agent, source_path.display());
                        let agent_dir = crate::dirs::agent_dir(&agent);
                        let collection = KnowledgeBaseManager::collection_name(&agent);

                        if let Some(sources) = crate::rag::ingestion::load_kb_config(&agent_dir) {
                            for source in &sources {
                                if crate::rag::ingestion::watch::source_matches_path(
                                    source, &source_path, &agent_dir,
                                ) {
                                    let report = manager
                                        .ingest_source(source, &collection, &agent_dir, false)
                                        .await;
                                    println!(
                                        "  Re-ingested: {} indexed, {} skipped, {} errors",
                                        report.docs_indexed, report.docs_skipped, report.errors.len(),
                                    );
                                }
                            }
                        }
                    }
                    crate::rag::ingestion::watch::KbWatchEvent::KbTomlChanged { agent } => {
                        println!("[{}] kb.toml changed — re-loading sources and re-ingesting", agent);
                        let agent_dir = crate::dirs::agent_dir(&agent);
                        let report = manager.ingest_agent(&agent, false).await;
                        println!(
                            "  Re-ingested: {} indexed, {} skipped, {} errors",
                            report.docs_indexed, report.docs_skipped, report.errors.len(),
                        );

                        // Re-scan kb.toml and register new watch paths
                        if let Some(ref mut watcher_ref) = watcher.watcher {
                            use notify::{RecursiveMode, Watcher};
                            if let Some(sources) = crate::rag::ingestion::load_kb_config(&agent_dir) {
                                for source in &sources {
                                    for p in crate::rag::ingestion::watch::source_paths_for_watch(source, &agent_dir) {
                                        if p.exists() {
                                            let _ = watcher_ref.watch(&p, RecursiveMode::NonRecursive);
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
            _ = shutdown.notified() => {
                println!("\nStopping KB watcher...");
                break;
            }
        }
    }

    Ok(())
}

/// Handle daemon `kb watch --daemon`.
async fn cmd_watch_daemon(agent: Option<&str>) -> Result<()> {
    let _ = agent;
    println!("KB watcher daemon mode: connecting to running daemon...");
    println!();
    println!("To enable KB watching in daemon mode, set:");
    println!("  [knowledge_base]");
    println!("  auto_ingest_on_startup = true");
    println!("in your config.toml and restart the daemon.");
    println!();
    println!("Or use `syscity kb watch` (without --daemon) for foreground mode.");
    Ok(())
}
