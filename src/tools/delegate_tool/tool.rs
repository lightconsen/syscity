//! The `delegate` tool: spawning children, waiting on them, and reporting.

use super::*;

/// Delegate tool for spawning child agents
pub struct DelegateTool {
    pub(super) tracker: DelegationTracker,
    /// Optional agent for executing child tasks
    agent: Option<Arc<crate::agent::Agent>>,
    /// Shared registry for cross-cutting subagent lifecycle tracking
    registry: Arc<SubagentRegistry>,
    /// Optional hooks for pre/post execution observability
    hooks: ToolHooks,
    /// Optional resolver for `target_agent` routing — when set, children
    /// are routed to the named agent instead of `self.agent`.
    agent_resolver: Option<Arc<dyn AgentResolver>>,
    /// Optional shared task state store.  When set, every spawned child gets a
    /// `delegation_tasks` row and can read/write shared state via `task_state`.
    store: Option<Arc<DelegationTaskStore>>,
    /// Optional handoff coordinator.  When set, a child that finishes while a
    /// sibling/descendant is `waiting_handoff` triggers successor continuation.
    coordinator: Option<Arc<DelegationCoordinator>>,
    /// Optional parent auto-wake.  When set, a child that completes after the
    /// parent's turn ended wakes the parent's session with the child's result
    /// so it can aggregate (see [`DelegationWake`]).
    wake: Option<Arc<DelegationWake>>,
}

impl std::fmt::Debug for DelegateTool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DelegateTool")
            .field("tracker", &self.tracker)
            .field("has_agent", &self.agent.is_some())
            .field("hooks", &self.hooks)
            .finish()
    }
}

impl DelegateTool {
    /// Create a new delegate tool with default limits.
    pub fn new(depth: usize) -> Self {
        Self::new_with_config(depth, DelegationConfig::default())
    }

    /// Create a new delegate tool with an agent for execution.
    pub fn with_agent(depth: usize, agent: Arc<crate::agent::Agent>) -> Self {
        Self::with_agent_and_config(depth, agent, DelegationConfig::default())
    }

    /// Create a new delegate tool with explicit depth/concurrency limits.
    fn new_with_config(depth: usize, config: DelegationConfig) -> Self {
        Self {
            tracker: DelegationTracker::with_limits(depth, config.clone()),
            agent: None,
            registry: Arc::new(SubagentRegistry::new(config.max_depth, config.max_children)),
            hooks: ToolHooks::new(),
            agent_resolver: None,
            store: None,
            coordinator: None,
            wake: None,
        }
    }

    /// Create a new delegate tool with an agent and explicit limits.
    fn with_agent_and_config(
        depth: usize,
        agent: Arc<crate::agent::Agent>,
        config: DelegationConfig,
    ) -> Self {
        Self {
            tracker: DelegationTracker::with_limits(depth, config.clone()),
            agent: Some(agent),
            registry: Arc::new(SubagentRegistry::new(config.max_depth, config.max_children)),
            hooks: ToolHooks::new(),
            agent_resolver: None,
            store: None,
            coordinator: None,
            wake: None,
        }
    }

    /// Create root-level delegate tool (depth 0)
    pub fn root() -> Self {
        Self::new(0)
    }

    /// Attach a shared [`SubagentRegistry`] (e.g. from a higher-level
    /// supervisor).
    pub fn with_registry(mut self, registry: Arc<SubagentRegistry>) -> Self {
        self.registry = registry;
        self
    }

    /// Attach execution hooks.
    pub fn with_hooks(mut self, hooks: ToolHooks) -> Self {
        self.hooks = hooks;
        self
    }

    /// Attach an [`AgentResolver`] for `target_agent` routing.
    ///
    /// When set, the `target_agent` field in [`TaskSpec`] is used to look
    /// up the appropriate agent. Falls back to `self.agent` when the target
    /// is not found or not specified.
    pub fn with_agent_resolver(mut self, resolver: Arc<dyn AgentResolver>) -> Self {
        self.agent_resolver = Some(resolver);
        self
    }

