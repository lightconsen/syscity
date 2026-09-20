//! Goal planner — decompose high-level goals into executable task DAGs.
//!
//! The planner takes a user goal (e.g. "deploy this project to a server"),
//! breaks it into a directed acyclic graph of [`Task`]s, and executes them
//! in topological order with automatic verification and rollback.
// INVARIANTS-NONE: plan artifacts are user-visible files parsed and validated at load time.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use crate::computer::VerificationEngine;
use crate::computer::{ComputerAdapter, DesktopAction, VerificationCriteria};
use crate::memory::MemoryStore;
use crate::providers::Provider;

pub mod composite_tool;
pub mod dag;
pub mod decomposer;
pub mod error_diagnosis;
pub mod executor;
pub mod persistent_queue;
pub mod recovery;
pub mod scheduled_tasks;
pub mod state;
pub mod tool_chain;
pub mod tool_learning;
pub mod util;
pub mod workflow;

pub use composite_tool::{CompositeTool, CompositeToolRegistry, ToolStep};
pub use dag::DagScheduler;
pub use decomposer::{GoalDecomposer, SubTask};
pub use error_diagnosis::{
    Diagnosis, ErrorCategory, ErrorDiagnosisEngine, RemediationStep, RootCause, Severity,
};
pub use executor::TaskExecutor;
pub use persistent_queue::{PersistentTaskManager, QueueHealth, QueueStatus};
pub use recovery::{check_startup_recovery, RecoveryOutcome};
pub use scheduled_tasks::{Schedule, ScheduledTask, TaskScheduler};
pub use state::{database_url, TaskStateStore};
pub use tool_chain::{ChainAnalysis, ChainLink, ToolChainReasoner};
pub use tool_learning::{ExperienceContext, ToolExperience, ToolLearningEngine, ToolSuggestion};
pub use workflow::{
    FailureStrategy, RecordedStep, StepResult, Workflow, WorkflowAction, WorkflowPlayer,
    WorkflowRecorder,
};

/// Unique identifier for a task.
pub type TaskId = String;

/// Current state of a task in the plan.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TaskStatus {
    /// Waiting for dependencies to complete.
    Pending,
    /// Currently executing.
    Running,
    /// Completed successfully.
    Completed,
    /// Failed after all retries exhausted.
    Failed,
    /// Rolled back due to downstream failure.
    RolledBack,
}

/// A single unit of work in a goal plan.
#[derive(Debug, Clone)]
pub struct Task {
    pub id: TaskId,
    pub description: String,
    /// The desktop action to execute.
    pub action: DesktopAction,
    /// Task IDs that must complete before this one can start.
    pub dependencies: Vec<TaskId>,
    /// How to verify this task succeeded.
    pub verification: Option<VerificationCriteria>,
    /// Whether to snapshot before executing (enables rollback).
    pub snapshot_before: bool,
    /// Number of retry attempts on failure (0 = no retries).
    pub max_retries: u32,
    /// Delay between retries.
    pub retry_delay: Duration,
    /// Status (managed by the executor).
    pub status: TaskStatus,
    /// Error message if the task failed.
    pub error: Option<String>,
    /// Result message if the task succeeded.
    pub result: Option<String>,
}

impl Task {
    pub fn new(
        id: impl Into<String>,
        description: impl Into<String>,
        action: DesktopAction,
    ) -> Self {
        Self {
            id: id.into(),
            description: description.into(),
            action,
            dependencies: vec![],
            verification: None,
            snapshot_before: false,
            max_retries: 2,
            retry_delay: Duration::from_secs(1),
            status: TaskStatus::Pending,
            error: None,
            result: None,
        }
    }

    pub fn depends_on(mut self, id: impl Into<String>) -> Self {
        self.dependencies.push(id.into());
        self
    }

    pub fn with_verification(mut self, criteria: VerificationCriteria) -> Self {
        self.verification = Some(criteria);
        self
    }

    pub fn with_snapshot(mut self) -> Self {
        self.snapshot_before = true;
        self
    }

