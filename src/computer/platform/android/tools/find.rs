//! Scroll until a label appears, and say where it landed.

use super::*;

/// Scroll until a label appears, and return where it is.
#[derive(Debug)]
pub struct AdbFindTool {
    device: Option<String>,
}

impl Default for AdbFindTool {
    fn default() -> Self {
        Self::new()
    }
}

impl AdbFindTool {
    pub fn new() -> Self {
        Self { device: None }
    }

    pub fn with_device(mut self, device: String) -> Self {
        self.device = Some(device);
        self
    }
}

#[async_trait]
impl Tool for AdbFindTool {
    fn name(&self) -> &str {
        "android_find"
    }

    fn description(&self) -> &str {
        "Scroll until an element with this text appears and return its coordinates, so you can \
         act without reading every screen in between. Matching is case-insensitive and looks at \
         the element's text or content description. Says plainly when it was not found, and how \
         far it looked."
    }

    fn parameters_schema(&self) -> Value {
        create_schema(
            "Find an element by scrolling",
            serde_json::json!({
                "text": {
                    "type": "string",
                    "description": "Text the element shows, or its content description."
                },
                "max_scrolls": {
                    "type": "integer",
                    "description": "How many screens to scroll before giving up (default 8, max 30)."
                },
                "direction": {
                    "type": "string",
                    "description": "down (default) or up.",
                    "enum": ["down", "up"]
                },
                "display": { "type": "integer", "description": DISPLAY_PARAM },
                "device": { "type": "string", "description": DEVICE_PARAM }
            }),
            vec!["text"],
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
        let _target = super::lock_device(&device).await;
        let target = match Target::from_args(&args) {
            Ok(target) => target,
            Err(message) => return Ok(ToolExecutionResult::error(message)),
        };
        let needle = args
            .get("text")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .trim()
            .to_lowercase();
        if needle.is_empty() {
            return Ok(ToolExecutionResult::error("'text' must be non-empty"));
        }
        let max_scrolls = args
            .get("max_scrolls")
            .and_then(|v| v.as_u64())
            .unwrap_or(8)
            .clamp(0, 30) as usize;
        let direction = match args.get("direction").and_then(|v| v.as_str()) {
            Some(value) => match Direction::parse(Some(value)) {
                Ok(direction) => direction,
                Err(message) => return Ok(ToolExecutionResult::error(message)),
            },
            None => Direction::Down,
        };

        let mut size: Option<(i32, i32)> = None;
        for step in 0..=max_scrolls {
            let dump = match dump_ui(&device, target).await {
                Ok(dump) => dump,
                Err(failure) => {
                    return Ok(ToolExecutionResult::error(failure.message)
                        .with_data(serde_json::json!({ "code": failure.code })))
                }
            };
            size = screen_size(&device, dump.screen).await.or(size);

            if let Some(found) = dump.elements.iter().find(|element| {
                element_label(element).is_some_and(|label| label.to_lowercase().contains(&needle))
            }) {
                return Ok(ToolExecutionResult::success(format!(
                    "Found after {} scroll(s): [{}] {} \"{}\" center=({},{}). Tap it with \
                     android_input using these coordinates, or re-read with android_ui_tree for \
                     its current index.",
                    step,
                    found.index,
                    found.class,
                    element_label(found).unwrap_or("-"),
                    found.center.0,
                    found.center.1
                ))
                .with_data(serde_json::json!({
                    "found": found,
                    // The screen the centre is a point of, so the caller can
                    // name it in the 0–999 space — which is what keeps a
                    // coordinate usable on a screen of another size.
                    "screen": size.map(|(width, height)| serde_json::json!({
                        "width": width,
                        "height": height,
                    })),
                    "scrolls": step,
                    "device": resolved_serial(&device).await,
                })));
            }

            if step == max_scrolls {
                break;
            }
            let Some(size) = size else {
                return Ok(ToolExecutionResult::error(
                    "the screen size is unknown, so it cannot be scrolled; scroll by hand with \
                     android_input action 'swipe' and read again",
                ));
            };
            let container = scroll_container(&dump.elements, size, direction);
            if let Err(message) = scroll_once(&device, size, direction, target, container).await {
                return Ok(ToolExecutionResult::error(message));
            }
            let before = tree_fingerprint(&dump.elements);
            let settle = settle_after_action(&device, target, Some(before), 900).await;
            if settle.effect == EFFECT_UNCHANGED {
                return Ok(ToolExecutionResult::success(format!(
                    "Not found: the screen stopped moving after {step} scroll(s), so there is \
                     nothing more in this direction. It may be on another tab or page."
                ))
                .with_data(serde_json::json!({
                    "found": Value::Null,
                    "scrolls": step,
                    "device": resolved_serial(&device).await,
                })));
            }
        }

        Ok(ToolExecutionResult::success(format!(
            "Not found after {max_scrolls} scroll(s). The list may have ended, or the text may \
             be drawn rather than exposed — read the screen with android_observe and look at the \
             screenshot."
        ))
        .with_data(serde_json::json!({
            "found": Value::Null,
            "scrolls": max_scrolls,
            "device": resolved_serial(&device).await,
        })))
    }
}
