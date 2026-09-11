//! [`Agent`] construction, builder-style setters, and fresh-context
//! building.

use std::collections::HashMap;
use std::sync::atomic::AtomicU64;
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::{Mutex, RwLock};

use tracing::{debug, info, warn};

use crate::channels::thread_binding::ThreadBindingManager;
use crate::providers::{CompletionRequest, Provider};
use crate::tools::{ToolContext, ToolRegistry};

use super::*;

impl Agent {
    /// Create a new Agent
    pub fn new(config: AgentConfig, provider: Arc<dyn Provider>, tools: Arc<ToolRegistry>) -> Self {
        let provider_clone = provider.clone();
        let retrospect_engine = config.reflection_config.as_ref().and_then(|rc| {
            if rc.retrospect_enabled {
                Some(reflection::RetrospectEngine::new(rc.retrospect.clone(), provider.clone()))
            } else {
                None
            }
        });
        let agent_id = config.agent_id.clone().unwrap_or_default();

        Self {
            paths: crate::dirs::paths(),
            config: config.into(),
            agent_id,
            provider,
            model: None,
            tools,
            thread_map: Arc::new(Mutex::new(HashMap::new())),
            session_store: None,
            session_id: None,
            shutdown_tx: Arc::new(RwLock::new(None)),
            memory_manager: None,
            memory_store: None,
            chat_history: None,
            session_search: None,
            response_cache: Arc::new(ResponseCache::new(Duration::from_secs(3600))), // 1 hour TTL
            task_planner: Arc::new(TaskPlanner::new(provider_clone)),
            active_plans: Arc::new(RwLock::new(std::collections::HashMap::new())),
            cost_guard: None,
            active_skill_trust: std::sync::Arc::new(std::sync::atomic::AtomicU8::new(1)), // Trusted
            skill_manager: None,
            execution_controller: Arc::new(RwLock::new(None)),
            max_tool_iterations_override: Arc::new(RwLock::new(None)),
            transcript_store: None,
            artifact_store: None,
            disk_budget: None,
            session_file_manager: None,
            extra_params: Arc::new(RwLock::new(None)),
            model_router: None,
            model_override: Arc::new(RwLock::new(None)),
            session_models: Arc::new(RwLock::new(HashMap::new())),
            plans_dir: None,
            pii_detector: None,
            computer_adapter: None,
            computer_config: None,
            goal_planner: None,
            retrospect_engine,
            retrospect_counter: Arc::new(AtomicU64::new(0)),
            thread_binding_manager: None,
            concurrency_guards: Arc::new(Mutex::new(HashMap::new())),
            risk_checker: None,
            pending_badcase_store: None,
            online_monitoring: crate::gateway::config::OnlineMonitoringConfig::default(),
            compression_quality: crate::gateway::config::CompressionQualityConfig::default(),
            sample_store: None,
            sampling: crate::gateway::config::OnlineSamplingConfig::default(),
        }
    }

    /// Root this agent's workspace/delegation directories resolve against.
    pub fn with_paths(mut self, paths: Arc<crate::dirs::SyscityPaths>) -> Self {
        self.paths = paths;
        self
    }

    /// Set provider-specific extra parameters (e.g. thinking config) to inject
    /// into every completion request.
    pub async fn set_extra_params(&self, params: Option<serde_json::Value>) {
        let mut guard = self.extra_params.write().await;
        *guard = params;
    }

    /// Set a temporary model override for the next provider call.
    ///
    /// Cleared automatically after each `process_message` invocation
    /// so that subsequent requests revert to the default model.
    pub async fn set_model_override(&self, model: Option<String>) {
        let mut guard = self.model_override.write().await;
        *guard = model;
    }

    /// Set or clear the concrete model ID for one conversation (session-scoped
    /// model binding). Unlike `set_model_override`, this is keyed by
    /// conversation id, so concurrent sessions on this shared agent do not
    /// interfere.
    pub async fn set_session_model(&self, conversation_id: &str, model: Option<String>) {
        let mut guard = self.session_models.write().await;
        match model {
            Some(m) => {
                guard.insert(conversation_id.to_string(), m);
            }
            None => {
                guard.remove(conversation_id);
            }
        }
    }