    pub fn with_retries(mut self, max_retries: u32) -> Self {
        self.max_retries = max_retries;
        self
    }
}

/// A plan is a collection of tasks with their dependency graph.
#[derive(Debug, Default)]
pub struct Plan {
    pub goal: String,
    pub tasks: HashMap<TaskId, Task>,
}

impl Plan {
    pub fn new(goal: impl Into<String>) -> Self {
        Self {
            goal: goal.into(),
            tasks: HashMap::new(),
        }
    }

    pub fn add_task(&mut self, task: Task) -> &mut Self {
        self.tasks.insert(task.id.clone(), task);
        self
    }

    pub fn get_task(&self, id: &str) -> Option<&Task> {
        self.tasks.get(id)
    }

    pub fn get_task_mut(&mut self, id: &str) -> Option<&mut Task> {
        self.tasks.get_mut(id)
    }

    /// Returns true if all tasks are completed.
    pub fn is_complete(&self) -> bool {
        self.tasks
            .values()
            .all(|t| matches!(t.status, TaskStatus::Completed))
    }

    /// Returns true if any task failed.
    pub fn has_failures(&self) -> bool {
        self.tasks
            .values()
            .any(|t| matches!(t.status, TaskStatus::Failed))
    }

    /// Tasks that are ready to run (all dependencies completed).
    pub fn ready_tasks(&self) -> Vec<TaskId> {
        self.tasks
            .values()
            .filter(|t| {
                t.status == TaskStatus::Pending
                    && t.dependencies.iter().all(|dep| {
                        self.tasks
                            .get(dep)
                            .map(|d| d.status == TaskStatus::Completed)
                            .unwrap_or(false)
                    })
            })
            .map(|t| t.id.clone())
            .collect()
    }

    /// Update a task's status.
    pub fn set_status(&mut self, id: &str, status: TaskStatus) {
        if let Some(task) = self.tasks.get_mut(id) {
            task.status = status;
        }
    }

    /// Mark a task as completed with a result message.
    pub fn complete_task(&mut self, id: &str, result: String) {
        if let Some(task) = self.tasks.get_mut(id) {
            task.status = TaskStatus::Completed;
            task.result = Some(result);
        }
    }

    /// Mark a task as failed with an error message.
    pub fn fail_task(&mut self, id: &str, error: String) {
        if let Some(task) = self.tasks.get_mut(id) {
            task.status = TaskStatus::Failed;
            task.error = Some(error);
        }
    }
}

/// Result of executing a plan.
#[derive(Debug, Clone)]
pub struct PlanResult {
    pub success: bool,
    pub goal: String,
    pub tasks_completed: usize,
    pub tasks_failed: usize,
    pub tasks_rolled_back: usize,
    pub message: String,
}

/// High-level planner that decomposes goals and executes plans.
#[derive(Clone)]
pub struct GoalPlanner {
    executor: TaskExecutor,
    decomposer: GoalDecomposer,
    memory: Option<Arc<dyn MemoryStore>>,
    state_store: Option<TaskStateStore>,
    /// Infers prerequisite tool chains for a goal.
    chain_reasoner: ToolChainReasoner,
    /// Registry of reusable composite tools.
    composite_registry: CompositeToolRegistry,
}

impl GoalPlanner {
    /// Create a new planner with only the adapter (no LLM decomposition).
    pub fn new(adapter: Arc<dyn ComputerAdapter>) -> Self {
        let verifier = VerificationEngine::new(adapter.clone());
        let executor = TaskExecutor::new(adapter, verifier);
        // Dummy decomposer — will error if used.
        let decomposer = GoalDecomposer::new(Arc::new(DummyProvider));
        Self {
            executor,
            decomposer,
            memory: None,
            state_store: None,
            chain_reasoner: ToolChainReasoner::new(),
            composite_registry: CompositeToolRegistry::new(),
        }
    }

