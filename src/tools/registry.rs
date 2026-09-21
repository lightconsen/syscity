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

    /// Execute a tool by name with optional caching, hooks, and approval flow.
    /// Checks both static and dynamic registries.
    ///
    /// # Policy and Approval Flow
    ///
    /// Run policy hooks and the built-in `requires_approval` fallback.
    ///
    /// If no explicit policy hooks are configured but the tool advertises
    /// `requires_approval`, this synthesises a `NeedsApproval` decision
    /// automatically so that high-risk tools (device access, etc.) are
    /// never executed silently without the caller going through approval.
    async fn evaluate_policy(
        &self,
        name: &str,
        args: &Value,
        ctx: &ToolContext,
    ) -> ToolPolicyDecision {
        self.run_gate(name, args, ctx, false).await
    }

    /// The approval a tool advertising `requires_approval` needs.
    ///
    /// One constructor for both policy paths, so what a caller sees cannot
    /// depend on which of them asked.
    fn approval_fallback(&self, name: &str, args: &Value) -> ToolPolicyDecision {
        let risk_level = self.tool_capabilities(name).risk_level;
        self.needs_approval(name, args, risk_level, format!("Tool '{}' requires approval", name))
    }

    /// A `[permissions].ask` rule matched: route to approval with the rule's
    /// reason and the tool's real risk level.
    fn force_ask(&self, name: &str, args: &Value, reason: String) -> ToolPolicyDecision {
        let risk_level = self.tool_capabilities(name).risk_level;
        self.needs_approval(name, args, risk_level, reason)
    }

    /// The one `NeedsApproval` constructor, so every ask path looks the same
    /// to the approval queue and the audit log.
    fn needs_approval(
        &self,
        name: &str,
        args: &Value,
        risk_level: crate::tools::approval::RiskLevel,
        message: String,
    ) -> ToolPolicyDecision {
        ToolPolicyDecision::NeedsApproval {
            approval_id: format!(
                "fallback-{}-{}",
                name,
                uuid::Uuid::new_v4()
                    .to_string()
                    .split('-')
                    .next()
                    .unwrap_or("0000")
            ),
            tool_name: name.to_string(),
            args: args.clone(),
            risk_level,
            requested_by: "system".to_string(),
            message,
        }
    }

    /// The permission engine: rules + mode, evaluated before any hook.
    ///
    /// Reads the true (post-wrapper-fix) capabilities, so plan mode and the
    /// `write` category see what the tool really is.
    fn evaluate_permissions(&self, name: &str, args: &Value, ctx: &ToolContext) -> EngineDecision {
        let caps = self.tool_capabilities(name);
        self.permissions.evaluate(
            name,
            args,
            &ctx.identity.conversation_id,
            caps.read_only,
            &caps.categories,
        )
    }

    /// The one policy gate both execution paths share: permission engine,
    /// then hooks, then the `requires_approval` fallback.
    ///
    /// Order is locked: a `deny` rule blocks before hooks can even speak; a
    /// hook `Deny` beats every pre-approval; a hook `NeedsApproval` is
    /// honoured as explicit configuration except under `bypass`, where it is
    /// downgraded to allow (and audited); an `allow` rule or mode
    /// pre-approval skips the fallback; an `ask` rule forces the fallback
    /// with the rule's reason. `hooks_only_fallback` preserves the two
    /// paths' existing difference: the buffered path additionally requires
    /// [`can_ask_a_human`](crate::tools::ask_user::can_ask_a_human) before
    /// the fallback fires.
    async fn run_gate(
        &self,
        name: &str,
        args: &Value,
        ctx: &ToolContext,
        hooks_only_fallback: bool,
    ) -> ToolPolicyDecision {
        let engine = self.evaluate_permissions(name, args, ctx);

        let decision = if let EngineDecision::Deny(reason) = engine {
            // A deny rule blocks before hooks can speak.
            ToolPolicyDecision::Deny { reason }
        } else {
            let hook = self.active_hooks().run_policy(name, args, ctx).await;
            match hook {
                ToolPolicyDecision::Deny { reason } => ToolPolicyDecision::Deny { reason },
                ToolPolicyDecision::NeedsApproval { .. }
                    if engine == EngineDecision::AllowNow
                        && self
                            .permissions
                            .effective_mode(&ctx.identity.conversation_id)
                            == PermissionMode::Bypass =>
                {
                    // bypass skips approval prompts; an explicit hook ask is
                    // downgraded with it and audited below.
                    ToolPolicyDecision::Allow
                }
                other => match (other, engine) {
                    (ToolPolicyDecision::Allow, EngineDecision::Ask(reason)) => {
                        self.force_ask(name, args, reason)
                    }
                    (ToolPolicyDecision::Allow, EngineDecision::AllowNow) => {
                        ToolPolicyDecision::Allow
                    }
                    (ToolPolicyDecision::Allow, EngineDecision::PassThrough) => {
                        // requires_approval fallback — only when no policy hook
                        // exists, so an explicitly-configured policy hook is
                        // always authoritative.
                        if !self.active_hooks().has_policy_hooks()
                            && self.get_capabilities(name).requires_approval
                            && (!hooks_only_fallback
                                || crate::tools::ask_user::can_ask_a_human(ctx))
                        {
                            self.approval_fallback(name, args)
                        } else {
                            ToolPolicyDecision::Allow
                        }
                    }
                    (other, _) => other,
                },
            }
        };

        self.audit_policy_decision(name, ctx, &decision).await;
        decision
    }

    /// Evaluate the registered policy hooks, plus the `requires_approval`
    /// fallback when there is a human to answer it — auditing any non-allow
    /// decision.
    ///
    /// Used by the buffered [`execute_call`](Self::execute_call) path. With no
    /// policy hooks installed, a tool that advertises `requires_approval` is
    /// now gated **and only where the question can be put to someone**: see
    /// [`can_ask_a_human`]. A policy hook that returns `NeedsApproval` is
    /// honoured either way (the caller routes it through the full approval
    /// flow).
    async fn evaluate_policy_hooks_only(
        &self,
        name: &str,
        args: &Value,
        ctx: &ToolContext,
    ) -> ToolPolicyDecision {
        self.run_gate(name, args, ctx, true).await
    }

    /// Audit a non-allow policy decision as a `ToolDeny` event so a denied or
    /// approval-flagged call forms an auditable pair with any later
    /// `ToolInvocation` entry.
    async fn audit_policy_decision(
        &self,
        name: &str,
        ctx: &ToolContext,
        decision: &ToolPolicyDecision,
    ) {
        if decision.is_allow() {
            return;
        }
        let Some(ref audit) = self.audit_log else {
            return;
        };
        let (kind, description) = match decision {
            ToolPolicyDecision::Deny { reason } => ("deny", reason.clone()),
            ToolPolicyDecision::NeedsApproval { message, .. } => ("ask", message.clone()),
            ToolPolicyDecision::Allow => return,
        };
        let details = serde_json::json!({ "kind": kind });
        audit
            .log_entry(
                crate::security::runtime_audit::AuditEventType::ToolDeny,
                ctx.user_id.clone(),
                name.to_string(),
                false,
                format!("Tool '{}' {} by policy: {}", name, kind, description),
                Some(details),
            )
            .await;
    }

    /// Execute a tool by name with optional caching, hooks, and approval flow.
    /// Checks both static and dynamic registries.
    ///
    /// # Policy and Approval Flow
    ///
    /// 1. Run policy hooks — if any hook returns `Deny`, return error
    ///    immediately
    /// 2. If any hook returns `NeedsApproval` and approval_queue is configured,
    ///    suspend execution and wait for human approval
    /// 3. Run before-hooks
    /// 4. Execute the tool
    /// 5. Run after-hooks
    pub async fn execute(
        &self,
        name: &str,
        args: Value,
        context: &ToolContext,
    ) -> Option<crate::Result<ToolExecutionResult>> {
        let policy_decision = self.evaluate_policy(name, &args, context).await;

        match policy_decision {
            ToolPolicyDecision::Allow => {
                // Proceed with execution
            }
            ToolPolicyDecision::Deny { reason } => {
                return Some(Err(crate::error::SyscityError::Validation(format!(
                    "Tool '{}' denied: {}",
                    name, reason
                ))));
            }
            ToolPolicyDecision::NeedsApproval {
                approval_id,
                tool_name,
                args: approval_args,
                risk_level,
                requested_by,
                message,
            } => {
                // Check if approval queue is configured
                let approval_queue = match &self.approval_queue {
                    Some(q) => q.clone(),
                    None => {
                        return Some(Err(crate::error::SyscityError::Validation(
                            "Tool requires approval but no approval queue configured".into(),
                        )));
                    }
                };

                // Create oneshot channel for the approval resolution
                let (tx, rx) = tokio::sync::oneshot::channel();

                // Create pending approval
                let approval =
                    PendingApproval::new(&approval_id, &tool_name, approval_args, requested_by)
                        .with_risk_level(risk_level)
                        .with_message(message)
                        .with_session(Some(context.conversation_id.clone()))
                        .with_response_tx(tx);

                // Submit to approval queue
                approval_queue.submit(approval).await;

                // Wait for human decision (with 5-minute timeout)
                const APPROVAL_TIMEOUT: Duration = Duration::from_secs(300);
                match tokio::time::timeout(APPROVAL_TIMEOUT, rx).await {
                    Ok(Ok(ApprovalDecision::Approve)) => {
                        tracing::info!(
                            "Approval {} granted, proceeding with tool execution",
                            approval_id
                        );
                        // Proceed with execution below
                    }
                    Ok(Ok(ApprovalDecision::Deny { reason })) => {
                        return Some(Err(crate::error::SyscityError::Validation(format!(
                            "Tool '{}' denied by user: {}",
                            name, reason
                        ))));
                    }
                    Ok(Err(_)) => {
                        return Some(Err(crate::error::SyscityError::Validation(
                            "Approval channel closed".into(),
                        )));
                    }
                    Err(_) => {
                        return Some(Err(crate::error::SyscityError::Timeout(format!(
                            "Tool '{}' approval request timed out after {:?}",
                            name, APPROVAL_TIMEOUT
                        ))));
                    }
                }
            }
        }

        // Run before-hooks
        self.active_hooks().run_before(name, &args).await;

        // Check cache first
        let cache_key = Self::cache_key(name, &args);
        if let Some(cached_result) = self.get_cached(&cache_key) {
            tracing::debug!("Cache hit for tool: {}", name);
            let result = Ok(cached_result);
            if let Ok(ref exec_result) = result {
                self.active_hooks()
                    .run_after(name, &args, exec_result)
                    .await;
            }
            return self
                .finalize_result(name, &args, context, Some(result))
                .await;
        }

        // Execute the tool — clone args so the original remains for after-hooks
        let execution_result: Option<crate::Result<ToolExecutionResult>> = {
            // Try static tools first
            if let Some(tool) = self.get(name) {
                let _t_exec = std::time::Instant::now();
                let result = tool.execute(args.clone(), context).await;
                info!("[Timing] tool.execute({}) returned in {:?}", name, _t_exec.elapsed());
                if let Ok(ref exec_result) = result {
                    self.store_cached(cache_key, exec_result.clone());
                }
                Some(result)
            } else {
                // Try dynamic tools
                let dynamic_tool = self
                    .dynamic_tools
                    .read()
                    .ok()
                    .and_then(|map| map.get(name).cloned());

                if let Some(tool) = dynamic_tool {
                    if !self.is_blocked(name) && !self.is_degraded(name) {
                        let result = tool.execute(args.clone(), context).await;
                        if let Ok(ref exec_result) = result {
                            self.store_cached(cache_key, exec_result.clone());
                        }
                        Some(result)
                    } else {
                        Some(Err(crate::error::SyscityError::Validation(format!(
                            "Tool '{}' is blocked or degraded",
                            name
                        ))))
                    }
                } else {
                    None
                }
            }
        };

        // Run after-hooks
        if let Some(Ok(ref exec_result)) = execution_result {
            let _t_hook = std::time::Instant::now();
            self.active_hooks()
                .run_after(name, &args, exec_result)
                .await;
            info!("[Timing] run_after({}) done in {:?}", name, _t_hook.elapsed());
        }

        let _t_filter = std::time::Instant::now();
        let result = self
            .finalize_result(name, &args, context, execution_result)
            .await;
        info!("[Timing] filter_and_audit({}) done in {:?}", name, _t_filter.elapsed());
        result
    }

    /// Apply content filtering and audit logging to a tool execution result.
    async fn filter_and_audit(
        &self,
        name: &str,
        context: &ToolContext,
        result: Option<crate::Result<ToolExecutionResult>>,
    ) -> Option<crate::Result<ToolExecutionResult>> {
        // ── Audit: tool invocation ─────────────────────────────────────────
        if let Some(ref audit) = self.audit_log {
            let allowed = matches!(result, Some(Ok(_)));
            audit
                .log_entry(
                    crate::security::runtime_audit::AuditEventType::ToolInvocation,
                    context.user_id.clone(),
                    name.to_string(),
                    allowed,
                    format!("Tool '{}' executed", name),
                    None,
                )
                .await;
        }

        // ── Content filtering ──────────────────────────────────────────────
        let result = match result {
            Some(Ok(exec_result)) => {
                // Let the tool itself decide whether content filtering applies
                let skip_filter = self
                    .get(name)
                    .map(|t| t.skip_content_filter())
                    .unwrap_or(false);

                if skip_filter {
                    Some(Ok(exec_result))
                } else if let Some(ref filter) = self.content_filter {
                    let outcome = filter.filter_result(&exec_result);

                    // Audit: content filter action
                    if let Some(ref audit) = self.audit_log {
                        if outcome.action != crate::security::content_filter::FilterAction::Pass {
                            let details = serde_json::json!({
                                "action": format!("{:?}", outcome.action),
                                "pii_findings": outcome.pii_findings.len(),
                                "secret_findings": outcome.secret_findings.len(),
                                "summary": outcome.summary,
                            });
                            audit
                                .log_entry(
                                    crate::security::runtime_audit::AuditEventType::ContentFilter,
                                    context.user_id.clone(),
                                    name.to_string(),
                                    outcome.action
                                        != crate::security::content_filter::FilterAction::Blocked,
                                    outcome.summary.clone(),
                                    Some(details),
                                )
                                .await;
                        }
                    }

                    let filtered = crate::tools::ToolExecutionResult {
                        success: if outcome.action
                            == crate::security::content_filter::FilterAction::Blocked
                        {
                            false
                        } else {
                            outcome.success
                        },
                        output: outcome.output,
                        error: if outcome.action
                            == crate::security::content_filter::FilterAction::Blocked
                        {
                            Some(outcome.summary)
                        } else {
                            exec_result.error
                        },
                        data: outcome.data,
                        execution_time: exec_result.execution_time,
                    };
                    Some(Ok(filtered))
                } else {
                    Some(Ok(exec_result))
                }
            }
            other => other,
        };

        result
    }

    /// Run the post-execute hook chain on a finished result, then apply
    /// content filtering and audit logging.
    ///
    /// This is the single choke point every execution path funnels through
    /// (buffered, streaming, cached, and bare `execute_call`). Post-execute
    /// hooks see the raw result and may replace its output or confiscate it
    /// with corrective feedback; the content filter always has the final
    /// word on whatever the hooks leave behind.
    async fn finalize_result(
        &self,
        name: &str,
        args: &Value,
        context: &ToolContext,
        result: Option<crate::Result<ToolExecutionResult>>,
    ) -> Option<crate::Result<ToolExecutionResult>> {
        let result = match result {
            Some(Ok(exec_result)) if self.active_hooks().has_post_execute_hooks() => {
                let decision = self
                    .active_hooks()
                    .run_post_execute(name, args, &exec_result, context)
                    .await;
                if let PostExecuteDecision::Block(feedback) = &decision {
                    self.audit_post_execute_block(name, context, feedback).await;
                }
                Some(Ok(Self::apply_post_execute(name, exec_result, decision)))
            }
            other => other,
        };
        // Spill bounds whatever the hooks produced; a hook Block yields
        // success=false and is naturally skipped.
        let result = match result {
            Some(Ok(exec_result)) => {
                Some(Ok(self.maybe_spill(name, args, context, exec_result).await))
            }
            other => other,
        };
        self.filter_and_audit(name, context, result).await
    }

    /// Spill an oversized successful output to a workspace file, replacing
    /// the model-facing output with a head/tail preview plus a retrieval
    /// hint. Best-effort: a failed write keeps the original output — a
    /// successful call must never turn into a failure here.
    async fn maybe_spill(
        &self,
        name: &str,
        args: &Value,
        context: &ToolContext,
        result: ToolExecutionResult,
    ) -> ToolExecutionResult {
        let Some(threshold) = self.spill_threshold else {
            return result;
        };
        if !result.success || result.output.len() <= threshold {
            return result;
        }
        // Re-reading a previously spilled artifact is exempt: spilling it
        // again would force the model into a read -> spill -> read loop. Every
        // other oversized output (including `file_read` of a large file)
        // spills, preserving the tail.
        let root = context.workspace_root().clone();
        if super::spill::arg_targets_spilled_file(&root, args) {
            return result;
        }
        let tool_name = name.to_string();
        let output = result.output.clone();
        let spilled = tokio::task::spawn_blocking(move || {
            super::spill::spill_output(&root, &tool_name, &output, threshold)
        })
        .await;

        match spilled {
            Ok(Ok(outcome)) => {
                info!(
                    "Tool '{}' output spilled ({} bytes) to {}",
                    name,
                    outcome.total_bytes,
                    outcome.path.display()
                );
                let mut result = ToolExecutionResult {
                    output: outcome.replacement,
                    ..result
                };
                let spill_meta = serde_json::json!({
                    "path": outcome.rel_path,
                    "total_bytes": outcome.total_bytes,
                });
                result.data = Some(match result.data.take() {
                    Some(mut data) => {
                        data["spill"] = spill_meta;
                        data
                    }
                    None => serde_json::json!({ "spill": spill_meta }),
                });
                result
            }
            Ok(Err(e)) => {
                warn!("Failed to spill tool '{}' output ({}); keeping original", name, e);
                result
            }
            Err(e) => {
                warn!("Spill task failed for tool '{}' ({}); keeping original", name, e);
                result
            }
        }
    }

    /// Apply a post-execute decision to a finished result.
    fn apply_post_execute(
        name: &str,
        result: ToolExecutionResult,
        decision: PostExecuteDecision,
    ) -> ToolExecutionResult {
        match decision {
            PostExecuteDecision::Accept => result,
            PostExecuteDecision::ReplaceOutput(output) => ToolExecutionResult { output, ..result },
            PostExecuteDecision::Block(feedback) => {
                warn!("Tool '{}' result blocked by post-execute policy: {}", name, feedback);
                ToolExecutionResult {
                    success: false,
                    output: String::new(),
                    error: Some(feedback),
                    data: None,
                    execution_time: result.execution_time,
                }
            }
        }
    }

    /// Audit a post-execute block as a `ToolDeny` event — the auditable
    /// counterpart to the tool's own `ToolInvocation` entry.
    async fn audit_post_execute_block(&self, name: &str, ctx: &ToolContext, feedback: &str) {
        let Some(ref audit) = self.audit_log else {
            return;
        };
        let details = serde_json::json!({ "kind": "post_execute_block" });
        audit
            .log_entry(
                crate::security::runtime_audit::AuditEventType::ToolDeny,
                ctx.user_id.clone(),
                name.to_string(),
                false,
                format!("Tool '{}' result blocked by post-execute policy: {}", name, feedback),
                Some(details),
            )
            .await;
    }

    /// Execute a tool by name, skipping the cache layer but still running the
    /// full policy, approval, hooks, and audit pipeline.
    ///
    /// Returns `None` only when the tool name is unknown (not registered).
    /// Blocked, degraded, or policy-denied tools return `Some(Err(...))`
    /// so callers can distinguish "not found" from "rejected".
    ///
    /// This is `pub(crate)` for use by other modules in the crate
    /// (e.g. streaming execution paths) that need to bypass the cache
    /// without sacrificing safety checks, though currently no external
    /// caller exists.
    #[cfg(test)]
    pub(crate) async fn execute_no_cache(
        &self,
        name: &str,
        args: Value,
        context: &ToolContext,
    ) -> Option<crate::Result<ToolExecutionResult>> {
        // Run policy evaluation (approval, denials, hooks all handled here).
        let policy_decision = self.evaluate_policy(name, &args, context).await;
        match policy_decision {
            ToolPolicyDecision::Allow => { /* proceed */ }
            ToolPolicyDecision::Deny { reason } => {
                return Some(Err(crate::error::SyscityError::Validation(format!(
                    "Tool '{}' denied: {}",
                    name, reason
                ))));
            }
            ToolPolicyDecision::NeedsApproval { .. } => {
                // execute_no_cache does not support the full approval flow;
                // callers should use `execute()` instead.
                return Some(Err(crate::error::SyscityError::Validation(format!(
                    "Tool '{}' requires approval; use execute() instead of execute_no_cache",
                    name,
                ))));
            }
        }

        // Run before-hooks
        self.active_hooks().run_before(name, &args).await;

        // Execute the tool
        let execution_result: Option<crate::Result<ToolExecutionResult>> = {
            // Try static tools first
            if let Some(tool) = self.get(name) {
                Some(tool.execute(args.clone(), context).await)
            } else {
                // Try dynamic tools
                let dynamic_tool = self
                    .dynamic_tools
                    .read()
                    .ok()
                    .and_then(|map| map.get(name).cloned());
                if let Some(tool) = dynamic_tool {
                    if !self.is_blocked(name) && !self.is_degraded(name) {
                        Some(tool.execute(args.clone(), context).await)
                    } else {
                        Some(Err(crate::error::SyscityError::Validation(format!(
                            "Tool '{}' is blocked or degraded",
                            name
                        ))))
                    }
                } else {
                    None
                }
            }
        };

        // Run after-hooks
        if let Some(Ok(ref exec_result)) = execution_result {
            let _t_hook = std::time::Instant::now();
            self.active_hooks()
                .run_after(name, &args, exec_result)
                .await;
            info!("[Timing] run_after({}) done in {:?}", name, _t_hook.elapsed());
        }

        let _t_filter = std::time::Instant::now();
        let result = self
            .finalize_result(name, &args, context, execution_result)
            .await;
        info!("[Timing] filter_and_audit({}) done in {:?}", name, _t_filter.elapsed());
        result
    }

    /// Parse tool call arguments, handling provider-specific edge cases.
    ///
    /// Some providers (DeepSeek) append trailing text after the JSON object
    /// or emit multiple JSON values. This uses a streaming parser that
    /// extracts only the first valid JSON value and ignores trailing content.
    pub(super) fn parse_tool_args(&self, raw: &str, tool_name: &str) -> crate::Result<Value> {
        let s = raw.trim();
        if s.is_empty() {
            return Ok(serde_json::json!({}));
        }

        // Fast path: direct parse works for clean JSON.
        if let Ok(val) = serde_json::from_str::<Value>(s) {
            return Ok(val);
        }

        // Fallback: streaming parser extracts only the first JSON value,
        // ignoring any trailing text or multiple objects.
        let stream = serde_json::Deserializer::from_str(s);
        if let Some(value) = stream.into_iter::<Value>().next() {
            return match value {
                Ok(val) => Ok(val),
                Err(e) => Err(crate::error::SyscityError::Validation(format!(
                    "Invalid arguments for tool {}: {}",
                    tool_name, e
                ))),
            };
        }

        Err(crate::error::SyscityError::Validation(format!(
            "Empty arguments for tool {}",
            tool_name
        )))
    }

    /// Execute a function call from an LLM.
    /// Checks both static and dynamic registries.
    /// Enforces the timeout configured in `ToolContext`.
    pub async fn execute_call(
        &self,
        call: &FunctionCall,
        context: &ToolContext,
    ) -> crate::Result<ToolExecutionResult> {
        let args: Value = self.parse_tool_args(&call.arguments, &call.name)?;
        let tool_name = call.name.clone();
        let timeout = context.timeout();

        // Pre-execute gate: policy hooks run for every buffered tool call.
        // Uses the hooks-only variant so the built-in `requires_approval`
        // fallback is NOT activated here — without installed policy hooks a
        // `requires_approval` tool keeps its historical ungated behaviour.
        let policy_decision = self
            .evaluate_policy_hooks_only(&tool_name, &args, context)
            .await;
        match policy_decision {
            ToolPolicyDecision::Allow => {}
            ToolPolicyDecision::Deny { reason } => {
                return Err(crate::error::SyscityError::Validation(format!(
                    "Tool '{}' denied: {}",
                    tool_name, reason
                )));
            }
            ToolPolicyDecision::NeedsApproval { .. } => {
                // Route through the full execute() approval flow so the call
                // suspends for human review (mirrors execute_call_streaming).
                let result = self.execute(&tool_name, args.clone(), context).await;
                return match result {
                    Some(r) => r,
                    None => Err(crate::error::SyscityError::Validation(format!(
                        "Tool '{}' was found but could not be executed",
                        tool_name
                    ))),
                };
            }
        }

        // Try static tools first
        if let Some(tool) = self.get(&tool_name) {
            let exec_start = std::time::Instant::now();
            let exec_future = tool.execute(args.clone(), context);
            let result: crate::Result<ToolExecutionResult> =
                tokio::time::timeout(timeout, exec_future)
                    .await
                    .map_err(|_| {
                        crate::error::SyscityError::Timeout(format!(
                            "Tool '{}' timed out after {:?} (actual execution: {:?}).{}",
                            tool_name,
                            timeout,
                            exec_start.elapsed(),
                            self.uncertainty_note(&tool_name)
                        ))
                    })?;
            info!(
                "execute_call: tool={} completed in {:?} (timeout={:?})",
                tool_name,
                exec_start.elapsed(),
                timeout
            );
            return match self
                .finalize_result(&tool_name, &args, context, Some(result))
                .await
            {
                Some(r) => r,
                None => Err(crate::error::SyscityError::Validation(format!(
                    "Tool '{}' finalization failed",
                    tool_name
                ))),
            };
        }

        // Try dynamic tools
        let dynamic_tool = self
            .dynamic_tools
            .read()
            .ok()
            .and_then(|map| map.get(&tool_name).cloned());

        if let Some(tool) = dynamic_tool {
            if !self.is_blocked(&tool_name) && !self.is_degraded(&tool_name) {
                let exec_start = std::time::Instant::now();
                let exec_future = tool.execute(args.clone(), context);
                let result: crate::Result<ToolExecutionResult> =
                    tokio::time::timeout(timeout, exec_future)
                        .await
                        .map_err(|_| {
                            crate::error::SyscityError::Timeout(format!(
                                "Tool '{}' timed out after {:?} (actual execution: {:?}).{}",
                                tool_name,
                                timeout,
                                exec_start.elapsed(),
                                self.uncertainty_note(&tool_name)
                            ))
                        })?;
                info!(
                    "execute_call: tool={} completed in {:?} (timeout={:?})",
                    tool_name,
                    exec_start.elapsed(),
                    timeout
                );
                return match self
                    .finalize_result(&tool_name, &args, context, Some(result))
                    .await
                {
                    Some(r) => r,
                    None => Err(crate::error::SyscityError::Validation(format!(
                        "Tool '{}' finalization failed",
                        tool_name
                    ))),
                };
            }
        }

        Err(crate::error::SyscityError::Validation(format!(
            "Unknown tool: {}. Available tools: {}",
            tool_name,
            self.list().join(", ")
        )))
    }

    /// Execute a function call from an LLM with streaming output.
    ///
    /// Policy hooks, approval, and before-hooks are run before chunks are
    /// yielded. `on_chunk` is invoked for every [`ToolExecutionChunk`]
    /// produced by the tool. After the stream completes, after-hooks,
    /// content filtering, and audit logging are applied and the final
    /// [`ToolExecutionResult`] is returned.
    ///
    /// This method owns the tool reference internally, so it works for both
    /// static and dynamically-registered tools without lifetime issues.
    pub async fn execute_call_streaming<F, Fut>(
        &self,
        call: &FunctionCall,
        context: &ToolContext,
        mut on_chunk: F,
    ) -> crate::Result<ToolExecutionResult>
    where
        F: FnMut(ToolExecutionChunk) -> Fut + Send,
        Fut: std::future::Future<Output = ()> + Send,
    {
        let args: Value = self.parse_tool_args(&call.arguments, &call.name)?;

        let tool_name = call.name.clone();

        let policy_decision = self.evaluate_policy(&tool_name, &args, context).await;
        match policy_decision {
            // Allow → proceed to execution below.
            ToolPolicyDecision::Allow => {}
            ToolPolicyDecision::Deny { reason } => {
                return Err(crate::error::SyscityError::Validation(format!(
                    "Tool '{}' denied: {}",
                    tool_name, reason
                )));
            }
            ToolPolicyDecision::NeedsApproval { .. } => {
                // For streaming tools, fall back to buffered execution so the
                // approval flow can suspend and resume in a single future.
                let result = self.execute(&tool_name, args.clone(), context).await;
                return match result {
                    Some(Ok(exec_result)) => {
                        if !exec_result.output.is_empty() {
                            on_chunk(ToolExecutionChunk::Output(exec_result.output.clone())).await;
                        }
                        if let Some(error) = exec_result.error.clone() {
                            on_chunk(ToolExecutionChunk::Error(error)).await;
                        }
                        if let Some(data) = exec_result.data.clone() {
                            on_chunk(ToolExecutionChunk::Data(data)).await;
                        }
                        Ok(exec_result)
                    }
                    Some(Err(e)) => {
                        on_chunk(ToolExecutionChunk::Error(e.to_string())).await;
                        Err(e)
                    }
                    None => Err(crate::error::SyscityError::Validation(format!(
                        "Tool '{}' was found but could not be executed (may have been \
                         deregistered)",
                        tool_name,
                    ))),
                };
            }
        }

        // Run before-hooks.
        self.active_hooks().run_before(&tool_name, &args).await;

        // Look up the tool and consume its stream.
        let collected = if let Some(tool) = self.get(&tool_name) {
            consume_stream(tool.execute_stream(args.clone(), context), &mut on_chunk).await
        } else {
            let dynamic_tool = self
                .dynamic_tools
                .read()
                .ok()
                .and_then(|map| map.get(&tool_name).cloned());
            if let Some(tool) = dynamic_tool {
                if !self.is_blocked(&tool_name) && !self.is_degraded(&tool_name) {
                    consume_stream(tool.execute_stream(args.clone(), context), &mut on_chunk).await
                } else {
                    return Err(crate::error::SyscityError::Validation(format!(
                        "Tool '{}' is blocked or degraded",
                        tool_name
                    )));
                }
            } else {
                return Err(crate::error::SyscityError::Validation(format!(
                    "Unknown tool: {}. Available tools: {}",
                    tool_name,
                    self.list().join(", ")
                )));
            }
        };

        // Apply after-hooks, content filtering, and audit logging.
        match self
            .finalize_stream_result(&tool_name, &args, context, collected)
            .await
        {
            Some(Ok(result)) => Ok(result),
            Some(Err(e)) => Err(e),
            None => Err(crate::error::SyscityError::Validation(format!(
                "Tool '{}' finalization failed",
                tool_name
            ))),
        }
    }

    /// Apply content filtering and audit logging to a collected streaming
    /// result, and run after-hooks.
    ///
    /// This is the streaming equivalent of the post-processing performed by
    /// [`execute`](ToolRegistry::execute) after a buffered call.
    pub async fn finalize_stream_result(
        &self,
        name: &str,
        args: &Value,
        context: &ToolContext,
        collected: ToolExecutionResult,
    ) -> Option<crate::Result<ToolExecutionResult>> {
        self.active_hooks().run_after(name, args, &collected).await;
        self.finalize_result(name, args, context, Some(Ok(collected)))
            .await
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicBool, Ordering};

    use super::*;
    use crate::tools::sdk::ToolCapabilities;
    use crate::tools::PermissionsConfig;

    /// A tool whose only job is to declare retry semantics.
    struct DeclaredTool {
        name: String,
        caps: ToolCapabilities,
    }

    #[async_trait::async_trait]
    impl Tool for DeclaredTool {
        fn name(&self) -> &str {
            &self.name
        }
        fn description(&self) -> &str {
            "declares retry semantics"
        }
        fn parameters_schema(&self) -> Value {
            serde_json::json!({ "type": "object" })
        }
        fn capabilities(&self) -> ToolCapabilities {
            self.caps.clone()
        }
        async fn execute(
            &self,
            _args: Value,
            _ctx: &ToolContext,
        ) -> crate::Result<ToolExecutionResult> {
            Ok(ToolExecutionResult::success("ok"))
        }
    }

    /// The retry declaration has to be honest in the careful direction, and the
    /// timeout path is where it is quoted: a caller that has just lost a tool to
    /// a timeout is deciding whether to try again.
    #[test]
    fn retry_safety_is_declared_and_quoted_on_timeout() {
        // The default is the careful answer: a tool that says nothing is
        // assumed not to be safely repeatable.
        let default = ToolCapabilities::default();
        assert!(!default.idempotent);
        assert!(default.compensation.is_none());

        let mut registry = ToolRegistry::new();
        registry.register(Box::new(DeclaredTool {
            name: "repeatable".into(),
            caps: ToolCapabilities {
                idempotent: true,
                ..Default::default()
            },
        }));
        registry.register(Box::new(DeclaredTool {
            name: "undoable".into(),
            caps: ToolCapabilities {
                compensation: Some("delete the thing it made"),
                ..Default::default()
            },
        }));
        registry.register(Box::new(DeclaredTool {
            name: "one_way".into(),
            caps: ToolCapabilities::default(),
        }));

        assert!(registry.uncertainty_note("repeatable").contains("safe"));
        let undoable = registry.uncertainty_note("undoable");
        assert!(
            undoable.contains("delete the thing it made"),
            "the compensating action is what the caller needs: {undoable}"
        );
        let one_way = registry.uncertainty_note("one_way");
        assert!(
            one_way.contains("duplicate"),
            "a tool with no way back has to say so: {one_way}"
        );
    }

    /// The tools that act on the world declare it, so the answer does not
    /// depend on a caller's guess.
    #[test]
    fn the_consequential_tools_declare_their_retry_safety() {
        // A read is repeatable.
        assert!(
            crate::tools::grep::GrepTool::new()
                .capabilities()
                .idempotent
        );
        // A command is not, and has nothing to compensate with.
        let shell = crate::tools::shell::ShellTool::default().capabilities();
        assert!(!shell.idempotent);
        assert!(shell.compensation.is_none());
    }

    /// A minimal tool that records whether its body actually ran.
    struct SpyTool {
        name: &'static str,
        ran: Arc<AtomicBool>,
        requires_approval: bool,
    }

    #[async_trait::async_trait]
    impl Tool for SpyTool {
        fn name(&self) -> &str {
            self.name
        }
        fn description(&self) -> &str {
            "spy tool"
        }
        fn parameters_schema(&self) -> Value {
            serde_json::json!({})
        }
        async fn execute(
            &self,
            _args: Value,
            _ctx: &ToolContext,
        ) -> crate::Result<ToolExecutionResult> {
            self.ran.store(true, Ordering::SeqCst);
            Ok(ToolExecutionResult::success(format!("{} ran", self.name)))
        }
        fn capabilities(&self) -> crate::tools::sdk::ToolCapabilities {
            crate::tools::sdk::ToolCapabilities {
                requires_approval: self.requires_approval,
                ..Default::default()
            }
        }
    }

    fn spy(name: &'static str, ran: Arc<AtomicBool>) -> Box<dyn Tool> {
        Box::new(SpyTool {
            name,
            ran,
            requires_approval: false,
        })
    }

    fn approval_spy(name: &'static str, ran: Arc<AtomicBool>) -> Box<dyn Tool> {
        Box::new(SpyTool {
            name,
            ran,
            requires_approval: true,
        })
    }

    fn call(name: &str) -> FunctionCall {
        FunctionCall {
            name: name.to_string(),
            arguments: "{}".to_string(),
        }
    }

    // ── permission gate tests ─────────────────────────────────────────────

    fn shell_call(command: &str) -> FunctionCall {
        FunctionCall {
            name: "spy".to_string(),
            arguments: serde_json::json!({ "command": command }).to_string(),
        }
    }

    fn perms(mode: PermissionMode, allow_bypass: bool) -> Arc<PermissionsRuntime> {
        Arc::new(PermissionsRuntime::from_config(&PermissionsConfig {
            mode,
            allow_bypass,
            ..Default::default()
        }))
    }

    fn read_only_declared(name: &'static str) -> Box<dyn Tool> {
        Box::new(DeclaredTool {
            name: name.to_string(),
            caps: ToolCapabilities {
                read_only: true,
                ..Default::default()
            },
        })
    }

    #[tokio::test]
    async fn a_deny_rule_blocks_before_hooks_and_the_tool_body() {
        let ran = Arc::new(AtomicBool::new(false));
        let mut registry =
            ToolRegistry::new().with_permissions(perms(PermissionMode::Default, false));
        registry.register(spy("spy", ran.clone()));
        registry.permissions().reload(&PermissionsConfig {
            deny: vec!["spy".to_string()],
            ..Default::default()
        });

        // Even a hook that would deny "more loudly" must not matter: the
        // engine blocks first.
        registry.set_hooks(
            ToolHooks::new().policy(|_, _, _| async {
                ToolPolicyDecision::Deny { reason: "hook denial".into() }
            }),
        );

        let err = registry
            .execute_call(&call("spy"), &ToolContext::new("u", "conv1"))
            .await
            .expect_err("deny rule blocks");
        assert!(err.to_string().contains("deny rule"), "got {err}");
        assert!(!ran.load(Ordering::SeqCst), "the tool body must not run");
    }

    #[tokio::test]
    async fn an_allow_rule_suppresses_the_requires_approval_fallback() {
        let ran = Arc::new(AtomicBool::new(false));
        let approval_queue = Arc::new(ApprovalQueue::new());
        let mut registry = ToolRegistry::new()
            .with_approval_queue(approval_queue.clone())
            .with_permissions(perms(PermissionMode::Default, false));
        registry.register(approval_spy("spy", ran.clone()));
        registry.permissions().reload(&PermissionsConfig {
            allow: vec!["spy".to_string()],
            ..Default::default()
        });

        let result = registry
            .execute_call(&call("spy"), &ToolContext::new("u", "conv1"))
            .await
            .expect("allowed call executes without approval");
        assert!(result.success);
        assert!(ran.load(Ordering::SeqCst));
        assert!(
            approval_queue.is_empty().await,
            "an allow rule is the don't-ask-again: nothing submitted"
        );
    }

    #[tokio::test]
    async fn an_ask_rule_routes_to_the_approval_queue() {
        let ran = Arc::new(AtomicBool::new(false));
        let approval_queue = Arc::new(ApprovalQueue::new());
        let queue = approval_queue.clone();
        let mut registry = ToolRegistry::new()
            .with_approval_queue(approval_queue.clone())
            .with_permissions(perms(PermissionMode::Default, false));
        registry.register(spy("spy", ran.clone()));
        registry.permissions().reload(&PermissionsConfig {
            ask: vec!["spy".to_string()],
            ..Default::default()
        });

        let mut rx = approval_queue.event_tx.subscribe();
        let approver = tokio::spawn(async move {
            let event = rx.recv().await.expect("approval event");
            queue
                .resolve(&event.approval_id, ApprovalDecision::Approve)
                .await;
        });

        let result = registry
            .execute_call(&call("spy"), &ToolContext::new("u", "conv1"))
            .await
            .expect("ask rule suspends, then approval releases");
        assert!(result.success);
        assert!(ran.load(Ordering::SeqCst));
        approver.await.expect("approver task");
    }

    #[tokio::test]
    async fn plan_mode_hides_and_refuses_everything_not_read_only() {
        let ran = Arc::new(AtomicBool::new(false));
        let mut registry = ToolRegistry::new().with_permissions(perms(PermissionMode::Plan, false));
        registry.register(spy("mutator", ran.clone()));
        registry.register(read_only_declared("reader"));

        let ctx = ToolContext::new("u", "conv1");

        // Advertisement: the mutator is gone from the offered toolset.
        let offered: Vec<String> = registry
            .get_available(&ctx)
            .into_iter()
            .map(|d| d.name)
            .collect();
        assert!(!offered.contains(&"mutator".to_string()), "got {offered:?}");
        assert!(offered.contains(&"reader".to_string()));

        // Backstop: a call that slips through is denied with the plan message.
        let err = registry
            .execute_call(&call("mutator"), &ctx)
            .await
            .expect_err("plan mode denies a mutating call");
        assert!(err.to_string().contains("plan mode"), "got {err}");
        assert!(!ran.load(Ordering::SeqCst));

        // And the read-only tool still runs.
        let result = registry
            .execute_call(&call("reader"), &ctx)
            .await
            .expect("read-only tools run in plan mode");
        assert!(result.success);
    }

    #[tokio::test]
    async fn accept_edits_pre_approves_write_category_tools() {
        let ran = Arc::new(AtomicBool::new(false));
        let approval_queue = Arc::new(ApprovalQueue::new());
        let mut registry = ToolRegistry::new()
            .with_approval_queue(approval_queue.clone())
            .with_permissions(perms(PermissionMode::AcceptEdits, false));
        registry.register(Box::new(DeclaredTool {
            name: "writer_declared".to_string(),
            caps: ToolCapabilities {
                requires_approval: true,
                categories: vec!["file".to_string(), "write".to_string()],
                ..Default::default()
            },
        }));

        let ctx = ToolContext::new("u", "conv1");
        let result = registry
            .execute_call(&call("writer_declared"), &ctx)
            .await
            .expect("accept_edits pre-approves write-category tools");
        assert!(result.success);
        assert!(approval_queue.is_empty().await, "nothing submitted under accept_edits");
    }

    #[tokio::test]
    async fn bypass_needs_the_config_flag_and_hooks_still_win() {
        let ran = Arc::new(AtomicBool::new(false));
        let approval_queue = Arc::new(ApprovalQueue::new());
        // allow_bypass = false: bypass behaves as default.
        let runtime = Arc::new(PermissionsRuntime::from_config(&PermissionsConfig {
            mode: PermissionMode::Bypass,
            allow_bypass: false,
            ..Default::default()
        }));
        runtime.set_session_mode("conv1", Some(PermissionMode::Bypass));
        let mut registry = ToolRegistry::new()
            .with_approval_queue(approval_queue.clone())
            .with_permissions(runtime);
        registry.register(approval_spy("spy", ran.clone()));
        let ctx = ToolContext::new("u", "conv1")
            .with_ask_queue(Arc::new(crate::tools::ask_user::AskQueue::new()));

        let mut rx = approval_queue.event_tx.subscribe();
        let queue = approval_queue.clone();
        let approver = tokio::spawn(async move {
            let event = rx.recv().await.expect("fallback still asks");
            queue
                .resolve(&event.approval_id, ApprovalDecision::Approve)
                .await;
        });
        let result = registry
            .execute_call(&call("spy"), &ctx)
            .await
            .expect("without allow_bypass, bypass == default: approval flow runs");
        assert!(result.success);
        approver.await.expect("approver task");

        // Flip the flag: now the same call runs without any approval.
        registry.permissions().reload(&PermissionsConfig {
            mode: PermissionMode::Bypass,
            allow_bypass: true,
            ..Default::default()
        });
        let result = registry
            .execute_call(&call("spy"), &ctx)
            .await
            .expect("bypass skips the approval flow once allowed");
        assert!(result.success);
        assert!(approval_queue.is_empty().await, "bypass must not submit approvals");

        // But an explicit hook Deny still blocks under bypass.
        registry.set_hooks(
            ToolHooks::new().policy(|_, _, _| async {
                ToolPolicyDecision::Deny { reason: "hook denial".into() }
            }),
        );
        let err = registry
            .execute_call(&call("spy"), &ctx)
            .await
            .expect_err("hook deny wins under bypass");
        assert!(err.to_string().contains("hook denial"), "got {err}");
    }

    #[tokio::test]
    async fn a_session_mode_override_drives_the_gate() {
        let ran = Arc::new(AtomicBool::new(false));
        // Gateway default plan; this session overridden to default mode.
        let runtime = Arc::new(PermissionsRuntime::from_config(&PermissionsConfig {
            mode: PermissionMode::Plan,
            ..Default::default()
        }));
        runtime.set_session_mode("conv1", Some(PermissionMode::Default));
        let mut registry = ToolRegistry::new().with_permissions(runtime);
        registry.register(spy("mutator", ran.clone()));

        let result = registry
            .execute_call(&call("mutator"), &ToolContext::new("u", "conv1"))
            .await
            .expect("the session override exits plan mode for this session");
        assert!(result.success);
    }

    /// A `Deny` from a policy hook must block a buffered `execute_call`
    /// before the tool body runs.
    #[tokio::test]
    async fn test_execute_call_runs_policy_and_blocks() {
        let ran = Arc::new(AtomicBool::new(false));
        let mut registry =
            ToolRegistry::new().with_hooks(ToolHooks::new().policy(|name, _args, _ctx| {
                let name = name.to_string();
                async move {
                    if name == "spy" {
                        ToolPolicyDecision::Deny {
                            reason: "blocked-by-policy".into(),
                        }
                    } else {
                        ToolPolicyDecision::Allow
                    }
                }
            }));
        registry.register(spy("spy", ran.clone()));

        let err = registry
            .execute_call(&call("spy"), &ToolContext::default())
            .await
            .expect_err("should be denied");
        assert!(err.to_string().contains("blocked-by-policy"), "err: {}", err);
        assert!(!ran.load(Ordering::SeqCst), "tool body must not run when denied");
    }

    /// A `requires_approval` tool with no policy hooks and nobody to ask runs
    /// ungated through `execute_call`.
    ///
    /// This is the half that keeps unattended work moving: with no channel to
    /// put the question on, a submitted prompt could only be waited on for five
    /// minutes and then fail. The other half is
    /// [`test_execute_call_requires_approval_asks_when_someone_can_answer`].
    #[tokio::test]
    async fn test_execute_call_requires_approval_without_a_human_runs() {
        let ran = Arc::new(AtomicBool::new(false));
        let mut registry = ToolRegistry::new(); // no hooks
        registry.register(approval_spy("spy", ran.clone()));

        let result = registry
            .execute_call(&call("spy"), &ToolContext::default())
            .await
            .expect("requires_approval tool must run ungated through execute_call");
        assert!(result.success);
        assert!(ran.load(Ordering::SeqCst));
    }

    /// The same tool, in a context that has a question channel but nobody on the
    /// other end of it, still runs ungated — and submits nothing.
    ///
    /// A cron run, a heartbeat or a delegation carries an `ask_queue` in some
    /// wiring but is explicitly not an interactive human; asking there would
    /// stall the very work that has no one to answer.
    #[tokio::test]
    async fn test_execute_call_requires_approval_runs_ungated_in_a_background_context() {
        let ran = Arc::new(AtomicBool::new(false));
        let approval_queue = Arc::new(ApprovalQueue::new());
        let mut registry = ToolRegistry::new().with_approval_queue(approval_queue.clone());
        registry.register(approval_spy("spy", ran.clone()));

        let ctx = ToolContext::new("system", "cron:job-1")
            .with_ask_queue(Arc::new(crate::tools::ask_user::AskQueue::new()));

        // A five-second cap: an ungated call returns at once, and a regression
        // that submits instead would otherwise sit on the five-minute approval
        // timeout and look like a hang.
        let result =
            tokio::time::timeout(Duration::from_secs(5), registry.execute_call(&call("spy"), &ctx))
                .await
                .expect("a background context must not wait on an approval")
                .expect("requires_approval tool must run ungated where nobody can answer");

        assert!(result.success);
        assert!(ran.load(Ordering::SeqCst));
        assert!(
            approval_queue.is_empty().await,
            "no approval should have been submitted for a background context"
        );
    }

    /// With both a question channel and an interactive context, the same tool is
    /// gated: the call reaches the approval queue and only runs once approved.
    #[tokio::test]
    async fn test_execute_call_requires_approval_asks_when_someone_can_answer() {
        let ran = Arc::new(AtomicBool::new(false));
        let approval_queue = Arc::new(ApprovalQueue::new());
        let mut registry = ToolRegistry::new().with_approval_queue(approval_queue.clone());
        registry.register(approval_spy("spy", ran.clone()));

        let ctx = ToolContext::new("user1", "conv1")
            .with_ask_queue(Arc::new(crate::tools::ask_user::AskQueue::new()));

        // Approve the first request the way a UI does.
        let mut rx = approval_queue.event_tx.subscribe();
        let queue = approval_queue.clone();
        let approver = tokio::spawn(async move {
            let event = rx.recv().await.expect("approval event");
            queue
                .resolve(&event.approval_id, ApprovalDecision::Approve)
                .await;
        });

        let result = registry
            .execute_call(&call("spy"), &ctx)
            .await
            .expect("approved call should execute");
        assert!(result.success);
        assert!(ran.load(Ordering::SeqCst), "the tool must run once the approval is given");
        approver.await.expect("approver task");
    }

    /// `NeedsApproval` from a policy hook must delegate to the full
    /// `execute()` approval flow: a submitted request gets approved and the
    /// tool runs.
    #[tokio::test]
    async fn test_execute_call_needs_approval_delegates_to_approval_queue() {
        let ran = Arc::new(AtomicBool::new(false));
        let approval_queue = Arc::new(ApprovalQueue::new());
        let queue = approval_queue.clone();
        let mut registry = ToolRegistry::new()
            .with_approval_queue(approval_queue.clone())
            .with_hooks(ToolHooks::new().policy(|name, _args, _ctx| {
                let name = name.to_string();
                async move {
                    if name == "spy" {
                        ToolPolicyDecision::NeedsApproval {
                            approval_id: "req-1".into(),
                            tool_name: name,
                            args: serde_json::json!({}),
                            risk_level: crate::tools::approval::RiskLevel::High,
                            requested_by: "user1".into(),
                            message: "needs approval".into(),
                        }
                    } else {
                        ToolPolicyDecision::Allow
                    }
                }
            }));
        registry.register(spy("spy", ran.clone()));

        // Auto-approve the first submitted request.
        let mut rx = approval_queue.event_tx.subscribe();
        let approver = tokio::spawn(async move {
            let event = rx.recv().await.expect("approval event");
            queue
                .resolve(&event.approval_id, ApprovalDecision::Approve)
                .await;
        });

        let result = registry
            .execute_call(&call("spy"), &ToolContext::default())
            .await
            .expect("approved call should execute");
        assert!(result.success);
        assert!(ran.load(Ordering::SeqCst), "tool must run after approval");
        approver.await.expect("approver task");
    }

    /// `NeedsApproval` with no approval queue must surface `execute()`'s
    /// error — proving the call delegated to the full approval flow rather
    /// than the ungated path.
    #[tokio::test]
    async fn test_execute_call_needs_approval_no_queue_errors() {
        let ran = Arc::new(AtomicBool::new(false));
        let mut registry =
            ToolRegistry::new().with_hooks(ToolHooks::new().policy(|_name, _args, _ctx| async {
                ToolPolicyDecision::NeedsApproval {
                    approval_id: "req-1".into(),
                    tool_name: "spy".into(),
                    args: serde_json::json!({}),
                    risk_level: crate::tools::approval::RiskLevel::High,
                    requested_by: "user1".into(),
                    message: "needs approval".into(),
                }
            }));
        registry.register(spy("spy", ran.clone()));

        let err = registry
            .execute_call(&call("spy"), &ToolContext::default())
            .await
            .expect_err("must delegate to execute() which fails without a queue");
        assert!(err.to_string().contains("no approval queue"), "err: {}", err);
        assert!(!ran.load(Ordering::SeqCst), "tool must not run");
    }

    /// A description override must replace the static description in every
    /// emitted `FunctionDefinition` (§十一).
    #[test]
    fn description_override_replaces_static_description() {
        let ran = Arc::new(AtomicBool::new(false));
        let mut registry = ToolRegistry::new();
        registry.register(spy("spy", ran));

        assert_eq!(registry.get_definitions()[0].description, "spy tool");

        registry.set_metadata(
            "spy",
            crate::tools::metadata::ToolDescriptionMeta::new(
                1,
                "renamed: reports what the spy tool does",
            ),
        );
        let def = registry
            .get_definitions()
            .into_iter()
            .find(|d| d.name == "spy")
            .expect("spy tool present");
        assert_eq!(def.description, "renamed: reports what the spy tool does");

        // `get_available` honors the override too.
        let ctx = ToolContext::default();
        let avail = registry
            .get_available(&ctx)
            .into_iter()
            .find(|d| d.name == "spy")
            .expect("spy tool available");
        assert_eq!(avail.description, "renamed: reports what the spy tool does");
    }
}
