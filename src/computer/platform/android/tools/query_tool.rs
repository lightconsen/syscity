//! Find one control without reading the whole tree.

use super::*;

/// Find controls on the current screen without reading the whole tree.
#[derive(Debug)]
pub struct AdbQueryTool {
    device: Option<String>,
}

impl Default for AdbQueryTool {
    fn default() -> Self {
        Self::new()
    }
}

impl AdbQueryTool {
    pub fn new() -> Self {
        Self { device: None }
    }

    pub fn with_device(mut self, device: String) -> Self {
        self.device = Some(device);
        self
    }
}

/// How many matches a query returns by default.
const QUERY_DEFAULT_LIMIT: usize = 30;
/// The most a query will return.
const QUERY_MAX_LIMIT: usize = 100;

/// Longest label quoted back in a query's text answer.
const QUERY_LABEL_CHARS: usize = 80;

#[async_trait]
impl Tool for AdbQueryTool {
    fn name(&self) -> &str {
        "android_query"
    }

    fn description(&self) -> &str {
        "Find elements on the current screen by text, resource id or class, without reading the \
         whole UI tree into the conversation. Every criterion is a case-insensitive substring and \
         they are combined, so `text: \"alice\"` and `class: \"TextView\"` narrows a name to the \
         row that draws it. Returns each match with its index, label, class and centre, plus how \
         many matched in total. Use it to check whether something is on screen, to count the rows \
         of a list, or to get the coordinates of one control — it is much cheaper than \
         android_ui_tree when you know what you are looking for."
    }

    fn parameters_schema(&self) -> Value {
        create_schema(
            "Find elements on the current screen",
            serde_json::json!({
                "text": {
                    "type": "string",
                    "description": "Substring of the element's text or content description (case-insensitive)."
                },
                "resource_id": {
                    "type": "string",
                    "description": "Substring of the element's Android resource id, e.g. 'id/send'."
                },
                "class": {
                    "type": "string",
                    "description": "Substring of the element's class, e.g. 'Button' or 'TextView'."
                },
                "limit": {
                    "type": "integer",
                    "description": "How many matches to return (default 30, max 100). The total is reported either way."
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
        _context: &ToolContext,
    ) -> crate::Result<ToolExecutionResult> {
        let device = match resolve_device(&args, self.device.as_deref()) {
            Ok(device) => device,
            Err(refusal) => return Ok(ToolExecutionResult::error(refusal)),
        };
        let target = match Target::from_args(&args) {
            Ok(target) => target,
            Err(message) => return Ok(ToolExecutionResult::error(message)),
        };
        let text = args.get("text").and_then(|v| v.as_str());
        let resource_id = args.get("resource_id").and_then(|v| v.as_str());
        let class = args.get("class").and_then(|v| v.as_str());
        let Some(query) = super::query::Query::new(text, resource_id, class) else {
            return Ok(ToolExecutionResult::error(
                "give at least one of `text`, `resource_id` or `class` — with none of them every \
                 element matches and the answer would be the whole screen",
            ));
        };
        let limit = args
            .get("limit")
            .and_then(|v| v.as_u64())
            .map(|n| n.clamp(1, QUERY_MAX_LIMIT as u64) as usize)
            .unwrap_or(QUERY_DEFAULT_LIMIT);

        let dump = match dump_ui(&device, target).await {
            Ok(dump) => dump,
            Err(failure) => {
                return Ok(ToolExecutionResult::error(failure.message)
                    .with_data(serde_json::json!({ "code": failure.code })))
            }
        };
        let total = query.count(&dump.elements);
        let found = query.find(&dump.elements, limit);
        // Two different truncations, and a caller has to tell them apart:
        // `matched` means more matched than `limit` returned, `truncated` means
        // the *tree itself* was a prefix of the screen, so `total` is a lower
        // bound and may not even contain the element being looked for.
        let matched_more = total > found.len();

        let text_out = if total == 0 {
            format!(
                "No element on this screen matches. The screen has {} element(s); read it with \
                 android_ui_tree if the label may be worded differently.",
                dump.elements.len()
            )
        } else {
            let mut lines = vec![format!(
                "{total} match(es){}:",
                if matched_more {
                    format!(", the first {}", found.len())
                } else {
                    String::new()
                }
            )];
            for element in &found {
                let label = element_label(element).unwrap_or("-");
                let label: String = label.chars().take(QUERY_LABEL_CHARS).collect();
                lines.push(format!(
                    "[{}] {} {:?} center=({},{}){}",
                    element.index,
                    element.class,
                    label,
                    element.center.0,
                    element.center.1,
                    if element.clickable { " clickable" } else { "" }
                ));
            }
            if dump.truncated {
                lines.push(
                    "The tree was truncated (the screen has more nodes than one read carries), \
                     so this count may be short and a match may be missing."
                        .to_string(),
                );
            }
            lines.join("\n")
        };

        Ok(ToolExecutionResult::success(text_out).with_data(serde_json::json!({
            "total": total,
            "returned": found.len(),
            // More matched than were returned — the caller narrowed with
            // `limit`, not with the query.
            "matched_more": matched_more,
            // The source capped the tree, so the screen itself was not fully
            // seen. Ambiguity detection has to treat this as "unknown", not as
            // "exactly one".
            "truncated": dump.truncated,
            "matches": found,
            "source": dump.source,
            "device": resolved_serial(&device).await,
        })))
    }
}
