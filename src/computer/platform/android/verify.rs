//! `android_verify`: asking the screen a yes/no question.
use async_trait::async_trait;
use serde_json::Value;

// The set, the device targeting, and the siblings' items: this file used
// to be part of `android`, and names them exactly as it did there.
use super::*;

/// The three answers a verification can give.
///
/// `unknown` never means success: it means the check could not be made, and the
/// result says why.
pub const VERDICT_SATISFIED: &str = "satisfied";
pub const VERDICT_UNSATISFIED: &str = "unsatisfied";
pub const VERDICT_UNKNOWN: &str = "unknown";

/// What a verification is looking for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Expectation {
    /// Some element's text or content description contains this.
    TextContains(String),
    /// The element at this index is still there, still labelled this way.
    ElementAt {
        index: usize,
        description: Option<String>,
    },
}

impl Expectation {
    /// What was checked, in the caller's terms, for the result to report.
    fn describe(&self) -> String {
        match self {
            Expectation::TextContains(text) => format!("the screen shows text containing {text:?}"),
            Expectation::ElementAt {
                index,
                description: Some(label),
            } => format!("element {index} is still {label:?}"),
            Expectation::ElementAt { index, description: None } => {
                format!("element {index} is still there")
            }
        }
    }

    pub(super) fn parse(args: &Value) -> Result<Self, String> {
        if let Some(text) = args.get("text").and_then(Value::as_str) {
            let text = text.trim();
            if text.is_empty() {
                return Err("`text` was given but is empty".to_string());
            }
            return Ok(Expectation::TextContains(text.to_string()));
        }
        if let Some(index) = args.get("target").and_then(Value::as_u64) {
            return Ok(Expectation::ElementAt {
                index: index as usize,
                description: args
                    .get("target_description")
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .filter(|label| !label.is_empty())
                    .map(str::to_string),
            });
        }
        Err(
            "give one of `text` (something the screen should now show) or `target` (an element \
             index from android_ui_tree that should still be there)"
                .to_string(),
        )
    }
}

/// Decide the verdict from the elements a dump produced.
///
/// The evidence is the accessibility tree and nothing else — not a screenshot
/// comparison, and not the fact that an action was dispatched earlier. That is
/// the whole reason to ask separately.
pub(super) fn evaluate(expectation: &Expectation, elements: &[UiElement]) -> &'static str {
    let satisfied = match expectation {
        Expectation::TextContains(text) => {
            let needle = text.to_lowercase();
            elements.iter().any(|element| {
                [element.text.as_deref(), element.content_desc.as_deref()]
                    .into_iter()
                    .flatten()
                    .any(|label| label.to_lowercase().contains(&needle))
            })
        }
        // Index 0 does not exist (indices are 1-based), and an index past the
        // end is an element that is gone — both are "not satisfied", not
        // "unverifiable": the tree was read and the element is not in it.
        Expectation::ElementAt { index, description } if *index > 0 => {
            match elements.get(index - 1) {
                Some(element) => description.as_deref().is_none_or(|wanted| {
                    let label = element
                        .text
                        .as_deref()
                        .or(element.content_desc.as_deref())
                        .unwrap_or_default();
                    label_matches(label, wanted)
                }),
                None => false,
            }
        }
        Expectation::ElementAt { .. } => false,
    };

    if satisfied {
        VERDICT_SATISFIED
    } else {
        VERDICT_UNSATISFIED
    }
}

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
        let expectation = match Expectation::parse(&args) {
            Ok(expectation) => expectation,
            Err(refusal) => return Ok(ToolExecutionResult::error(refusal)),
        };
        let checked = expectation.describe();

        let target = match Target::from_args(&args) {
            Ok(target) => target,
            Err(message) => return Ok(ToolExecutionResult::error(message)),
        };
        match dump_ui_elements(&device, target).await {
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
