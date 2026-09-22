//! Android UI tree tool — the numbered element list.

use async_trait::async_trait;
use serde_json::Value;

use super::{dump_ui_elements, format_ui_summary, resolve_device, resolved_serial, DEVICE_PARAM};
use crate::tools::{create_schema, Tool, ToolContext, ToolExecutionResult};

// ── ADB UI Tree Tool ───────────────────────────────────────────────────────

/// Dump the Android accessibility UI tree via `uiautomator`.
#[derive(Debug)]
pub struct AdbUiTreeTool {
    device: Option<String>,
}

impl Default for AdbUiTreeTool {
    fn default() -> Self {
        Self::new()
    }
}

impl AdbUiTreeTool {
    pub fn new() -> Self {
        Self { device: None }
    }

    pub fn with_device(mut self, device: String) -> Self {
        self.device = Some(device);
        self
    }
}

#[async_trait]
impl Tool for AdbUiTreeTool {
    fn name(&self) -> &str {
        "android_ui_tree"
    }

    fn description(&self) -> &str {
        "Read the Android device's UI: a numbered list of the elements you can act on, each with \
         its text or content description, whether it is clickable, and the screen coordinates of \
         its centre. Tap an element with android_input by passing that centre as x/y. Bare layout \
         containers are omitted."
    }

    fn parameters_schema(&self) -> Value {
        create_schema(
            "Get Android UI tree",
            serde_json::json!({
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
        // The raw dump is an order of magnitude larger than what the model can
        // use, and the model would have to parse `bounds` itself to act on it.
        // Hand back an indexed list instead; the XML stays out of the context.
        let elements = match dump_ui_elements(&device).await {
            Ok(e) => e,
            Err(e) => {
                return Ok(ToolExecutionResult::error(e.message)
                    .with_data(serde_json::json!({ "code": e.code })))
            }
        };
        let summary = format_ui_summary(&elements);
        Ok(ToolExecutionResult::success(summary).with_data(serde_json::json!({
            "count": elements.len(),
            "elements": elements,
            "device": resolved_serial(&device).await,
        })))
    }
}
