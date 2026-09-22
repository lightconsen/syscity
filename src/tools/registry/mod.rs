//! [`ToolRegistry`]: tool storage, execution, caching, and policy gating.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

use serde_json::Value;
use tracing::{info, warn};

use super::permissions::{EngineDecision, PermissionMode, PermissionsRuntime};
use super::util::consume_stream;
use super::{
    ApprovalDecision, ApprovalQueue, AskQueue, BoxedTool, PendingApproval, PostExecuteDecision,
    SharedTool, SkillTrust, Tool, ToolContext, ToolExecutionChunk, ToolExecutionResult, ToolHooks,
    ToolPolicyDecision,
};
use crate::providers::{FunctionCall, FunctionDefinition};

mod execute;
mod policy;
mod streaming;

#[cfg(test)]
mod tests;

#[derive(Debug, Clone)]
struct CacheEntry {
    result: ToolExecutionResult,
    timestamp: std::time::Instant,
}

/// Shared, mutable list of web search providers. Held by both WebSearchTool
/// and ToolRegistry so hot-reload can update providers without rebuilding
/// the registry.
pub type WebSearchProviders =
    std::sync::Arc<tokio::sync::RwLock<Vec<crate::tools::web::SearchProvider>>>;

/// Registry of tools with optional caching, circuit breaker, and trust-level
/// filtering.
pub struct ToolRegistry {
    tools: std::sync::RwLock<HashMap<String, SharedTool>>,
    /// Dynamically registered tools (e.g. MCP auto-discovered tools).
    /// Uses interior mutability so tools can be added through
    /// `Arc<ToolRegistry>`.
    dynamic_tools: std::sync::RwLock<HashMap<String, std::sync::Arc<dyn Tool>>>,
    /// Tool-name prefixes that have been logically deregistered (e.g. MCP
    /// server disconnect). Tools matching any blocked prefix are excluded
    /// from `get`, `list`, `has`, `get_definitions`, and `get_available`
    /// without requiring `&mut self` — allowing this to be called through an
    /// `Arc<ToolRegistry>`.
    blocked_prefixes: std::sync::RwLock<HashSet<String>>,
    cache: std::sync::Mutex<HashMap<String, CacheEntry>>,
    cache_ttl: Option<Duration>,
    cache_enabled: bool,
    /// Per-tool failure counts for circuit breaker logic.
    failure_counts: std::sync::RwLock<HashMap<String, u32>>,
    /// Tool names that require `SkillTrust::Trusted` access.
    /// When a context has `skill_trust == Community` these tools are hidden.
    privileged_tools: std::sync::RwLock<HashSet<String>>,
    /// Hooks for tool execution (before/after/policy).
    hooks: ToolHooks,
    /// Runtime-override hooks (set through `&self` via `set_hooks`).
    /// Allows tests to inject policy hooks through an `Arc<ToolRegistry>`.
    /// Takes precedence over `self.hooks` when `Some`.
    hooks_override: std::sync::Mutex<Option<ToolHooks>>,
    /// Approval queue for human-in-the-loop tool execution.
    /// When set, high-risk tool calls can be suspended pending human approval.
    approval_queue: Option<Arc<ApprovalQueue>>,
    /// Ask queue for the `ask_user` clarification tool. When set, the tool
    /// can suspend a turn and wait for a human answer.
    ask_queue: Option<Arc<AskQueue>>,
    /// Permission modes and allow/deny/ask rules, consulted before every
    /// tool execution and before the toolset is advertised. Shared with the
    /// gateway, which seeds it from config and applies session overrides.
    permissions: Arc<PermissionsRuntime>,
    /// Content filter for scanning tool outputs for PII and secrets.
    content_filter: Option<Arc<crate::security::content_filter::ContentFilter>>,
    /// Audit logger for recording tool invocations and security events.
    audit_log: Option<Arc<dyn crate::security::runtime_audit::AuditLogger>>,
    /// Shared provider list for the web_search tool. Hot-reload updates this
    /// directly when `[search]` configuration changes.
    web_search_providers: Option<WebSearchProviders>,
    /// Shared todo state backing the `todo` tool. The agent engine reads
    /// this to clear a conversation's active plan at the start of each new
    /// user turn.
    todo_state: Option<Arc<super::todo_tool::TodoState>>,
    /// Oversized successful tool outputs above this many bytes are spilled
    /// to a workspace file and replaced with a head/tail preview.
    /// `None` disables spilling.
    spill_threshold: Option<usize>,
    /// Runtime versioned description overrides (§十一 工具描述成为可搜索的数据).
    /// When present for a tool, its `description` replaces the static
    /// `Tool::description()` in every emitted `FunctionDefinition`.
    metadata: std::sync::RwLock<HashMap<String, super::metadata::ToolDescriptionMeta>>,
}