    /// Patch a [`CompletionRequest`] with provider-specific reasoning
    /// parameters when the target model is a known reasoning / thinking
    /// model and no explicit reasoning config has already been supplied via
    /// `extra`.
    pub(super) fn patch_request_for_reasoning(&self, request: &mut CompletionRequest) {
        let family = self.provider.stream_family();
        let model = request
            .model
            .as_deref()
            .or(self.model.as_deref())
            .unwrap_or_else(|| self.provider.default_model());

        // Skip if user already provided explicit reasoning config in extra
        let has_reasoning_config = request.extra.as_ref().is_some_and(|v| {
            v.get("reasoning_effort").is_some()
                || v.get("thinking").is_some()
                || v.get("thinkingConfig").is_some()
        });
        if has_reasoning_config {
            return;
        }

        match family {
            crate::providers::stream_wrappers::ProviderStreamFamily::OpenAi
            | crate::providers::stream_wrappers::ProviderStreamFamily::OpenAiReasoning => {
                // OpenAI o1 / o3 series use `reasoning_effort`
                if model.starts_with("o1") || model.starts_with("o3") {
                    request.extra = Some(
                        serde_json::json!({ "reasoning_effort": "medium", "service_tier": "auto" }),
                    );
                }
                // DeepSeek thinking models (via OpenAI-compatible API) require
                // `reasoning` parameter so that the API accepts reasoning_content
                // in conversation history.
                else if model.starts_with("deepseek") {
                    request.extra = Some(serde_json::json!({ "reasoning": { "enabled": true } }));
                }
            }
            crate::providers::stream_wrappers::ProviderStreamFamily::Anthropic
            | crate::providers::stream_wrappers::ProviderStreamFamily::AnthropicThinking => {
                // Anthropic thinking models (claude-3-7-sonnet-thinking, etc.)
                if model.contains("thinking") || model.contains("-extended-thinking") {
                    request.extra = Some(serde_json::json!({
                        "thinking": { "type": "enabled", "budget_tokens": 16000 }
                    }));
                }
            }
            crate::providers::stream_wrappers::ProviderStreamFamily::GoogleThinking
                if model.contains("thinking") || model.contains("-exp") =>
            {
                request.extra =
                    Some(serde_json::json!({ "thinkingConfig": { "thinkingBudget": 16000 } }));
            }
            _ => {}
        }
    }

    /// Set the skill trust level for the next process_message invocation.
    ///
    /// Call this before invoking `process_message` or
    /// `process_message_with_progress` to constrain which tools the agent
    /// may call. The gateway resets this to `Trusted` after the invocation
    /// completes.
    pub fn set_skill_trust(&self, trust: crate::tools::SkillTrust) {
        use std::sync::atomic::Ordering;
        self.active_skill_trust
            .store(trust as u8, Ordering::Relaxed);
    }

    /// Read the current active skill trust level from the atomic.
    pub(super) fn current_skill_trust(&self) -> crate::tools::SkillTrust {
        use std::sync::atomic::Ordering;
        match self.active_skill_trust.load(Ordering::Relaxed) {
            0 => crate::tools::SkillTrust::Community,
            _ => crate::tools::SkillTrust::Trusted,
        }
    }

    fn infer_model_vision(&self) -> bool {
        self.model
            .as_deref()
            .map(|m| {
                let m = m.to_lowercase();
                m.contains("vision")
                    || m.contains("claude-3")
                    || m.contains("gpt-4o")
                    || m.contains("gemini-pro-vision")
                    || m.contains("llava")
            })
            .unwrap_or(false)
    }

