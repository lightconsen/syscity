//! Screen-state tools — let the LLM capture the full screen state
//! (screenshot + UI tree + optional OCR) or run OCR on demand.
//!
//! - [`ScreenStateTool`] (`screen_state`): unified snapshot, the antidote to
//!   "blind operation" — the LLM sees structure *and* pixels before acting.
//! - [`ScreenOcrTool`] (`screen_ocr`): on-demand OCR of the full screen or a
//!   region, for text the accessibility tree cannot see (PDFs, dialogs,
//!   image-based UIs).
//! - [`ScreenUiDetectTool`] (`screen_ui_detect`): on-demand UI element
//!   detection (buttons, text fields, checkboxes, icons) via ONNX (OmniParser),
//!   for when the accessibility tree is empty or incomplete.

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{json, Value};
use tracing::warn;

use crate::computer::sensitive_ui::SensitiveUiDetector;
use crate::computer::vision::ScreenState;
#[cfg(feature = "vision")]
use crate::computer::Rect;
use crate::computer::{ComputerAdapter, CoordinateSpace, UiElement};
use crate::tools::{
    approval::RiskLevel, create_schema, sdk::ToolCapabilities, Tool, ToolContext,
    ToolExecutionResult,
};

/// Lazily-initialized shared OCR engine (model loading is expensive, so it
/// only happens on first use).
#[cfg(feature = "vision")]
pub type SharedOcr = Arc<tokio::sync::Mutex<Option<crate::computer::vision::ocr_rapid::RapidOcr>>>;

#[cfg(feature = "vision")]
pub fn new_shared_ocr() -> SharedOcr {
    Arc::new(tokio::sync::Mutex::new(None))
}

/// Lock the shared OCR engine, initializing it on first use. The returned
/// guard may be held across `.await` (it is a `tokio::sync::Mutex`) and also
/// serializes concurrent OCR calls.
#[cfg(feature = "vision")]
async fn lock_ocr(
    shared: &SharedOcr,
) -> crate::Result<tokio::sync::MutexGuard<'_, Option<crate::computer::vision::ocr_rapid::RapidOcr>>>
{
    let mut guard = shared.lock().await;
    if guard.is_none() {
        *guard = Some(
            crate::computer::vision::ocr_rapid::RapidOcr::new_auto()
                .await
                .map_err(|e| {
                    crate::error::SyscityError::Internal(format!("OCR init failed: {}", e))
                })?,
        );
    }
    Ok(guard)
}

/// Lazily-initialized shared vision UI-element detector (OmniParser). Model
/// loading is expensive, so it only happens on first use.
#[cfg(feature = "vision")]
pub type SharedUiDetector =
    Arc<tokio::sync::Mutex<Option<crate::computer::vision::ui_onnx::OmniParserDetector>>>;

#[cfg(feature = "vision")]
pub fn new_shared_ui_detector() -> SharedUiDetector {
    Arc::new(tokio::sync::Mutex::new(None))
}

/// Lock the shared UI-element detector, initializing it on first use. The
/// returned guard may be held across `.await` (it is a `tokio::sync::Mutex`)
/// and also serializes concurrent detection calls.
#[cfg(feature = "vision")]
async fn lock_ui_detector(
    shared: &SharedUiDetector,
) -> crate::Result<
    tokio::sync::MutexGuard<'_, Option<crate::computer::vision::ui_onnx::OmniParserDetector>>,
> {
    let mut guard = shared.lock().await;
    if guard.is_none() {
        *guard = Some(
            crate::computer::vision::ui_onnx::OmniParserDetector::new_auto()
                .await
                .map_err(|e| {
                    crate::error::SyscityError::Internal(format!("UI detector init failed: {}", e))
                })?,
        );
    }
    Ok(guard)
}

// ── screen_state ───────────────────────────────────────────────────────────

/// Tool that captures a unified [`ScreenState`] for the LLM.
pub struct ScreenStateTool {
    adapter: Option<Arc<dyn ComputerAdapter>>,
    #[cfg(feature = "vision")]
    ocr: SharedOcr,
    #[cfg(feature = "vision")]
    ui_detector: SharedUiDetector,
}

impl ScreenStateTool {
    pub fn new(
        adapter: Option<Arc<dyn ComputerAdapter>>,
        #[cfg(feature = "vision")] ocr: SharedOcr,
        #[cfg(feature = "vision")] ui_detector: SharedUiDetector,
    ) -> Self {
        Self {
            adapter,
            #[cfg(feature = "vision")]
            ocr,
            #[cfg(feature = "vision")]
            ui_detector,
        }
    }
}

#[async_trait]
impl Tool for ScreenStateTool {
    fn name(&self) -> &str {
        "screen_state"
    }

