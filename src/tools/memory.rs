//! Memory tools for Syscity
//!
//! Provides three distinct tools:
//!
//! * [`MemoryTool`] — legacy combined store/retrieve/search/list/delete/update
//! * [`MemorySearchTool`] — hybrid vector + FTS5 semantic search with optional
//!   storage
//! * [`MemoryGetTool`] — exact lookup and CRUD mutations (store / retrieve /
//!   delete / update)

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::Value;
use tracing::{debug, info};

use super::{create_schema, Tool, ToolContext, ToolExecutionResult};
use crate::memory::{
    Memory, MemoryEntryType, MemoryId, MemoryManager, MemoryQuery, MemoryStore, SqliteMemoryStore,
};
use crate::tools::sdk::ToolCapabilities;

/// Memory tool for storing and retrieving information
#[derive(Debug, Clone)]
pub struct MemoryTool {
    /// SQLite storage backend
    storage: Arc<SqliteMemoryStore>,
}

impl MemoryTool {
    /// Create a new memory tool with SQLite storage rooted at `paths`.
    pub async fn new(paths: &crate::dirs::SyscityPaths) -> crate::Result<Self> {
        let db_path = paths.default_memory_db();
        let db_url = format!("sqlite:///{}", db_path.display());

        info!("Initializing memory tool with database: {}", db_path.display());

        let storage = Arc::new(SqliteMemoryStore::new(&db_url).await?);

        Ok(Self { storage })
    }

    /// Create with custom database URL (for testing)
    pub async fn with_database_url(database_url: &str) -> crate::Result<Self> {
        let storage = Arc::new(SqliteMemoryStore::new(database_url).await?);
        Ok(Self { storage })
    }

    /// Create with an existing store (for sharing with the agent)
    pub async fn with_store(storage: Arc<SqliteMemoryStore>) -> crate::Result<Self> {
        Ok(Self { storage })
    }

    /// Search for relevant memories to inject into context
    pub async fn search_relevant(
        &self,
        query: &str,
        user_id: &str,
        limit: usize,
    ) -> crate::Result<Vec<Memory>> {
        let memory_query = MemoryQuery::new()
            .for_user(user_id)
            .with_content(query)
            .limit(limit);

        let results = self.storage.search(memory_query).await?;
        Ok(results)
    }

    /// Get recent memories for a user
    pub async fn get_recent(&self, user_id: &str, limit: usize) -> crate::Result<Vec<Memory>> {
        let memory_query = MemoryQuery::new().for_user(user_id).limit(limit);

        let results = self.storage.search(memory_query).await?;
        Ok(results)
    }

    /// Format memories for injection into system prompt
    pub fn format_memories_for_prompt(memories: &[Memory]) -> String {
        if memories.is_empty() {
            return String::new();
        }

        let mut sections = vec!["### Relevant Memories".to_string()];

        for mem in memories.iter().take(5) {
            let mem_type = &mem.memory_type;
            let content = &mem.content;
            sections.push(format!("- [{}] {}", mem_type, content));
        }

        sections.join("\n")
    }
}

#[async_trait]
impl Tool for MemoryTool {
    fn name(&self) -> &str {
        "memory"
    }

    fn description(&self) -> &str {
        r#"Store and retrieve memories, facts, and context that persist across conversations.

This tool saves information to a SQLite database that persists across restarts.
Use this to:
- Remember important information about the user
- Store facts for later retrieval
- Keep track of preferences
- Save context from previous conversations
- Build a knowledge base over time

Categories: 'user' (user preferences), 'fact' (general facts), 'context' (conversation context),
'task' (task-related), 'project' (project-specific)

Memories are automatically searched and relevant ones injected into new conversations."#
    }

