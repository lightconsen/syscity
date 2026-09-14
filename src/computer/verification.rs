//! Verification engine — execute desktop actions and verify results.
//!
//! Wraps [`ComputerAdapter`] with automatic retry and post-action
//! validation so the Agent can be sure an action had the intended
//! effect before moving on.

use std::sync::Arc;
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::computer::reflection::{FailureAnalysis, ReflectionEngine};
use crate::computer::{
    ActionResult, ComputerAdapter, ComputerError, DesktopAction, Result, Screenshot,
};

/// Criteria used to verify that an action produced the expected outcome.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VerificationCriteria {
    /// UI tree must contain an element with the given role (and optionally
    /// label).
    UiTreeContains { role: String, label: Option<String> },
    /// Screenshot must differ from the baseline by **more** than
    /// `max_pixel_diff` — something visible has to have changed. A baseline is
    /// captured automatically before the action.
    ScreenshotChanged { max_pixel_diff: u32 },
    /// Screenshot must be stable (two consecutive screenshots differ by no more
    /// than `max_pixel_diff`). Useful after waiting for animations to finish.
    ScreenshotStable { max_pixel_diff: u32, poll_ms: u64 },
    /// A process with the given name must be running.
    ProcessRunning { name: String },
    /// A process with the given name must no longer be running.
    ProcessExited { name: String },
    /// The active window title must contain the given pattern.
    WindowTitleContains { pattern: String },
    /// A file must exist at the given path.
    FileExists { path: String },
    /// The action result message must contain the given text.
    OutputContains { text: String },
    /// The action result must indicate success.
    Success,
}

/// Configuration for the verification / retry loop.
#[derive(Debug, Clone, Copy)]
pub struct VerificationConfig {
    /// Maximum number of retry attempts (0 = no retries).
    pub max_retries: u32,
    /// Delay between retries (also used as settle time before first
    /// verification).
    pub retry_delay_ms: u64,
    /// Delay before capturing the "before" baseline screenshot.
    pub baseline_delay_ms: u64,
}

impl Default for VerificationConfig {
    fn default() -> Self {
        Self {
            max_retries: 2,
            retry_delay_ms: 500,
            baseline_delay_ms: 200,
        }
    }
}

/// Whether a screenshot difference satisfies `ScreenshotChanged`.
///
/// The direction is the whole content of the check: a criterion named for
/// change is satisfied by a difference *larger* than the threshold.
fn changed_beyond(diff: u32, max_pixel_diff: u32) -> bool {
    diff > max_pixel_diff
}

/// Execute actions and verify their outcomes.
#[derive(Clone)]
pub struct VerificationEngine {
    adapter: Arc<dyn ComputerAdapter>,
    config: VerificationConfig,
    reflection: Option<ReflectionEngine>,
}

impl VerificationEngine {
    pub fn new(adapter: Arc<dyn ComputerAdapter>) -> Self {
        Self {
            adapter,
            config: VerificationConfig::default(),
            reflection: None,
        }
    }

    pub fn with_config(mut self, config: VerificationConfig) -> Self {
        self.config = config;
        self
    }

    /// Attach a reflection engine for root-cause analysis and adaptive retry.
    pub fn with_reflection(mut self, reflection: ReflectionEngine) -> Self {
        self.reflection = Some(reflection);
        self
    }