    fn description(&self) -> &str {
        r#"Capture the current screen state: a screenshot plus the accessibility UI tree (windows, buttons, text fields in hierarchy), with optional OCR text.

Use this BEFORE desktop actions to see what is on screen instead of operating blind. The UI tree gives structured element positions; OCR reads text the tree cannot see (PDFs, image-based UIs). OCR is slow (seconds) — only enable it when the tree is insufficient.

When the accessibility tree is empty (games, image-based UIs, remote desktops, webviews), interactive elements are auto-detected from the screenshot via OmniParser; disable with ui_fallback=false.

The result states the image's pixel→screen-point mapping (`coordinate_space`) and every display's frame. Read positions off the image, then pass them to `computer` with `from_screenshot=true`, or target elements by their `[id=…]`."#
    }

    fn parameters_schema(&self) -> Value {
        create_schema(
            "Capture screen state (screenshot + UI tree + optional OCR)",
            json!({
                "include_ocr": {
                    "type": "boolean",
                    "description": "Also run OCR to extract visible text (slow, seconds). Default false."
                },
                "ui_fallback": {
                    "type": "boolean",
                    "description": "Run OmniParser UI detection when the accessibility tree is empty (slow, downloads model on first use). Default true."
                },
                "max_tree_lines": {
                    "type": "integer",
                    "description": "Maximum UI-tree outline lines to return (default 100)"
                }
            }),
            Vec::<String>::new(),
        )
    }

    fn capabilities(&self) -> ToolCapabilities {
        ToolCapabilities {
            read_only: true,
            requires_approval: false,
            risk_level: RiskLevel::Low,
            categories: vec!["computer".to_string(), "desktop".to_string()],
            // The screen it reads is the one its own earlier calls changed, so
            // the same request twice is two observations, not one repeated.
            deduplicate_identical_calls: false,
            ..Default::default()
        }
    }

    fn is_available(&self, _context: &ToolContext) -> bool {
        self.adapter.is_some()
    }

    async fn execute(
        &self,
        args: Value,
        context: &ToolContext,
    ) -> crate::Result<ToolExecutionResult> {
        // A capture is bounded by the call's own ceiling, less a margin, so a
        // tree walk that will not finish says so in the terms of this call
        // instead of being killed from above as "tool timed out".
        let budget = crate::computer::StepBudget::new(screen_state_budget_for(context.timeout()));
        self.screen_state(args, &budget).await
    }
}

/// How much of the tool call's timeout a capture may spend.
///
/// Same rule as the computer tool's step: the timeout minus a tenth, capped
/// at five seconds, so the result lands before the registry kills the call.
fn screen_state_budget_for(timeout: std::time::Duration) -> std::time::Duration {
    let margin = (timeout / 10).min(std::time::Duration::from_secs(5));
    timeout.saturating_sub(margin)
}

