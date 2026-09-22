//! Android input tool — tap, swipe, type and key events.

use async_trait::async_trait;
use serde_json::Value;

use super::{
    adb_args, dump_ui_elements, encode_input_text, encode_tap_sequence, parse_tap_steps,
    resolve_device, resolved_serial, validate_target, DEVICE_PARAM,
};
use crate::computer::platform::mobile::run_cmd;
use crate::tools::{create_schema, Tool, ToolContext, ToolExecutionResult};

// ── ADB Input Tool ─────────────────────────────────────────────────────────

/// Tap, swipe, type, and press keys on the device.
#[derive(Debug)]
pub struct AdbInputTool {
    device: Option<String>,
}

impl Default for AdbInputTool {
    fn default() -> Self {
        Self::new()
    }
}

impl AdbInputTool {
    pub fn new() -> Self {
        Self { device: None }
    }

    pub fn with_device(mut self, device: String) -> Self {
        self.device = Some(device);
        self
    }

    /// Whether the device's active input method is ADBKeyboard.
    ///
    /// ADBKeyboard accepts text over a broadcast, which is the only way to type
    /// non-ASCII without a UI: `input text` goes through the device's key
    /// character map and drops or mangles anything outside ASCII (CJK in
    /// particular). Costs one extra `adb` round trip, so it is only consulted
    /// for text that actually needs it.
    async fn adbkeyboard_is_active(&self, device: &Option<String>) -> bool {
        let args =
            adb_args(device, &["shell", "settings", "get", "secure", "default_input_method"]);
        match run_cmd("adb", &args.iter().map(|s| s.as_str()).collect::<Vec<_>>()).await {
            Ok((status, stdout, _)) if status.success() => {
                stdout.to_lowercase().contains("adbkeyboard")
            }
            _ => false,
        }
    }

    /// Re-read the tree and turn an element index into a tap point.
    ///
    /// Costs one extra dump, which is the point: tapping the coordinates from an
    /// earlier read silently hits whatever has moved into place since.
    async fn resolve_tap_target(
        &self,
        device: &Option<String>,
        index: usize,
        description: Option<&str>,
    ) -> Result<(i32, i32), String> {
        let elements = dump_ui_elements(device).await.map_err(|e| e.message)?;
        validate_target(&elements, index, description).map(|e| e.center)
    }
}

#[async_trait]
impl Tool for AdbInputTool {
    fn name(&self) -> &str {
        "android_input"
    }

    fn description(&self) -> &str {
        "Send tap, swipe, text, or key events to an Android device via ADB. For taps prefer \
         `target` (an element index from android_ui_tree): it is re-read from the live screen \
         before dispatch, so a screen that changed under you is reported instead of silently \
         tapping the wrong thing. Use action 'sequence' with several coordinate steps when you \
         need to hit something that will disappear before your next turn. Success means the \
         input was dispatched, not that it took effect — observe afterwards to see what the \
         screen actually did."
    }

    fn parameters_schema(&self) -> Value {
        create_schema(
            "Send input to Android device",
            serde_json::json!({
                "action": {
                    "type": "string",
                    "description": "tap | sequence | swipe | text | key",
                    "enum": ["tap", "sequence", "swipe", "text", "key"]
                },
                "steps": {
                    "type": "array",
                    "description": "For 'sequence': taps to dispatch back to back in one call, for a control that will auto-fade before your next turn. Each is { \"x\": int, \"y\": int }.",
                    "items": {
                        "type": "object",
                        "properties": {
                            "x": { "type": "integer" },
                            "y": { "type": "integer" }
                        },
                        "required": ["x", "y"]
                    }
                },
                "target": {
                    "type": "integer",
                    "description": "Element index from android_ui_tree. Preferred over x/y for taps: the element is re-read from the live screen and the tap is refused if it moved, changed, or is not clickable."
                },
                "target_description": {
                    "type": "string",
                    "description": "Optional label you expect at `target`. A mismatch refuses the tap and reports what is actually there."
                },
                "x": { "type": "integer", "description": "X coordinate (tap/swipe). No safety net — prefer `target`." },
                "y": { "type": "integer", "description": "Y coordinate (tap/swipe). No safety net — prefer `target`." },
                "x2": { "type": "integer", "description": "End X (swipe)" },
                "y2": { "type": "integer", "description": "End Y (swipe)" },
                "text": { "type": "string", "description": "Text to type" },
                "keycode": { "type": "string", "description": "Android keycode name or number" },
                "device": { "type": "string", "description": DEVICE_PARAM }
            }),
            vec!["action"],
        )
    }