    /// Attach a shared delegation task store.  When set, every spawned child
    /// gets a shared-state row that sibling/descendant agents can read and
    /// write through the `task_state` tool.
    pub fn with_task_store(mut self, store: Arc<DelegationTaskStore>) -> Self {
        self.store = Some(store);
        self
    }

    /// Attach a handoff coordinator.  When set, a child that finishes while a
    /// task under the same root is `waiting_handoff` drives successor
    /// continuation via the coordinator.
    pub fn with_coordinator(mut self, coordinator: Arc<DelegationCoordinator>) -> Self {
        self.coordinator = Some(coordinator);
        self
    }

    /// Attach a parent auto-wake dispatcher.  When set, a child that completes
    /// after the parent's turn ended wakes the parent's session with the
    /// child's result (see [`DelegationWake`]).
    pub fn with_wake(mut self, wake: Arc<DelegationWake>) -> Self {
        self.wake = Some(wake);
        self
    }

    /// Override the delegation depth/concurrency limits.
    ///
    /// Rebuilds the shared [`SubagentRegistry`] and tracker so both enforce
    /// the same bounds (defaults: depth 3, 3 concurrent children).
    pub fn with_delegation_config(mut self, config: DelegationConfig) -> Self {
        self.tracker.set_limits(config.clone());
        self.registry = Arc::new(SubagentRegistry::new(config.max_depth, config.max_children));
        self
    }

    /// Access the underlying registry for metrics / status queries.
    pub fn registry(&self) -> &Arc<SubagentRegistry> {
        &self.registry
    }