    /// Build a ToolContext pre-configured with workspace settings from agent
    /// config.
    ///
    /// `delegation` carries the active delegation scope (if any) so tools like
    /// `task_state` can address the child's shared task row.  `None` for
    /// ordinary conversations.
    pub(super) fn build_tool_context(
        &self,
        user_id: impl Into<String>,
        conversation_id: impl Into<String>,
        delegation: Option<crate::delegation::DelegationScope>,
    ) -> ToolContext {
        let user_id = user_id.into();
        let conversation_id = conversation_id.into();

        let cfg = self.config_snapshot();

        let model_capabilities = crate::tools::ModelCapabilities {
            has_vision: self.infer_model_vision(),
            supports_tool_use: self.provider.supports_tools(),
            max_context_length: None,
        };

        let agent_workspace = cfg.resolve_workspace_dir(&self.paths);

        let mut ctx = ToolContext::new(user_id.clone(), conversation_id)
            .with_timeout(Duration::from_secs(120))
            .with_skill_trust(self.current_skill_trust())
            .with_workspace_root(agent_workspace.clone())
            .with_agent_workspace(agent_workspace.clone())
            .with_workspace_only(cfg.workspace_only)
            .with_model_name(self.model.clone().unwrap_or_default())
            .with_provider_name(self.provider.name().to_string())
            .with_sender_id(user_id)
            .with_model_capabilities(model_capabilities);

        // A delegated child operates inside its delegation tree's shared
        // workspace: relative file paths resolve into this task's scratch dir,
        // while the whole tree plus the agent's own workspace stay reachable by
        // absolute path.  Other agents' workspaces are not granted, preserving
        // cross-agent isolation.
        if let Some(scope) = delegation.as_ref() {
            let task_dir = self
                .paths
                .delegation_task_dir(&scope.root_id, &scope.task_id);
            ctx = ctx
                .with_workspace_root(task_dir)
                .allow_path(agent_workspace)
                .allow_path(self.paths.delegation_workspace_dir(&scope.root_id));
        }

        // Carry the shared ask-user queue so `ask_user` can block for a human
        // answer.  `None` (unit tests, goal runner's own context) means the
        // tool refuses via its guard.
        let ctx = ctx.with_delegation(delegation);
        match self.tools.ask_queue() {
            Some(queue) => ctx.with_ask_queue(Arc::clone(queue)),
            None => ctx,
        }
    }

    /// Attach a `SessionStore` for turn persistence.
    ///
    /// When set, every completed turn is persisted asynchronously and the
    /// conversation history can be restored across restarts via
    /// [`Agent::restore_threads`].
    pub fn with_session_store(
        mut self,
        store: Arc<SessionStore>,
        session_id: impl Into<String>,
    ) -> Self {
        self.session_store = Some(store);
        self.session_id = Some(session_id.into());
        self
    }

    /// Attach a `CostGuard` to this agent.  When set, every provider call
    /// first checks `cost_guard.is_exceeded()` and returns an error if the
    /// budget has been exhausted.
    pub fn with_cost_guard(mut self, guard: Arc<CostGuard>) -> Self {
        self.cost_guard = Some(guard);
        self
    }

    /// Set the model name to use for completions
    pub fn with_model(mut self, model: impl Into<String>) -> Self {
        let model = model.into();
        self.model = Some(model.clone());
        // Update task planner with the model
        let provider = self.provider.clone();
        self.task_planner = Arc::new(TaskPlanner::new(provider).with_model(model));
        self
    }

    /// Set the memory store
    pub fn with_memory_store(mut self, store: Arc<dyn crate::memory::MemoryStore>) -> Self {
        self.memory_store = Some(store);
        self
    }

    /// Set the chat history store
    pub fn with_chat_history(mut self, store: Arc<dyn crate::memory::ChatHistoryStore>) -> Self {
        self.chat_history = Some(store);
        self
    }

    /// Set the session search for conversation indexing
    pub fn with_session_search(mut self, search: Arc<crate::memory::SessionSearch>) -> Self {
        self.session_search = Some(search);
        self
    }

    /// Set the memory manager for unified memory operations.
    ///
    /// The memory manager provides retrieval, storage, and compaction
    /// capabilities. When set, it takes precedence over the legacy
    /// memory_store and chat_history fields.
    pub fn with_memory_manager(mut self, manager: Arc<crate::memory::MemoryManager>) -> Self {
        self.memory_manager = Some(manager);
        self
    }

    /// Set the skill manager for deterministic skill prefiltering.
    ///
    /// When set, the agent will dynamically filter skills based on trigger
    /// patterns (regex, keywords, commands) before including them in the
    /// system prompt. This reduces token usage and improves relevance by
    /// only including skills that match the user's message.
    pub fn with_skill_manager(mut self, manager: Arc<RwLock<crate::skills::SkillManager>>) -> Self {
        self.skill_manager = Some(manager);
        self
    }