impl ScreenStateTool {
    /// Assemble the screen-state answer. Separated from `execute` so the
    /// completeness rule can be exercised without a live adapter.
    async fn screen_state(
        &self,
        args: Value,
        budget: &crate::computer::StepBudget,
    ) -> crate::Result<ToolExecutionResult> {
        let adapter = self.adapter.as_ref().ok_or_else(|| {
            crate::error::SyscityError::Unsupported(
                "Computer adapter is not configured".to_string(),
            )
        })?;

        let max_lines = args["max_tree_lines"].as_u64().unwrap_or(100) as usize;
        let want_ocr = args["include_ocr"].as_bool().unwrap_or(false);

        #[allow(unused_mut)]
        let mut state = crate::computer::budget::within(
            budget,
            "the screen capture",
            ScreenState::capture_light(adapter.as_ref()),
        )
        .await
        .map_err(|e| crate::error::SyscityError::Internal(e.to_string()))?
        .map_err(|e| crate::error::SyscityError::Internal(e.to_string()))?;

        #[cfg(feature = "vision")]
        if want_ocr {
            let mut guard = lock_ocr(&self.ocr).await?;
            let ocr = guard.as_mut().ok_or_else(|| {
                crate::error::SyscityError::Internal("OCR engine unavailable".to_string())
            })?;
            let blocks = ocr.detect_text(&state.screenshot).await.unwrap_or_default();
            drop(guard);
            state.ocr_text = blocks
                .iter()
                .map(|b| b.text.as_str())
                .collect::<Vec<_>>()
                .join("\n");
            state.ocr_regions = blocks
                .iter()
                .map(crate::computer::vision::TextBlockSer::from)
                .collect();
        }

        #[cfg(not(feature = "vision"))]
        if want_ocr {
            state.ocr_text = "(OCR unavailable: built without 'vision' feature)".to_string();
        }

        // OmniParser UI-detection fallback: when the accessibility tree is empty,
        // detect interactive elements from the screenshot so the LLM is not blind.
        // Deliberately *not* in ScreenState::capture_light — that path feeds the
        // cheap verification loop (~300ms) and must never run slow vision inference.
        let want_fallback = args["ui_fallback"].as_bool().unwrap_or(true);
        let mut ui_source = "accessibility";
        let mut fallback_count = 0usize;

        #[cfg(feature = "vision")]
        if want_fallback && state.ui_tree.is_empty() {
            match lock_ui_detector(&self.ui_detector).await {
                Ok(mut guard) => {
                    if let Some(detector) = guard.as_mut() {
                        match detector.detect_elements(&state.screenshot).await {
                            Ok(detected) if !detected.is_empty() => {
                                fallback_count = detected.len();
                                state.ui_tree =
                                    crate::computer::vision::ui_onnx::OmniParserDetector::to_ui_elements(
                                        detected,
                                    );
                                ui_source = "omniparser";
                            }
                            Ok(_) => warn!("OmniParser fallback found no elements"),
                            Err(e) => warn!("OmniParser fallback inference failed: {}", e),
                        }
                    }
                }
                Err(e) => warn!("OmniParser fallback init failed: {}", e),
            }
        }

        // Coordinate contract: say how the image the model is looking at maps
        // back to the points that clicks use. On a Retina display this is the
        // difference between a click landing on the button and 900 points away.
        let geometry = adapter.display_geometry().await.unwrap_or_default();
        let coordinate_space = CoordinateSpace::from_screenshot(&state.screenshot, &geometry);

        // Sensitive-UI scan runs over the tree the model is about to see, so
        // the warning is attached to the exact lines it applies to. Keyed by
        // element id; elements without a snapshot id cannot be targeted by id
        // anyway, so they are only counted in the header.
        let detector = SensitiveUiDetector::new();
        let mut findings = Vec::new();
        for root in &state.ui_tree {
            findings.extend(detector.scan_tree(root));
        }
        let mut sensitive: HashMap<String, &'static str> = HashMap::new();
        for finding in &findings {
            if !finding.element_id.is_empty() {
                sensitive
                    .entry(finding.element_id.clone())
                    .or_insert(finding.category.as_str());
            }
        }

        let mut outline = TreeOutline::default();
        outline.push_tree(
            &state.ui_tree,
            ui_source,
            max_lines,
            want_fallback,
            fallback_count,
            &sensitive,
        );
        if let Some(note) = outline.incomplete_note() {
            outline.text.push_str(note);
        }
        let mut output = outline.text;
        output.push_str(&trust_line(ui_source));
        if !findings.is_empty() {
            output.push_str(&format!(
                "(⚠ sensitive UI detected: {}) — pause and confirm before typing or clicking here\n",
                findings
                    .iter()
                    .map(|f| format!("{} ({})", f.category.as_str(), f.description))
                    .collect::<Vec<_>>()
                    .join("; ")
            ));
        }
        match coordinate_space {
            Some(space) => {
                output.push_str(&format!(
                    "\nCoordinates: {}\n(click with from_screenshot=true to use those image pixels; \
                     x/y without it are screen points. Element ids need no conversion.)\n",
                    space.summary()
                ));
                if geometry.displays.len() > 1 {
                    output.push_str(&format!("Displays: {}\n", geometry.summary()));
                }
            }
            None => output.push_str(
                "\nCoordinates: display geometry unavailable — screenshot pixels cannot be \
                 converted to screen points; target elements by id instead.\n",
            ),
        }

        if !state.ocr_text.is_empty() {
            output.push_str("\nOCR text:\n");
            output.push_str(&state.ocr_text);
        }

        let mut data = json!({
            "screenshot_width": state.screenshot.width,
            "screenshot_height": state.screenshot.height,
            "display": serde_json::to_value(&geometry).unwrap_or_default(),
            "coordinate_space": coordinate_space
                .map(|space| serde_json::to_value(space).unwrap_or_default())
                .unwrap_or(Value::Null),
            "ui_tree": serde_json::to_value(&state.ui_tree).unwrap_or_default(),
            "ui_source": ui_source,
            "ui_trust": if ui_source == "omniparser" {
                "inferred"
            } else {
                "authoritative"
            },
            "sensitive_ui": serde_json::to_value(&findings).unwrap_or_default(),
            // Machine-readable, so a caller does not have to parse the prose.
            // `not_found` from a complete tree means the element is not there;
            // from an incomplete one it means "not where this read looked".
            "ui_tree_complete": !outline.incomplete,
            "ocr_text": state.ocr_text,
            "ocr_regions": serde_json::to_value(&state.ocr_regions).unwrap_or_default(),
        });
        if state.screenshot.base64.is_empty() {
            // Adapters may deliver a file-only screenshot; nothing to store,
            // keep the legacy (empty) field for compatibility.
            data["screenshot_base64"] = json!(state.screenshot.base64);
        } else {
            match crate::attachments::store_base64_image_async(
                &state.screenshot.base64,
                "image/png",
            )
            .await
            {
                Ok(aref) => {
                    data["screenshot_ref"] = aref.to_json();
                    output.push_str(&format!(
                        "\nScreenshot: {}x{} {} (attachment {}, {} bytes)\n{}",
                        state.screenshot.width,
                        state.screenshot.height,
                        aref.mime,
                        crate::attachments::short_id(&aref.digest),
                        aref.size,
                        crate::attachments::render_ref_line(&aref),
                    ));
                }
                Err(e) => {
                    warn!("attachment store write failed ({}); falling back to inline base64", e);
                    data["screenshot_base64"] = json!(state.screenshot.base64);
                }
            }
        }

        Ok(ToolExecutionResult::success(output).with_data(data))
    }
}

