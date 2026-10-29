//! Workflow recorder and replay — capture user actions and replay them.
//!
//! ```text
//! User actions → WorkflowRecorder → Workflow (parameterised)
//!                                               ↓
//! Agent/Planner ← WorkflowPlayer ←─ replay with retries
//! ```

use std::collections::HashMap;
use std::time::{Duration, Instant};

use crate::computer::{ComputerAdapter, DesktopAction};

/// A single recorded action with the delay before it.
#[derive(Debug, Clone, PartialEq)]
pub struct RecordedStep {
    /// Time elapsed since the previous step.
    pub delay_before: Duration,
    /// The action to execute.
    pub action: WorkflowAction,
    /// Human-readable description.
    pub description: String,
}

/// Actions that can be recorded in a workflow.
#[derive(Debug, Clone, PartialEq)]
pub enum WorkflowAction {
    /// A desktop GUI action.
    Desktop(DesktopAction),
    /// A shell command.
    Shell { command: String },
    /// Wait for a duration.
    Wait { milliseconds: u64 },
}

/// A parameterised, replayable workflow.
#[derive(Debug, Clone, Default)]
pub struct Workflow {
    pub name: String,
    pub description: Option<String>,
    pub steps: Vec<RecordedStep>,
    /// Parameter names inferred from the recording (e.g. `{{filename}}`).
    pub parameters: Vec<String>,
}

impl Workflow {
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            description: None,
            steps: Vec::new(),
            parameters: Vec::new(),
        }
    }

    pub fn with_description(mut self, desc: impl Into<String>) -> Self {
        self.description = Some(desc.into());
        self
    }

    /// Replace parameter placeholders with actual values.
    pub fn bind_parameters(&self, values: HashMap<String, String>) -> Vec<RecordedStep> {
        self.steps
            .iter()
            .cloned()
            .map(|mut step| {
                step.action = Self::substitute_in_action(&step.action, &values);
                step
            })
            .collect()
    }

    fn substitute_in_action(
        action: &WorkflowAction,
        values: &HashMap<String, String>,
    ) -> WorkflowAction {
        let mut result = action.clone();
        match &mut result {
            WorkflowAction::Shell { command } => {
                for (k, v) in values {
                    *command = command.replace(&format!("{{{{{}}}}}", k), v);
                }
            }
            WorkflowAction::Desktop(DesktopAction::Type { text, .. }) => {
                for (k, v) in values {
                    *text = text.replace(&format!("{{{{{}}}}}", k), v);
                }
            }
            WorkflowAction::Desktop(DesktopAction::LaunchApp { name, args, .. }) => {
                for (k, v) in values {
                    *name = name.replace(&format!("{{{{{}}}}}", k), v);
                    for arg in args.iter_mut() {
                        *arg = arg.replace(&format!("{{{{{}}}}}", k), v);
                    }
                }
            }
            _ => {}
        }
        result
    }
}

/// Records user actions into a workflow.
#[derive(Debug)]
pub struct WorkflowRecorder {
    steps: Vec<RecordedStep>,
    recording: bool,
    last_action_time: Option<Instant>,
}

impl Default for WorkflowRecorder {
    fn default() -> Self {
        Self::new()
    }
}

impl WorkflowRecorder {
    pub fn new() -> Self {
        Self {
            steps: Vec::new(),
            recording: false,
            last_action_time: None,
        }
    }

    /// Start recording.
    pub fn start_recording(&mut self) {
        self.recording = true;
        self.steps.clear();
        self.last_action_time = Some(Instant::now());
    }

    /// Stop recording and return the captured workflow.
    pub fn stop_recording(&mut self, name: impl Into<String>) -> Workflow {
        self.recording = false;
        let parameters = Self::infer_parameters(&self.steps);
        Workflow {
            name: name.into(),
            description: None,
            steps: std::mem::take(&mut self.steps),
            parameters,
        }
    }

    /// Record a desktop action.
    pub fn record_desktop(&mut self, action: DesktopAction, description: impl Into<String>) {
        if !self.recording {
            return;
        }
        let delay = self.delay_since_last();
        self.steps.push(RecordedStep {
            delay_before: delay,
            action: WorkflowAction::Desktop(action),
            description: description.into(),
        });
    }

