//! Natural Language Task Planning for Syscity
//!
//! Automatically decomposes complex user requests into structured task plans
//! using LLM-based analysis. Integrates with the Todo system for execution.

use std::sync::Arc;
use std::time::SystemTime;

use serde::{Deserialize, Serialize};
use tracing::{debug, info, warn};

use super::todo::TodoStore;
use crate::providers::{CompletionRequest, Message, Provider};

/// A planned task with dependencies
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlannedTask {
    /// Task ID
    pub id: String,
    /// Task description
    pub description: String,
    /// Estimated complexity (1-5)
    pub complexity: u8,
    /// Dependencies (task IDs that must complete first)
    pub dependencies: Vec<String>,
    /// Suggested tools to use
    pub suggested_tools: Vec<String>,
    /// Expected outcome
    pub expected_outcome: String,
}

/// A complete task plan
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskPlan {
    /// Plan ID
    pub id: String,
    /// Original user request
    pub original_request: String,
    /// Overall goal
    pub goal: String,
    /// List of tasks in execution order
    pub tasks: Vec<PlannedTask>,
    /// Current task index
    pub current_task_index: usize,
    /// Whether the plan is complete
    pub is_complete: bool,
}

impl TaskPlan {
    /// Create a new task plan
    pub fn new(original_request: impl Into<String>, goal: impl Into<String>) -> Self {
        Self {
            id: uuid::Uuid::new_v4().to_string(),
            original_request: original_request.into(),
            goal: goal.into(),
            tasks: Vec::new(),
            current_task_index: 0,
            is_complete: false,
        }
    }

    /// Get the current task
    pub fn current_task(&self) -> Option<&PlannedTask> {
        self.tasks.get(self.current_task_index)
    }

    /// Move to the next task
    pub fn advance(&mut self) {
        if self.current_task_index < self.tasks.len().saturating_sub(1) {
            self.current_task_index += 1;
        } else {
            self.is_complete = true;
        }
    }

    /// Check if all dependencies for a task are satisfied
    pub fn dependencies_met(&self, task: &PlannedTask, completed_tasks: &[String]) -> bool {
        task.dependencies
            .iter()
            .all(|dep| completed_tasks.contains(dep))
    }

    /// Get progress as percentage
    pub fn progress_percent(&self) -> u8 {
        if self.tasks.is_empty() {
            return 100;
        }
        ((self.current_task_index as f32 / self.tasks.len() as f32) * 100.0) as u8
    }

    /// Format plan for display
    pub fn format_summary(&self) -> String {
        if self.tasks.is_empty() {
            return format!("🎯 Goal: {}\n📋 No tasks defined", self.goal);
        }
        let mut lines = vec![
            format!("🎯 Goal: {}", self.goal),
            format!("📋 Tasks: {}/{}", self.current_task_index + 1, self.tasks.len()),
            format!("📊 Progress: {}%", self.progress_percent()),
            "".to_string(),
        ];

        for (i, task) in self.tasks.iter().enumerate() {
            let status = if i < self.current_task_index {
                "✅"
            } else if i == self.current_task_index {
                "🔄"
            } else {
                "⏳"
            };
            lines.push(format!("{} {}. {}", status, i + 1, task.description));
        }

        lines.join("\n")
    }
}

/// Snapshot a plan DAG for observability. Plan turns return before any LLM
/// round runs, so the plan snapshot is the whole observable payload of a plan
/// turn (see `observe::record::PlanSnapshot`).
impl From<&TaskPlan> for crate::observe::record::PlanSnapshot {
    fn from(plan: &TaskPlan) -> Self {
        Self {
            plan_id: plan.id.clone(),
            goal: plan.goal.clone(),
            steps: plan
                .tasks
                .iter()
                .map(|t| crate::observe::record::PlanStepSnapshot {
                    id: t.id.clone(),
                    goal: t.description.clone(),
                    status: "pending".into(),
                })
                .collect(),
        }
    }
}

/// Task planner using LLM for natural language decomposition
pub struct TaskPlanner {
    provider: Arc<dyn Provider>,
    model: Option<String>,
}