/// The rendered tree section of a `screen_state` result.
///
/// Kept apart from the adapter so the two ways a tree can be short — this
/// renderer's line cap and a read that stopped early — can be tested without
/// a live desktop. They mean different things to the model: the first is
/// fixed by asking for fewer elements, the second only by re-reading deeper.
#[derive(Debug, Default)]
struct TreeOutline {
    /// The rendered outline, header included.
    text: String,
    /// The line cap stopped the render.
    cut_by_cap: bool,
    /// The accessibility read itself stopped early.
    incomplete: bool,
}

impl TreeOutline {
    fn push_tree(
        &mut self,
        roots: &[UiElement],
        ui_source: &str,
        max_lines: usize,
        want_fallback: bool,
        fallback_count: usize,
        sensitive: &HashMap<String, &'static str>,
    ) {
        self.text.push_str("UI tree:\n");
        let mut lines = 0usize;
        for root in roots {
            format_element(root, 0, &mut self.text, &mut lines, max_lines, sensitive);
        }
        if roots.is_empty() {
            self.text
                .push_str("(empty — no accessibility tree available)\n");
            if cfg!(not(feature = "vision")) && want_fallback {
                self.text
                    .push_str("(UI detection unavailable: built without 'vision' feature)\n");
            }
            return;
        }
        if ui_source == "omniparser" {
            self.text.push_str(&format!(
                "(accessibility tree empty — {fallback_count} UI elements detected via OmniParser)\n"
            ));
        }
        // The renderer's own cap. Saying so is what lets the model reason
        // about a list it asked to be short.
        self.cut_by_cap = lines >= max_lines;
        if self.cut_by_cap {
            self.text.push_str("… (tree truncated)\n");
        }
        self.incomplete = !UiElement::tree_is_complete(roots);
    }

    /// The sentence to append when the read — not the renderer — stopped
    /// short, and `None` when the tree is whole.
    fn incomplete_note(&self) -> Option<&'static str> {
        self.incomplete.then_some(
            "(⚠ tree incomplete — the accessibility read stopped at its depth or element \
             limit; an element you cannot see may still exist)\n",
        )
    }
}

/// How far the model can trust the tree it was shown.
fn trust_line(ui_source: &str) -> String {
    format!(
        "(tree source: {}; {})\n",
        ui_source,
        match ui_source {
            "omniparser" => "inferred from pixels — treat roles/names as guesses, verify visually",
            _ => "authoritative — element ids address real accessibility elements",
        }
    )
}