    fn parameters_schema(&self) -> Value {
        create_schema(
            "Memory operations",
            serde_json::json!({
                "action": {
                    "type": "string",
                    "enum": ["store", "retrieve", "search", "list", "delete", "update"],
                    "description": "The memory operation to perform"
                },
                "content": {
                    "type": "string",
                    "description": "Content to store (for 'store' action)"
                },
                "id": {
                    "type": "string",
                    "description": "Memory ID (for 'retrieve', 'delete', 'update')"
                },
                "category": {
                    "type": "string",
                    "description": "Category for the memory (user, fact, context, task, project)"
                },
                "query": {
                    "type": "string",
                    "description": "Search query (for 'search' action)"
                },
                "limit": {
                    "type": "integer",
                    "description": "Maximum number of results",
                    "default": 10
                }
            }),
            vec!["action"],
        )
    }

    fn capabilities(&self) -> ToolCapabilities {
        ToolCapabilities {
            requires_approval: false,
            risk_level: crate::tools::approval::RiskLevel::Medium,
            categories: vec!["memory".to_string()],
            ..Default::default()
        }
    }

    async fn execute(
        &self,
        args: Value,
        context: &ToolContext,
    ) -> crate::Result<ToolExecutionResult> {
        let action = args["action"].as_str().ok_or_else(|| {
            crate::error::SyscityError::Validation("Missing 'action' argument".to_string())
        })?;

        match action {
            "store" => {
                let content = args["content"].as_str().ok_or_else(|| {
                    crate::error::SyscityError::Validation("Missing 'content' argument".to_string())
                })?;

                let memory_type = args["category"].as_str().unwrap_or("fact");

                let memory = Memory::new(
                    context.user_id.clone(),
                    content.to_string(),
                    memory_type.to_string(),
                )
                .with_conversation(context.conversation_id.clone());

                let memory_id = self.storage.store(memory).await?;
                info!("Stored memory with ID: {}", memory_id);

                Ok(ToolExecutionResult::success(format!("Memory stored with ID: {}", memory_id.0))
                    .with_data(serde_json::json!({"id": memory_id.0, "stored": true})))
            }

            "retrieve" => {
                let id = args["id"].as_str().ok_or_else(|| {
                    crate::error::SyscityError::Validation("Missing 'id' argument".to_string())
                })?;

                let memory_id = MemoryId::new(id);
                match self.storage.get(&memory_id).await? {
                    Some(memory) => {
                        debug!("Retrieved memory: {}", id);
                        Ok(ToolExecutionResult::success(memory.content.clone()).with_data(
                            serde_json::json!({
                                "id": memory.id.0,
                                "content": memory.content,
                                "memory_type": memory.memory_type,
                                "created_at": memory.created_at
                            }),
                        ))
                    }
                    None => Ok(ToolExecutionResult::error(format!("Memory '{}' not found", id))),
                }
            }

            "search" => {
                let query = args["query"].as_str().ok_or_else(|| {
                    crate::error::SyscityError::Validation("Missing 'query' argument".to_string())
                })?;

                let limit = args["limit"].as_u64().map(|l| l as usize).unwrap_or(10);
                let category = args["category"].as_str();

                let mut memory_query = MemoryQuery::new()
                    .for_user(&context.user_id)
                    .with_content(query)
                    .limit(limit);

                if let Some(cat) = category {
                    memory_query = memory_query.of_type(cat);
                }

                let results = self.storage.search(memory_query).await?;

                if results.is_empty() {
                    return Ok(ToolExecutionResult::success(format!(
                        "No memories found matching '{}'",
                        query
                    )));
                }

                let formatted: Vec<String> = results
                    .iter()
                    .map(|m| format!("[{}] ({}): {}", m.id.0, m.memory_type, m.content))
                    .collect();

                info!("Found {} memories matching '{}'", results.len(), query);

                Ok(ToolExecutionResult::success(formatted.join("\n\n")).with_data(
                    serde_json::json!({
                        "count": results.len(),
                        "memories": results.iter().map(|m| serde_json::json!({
                            "id": m.id.0,
                            "content": m.content,
                            "memory_type": m.memory_type
                        })).collect::<Vec<_>>()
                    }),
                ))
            }

            "list" => {
                let limit = args["limit"].as_u64().map(|l| l as usize).unwrap_or(10);
                let category = args["category"].as_str();

                let mut memory_query = MemoryQuery::new().for_user(&context.user_id).limit(limit);

                if let Some(cat) = category {
                    memory_query = memory_query.of_type(cat);
                }

                let memories = self.storage.search(memory_query).await?;

                if memories.is_empty() {
                    let cat_msg = category
                        .map(|c| format!(" in category '{}'", c))
                        .unwrap_or_default();
                    return Ok(ToolExecutionResult::success(format!(
                        "No memories found{}",
                        cat_msg
                    )));
                }

                let formatted: Vec<String> = memories
                    .iter()
                    .map(|m| {
                        format!(
                            "[{}] ({}): {}",
                            m.id.0,
                            m.memory_type,
                            m.content.chars().take(100).collect::<String>()
                        )
                    })
                    .collect();

                Ok(ToolExecutionResult::success(formatted.join("\n")).with_data(
                    serde_json::json!({
                        "count": memories.len(),
                        "memories": memories.iter().map(|m| serde_json::json!({
                            "id": m.id.0,
                            "content": m.content,
                            "memory_type": m.memory_type
                        })).collect::<Vec<_>>()
                    }),
                ))
            }

            "delete" => {
                let id = args["id"].as_str().ok_or_else(|| {
                    crate::error::SyscityError::Validation("Missing 'id' argument".to_string())
                })?;

                let memory_id = MemoryId::new(id);
                if self.storage.delete(&memory_id).await? {
                    info!("Deleted memory: {}", id);
                    Ok(ToolExecutionResult::success(format!("Memory '{}' deleted", id)))
                } else {
                    Ok(ToolExecutionResult::error(format!("Memory '{}' not found", id)))
                }
            }

            "update" => {
                let id = args["id"].as_str().ok_or_else(|| {
                    crate::error::SyscityError::Validation("Missing 'id' argument".to_string())
                })?;

                let memory_id = MemoryId::new(id);

                // Get existing memory
                let mut memory = match self.storage.get(&memory_id).await? {
                    Some(m) => m,
                    None => {
                        return Ok(ToolExecutionResult::error(format!("Memory '{}' not found", id)))
                    }
                };

                // Update fields if provided
                if let Some(content) = args["content"].as_str() {
                    memory.content = content.to_string();
                }
                if let Some(memory_type) = args["category"].as_str() {
                    memory.memory_type = MemoryEntryType::from(memory_type);
                }

                self.storage.update(memory).await?;
                info!("Updated memory: {}", id);

                Ok(ToolExecutionResult::success(format!("Memory '{}' updated", id)))
            }

            _ => Err(crate::error::SyscityError::Validation(format!(
                "Unknown action: {}. Valid actions: store, retrieve, search, list, delete, update",
                action
            ))),
        }
    }
}

