//! Android verify tool — did the screen end up showing what was expected?

use async_trait::async_trait;
use serde_json::Value;

use super::{
    dump_ui_elements, evaluate, resolve_device, resolved_serial, Expectation, DEVICE_PARAM,
    VERDICT_UNKNOWN,
};
use crate::tools::{create_schema, Tool, ToolContext, ToolExecutionResult};

// ── ADB Verify Tool ────────────────────────────────────────────────────────

/// Check that the screen shows what was expected.
#[derive(Debug)]
pub struct AdbVerifyTool {
    device: Option<String>,
}

impl Default for AdbVerifyTool {
    fn default() -> Self {
        Self::new()
    }
}

impl AdbVerifyTool {
    pub fn new() -> Self {
        Self { device: None }
    }

    pub fn with_device(mut self, device: String) -> Self {
        self.device = Some(device);
        self
    }
}

#[async_trait]
impl Tool for AdbVerifyTool {
    fn name(&self) -> &str {
        "android_verify"
    }

    fn description(&self) -> &str {
        "Check that the device's screen now shows what you expect, after an action. Answers \
         `satisfied`, `unsatisfied` or `unknown` — and `unknown` never means success: it means the \
         check could not be made, and the result says why. The evidence is the accessibility tree, \
         nothing else: not a screenshot comparison, and not the fact that an action was dispatched \
         earlier. Ask this when the question is whether something worked; android_observe is for \
         looking around."
    }

    fn parameters_schema(&self) -> Value {
        create_schema(
            "Verify the Android screen",
            serde_json::json!({
                "text": { "type": "string", "description": "The screen should show this text, in some element's text or content description" },
                "target": { "type": "integer", "description": "An element index from android_ui_tree that should still be there" },
                "target_description": { "type": "string", "description": "Optional label you expect at `target`" },
                "device": { "type": "string", "description": DEVICE_PARAM }
            }),
            Vec::<String>::new(),
        )
    }

    async fn execute(
        &self,
        args: Value,
        _context: &ToolContext,
    ) -> crate::Result<ToolExecutionResult> {
        let device = match resolve_device(&args, self.device.as_deref()) {
            Ok(device) => device,
            Err(refusal) => return Ok(ToolExecutionResult::error(refusal)),
        };
        let expectation = match Expectation::parse(&args) {
            Ok(expectation) => expectation,
            Err(refusal) => return Ok(ToolExecutionResult::error(refusal)),
        };
        let checked = expectation.describe();

        match dump_ui_elements(&device).await {
            Ok(elements) => {
                let verdict = evaluate(&expectation, &elements);
                Ok(ToolExecutionResult::success(format!(
                    "{verdict}: checked that {checked} (read {} element(s) from the tree)",
                    elements.len()
                ))
                .with_data(serde_json::json!({
                    "verdict": verdict,
                    "checked": checked,
                    "elements_read": elements.len(),
                    "device": resolved_serial(&device).await,
                })))
            }
            Err(failure) => Ok(ToolExecutionResult::success(format!(
                "{VERDICT_UNKNOWN}: could not check that {checked} — {}",
                failure.message
            ))
            .with_data(serde_json::json!({
                "verdict": VERDICT_UNKNOWN,
                "checked": checked,
                "code": failure.code,
                "device": resolved_serial(&device).await,
            }))),
        }
    }
}