impl Default for ToolRegistry {
    fn default() -> Self {
        Self {
            tools: std::sync::RwLock::new(HashMap::new()),
            dynamic_tools: std::sync::RwLock::new(HashMap::new()),
            blocked_prefixes: std::sync::RwLock::new(HashSet::new()),
            cache: std::sync::Mutex::new(HashMap::new()),
            cache_ttl: None,
            cache_enabled: true,
            failure_counts: std::sync::RwLock::new(HashMap::new()),
            privileged_tools: std::sync::RwLock::new(HashSet::new()),
            hooks: ToolHooks::new(),
            hooks_override: std::sync::Mutex::new(None),
            approval_queue: None,
            ask_queue: None,
            permissions: Arc::new(PermissionsRuntime::default()),
            content_filter: None,
            audit_log: None,
            web_search_providers: None,
            todo_state: None,
            spill_threshold: Some(Self::DEFAULT_SPILL_THRESHOLD),
            metadata: std::sync::RwLock::new(HashMap::new()),
        }
    }
}

impl std::fmt::Debug for ToolRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ToolRegistry")
            .field(
                "tools",
                &self
                    .tools
                    .read()
                    .map(|m| m.keys().cloned().collect::<Vec<_>>())
                    .unwrap_or_default(),
            )
            .field("hooks", &self.hooks)
            .field("approval_queue", &self.approval_queue.is_some())
            .finish()
    }
}

impl ToolRegistry {
    /// Number of consecutive failures before a tool is circuit-broken.
    pub const CIRCUIT_BREAKER_THRESHOLD: u32 = 3;

    /// Default byte budget above which a successful tool output is spilled
    /// to a workspace file and replaced with a head/tail preview.
    pub const DEFAULT_SPILL_THRESHOLD: usize = 32 * 1024;

    /// Create a new empty registry
    pub fn new() -> Self {
        Self {
            tools: std::sync::RwLock::new(HashMap::new()),
            dynamic_tools: std::sync::RwLock::new(HashMap::new()),
            blocked_prefixes: std::sync::RwLock::new(HashSet::new()),
            cache: std::sync::Mutex::new(HashMap::new()),
            cache_ttl: None,
            cache_enabled: false,
            failure_counts: std::sync::RwLock::new(HashMap::new()),
            privileged_tools: std::sync::RwLock::new(HashSet::new()),
            hooks: ToolHooks::new(),
            hooks_override: std::sync::Mutex::new(None),
            approval_queue: None,
            ask_queue: None,
            permissions: Arc::new(PermissionsRuntime::default()),
            content_filter: None,
            audit_log: None,
            web_search_providers: None,
            todo_state: None,
            spill_threshold: Some(Self::DEFAULT_SPILL_THRESHOLD),
            metadata: std::sync::RwLock::new(HashMap::new()),
        }
    }