/// Render a UI element and its children as an indented outline.
/// Returns `false` when the line cap was hit (tree truncated).
fn format_element(
    el: &UiElement,
    depth: usize,
    out: &mut String,
    lines: &mut usize,
    max_lines: usize,
    sensitive: &HashMap<String, &'static str>,
) {
    if *lines >= max_lines {
        return;
    }
    *lines += 1;
    let indent = "  ".repeat(depth);
    // Omit the quoted label entirely when it is empty (e.g. OmniParser-detected
    // elements carry no accessibility label), instead of rendering `""`.
    let label = el.label.as_deref().filter(|l| !l.is_empty());
    let label_part = label.map(|l| format!(" {:?}", l)).unwrap_or_default();
    // Only a positive "disabled" is worth a mark. An unread state stays
    // silent rather than being drawn as enabled — the model should not build
    // confidence on an answer nobody gave.
    let state = if el.enabled.is_false() {
        " [disabled]"
    } else {
        ""
    };
    // Focus is the same three-state fact as `enabled`, and it is the one that
    // decides where a bare `type` or `key_press` lands. An unread focus stays
    // unmarked, like an unread enabled state.
    let focus_part = if el.focused.known() == Some(true) {
        " [focused]"
    } else {
        ""
    };
    // The id is what lets the model act on this exact element later: it is
    // snapshot-bound, and `computer` resolves it or reports it as stale.
    let id_part = if el.id.is_empty() {
        String::new()
    } else {
        format!(" [id={}]", el.id)
    };
    // Inline the reason the model must slow down on *this* line, rather than
    // leaving it to a header it may skim past.
    let sensitive_part = sensitive
        .get(&el.id)
        .map(|kind| format!(" [sensitive: {kind}]"))
        .unwrap_or_default();
    // An element whose geometry the read did not publish has no point to
    // click. Saying nothing is honest; printing `(0,0)` would invite a click
    // at the top-left corner of the screen.
    let geometry_part = el
        .bounds
        .map(|b| format!(" at ({},{}) {}x{}", b.x, b.y, b.width, b.height))
        .unwrap_or_default();
    out.push_str(&format!(
        "{}{}{}{}{}{}{}{}\n",
        indent, el.role, label_part, state, focus_part, geometry_part, id_part, sensitive_part
    ));
    for child in &el.children {
        format_element(child, depth + 1, out, lines, max_lines, sensitive);
    }
}

// ── screen_ocr ─────────────────────────────────────────────────────────────

/// Tool that runs OCR on the full screen or a region.
pub struct ScreenOcrTool {
    adapter: Option<Arc<dyn ComputerAdapter>>,
    #[cfg(feature = "vision")]
    ocr: SharedOcr,
}

impl ScreenOcrTool {
    pub fn new(
        adapter: Option<Arc<dyn ComputerAdapter>>,
        #[cfg(feature = "vision")] ocr: SharedOcr,
    ) -> Self {
        Self {
            adapter,
            #[cfg(feature = "vision")]
            ocr,
        }
    }
}

#[async_trait]
impl Tool for ScreenOcrTool {
    fn name(&self) -> &str {
        "screen_ocr"
    }

    fn description(&self) -> &str {
        r#"Extract text from the screen using OCR (RapidOCR). Returns text blocks with positions and confidence scores.

Use when the accessibility tree lacks the text you need: PDF viewers, dialogs, image-based UIs, games. Optionally restrict to a screen region to speed up detection and improve accuracy."#
    }

    fn parameters_schema(&self) -> Value {
        create_schema(
            "OCR the screen or a region",
            json!({
                "region_x": { "type": "integer", "description": "Region left coordinate (omit for full screen)" },
                "region_y": { "type": "integer", "description": "Region top coordinate" },
                "region_width": { "type": "integer", "description": "Region width" },
                "region_height": { "type": "integer", "description": "Region height" }
            }),
            Vec::<String>::new(),
        )
    }

    fn capabilities(&self) -> ToolCapabilities {
        ToolCapabilities {
            read_only: true,
            requires_approval: false,
            risk_level: RiskLevel::Low,
            categories: vec!["computer".to_string(), "desktop".to_string()],
            ..Default::default()
        }
    }

    fn is_available(&self, _context: &ToolContext) -> bool {
        self.adapter.is_some() && cfg!(feature = "vision")
    }

    async fn execute(
        &self,
        args: Value,
        _context: &ToolContext,
    ) -> crate::Result<ToolExecutionResult> {
        #[cfg(not(feature = "vision"))]
        {
            let _ = args;
            return Ok(ToolExecutionResult::error(
                "screen_ocr requires the 'vision' feature".to_string(),
            ));
        }

        #[cfg(feature = "vision")]
        {
            let adapter = self.adapter.as_ref().ok_or_else(|| {
                crate::error::SyscityError::Unsupported(
                    "Computer adapter is not configured".to_string(),
                )
            })?;

            let region = parse_region(&args);
            let screenshot = adapter
                .screenshot(region)
                .await
                .map_err(|e| crate::error::SyscityError::Internal(e.to_string()))?;

            let mut guard = lock_ocr(&self.ocr).await?;
            let ocr = guard.as_mut().ok_or_else(|| {
                crate::error::SyscityError::Internal("OCR engine unavailable".to_string())
            })?;
            let blocks = ocr
                .detect_text(&screenshot)
                .await
                .map_err(|e| crate::error::SyscityError::Internal(e.to_string()))?;
            drop(guard);

            // Offset block bounds back to screen coordinates for region OCR.
            let (ox, oy) = region.map(|r| (r.x, r.y)).unwrap_or((0, 0));
            let regions: Vec<Value> = blocks
                .iter()
                .map(|b| {
                    json!({
                        "text": b.text,
                        "confidence": b.confidence,
                        "x": b.bounds.x + ox,
                        "y": b.bounds.y + oy,
                        "width": b.bounds.width,
                        "height": b.bounds.height,
                    })
                })
                .collect();

            let text = blocks
                .iter()
                .map(|b| b.text.as_str())
                .collect::<Vec<_>>()
                .join("\n");

            let output = if text.is_empty() {
                "No text detected.".to_string()
            } else {
                text.clone()
            };

            Ok(ToolExecutionResult::success(output).with_data(json!({
                "text": text,
                "regions": regions,
            })))
        }
    }
}