// ── MemorySearchTool
// ──────────────────────────────────────────────────────────

/// Semantic / hybrid memory search tool.
///
/// Combines vector (cosine similarity) and FTS5 (BM25) search to surface
/// relevant memories.  When neither a vector service nor a session-search
/// service is wired in, falls back to the SQLite keyword search of
/// [`MemoryTool`].
///
/// The tool exposes two actions:
/// * `search` — run hybrid search and return scored results.
/// * `store`  — persist a new memory (delegates to the shared SQLite store).
#[derive(Debug, Clone)]
pub struct MemorySearchTool {
    storage: Arc<SqliteMemoryStore>,
    /// Shared holder for the memory manager.  Populated after vector/FTS5
    /// services finish starting, so the tool sees hybrid search as soon as
    /// it becomes available without requiring a mutable registry.
    memory_manager: Arc<tokio::sync::RwLock<Option<Arc<MemoryManager>>>>,
}

impl MemorySearchTool {
    /// Create with the `<root>/memory` database.
    pub async fn new(paths: &crate::dirs::SyscityPaths) -> crate::Result<Self> {
        let db_path = paths.default_memory_db();
        let db_url = format!("sqlite:///{}", db_path.display());
        let storage = Arc::new(SqliteMemoryStore::new(&db_url).await?);
        Ok(Self {
            storage,
            memory_manager: Arc::new(tokio::sync::RwLock::new(None)),
        })
    }

