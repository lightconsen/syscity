//! Reflection engine — root-cause analysis and adaptive retry for desktop
//! actions.
//!
//! When a verification or action fails, `ReflectionEngine` analyses *why* it
//! failed (timing, missing target, wrong action, resource exhaustion, etc.),
//! queries past experiences from memory, and suggests an adapted retry
//! strategy.  On success or final failure the experience is recorded so the
//! agent learns over time.
//!
//! The analysis is currently rule-based (fast, deterministic, testable).  A
//! future enhancement can delegate to an LLM for ambiguous cases.

use std::sync::Arc;
use std::time::SystemTime;

use serde::{Deserialize, Serialize};

use crate::computer::{ActionResult, DesktopAction, VerificationCriteria};
use crate::memory::{Memory, MemoryEntryType, MemoryId, MemoryQuery, MemoryStore};

/// Categorisation of why a desktop action or verification failed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FailureRootCause {
    /// UI / window / process was not ready yet.
    TimingIssue,
    /// Expected element, window, file, or process was not found.
    TargetNotFound,
    /// Action type or parameters were inappropriate.
    WrongAction,
    /// CPU, memory, disk, or network was exhausted.
    ResourceExhausted,
    /// Insufficient privileges for the operation.
    PermissionDenied,
    /// UI was in an unexpected state (e.g. unexpected dialog open).
    StateMismatch,
    /// Could not determine the root cause.
    Unknown,
}

/// Structured output of a failure analysis.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FailureAnalysis {
    pub root_cause: FailureRootCause,
    pub confidence: f32,
    pub details: String,
    pub suggested_delay_ms: Option<u64>,
    pub suggested_fix: Option<String>,
}

/// A recorded experience of a failure and its recovery.
#[derive(Debug, Clone)]
pub struct PastExperience {
    pub root_cause: FailureRootCause,
    pub action_type: String,
    pub original_delay_ms: u64,
    pub adjusted_delay_ms: u64,
    pub success: bool,
}

/// Reflection engine that analyses failures, queries memory, and adapts retry
/// configuration.
#[derive(Clone)]
pub struct ReflectionEngine {
    memory_store: Option<Arc<dyn MemoryStore>>,
}

impl ReflectionEngine {
    /// Create a reflection engine without memory (analysis only).
    pub fn new() -> Self {
        Self { memory_store: None }
    }

    /// Attach a memory store for experience recording and retrieval.
    pub fn with_memory_store(mut self, store: Arc<dyn MemoryStore>) -> Self {
        self.memory_store = Some(store);
        self
    }

    // ── Analysis ──────────────────────────────────────────────────────────