    /// Create a new registry with caching enabled
    pub fn with_cache(ttl: Duration) -> Self {
        Self {
            tools: std::sync::RwLock::new(HashMap::new()),
            dynamic_tools: std::sync::RwLock::new(HashMap::new()),
            blocked_prefixes: std::sync::RwLock::new(HashSet::new()),
            cache: std::sync::Mutex::new(HashMap::new()),
            cache_ttl: Some(ttl),
            cache_enabled: true,
            failure_counts: std::sync::RwLock::new(HashMap::new()),
            privileged_tools: std::sync::RwLock::new(HashSet::new()),
            hooks: ToolHooks::new(),
            hooks_override: std::sync::Mutex::new(None),
            approval_queue: None,
            ask_queue: None,
            permissions: Arc::new(PermissionsRuntime::default()),
            content_filter: None,
            audit_log: None,
            web_search_providers: None,
            todo_state: None,
            spill_threshold: Some(Self::DEFAULT_SPILL_THRESHOLD),
            metadata: std::sync::RwLock::new(HashMap::new()),
        }
    }

    /// Attach the shared web_search provider list so hot-reload can update it
    /// without rebuilding the registry.
    pub fn with_web_search_providers(mut self, providers: WebSearchProviders) -> Self {
        self.web_search_providers = Some(providers);
        self
    }

    /// Override the spill threshold (bytes). `None` disables spilling.
    pub fn with_spill_threshold(mut self, threshold: Option<usize>) -> Self {
        self.spill_threshold = threshold;
        self
    }

    /// Get a clone of the shared web_search provider list, if one was set.
    pub fn web_search_providers(&self) -> Option<WebSearchProviders> {
        self.web_search_providers.clone()
    }

    /// Attach the shared todo state so the agent engine can clear a
    /// conversation's active plan at the start of each new user turn. The
    /// registered `todo` tool must be built over the same handle
    /// ([`TodoTool::with_state`](super::todo_tool::TodoTool::with_state)).
    pub fn with_todo_state(mut self, state: Arc<super::todo_tool::TodoState>) -> Self {
        self.todo_state = Some(state);
        self
    }

    /// Get a clone of the shared todo state, if one was set.
    pub fn todo_state(&self) -> Option<Arc<super::todo_tool::TodoState>> {
        self.todo_state.clone()
    }

    // ── Circuit breaker ───────────────────────────────────────────────────────

    /// Record a failure for `name`. After `CIRCUIT_BREAKER_THRESHOLD`
    /// consecutive failures the tool is considered degraded and excluded from
    /// `get_available()`.
    pub fn record_failure(&self, name: &str) {
        if let Ok(mut counts) = self.failure_counts.write() {
            let entry = counts.entry(name.to_string()).or_insert(0);
            *entry += 1;
            if *entry >= Self::CIRCUIT_BREAKER_THRESHOLD {
                tracing::warn!(
                    tool = name,
                    failures = *entry,
                    "Tool circuit-breaker tripped — marking as degraded"
                );
            }
        }
    }

    /// Reset the failure count for `name` (e.g. after a successful execution).
    pub fn reset_failure(&self, name: &str) {
        if let Ok(mut counts) = self.failure_counts.write() {
            counts.remove(name);
        }
    }

    /// Returns `true` if the tool has been circuit-broken due to repeated
    /// failures.
    pub fn is_degraded(&self, name: &str) -> bool {
        self.failure_counts
            .read()
            .map(|counts| counts.get(name).copied().unwrap_or(0) >= Self::CIRCUIT_BREAKER_THRESHOLD)
            .unwrap_or(false)
    }

    /// List all currently-degraded tool names.
    pub fn degraded_tools(&self) -> Vec<String> {
        self.failure_counts
            .read()
            .map(|counts| {
                counts
                    .iter()
                    .filter(|(_, &v)| v >= Self::CIRCUIT_BREAKER_THRESHOLD)
                    .map(|(k, _)| k.clone())
                    .collect()
            })
            .unwrap_or_default()
    }

    // ── Privilege / trust-level filtering ────────────────────────────────────

    /// Mark `name` as a privileged tool (shell execution, file writes, etc.).
    /// Privileged tools are hidden when `context.skill_trust == Community`.
    pub fn mark_privileged(&mut self, name: &str) {
        if let Ok(mut set) = self.privileged_tools.write() {
            set.insert(name.to_string());
        }
    }