    /// Set the directory for persisting active plans.
    pub fn with_plans_dir(mut self, dir: std::path::PathBuf) -> Self {
        self.plans_dir = Some(dir);
        self
    }

    /// Attach a computer adapter for desktop automation.
    ///
    /// When set, the agent can detect desktop-operation tasks and launch
    /// the [`ComputerUseLoop`] to interact with the GUI.
    /// Also creates a [`GoalPlanner`] for complex multi-step tasks.
    pub fn with_computer_adapter(
        mut self,
        adapter: Arc<dyn crate::computer::ComputerAdapter>,
    ) -> Self {
        self.computer_adapter = Some(adapter.clone());
        // Auto-create GoalPlanner when adapter + provider are both available.
        let mut planner =
            crate::planner::GoalPlanner::with_provider(adapter, self.provider.clone());
        if let Some(ref memory) = self.memory_store {
            planner = planner.with_memory(memory.clone());
        }
        self.goal_planner = Some(Arc::new(planner));
        self
    }

    /// Set the configuration for the computer use loop.
    pub fn with_computer_config(mut self, config: crate::computer::LoopConfig) -> Self {
        self.computer_config = Some(config);
        self
    }

    /// Attach a persistent state store to the goal planner for crash recovery.
    pub fn with_planner_state_store(mut self, store: crate::planner::TaskStateStore) -> Self {
        if let Some(ref mut planner) = self.goal_planner {
            let updated = (**planner).clone().with_state_store(store);
            *planner = Arc::new(updated);
        }
        self
    }

    /// Attach a thread binding manager for tracking session/thread hierarchy.
    pub fn with_thread_binding_manager(mut self, manager: ThreadBindingManager) -> Self {
        self.thread_binding_manager = Some(manager);
        self
    }

    /// Persist all active plans to disk.
    pub async fn save_all_plans(&self) -> crate::Result<()> {
        let Some(ref dir) = self.plans_dir else {
            return Ok(());
        };
        let plans = self.active_plans.read().await;
        for (conv_id, active) in plans.iter() {
            let snapshot = PersistedPlan::from_active(active);
            let path = dir.join(format!("{}.json", conv_id));
            if let Err(e) = snapshot.persist_to(&path).await {
                warn!("Failed to persist plan for {}: {}", conv_id, e);
            }
        }
        debug!("Persisted {} plans to {:?}", plans.len(), dir);
        Ok(())
    }

    /// Load previously persisted plans from disk and restore them.
    pub async fn load_plans(&self) -> crate::Result<usize> {
        let Some(ref dir) = self.plans_dir else {
            return Ok(0);
        };
        let persisted = planner::load_all_plans(dir).await?;
        let mut plans = self.active_plans.write().await;
        let mut count = 0;
        for pp in persisted {
            let conv_id = pp.plan.id.clone();
            let active = pp.into_active(&self.task_planner);
            plans.insert(conv_id, active);
            count += 1;
        }
        if count > 0 {
            info!("Restored {} plans from {:?}", count, dir);
        }
        Ok(count)
    }

    /// Attach a `TranscriptStore` for conversation recording.
    ///
    /// When set, every user and assistant message is appended to a
    /// per-session transcript that can be exported in multiple formats.
    pub fn with_transcript_store(mut self, store: Arc<crate::agent::TranscriptStore>) -> Self {
        self.transcript_store = Some(store);
        self
    }

    /// Attach an `ArtifactStore` for session-bound artifacts.
    ///
    /// When set, code blocks and documents produced during tool execution
    /// are automatically captured as artifacts.
    pub fn with_artifact_store(mut self, store: Arc<crate::agent::ArtifactStore>) -> Self {
        self.artifact_store = Some(store);
        self
    }

    /// Attach a `DiskBudgetManager` for per-session storage quota.
    ///
    /// When set, file operations are checked against the session's
    /// disk budget before proceeding.
    pub fn with_disk_budget(mut self, budget: Arc<crate::agent::DiskBudgetManager>) -> Self {
        self.disk_budget = Some(budget);
        self
    }