    /// Create a planner backed by an LLM provider for automatic goal
    /// decomposition.
    pub fn with_provider(adapter: Arc<dyn ComputerAdapter>, provider: Arc<dyn Provider>) -> Self {
        let verifier = VerificationEngine::new(adapter.clone());
        let executor = TaskExecutor::new(adapter, verifier);
        let decomposer = GoalDecomposer::new(provider.clone());
        Self {
            executor,
            decomposer,
            memory: None,
            state_store: None,
            chain_reasoner: ToolChainReasoner::with_provider(provider),
            composite_registry: CompositeToolRegistry::with_builtins(),
        }
    }

    /// Attach a memory store for retrieval-augmented planning.
    pub fn with_memory(mut self, memory: Arc<dyn MemoryStore>) -> Self {
        self.memory = Some(memory);
        self
    }

    /// Attach a persistent state store for crash recovery.
    pub fn with_state_store(mut self, store: TaskStateStore) -> Self {
        self.state_store = Some(store);
        self
    }

    /// Replace the composite-tool registry (e.g. for custom workflows).
    pub fn with_composite_registry(mut self, registry: CompositeToolRegistry) -> Self {
        self.composite_registry = registry;
        self
    }

    /// Attach an error-diagnosis engine to the task executor.
    pub fn with_diagnosis_engine(mut self, engine: ErrorDiagnosisEngine) -> Self {
        self.executor = self.executor.with_diagnosis_engine(engine);
        self
    }

    /// Attach a learning engine to the task executor for experience recording.
    pub fn with_learning_engine(mut self, engine: Arc<ToolLearningEngine>) -> Self {
        self.executor = self.executor.with_learning_engine(engine);
        self
    }

    /// Decompose a high-level goal into an executable [`Plan`].
    ///
    /// The LLM is given the list of available tool names (taken from the
    /// registry if one is available) and returns a DAG of [`SubTask`]s which
    /// are converted into [`Task`]s.
    ///
    /// If a memory store is attached, past successful experiences for similar
    /// goals are retrieved and included in the decomposition prompt.
    pub async fn decompose(&self, goal: &str, available_tools: &[String]) -> crate::Result<Plan> {
        let mut context = String::new();

        // Retrieve relevant past experiences from memory.
        if let Some(ref memory) = self.memory {
            let query = crate::memory::MemoryQuery::new()
                .of_type("experience")
                .with_content(goal)
                .limit(3);
            match memory.search(query).await {
                Ok(experiences) if !experiences.is_empty() => {
                    context.push_str("\n\nRelevant past experiences:\n");
                    for (i, exp) in experiences.iter().enumerate() {
                        context.push_str(&format!("{}. {}\n", i + 1, exp.content));
                    }
                }
                Ok(_) => {
                    // No relevant experiences found — proceed without context.
                }
                Err(e) => {
                    tracing::warn!("Failed to search memory for relevant experiences: {}", e);
                }
            }
        }

        // 1. Check for a matching composite tool (exact macro match).
        if let Some(composite) = self.composite_registry.match_by_goal(goal) {
            let mut plan = Plan::new(goal);
            for task in composite.to_tasks() {
                plan.add_task(task);
            }
            return Ok(plan);
        }

        // 2. Analyse prerequisite chain and prepend high-confidence links.
        let mut prerequisite_tasks = Vec::new();
        match self.chain_reasoner.analyse(goal).await {
            Ok(chain) if chain.confidence > 0.7 => {
                prerequisite_tasks = ToolChainReasoner::links_to_tasks(&chain.prerequisites);
            }
            Ok(_) => {
                // Low-confidence or empty chain — no prerequisites to prepend.
            }
            Err(e) => {
                tracing::warn!("Tool-chain analysis failed for goal '{}': {}", goal, e);
            }
        }

        let subtasks = self
            .decomposer
            .decompose_with_context(goal, available_tools, &context)
            .await?;
        let mut plan = Plan::new(goal);
        for task in prerequisite_tasks {
            plan.add_task(task);
        }
        for st in subtasks {
            plan.add_task(st.into_task());
        }
        Ok(plan)
    }