    /// Returns `true` if `name` is a privileged tool.
    pub fn is_privileged(&self, name: &str) -> bool {
        self.privileged_tools
            .read()
            .map(|set| set.contains(name))
            .unwrap_or(false)
    }

    /// Returns `true` if `name` matches any blocked prefix.
    fn is_blocked(&self, name: &str) -> bool {
        self.blocked_prefixes
            .read()
            .map(|set| set.iter().any(|p| name.starts_with(p.as_str())))
            .unwrap_or(false)
    }

    /// Returns `true` if the tool should be excluded from availability checks,
    /// considering blocked prefixes, circuit-breaker state, trust level, and
    /// plugin allowlists.
    fn is_excluded(&self, name: &str, context: &ToolContext) -> bool {
        if self.is_blocked(name) {
            return true;
        }
        if self.is_degraded(name) {
            return true;
        }
        if context.model.skill_trust < SkillTrust::Trusted && self.is_privileged(name) {
            return true;
        }

        // Plan mode hides non-read-only tools from the advertised toolset;
        // the per-call gate below is the backstop for anything that was
        // advertised before the mode flipped mid-turn.
        if self
            .permissions
            .effective_mode(&context.identity.conversation_id)
            == PermissionMode::Plan
            && !self.tool_capabilities(name).read_only
        {
            return true;
        }

        // Determine registration provenance for source gating.
        let is_dynamic = self.is_dynamic_tool(name);

        // Plugin allowlist at the context level (runtime restriction).
        if is_dynamic && Self::is_plugin_like_name(name) {
            if let Some(allowlist) = context.plugin_allowlist() {
                let allowed = allowlist
                    .iter()
                    .any(|prefix| name == prefix || name.starts_with(prefix));
                if !allowed {
                    return true;
                }
            }
        }

        false
    }

    /// What a caller should know about a call whose outcome is unknown.
    ///
    /// A timeout is the one moment the gateway knows a tool *may* have taken
    /// effect and cannot say whether it did — and the shape of that doubt
    /// differs per tool: a read can be repeated, a send cannot. Quoting the
    /// tool's own [`ToolCapabilities`] is what the declaration is for.
    fn uncertainty_note(&self, tool_name: &str) -> String {
        let caps = self.get_capabilities(tool_name);
        if caps.idempotent {
            " The tool is idempotent: retrying it is safe.".to_string()
        } else {
            match caps.compensation {
                Some(compensation) => format!(
                    " It is not idempotent — the call may have taken effect. To undo it: \
                     {compensation}."
                ),
                None => " It is not idempotent and has no compensating action: retrying may \
                         duplicate the effect."
                    .to_string(),
            }
        }
    }

    /// Helper to look up tool capabilities from either registry.
    fn tool_capabilities(&self, name: &str) -> crate::tools::sdk::ToolCapabilities {
        self.tools
            .read()
            .ok()
            .and_then(|map| map.get(name).map(|t| t.capabilities()))
            .or_else(|| {
                self.dynamic_tools
                    .read()
                    .ok()
                    .and_then(|map| map.get(name).map(|t| t.capabilities()))
            })
            .unwrap_or_default()
    }

    /// Get the advertised capabilities for a tool by name.
    pub fn get_capabilities(&self, name: &str) -> crate::tools::sdk::ToolCapabilities {
        self.tool_capabilities(name)
    }

    // ── Versioned description metadata (§十一) ──────────────────────────────

    /// Set (or replace) the runtime description override for `name`.
    ///
    /// The registry is shared with all running agents, so an override is picked
    /// up on the next turn without restart. When present, this description
    /// replaces the tool's static `Tool::description()` in `get_definitions()`
    /// and `get_available()`.
    pub fn set_metadata(&self, name: &str, meta: crate::tools::metadata::ToolDescriptionMeta) {
        if let Ok(mut map) = self.metadata.write() {
            map.insert(name.to_string(), meta);
        } else {
            warn!("Metadata RwLock poisoned in set_metadata for '{}'", name);
        }
    }