    /// Attach a `SessionFileManager` for isolated per-session file ops.
    ///
    /// When set, each conversation gets its own scoped directory.
    pub fn with_session_file_manager(
        mut self,
        manager: Arc<crate::agent::SessionFileManager>,
    ) -> Self {
        self.session_file_manager = Some(manager);
        self
    }

    /// Attach a `ModelRouter` for advanced routing, key rotation, and fallback.
    pub fn with_model_router(mut self, router: Arc<crate::model_router::ModelRouter>) -> Self {
        self.model_router = Some(router);
        self
    }

    /// Attach a PII detector for output content filtering.
    pub fn with_pii_detector(mut self, detector: Arc<crate::security::PiiDetector>) -> Self {
        self.pii_detector = Some(detector);
        self
    }

    /// Enable the online badcase auto-collection pipeline.
    ///
    /// When both the risk checker and the pending store are attached, every
    /// completed turn is scanned post-hoc and turns that trip a risk signal
    /// are inserted into the pending-badcase pool (source `online:risk`).
    pub fn with_badcase_pipeline(
        mut self,
        risk_checker: crate::eval::RiskSignalChecker,
        store: Arc<crate::eval::PendingBadcaseStore>,
    ) -> Self {
        self.risk_checker = Some(risk_checker);
        self.pending_badcase_store = Some(store);
        self
    }

    /// Set the 压缩质量门禁（§三）configuration snapshot.
    ///
    /// When `enabled`, turns whose context compression drops below
    /// `min_retention_ratio` are collected into the pending-badcase pool as
    /// `online:risk` signals. Disabled by default — existing deployments see
    /// no behavioral change.
    pub fn with_compression_quality(
        mut self,
        cfg: crate::gateway::config::CompressionQualityConfig,
    ) -> Self {
        self.compression_quality = cfg;
        self
    }

    /// Attach the production turn sampling store.
    ///
    /// When attached (and [`Agent::with_sampling`] enables it), a sampled
    /// subset of completed turns is persisted to `turn_samples` for the
    /// scoring / compression-gate / feedback-aggregation / shadow-replay
    /// pipelines. Accepts `None` so the gateway can wire the optional infra
    /// store unconditionally.
    pub fn with_sample_store(mut self, store: Option<Arc<crate::eval::TurnSampleStore>>) -> Self {
        self.sample_store = store;
        self
    }

    /// Set the 生产流量在线采样（§…）configuration snapshot.
    ///
    /// When `enabled` (and a sample store is attached), every completed turn
    /// is persisted to the sample store — optionally gated by `sample_rate`.
    /// Disabled by default — existing deployments see no behavioral change.
    pub fn with_sampling(mut self, cfg: crate::gateway::config::OnlineSamplingConfig) -> Self {
        self.sampling = cfg;
        self
    }

    /// Snapshot the agent's current configuration.
    ///
    /// Config reads take a copy at the start of each request/context build, so
    /// a config update mid-turn is only picked up from the next turn onward.
    pub(crate) fn config_snapshot(&self) -> AgentConfig {
        self.config.snapshot()
    }

    /// Update agent configuration at runtime.
    ///
    /// Applies fields from `new_config` to the running agent.  The update is
    /// applied immediately; in-flight requests use the previous values.
    pub fn update_config(&self, new_config: AgentConfig) {
        self.config.replace(new_config);
    }

    /// Get chat history for a conversation
    pub async fn get_chat_history(
        &self,
        conversation_id: &str,
        limit: usize,
    ) -> crate::Result<Vec<crate::memory::ChatMessage>> {
        if let Some(ref store) = self.chat_history {
            store.get_conversation_history(conversation_id, limit).await
        } else {
            Ok(Vec::new())
        }
    }

    /// Get the last conversation ID for a user
    pub async fn get_last_conversation(&self, user_id: &str) -> crate::Result<Option<String>> {
        if let Some(ref store) = self.chat_history {
            store.get_last_conversation(user_id).await
        } else {
            Ok(None)
        }
    }