    /// End-to-end goal achievement: decompose, execute, verify.
    ///
    /// 1. Decompose the goal into a [`Plan`].
    /// 2. Persist the plan if a [`TaskStateStore`] is configured.
    /// 3. Execute the plan via [`TaskExecutor`].
    /// 4. Mark the plan completed in the state store.
    /// 5. Return the [`PlanResult`].
    pub async fn achieve(
        &self,
        goal: &str,
        available_tools: &[String],
    ) -> crate::Result<PlanResult> {
        let decompose_result = self.decompose(goal, available_tools).await;
        let mut plan = decompose_result?;

        let plan_id = format!("plan_{}", uuid::Uuid::new_v4());

        if let Some(ref store) = self.state_store {
            store.save_plan(&plan_id, &plan).await?;
        }

        // Execute and update persisted state after each task change.
        let result = if let Some(ref store) = self.state_store {
            self.execute_plan_with_persistence(&mut plan, &plan_id, store)
                .await?
        } else {
            self.executor.execute(&mut plan).await?
        };

        if let Some(ref store) = self.state_store {
            store.complete_plan(&plan_id, result.success).await?;
        }

        // Store experience in memory for future retrieval.
        if let Some(ref memory) = self.memory {
            let experience = format!(
                "Goal: {}\nSuccess: {}\nTasks: {} completed, {} failed, {} rolled back\nMessage: \
                 {}",
                goal,
                result.success,
                result.tasks_completed,
                result.tasks_failed,
                result.tasks_rolled_back,
                result.message
            );
            let mem = crate::memory::Memory::new("agent", experience, "experience")
                .with_importance_score(if result.success { 0.8 } else { 0.5 })
                .with_source("planner");
            if let Err(e) = memory.store(mem).await {
                tracing::warn!("Failed to store plan experience in memory: {}", e);
            }
        }

        Ok(result)
    }

    /// Execute a pre-built plan.
    pub async fn execute_plan(&self, plan: &mut Plan) -> crate::Result<PlanResult> {
        self.executor.execute(plan).await
    }

    /// Quick helper: execute a linear sequence of tasks (no branching).
    pub async fn execute_sequence(
        &self,
        goal: &str,
        tasks: Vec<Task>,
    ) -> crate::Result<PlanResult> {
        let mut plan = Plan::new(goal);
        for task in tasks {
            plan.add_task(task);
        }
        self.execute_plan(&mut plan).await
    }

    /// Resume an incomplete plan from the state store.
    pub async fn resume_plan(&self, plan_id: &str) -> crate::Result<Option<PlanResult>> {
        let store = self.state_store.as_ref().ok_or_else(|| {
            crate::error::SyscityError::Validation("No state store configured".to_string())
        })?;

        let mut plan = match store.load_plan(plan_id).await? {
            Some(p) => p,
            None => return Ok(None),
        };

        if plan.is_complete() {
            return Ok(Some(PlanResult {
                success: !plan.has_failures(),
                goal: plan.goal.clone(),
                tasks_completed: plan
                    .tasks
                    .values()
                    .filter(|t| matches!(t.status, TaskStatus::Completed))
                    .count(),
                tasks_failed: plan
                    .tasks
                    .values()
                    .filter(|t| matches!(t.status, TaskStatus::Failed))
                    .count(),
                tasks_rolled_back: plan
                    .tasks
                    .values()
                    .filter(|t| matches!(t.status, TaskStatus::RolledBack))
                    .count(),
                message: "Plan already complete".to_string(),
            }));
        }

        let result = self
            .execute_plan_with_persistence(&mut plan, plan_id, store)
            .await?;
        store.complete_plan(plan_id, result.success).await?;
        Ok(Some(result))
    }

    /// Internal: execute a plan while persisting task state updates.
    async fn execute_plan_with_persistence(
        &self,
        plan: &mut Plan,
        plan_id: &str,
        store: &TaskStateStore,
    ) -> crate::Result<PlanResult> {
        // We wrap the standard executor but hook into state updates.
        // For now we re-use the executor and persist the final state.
        // A future enhancement could wrap the adapter to intercept every
        // task completion / failure in real time.
        let result = self.executor.execute(plan).await?;

        // Persist final task states.
        for task in plan.tasks.values() {
            store.save_task(plan_id, task).await?;
        }

        Ok(result)
    }
}