    /// Analyse why a verification or action failed.
    ///
    /// Uses rule-based heuristics keyed on the `VerificationCriteria`, the
    /// `DesktopAction`, the `ActionResult`, and the attempt number.
    pub fn analyze_failure(
        &self,
        criteria: &VerificationCriteria,
        action: &DesktopAction,
        result: &ActionResult,
        attempt: u32,
    ) -> FailureAnalysis {
        // 1. Check the error message for strong signals.
        let msg_lower = result.message.to_lowercase();

        if msg_lower.contains("permission")
            || msg_lower.contains("access denied")
            || msg_lower.contains("eperm")
            || msg_lower.contains("not permitted")
        {
            return FailureAnalysis {
                root_cause: FailureRootCause::PermissionDenied,
                confidence: 0.95,
                details: format!("Permission denied during {:?}: {}", action, result.message),
                suggested_delay_ms: None,
                suggested_fix: Some(
                    "Run with elevated privileges or adjust permissions.".to_string(),
                ),
            };
        }

        if msg_lower.contains("not found")
            || msg_lower.contains("no such")
            || msg_lower.contains("does not exist")
            || msg_lower.contains("missing")
        {
            return FailureAnalysis {
                root_cause: FailureRootCause::TargetNotFound,
                confidence: 0.90,
                details: format!("Target not found for {:?}: {}", action, result.message),
                suggested_delay_ms: Some(2000),
                suggested_fix: Some(
                    "Wait longer for target to appear or verify path/name.".to_string(),
                ),
            };
        }

        if msg_lower.contains("timeout")
            || msg_lower.contains("timed out")
            || msg_lower.contains("deadline")
        {
            return FailureAnalysis {
                root_cause: FailureRootCause::TimingIssue,
                confidence: 0.85,
                details: format!("Operation timed out for {:?}: {}", action, result.message),
                suggested_delay_ms: Some((500 * (attempt + 2)) as u64),
                suggested_fix: Some("Increase settle delay before verification.".to_string()),
            };
        }

        if msg_lower.contains("resource")
            || msg_lower.contains("out of memory")
            || msg_lower.contains("disk full")
            || msg_lower.contains("no space")
        {
            return FailureAnalysis {
                root_cause: FailureRootCause::ResourceExhausted,
                confidence: 0.90,
                details: format!("Resource exhausted during {:?}: {}", action, result.message),
                suggested_delay_ms: Some(3000),
                suggested_fix: Some(
                    "Wait for system to recover or free resources before retrying.".to_string(),
                ),
            };
        }

        // 2. Criteria-specific heuristics.
        match criteria {
            VerificationCriteria::UiTreeContains { role, .. } => {
                // If UI tree is empty or missing the role, it's usually timing.
                if msg_lower.contains("empty") || msg_lower.contains("no elements") {
                    return FailureAnalysis {
                        root_cause: FailureRootCause::TimingIssue,
                        confidence: 0.80,
                        details: format!(
                            "UI tree empty when looking for role '{}'; window may not be ready.",
                            role
                        ),
                        suggested_delay_ms: Some((500 * (attempt + 2)) as u64),
                        suggested_fix: Some(format!(
                            "Wait longer for UI to load before searching for '{}'.",
                            role
                        )),
                    };
                }
                FailureAnalysis {
                    root_cause: FailureRootCause::TargetNotFound,
                    confidence: 0.75,
                    details: format!("UI tree does not contain element with role '{}'.", role),
                    suggested_delay_ms: Some(1500),
                    suggested_fix: Some(format!(
                        "Verify the element with role '{}' exists in the current UI.",
                        role
                    )),
                }
            }
            VerificationCriteria::ScreenshotChanged { .. }
            | VerificationCriteria::ScreenshotStable { .. } => {
                // Screenshot differences usually mean the UI is still changing.
                FailureAnalysis {
                    root_cause: FailureRootCause::TimingIssue,
                    confidence: 0.85,
                    details: "Screenshot changed more than expected; UI may still be animating."
                        .to_string(),
                    suggested_delay_ms: Some((500 * (attempt + 2)) as u64),
                    suggested_fix: Some(
                        "Wait longer for animations or transitions to finish.".to_string(),
                    ),
                }
            }
            VerificationCriteria::ProcessRunning { name } => FailureAnalysis {
                root_cause: FailureRootCause::TargetNotFound,
                confidence: 0.80,
                details: format!("Process '{}' is not running.", name),
                suggested_delay_ms: Some(2000),
                suggested_fix: Some(format!("Verify '{}' was launched successfully.", name)),
            },
            VerificationCriteria::ProcessExited { name } => FailureAnalysis {
                root_cause: FailureRootCause::TimingIssue,
                confidence: 0.70,
                details: format!("Process '{}' is still running.", name),
                suggested_delay_ms: Some(1000),
                suggested_fix: Some(format!("Wait longer for '{}' to exit.", name)),
            },
            VerificationCriteria::WindowTitleContains { pattern } => FailureAnalysis {
                root_cause: FailureRootCause::TimingIssue,
                confidence: 0.80,
                details: format!("Window title does not contain '{}' yet.", pattern),
                suggested_delay_ms: Some((500 * (attempt + 2)) as u64),
                suggested_fix: Some(format!(
                    "Wait longer for window with '{}' in title to appear.",
                    pattern
                )),
            },
            VerificationCriteria::FileExists { path } => FailureAnalysis {
                root_cause: FailureRootCause::TargetNotFound,
                confidence: 0.85,
                details: format!("File '{}' does not exist.", path),
                suggested_delay_ms: Some(1500),
                suggested_fix: Some(format!(
                    "Verify the path '{}' is correct and the file was created.",
                    path
                )),
            },
            VerificationCriteria::OutputContains { text } => FailureAnalysis {
                root_cause: FailureRootCause::StateMismatch,
                confidence: 0.70,
                details: format!("Output does not contain expected text '{}'.", text),
                suggested_delay_ms: Some(1000),
                suggested_fix: Some(format!(
                    "Check if the operation produced the expected output containing '{}'.",
                    text
                )),
            },
            VerificationCriteria::Success => {
                if result.success {
                    // Should not reach here if success, but handle defensively.
                    FailureAnalysis {
                        root_cause: FailureRootCause::Unknown,
                        confidence: 0.5,
                        details: "Action reported success but verification failed.".to_string(),
                        suggested_delay_ms: None,
                        suggested_fix: None,
                    }
                } else {
                    // Action itself failed — look at action type for clues.
                    match action {
                        DesktopAction::Click { .. } => FailureAnalysis {
                            root_cause: FailureRootCause::TimingIssue,
                            confidence: 0.75,
                            details: "Click may have missed because target was not ready."
                                .to_string(),
                            suggested_delay_ms: Some((500 * (attempt + 2)) as u64),
                            suggested_fix: Some(
                                "Wait for target to be fully rendered before clicking.".to_string(),
                            ),
                        },
                        DesktopAction::Type { .. } => FailureAnalysis {
                            root_cause: FailureRootCause::StateMismatch,
                            confidence: 0.70,
                            details: "Text input failed; focus may have been lost.".to_string(),
                            suggested_delay_ms: Some(1000),
                            suggested_fix: Some(
                                "Ensure the input field has focus before typing.".to_string(),
                            ),
                        },
                        DesktopAction::LaunchApp { name, .. } => FailureAnalysis {
                            root_cause: FailureRootCause::ResourceExhausted,
                            confidence: 0.65,
                            details: format!(
                                "Failed to launch '{}'; may be resource or permission issue.",
                                name
                            ),
                            suggested_delay_ms: Some(2000),
                            suggested_fix: Some(format!(
                                "Check if '{}' is installed and system has resources.",
                                name
                            )),
                        },
                        _ => FailureAnalysis {
                            root_cause: FailureRootCause::Unknown,
                            confidence: 0.5,
                            details: format!(
                                "Action {:?} failed with no specific diagnosis.",
                                action
                            ),
                            suggested_delay_ms: None,
                            suggested_fix: None,
                        },
                    }
                }
            }
        }
    }