    /// Current runtime description override for `name`, if any.
    pub fn metadata_for(&self, name: &str) -> Option<crate::tools::metadata::ToolDescriptionMeta> {
        self.metadata
            .read()
            .ok()
            .and_then(|map| map.get(name).cloned())
    }

    /// The effective LLM-facing description for a tool: the runtime override
    /// when set, otherwise the tool's static description.
    fn effective_description(&self, name: &str, tool: &SharedTool) -> String {
        self.metadata_for(name)
            .map(|m| m.description)
            .unwrap_or_else(|| tool.description().to_string())
    }

    /// Resolve the effective description for a tool by name, checking the
    /// metadata override, then the static registry, then the dynamic registry.
    ///
    /// Unlike [`ToolRegistry::get`], this covers dynamically-registered tools,
    /// so the structural proposer can read the current description of any
    /// registered tool regardless of where it lives.
    pub fn description_for(&self, name: &str) -> Option<String> {
        if let Some(m) = self.metadata_for(name) {
            return Some(m.description);
        }
        if let Some(tool) = self.get(name) {
            return Some(tool.description().to_string());
        }
        self.dynamic_tools
            .read()
            .ok()
            .and_then(|map| map.get(name).map(|t| t.description().to_string()))
    }

    /// Build a `FunctionDefinition` for a tool, applying any description
    /// override from the metadata store.
    fn definition_for(&self, name: &str, tool: &SharedTool) -> FunctionDefinition {
        let mut def = tool.to_function_definition();
        def.description = self.effective_description(name, tool);
        def
    }

    /// Returns `true` if `name` is registered only in the dynamic registry.
    fn is_dynamic_tool(&self, name: &str) -> bool {
        self.tools
            .read()
            .ok()
            .is_none_or(|map| !map.contains_key(name))
            && self
                .dynamic_tools
                .read()
                .map(|map| map.contains_key(name))
                .unwrap_or(false)
    }

    /// Heuristic: plugin tools often use `__` separators (MCP or plugin
    /// runtime).
    fn is_plugin_like_name(name: &str) -> bool {
        name.contains("__")
    }

    /// Enable caching with the specified TTL
    pub fn enable_cache(&mut self, ttl: Duration) {
        self.cache_enabled = true;
        self.cache_ttl = Some(ttl);
    }

    /// Disable caching
    pub fn disable_cache(&mut self) {
        self.cache_enabled = false;
        // Clear existing cache
        if let Ok(mut cache) = self.cache.lock() {
            cache.clear();
        }
    }

    /// Clear the tool result cache
    pub fn clear_cache(&self) {
        if let Ok(mut cache) = self.cache.lock() {
            cache.clear();
        }
    }

    // ── Unified tool iteration ───────────────────────────────────────────────

    /// Iterate over both static and dynamic registries, yielding
    /// `(name, Arc<dyn Tool>)` for every tool that satisfies `filter`.
    ///
    /// This is the single point of iteration for `list()`, `get_definitions()`,
    /// `get_available()`, and `all_tools_arc()` — they all delegate here rather
    /// than duplicating the two-registry walk.
    fn iter_tools<F>(&self, filter: F) -> Vec<(String, Arc<dyn Tool>)>
    where
        F: Fn(&str) -> bool,
    {
        let mut result = Vec::new();
        if let Ok(map) = self.tools.read() {
            for (name, tool) in map.iter() {
                if filter(name) {
                    result.push((name.clone(), tool.clone()));
                }
            }
        }
        if let Ok(dynamic) = self.dynamic_tools.read() {
            for (name, tool) in dynamic.iter() {
                if filter(name) {
                    result.push((name.clone(), tool.clone()));
                }
            }
        }
        result
    }