impl TaskPlanner {
    /// Create a new task planner
    pub fn new(provider: Arc<dyn Provider>) -> Self {
        Self { provider, model: None }
    }

    /// Set the model to use for completions
    pub fn with_model(mut self, model: impl Into<String>) -> Self {
        self.model = Some(model.into());
        self
    }

    /// Analyze if a request needs task planning (is it complex?)
    pub async fn needs_planning(&self, request: &str) -> bool {
        // Quick heuristic check first
        let complexity_indicators = [
            "steps",
            "plan",
            "break down",
            "decompose",
            "implement",
            "build",
            "create",
            "design",
            "refactor",
            "migrate",
            "set up",
            "configure",
            "and then",
            "first",
            "after that",
            "finally",
            "multiple",
            "complex",
            "complicated",
        ];

        let request_lower = request.to_lowercase();
        let indicator_count = complexity_indicators
            .iter()
            .filter(|&&indicator| request_lower.contains(indicator))
            .count();

        // If multiple complexity indicators, definitely needs planning
        if indicator_count >= 2 {
            return true;
        }

        // If very short, probably doesn't need planning
        if request.len() < 30 {
            return false;
        }

        // Use LLM to analyze complexity
        let prompt = format!(
            r#"Analyze this user request and determine if it requires multiple steps to complete.

Request: "{}"

A request NEEDS multi-step planning if it:
- Requires implementing/building/creating something
- Has multiple distinct phases or components
- Needs research followed by action
- Involves "do X, then Y, then Z"
- Is a complex task that should be broken down

A request does NOT need planning if it:
- Is a simple question
- Asks for explanation/advice
- Is a single straightforward action
- Is conversational/greeting

Reply with ONLY "PLAN" if it needs multi-step planning, or "SIMPLE" if it's a simple request."#,
            request
        );

        let completion_req = CompletionRequest {
            model: self.model.clone(),
            messages: vec![Message::user(&prompt)],
            temperature: Some(0.0),
            max_tokens: Some(10),
            stream: false,
            ..Default::default()
        };

        match self.provider.complete(completion_req).await {
            Ok(response) => {
                let content = response.message.content.trim().to_uppercase();
                content.contains("PLAN")
            }
            Err(e) => {
                warn!("Failed to analyze complexity: {}, defaulting to simple", e);
                false
            }
        }
    }

    /// Create a task plan from a natural language request
    pub async fn create_plan(&self, request: &str) -> crate::Result<TaskPlan> {
        info!("Creating task plan for request: {}", request);

        let prompt = format!(
            r#"You are a task planning assistant. Break down the following request into a structured plan.

User Request: "{}"

Create a detailed plan with 3-7 specific, actionable tasks. For each task:
1. Give it a clear, actionable description
2. Estimate complexity (1-5, where 5 is most complex)
3. List dependencies (which task numbers must complete first)
4. Suggest relevant tools (file_read, file_write, shell, web_search, browser, code_execution, etc.)
5. Describe the expected outcome

Format your response as a JSON object with this structure:
{{
  "goal": "Brief overall goal",
  "tasks": [
    {{
      "id": "task_1",
      "description": "Specific action to take",
      "complexity": 3,
      "dependencies": [],
      "suggested_tools": ["tool_name"],
      "expected_outcome": "What this accomplishes"
    }}
  ]
}}

Ensure tasks are in logical execution order. Tasks with no dependencies should come first."#,
            request
        );

        let completion_req = CompletionRequest {
            model: self.model.clone(),
            messages: vec![Message::user(&prompt)],
            temperature: Some(0.3),
            max_tokens: Some(2000),
            stream: false,
            ..Default::default()
        };

        let response = self.provider.complete(completion_req).await?;
        let content = response.message.content;

        // Extract JSON from response (handle markdown code blocks)
        let json_str = Self::extract_json(&content)?;

        // Parse the plan
        let plan_data: serde_json::Value = serde_json::from_str(&json_str).map_err(|e| {
            crate::error::SyscityError::Validation(format!("Failed to parse task plan: {}", e))
        })?;

        let goal = plan_data["goal"]
            .as_str()
            .unwrap_or("Complete the request")
            .to_string();

        let mut plan = TaskPlan::new(request, goal);

        // Parse tasks
        if let Some(tasks) = plan_data["tasks"].as_array() {
            for (i, task_data) in tasks.iter().enumerate() {
                let task = PlannedTask {
                    id: task_data["id"]
                        .as_str()
                        .unwrap_or(&format!("task_{}", i + 1))
                        .to_string(),
                    description: task_data["description"]
                        .as_str()
                        .unwrap_or("Unnamed task")
                        .to_string(),
                    complexity: task_data["complexity"].as_u64().unwrap_or(3) as u8,
                    dependencies: task_data["dependencies"]
                        .as_array()
                        .map(|arr| {
                            arr.iter()
                                .filter_map(|v| v.as_str().map(String::from))
                                .collect()
                        })
                        .unwrap_or_default(),
                    suggested_tools: task_data["suggested_tools"]
                        .as_array()
                        .map(|arr| {
                            arr.iter()
                                .filter_map(|v| v.as_str().map(String::from))
                                .collect()
                        })
                        .unwrap_or_default(),
                    expected_outcome: task_data["expected_outcome"]
                        .as_str()
                        .unwrap_or("")
                        .to_string(),
                };
                plan.tasks.push(task);
            }
        }

        info!("Created plan with {} tasks", plan.tasks.len());
        Ok(plan)
    }