    /// Spawn a child agent
    ///
    /// `parent_scope` is the caller's delegation scope (`None` for a
    /// top-level delegation).  The child's own scope is derived from it: one
    /// level deeper, sharing the same tree root.
    async fn spawn_child(
        &self,
        task: TaskSpec,
        parent_budget: Option<IterationBudget>,
        parent_id: String,
        parent_scope: Option<DelegationScope>,
    ) -> crate::Result<ChildAgent> {
        let budget = parent_budget.unwrap_or_else(|| IterationBudget::new(50));
        let iterations = Arc::new(AtomicUsize::new(0));

        // Compute the child's intended depth before asking the registry, which
        // validates it against its configured max depth.
        let depth = match &parent_scope {
            Some(ps) => ps.depth + 1,
            None => 1,
        };
        let max_depth = self.registry.max_depth();
        let root_id = match &parent_scope {
            Some(ps) => ps.root_id.clone(),
            None => Uuid::new_v4().to_string(),
        };
        let parent_task_id = parent_scope.as_ref().map(|ps| ps.task_id.clone());

        // Which agent runs this child.
        //
        // `target_agent` is parsed out of the *model's* tool arguments (see the
        // `task_json` handling above), so it is a privilege selector rather than
        // a routing hint from the operator: naming another running agent runs
        // the child under that agent's workspace, secrets and skill trust. A
        // delegation may therefore only name the agent it delegates for;
        // anything else falls back to that agent and is logged. Nothing else in
        // the tree sets `target_agent`, and no configuration exposes it, so
        // default-deny costs no supported use — if cross-agent delegation is
        // wanted, it should arrive as an operator-configured allowlist rather
        // than as a name the model chose.
        let parent_agent_id = self.agent.as_ref().map(|a| a.agent_id.clone());
        let child_agent = match (&task.target_agent, &self.agent_resolver) {
            (Some(target), Some(resolver))
                if parent_agent_id.as_deref() == Some(target.as_str()) =>
            {
                resolver
                    .resolve(target)
                    .await
                    .or_else(|| self.agent.clone())
            }
            (Some(target), _) => {
                warn!(
                    "delegate: refusing target_agent '{}' — a delegation may only name the \
                     agent it delegates for ({:?})",
                    target, parent_agent_id
                );
                self.agent.clone()
            }
            (None, _) => self.agent.clone(),
        };

        // The identity recorded on the task row and its events is the agent that
        // will actually run the child, not the string the caller asked for: that
        // string reached the audit trail even when the lookup fell back to the
        // parent, so the record named an agent that did not run.
        let agent_type = child_agent
            .as_ref()
            .map(|a| a.agent_id.clone())
            .filter(|id| !id.is_empty())
            .unwrap_or_else(|| "delegate".to_string());

        // Build the execution closure. The registry will supply the run_id, which
        // becomes the child id so local tracking and registry tracking share a
        // key; the child's delegation scope is built inside the closure once
        // that run_id is known.
        let registry = Arc::clone(&self.registry);
        let store_opt = self.store.clone();
        let reg_task = task.clone();
        // The child's execution tracker carries its actual depth so any
        // depth-based gating (e.g. `can_delegate`) reflects the child's level
        // in the tree rather than the shared tool's construction depth (0).
        let mut reg_tracker = self.tracker.clone();
        reg_tracker.depth = depth as usize;
        let iterations_bg = iterations.clone();
        let coordinator = self.coordinator.clone();
        let wake = self.wake.clone();
        let root_id_bg = root_id.clone();
        let parent_task_id_bg = parent_task_id.clone();
        // `parent_id` is the caller's conversation id: the user session for a
        // tree root, `delegation:<parent_run_id>` for a delegated parent.
        // Recorded on the row so the gateway can route push events to the
        // session the operator's client is actually subscribed to.
        let parent_session_bg = parent_id.clone();
        let agent_id_owned = agent_type.clone();
        let task_fn = move |run_id: String, _task_str: String| {
            let reg_task = reg_task.clone();
            let reg_tracker = reg_tracker.clone();
            let iterations_bg = iterations_bg.clone();
            let child_agent = child_agent.clone();
            let registry = Arc::clone(&registry);
            let store_opt = store_opt.clone();
            let coordinator = coordinator.clone();
            let wake = wake.clone();
            let agent_id = agent_id_owned.clone();
            let parent_session = parent_session_bg.clone();
            let scope = DelegationScope {
                root_id: root_id_bg.clone(),
                task_id: run_id.clone(),
                parent_task_id: parent_task_id_bg.clone(),
                depth,
                max_depth,
                allowed_tools: if reg_task.allowed_tools.is_empty() {
                    None
                } else {
                    Some(reg_task.allowed_tools.clone())
                },
                max_iterations: reg_task.max_iterations,
            };
            async move {
                execute_child_task(
                    run_id,
                    reg_task,
                    ChildTaskEnv {
                        tracker: reg_tracker,
                        iterations: iterations_bg,
                        agent: child_agent,
                        registry,
                        store: store_opt,
                        scope,
                        agent_id,
                        coordinator,
                        wake,
                        parent_session: Some(parent_session),
                    },
                )
                .await;
            }
        };

        // Ask the registry to enforce depth/concurrency limits and assign a run id.
        // If the registry rejects the spawn, no child is registered locally.
        let run_id = self
            .registry
            .spawn(&parent_id, &agent_type, &task.prompt, depth, task_fn)
            .await?;

        let child = ChildAgent {
            id: run_id,
            parent_id: parent_id.clone(),
            task: task.clone(),
            status: ChildStatus::Pending,
            created_at: chrono::Utc::now(),
            result: None,
            error: None,
            budget: budget.child(),
            iterations: iterations.clone(),
        };

        // Register the child with the local tracker
        self.tracker.register_child(child.clone()).await;

        info!(
            "Spawned child agent {} for task: {}",
            child.id,
            task.prompt.chars().take(50).collect::<String>()
        );

        Ok(child)
    }
}

#[async_trait]
impl Tool for DelegateTool {
    fn name(&self) -> &str {
        "delegate"
    }