    /// Generate a cache key from tool name and arguments
    fn cache_key(name: &str, args: &Value) -> String {
        use std::collections::hash_map::DefaultHasher;
        use std::hash::{Hash, Hasher};

        let mut hasher = DefaultHasher::new();
        name.hash(&mut hasher);
        // Hash the JSON string representation of args
        args.to_string().hash(&mut hasher);
        format!("{}:{}", name, hasher.finish())
    }

    /// Get cached result if available and not expired
    fn get_cached(&self, key: &str) -> Option<ToolExecutionResult> {
        if !self.cache_enabled {
            return None;
        }

        let cache = match self.cache.lock() {
            Ok(guard) => guard,
            Err(e) => {
                warn!("Cache mutex poisoned in get_cached: {}", e);
                return None;
            }
        };
        let entry = cache.get(key)?;

        // Check if cache entry is expired
        if let Some(ttl) = self.cache_ttl {
            if entry.timestamp.elapsed() > ttl {
                return None;
            }
        }

        Some(entry.result.clone())
    }

    /// Store result in cache
    fn store_cached(&self, key: String, result: ToolExecutionResult) {
        if !self.cache_enabled {
            return;
        }

        if let Ok(mut cache) = self.cache.lock() {
            cache.insert(
                key,
                CacheEntry {
                    result,
                    timestamp: std::time::Instant::now(),
                },
            );
        }
    }

    // ── Hooks and approval queue ──────────────────────────────────────────────

    /// Set the hooks for this registry.
    ///
    /// Hooks allow policy decisions, before/after execution callbacks,
    /// and human-in-the-loop approval for high-risk tools.
    pub fn with_hooks(mut self, hooks: ToolHooks) -> Self {
        self.hooks = hooks;
        self
    }

    /// Set the hooks for this registry through `&self` (interior mutability).
    ///
    /// This allows setting hooks through an `Arc<ToolRegistry>` without
    /// requiring `&mut self`. Used by tests that need to inject policy
    /// hooks at runtime (e.g. auto-approval for device tool calls).
    pub fn set_hooks(&self, hooks: ToolHooks) {
        if let Ok(mut guard) = self.hooks_override.lock() {
            *guard = Some(hooks);
        }
    }

    /// Return the active hooks — the override hooks if set, otherwise the
    /// builder-configured hooks.  Override hooks take precedence so that
    /// `set_hooks()` (called through `Arc<ToolRegistry>`) can inject hooks
    /// at runtime without requiring `&mut self`.
    fn active_hooks(&self) -> ToolHooks {
        self.hooks_override
            .lock()
            .ok()
            .and_then(|guard| guard.clone())
            .unwrap_or_else(|| self.hooks.clone())
    }

    /// Install the shared permission runtime (modes + rules). The gateway
    /// seeds it from `[permissions]` config and keeps it updated on
    /// `config.set` / hot reload.
    pub fn with_permissions(mut self, permissions: Arc<PermissionsRuntime>) -> Self {
        self.permissions = permissions;
        self
    }

    /// The shared permission runtime, for gateway-side updates and reads.
    pub fn permissions(&self) -> &Arc<PermissionsRuntime> {
        &self.permissions
    }

    /// Set the approval queue for human-in-the-loop execution.
    ///
    /// When set, tool calls that return `ToolPolicyDecision::NeedsApproval`
    /// will suspend execution and wait for human approval via the queue.
    pub fn with_approval_queue(mut self, queue: Arc<ApprovalQueue>) -> Self {
        self.approval_queue = Some(queue);
        self
    }

    /// Get a reference to the approval queue if set.
    pub fn approval_queue(&self) -> Option<&Arc<ApprovalQueue>> {
        self.approval_queue.as_ref()
    }

    /// Set the ask queue for the `ask_user` clarification tool.
    pub fn with_ask_queue(mut self, queue: Arc<AskQueue>) -> Self {
        self.ask_queue = Some(queue);
        self
    }

    /// Get a reference to the ask queue if set.
    pub fn ask_queue(&self) -> Option<&Arc<AskQueue>> {
        self.ask_queue.as_ref()
    }