/// Minimal dummy provider so that `GoalPlanner::new()` compiles without
/// requiring a real LLM provider.  Calls to `decompose` will fail gracefully.
#[derive(Debug)]
struct DummyProvider;

#[async_trait::async_trait]
impl Provider for DummyProvider {
    fn name(&self) -> &str {
        "dummy"
    }
    fn default_model(&self) -> &str {
        "dummy"
    }
    fn supports_tools(&self) -> bool {
        false
    }
    fn max_context(&self) -> usize {
        0
    }
    async fn complete(
        &self,
        _request: crate::providers::CompletionRequest,
    ) -> crate::Result<crate::providers::CompletionResponse> {
        Err(crate::error::SyscityError::Validation(
            "No LLM provider configured. Use GoalPlanner::with_provider() to enable decomposition."
                .to_string(),
        ))
    }
    async fn stream(
        &self,
        _request: crate::providers::CompletionRequest,
    ) -> crate::Result<crate::providers::CompletionStream> {
        Err(crate::error::SyscityError::Validation(
            "No LLM provider configured. Use GoalPlanner::with_provider() to enable decomposition."
                .to_string(),
        ))
    }
    async fn health_check(&self) -> crate::Result<bool> {
        Ok(false)
    }

    async fn set_credential(
        &self,
        _credential: crate::model_router::Credential,
    ) -> crate::Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::computer::{ClickTarget, MouseButton, Point};

    #[test]
    fn test_task_builder() {
        let task = Task::new(
            "click_ok",
            "Click the OK button",
            DesktopAction::Click {
                target: ClickTarget::Coordinate(Point::new(100, 200)),
                button: MouseButton::Left,
            },
        )
        .depends_on("open_dialog")
        .with_retries(3);

        assert_eq!(task.id, "click_ok");
        assert_eq!(task.dependencies, vec!["open_dialog"]);
        assert_eq!(task.max_retries, 3);
        assert_eq!(task.status, TaskStatus::Pending);
    }

    #[test]
    fn test_plan_ready_tasks() {
        let mut plan = Plan::new("test goal");
        plan.add_task(Task::new("a", "step A", DesktopAction::Wait { milliseconds: 10 }));
        plan.add_task(
            Task::new("b", "step B", DesktopAction::Wait { milliseconds: 10 }).depends_on("a"),
        );
        plan.add_task(
            Task::new("c", "step C", DesktopAction::Wait { milliseconds: 10 }).depends_on("a"),
        );

        // Initially only 'a' is ready (no deps).
        let ready = plan.ready_tasks();
        assert_eq!(ready, vec!["a"]);

        // After 'a' completes, 'b' and 'c' are ready.
        plan.complete_task("a", "done".to_string());
        let mut ready = plan.ready_tasks();
        ready.sort();
        assert_eq!(ready, vec!["b", "c"]);
    }

    #[test]
    fn test_plan_is_complete() {
        let mut plan = Plan::new("test");
        plan.add_task(Task::new("a", "step A", DesktopAction::Wait { milliseconds: 10 }));

        assert!(!plan.is_complete());
        plan.complete_task("a", "done".to_string());
        assert!(plan.is_complete());
    }

    #[test]
    fn test_plan_has_failures() {
        let mut plan = Plan::new("test");
        plan.add_task(Task::new("a", "step A", DesktopAction::Wait { milliseconds: 10 }));

        assert!(!plan.has_failures());
        plan.fail_task("a", "oops".to_string());
        assert!(plan.has_failures());
    }

    // ── GoalPlanner integration tests (MockProvider + HeadlessComputerAdapter) ──

    use crate::computer::headless::HeadlessComputerAdapter;
    use crate::providers::mock::MockProvider;
    use crate::providers::{Message, Role};

    /// Return true if the message list is a GoalPlanner decomposition request.
    fn is_decompose_request(messages: &[Message]) -> bool {
        messages
            .iter()
            .any(|m| m.role == Role::System && m.content.contains("task-decomposition engine"))
    }