    /// Record a shell command.
    pub fn record_shell(&mut self, command: impl Into<String>, description: impl Into<String>) {
        if !self.recording {
            return;
        }
        let delay = self.delay_since_last();
        self.steps.push(RecordedStep {
            delay_before: delay,
            action: WorkflowAction::Shell { command: command.into() },
            description: description.into(),
        });
    }

    /// Record a wait step.
    pub fn record_wait(&mut self, milliseconds: u64, description: impl Into<String>) {
        if !self.recording {
            return;
        }
        let delay = self.delay_since_last();
        self.steps.push(RecordedStep {
            delay_before: delay,
            action: WorkflowAction::Wait { milliseconds },
            description: description.into(),
        });
    }

    fn delay_since_last(&mut self) -> Duration {
        let now = Instant::now();
        match self.last_action_time {
            Some(t) => now.duration_since(t),
            None => Duration::ZERO,
        }
    }

    /// Heuristically infer parameters from recorded steps.
    ///
    /// Looks for hard-coded file paths, URLs, and text that look like
    /// variable candidates.
    fn infer_parameters(steps: &[RecordedStep]) -> Vec<String> {
        let mut candidates: HashMap<String, usize> = HashMap::new();

        for step in steps {
            match &step.action {
                WorkflowAction::Shell { command } => {
                    Self::extract_candidates(command, &mut candidates);
                }
                WorkflowAction::Desktop(DesktopAction::Type { text, .. }) => {
                    Self::extract_candidates(text, &mut candidates);
                }
                _ => {}
            }
        }

        // Return candidates that appear more than once (likely variables).
        let mut result: Vec<String> = candidates
            .iter()
            .filter(|(_, count)| **count > 1)
            .map(|(k, _)| k.clone())
            .collect();
        result.sort();
        result
    }

    fn extract_candidates(text: &str, candidates: &mut HashMap<String, usize>) {
        // Simple heuristic: look for quoted strings that look like paths or URLs
        for word in text.split_whitespace() {
            let trimmed = word.trim_matches('\'').trim_matches('"');
            if trimmed.starts_with("http") || trimmed.starts_with('/') || trimmed.starts_with("~/")
            {
                *candidates.entry(trimmed.to_string()).or_insert(0) += 1;
            }
        }
    }
}

/// Result of replaying a single step.
#[derive(Debug, Clone)]
pub enum StepResult {
    Success { message: String },
    Skipped { reason: String },
    Failed { error: String },
}

/// Strategy when a step fails during replay.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FailureStrategy {
    /// Abort the entire workflow.
    Abort,
    /// Skip the failed step and continue.
    Skip,
    /// Retry the step up to N times.
    Retry { max_retries: u32, delay_ms: u64 },
}

/// Replays a workflow against a `ComputerAdapter`.
pub struct WorkflowPlayer {
    failure_strategy: FailureStrategy,
}

impl Default for WorkflowPlayer {
    fn default() -> Self {
        Self::new()
    }
}

impl WorkflowPlayer {
    pub fn new() -> Self {
        Self {
            failure_strategy: FailureStrategy::Retry { max_retries: 2, delay_ms: 500 },
        }
    }

    pub fn with_failure_strategy(mut self, strategy: FailureStrategy) -> Self {
        self.failure_strategy = strategy;
        self
    }

    /// Replay a workflow bound with the given parameter values.
    pub async fn replay(
        &self,
        workflow: &Workflow,
        parameters: HashMap<String, String>,
        adapter: &dyn ComputerAdapter,
    ) -> crate::Result<Vec<StepResult>> {
        let steps = workflow.bind_parameters(parameters);
        let mut results = Vec::with_capacity(steps.len());

        for step in steps {
            // Wait the recorded delay before executing.
            if step.delay_before > Duration::ZERO {
                tokio::time::sleep(step.delay_before).await;
            }

            let result = self.execute_step(&step, adapter).await;
            let should_continue = match &result {
                StepResult::Success { .. } => true,
                StepResult::Skipped { .. } => true,
                StepResult::Failed { .. } => {
                    matches!(self.failure_strategy, FailureStrategy::Skip)
                }
            };

            results.push(result);

            if !should_continue {
                break;
            }
        }

        Ok(results)
    }

