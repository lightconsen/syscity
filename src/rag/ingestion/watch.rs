//! File watcher for Knowledge Base auto-ingestion.
//!
//! Watches agent KB source files and `kb.toml` for changes, debouncing
//! rapid events (editor saves) and issuing targeted re-ingest commands.
//! Used by the `syscity kb watch` CLI command and potentially by the
//! daemon for background watching.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

use notify::{EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use tokio::sync::mpsc;
use tracing::{debug, info, warn};

use crate::dirs::SyscityPaths;
use crate::rag::ingestion::loader::{load_kb_config, KnowledgeSource, SourceType};

/// An event emitted by the KB watcher.
#[derive(Debug, Clone)]
pub enum KbWatchEvent {
    /// A source file changed — re-ingest this file for this agent.
    SourceFileChanged { agent: String, source_path: PathBuf },
    /// `kb.toml` changed — re-load config and re-ingest added/changed sources.
    KbTomlChanged { agent: String },
}

/// Internal state for a watched agent.
#[allow(dead_code)]
struct WatchedAgent {
    agent_id: String,
    agent_dir: PathBuf,
}

/// Manages file-system watching for KB source files across one or more agents.
///
/// Uses raw `notify::RecommendedWatcher` with a manual 500 ms debounce to
/// avoid reacting to intermediate editor saves.
///
/// # CLI example
///
/// ```ignore
/// let mut watcher = KbWatcher::new()?;
/// watcher.add_agent("sre")?;
/// while let Some(event) = watcher.event_rx.recv().await {
///     handle_event(event).await;
/// }
/// ```
pub struct KbWatcher {
    /// The notify watcher (wrapped in `Option` so we can drop it in `Drop`).
    pub watcher: Option<RecommendedWatcher>,
    /// Sender side exposed so external code can inject synthetic events.
    pub event_tx: mpsc::Sender<KbWatchEvent>,
    /// Receiver for ingestion events.
    pub event_rx: mpsc::Receiver<KbWatchEvent>,
    /// Watched agents (for potential future re-scan).
    _agents: HashMap<String, WatchedAgent>,
    /// Layout root the watched agent directories resolve against.
    paths: Arc<SyscityPaths>,
}

impl KbWatcher {
    /// Create a new KB watcher.
    ///
    /// Opens the underlying `RecommendedWatcher` and sets up the event
    /// channel. Returns an error if the OS-level watcher cannot be created.
    pub fn new(paths: Arc<SyscityPaths>) -> crate::Result<Self> {
        let (event_tx, event_rx) = mpsc::channel(256);
        let inner_event_tx = event_tx.clone();
        let debounce_map: Arc<std::sync::Mutex<HashMap<PathBuf, Instant>>> =
            Arc::new(std::sync::Mutex::new(HashMap::new()));
        let debounce_map_clone = debounce_map.clone();

        let agents_dir = paths.agents_dir();
        let watcher: RecommendedWatcher = notify::recommended_watcher(
            move |res: notify::Result<notify::Event>| {
                let debounce = debounce_map_clone.clone();
                let tx = inner_event_tx.clone();
                let agents_dir = agents_dir.clone();
                if let Ok(event) = res {
                    let now = Instant::now();

                    // Debounce: skip if same path seen within 500 ms
                    let mut any_new = false;
                    {
                        let mut map = match debounce.lock() {
                            Ok(m) => m,
                            Err(_) => return,
                        };
                        for path in &event.paths {
                            let should_emit = !matches!(map.get(path), Some(last) if now.duration_since(*last).as_millis() < 500);
                            if should_emit {
                                map.insert(path.clone(), now);
                                any_new = true;
                            }
                        }
                    }
                    if !any_new {
                        return;
                    }

                    // Only react to Modify / Create / Remove events
                    let is_relevant = matches!(
                        event.kind,
                        EventKind::Modify(_) | EventKind::Create(_) | EventKind::Remove(_)
                    );
                    if !is_relevant {
                        return;
                    }

                    for path in &event.paths {
                        // Determine which agent this path belongs to
                        if let Ok(rel) = path.strip_prefix(&agents_dir) {
                            let components: Vec<_> = rel.components().collect();
                            if components.len() >= 2 {
                                let agent = components[0]
                                    .as_os_str()
                                    .to_string_lossy()
                                    .to_string();
                                let rel_path: PathBuf = components[1..].iter().collect();

                                // Check if it's kb.toml
                                if rel_path == Path::new("kb.toml") {
                                    let _ = tx.try_send(KbWatchEvent::KbTomlChanged {
                                        agent,
                                    });
                                } else if matches!(
                                    event.kind,
                                    EventKind::Modify(_) | EventKind::Create(_)
                                ) {
                                    // Source file changed
                                    let _ = tx.try_send(KbWatchEvent::SourceFileChanged {
                                        agent,
                                        source_path: path.clone(),
                                    });
                                }
                            }
                        }
                    }
                }
            },
        )
        .map_err(|e| {
            crate::error::SyscityError::Internal(format!("Failed to create KB watcher: {}", e))
        })?;

        Ok(Self {
            watcher: Some(watcher),
            event_tx,
            event_rx,
            _agents: HashMap::new(),
            paths,
        })
    }

    /// Start watching a specific agent's KB directory.
    ///
    /// Watches `kb.toml`, all source files referenced in `kb.toml`, and the
    /// agent directory itself (non-recursive) so new files are also caught.
    pub fn add_agent(&mut self, agent_id: &str) -> crate::Result<()> {
        let agent_dir = self.paths.agent_dir(agent_id);
        if !agent_dir.exists() {
            return Err(crate::error::SyscityError::Validation(format!(
                "Agent directory not found: {}",
                agent_dir.display()
            )));
        }

        let watcher = self.watcher.as_mut().ok_or_else(|| {
            crate::error::SyscityError::Internal("KbWatcher not initialized".into())
        })?;

        // Watch kb.toml
        let kb_toml = agent_dir.join("kb.toml");
        if kb_toml.exists() {
            watcher
                .watch(&kb_toml, RecursiveMode::NonRecursive)
                .map_err(|e| {
                    crate::error::SyscityError::Internal(format!(
                        "Failed to watch kb.toml for '{}': {}",
                        agent_id, e
                    ))
                })?;
            info!("Watching kb.toml for agent '{}'", agent_id);
        }

        // Watch source directories referenced in kb.toml
        if let Some(sources) = load_kb_config(&agent_dir) {
            for source in &sources {
                let paths = source_paths_for_watch(source, &agent_dir);
                for p in paths {
                    if p.exists() {
                        let _ = watcher.watch(&p, RecursiveMode::NonRecursive);
                        debug!("Watching source path {:?} for agent '{}'", p, agent_id);
                    }
                }
            }
        }

        // Watch the agent directory so we catch new files
        watcher
            .watch(&agent_dir, RecursiveMode::NonRecursive)
            .map_err(|e| {
                crate::error::SyscityError::Internal(format!(
                    "Failed to watch agent directory '{}': {}",
                    agent_id, e
                ))
            })?;

        self._agents.insert(
            agent_id.to_string(),
            WatchedAgent {
                agent_id: agent_id.to_string(),
                agent_dir,
            },
        );

        info!("KB watcher started for agent '{}'", agent_id);
        Ok(())
    }

    /// Watch ALL agents that have `kb.toml` files.
    ///
    /// Returns the list of agent IDs that were successfully added.
    pub fn add_all_agents(&mut self) -> crate::Result<Vec<String>> {
        let agents_dir = self.paths.agents_dir();
        if !agents_dir.exists() {
            return Ok(Vec::new());
        }

        let mut agent_ids = Vec::new();
        let read_dir = match std::fs::read_dir(&agents_dir) {
            Ok(d) => d,
            Err(e) => return Err(crate::error::SyscityError::Io(e)),
        };

        for entry in read_dir.flatten() {
            let path = entry.path();
            if path.is_dir() {
                if let Some(agent_id) = path.file_name().and_then(|n| n.to_str()) {
                    let kb_toml = path.join("kb.toml");
                    if kb_toml.exists() {
                        if let Err(e) = self.add_agent(agent_id) {
                            warn!("Failed to watch agent '{}': {}", agent_id, e);
                        } else {
                            agent_ids.push(agent_id.to_string());
                        }
                    }
                }
            }
        }

        Ok(agent_ids)
    }
}

/// Resolve the filesystem paths that a `KnowledgeSource` refers to, for
/// watching.
pub(crate) fn source_paths_for_watch(source: &KnowledgeSource, agent_dir: &Path) -> Vec<PathBuf> {
    match &source.source_type {
        SourceType::File { path } => {
            let p = Path::new(path);
            if p.is_absolute() {
                vec![p.to_path_buf()]
            } else {
                vec![agent_dir.join(p)]
            }
        }
        SourceType::Dir { path } => {
            let p = Path::new(path);
            let full = if p.is_absolute() {
                p.to_path_buf()
            } else {
                agent_dir.join(p)
            };
            // Watch the directory (not recursive — notify handles subdirs)
            vec![full]
        }
        SourceType::Glob { pattern } => {
            // Watch the agent directory for glob-based sources
            let _ = pattern;
            vec![agent_dir.to_path_buf()]
        }
        SourceType::Url { .. } => {
            // URLs can't be filesystem-watched
            vec![]
        }
    }
}

/// Check if a file path is covered by a `KnowledgeSource`.
///
/// Used by the watcher to determine which `KnowledgeSource` in `kb.toml`
/// matches a changed file path.
pub fn source_matches_path(
    source: &KnowledgeSource,
    changed_path: &Path,
    agent_dir: &Path,
) -> bool {
    match &source.source_type {
        SourceType::File { path } => {
            let full = Path::new(path);
            let full = if full.is_absolute() {
                full.to_path_buf()
            } else {
                agent_dir.join(full)
            };
            full == changed_path
        }
        SourceType::Dir { path } => {
            let full = Path::new(path);
            let full = if full.is_absolute() {
                full.to_path_buf()
            } else {
                agent_dir.join(full)
            };
            changed_path.starts_with(&full)
        }
        SourceType::Glob { pattern } => {
            if let Ok(rel) = changed_path.strip_prefix(agent_dir) {
                crate::rag::ingestion::loader::glob_match(pattern, &rel.to_string_lossy())
            } else {
                false
            }
        }
        SourceType::Url { .. } => {
            // URLs don't correspond to local filesystem paths
            false
        }
    }
}