    fn description(&self) -> &str {
        r#"Spawn a child agent to handle a subtask in parallel.

Use this tool to:
- Break complex tasks into parallel subtasks
- Delegate work to specialized agents
- Process multiple items concurrently

Limitations:
- Maximum 3 concurrent children per parent
- Delegation nests up to 3 levels deep; agents at the deepest level cannot
  delegate further
- Child agents cannot use: clarify, memory, send_message, execute_code
- Children share parent's iteration budget

The child agent executes its task independently. Results are NOT relayed
automatically — call action="wait" (child_id) to block for the child's
result directly. If wait reports "still running", end your turn: when the
child completes you will be woken with its result so you can continue and
aggregate. You may also poll action="status" (child_id) at any time."#
    }

    fn parameters_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "action": {
                    "type": "string",
                    "enum": ["spawn", "wait", "status", "list", "cancel", "metrics"],
                    "description": "Action to perform"
                },
                "task": {
                    "type": "object",
                    "description": "Task specification (for spawn)",
                    "properties": {
                        "prompt": {
                            "type": "string",
                            "description": "Task description/prompt for child"
                        },
                        "output_format": {
                            "type": "string",
                            "description": "Expected output format"
                        },
                        "max_iterations": {
                            "type": "integer",
                            "description": "Maximum iterations for child"
                        },
                        "allowed_tools": {
                            "type": "array",
                            "items": {"type": "string"},
                            "description": "Tools allowed for child (empty = all except blocked)"
                        }
                    },
                    "required": ["prompt"]
                },
                "child_id": {
                    "type": "string",
                    "description": "Child agent ID (for wait/status/cancel)"
                },
                "seconds": {
                    "type": "integer",
                    "description": "Max seconds to block for a child (wait action; 1-60, default 60)"
                }
            },
            "required": ["action"]
        })
    }

    fn capabilities(&self) -> ToolCapabilities {
        ToolCapabilities {
            requires_approval: true,
            risk_level: crate::tools::approval::RiskLevel::High,
            categories: vec!["system".to_string(), "delegate".to_string()],
            // A retry spawns a second child that does the work again.
            idempotent: false,
            compensation: None,
            ..Default::default()
        }
    }

    fn is_available(&self, context: &ToolContext) -> bool {
        !context.sandboxed() || !context.allowed_commands().is_empty()
    }

    async fn execute(
        &self,
        args: serde_json::Value,
        context: &ToolContext,
    ) -> crate::Result<ToolExecutionResult> {
        // ── before hooks ─────────────────────────────────────────────────
        self.hooks.run_before(self.name(), &args).await;

        let result = self.execute_inner(args.clone(), context).await;

        // ── after hooks ──────────────────────────────────────────────────
        let exec_result = match &result {
            Ok(r) => r.clone(),
            Err(e) => ToolExecutionResult::error(e.to_string()),
        };
        self.hooks.run_after(self.name(), &args, &exec_result).await;

        result
    }
}