    // ── Experience Memory ─────────────────────────────────────────────────

    /// Map a `DesktopAction` to a short human-readable action-type key used
    /// for past-experience matching in the memory store.
    fn action_type_key(action: &DesktopAction) -> &'static str {
        match action {
            DesktopAction::Screenshot { .. } => "Screenshot",
            DesktopAction::Click { .. } => "Click",
            DesktopAction::DoubleClick { .. } => "DoubleClick",
            DesktopAction::Type { .. } => "Type",
            DesktopAction::KeyPress { .. } => "KeyPress",
            DesktopAction::Scroll { .. } => "Scroll",
            DesktopAction::Drag { .. } => "Drag",
            DesktopAction::ReadUiTree { .. } => "ReadUiTree",
            DesktopAction::LaunchApp { .. } => "LaunchApp",
            DesktopAction::ActivateWindow { .. } => "ActivateWindow",
            DesktopAction::CloseWindow { .. } => "CloseWindow",
            DesktopAction::Wait { .. } => "Wait",
            DesktopAction::ClipboardGet => "ClipboardGet",
            DesktopAction::ClipboardSet { .. } => "ClipboardSet",
            DesktopAction::GetSystemStatus => "GetSystemStatus",
            DesktopAction::ListProcesses { .. } => "ListProcesses",
            DesktopAction::KillProcess { .. } => "KillProcess",
            DesktopAction::RestartProcess { .. } => "RestartProcess",
            DesktopAction::SetProcessPriority { .. } => "SetProcessPriority",
            DesktopAction::ListWindows => "ListWindows",
            DesktopAction::GetWindowGeometry { .. } => "GetWindowGeometry",
            DesktopAction::MoveWindow { .. } => "MoveWindow",
            DesktopAction::ResizeWindow { .. } => "ResizeWindow",
            DesktopAction::MinimizeWindow { .. } => "MinimizeWindow",
            DesktopAction::MaximizeWindow { .. } => "MaximizeWindow",
        }
    }

    /// Query the memory store for past experiences matching this failure.
    pub async fn query_past_experience(
        &self,
        analysis: &FailureAnalysis,
        action: &DesktopAction,
    ) -> Vec<PastExperience> {
        let store = match &self.memory_store {
            Some(s) => s,
            None => return Vec::new(),
        };

        let action_type = Self::action_type_key(action);

        let query = MemoryQuery::new()
            .of_type("failure_experience")
            .with_content(analysis.root_cause.to_string())
            .limit(5);

        let memories = match store.search(query).await {
            Ok(m) => m,
            Err(e) => {
                tracing::warn!("Failed to query failure experiences: {}", e);
                return Vec::new();
            }
        };

        memories
            .into_iter()
            .filter_map(|m| {
                let meta = m.metadata?;
                let stored_action = meta.get("action_type")?.as_str()?;
                if stored_action != action_type {
                    return None;
                }
                Some(PastExperience {
                    root_cause: serde_json::from_value(meta.get("root_cause")?.clone()).ok()?,
                    action_type: stored_action.to_string(),
                    original_delay_ms: meta.get("original_delay_ms")?.as_u64()?,
                    adjusted_delay_ms: meta.get("adjusted_delay_ms")?.as_u64()?,
                    success: meta.get("success")?.as_bool()?,
                })
            })
            .collect()
    }

    /// Adapt retry configuration based on analysis and historical experience.
    pub fn adapt_retry_config(
        &self,
        mut config: crate::computer::VerificationConfig,
        analysis: &FailureAnalysis,
        experiences: &[PastExperience],
    ) -> crate::computer::VerificationConfig {
        // If we have a successful past experience with an adjusted delay, use it.
        let best_experience = experiences
            .iter()
            .filter(|e| e.success)
            .max_by(|a, b| a.adjusted_delay_ms.cmp(&b.adjusted_delay_ms));

        if let Some(exp) = best_experience {
            tracing::info!(
                "Using past experience: {}ms delay worked for {:?} + {:?}",
                exp.adjusted_delay_ms,
                exp.root_cause,
                exp.action_type
            );
            config.retry_delay_ms = exp.adjusted_delay_ms;
            return config;
        }

        // Otherwise, adjust based on root cause heuristics.
        match analysis.root_cause {
            FailureRootCause::TimingIssue => {
                // Exponential backoff: 500 -> 1000 -> 2000.
                config.retry_delay_ms = config.retry_delay_ms.saturating_mul(2).min(5000);
            }
            FailureRootCause::TargetNotFound => {
                config.retry_delay_ms = config.retry_delay_ms.saturating_add(1000).min(5000);
            }
            FailureRootCause::ResourceExhausted => {
                config.retry_delay_ms = config.retry_delay_ms.saturating_mul(3).min(10000);
            }
            FailureRootCause::PermissionDenied => {
                // No point retrying permission issues quickly.
                config.retry_delay_ms = config.retry_delay_ms.saturating_mul(2).min(5000);
            }
            FailureRootCause::StateMismatch => {
                config.retry_delay_ms = config.retry_delay_ms.saturating_add(500).min(3000);
            }
            FailureRootCause::WrongAction | FailureRootCause::Unknown => {
                // Keep default, no adaptation.
            }
        }

        // Also incorporate the analysis's own suggestion if it's higher.
        if let Some(suggested) = analysis.suggested_delay_ms {
            if suggested > config.retry_delay_ms {
                config.retry_delay_ms = suggested;
            }
        }

        config
    }

    /// Record a failure/recovery experience into memory.
    pub async fn record_experience(
        &self,
        analysis: &FailureAnalysis,
        action: &DesktopAction,
        original_delay_ms: u64,
        adjusted_delay_ms: u64,
        success: bool,
    ) {
        let store = match &self.memory_store {
            Some(s) => s.clone(),
            None => return,
        };

        let action_type = Self::action_type_key(action);
        let content = if success {
            format!(
                "{} with {} succeeded after waiting {}ms (was {}ms). Root cause: {:?}.",
                action_type,
                analysis.details,
                adjusted_delay_ms,
                original_delay_ms,
                analysis.root_cause
            )
        } else {
            format!(
                "{} with {} failed after {}ms. Root cause: {:?}.",
                action_type, analysis.details, adjusted_delay_ms, analysis.root_cause
            )
        };

        let memory = Memory {
            id: MemoryId::generate(),
            user_id: "reflection".to_string(),
            conversation_id: None,
            content,
            memory_type: MemoryEntryType::Custom("failure_experience".to_string()),
            embedding: None,
            created_at: SystemTime::now(),
            last_accessed: SystemTime::now(),
            access_count: 0,
            expires_at: None,
            metadata: Some(serde_json::json!({
                "action_type": action_type,
                "root_cause": analysis.root_cause,
                "original_delay_ms": original_delay_ms,
                "adjusted_delay_ms": adjusted_delay_ms,
                "success": success,
                "confidence": analysis.confidence,
            })),
            importance_score: if success { 0.8 } else { 0.5 },
            source: "reflection".to_string(),
        };

        if let Err(e) = store.store(memory).await {
            tracing::warn!("Failed to record reflection experience: {}", e);
        }
    }
}

