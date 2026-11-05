//! Walk a long screen, screen by screen, and return what it says.

use super::*;

/// Walk a scrollable screen and return everything readable on it.
#[derive(Debug)]
pub struct AdbCollectTool {
    device: Option<String>,
}

impl Default for AdbCollectTool {
    fn default() -> Self {
        Self::new()
    }
}

impl AdbCollectTool {
    pub fn new() -> Self {
        Self { device: None }
    }

    pub fn with_device(mut self, device: String) -> Self {
        self.device = Some(device);
        self
    }
}

#[async_trait]
impl Tool for AdbCollectTool {
    fn name(&self) -> &str {
        "android_collect"
    }

    fn description(&self) -> &str {
        "Read everything on a long screen (a chat, a list, settings) by scrolling and merging. \
         One call replaces a read-scroll-read loop: it returns the text in screen order, with \
         the reason it stopped. Use it for 'message me everything', 'what are my orders' and \
         anything where the answer is spread over several screens. Screens the accessibility \
         tree cannot describe are read from the screenshot with OCR and merged in place, so a \
         chat drawn by hand is still collected; the result says which screens needed it. Stops \
         at 50,000 characters."
    }

    fn parameters_schema(&self) -> Value {
        create_schema(
            "Collect the readable text of a scrollable screen",
            serde_json::json!({
                "max_screens": {
                    "type": "integer",
                    "description": "How many screens to read at most (default 5, max 15)."
                },
                "direction": {
                    "type": "string",
                    "description": "down (default) reads towards older/newer content; up reverses.",
                    "enum": ["down", "up"]
                },
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
        let _target = super::lock_device(&device).await;
        let target = match Target::from_args(&args) {
            Ok(target) => target,
            Err(message) => return Ok(ToolExecutionResult::error(message)),
        };
        let max_screens = args
            .get("max_screens")
            .and_then(|v| v.as_u64())
            .unwrap_or(5)
            .clamp(1, 15) as usize;
        let direction = match args.get("direction").and_then(|v| v.as_str()) {
            Some(value) => match Direction::parse(Some(value)) {
                Ok(direction) => direction,
                Err(message) => return Ok(ToolExecutionResult::error(message)),
            },
            None => Direction::Down,
        };

        let guard_key = super::guard_key_for(context, device.as_deref(), target);
        if let Some(reason) = super::guard::Guard::global()
            .check_read(&guard_key, "android_collect")
            .refusal()
        {
            return Ok(ToolExecutionResult::error(reason.to_string()));
        }
        let mut seen = std::collections::HashSet::new();
        let mut screens: Vec<(Vec<ReadLine>, &'static str)> = Vec::new();
        let mut size: Option<(i32, i32)> = None;
        let mut stop = "reached max_screens";
        let mut cost = 0_usize;
        // The screen the last successful read left the task on. Only the last
        // one: a collect that scrolled twelve screens ended on the twelfth, and
        // that is where the task is now.
        let mut last_read: Option<u64> = None;
        // Whether any screen was read through OCR. Reported once, at the end:
        // the per-screen tag is on the lines, but the model needs to know
        // before it trusts the text that some of it was read from pixels.
        let mut used_ocr = false;

        for index in 0..max_screens {
            let dump = match dump_ui(&device, target).await {
                Ok(dump) => dump,
                Err(failure) => {
                    if screens.is_empty() {
                        return Ok(ToolExecutionResult::error(failure.message)
                            .with_data(serde_json::json!({ "code": failure.code })));
                    }
                    stop = "the tree stopped being readable";
                    break;
                }
            };
            size = screen_size(&device, dump.screen).await.or(size);
            last_read =
                (!dump.elements.is_empty()).then(|| super::tree_fingerprint(&dump.elements));

            // OCR is a helper, not a second main path: the tree is exact and
            // cheap, so it is read alone while it describes the screen. It is
            // read only when the tree cannot — a chat drawn in a custom view,
            // a canvas — where the alternative is a screen of nothing.
            let quality = assess_tree(&dump.elements);
            let ocr = if quality == TreeQuality::Usable {
                Vec::new()
            } else {
                match ocr_screen(&device, target).await {
                    Ok(lines) => lines,
                    Err(why) => {
                        if index == 0 && quality == TreeQuality::Unusable {
                            // Nothing came from either reader. Say so once,
                            // with the reason OCR had, rather than scrolling a
                            // screen it cannot read and calling the result
                            // empty.
                            return Ok(ToolExecutionResult::success(format!(
                                "android_collect found nothing readable on this screen: the \
                                 accessibility tree describes none of it and OCR could not read \
                                 it ({why}). Use android_observe to look at the screenshot \
                                 instead."
                            ))
                            .with_data(serde_json::json!({
                                "collected": 0,
                                "quality": quality.as_str(),
                                "source": "none",
                            })));
                        }
                        Vec::new()
                    }
                }
            };

            let read = merge_screen(&dump.elements, &ocr);
            if read.ocr {
                used_ocr = true;
            }
            let source = read.source();
            let fresh: Vec<ReadLine> = read
                .lines
                .into_iter()
                .filter(|line| seen.insert(line.text.clone()))
                .collect();
            if index > 0 && fresh.is_empty() {
                stop = "nothing new appeared";
                break;
            }
            for line in &fresh {
                cost += line.text.chars().count() + 1;
            }
            let source = if fresh.is_empty() { "none" } else { source };
            screens.push((fresh, source));
            if cost >= COLLECT_MAX_CHARS {
                stop = "reached the 50k-character budget";
                break;
            }

            if index + 1 >= max_screens {
                break;
            }
            let Some(size) = size else {
                stop = "the screen size is unknown, so it cannot be scrolled";
                break;
            };
            let container = scroll_container(&dump.elements, size, direction);
            if let Err(message) = scroll_once(&device, size, direction, target, container).await {
                stop = "scrolling failed";
                if screens.len() == 1 {
                    return Ok(ToolExecutionResult::error(message));
                }
                break;
            }
            let before = tree_fingerprint(&dump.elements);
            let settle = settle_after_action(&device, target, Some(before), 900).await;
            if settle.effect == EFFECT_UNCHANGED {
                stop = "the screen stopped moving";
                break;
            }
        }

        let total: usize = screens.iter().map(|(lines, _)| lines.len()).sum();
        let mut text = format!(
            "Read {total} line(s) over {} screen(s); stopped because {stop}.",
            screens.len()
        );
        if screens.len() == max_screens {
            text.push_str(" If the answer may continue, call again with a larger max_screens.");
        }
        if used_ocr {
            text.push_str(
                "\n\nSome screens were read from the screenshot with OCR rather than the \
                 accessibility tree; those lines are marked `(ocr)` and are less exact than the \
                 rest.",
            );
        }
        let mut data = serde_json::json!({
            "screens": screens
                .iter()
                .map(|(lines, source)| {
                    let rendered: Vec<serde_json::Value> = lines
                        .iter()
                        .map(|line| {
                            serde_json::json!({
                                "text": line.text,
                                "source": line.source.tag(),
                                "center": [line.center.0, line.center.1],
                            })
                        })
                        .collect();
                    serde_json::json!({ "source": source, "lines": rendered })
                })
                .collect::<Vec<_>>(),
            "stop_reason": stop,
            "source": if used_ocr { "mixed" } else { "tree" },
            "device": resolved_serial(&device).await,
        });
        if cost >= COLLECT_MAX_CHARS {
            data["truncated"] = serde_json::json!(true);
        }
        for (index, (lines, source)) in screens.iter().enumerate() {
            let name = match *source {
                "tree" => "tree".to_string(),
                "ocr" => "ocr".to_string(),
                "mixed" => "tree + OCR".to_string(),
                _ => "nothing new".to_string(),
            };
            text.push_str(&format!("\n\n── screen {} ({name}) ──", index + 1));
            for line in lines {
                text.push('\n');
                text.push_str(&line.text);
                if line.source == LineSource::Ocr {
                    text.push_str(" (ocr)");
                }
            }
        }

        // The collect is one read of wherever it ended up. Recorded with the
        // last screen's fingerprint, not the first: the task is now on the
        // screen the scrolling stopped at, and that is the one the next read
        // would be a re-read of.
        let note =
            super::guard::Guard::global().record_read(&guard_key, "android_collect", last_read);
        Ok(ToolExecutionResult::success(super::with_note(text, note)).with_data(data))
    }
}