impl DelegateTool {
    async fn execute_inner(
        &self,
        args: serde_json::Value,
        context: &ToolContext,
    ) -> crate::Result<ToolExecutionResult> {
        let action = args["action"].as_str().ok_or_else(|| {
            crate::error::SyscityError::Validation("action is required".to_string())
        })?;

        match action {
            "spawn" => {
                // Fast pre-check using the shared registry, which is the authority
                // for depth and concurrency limits.
                let current_count = self.registry.active_count().await;
                let max_children = self.registry.max_concurrent();
                if current_count >= max_children {
                    return Ok(ToolExecutionResult::error(format!(
                        "Maximum children ({}) already active. Cannot spawn more.",
                        max_children
                    )));
                }

                let task_json = &args["task"];
                let prompt = task_json["prompt"].as_str().ok_or_else(|| {
                    crate::error::SyscityError::Validation("task.prompt is required".to_string())
                })?;

                // Parse requested tools, then strip BLOCKED_TOOLS. `delegate`
                // is only stripped when the child cannot recurse (a leaf) —
                // interior nodes keep it so they can delegate further. The
                // scope re-enforces both rules at execution time.
                let requested_tools: Vec<String> = task_json["allowed_tools"]
                    .as_array()
                    .map(|arr| {
                        arr.iter()
                            .filter_map(|v| v.as_str().map(String::from))
                            .collect()
                    })
                    .unwrap_or_default();

                let child_depth = match &context.delegation {
                    Some(ps) => ps.depth + 1,
                    None => 1,
                };
                let can_child_delegate = child_depth < self.registry.max_depth();

                let blocked_requested: Vec<&str> = requested_tools
                    .iter()
                    .filter(|t| {
                        BLOCKED_TOOLS.contains(&t.as_str())
                            && !(*t == "delegate" && can_child_delegate)
                    })
                    .map(|t| t.as_str())
                    .collect();

                if !blocked_requested.is_empty() {
                    warn!(
                        "Child agent spawn: removing {} blocked tool(s) from allowed list: {:?}",
                        blocked_requested.len(),
                        blocked_requested
                    );
                }

                let allowed_tools: Vec<String> = requested_tools
                    .into_iter()
                    .filter(|t| {
                        !BLOCKED_TOOLS.contains(&t.as_str())
                            || (*t == "delegate" && can_child_delegate)
                    })
                    .collect();

                let task = TaskSpec {
                    prompt: prompt.to_string(),
                    output_format: task_json["output_format"].as_str().map(String::from),
                    max_iterations: task_json["max_iterations"].as_u64().map(|v| v as usize),
                    allowed_tools,
                    context: HashMap::new(),
                    target_agent: task_json["target_agent"].as_str().map(String::from),
                    task_id: task_json["task_id"].as_str().map(String::from),
                };

                let child = self
                    .spawn_child(
                        task,
                        None,
                        context.conversation_id.clone(),
                        context.delegation.clone(),
                    )
                    .await?;

                let depth = match &context.delegation {
                    Some(scope) => scope.depth + 1,
                    None => 1,
                };

                Ok(ToolExecutionResult::success(format!("Spawned child agent: {}", child.id))
                    .with_data(json!({
                        "child_id": child.id,
                        "status": child.status,
                        "depth": depth,
                        "max_depth": self.registry.max_depth(),
                    })))
            }

            "status" => {
                let child_id = args["child_id"].as_str().ok_or_else(|| {
                    crate::error::SyscityError::Validation(
                        "child_id is required for status".to_string(),
                    )
                })?;

                match self.tracker.get_child(child_id).await {
                    Some(child) => {
                        let hint = match child.status {
                            ChildStatus::Pending | ChildStatus::Running => format!(
                                ". Use delegate action=\"wait\" child_id={} to block for the result",
                                child_id
                            ),
                            _ => String::new(),
                        };
                        Ok(ToolExecutionResult::success(format!(
                            "Child {} status: {:?}{}",
                            child_id, child.status, hint
                        ))
                        .with_data(json!({
                            "child_id": child.id,
                            "status": child.status,
                            "result": child.result,
                            "error": child.error,
                            "created_at": child.created_at.to_rfc3339(),
                        })))
                    }
                    None => Ok(ToolExecutionResult::error(format!("Child {} not found", child_id))),
                }
            }

            "wait" => {
                let child_id = args["child_id"].as_str().ok_or_else(|| {
                    crate::error::SyscityError::Validation(
                        "child_id is required for wait".to_string(),
                    )
                })?;
                let seconds = clamp_wait_seconds(args["seconds"].as_u64().unwrap_or(60));
                Ok(self
                    .wait_for_child(child_id, std::time::Duration::from_secs(seconds))
                    .await)
            }

            "list" => {
                let children = self.tracker.list_children().await;
                let summary: Vec<serde_json::Value> = children.iter().map(|c| {
                    json!({
                        "id": c.id,
                        "status": c.status,
                        "prompt_preview": c.task.prompt.chars().take(50).collect::<String>() + "...",
                    })
                }).collect();

                Ok(ToolExecutionResult::success(format!("{} active children", children.len()))
                    .with_data(json!({
                        "children": summary,
                        "count": children.len(),
                        "max_children": self.registry.max_concurrent(),
                    })))
            }

            "cancel" => {
                let child_id = args["child_id"].as_str().ok_or_else(|| {
                    crate::error::SyscityError::Validation(
                        "child_id is required for cancel".to_string(),
                    )
                })?;

                if let Some(_child) = self.tracker.remove_child(child_id).await {
                    // Also kill in the registry so metrics stay correct.
                    if let Err(e) = self.registry.kill(child_id).await {
                        warn!("Failed to kill child agent '{}': {}", child_id, e);
                    }
                    info!("Cancelled child agent: {}", child_id);
                    Ok(ToolExecutionResult::success(format!("Cancelled child {}", child_id)))
                } else {
                    Ok(ToolExecutionResult::error(format!("Child {} not found", child_id)))
                }
            }

            "metrics" => {
                let m = self.registry.metrics().await;
                Ok(ToolExecutionResult::success(format!(
                    "Subagent metrics: {} spawned, {} completed, {} failed, {} killed",
                    m.total_spawned, m.total_completed, m.total_failed, m.total_killed
                ))
                .with_data(json!({
                    "total_spawned": m.total_spawned,
                    "total_completed": m.total_completed,
                    "total_failed": m.total_failed,
                    "total_killed": m.total_killed,
                    "active_count": self.registry.active_count().await,
                })))
            }

            _ => Err(crate::error::SyscityError::Validation(format!("Unknown action: {}", action))),
        }
    }

