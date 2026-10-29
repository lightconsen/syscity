//! Composite tools — combine multiple atomic tools into reusable units.
//!
//! A [`CompositeTool`] is a named sequence of [`ToolStep`]s that can be
//! parameterised and executed as a single logical tool.  For example,
//! "git-clone-build" might combine:
//!
//! 1. Launch git to clone {{repo_url}} to {{dest}}
//! 2. Launch npm to install deps in {{dest}}
//! 3. Launch npm to run build in {{dest}}
//!
//! Composite tools are registered in the [`CompositeToolRegistry`] and can
//! be referenced by name in plans or exposed to the LLM as first-class tools.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use crate::computer::DesktopAction;
use crate::planner::{Task, TaskId};

/// A single step inside a composite tool.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolStep {
    /// Human-readable description.
    pub description: String,
    /// The concrete action to execute.
    pub action: DesktopAction,
    /// Whether this step must succeed before the next one runs.
    #[serde(default = "default_true")]
    pub required: bool,
}

fn default_true() -> bool {
    true
}

/// A reusable composite tool made of sequential steps.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CompositeTool {
    /// Unique name (kebab-case), e.g. "git-clone-build".
    pub name: String,
    /// Human-readable description for LLM tool listings.
    pub description: String,
    /// Ordered list of steps.
    pub steps: Vec<ToolStep>,
    /// Named parameters and their default values (if any).
    #[serde(default)]
    pub parameters: HashMap<String, Option<String>>,
}

impl CompositeTool {
    /// Create a new composite tool with the given name and description.
    pub fn new(name: impl Into<String>, description: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            description: description.into(),
            steps: Vec::new(),
            parameters: HashMap::new(),
        }
    }

    /// Add a step.
    pub fn step(mut self, description: impl Into<String>, action: DesktopAction) -> Self {
        self.steps.push(ToolStep {
            description: description.into(),
            action,
            required: true,
        });
        self
    }

    /// Add an optional (non-required) step.
    pub fn optional_step(mut self, description: impl Into<String>, action: DesktopAction) -> Self {
        self.steps.push(ToolStep {
            description: description.into(),
            action,
            required: false,
        });
        self
    }

    /// Declare a parameter.
    pub fn parameter(mut self, name: impl Into<String>, default: Option<String>) -> Self {
        self.parameters.insert(name.into(), default);
        self
    }

    /// Convert to tasks using default parameter values (no substitution needed
    /// for parameters that already have defaults).
    pub fn to_tasks(&self) -> Vec<Task> {
        let defaults: HashMap<String, String> = self
            .parameters
            .iter()
            .filter_map(|(k, v)| v.as_ref().map(|val| (k.clone(), val.clone())))
            .collect();
        self.bind(&defaults)
    }

    /// Bind concrete values to parameters, producing an executable sequence
    /// of [`Task`]s.
    ///
    /// Parameters are substituted into step actions using simple `{{name}}`
    /// string replacement.
    pub fn bind(&self, bindings: &HashMap<String, String>) -> Vec<Task> {
        let mut tasks = Vec::with_capacity(self.steps.len());
        let mut last_id: Option<TaskId> = None;

        for (i, step) in self.steps.iter().enumerate() {
            let step_id = format!("{}-step-{}", self.name, i);
            let action = substitute_params(&step.action, bindings);
            let mut task = Task::new(&step_id, &step.description, action);

            if let Some(ref dep) = last_id {
                task = task.depends_on(dep.clone());
            }

            tasks.push(task);
            last_id = Some(step_id);
        }

        tasks
    }

    /// Return a JSON schema-like description of the parameters for LLM tool
    /// listings.
    pub fn parameters_schema(&self) -> serde_json::Value {
        let props: serde_json::Map<String, serde_json::Value> = self
            .parameters
            .iter()
            .map(|(k, default)| {
                let mut prop = serde_json::json!({
                    "type": "string",
                    "description": format!("Parameter '{}'", k)
                });
                if let Some(ref d) = default {
                    prop["default"] = serde_json::Value::String(d.clone());
                }
                (k.clone(), prop)
            })
            .collect();

        let required: Vec<String> = self
            .parameters
            .iter()
            .filter(|(_, default)| default.is_none())
            .map(|(k, _)| k.clone())
            .collect();

        serde_json::json!({
            "type": "object",
            "properties": props,
            "required": required,
        })
    }
}

/// In-memory registry of named composite tools.
#[derive(Clone, Debug, Default)]
pub struct CompositeToolRegistry {
    tools: HashMap<String, CompositeTool>,
}