    /// Convert a TaskPlan to a TodoStore for execution
    pub fn plan_to_todos(&self, plan: &TaskPlan) -> TodoStore {
        let mut store = TodoStore::new();

        for planned_task in &plan.tasks {
            let content = format!(
                "{} (Tools: {})",
                planned_task.description,
                planned_task.suggested_tools.join(", ")
            );

            let mut task = store.create_task_with_id(&planned_task.id, content);

            // Set priority based on complexity (higher complexity = higher priority to
            // tackle early)
            task.set_priority(planned_task.complexity);

            // Store metadata
            task = task.with_metadata("expected_outcome", planned_task.expected_outcome.clone());
            task = task.with_metadata("complexity", planned_task.complexity as i32);

            // Update the store with our configured task
            store.update(task);
        }

        store
    }

    /// Refine a plan based on intermediate results
    pub async fn refine_plan(
        &self,
        plan: &mut TaskPlan,
        completed_task_id: &str,
        result_summary: &str,
    ) -> crate::Result<()> {
        debug!("Refining plan after completing: {}", completed_task_id);

        // Check if we need to adjust remaining tasks based on results
        let prompt = format!(
            r#"A task in the plan has been completed. Review and potentially adjust remaining tasks.

Original Goal: {}
Completed Task: {}
Result Summary: {}

Remaining Tasks:
{}

Should any remaining tasks be modified, added, or removed based on this result?
If no changes needed, reply "NO_CHANGES".

If changes are needed, provide a JSON object with:
{{
  "updates": [
    {{"task_id": "task_2", "new_description": "Updated description"}}
  ],
  "additions": [
    {{"after_task": "task_2", "description": "New task", "complexity": 3}}
  ],
  "removals": ["task_3"]
}}"#,
            plan.goal,
            completed_task_id,
            result_summary,
            plan.tasks
                .iter()
                .skip(plan.current_task_index)
                .map(|t| format!("- {}: {}", t.id, t.description))
                .collect::<Vec<_>>()
                .join("\n")
        );

        let completion_req = CompletionRequest {
            model: self.model.clone(),
            messages: vec![Message::user(&prompt)],
            temperature: Some(0.3),
            max_tokens: Some(1000),
            stream: false,
            ..Default::default()
        };

        let response = self.provider.complete(completion_req).await?;
        let content = response.message.content.trim();

        if content == "NO_CHANGES" || content.contains("NO_CHANGES") {
            return Ok(());
        }

        // Try to parse and apply changes
        if let Ok(json_str) = Self::extract_json(content) {
            if let Ok(changes) = serde_json::from_str::<serde_json::Value>(&json_str) {
                // Apply updates
                if let Some(updates) = changes["updates"].as_array() {
                    for update in updates {
                        if let (Some(task_id), Some(new_desc)) =
                            (update["task_id"].as_str(), update["new_description"].as_str())
                        {
                            if let Some(task) = plan.tasks.iter_mut().find(|t| t.id == task_id) {
                                task.description = new_desc.to_string();
                            }
                        }
                    }
                }

                // Note: Additions and removals would require more complex logic
                // to maintain task dependencies and ordering
            } else {
                warn!("refine_plan: LLM returned unparseable JSON — {}", &json_str);
            }
        } else {
            warn!("refine_plan: failed to extract JSON block from LLM output");
        }

        Ok(())
    }