    /// Create with an existing store (for sharing with the agent).
    pub fn with_store(storage: Arc<SqliteMemoryStore>) -> Self {
        Self {
            storage,
            memory_manager: Arc::new(tokio::sync::RwLock::new(None)),
        }
    }

    /// Attach a shared `MemoryManager` holder to enable lazy hybrid search.
    pub fn with_manager_holder(
        mut self,
        holder: Arc<tokio::sync::RwLock<Option<Arc<MemoryManager>>>>,
    ) -> Self {
        self.memory_manager = holder;
        self
    }
}

#[async_trait]
impl Tool for MemorySearchTool {
    fn name(&self) -> &str {
        "memory_search"
    }

    fn description(&self) -> &str {
        r#"Semantic search across stored memories using hybrid vector + keyword matching.

Use this tool to:
- Find memories relevant to the current conversation
- Recall past decisions, preferences, or facts
- Surface related context before deciding to store new information

Actions:
  search — run semantic search and return scored results
  store  — persist a new memory for future retrieval"#
    }

    fn parameters_schema(&self) -> Value {
        create_schema(
            "Hybrid memory search",
            serde_json::json!({
                "action": {
                    "type": "string",
                    "enum": ["search", "store"],
                    "description": "Operation to perform"
                },
                "query": {
                    "type": "string",
                    "description": "Search query (for 'search' action)"
                },
                "content": {
                    "type": "string",
                    "description": "Memory content to store (for 'store' action)"
                },
                "category": {
                    "type": "string",
                    "description": "Category tag (user, fact, context, task, project)"
                },
                "limit": {
                    "type": "integer",
                    "description": "Maximum results",
                    "default": 6
                }
            }),
            vec!["action"],
        )
    }

    fn capabilities(&self) -> ToolCapabilities {
        ToolCapabilities {
            requires_approval: false,
            risk_level: crate::tools::approval::RiskLevel::Low,
            categories: vec!["memory".to_string(), "search".to_string()],
            ..Default::default()
        }
    }

    async fn execute(
        &self,
        args: Value,
        context: &ToolContext,
    ) -> crate::Result<ToolExecutionResult> {
        let action = args["action"].as_str().ok_or_else(|| {
            crate::error::SyscityError::Validation("Missing 'action'".to_string())
        })?;

        match action {
            "search" => {
                let query = args["query"].as_str().ok_or_else(|| {
                    crate::error::SyscityError::Validation("Missing 'query'".to_string())
                })?;
                let limit = args["limit"].as_u64().map(|l| l as usize).unwrap_or(6);
                let category = args["category"].as_str();

                let mm_guard = self.memory_manager.read().await;
                let results = if let Some(ref mm) = *mm_guard {
                    // Hybrid path: vector + FTS5 via MemoryManager
                    mm.retrieve(
                        &context.user_id,
                        Some(&context.conversation_id),
                        query,
                        Some(limit),
                        None,
                    )
                    .await?
                } else {
                    // Fallback: SQLite keyword search
                    let mut memory_query = MemoryQuery::new()
                        .for_user(&context.user_id)
                        .with_content(query)
                        .limit(limit);

                    if let Some(cat) = category {
                        memory_query = memory_query.of_type(cat);
                    }

                    self.storage.search(memory_query).await?
                };

                if results.is_empty() {
                    return Ok(ToolExecutionResult::success(format!(
                        "No memories found for '{}'",
                        query
                    )));
                }

                let formatted: Vec<String> = results
                    .iter()
                    .map(|m| format!("[{}] ({}): {}", m.id.0, m.memory_type, m.content))
                    .collect();

                info!("memory_search: {} results for '{}'", results.len(), query);

                Ok(ToolExecutionResult::success(formatted.join("\n\n")).with_data(
                    serde_json::json!({
                        "count": results.len(),
                        "memories": results.iter().map(|m| serde_json::json!({
                            "id": m.id.0,
                            "content": m.content,
                            "memory_type": m.memory_type,
                        })).collect::<Vec<_>>()
                    }),
                ))
            }

            "store" => {
                let content = args["content"].as_str().ok_or_else(|| {
                    crate::error::SyscityError::Validation("Missing 'content'".to_string())
                })?;
                let memory_type = args["category"].as_str().unwrap_or("fact");
                let memory = Memory::new(
                    context.user_id.clone(),
                    content.to_string(),
                    memory_type.to_string(),
                )
                .with_conversation(context.conversation_id.clone());

                let id = self.storage.store(memory).await?;
                info!("memory_search: stored memory {}", id);
                Ok(ToolExecutionResult::success(format!("Memory stored: {}", id.0))
                    .with_data(serde_json::json!({"id": id.0})))
            }

            _ => Err(crate::error::SyscityError::Validation(format!(
                "Unknown action '{}'. Use: search, store",
                action
            ))),
        }
    }
}