    async fn execute_step(&self, step: &RecordedStep, adapter: &dyn ComputerAdapter) -> StepResult {
        let mut last_error = String::new();
        let max_retries = match self.failure_strategy {
            FailureStrategy::Retry { max_retries, .. } => max_retries,
            _ => 0,
        };

        for attempt in 0..=max_retries {
            match self.try_execute_step(step, adapter).await {
                Ok(msg) => {
                    return StepResult::Success { message: msg };
                }
                Err(e) => {
                    last_error = e.clone();
                    if attempt < max_retries {
                        if let FailureStrategy::Retry { delay_ms, .. } = self.failure_strategy {
                            tokio::time::sleep(Duration::from_millis(delay_ms)).await;
                        }
                    }
                }
            }
        }

        match self.failure_strategy {
            FailureStrategy::Skip => StepResult::Skipped {
                reason: format!("Failed but skipped: {}", last_error),
            },
            _ => StepResult::Failed { error: last_error },
        }
    }

    async fn try_execute_step(
        &self,
        step: &RecordedStep,
        adapter: &dyn ComputerAdapter,
    ) -> Result<String, String> {
        match &step.action {
            WorkflowAction::Desktop(action) => adapter
                .execute(action.clone())
                .await
                .map(|r| r.message)
                .map_err(|e| e.to_string()),
            WorkflowAction::Shell { command } => {
                let output = tokio::process::Command::new("sh")
                    .arg("-c")
                    .arg(command)
                    .output()
                    .await
                    .map_err(|e| format!("shell failed: {}", e))?;
                if output.status.success() {
                    Ok(String::from_utf8_lossy(&output.stdout).to_string())
                } else {
                    Err(String::from_utf8_lossy(&output.stderr).to_string())
                }
            }
            WorkflowAction::Wait { milliseconds } => {
                tokio::time::sleep(Duration::from_millis(*milliseconds)).await;
                Ok(format!("Waited {}ms", milliseconds))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::computer::{ClickTarget, MouseButton, Point};

    #[test]
    fn test_workflow_recorder_start_stop() {
        let mut recorder = WorkflowRecorder::new();
        recorder.start_recording();
        recorder.record_desktop(
            DesktopAction::Click {
                target: ClickTarget::Coordinate(Point::new(100, 200)),
                button: MouseButton::Left,
            },
            "Click OK",
        );
        recorder.record_wait(500, "Wait for dialog");
        let workflow = recorder.stop_recording("test_workflow");

        assert_eq!(workflow.name, "test_workflow");
        assert_eq!(workflow.steps.len(), 2);
        assert!(matches!(workflow.steps[0].action, WorkflowAction::Desktop(..)));
        assert!(matches!(workflow.steps[1].action, WorkflowAction::Wait { .. }));
    }

    #[test]
    fn test_workflow_parameter_binding() {
        let mut workflow = Workflow::new("deploy");
        workflow.steps.push(RecordedStep {
            delay_before: Duration::ZERO,
            action: WorkflowAction::Shell {
                command: "git clone {{repo}} /tmp/{{name}}".to_string(),
            },
            description: "Clone repo".to_string(),
        });
        workflow.parameters = vec!["repo".to_string(), "name".to_string()];

        let mut values = HashMap::new();
        values.insert("repo".to_string(), "https://github.com/foo/bar".to_string());
        values.insert("name".to_string(), "myproject".to_string());

        let bound = workflow.bind_parameters(values);
        match &bound[0].action {
            WorkflowAction::Shell { command } => {
                assert_eq!(command, "git clone https://github.com/foo/bar /tmp/myproject");
            }
            _ => panic!("Expected shell action"),
        }
    }

    #[test]
    fn test_workflow_recorder_no_record_when_stopped() {
        let mut recorder = WorkflowRecorder::new();
        // Not started
        recorder.record_desktop(DesktopAction::Wait { milliseconds: 10 }, "should not appear");
        let workflow = recorder.stop_recording("empty");
        assert!(workflow.steps.is_empty());
    }

    #[test]
    fn test_step_result_display() {
        let r = StepResult::Success { message: "done".to_string() };
        assert!(matches!(r, StepResult::Success { .. }));
    }

    #[test]
    fn test_failure_strategy_clone_eq() {
        let s1 = FailureStrategy::Retry { max_retries: 3, delay_ms: 100 };
        let s2 = s1.clone();
        assert_eq!(s1, s2);
    }
}