impl CompositeToolRegistry {
    pub fn new() -> Self {
        Self { tools: HashMap::new() }
    }

    /// Register a composite tool.
    pub fn register(&mut self, tool: CompositeTool) {
        self.tools.insert(tool.name.clone(), tool);
    }

    /// Lookup by name.
    pub fn get(&self, name: &str) -> Option<&CompositeTool> {
        self.tools.get(name)
    }

    /// List all registered names.
    pub fn list(&self) -> Vec<String> {
        self.tools.keys().cloned().collect()
    }

    /// Find a composite tool whose name or description loosely matches the
    /// given goal.
    pub fn match_by_goal(&self, goal: &str) -> Option<&CompositeTool> {
        let lower = goal.to_lowercase();
        self.tools.values().find(|t| {
            lower.contains(&t.name.replace('-', " "))
                || lower.contains(&t.description.to_lowercase())
        })
    }

    /// Load built-in composite tools for common workflows.
    pub fn with_builtins() -> Self {
        let mut reg = Self::new();

        // git-clone-build: clone a repo, install deps, build
        reg.register(
            CompositeTool::new(
                "git-clone-build",
                "Clone a git repository, install dependencies, and build the project",
            )
            .parameter("repo_url", None)
            .parameter("dest", Some("./repo".to_string()))
            .step(
                "Clone the repository",
                DesktopAction::LaunchApp {
                    name: "git".to_string(),
                    args: vec![
                        "clone".to_string(),
                        "{{repo_url}}".to_string(),
                        "{{dest}}".to_string(),
                    ],
                    wait_for_ready: true,
                },
            )
            .step(
                "Install dependencies",
                DesktopAction::LaunchApp {
                    name: "npm".to_string(),
                    args: vec!["install".to_string()],
                    wait_for_ready: true,
                },
            )
            .step(
                "Build the project",
                DesktopAction::LaunchApp {
                    name: "npm".to_string(),
                    args: vec!["run".to_string(), "build".to_string()],
                    wait_for_ready: true,
                },
            ),
        );

        reg
    }
}

// ---------------------------------------------------------------------------
// Parameter substitution
// ---------------------------------------------------------------------------

fn substitute_params(action: &DesktopAction, bindings: &HashMap<String, String>) -> DesktopAction {
    let sub = |s: &str| -> String {
        let mut result = s.to_string();
        for (k, v) in bindings {
            result = result.replace(&format!("{{{{{}}}}}", k), v);
        }
        result
    };

    match action {
        DesktopAction::LaunchApp { name, args, wait_for_ready } => DesktopAction::LaunchApp {
            name: sub(name),
            args: args.iter().map(|a| sub(a)).collect(),
            wait_for_ready: *wait_for_ready,
        },
        DesktopAction::ActivateWindow { title_pattern } => DesktopAction::ActivateWindow {
            title_pattern: sub(title_pattern),
        },
        DesktopAction::CloseWindow { title_pattern } => DesktopAction::CloseWindow {
            title_pattern: sub(title_pattern),
        },
        DesktopAction::Type { text, target } => DesktopAction::Type {
            text: sub(text),
            target: target.clone(),
        },
        DesktopAction::ClipboardSet { text } => DesktopAction::ClipboardSet { text: sub(text) },
        // For other variants, just clone (no string fields to substitute).
        other => other.clone(),
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_composite_tool_bind() {
        let tool = CompositeTool::new("test", "test tool")
            .parameter("name", None)
            .step(
                "Say hello",
                DesktopAction::Type {
                    text: "hello {{name}}".to_string(),
                    target: None,
                },
            );

        let mut bindings = HashMap::new();
        bindings.insert("name".to_string(), "world".to_string());
        let tasks = tool.bind(&bindings);

        assert_eq!(tasks.len(), 1);
        match &tasks[0].action {
            DesktopAction::Type { text, target } => {
                assert!(target.is_none());
                assert_eq!(text, "hello world");
            }
            _ => panic!("expected Type"),
        }
    }

    #[test]
    fn test_composite_tool_dependencies() {
        let tool = CompositeTool::new("seq", "sequence")
            .step("A", DesktopAction::Wait { milliseconds: 0 })
            .step("B", DesktopAction::Wait { milliseconds: 0 });

        let tasks = tool.bind(&HashMap::new());
        assert_eq!(tasks.len(), 2);
        assert!(tasks[0].dependencies.is_empty());
        assert_eq!(tasks[1].dependencies, vec!["seq-step-0"]);
    }

    #[test]
    fn test_builtin_registry() {
        let reg = CompositeToolRegistry::with_builtins();
        assert!(reg.get("git-clone-build").is_some());
    }
}