impl Default for ReflectionEngine {
    fn default() -> Self {
        Self::new()
    }
}

// Display impl for FailureRootCause so it can be stored in memory content.
impl std::fmt::Display for FailureRootCause {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FailureRootCause::TimingIssue => write!(f, "timing_issue"),
            FailureRootCause::TargetNotFound => write!(f, "target_not_found"),
            FailureRootCause::WrongAction => write!(f, "wrong_action"),
            FailureRootCause::ResourceExhausted => write!(f, "resource_exhausted"),
            FailureRootCause::PermissionDenied => write!(f, "permission_denied"),
            FailureRootCause::StateMismatch => write!(f, "state_mismatch"),
            FailureRootCause::Unknown => write!(f, "unknown"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::computer::{ActionResult, DesktopAction, MouseButton, Point};

    #[test]
    fn test_analyze_permission_denied() {
        let engine = ReflectionEngine::new();
        let analysis = engine.analyze_failure(
            &VerificationCriteria::Success,
            &DesktopAction::KillProcess {
                pid: Some(1234),
                name: None,
                force: false,
            },
            &ActionResult::error("Operation not permitted".to_string()),
            0,
        );
        assert_eq!(analysis.root_cause, FailureRootCause::PermissionDenied);
        assert!(analysis.confidence > 0.9);
    }

    #[test]
    fn test_analyze_target_not_found() {
        let engine = ReflectionEngine::new();
        let analysis = engine.analyze_failure(
            &VerificationCriteria::ProcessRunning { name: "chrome".to_string() },
            &DesktopAction::LaunchApp {
                name: "chrome".to_string(),
                args: vec![],
                wait_for_ready: true,
            },
            &ActionResult::error("No process matching 'chrome' found".to_string()),
            0,
        );
        assert_eq!(analysis.root_cause, FailureRootCause::TargetNotFound);
    }

    #[test]
    fn test_analyze_timing_issue() {
        let engine = ReflectionEngine::new();
        let analysis = engine.analyze_failure(
            &VerificationCriteria::UiTreeContains {
                role: "button".to_string(),
                label: Some("OK".to_string()),
            },
            &DesktopAction::Click {
                target: crate::computer::ClickTarget::Coordinate(Point::new(100, 200)),
                button: MouseButton::Left,
            },
            &ActionResult::error("UI tree is empty".to_string()),
            0,
        );
        assert_eq!(analysis.root_cause, FailureRootCause::TimingIssue);
        assert!(analysis.suggested_delay_ms.is_some());
    }

    #[test]
    fn test_adapt_config_timing_issue() {
        let engine = ReflectionEngine::new();
        let config = crate::computer::VerificationConfig {
            max_retries: 2,
            retry_delay_ms: 500,
            baseline_delay_ms: 200,
        };
        let analysis = FailureAnalysis {
            root_cause: FailureRootCause::TimingIssue,
            confidence: 0.9,
            details: "Window not ready".to_string(),
            suggested_delay_ms: Some(2000),
            suggested_fix: None,
        };
        let adapted = engine.adapt_retry_config(config, &analysis, &[]);
        assert_eq!(adapted.retry_delay_ms, 2000); // uses suggested_delay_ms
    }

    #[test]
    fn test_adapt_config_from_experience() {
        let engine = ReflectionEngine::new();
        let config = crate::computer::VerificationConfig {
            max_retries: 2,
            retry_delay_ms: 500,
            baseline_delay_ms: 200,
        };
        let analysis = FailureAnalysis {
            root_cause: FailureRootCause::TimingIssue,
            confidence: 0.8,
            details: "Window not ready".to_string(),
            suggested_delay_ms: Some(1000),
            suggested_fix: None,
        };
        let experiences = vec![PastExperience {
            root_cause: FailureRootCause::TimingIssue,
            action_type: "Click".to_string(),
            original_delay_ms: 500,
            adjusted_delay_ms: 3000,
            success: true,
        }];
        let adapted = engine.adapt_retry_config(config, &analysis, &experiences);
        assert_eq!(adapted.retry_delay_ms, 3000); // uses past experience
    }

    #[test]
    fn test_adapt_config_resource_exhausted() {
        let engine = ReflectionEngine::new();
        let config = crate::computer::VerificationConfig {
            max_retries: 2,
            retry_delay_ms: 500,
            baseline_delay_ms: 200,
        };
        let analysis = FailureAnalysis {
            root_cause: FailureRootCause::ResourceExhausted,
            confidence: 0.9,
            details: "Disk full".to_string(),
            suggested_delay_ms: None,
            suggested_fix: None,
        };
        let adapted = engine.adapt_retry_config(config, &analysis, &[]);
        assert_eq!(adapted.retry_delay_ms, 1500); // 500 * 3
    }
}
