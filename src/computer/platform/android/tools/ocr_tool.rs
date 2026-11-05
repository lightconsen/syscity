//! Read the screen from the screenshot, for when the tree says nothing.

use super::*;

/// Read the screen's text from the screenshot.
#[derive(Debug)]
pub struct AdbOcrTool {
    device: Option<String>,
}

impl Default for AdbOcrTool {
    fn default() -> Self {
        Self::new()
    }
}

impl AdbOcrTool {
    pub fn new() -> Self {
        Self { device: None }
    }

    pub fn with_device(mut self, device: String) -> Self {
        self.device = Some(device);
        self
    }
}

#[async_trait]
impl Tool for AdbOcrTool {
    fn name(&self) -> &str {
        "android_ocr"
    }

    fn description(&self) -> &str {
        "Read the screen's text with OCR when the accessibility tree cannot describe it. \
         android_ui_tree and android_observe already fall back to this on their own; call it \
         directly when you want only the text, or to repeat a read after the screen changed. Each \
         line comes with the centre coordinates to tap. The engine differs by host: on the phone \
         itself it is ML Kit's bundled Chinese model (offline, on-device), while a desktop driving \
         a device uses its own ONNX model — so the same screen can read slightly differently."
    }

    fn parameters_schema(&self) -> Value {
        create_schema(
            "Read text from the screen with OCR",
            serde_json::json!({
                "display": { "type": "integer", "description": DISPLAY_PARAM },
                "device": { "type": "string", "description": DEVICE_PARAM }
            }),
            Vec::<String>::new(),
        )
    }

    async fn execute(
        &self,
        args: Value,
        context: &ToolContext,
    ) -> crate::Result<ToolExecutionResult> {
        let device = match resolve_device(&args, self.device.as_deref()) {
            Ok(device) => device,
            Err(refusal) => return Ok(ToolExecutionResult::error(refusal)),
        };
        let target = match Target::from_args(&args) {
            Ok(target) => target,
            Err(message) => return Ok(ToolExecutionResult::error(message)),
        };
        // A read loop is refused before the ADB work, not after: the screen has
        // been read repeatedly without telling the model anything new.
        let guard_key = super::guard_key_for(context, device.as_deref(), target);
        if let Some(reason) = super::guard::Guard::global()
            .check_read(&guard_key, "android_ocr")
            .refusal()
        {
            return Ok(ToolExecutionResult::error(reason.to_string()));
        }
        match ocr_screen(&device, target).await {
            Ok(lines) => {
                let note = super::read_note_ocr(&guard_key, "android_ocr", &lines);
                Ok(ToolExecutionResult::success(super::with_note(format_ocr_lines(&lines), note))
                    .with_data(serde_json::json!({
                        "lines": lines,
                        "count": lines.len(),
                        "device": resolved_serial(&device).await,
                    })))
            }
            Err(message) => {
                Ok(ToolExecutionResult::error(format!("OCR is unavailable: {message}")))
            }
        }
    }
}