    /// Set the content filter for scanning tool outputs.
    pub fn with_content_filter(
        mut self,
        filter: Arc<crate::security::content_filter::ContentFilter>,
    ) -> Self {
        self.content_filter = Some(filter);
        self
    }

    /// Set the audit logger for recording security events.
    pub fn with_audit_log(
        mut self,
        audit_log: Arc<dyn crate::security::runtime_audit::AuditLogger>,
    ) -> Self {
        self.audit_log = Some(audit_log);
        self
    }

    /// Get a clone of the configured audit logger, if any.
    pub fn audit_log(&self) -> Option<Arc<dyn crate::security::runtime_audit::AuditLogger>> {
        self.audit_log.clone()
    }

    /// Get a clone of the configured content filter, if any.
    pub fn content_filter(&self) -> Option<Arc<crate::security::content_filter::ContentFilter>> {
        self.content_filter.clone()
    }

    /// Return a snapshot of all dynamically-registered tools.
    pub fn dynamic_tools(&self) -> Vec<(String, std::sync::Arc<dyn Tool>)> {
        match self.dynamic_tools.read() {
            Ok(map) => map.iter().map(|(k, v)| (k.clone(), v.clone())).collect(),
            Err(e) => {
                warn!("dynamic_tools lock poisoned: {}", e);
                Vec::new()
            }
        }
    }

    /// Register a tool from a boxed implementation.
    pub fn register(&mut self, tool: BoxedTool) {
        let name = tool.name().to_string();
        let tool: SharedTool = tool.into();
        match self.tools.write() {
            Ok(mut map) => {
                map.insert(name, tool);
            }
            Err(e) => warn!("Tools RwLock poisoned in register: {}", e),
        }
    }

    /// Remove a single tool by exact name.
    pub fn remove(&mut self, name: &str) -> Option<SharedTool> {
        match self.tools.write() {
            Ok(mut map) => map.remove(name),
            Err(e) => {
                warn!("Tools RwLock poisoned in remove: {}", e);
                None
            }
        }
    }

    /// Replace a statically-registered tool by exact name.
    /// Returns the previous tool if one existed.
    pub fn replace(&mut self, name: &str, tool: BoxedTool) -> Option<SharedTool> {
        let new_name = tool.name().to_string();
        if name != new_name {
            warn!("Tool replacement name mismatch: replacing '{}' with '{}'", name, new_name);
        }
        let tool: SharedTool = tool.into();
        match self.tools.write() {
            Ok(mut map) => map.insert(new_name, tool),
            Err(e) => {
                warn!("Tools RwLock poisoned in replace: {}", e);
                None
            }
        }
    }

    /// Remove all tools whose names start with `prefix`.
    ///
    /// Uses interior mutability so it works through `Arc<ToolRegistry>` —
    /// tools are hidden from all lookup methods immediately. The underlying
    /// map entries are lazily cleaned up (they remain allocated but invisible).
    ///
    /// Used by the MCP subsystem to clean up `mcp__{server}__*` tools when a
    /// server disconnects.
    pub fn deregister_prefix(&self, prefix: &str) {
        if let Ok(mut set) = self.blocked_prefixes.write() {
            set.insert(prefix.to_string());
        }
        // Also remove matching static and dynamic tools immediately so
        // stale entries don't accumulate in memory.
        if let Ok(mut map) = self.tools.write() {
            map.retain(|k, _| !k.starts_with(prefix));
        }
        if let Ok(mut map) = self.dynamic_tools.write() {
            map.retain(|k, _| !k.starts_with(prefix));
        }
    }

    /// Dynamically register a tool without requiring `&mut self`.
    ///
    /// This allows tools to be added through an `Arc<ToolRegistry>` — used by
    /// the MCP subsystem to register auto-discovered tools at startup.
    pub fn register_dynamic(&self, tool: std::sync::Arc<dyn Tool>) {
        let name = tool.name().to_string();
        if let Ok(mut map) = self.dynamic_tools.write() {
            map.insert(name, tool);
        }
    }