    /// Execute an action and verify the result.
    ///
    /// 1. Capture baseline (screenshot / UI tree) if needed by criteria.
    /// 2. Execute the action.
    /// 3. Wait a short settle time.
    /// 4. Verify against criteria.
    /// 5. Retry up to `max_retries` if verification fails.
    ///
    /// When a [`ReflectionEngine`] is attached, each failure is analysed to
    /// determine the root cause, past experiences are queried, and the retry
    /// delay is adapted before the next attempt.
    pub async fn execute_with_verification(
        &self,
        action: DesktopAction,
        criteria: VerificationCriteria,
    ) -> Result<ActionResult> {
        let mut baseline: Option<Screenshot> = None;

        // Capture baseline screenshot if needed.
        if matches!(
            criteria,
            VerificationCriteria::ScreenshotChanged { .. }
                | VerificationCriteria::ScreenshotStable { .. }
        ) {
            tokio::time::sleep(Duration::from_millis(self.config.baseline_delay_ms)).await;
            baseline = Some(self.adapter.screenshot(None).await?);
        }

        let mut current_config = self.config;
        let mut last_analysis: Option<FailureAnalysis> = None;

        for attempt in 0..=current_config.max_retries {
            let result = self.adapter.execute(action.clone()).await?;

            // Wait for UI to settle before verifying.
            tokio::time::sleep(Duration::from_millis(current_config.retry_delay_ms)).await;

            match self.verify(&criteria, &result, baseline.as_ref()).await {
                Ok(true) => {
                    // Success — record experience if reflection is enabled.
                    if let Some(ref reflection) = self.reflection {
                        if let Some(ref analysis) = last_analysis {
                            reflection
                                .record_experience(
                                    analysis,
                                    &action,
                                    self.config.retry_delay_ms,
                                    current_config.retry_delay_ms,
                                    true,
                                )
                                .await;
                        }
                    }
                    return Ok(result);
                }
                Ok(false) => {
                    if attempt < current_config.max_retries {
                        if let Some(ref reflection) = self.reflection {
                            let analysis =
                                reflection.analyze_failure(&criteria, &action, &result, attempt);
                            tracing::warn!(
                                "Verification failed (attempt {}/{}): {} — adapting retry strategy",
                                attempt + 1,
                                current_config.max_retries + 1,
                                analysis.details
                            );
                            let experiences =
                                reflection.query_past_experience(&analysis, &action).await;
                            current_config = reflection.adapt_retry_config(
                                current_config,
                                &analysis,
                                &experiences,
                            );
                            last_analysis = Some(analysis);
                        } else {
                            tracing::warn!(
                                "Verification failed (attempt {}/{}), retrying...",
                                attempt + 1,
                                current_config.max_retries + 1
                            );
                        }
                        tokio::time::sleep(Duration::from_millis(current_config.retry_delay_ms))
                            .await;
                    } else {
                        // Final attempt failed — record failure experience.
                        if let Some(ref reflection) = self.reflection {
                            let analysis = last_analysis.clone().unwrap_or_else(|| {
                                reflection.analyze_failure(&criteria, &action, &result, attempt)
                            });
                            reflection
                                .record_experience(
                                    &analysis,
                                    &action,
                                    self.config.retry_delay_ms,
                                    current_config.retry_delay_ms,
                                    false,
                                )
                                .await;
                        }
                    }
                }
                Err(e) => {
                    // Verification itself errored — abort unless we can retry.
                    if attempt < current_config.max_retries {
                        if let Some(ref reflection) = self.reflection {
                            let analysis = reflection.analyze_failure(
                                &criteria,
                                &action,
                                &ActionResult::error(e.to_string()),
                                attempt,
                            );
                            tracing::warn!(
                                "Verification error (attempt {}/{}): {} — {} — retrying...",
                                attempt + 1,
                                current_config.max_retries + 1,
                                e,
                                analysis.details
                            );
                            let experiences =
                                reflection.query_past_experience(&analysis, &action).await;
                            current_config = reflection.adapt_retry_config(
                                current_config,
                                &analysis,
                                &experiences,
                            );
                            last_analysis = Some(analysis);
                        } else {
                            tracing::warn!(
                                "Verification error (attempt {}/{}): {}, retrying...",
                                attempt + 1,
                                current_config.max_retries + 1,
                                e
                            );
                        }
                        tokio::time::sleep(Duration::from_millis(current_config.retry_delay_ms))
                            .await;
                    } else {
                        return Err(e);
                    }
                }
            }
        }

        Err(ComputerError::Other(format!(
            "Verification failed after {} attempts",
            current_config.max_retries + 1
        )))
    }