// ── MemoryGetTool
// ─────────────────────────────────────────────────────────────

/// Exact-ID memory operations: retrieve, delete, update, and list.
///
/// This tool is intentionally narrow — use [`MemorySearchTool`] for
/// semantic recall and this tool when you already know the memory ID or need
/// to manage stored entries.
#[derive(Debug, Clone)]
pub struct MemoryGetTool {
    storage: Arc<SqliteMemoryStore>,
}

impl MemoryGetTool {
    /// Create with the `<root>/memory` database.
    pub async fn new(paths: &crate::dirs::SyscityPaths) -> crate::Result<Self> {
        let db_path = paths.default_memory_db();
        let db_url = format!("sqlite:///{}", db_path.display());
        let storage = Arc::new(SqliteMemoryStore::new(&db_url).await?);
        Ok(Self { storage })
    }

    /// Create with an existing store (for sharing with the agent).
    pub fn with_store(storage: Arc<SqliteMemoryStore>) -> Self {
        Self { storage }
    }
}

#[async_trait]
impl Tool for MemoryGetTool {
    fn name(&self) -> &str {
        "memory_get"
    }

    fn description(&self) -> &str {
        r#"Fetch, list, update, or delete memories by ID.

Actions:
  retrieve — get a single memory by ID
  list     — list recent memories (optionally filtered by category)
  delete   — permanently remove a memory
  update   — change the content or category of an existing memory"#
    }

    fn parameters_schema(&self) -> Value {
        create_schema(
            "Memory CRUD operations",
            serde_json::json!({
                "action": {
                    "type": "string",
                    "enum": ["retrieve", "list", "delete", "update"],
                    "description": "Operation to perform"
                },
                "id": {
                    "type": "string",
                    "description": "Memory ID (for retrieve / delete / update)"
                },
                "category": {
                    "type": "string",
                    "description": "Filter by category (for list)"
                },
                "content": {
                    "type": "string",
                    "description": "New content (for update)"
                },
                "limit": {
                    "type": "integer",
                    "description": "Maximum results for list",
                    "default": 10
                }
            }),
            vec!["action"],
        )
    }

    fn capabilities(&self) -> ToolCapabilities {
        ToolCapabilities {
            requires_approval: false,
            risk_level: crate::tools::approval::RiskLevel::Low,
            categories: vec!["memory".to_string()],
            ..Default::default()
        }
    }