    /// Remove a single dynamically-registered tool by exact name.
    pub fn deregister_dynamic(&self, name: &str) {
        if let Ok(mut map) = self.dynamic_tools.write() {
            map.remove(name);
        }
    }

    /// Get a tool by name (returns `None` for blocked or degraded tools).
    ///
    /// Only covers statically-registered tools. For dynamic tools use
    /// `execute()` or `execute_call()` which check both registries.
    pub fn get(&self, name: &str) -> Option<SharedTool> {
        if self.is_blocked(name) || self.is_degraded(name) {
            return None;
        }
        self.tools
            .read()
            .ok()
            .and_then(|map| map.get(name).cloned())
    }

    /// List available tool names (excludes blocked and degraded tools).
    /// Includes both statically- and dynamically-registered tools.
    /// List available tool names (excludes blocked and degraded tools).
    /// Includes both statically- and dynamically-registered tools.
    pub fn list(&self) -> Vec<String> {
        self.iter_tools(|name| !self.is_blocked(name) && !self.is_degraded(name))
            .into_iter()
            .map(|(name, _)| name)
            .collect()
    }

    /// Get all dynamically-registered tools as `Arc<dyn Tool>` references.
    ///
    /// Excludes blocked and degraded tools. Static tools registered via
    /// `register(Box<dyn Tool>)` are NOT returned — callers that need
    /// `Arc<dyn Tool>` for static tools should collect `Arc` references
    /// at registration time via `register_arc()`.
    pub fn all_tools_arc(&self) -> Vec<std::sync::Arc<dyn Tool>> {
        let mut result: Vec<std::sync::Arc<dyn Tool>> = Vec::new();

        if let Ok(dynamic) = self.dynamic_tools.read() {
            for (name, tool) in dynamic.iter() {
                if !self.is_blocked(name) && !self.is_degraded(name) {
                    result.push(tool.clone());
                }
            }
        }

        result
    }

    /// Whether the blocked-prefix policy refuses this tool name.
    ///
    /// [`has`](Self::has) and [`list`](Self::list) fold this together with
    /// degradation and registration, which is what availability wants. A caller
    /// that executes a tool the registry does not dispatch — and so cannot run
    /// it through [`execute_call`](Self::execute_call) — still needs to be able
    /// to ask this question on its own.
    pub fn is_name_blocked(&self, name: &str) -> bool {
        self.is_blocked(name)
    }

    /// Check if a tool exists, is not blocked, and is not degraded.
    /// Checks both static and dynamic registries.
    pub fn has(&self, name: &str) -> bool {
        if self.is_blocked(name) || self.is_degraded(name) {
            return false;
        }
        if self
            .tools
            .read()
            .ok()
            .is_some_and(|map| map.contains_key(name))
        {
            return true;
        }
        self.dynamic_tools
            .read()
            .map(|map| map.contains_key(name))
            .unwrap_or(false)
    }

    /// Get all tools as function definitions (excludes blocked and degraded
    /// tools). Includes both statically- and dynamically-registered tools.
    /// Runtime description overrides (§十一) replace static descriptions.
    pub fn get_definitions(&self) -> Vec<FunctionDefinition> {
        self.iter_tools(|name| !self.is_blocked(name) && !self.is_degraded(name))
            .into_iter()
            .map(|(name, tool)| self.definition_for(&name, &tool))
            .collect()
    }

    /// Get all available tools for a given context.
    ///
    /// Excludes:
    /// - Blocked-prefix tools (MCP server disconnected)
    /// - Degraded tools (circuit-breaker tripped)
    /// - Privileged tools when `context.skill_trust == Community`
    ///
    /// Includes both statically- and dynamically-registered tools.
    pub fn get_available(&self, context: &ToolContext) -> Vec<FunctionDefinition> {
        self.iter_tools(|name| !self.is_excluded(name, context))
            .into_iter()
            .filter(|(_, tool)| tool.is_available(context))
            .map(|(name, tool)| self.definition_for(&name, &tool))
            .collect()
    }
}