// ── screen_ui_detect ───────────────────────────────────────────────────────

/// Tool that detects interactive UI elements (buttons, text fields, checkboxes,
/// icons, links) from a screenshot via ONNX (OmniParser), as a fallback when
/// the accessibility tree is empty or incomplete.
pub struct ScreenUiDetectTool {
    adapter: Option<Arc<dyn ComputerAdapter>>,
    #[cfg(feature = "vision")]
    detector: SharedUiDetector,
}

impl ScreenUiDetectTool {
    pub fn new(
        adapter: Option<Arc<dyn ComputerAdapter>>,
        #[cfg(feature = "vision")] detector: SharedUiDetector,
    ) -> Self {
        Self {
            adapter,
            #[cfg(feature = "vision")]
            detector,
        }
    }
}

#[async_trait]
impl Tool for ScreenUiDetectTool {
    fn name(&self) -> &str {
        "screen_ui_detect"
    }

    fn description(&self) -> &str {
        r#"Detect interactive UI elements (buttons, text fields, checkboxes, icons, links) from a screenshot using ONNX (OmniParser). Returns each element's role, screen bounds, and confidence.

Use when the accessibility tree is empty or incomplete: games, image-based UIs, remote desktops, webviews without accessibility support."#
    }

    fn parameters_schema(&self) -> Value {
        create_schema("Detect UI elements from the screen", json!({}), Vec::<String>::new())
    }

    fn capabilities(&self) -> ToolCapabilities {
        ToolCapabilities {
            read_only: true,
            requires_approval: false,
            risk_level: RiskLevel::Low,
            categories: vec!["computer".to_string(), "desktop".to_string()],
            ..Default::default()
        }
    }

    fn is_available(&self, _context: &ToolContext) -> bool {
        self.adapter.is_some() && cfg!(feature = "vision")
    }

    async fn execute(
        &self,
        _args: Value,
        _context: &ToolContext,
    ) -> crate::Result<ToolExecutionResult> {
        #[cfg(not(feature = "vision"))]
        {
            return Ok(ToolExecutionResult::error(
                "screen_ui_detect requires the 'vision' feature".to_string(),
            ));
        }

        #[cfg(feature = "vision")]
        {
            let adapter = self.adapter.as_ref().ok_or_else(|| {
                crate::error::SyscityError::Unsupported(
                    "Computer adapter is not configured".to_string(),
                )
            })?;

            let screenshot = adapter
                .screenshot(None)
                .await
                .map_err(|e| crate::error::SyscityError::Internal(e.to_string()))?;

            let mut guard = lock_ui_detector(&self.detector).await?;
            let detector = guard.as_mut().ok_or_else(|| {
                crate::error::SyscityError::Internal("UI detector unavailable".to_string())
            })?;
            let detected = detector
                .detect_elements(&screenshot)
                .await
                .map_err(|e| crate::error::SyscityError::Internal(e.to_string()))?;
            drop(guard);

            let elements: Vec<Value> = detected
                .iter()
                .map(|d| {
                    json!({
                        "role": d.role,
                        "x": d.bounds.x,
                        "y": d.bounds.y,
                        "width": d.bounds.width,
                        "height": d.bounds.height,
                        "confidence": d.confidence,
                    })
                })
                .collect();

            let output = if elements.is_empty() {
                "No UI elements detected.".to_string()
            } else {
                let lines = detected
                    .iter()
                    .map(|d| {
                        format!(
                            "{} at ({}, {}) {}x{} (conf {:.2})",
                            d.role,
                            d.bounds.x,
                            d.bounds.y,
                            d.bounds.width,
                            d.bounds.height,
                            d.confidence
                        )
                    })
                    .collect::<Vec<_>>()
                    .join("\n");
                format!("Detected {} UI elements:\n{}", elements.len(), lines)
            };

            Ok(ToolExecutionResult::success(output).with_data(json!({
                "elements": elements,
            })))
        }
    }
}

