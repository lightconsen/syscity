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

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{json, Value};
use tracing::warn;

use crate::computer::vision::ScreenState;
#[cfg(feature = "vision")]
use crate::computer::Rect;
use crate::computer::{ComputerAdapter, UiElement};
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

When the accessibility tree is empty (games, image-based UIs, remote desktops, webviews), interactive elements are auto-detected from the screenshot via OmniParser; disable with ui_fallback=false."#
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
            ..Default::default()
        }
    }

    fn is_available(&self, _context: &ToolContext) -> bool {
        self.adapter.is_some()
    }

    async fn execute(
        &self,
        args: Value,
        _context: &ToolContext,
    ) -> crate::Result<ToolExecutionResult> {
        let adapter = self.adapter.as_ref().ok_or_else(|| {
            crate::error::SyscityError::Unsupported(
                "Computer adapter is not configured".to_string(),
            )
        })?;

        let max_lines = args["max_tree_lines"].as_u64().unwrap_or(100) as usize;
        let want_ocr = args["include_ocr"].as_bool().unwrap_or(false);

        #[allow(unused_mut)]
        let mut state = ScreenState::capture_light(adapter.as_ref())
            .await
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

        // Format: indented UI-tree outline + OCR text + screenshot in data.
        let mut output = String::from("UI tree:\n");
        let mut lines = 0usize;
        for root in &state.ui_tree {
            format_element(root, 0, &mut output, &mut lines, max_lines);
        }
        if state.ui_tree.is_empty() {
            output.push_str("(empty — no accessibility tree available)\n");
            #[cfg(not(feature = "vision"))]
            if want_fallback {
                output.push_str("(UI detection unavailable: built without 'vision' feature)\n");
            }
        } else {
            if ui_source == "omniparser" {
                output.push_str(&format!(
                    "(accessibility tree empty — {fallback_count} UI elements detected via OmniParser)\n"
                ));
            }
            if lines >= max_lines {
                output.push_str("… (tree truncated)\n");
            }
        }
        if !state.ocr_text.is_empty() {
            output.push_str("\nOCR text:\n");
            output.push_str(&state.ocr_text);
        }

        // CAS-first: store the screenshot bytes once and return a compact
        // reference; fall back to inline base64 when the store is
        // unavailable (fail-open — see crate::attachments docs).
        let mut data = json!({
            "screenshot_width": state.screenshot.width,
            "screenshot_height": state.screenshot.height,
            "ui_tree": serde_json::to_value(&state.ui_tree).unwrap_or_default(),
            "ui_source": ui_source,
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

/// Render a UI element and its children as an indented outline.
/// Returns `false` when the line cap was hit (tree truncated).
fn format_element(
    el: &UiElement,
    depth: usize,
    out: &mut String,
    lines: &mut usize,
    max_lines: usize,
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
    let state = if el.enabled { "" } else { " [disabled]" };
    out.push_str(&format!(
        "{}{}{}{} at ({},{}) {}x{}\n",
        indent,
        el.role,
        label_part,
        state,
        el.bounds.x,
        el.bounds.y,
        el.bounds.width,
        el.bounds.height
    ));
    for child in &el.children {
        format_element(child, depth + 1, out, lines, max_lines);
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

    fn el(role: &str, label: Option<&str>, children: Vec<UiElement>) -> UiElement {
        UiElement {
            id: String::new(),
            role: role.to_string(),
            label: label.map(String::from),
            value: None,
            bounds: Rect::new(10, 20, 100, 30),
            enabled: true,
            focused: false,
            children,
        }
    }

    #[test]
    fn format_element_renders_indented_outline() {
        let tree = el("window", Some("App"), vec![el("button", Some("OK"), vec![])]);
        let mut out = String::new();
        let mut lines = 0;
        format_element(&tree, 0, &mut out, &mut lines, 100);
        assert!(out.contains("window \"App\" at (10,20) 100x30"));
        assert!(out.contains("  button \"OK\""));
        assert_eq!(lines, 2);
    }

    #[test]
    fn format_element_omits_empty_label() {
        // OmniParser-detected elements carry no label — render without quotes.
        let tree = el("button", None, vec![]);
        let mut out = String::new();
        let mut lines = 0;
        format_element(&tree, 0, &mut out, &mut lines, 100);
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
        format_element(&tree, 0, &mut out, &mut lines, 5);
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
}