    async fn execute(
        &self,
        args: Value,
        _context: &ToolContext,
    ) -> crate::Result<ToolExecutionResult> {
        // One device: two conversations must not type into it at once. See
        // `tools::target_lock`.
        let _target = crate::tools::target_lock::TargetLocks::global()
            .lock(crate::tools::target_lock::ANDROID)
            .await;
        let action = args.get("action").and_then(|v| v.as_str()).unwrap_or("tap");
        let device = match resolve_device(&args, self.device.as_deref()) {
            Ok(device) => device,
            Err(refusal) => return Ok(ToolExecutionResult::error(refusal)),
        };

        let shell_cmd = match action {
            "tap" => {
                match args.get("target").and_then(|v| v.as_u64()) {
                    // Preferred: an element index, re-checked against the live
                    // screen before anything is dispatched.
                    Some(index) => {
                        let description = args.get("target_description").and_then(|v| v.as_str());
                        match self
                            .resolve_tap_target(&device, index as usize, description)
                            .await
                        {
                            Ok((x, y)) => format!("input tap {} {}", x, y),
                            Err(refusal) => return Ok(ToolExecutionResult::error(refusal)),
                        }
                    }
                    // Raw coordinates: no safety net, the caller is on its own.
                    None => {
                        let x = args.get("x").and_then(|v| v.as_i64()).unwrap_or(0);
                        let y = args.get("y").and_then(|v| v.as_i64()).unwrap_or(0);
                        format!("input tap {} {}", x, y)
                    }
                }
            }
            "swipe" => {
                let x1 = args.get("x").and_then(|v| v.as_i64()).unwrap_or(0);
                let y1 = args.get("y").and_then(|v| v.as_i64()).unwrap_or(0);
                let x2 = args.get("x2").and_then(|v| v.as_i64()).unwrap_or(0);
                let y2 = args.get("y2").and_then(|v| v.as_i64()).unwrap_or(0);
                format!("input swipe {} {} {} {}", x1, y1, x2, y2)
            }
            "text" => {
                let text = args.get("text").and_then(|v| v.as_str()).unwrap_or("");
                if text.is_empty() {
                    return Ok(ToolExecutionResult::error(
                        "action 'text' requires a non-empty 'text' argument",
                    ));
                }
                if text.is_ascii() {
                    encode_input_text(text)
                } else if self.adbkeyboard_is_active(&device).await {
                    // ADBKeyboard takes the text base64-encoded over a broadcast,
                    // which carries any character the device can render.
                    let payload = base64::Engine::encode(
                        &base64::engine::general_purpose::STANDARD,
                        text.as_bytes(),
                    );
                    format!("am broadcast -a ADB_INPUT_B64 --es msg '{}'", payload)
                } else {
                    // `input text` types through the device's key character map,
                    // so non-ASCII (CJK in particular) comes out dropped or
                    // mangled. Fail loudly rather than silently typing the wrong
                    // thing — the previous behaviour replaced only spaces and
                    // then sent the text as-is.
                    return Ok(ToolExecutionResult::error(
                        "Cannot type non-ASCII text: `input text` only types ASCII reliably and \
                         ADBKeyboard is not the active input method. Install/enable ADBKeyboard \
                         (com.android.adbkeyboard) and select it as the input method, or keep the \
                         text ASCII.",
                    ));
                }
            }
            "sequence" => match parse_tap_steps(args.get("steps")) {
                Ok(steps) => encode_tap_sequence(&steps),
                Err(msg) => return Ok(ToolExecutionResult::error(msg)),
            },
            "key" => {
                let keycode = args
                    .get("keycode")
                    .and_then(|v| v.as_str())
                    .unwrap_or("HOME");
                format!("input keyevent {}", keycode)
            }
            _ => return Ok(ToolExecutionResult::error(format!("Unknown action: {}", action))),
        };

        let argv = adb_args(&device, &["shell", &shell_cmd]);
        let (status, _stdout, stderr) =
            run_cmd("adb", &argv.iter().map(|s| s.as_str()).collect::<Vec<_>>())
                .await
                .map_err(|e| crate::error::SyscityError::ExternalService {
                    source: "adb input failed".to_string(),
                    cause: Some(Box::new(e)),
                })?;

        if !status.success() {
            return Ok(ToolExecutionResult::error(format!("adb input failed: {}", stderr)));
        }

        Ok(ToolExecutionResult::success(format!("Input '{}' sent", action))
            .with_data(serde_json::json!({ "device": resolved_serial(&device).await })))
    }
}