    async fn execute(
        &self,
        args: Value,
        context: &ToolContext,
    ) -> crate::Result<ToolExecutionResult> {
        let action = args["action"].as_str().ok_or_else(|| {
            crate::error::SyscityError::Validation("Missing 'action'".to_string())
        })?;

        match action {
            "retrieve" => {
                let id = args["id"].as_str().ok_or_else(|| {
                    crate::error::SyscityError::Validation("Missing 'id'".to_string())
                })?;
                let memory_id = MemoryId::new(id);
                match self.storage.get(&memory_id).await? {
                    Some(m) => {
                        debug!("memory_get: retrieved {}", id);
                        Ok(ToolExecutionResult::success(m.content.clone()).with_data(
                            serde_json::json!({
                                "id": m.id.0,
                                "content": m.content,
                                "memory_type": m.memory_type,
                            }),
                        ))
                    }
                    None => Ok(ToolExecutionResult::error(format!("Memory '{}' not found", id))),
                }
            }

            "list" => {
                let limit = args["limit"].as_u64().map(|l| l as usize).unwrap_or(10);
                let category = args["category"].as_str();
                let mut query = MemoryQuery::new().for_user(&context.user_id).limit(limit);
                if let Some(cat) = category {
                    query = query.of_type(cat);
                }
                let memories = self.storage.search(query).await?;
                if memories.is_empty() {
                    return Ok(ToolExecutionResult::success("No memories found."));
                }
                let formatted: Vec<String> = memories
                    .iter()
                    .map(|m| {
                        format!(
                            "[{}] ({}): {}",
                            m.id.0,
                            m.memory_type,
                            m.content.chars().take(100).collect::<String>()
                        )
                    })
                    .collect();
                Ok(ToolExecutionResult::success(formatted.join("\n"))
                    .with_data(serde_json::json!({ "count": memories.len() })))
            }

            "delete" => {
                let id = args["id"].as_str().ok_or_else(|| {
                    crate::error::SyscityError::Validation("Missing 'id'".to_string())
                })?;
                let memory_id = MemoryId::new(id);
                if self.storage.delete(&memory_id).await? {
                    info!("memory_get: deleted {}", id);
                    Ok(ToolExecutionResult::success(format!("Memory '{}' deleted", id)))
                } else {
                    Ok(ToolExecutionResult::error(format!("Memory '{}' not found", id)))
                }
            }

            "update" => {
                let id = args["id"].as_str().ok_or_else(|| {
                    crate::error::SyscityError::Validation("Missing 'id'".to_string())
                })?;
                let memory_id = MemoryId::new(id);
                let mut memory = match self.storage.get(&memory_id).await? {
                    Some(m) => m,
                    None => {
                        return Ok(ToolExecutionResult::error(format!("Memory '{}' not found", id)))
                    }
                };
                if let Some(content) = args["content"].as_str() {
                    memory.content = content.to_string();
                }
                if let Some(cat) = args["category"].as_str() {
                    memory.memory_type = MemoryEntryType::from(cat);
                }
                self.storage.update(memory).await?;
                info!("memory_get: updated {}", id);
                Ok(ToolExecutionResult::success(format!("Memory '{}' updated", id)))
            }

            _ => Err(crate::error::SyscityError::Validation(format!(
                "Unknown action '{}'. Use: retrieve, list, delete, update",
                action
            ))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_memory_tool_creation() {
        let tool = MemoryTool::with_database_url("sqlite::memory:")
            .await
            .unwrap();
        assert_eq!(tool.name(), "memory");
    }

    #[tokio::test]
    async fn test_memory_store_and_retrieve() {
        let tool = MemoryTool::with_database_url("sqlite::memory:")
            .await
            .unwrap();
        let context = ToolContext::new("user1", "conv1");

        // Store a memory
        let store_args = serde_json::json!({
            "action": "store",
            "content": "User prefers dark mode",
            "category": "user"
        });

        let result = tool.execute(store_args, &context).await.unwrap();
        assert!(result.success);

        let id = result.data.unwrap()["id"].as_str().unwrap().to_string();

        // Retrieve the memory
        let retrieve_args = serde_json::json!({
            "action": "retrieve",
            "id": id
        });

        let result = tool.execute(retrieve_args, &context).await.unwrap();
        assert!(result.success);
        assert!(result.output.contains("dark mode"));
    }

    #[tokio::test]
    async fn test_memory_search() {
        let tool = MemoryTool::with_database_url("sqlite::memory:")
            .await
            .unwrap();
        let context = ToolContext::new("user1", "conv1");

        // Store some memories
        let memories = vec![
            ("User likes pizza", "food"),
            ("User is vegetarian", "food"),
            ("User works remotely", "work"),
        ];

        for (content, cat) in memories {
            let args = serde_json::json!({
                "action": "store",
                "content": content,
                "category": cat
            });
            tool.execute(args, &context).await.unwrap();
        }

        // Search for food-related memories
        let search_args = serde_json::json!({
            "action": "search",
            "query": "food"
        });

        let result = tool.execute(search_args, &context).await.unwrap();
        assert!(result.success);
    }

    // ── MemorySearchTool tests ────────────────────────────────────────────────

    #[tokio::test]
    async fn test_memory_search_tool_name() {
        let tool = MemorySearchTool::with_store(Arc::new(
            SqliteMemoryStore::new("sqlite::memory:").await.unwrap(),
        ));
        assert_eq!(tool.name(), "memory_search");
    }

    #[tokio::test]
    async fn test_memory_search_store_and_search() {
        let db = Arc::new(SqliteMemoryStore::new("sqlite::memory:").await.unwrap());
        let tool = MemorySearchTool::with_store(db);
        let context = ToolContext::new("user1", "conv1");

        let store = serde_json::json!({
            "action": "store",
            "content": "The user prefers dark mode",
            "category": "user"
        });
        let stored = tool.execute(store, &context).await.unwrap();
        assert!(stored.success);

        let search = serde_json::json!({
            "action": "search",
            "query": "dark mode",
            "limit": 5
        });
        let result = tool.execute(search, &context).await.unwrap();
        assert!(result.success);
    }

    // ── MemoryGetTool tests ───────────────────────────────────────────────────

    #[tokio::test]
    async fn test_memory_get_tool_name() {
        let tool = MemoryGetTool::with_store(Arc::new(
            SqliteMemoryStore::new("sqlite::memory:").await.unwrap(),
        ));
        assert_eq!(tool.name(), "memory_get");
    }

    #[tokio::test]
    async fn test_memory_get_retrieve_and_delete() {
        // Use MemoryTool for storing so we can get the ID back.
        let db = Arc::new(SqliteMemoryStore::new("sqlite::memory:").await.unwrap());
        let store_tool = MemoryTool::with_store(db.clone()).await.unwrap();
        let get_tool = MemoryGetTool::with_store(db);
        let context = ToolContext::new("user1", "conv1");

        // Store via legacy tool
        let store_args = serde_json::json!({
            "action": "store",
            "content": "I prefer Python",
            "category": "user"
        });
        let stored = store_tool.execute(store_args, &context).await.unwrap();
        let id = stored.data.unwrap()["id"].as_str().unwrap().to_string();

        // Retrieve via MemoryGetTool
        let retrieve_args = serde_json::json!({ "action": "retrieve", "id": id });
        let retrieved = get_tool.execute(retrieve_args, &context).await.unwrap();
        assert!(retrieved.success);
        assert!(retrieved.output.contains("Python"));

        // Delete
        let delete_args = serde_json::json!({ "action": "delete", "id": id });
        let deleted = get_tool.execute(delete_args, &context).await.unwrap();
        assert!(deleted.success);
    }

    #[tokio::test]
    async fn test_memory_get_list() {
        let db = Arc::new(SqliteMemoryStore::new("sqlite::memory:").await.unwrap());
        let store_tool = MemoryTool::with_store(db.clone()).await.unwrap();
        let get_tool = MemoryGetTool::with_store(db);
        let context = ToolContext::new("user2", "conv2");

        for i in 0..3 {
            let args = serde_json::json!({
                "action": "store",
                "content": format!("Fact {}", i),
                "category": "fact"
            });
            store_tool.execute(args, &context).await.unwrap();
        }

        let list_args = serde_json::json!({ "action": "list", "limit": 10 });
        let result = get_tool.execute(list_args, &context).await.unwrap();
        assert!(result.success);
        assert!(result.data.unwrap()["count"].as_u64().unwrap() >= 3);
    }
}