    /// Block up to `budget` for a child to finish, returning its result as soon
    /// as it completes.
    ///
    /// Every exit is `Ok`: a timeout, a failed child, or an unknown child are
    /// reported to the model as information rather than as an `Err`, so a
    /// waiting call can never trip the circuit breaker. The caller clamps
    /// `budget` well under the 120 s tool-call timeout ceiling (see
    /// [`MAX_WAIT_SECONDS`]).
    pub(super) async fn wait_for_child(
        &self,
        child_id: &str,
        budget: std::time::Duration,
    ) -> ToolExecutionResult {
        let deadline = tokio::time::Instant::now() + budget;
        let poll = std::time::Duration::from_secs(1);

        loop {
            // Snapshot the child state, then drop the lock before sleeping —
            // never hold the tracker lock across an await.
            match self.tracker.get_child(child_id).await {
                Some(child) => match child.status {
                    ChildStatus::Completed => {
                        let result = child.result.unwrap_or_default();
                        return ToolExecutionResult::success(format!(
                            "Child {} completed: {}",
                            child_id, result
                        ))
                        .with_data(json!({
                            "child_id": child.id,
                            "status": "completed",
                            "result": result,
                        }));
                    }
                    ChildStatus::Failed => {
                        let error = child.error.clone().unwrap_or_default();
                        return ToolExecutionResult::error(format!(
                            "Child {} failed: {}",
                            child_id, error
                        ))
                        .with_data(json!({
                            "child_id": child.id,
                            "status": "failed",
                            "error": error,
                        }));
                    }
                    ChildStatus::Cancelled => {
                        return ToolExecutionResult::success(format!(
                            "Child {} was cancelled",
                            child_id
                        ));
                    }
                    // Pending / Running — keep waiting.
                    _ => {}
                },
                None => {
                    return ToolExecutionResult::error(format!("Child {} not found", child_id));
                }
            }

            let now = tokio::time::Instant::now();
            if now >= deadline {
                return ToolExecutionResult::success(format!(
                    "Child {} is still running after {}s. End your turn — you will be \
                     woken with its result when it completes.",
                    child_id,
                    budget.as_secs().max(1)
                ))
                .with_data(json!({
                    "child_id": child_id,
                    "status": "running",
                }));
            }
            tokio::time::sleep(poll.min(deadline.saturating_duration_since(now))).await;
        }
    }
}

/// Clamp a requested `wait` budget (seconds) into the safe window.
pub(super) fn clamp_wait_seconds(seconds: u64) -> u64 {
    seconds.clamp(1, MAX_WAIT_SECONDS)
}