    /// Extract JSON from text that might contain markdown or other content
    fn extract_json(text: &str) -> crate::Result<String> {
        // Try to find JSON between code blocks
        if let Some(start) = text.find("```json") {
            if let Some(end) = text[start + 7..].find("```") {
                return Ok(text[start + 7..start + 7 + end].trim().to_string());
            }
        }

        // Try to find JSON between generic code blocks
        if let Some(start) = text.find("```") {
            if let Some(end) = text[start + 3..].find("```") {
                let candidate = text[start + 3..start + 3 + end].trim();
                if candidate.starts_with('{') || candidate.starts_with('[') {
                    return Ok(candidate.to_string());
                }
            }
        }

        // Look for JSON object/array directly.
        // Use brace-depth counting from the first '{' to find the matching
        // closing '}', rather than text.rfind('}') which would concatenate
        // multiple JSON objects into one invalid blob.
        if let Some(start) = text.find('{') {
            let mut depth = 0u32;
            for (i, c) in text[start..].char_indices() {
                match c {
                    '{' => depth += 1,
                    '}' => {
                        depth -= 1;
                        if depth == 0 {
                            return Ok(text[start..start + i + 1].to_string());
                        }
                    }
                    _ => {}
                }
            }
        }

        // Assume the whole text is JSON
        Ok(text.trim().to_string())
    }
}

/// Active plan tracking for a conversation
pub struct ActivePlan {
    /// The plan being executed
    pub plan: TaskPlan,
    /// Todo store for tracking
    pub todos: TodoStore,
    /// Completed task IDs
    pub completed_tasks: Vec<String>,
}

impl ActivePlan {
    /// Mark current task as complete and advance
    pub fn complete_current_task(&mut self) {
        if let Some(task) = self.plan.current_task() {
            self.completed_tasks.push(task.id.clone());

            // Update todo store
            if let Some(todo) = self.todos.get_mut(&task.id) {
                todo.complete();
            }
        }
        self.plan.advance();
    }

    /// Get current task description for the agent
    pub fn current_task_prompt(&self) -> Option<String> {
        self.plan.current_task().map(|task| {
            format!(
                "Current Task ({} of {}): {}\nExpected Outcome: {}\nSuggested Tools: {}",
                self.plan.current_task_index + 1,
                self.plan.tasks.len(),
                task.description,
                task.expected_outcome,
                task.suggested_tools.join(", ")
            )
        })
    }
}

// ── Persistence ──────────────────────────────────────────────────────────

/// What a client needs to show a plan: the goal, the steps in order, and which
/// one is current.
///
/// A view rather than the plan itself, because the full `ActivePlan` holds a
/// `TodoStore` that is neither serializable nor anybody else's business. The
/// three fields here are the plan as its reader sees it.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ActivePlanView {
    /// The request the plan was made for.
    pub original_request: String,
    /// What the plan is trying to achieve.
    pub goal: String,
    /// The steps, in execution order.
    pub tasks: Vec<PlanTaskView>,
    /// Whether every step has run.
    pub is_complete: bool,
}

/// One step of a plan, as its reader sees it.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PlanTaskView {
    /// Step description.
    pub description: String,
    /// The expected outcome, which is what "done" means for the step.
    pub expected_outcome: String,
    /// This is the step being worked on.
    pub current: bool,
    /// This step has been completed.
    pub done: bool,
}