/// Parse an optional region from tool args.
#[cfg(feature = "vision")]
fn parse_region(args: &Value) -> Option<Rect> {
    let x = args["region_x"].as_i64()?;
    let y = args["region_y"].as_i64()?;
    let w = args["region_width"].as_u64()?;
    let h = args["region_height"].as_u64()?;
    Some(Rect::new(x as i32, y as i32, w as u32, h as u32))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::computer::types::Rect;
    use crate::computer::Tri;

    fn no_sensitive() -> HashMap<String, &'static str> {
        HashMap::new()
    }

    fn el(role: &str, label: Option<&str>, children: Vec<UiElement>) -> UiElement {
        UiElement {
            id: String::new(),
            role: role.to_string(),
            label: label.map(String::from),
            value: None,
            bounds: Some(Rect::new(10, 20, 100, 30)),
            enabled: Tri::Known(true),
            focused: Tri::Unknown,
            actions: None,
            children,
            children_count: None,
            truncated: false,
        }
    }

    #[test]
    fn format_element_renders_indented_outline() {
        let tree = el("window", Some("App"), vec![el("button", Some("OK"), vec![])]);
        let mut out = String::new();
        let mut lines = 0;
        format_element(&tree, 0, &mut out, &mut lines, 100, &no_sensitive());
        assert!(out.contains("window \"App\" at (10,20) 100x30"));
        assert!(out.contains("  button \"OK\""));
        assert_eq!(lines, 2);
    }

    /// Focus is drawn only when the read established it; an unread focus is
    /// not the same answer as "not focused".
    #[test]
    fn format_element_marks_the_focused_field() {
        let mut tree = el("text field", Some("Search"), vec![]);
        tree.focused = Tri::Known(true);
        let mut out = String::new();
        let mut lines = 0;
        format_element(&tree, 0, &mut out, &mut lines, 100, &no_sensitive());
        assert!(out.contains("[focused]"), "{out}");

        let mut unknown = el("text field", Some("Search"), vec![]);
        unknown.focused = Tri::Unknown;
        let mut out = String::new();
        format_element(&unknown, 0, &mut out, &mut lines, 100, &no_sensitive());
        assert!(!out.contains("[focused]"), "{out}");

        let mut unfocused = el("text field", Some("Search"), vec![]);
        unfocused.focused = Tri::Known(false);
        let mut out = String::new();
        format_element(&unfocused, 0, &mut out, &mut lines, 100, &no_sensitive());
        assert!(!out.contains("[focused]"), "{out}");
    }

    #[test]
    fn format_element_appends_the_element_id() {
        let mut tree = el("button", Some("Save"), vec![]);
        tree.id = "s0000002a:0.1".to_string();
        let mut out = String::new();
        let mut lines = 0;
        format_element(&tree, 0, &mut out, &mut lines, 100, &no_sensitive());
        assert!(out.contains("[id=s0000002a:0.1]"), "{out}");
    }

    #[test]
    fn format_element_marks_sensitive_elements() {
        let mut tree = el("text_field", Some("Password"), vec![]);
        tree.id = "s00000003:0.2".to_string();
        let mut sensitive = HashMap::new();
        sensitive.insert("s00000003:0.2".to_string(), "password_field");
        let mut out = String::new();
        let mut lines = 0;
        format_element(&tree, 0, &mut out, &mut lines, 100, &sensitive);
        assert!(out.contains("[sensitive: password_field]"), "{out}");
    }

    #[test]
    fn format_element_omits_empty_label() {
        // OmniParser-detected elements carry no label — render without quotes.
        let tree = el("button", None, vec![]);
        let mut out = String::new();
        let mut lines = 0;
        format_element(&tree, 0, &mut out, &mut lines, 100, &no_sensitive());
        assert_eq!(out, "button at (10,20) 100x30\n");
        assert!(!out.contains("\"\""));
    }

    #[test]
    fn format_element_truncates_at_max_lines() {
        let children = (0..10)
            .map(|i| el("button", Some(Box::leak(i.to_string().into_boxed_str())), vec![]))
            .collect();
        let tree = el("window", Some("App"), children);
        let mut out = String::new();
        let mut lines = 0;
        format_element(&tree, 0, &mut out, &mut lines, 5, &no_sensitive());
        assert_eq!(lines, 5);
        // Caller appends the truncation notice when the cap is hit.
        if lines >= 5 {
            out.push_str("… (tree truncated)\n");
        }
        assert!(out.contains("truncated"));
    }

    #[cfg(feature = "vision")]
    #[test]
    fn parse_region_requires_all_fields() {
        assert!(parse_region(&json!({})).is_none());
        assert!(parse_region(&json!({"region_x": 1})).is_none());
        let r = parse_region(&json!({
            "region_x": 10, "region_y": 20, "region_width": 300, "region_height": 200
        }))
        .unwrap();
        assert_eq!((r.x, r.y, r.width, r.height), (10, 20, 300, 200));
    }

    /// The renderer's line cap and a partial read are different facts, and
    /// the model is told which one it is looking at.
    #[test]
    fn a_cap_truncation_and_a_partial_read_are_distinguished() {
        let whole = el("window", Some("App"), vec![el("button", Some("OK"), vec![])]);
        let mut outline = TreeOutline::default();
        outline.push_tree(&[whole], "accessibility", 100, true, 0, &no_sensitive());
        assert!(!outline.cut_by_cap);
        assert!(outline.incomplete_note().is_none());
        assert!(!outline.text.contains("truncated"));

        // Same tree, rendered with a one-line cap: the renderer is short, the
        // read is not.
        let whole = el("window", Some("App"), vec![el("button", Some("OK"), vec![])]);
        let mut capped = TreeOutline::default();
        capped.push_tree(&[whole], "accessibility", 1, true, 0, &no_sensitive());
        assert!(capped.cut_by_cap);
        assert!(capped.incomplete_note().is_none());
        assert!(capped.text.contains("(tree truncated)"));

        // A read that stopped early: the tree is short whatever the cap.
        let mut cut = el("window", Some("App"), vec![]);
        cut.children_count = Some(4);
        let mut partial = TreeOutline::default();
        partial.push_tree(&[cut], "accessibility", 100, true, 0, &no_sensitive());
        assert!(!partial.cut_by_cap);
        let note = partial.incomplete_note().expect("a partial read says so");
        assert!(note.contains("incomplete"), "{note}");
        assert!(note.contains("max_depth") || note.contains("depth"), "{note}");
    }

    /// An empty tree is not evidence that the screen is empty — it must not
    /// be reported as a complete one.
    #[test]
    fn an_empty_tree_is_not_a_truncation() {
        let mut outline = TreeOutline::default();
        outline.push_tree(&[], "accessibility", 100, false, 0, &no_sensitive());
        assert!(outline
            .text
            .contains("(empty — no accessibility tree available)"));
        assert!(!outline.cut_by_cap);
        assert!(outline.incomplete_note().is_none());
    }

    /// The trust line rides with the tree, because a click on a guessed box
    /// and a click on a real element are different promises.
    #[test]
    fn the_trust_line_names_the_source() {
        assert!(trust_line("accessibility").contains("authoritative"));
        assert!(trust_line("omniparser").contains("inferred from pixels"));
    }

    /// A mock adapter that takes its time, for the capture budget.
    struct SlowAdapter(std::time::Duration);

    #[async_trait::async_trait]
    impl ComputerAdapter for SlowAdapter {
        async fn screenshot(
            &self,
            _region: Option<crate::computer::Rect>,
        ) -> crate::computer::Result<crate::computer::Screenshot> {
            tokio::time::sleep(self.0).await;
            Ok(crate::computer::Screenshot::new(String::new(), 100, 100))
        }

        async fn read_ui_tree(
            &self,
            _app: Option<&str>,
        ) -> crate::computer::Result<Vec<UiElement>> {
            Ok(Vec::new())
        }

        async fn execute(
            &self,
            _action: crate::computer::DesktopAction,
        ) -> crate::computer::Result<crate::computer::ActionResult> {
            Ok(crate::computer::ActionResult::success("ok"))
        }

        async fn wait_for(
            &self,
            _condition: crate::computer::WaitCondition,
            _timeout: std::time::Duration,
        ) -> crate::computer::Result<bool> {
            Ok(true)
        }
    }

    /// The capture ceiling follows the same rule as the computer tool's step.
    #[test]
    fn the_capture_budget_is_the_call_timeout_less_a_margin() {
        use std::time::Duration;
        assert_eq!(screen_state_budget_for(Duration::from_secs(30)), Duration::from_secs(27));
        assert_eq!(screen_state_budget_for(Duration::from_secs(120)), Duration::from_secs(115));
    }

    /// A capture slower than the call's budget fails with a message that
    /// names the budget, instead of the registry's opaque "tool timed out".
    #[tokio::test]
    async fn a_capture_past_the_budget_says_so() {
        let tool = ScreenStateTool::new(
            Some(Arc::new(SlowAdapter(std::time::Duration::from_millis(50)))),
            #[cfg(feature = "vision")]
            crate::tools::screen_state::new_shared_ocr(),
            #[cfg(feature = "vision")]
            crate::tools::screen_state::new_shared_ui_detector(),
        );
        let context = ToolContext::default().with_timeout(std::time::Duration::from_millis(20));
        let error = tool
            .execute(json!({}), &context)
            .await
            .expect_err("past the budget");
        assert!(error.to_string().contains("ms"), "{error}");
    }
}