    /// Build a fresh `Context` for a new conversation thread.
    ///
    /// This is called only when no existing [`Thread`] is found for a
    /// `conversation_id`.  It constructs the system prompt, applies token
    /// limits and dynamic tool iteration caps, but does NOT store anything
    /// — callers are responsible for wrapping the returned `Context` in a
    /// `Thread` and inserting it into `thread_map`.
    pub(super) async fn build_fresh_context(
        &self,
        conversation_id: &str,
        user_id: &str,
        user_message: &str,
    ) -> Context {
        let cfg = self.config_snapshot();
        // Build dynamic prompt context
        let mut prompt_ctx = PromptContext::new(user_message);
        prompt_ctx.detect_task_type();
        // New thread → no prior history; phase starts at Initial.
        prompt_ctx = prompt_ctx.set_phase(0);

        // Check for active plan
        let active_plans = self.active_plans.read().await;
        if let Some(active_plan) = active_plans.get(conversation_id) {
            if let Some(task_prompt) = active_plan.current_task_prompt() {
                prompt_ctx.task_context = Some(task_prompt);
            }
        }
        drop(active_plans);

        // Get available tools.  Fresh contexts have no delegation scope yet —
        // the scope is applied later from message metadata in the engine.
        let tool_context = self.build_tool_context(user_id, conversation_id, None);
        let tool_defs = self.tools.get_available(&tool_context);
        prompt_ctx.available_tools = tool_defs;

        // Get base prompt
        let base_prompt = cfg.full_system_prompt_with_personality().await;

        // Derive KB collection from agent_id (e.g. "kb-sre")
        let kb_collection = cfg.agent_id.as_ref().map(|id| format!("kb-{}", id));

        // Retrieve relevant memories via MemoryManager and inject into context
        let memory_context = if let Some(ref mm) = self.memory_manager {
            match mm
                .session_context(
                    user_id,
                    conversation_id,
                    Some(user_message),
                    kb_collection.as_deref(),
                )
                .await
            {
                Ok(ctx) => {
                    let formatted = ctx.format_for_injection();
                    if formatted.is_empty() {
                        None
                    } else {
                        Some(formatted)
                    }
                }
                Err(e) => {
                    tracing::warn!("Memory context retrieval failed: {}", e);
                    None
                }
            }
        } else {
            None
        };

        // Build dynamic system prompt BEFORE memory/skills injection so that
        // task-specific guidance (phase, task type, tool relevance) takes
        // priority over supporting context.
        let base_with_dynamic = PromptBuilder::build_from_context(
            &base_prompt,
            &prompt_ctx,
            cfg.max_context_tokens / 4,
        );

        // Combine with memory context and skills
        let full_prompt = {
            let mut prompt = base_with_dynamic;

            // Add memory context if available
            if let Some(ref mem_ctx) = memory_context {
                prompt = format!("{}\n\n{}", prompt, mem_ctx);
            }

            // Add the stable skills catalog (name + description only).
            // Skill bodies are loaded on demand via the `skill` tool, so
            // the system prompt prefix stays identical across messages and
            // provider prompt caches remain effective.
            if let Some(ref skill_manager) = self.skill_manager {
                let mgr = skill_manager.read().await;
                let catalog = mgr.build_catalog().await;
                if !catalog.is_empty() {
                    prompt = format!("{}\n\n{}", prompt, catalog);
                }
            } else if let Some(ref static_skills) = cfg.skills_prompt {
                // Fallback to static skills prompt if skill_manager not set
                prompt = format!("{}\n\n{}", prompt, static_skills);
            }

            prompt
        };

        let mut context =
            Context::new(conversation_id.to_string(), full_prompt, cfg.max_context_tokens);

        // Apply turn cap from config so the agent never accumulates an
        // unbounded conversation history.
        if let Some(max_turns) = cfg.max_turns {
            context = context.with_max_turns(max_turns);
        }

        // Restore prior conversation messages so the LLM sees real history
        // instead of relying only on the memory-injected summary (which can
        // leak wrong/stale context from other sessions).
        if let Some(ref store) = self.chat_history {
            let limit = cfg.max_turns.unwrap_or(50) * 2;
            // A durable compaction record rehydrates as `[summary] + tail`
            // instead of the full history, keeping the token mask effective
            // across restarts.
            let compaction = match store.get_compaction(conversation_id).await {
                Ok(c) => c,
                Err(e) => {
                    tracing::warn!(
                        "Failed to load compaction record for {}: {}",
                        conversation_id,
                        e
                    );
                    None
                }
            };
            let mut summary: Option<String> = None;
            let history = match &compaction {
                Some(comp) => {
                    match store
                        .get_conversation_history_since(
                            conversation_id,
                            &comp.boundary_role,
                            &comp.boundary_content,
                            limit,
                        )
                        .await
                    {
                        Ok(tail) if !tail.is_empty() => {
                            summary = Some(comp.summary.clone());
                            Ok(tail)
                        }
                        Ok(_) => {
                            tracing::warn!(
                                "Could not locate compaction boundary for {} (role={}, content={:?}) — falling back to full history",
                                conversation_id,
                                comp.boundary_role,
                                comp.boundary_content
                            );
                            store.get_conversation_history(conversation_id, limit).await
                        }
                        Err(e) => {
                            tracing::warn!(
                                "Failed to load compacted history for {}: {} — falling back to full history",
                                conversation_id,
                                e
                            );
                            store.get_conversation_history(conversation_id, limit).await
                        }
                    }
                }
                None => store.get_conversation_history(conversation_id, limit).await,
            };

            match history {
                Ok(history) => {
                    if let Some(ref summary) = summary {
                        let mut summary_msg = crate::providers::Message::system(summary.clone());
                        summary_msg.name = Some("compaction_summary".to_string());
                        context.add_message(summary_msg);
                    }
                    for msg in history {
                        let role = match msg.role.as_str() {
                            "user" => crate::providers::Role::User,
                            "assistant" => crate::providers::Role::Assistant,
                            _ => continue,
                        };
                        let mut message = crate::providers::Message {
                            role,
                            content: msg.content,
                            content_blocks: None,
                            reasoning_content: None,
                            name: None,
                            tool_calls: None,
                            tool_call_id: None,
                            metadata: None,
                        };
                        if role == crate::providers::Role::User && !msg.user_id.is_empty() {
                            message.name = Some(msg.user_id);
                        }
                        context.add_message(message);
                    }
                }
                Err(e) => {
                    tracing::warn!(
                        "Failed to load conversation history for {}: {}",
                        conversation_id,
                        e
                    );
                }
            }
        }

        // Set dynamic tool iteration limit based on message complexity
        let dynamic_limit = Context::calculate_dynamic_limit(user_message);
        context.set_max_tool_iterations(dynamic_limit);
        info!(
            "Set dynamic tool iteration limit: {} for conversation {}",
            dynamic_limit, conversation_id
        );

        // Apply ACP max iteration override if set
        let override_opt = *self.max_tool_iterations_override.read().await;
        if let Some(max_iter) = override_opt {
            context.set_max_tool_iterations(max_iter);
            info!(
                "Applied ACP max iteration override: {} for conversation {}",
                max_iter, conversation_id
            );
        }

        context
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::agent::{Agent, AgentConfig};
    use crate::delegation::DelegationScope;
    use crate::providers::mock::MockProvider;
    use crate::tools::ToolRegistry;

    fn named_agent() -> Agent {
        let config = AgentConfig {
            agent_id: Some("worker".to_string()),
            ..AgentConfig::default()
        };
        let provider = Arc::new(MockProvider::new());
        Agent::new(config, provider, Arc::new(ToolRegistry::new()))
    }

    fn scope(task_id: &str) -> DelegationScope {
        DelegationScope::new("root-1", task_id, 2, 3)
    }

    #[test]
    fn test_agent_id_populated_from_config() {
        let agent = named_agent();
        assert_eq!(agent.agent_id, "worker");
    }

    #[tokio::test]
    async fn test_session_models_are_per_conversation() {
        let agent = named_agent();

        // Two concurrent sessions on the shared agent bind different models.
        agent
            .set_session_model("conv-a", Some("alt".to_string()))
            .await;
        agent
            .set_session_model("conv-b", Some("fast".to_string()))
            .await;

        let map = agent.session_models.read().await;
        assert_eq!(map.get("conv-a").map(String::as_str), Some("alt"));
        assert_eq!(map.get("conv-b").map(String::as_str), Some("fast"));

        // Clearing one conversation does not disturb the other.
        drop(map);
        agent.set_session_model("conv-a", None).await;
        let map = agent.session_models.read().await;
        assert_eq!(map.get("conv-a"), None);
        assert_eq!(map.get("conv-b").map(String::as_str), Some("fast"));
    }

    #[test]
    fn test_delegated_child_gets_tree_workspace() {
        let agent = named_agent();
        let ctx = agent.build_tool_context("user", "conv-1", Some(scope("run-1")));

        // Relative paths resolve into the task's scratch dir inside the tree.
        let task_dir = crate::dirs::delegation_task_dir("root-1", "run-1");
        assert_eq!(ctx.workspace_root(), &task_dir);
        assert_eq!(ctx.resolve_path(std::path::Path::new("draft.md")), task_dir.join("draft.md"));

        // The whole tree plus the agent's own workspace are reachable.
        let agent_workspace = crate::dirs::agent_workspace_dir("worker");
        let allowed = ctx.allowed_paths();
        assert!(allowed.contains(&crate::dirs::delegation_workspace_dir("root-1")));
        assert!(allowed.contains(&agent_workspace));
        assert!(ctx.is_path_allowed(&crate::dirs::delegation_shared_dir("root-1")));
        assert!(ctx.is_path_allowed(&task_dir));
    }

    #[test]
    fn test_delegated_child_cannot_reach_other_agents_workspace() {
        let agent = named_agent();
        let ctx = agent.build_tool_context("user", "conv-1", Some(scope("run-1")));

        // Another agent's workspace is not in the allowlist, so a path there
        // is rejected — cross-agent isolation holds even inside a tree.
        let other = crate::dirs::agent_workspace_dir("other").join("secret.md");
        assert!(!ctx.is_path_allowed(&other));
        // Nor can it reach another delegation tree.
        let other_tree = crate::dirs::delegation_workspace_dir("root-other").join("x.md");
        assert!(!ctx.is_path_allowed(&other_tree));
    }

    #[test]
    fn test_ordinary_context_unaffected() {
        let agent = named_agent();
        let ctx = agent.build_tool_context("user", "conv-1", None);

        // No delegation → workspace stays the agent's own, no allowlist.
        assert_eq!(ctx.workspace_root(), &crate::dirs::agent_workspace_dir("worker"));
        assert!(ctx.allowed_paths().is_empty());
    }

    #[tokio::test]
    async fn test_build_fresh_context_rehydrates_compaction_boundary() {
        use crate::memory::{ChatHistoryStore, ChatMessage};
        let store = Arc::new(crate::memory::DatabaseStore::new_in_memory().await.unwrap());
        let agent = Agent::new(
            AgentConfig::default(),
            Arc::new(MockProvider::new()),
            Arc::new(ToolRegistry::new()),
        )
        .with_chat_history(store.clone());

        // Populate persisted history and record a compaction whose boundary is
        // the final user turn — everything before it is "masked" by the summary.
        for i in 0..6 {
            store
                .store_message(ChatMessage::new(
                    "conv-c",
                    "u",
                    "user",
                    format!("original user {i}"),
                ))
                .await
                .unwrap();
            store
                .store_message(ChatMessage::new(
                    "conv-c",
                    "u",
                    "assistant",
                    format!("original assistant {i}"),
                ))
                .await
                .unwrap();
        }
        store
            .record_compaction("conv-c", "user", "original user 5", "DUMMY SUMMARY")
            .await
            .unwrap();

        let ctx = agent.build_fresh_context("conv-c", "u", "follow up").await;

        // Rehydration replays `[summary] + tail` instead of the full history.
        let messages = ctx.history();
        assert_eq!(messages[0].role, crate::providers::Role::System);
        assert_eq!(messages[0].name.as_deref(), Some("compaction_summary"));
        assert!(messages[0].content.contains("DUMMY SUMMARY"));
        let tail: Vec<(crate::providers::Role, &str)> = messages[1..]
            .iter()
            .map(|m| (m.role, m.content.as_str()))
            .collect();
        assert_eq!(
            tail,
            vec![
                (crate::providers::Role::User, "original user 5"),
                (crate::providers::Role::Assistant, "original assistant 5"),
            ],
            "only the tail after the boundary is replayed"
        );
    }
}