    /// Verify that the current state matches the criteria.
    pub async fn verify(
        &self,
        criteria: &VerificationCriteria,
        result: &ActionResult,
        baseline: Option<&Screenshot>,
    ) -> Result<bool> {
        match criteria {
            VerificationCriteria::UiTreeContains { role, label } => {
                let tree = self.adapter.read_ui_tree(None).await?;
                Ok(tree.iter().any(|e| {
                    e.role == *role
                        && label
                            .as_ref()
                            .map(|l| e.label.as_ref().map(|el| el.contains(l)).unwrap_or(false))
                            .unwrap_or(true)
                }))
            }
            VerificationCriteria::ScreenshotChanged { max_pixel_diff } => {
                let before = baseline.ok_or_else(|| {
                    ComputerError::Other("No baseline screenshot for diff verification".to_string())
                })?;
                let after = self.adapter.screenshot(None).await?;
                let diff = compute_screenshot_diff(before, &after);
                Ok(changed_beyond(diff, *max_pixel_diff))
            }
            VerificationCriteria::ScreenshotStable { max_pixel_diff, poll_ms } => {
                let first = self.adapter.screenshot(None).await?;
                tokio::time::sleep(Duration::from_millis(*poll_ms)).await;
                let second = self.adapter.screenshot(None).await?;
                let diff = compute_screenshot_diff(&first, &second);
                Ok(diff <= *max_pixel_diff)
            }
            VerificationCriteria::ProcessRunning { name } => {
                self.adapter
                    .wait_for(
                        crate::computer::WaitCondition::ProcessRunning { name: name.clone() },
                        Duration::from_secs(1),
                    )
                    .await
            }
            VerificationCriteria::ProcessExited { name } => {
                self.adapter
                    .wait_for(
                        crate::computer::WaitCondition::ProcessExited { name: name.clone() },
                        Duration::from_secs(1),
                    )
                    .await
            }
            VerificationCriteria::WindowTitleContains { pattern } => {
                self.adapter
                    .wait_for(
                        crate::computer::WaitCondition::WindowTitleContains {
                            pattern: pattern.clone(),
                        },
                        Duration::from_secs(1),
                    )
                    .await
            }
            VerificationCriteria::FileExists { path } => {
                self.adapter
                    .wait_for(
                        crate::computer::WaitCondition::FileExists { path: path.clone() },
                        Duration::from_secs(1),
                    )
                    .await
            }
            VerificationCriteria::OutputContains { text } => Ok(result.message.contains(text)),
            VerificationCriteria::Success => Ok(result.success),
        }
    }
}

/// Compute a difference metric between two screenshots.
///
/// Returns 0 for identical images. Otherwise counts differing bytes
/// between the decoded PNG data. A future enhancement could decode the
/// PNGs and count per-pixel differences using the `image` crate.
fn compute_screenshot_diff(a: &Screenshot, b: &Screenshot) -> u32 {
    if a.base64 == b.base64 {
        return 0;
    }

    let Ok(a_bytes) = base64::Engine::decode(&base64::engine::general_purpose::STANDARD, &a.base64)
    else {
        return u32::MAX;
    };
    let Ok(b_bytes) = base64::Engine::decode(&base64::engine::general_purpose::STANDARD, &b.base64)
    else {
        return u32::MAX;
    };

    let min_len = a_bytes.len().min(b_bytes.len());
    let mut diff = 0u32;
    for i in 0..min_len {
        if a_bytes[i] != b_bytes[i] {
            diff += 1;
        }
    }
    // Length difference also counts as differences
    diff += (a_bytes.len().max(b_bytes.len()) - min_len) as u32;
    diff
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Whether a screenshot difference satisfies `ScreenshotChanged`.
    ///
    /// Separate and named because the direction is the entire content of the
    /// check — and it was the wrong way round. The criterion passed when the
    /// difference was *at or below* the threshold, which is the question
    /// `ScreenshotStable` asks, so a check named for change was satisfied by a
    /// screen that had not moved.
    #[test]
    fn screenshot_changed_means_changed() {
        assert!(changed_beyond(1, 0));
        assert!(changed_beyond(4_000, 100));
        assert!(changed_beyond(101, 100));

        // Unchanged is the case that used to pass.
        assert!(!changed_beyond(0, 0));
        // At the threshold is not beyond it.
        assert!(!changed_beyond(100, 100));
    }

    #[test]
    fn test_verification_config_default() {
        let cfg = VerificationConfig::default();
        assert_eq!(cfg.max_retries, 2);
        assert_eq!(cfg.retry_delay_ms, 500);
    }

    #[test]
    fn test_screenshot_diff_identical() {
        let ss = Screenshot::new("aGVsbG8=".to_string(), 100, 100);
        assert_eq!(compute_screenshot_diff(&ss, &ss), 0);
    }

    #[test]
    fn test_screenshot_diff_different() {
        let a = Screenshot::new("aGVsbG8=".to_string(), 100, 100);
        let b = Screenshot::new("d29ybGQ=".to_string(), 100, 100);
        // "hello" vs "world" — decoded bytes differ
        assert!(compute_screenshot_diff(&a, &b) > 0);
        assert!(compute_screenshot_diff(&a, &b) < u32::MAX);
    }

    #[test]
    fn test_verification_criteria_clone_eq() {
        let c1 = VerificationCriteria::Success;
        let c2 = VerificationCriteria::Success;
        assert_eq!(c1, c2);

        let c3 = VerificationCriteria::UiTreeContains {
            role: "button".to_string(),
            label: Some("OK".to_string()),
        };
        let c4 = c3.clone();
        assert_eq!(c3, c4);
    }
}