    #[tokio::test]
    async fn test_goal_planner_decompose_with_mock() {
        let json = r#"[
            {"id":"step-a","description":"Step A","dependencies":[],"action":{"wait":{"milliseconds":0}},"max_retries":1},
            {"id":"step-b","description":"Step B","dependencies":["step-a"],"action":{"wait":{"milliseconds":0}},"max_retries":1}
        ]"#;

        let mock = MockProvider::new().with_callback(move |messages| {
            if is_decompose_request(messages) {
                return Message::assistant(json.to_string());
            }
            Message::assistant("ok")
        });

        let adapter = Arc::new(HeadlessComputerAdapter::new());
        let planner = GoalPlanner::with_provider(adapter, Arc::new(mock));

        let tools = vec!["shell".to_string()];
        let plan = planner.decompose("test goal", &tools).await.unwrap();

        assert_eq!(plan.goal, "test goal");
        assert_eq!(plan.tasks.len(), 2);
        assert!(plan.tasks.contains_key("step-a"));
        assert!(plan.tasks.contains_key("step-b"));
        assert_eq!(plan.tasks["step-b"].dependencies, vec!["step-a"]);
    }

    #[tokio::test]
    async fn test_goal_planner_achieve_simple_wait_goal() {
        let json = r#"[
            {"id":"noop","description":"No-op wait","dependencies":[],"action":{"wait":{"milliseconds":0}},"max_retries":1}
        ]"#;

        let mock = MockProvider::new().with_callback(move |messages| {
            if is_decompose_request(messages) {
                return Message::assistant(json.to_string());
            }
            Message::assistant("done")
        });

        let adapter = Arc::new(HeadlessComputerAdapter::new());
        let planner = GoalPlanner::with_provider(adapter, Arc::new(mock));

        let result = planner.achieve("run a no-op", &[]).await.unwrap();

        assert!(result.success, "Plan failed: {}", result.message);
        assert_eq!(result.tasks_completed, 1);
        assert_eq!(result.tasks_failed, 0);
    }

    #[tokio::test]
    async fn test_goal_planner_achieve_with_dependencies() {
        let json = r#"[
            {"id":"first","description":"First","dependencies":[],"action":{"wait":{"milliseconds":0}},"max_retries":1},
            {"id":"second","description":"Second","dependencies":["first"],"action":{"wait":{"milliseconds":0}},"max_retries":1},
            {"id":"third","description":"Third","dependencies":["second"],"action":{"wait":{"milliseconds":0}},"max_retries":1}
        ]"#;

        let mock = MockProvider::new().with_callback(move |messages| {
            if is_decompose_request(messages) {
                return Message::assistant(json.to_string());
            }
            Message::assistant("done")
        });

        let adapter = Arc::new(HeadlessComputerAdapter::new());
        let planner = GoalPlanner::with_provider(adapter, Arc::new(mock));

        let result = planner.achieve("chain of waits", &[]).await.unwrap();

        assert!(result.success, "Plan failed: {}", result.message);
        assert_eq!(result.tasks_completed, 3);
        assert_eq!(result.tasks_failed, 0);
    }

    #[tokio::test]
    async fn test_goal_planner_rejects_cyclic_deps() {
        let json = r#"[
            {"id":"a","description":"A","dependencies":["c"],"max_retries":0},
            {"id":"b","description":"B","dependencies":["a"],"max_retries":0},
            {"id":"c","description":"C","dependencies":["b"],"max_retries":0}
        ]"#;

        let mock = MockProvider::new().with_callback(move |messages| {
            if is_decompose_request(messages) {
                return Message::assistant(json.to_string());
            }
            Message::assistant("ok")
        });

        let adapter = Arc::new(HeadlessComputerAdapter::new());
        let planner = GoalPlanner::with_provider(adapter, Arc::new(mock));

        let err = planner.decompose("cyclic goal", &[]).await.unwrap_err();
        assert!(err.to_string().contains("Cycle detected"));
    }
}