impl ActivePlanView {
    /// Read a live plan as the view.
    pub fn from_active(active: &ActivePlan) -> Self {
        let current = active.plan.current_task_index;
        Self {
            original_request: active.plan.original_request.clone(),
            goal: active.plan.goal.clone(),
            tasks: active
                .plan
                .tasks
                .iter()
                .enumerate()
                .map(|(index, task)| PlanTaskView {
                    description: task.description.clone(),
                    expected_outcome: task.expected_outcome.clone(),
                    current: index == current,
                    done: active.completed_tasks.contains(&task.id),
                })
                .collect(),
            is_complete: active.plan.is_complete,
        }
    }
}

/// Serializable snapshot of an ActivePlan for disk persistence.
///
/// We don't persist the full `ActivePlan` because `TodoStore` isn't
/// serializable. Instead, `PersistedPlan` captures the plan state and completed
/// task IDs; the `TodoStore` is reconstructed on load via
/// `TaskPlanner::plan_to_todos`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PersistedPlan {
    /// The serializable task plan.
    pub plan: TaskPlan,
    /// IDs of tasks that have been completed.
    pub completed_tasks: Vec<String>,
    /// Wall-clock time of the last persistence.
    pub persisted_at: SystemTime,
}

impl PersistedPlan {
    /// Create a persisted snapshot from an active plan.
    pub fn from_active(active: &ActivePlan) -> Self {
        Self {
            plan: active.plan.clone(),
            completed_tasks: active.completed_tasks.clone(),
            persisted_at: SystemTime::now(),
        }
    }

    /// Reconstruct an ActivePlan from persisted state.
    ///
    /// The `TodoStore` is rebuilt from the plan via
    /// `TaskPlanner::plan_to_todos`, then completed tasks are marked.
    pub fn into_active(self, planner: &TaskPlanner) -> ActivePlan {
        let mut todos = planner.plan_to_todos(&self.plan);

        // Mark completed tasks
        for task_id in &self.completed_tasks {
            if let Some(todo) = todos.get_mut(task_id) {
                todo.complete();
            }
        }

        ActivePlan {
            plan: self.plan,
            todos,
            completed_tasks: self.completed_tasks,
        }
    }

    /// Persist this plan to a JSON file at `path`.
    pub async fn persist_to(&self, path: impl AsRef<std::path::Path>) -> crate::Result<()> {
        let path = path.as_ref();
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent).await.map_err(|e| {
                crate::error::SyscityError::Storage {
                    context: format!("Failed to create plans directory: {:?}", parent),
                    details: e.to_string(),
                }
            })?;
        }
        let json = serde_json::to_string_pretty(self).map_err(|e| {
            crate::error::SyscityError::Storage {
                context: "Failed to serialize plan".to_string(),
                details: e.to_string(),
            }
        })?;
        tokio::fs::write(path, json)
            .await
            .map_err(|e| crate::error::SyscityError::Storage {
                context: format!("Failed to write plan file: {:?}", path),
                details: e.to_string(),
            })?;
        debug!("Plan persisted to {:?}", path);
        Ok(())
    }

    /// Load a persisted plan from a JSON file.
    pub async fn load_from(path: impl AsRef<std::path::Path>) -> crate::Result<Option<Self>> {
        let path = path.as_ref();
        if !path.exists() {
            return Ok(None);
        }
        let json = tokio::fs::read_to_string(path).await.map_err(|e| {
            crate::error::SyscityError::Storage {
                context: format!("Failed to read plan file: {:?}", path),
                details: e.to_string(),
            }
        })?;
        let plan: Self =
            serde_json::from_str(&json).map_err(|e| crate::error::SyscityError::Storage {
                context: "Failed to deserialize plan".to_string(),
                details: e.to_string(),
            })?;
        Ok(Some(plan))
    }
}

