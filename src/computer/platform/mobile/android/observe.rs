//! Android observe tool — one call returns the screenshot and the UI tree.

use async_trait::async_trait;
use serde_json::Value;

use super::{
    capture_png, coordinate_space_consistent, dump_ui, format_ui_summary, png_dimensions,
    resolve_device, resolved_serial, DEVICE_PARAM,
};
use crate::tools::{create_schema, Tool, ToolContext, ToolExecutionResult};

// ── ADB Observe Tool ───────────────────────────────────────────────────────

/// Screenshot **and** UI tree from one call, with the coordinate space made
/// explicit.
///
/// Two separate adb reads cannot be simultaneous, but doing them back to back in
/// one call shrinks the window — and, more usefully, the result states what was
/// actually captured. If the screenshot's real pixel size and the rectangle the
/// element tree describes disagree (a rotation, a scaled capture), then every
/// coordinate in the tree is wrong for the image the model is looking at, and it
/// is told so instead of discovering it by tapping the wrong place.
#[derive(Debug)]
pub struct AdbObserveTool {
    device: Option<String>,
}

impl Default for AdbObserveTool {
    fn default() -> Self {
        Self::new()
    }
}

impl AdbObserveTool {
    pub fn new() -> Self {
        Self { device: None }
    }

    pub fn with_device(mut self, device: String) -> Self {
        self.device = Some(device);
        self
    }
}

#[async_trait]
impl Tool for AdbObserveTool {
    fn name(&self) -> &str {
        "android_observe"
    }

    fn description(&self) -> &str {
        "Look at the Android device: returns the numbered actionable elements and a screenshot \
         together, so the elements and the image describe the same moment. Preferred over \
         calling android_ui_tree and android_screenshot separately. Says so when the two \
         disagree about the screen size, which would make the element coordinates unusable. When \
         the UI tree cannot be read at all it still returns the screenshot and says why: in that \
         case the elements are absent from the result rather than empty, because an empty list \
         means the tree was read and held nothing actionable."
    }

    fn parameters_schema(&self) -> Value {
        create_schema(
            "Observe the Android device",
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
        let png = match capture_png(&device).await {
            Ok(png) => png,
            Err(e) => return Ok(ToolExecutionResult::error(e.to_string())),
        };
        let as_size = |dims: Option<(u32, u32)>| {
            dims.map(|(w, h)| serde_json::json!({ "width": w, "height": h }))
        };

        let dump = match dump_ui(&device).await {
            Ok(dump) => dump,
            // A tree that cannot be read does not fail this call. The screenshot
            // is already in hand and is most of what the tool is for, and every
            // message about a failed dump tells the model to work from the
            // screenshot that android_observe returns — which an error result
            // would throw away, since its payload does not reach the model. So
            // the observation comes back with the image, no elements, and the
            // failure's code for a caller to branch on.
            Err(failure) => {
                let shot = png_dimensions(&png);
                let size = shot
                    .map(|(w, h)| format!("{w}x{h}"))
                    .unwrap_or_else(|| "unknown size".to_string());

                let mut data = serde_json::Map::new();
                data.insert("tree_available".to_string(), serde_json::json!(false));
                data.insert("code".to_string(), serde_json::json!(failure.code));
                data.insert(
                    "screenshot".to_string(),
                    as_size(shot).unwrap_or(serde_json::Value::Null),
                );
                data.insert(
                    "screenshot_base64".to_string(),
                    serde_json::json!(base64::Engine::encode(
                        &base64::engine::general_purpose::STANDARD,
                        &png,
                    )),
                );
                data.insert(
                    "device".to_string(),
                    serde_json::json!(resolved_serial(&device).await),
                );

                return Ok(ToolExecutionResult::success(format!(
                    "Screenshot captured ({size}), but the UI tree could not be read, so there are \
                     no elements in this result — absent, not empty. {}",
                    failure.message
                ))
                .with_data(serde_json::Value::Object(data)));
            }
        };

        let shot = png_dimensions(&png);
        let consistent = coordinate_space_consistent(dump.screen, shot);

        let mut summary = format_ui_summary(&dump.elements);
        if !consistent {
            let (sw, sh) = dump.screen.unwrap_or((0, 0));
            let (iw, ih) = shot.unwrap_or((0, 0));
            summary.push_str(&format!(
                "\n\nWARNING: the screenshot ({iw}x{ih}) and the UI tree ({sw}x{sh}) do not describe \
                 the same screen — it most likely rotated or was scaled between the two reads. \
                 The element coordinates above may not match the image: observe again before \
                 tapping."
            ));
        }

        Ok(ToolExecutionResult::success(summary).with_data(serde_json::json!({
            "count": dump.elements.len(),
            "elements": dump.elements,
            "screenshot_base64": base64::Engine::encode(
                &base64::engine::general_purpose::STANDARD,
                &png,
            ),
            "screenshot": as_size(shot),
            "screen": dump.screen.map(|(w, h)| serde_json::json!({ "width": w, "height": h })),
            "coordinates_consistent": consistent,
            "device": resolved_serial(&device).await,
        })))
    }
}