/// Load all persisted plans from a directory.
///
/// Returns a list of `PersistedPlan`s that can be reconstructed into
/// `ActivePlan`s via `PersistedPlan::into_active()`.
pub async fn load_all_plans(dir: impl AsRef<std::path::Path>) -> crate::Result<Vec<PersistedPlan>> {
    let dir = dir.as_ref();
    if !dir.exists() {
        return Ok(Vec::new());
    }
    let mut plans = Vec::new();
    let mut entries =
        tokio::fs::read_dir(dir)
            .await
            .map_err(|e| crate::error::SyscityError::Storage {
                context: format!("Failed to read plans directory: {:?}", dir),
                details: e.to_string(),
            })?;
    while let Some(entry) =
        entries
            .next_entry()
            .await
            .map_err(|e| crate::error::SyscityError::Storage {
                context: "Failed to read plans directory entry".to_string(),
                details: e.to_string(),
            })?
    {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) == Some("json") {
            if let Some(plan) = PersistedPlan::load_from(&path).await? {
                plans.push(plan);
            }
        }
    }
    info!("Loaded {} persisted plans from {:?}", plans.len(), dir);
    Ok(plans)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_task_plan_progress() {
        let mut plan = TaskPlan::new("test request", "test goal");
        plan.tasks.push(PlannedTask {
            id: "task_1".to_string(),
            description: "First task".to_string(),
            complexity: 2,
            dependencies: vec![],
            suggested_tools: vec![],
            expected_outcome: "Done".to_string(),
        });
        plan.tasks.push(PlannedTask {
            id: "task_2".to_string(),
            description: "Second task".to_string(),
            complexity: 3,
            dependencies: vec!["task_1".to_string()],
            suggested_tools: vec![],
            expected_outcome: "Done".to_string(),
        });

        assert_eq!(plan.progress_percent(), 0);
        plan.advance();
        assert_eq!(plan.progress_percent(), 50);
        plan.advance();
        assert!(plan.is_complete);
    }

    #[test]
    fn test_extract_json() {
        let text = r#"Some text
```json
{"key": "value"}
```
More text"#;
        let result = TaskPlanner::extract_json(text).unwrap();
        assert!(result.contains("\"key\": \"value\""));

        let text2 = r#"{"direct": "json"}"#;
        let result2 = TaskPlanner::extract_json(text2).unwrap();
        assert!(result2.contains("\"direct\": \"json\""));
    }

    #[test]
    fn test_task_plan_current_task() {
        let mut plan = TaskPlan::new("req", "goal");
        assert!(plan.current_task().is_none());

        plan.tasks.push(PlannedTask {
            id: "t1".to_string(),
            description: "First".to_string(),
            complexity: 1,
            dependencies: vec![],
            suggested_tools: vec![],
            expected_outcome: "Done".to_string(),
        });
        assert_eq!(plan.current_task().unwrap().id, "t1");

        plan.advance();
        assert!(plan.is_complete);
        // advance() keeps index at last position, so current_task still returns the
        // last task
        assert!(plan.current_task().is_some());
    }

    #[test]
    fn test_task_plan_dependencies_met() {
        let plan = TaskPlan::new("req", "goal");
        let task = PlannedTask {
            id: "t2".to_string(),
            description: "Second".to_string(),
            complexity: 2,
            dependencies: vec!["t1".to_string(), "t3".to_string()],
            suggested_tools: vec![],
            expected_outcome: "Done".to_string(),
        };

        assert!(!plan.dependencies_met(&task, &["t1".to_string()]));
        assert!(plan.dependencies_met(&task, &["t1".to_string(), "t3".to_string()]));
        assert!(
            plan.dependencies_met(&task, &["t1".to_string(), "t3".to_string(), "t4".to_string()])
        );
    }

    #[test]
    fn test_task_plan_format_summary() {
        let mut plan = TaskPlan::new("req", "Build app");
        plan.tasks.push(PlannedTask {
            id: "t1".to_string(),
            description: "Setup".to_string(),
            complexity: 1,
            dependencies: vec![],
            suggested_tools: vec![],
            expected_outcome: "Done".to_string(),
        });

        let summary = plan.format_summary();
        assert!(summary.contains("Build app"));
        assert!(summary.contains("Setup"));
        assert!(summary.contains("1/1"));
    }

    #[test]
    fn test_extract_json_generic_code_block() {
        let text = r#"```
{"plain": "block"}
```"#;
        let result = TaskPlanner::extract_json(text).unwrap();
        assert!(result.contains("\"plain\": \"block\""));
    }

    #[test]
    fn test_extract_json_nested_braces() {
        let text = r#"prefix {"outer": {"inner": 1}} suffix"#;
        let result = TaskPlanner::extract_json(text).unwrap();
        assert!(result.contains("\"outer\":"));
    }

    #[test]
    fn test_active_plan_complete_and_advance() {
        let mut plan = TaskPlan::new("req", "goal");
        plan.tasks.push(PlannedTask {
            id: "t1".to_string(),
            description: "Task 1".to_string(),
            complexity: 1,
            dependencies: vec![],
            suggested_tools: vec!["shell".to_string()],
            expected_outcome: "Done".to_string(),
        });
        plan.tasks.push(PlannedTask {
            id: "t2".to_string(),
            description: "Task 2".to_string(),
            complexity: 2,
            dependencies: vec![],
            suggested_tools: vec![],
            expected_outcome: "Done".to_string(),
        });

        let todos = TodoStore::new();
        let mut active = ActivePlan {
            plan,
            todos,
            completed_tasks: vec![],
        };

        assert!(active.current_task_prompt().is_some());
        active.complete_current_task();
        assert_eq!(active.completed_tasks, vec!["t1"]);
        assert!(!active.plan.is_complete);

        active.complete_current_task();
        assert!(active.plan.is_complete);
        assert_eq!(active.completed_tasks, vec!["t1", "t2"]);
    }

    #[test]
    fn test_active_plan_prompt_content() {
        let mut plan = TaskPlan::new("req", "goal");
        plan.tasks.push(PlannedTask {
            id: "t1".to_string(),
            description: "Write code".to_string(),
            complexity: 3,
            dependencies: vec![],
            suggested_tools: vec!["shell".to_string(), "file".to_string()],
            expected_outcome: "Code written".to_string(),
        });

        let todos = TodoStore::new();
        let active = ActivePlan {
            plan,
            todos,
            completed_tasks: vec![],
        };

        let prompt = active.current_task_prompt().unwrap();
        assert!(prompt.contains("Write code"));
        assert!(prompt.contains("shell"));
        assert!(prompt.contains("Code written"));
    }

    #[test]
    fn test_empty_plan_progress_is_100() {
        let plan = TaskPlan::new("req", "goal");
        assert_eq!(plan.progress_percent(), 100);
    }

    #[test]
    fn plan_snapshot_maps_tasks_to_steps() {
        let mut plan = TaskPlan::new("req", "Build app");
        plan.tasks.push(PlannedTask {
            id: "task_1".to_string(),
            description: "Setup".to_string(),
            complexity: 1,
            dependencies: vec![],
            suggested_tools: vec![],
            expected_outcome: "Done".to_string(),
        });
        plan.tasks.push(PlannedTask {
            id: "task_2".to_string(),
            description: "Deploy".to_string(),
            complexity: 2,
            dependencies: vec!["task_1".to_string()],
            suggested_tools: vec![],
            expected_outcome: "Done".to_string(),
        });

        let snap = crate::observe::record::PlanSnapshot::from(&plan);
        assert_eq!(snap.plan_id, plan.id);
        assert_eq!(snap.goal, "Build app");
        assert_eq!(snap.steps.len(), 2);
        assert_eq!(snap.steps[0].id, "task_1");
        assert_eq!(snap.steps[0].goal, "Setup");
        assert_eq!(snap.steps[1].id, "task_2");
        assert_eq!(snap.steps[1].goal, "Deploy");
        assert!(snap.steps.iter().all(|s| s.status == "pending"));

        // The snapshot must be serde round-trippable (it is persisted as JSON).
        let json = serde_json::to_string(&snap).unwrap();
        let back: crate::observe::record::PlanSnapshot = serde_json::from_str(&json).unwrap();
        assert_eq!(back.steps.len(), 2);
    }
}
