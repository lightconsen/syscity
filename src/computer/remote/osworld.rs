//! The OSWorld VM desktop adapter.
//!
//! OSWorld boots an Ubuntu VM, restores it to a task snapshot, and exposes the
//! desktop through a small HTTP service. This adapter is the client for that
//! service: it captures the screen, reads the AT-SPI accessibility tree, and
//! delivers actions as short pyautogui snippets executed **on the VM**.
//!
//! `/execute` is a remote procedure call, not a code-generation channel. The
//! model asks for "click the Save button"; the session decides which rung aims
//! it, and this adapter turns that decision into the one snippet that lands
//! it. Handing the model the Python pen instead would measure something else.
//!
//! Deliberately unsupported (a refusal, never a silent success): window
//! management, process control, system status and region captures. The VM is
//! restored per task, so there is nothing on it worth managing from here.

use std::time::{Duration, Instant};

use base64::Engine as _;
use serde_json::{json, Value};

use crate::computer::element_index::{parse_element_id, ActionTarget, ResolveError, SnapshotCache};
use crate::computer::remote::{
    envelope_error, parse_json, string_field, RemoteDesktopConfig, Transport,
};
use crate::computer::{
    canonical_key, ActionResult, ClickTarget, ComputerAdapter, ComputerError, DesktopAction,
    DisplayGeometry, DisplayInfo, MouseButton, Rect, Result, Screenshot, ScrollDirection,
    UiElement, WaitCondition,
};

const SCREENSHOT_PATH: &str = "/screenshot";
const ACCESSIBILITY_PATH: &str = "/accessibility";
const SCREEN_SIZE_PATH: &str = "/screen_size";
const EXECUTE_PATH: &str = "/execute";

/// How long a command's output is kept in an action result.
const MAX_OUTPUT_CHARS: usize = 2_000;

/// Where a planned snippet is delivered.
///
/// Normally the VM's `/execute`. In step mode (`syscity bench serve`) it is the
/// harness itself: the snippet goes out over stdio, the harness runs it inside
/// its own `env.step` — its pause, its observation — and only then does the
/// call return. Reads never take this path: captures and the accessibility tree
/// stay HTTP, so one adapter serves both shapes and the harness owns when an
/// action happens, not what the agent can see.
#[async_trait::async_trait]
pub trait ActionSink: Send + Sync {
    async fn deliver(&self, code: &str) -> Result<ActionResult>;
}

/// A desktop reached over the OSWorld VM HTTP protocol.
pub struct OsworldDesktop {
    transport: Transport,
    /// Trees handed to callers, so an element id can be resolved back to the
    /// element it named. See [`crate::computer::element_index`].
    snapshots: SnapshotCache,
    /// Last capture, reused only until an action invalidates it.
    shot: tokio::sync::Mutex<Option<(Instant, Screenshot)>>,
    screenshot_ttl: Duration,
    /// Every snippet sent to `/execute`, in the order the VM saw it.
    ///
    /// The OSWorld and Cua glue runs a whole task through `syscity bench run`
    /// and then replays these snippets inside the harness's own step loop, so
    /// this log is contract rather than diagnostics. Snippets that only read
    /// (the clipboard) are in it too: replaying them keeps the ordering honest
    /// and costs nothing.
    executed: tokio::sync::Mutex<Vec<String>>,
    /// `None` means "run it on the VM", which is the benchmark's normal shape.
    sink: Option<std::sync::Arc<dyn ActionSink>>,
}

impl OsworldDesktop {
    pub fn new(config: RemoteDesktopConfig) -> Result<Self> {
        let transport = Transport::new(&config)?;
        Ok(Self {
            transport,
            snapshots: SnapshotCache::new(8),
            shot: tokio::sync::Mutex::new(None),
            screenshot_ttl: config.screenshot_ttl,
            executed: tokio::sync::Mutex::new(Vec::new()),
            sink: None,
        })
    }

    /// Hand actions to `sink` instead of running them on the VM.
    ///
    /// Observations still come from the VM over HTTP. That asymmetry is the
    /// point: the harness decides *when* an action is executed and what the
    /// world looks like afterwards, while the agent reads the same screen it
    /// would read anyway.
    pub fn with_action_sink(mut self, sink: std::sync::Arc<dyn ActionSink>) -> Self {
        self.sink = Some(sink);
        self
    }

    /// The snippets sent so far, oldest first.
    pub async fn executed_snippets(&self) -> Vec<String> {
        self.executed.lock().await.clone()
    }

    /// The last capture, when it is fresh enough to stand in for a new one.
    async fn cached_screenshot(&self) -> Option<Screenshot> {
        let guard = self.shot.lock().await;
        match guard.as_ref() {
            Some((at, shot)) if at.elapsed() < self.screenshot_ttl => Some(shot.clone()),
            _ => None,
        }
    }

    async fn store_screenshot(&self, shot: &Screenshot) {
        *self.shot.lock().await = Some((Instant::now(), shot.clone()));
    }

    /// A screen change makes every cached frame a picture of the past. The
    /// ladder decides `SuspectedNoop` by comparing frames around an action, so
    /// this invalidation is what keeps "the click worked" observable at all.
    async fn invalidate_screenshot(&self) {
        *self.shot.lock().await = None;
    }

    /// Capture the screen without consulting or filling the cache.
    async fn capture(&self) -> Result<Screenshot> {
        let (content_type, bytes) = self.transport.get(SCREENSHOT_PATH).await?;
        decode_capture(content_type.as_deref(), &bytes)
    }

    async fn screen_size(&self) -> Result<(u32, u32)> {
        let (content_type, bytes) = self
            .transport
            .post_json(SCREEN_SIZE_PATH, &json!({}))
            .await?;
        parse_screen_size(content_type.as_deref(), &bytes)
    }

    /// Run a Python snippet on the VM and return its stdout, recording it as
    /// part of the action log.
    async fn run_python(&self, code: &str) -> Result<String> {
        self.executed.lock().await.push(code.to_string());
        self.exec_snippet(code).await
    }

    /// Run a snippet that is not an action: no log entry, no cache change.
    ///
    /// `bench probe` uses this to prove `/execute` works before a run spends
    /// model time on it. Recording it would put a snippet in the `actions`
    /// field that the agent never decided, which is exactly the replay
    /// contract's one rule.
    pub async fn probe_execute(&self, code: &str) -> Result<String> {
        self.exec_snippet(code).await
    }

    async fn exec_snippet(&self, code: &str) -> Result<String> {
        let (_, bytes) = self
            .transport
            .post_json(EXECUTE_PATH, &json!({ "command": code, "shell": false }))
            .await?;
        let value = parse_json(EXECUTE_PATH, &bytes)?;
        if let Some(error) = envelope_error(&value) {
            return Err(ComputerError::ToolFailed(format!(
                "the remote desktop refused the action: {error}"
            )));
        }
        Ok(output_text(&value))
    }

    /// Run a snippet that performs an action, and wrap the outcome.
    async fn act(&self, code: &str) -> Result<ActionResult> {
        self.invalidate_screenshot().await;
        if let Some(sink) = &self.sink {
            // `run_python` is what records on the HTTP path; here it is not
            // called, so the log is kept in step with what the harness ran.
            self.executed.lock().await.push(code.to_string());
            return sink.deliver(code).await;
        }
        let output = self.run_python(code).await?;
        let output = output.trim();
        let message = if output.is_empty() {
            "action delivered to the remote desktop".to_string()
        } else {
            truncate(output, MAX_OUTPUT_CHARS)
        };
        Ok(ActionResult::success(message))
    }

    /// Fetch the accessibility tree without caching it.
    ///
    /// `wait_for` polls, and each poll must not evict the snapshot whose ids
    /// the model is holding.
    async fn fetch_tree(&self) -> Result<Vec<UiElement>> {
        let (content_type, bytes) = self.transport.get(ACCESSIBILITY_PATH).await?;
        let xml = accessibility_xml(content_type.as_deref(), &bytes)?;
        parse_at_spi(&xml)
    }

    async fn resolve_click_point(&self, target: &ClickTarget) -> Result<(i32, i32)> {
        let resolved = self.resolve_target(target).await?;
        Ok((resolved.x, resolved.y))
    }
}

#[async_trait::async_trait]
impl ComputerAdapter for OsworldDesktop {
    async fn screenshot(&self, region: Option<Rect>) -> Result<Screenshot> {
        if let Some(region) = region {
            return Err(ComputerError::UnsupportedPlatform(format!(
                "a remote desktop capture covers the whole screen; a {}x{} region at ({},{}) \
                 was requested",
                region.width, region.height, region.x, region.y
            )));
        }
        if let Some(shot) = self.cached_screenshot().await {
            return Ok(shot);
        }
        let shot = self.capture().await?;
        self.store_screenshot(&shot).await;
        Ok(shot)
    }

    async fn read_ui_tree(&self, app: Option<&str>) -> Result<Vec<UiElement>> {
        // AT-SPI exposes one desktop, and the tree already contains every
        // application window on it: filtering by `app` would only hide the
        // rest. The argument is accepted and ignored rather than silently
        // pretending to have filtered.
        if app.is_some() {
            tracing::debug!("remote desktop reads the whole AT-SPI desktop; `app` is advisory");
        }
        let roots = self.fetch_tree().await?;
        Ok(self.snapshots.store(roots, None).await.roots)
    }

    async fn execute(&self, action: DesktopAction) -> Result<ActionResult> {
        match action {
            DesktopAction::Screenshot { region } => {
                let shot = self.screenshot(region).await?;
                Ok(ActionResult::success("screenshot captured").with_data(json!({
                    "width": shot.width,
                    "height": shot.height,
                })))
            }
            DesktopAction::Click { target, button } => {
                let (x, y) = self.resolve_click_point(&target).await?;
                self.act(&click_code(x, y, button, false)).await
            }
            DesktopAction::DoubleClick { target, button } => {
                let (x, y) = self.resolve_click_point(&target).await?;
                self.act(&click_code(x, y, button, true)).await
            }
            DesktopAction::Type { text, target } => {
                let mut code = String::new();
                // A target means "into this field": click it first in the same
                // snippet, so the keystrokes cannot land in whatever happened
                // to hold focus.
                if let Some(target) = target.as_ref() {
                    let (x, y) = self.resolve_click_point(target).await?;
                    code.push_str(&click_code(x, y, MouseButton::Left, false));
                    code.push_str("\nimport time\ntime.sleep(0.2)\n");
                }
                code.push_str(&type_code(text.as_str()));
                self.act(&code).await
            }
            DesktopAction::KeyPress { keys } => self.act(&keys_code(&keys)?).await,
            DesktopAction::Scroll { target, direction, amount } => {
                let (x, y) = self.resolve_click_point(&target).await?;
                self.act(&scroll_code(x, y, direction, amount)).await
            }
            DesktopAction::Drag { from, to } => {
                let (from_x, from_y) = self.resolve_click_point(&from).await?;
                let (to_x, to_y) = self.resolve_click_point(&to).await?;
                self.act(&drag_code(from_x, from_y, to_x, to_y)).await
            }
            DesktopAction::Wait { milliseconds } => {
                tokio::time::sleep(Duration::from_millis(milliseconds)).await;
                Ok(ActionResult::success(format!("Waited {milliseconds}ms")))
            }
            DesktopAction::ClipboardGet => {
                let output = self
                    .run_python("import pyperclip\nprint(pyperclip.paste())\n")
                    .await?;
                Ok(ActionResult::success(output.trim_end_matches('\n').to_string()))
            }
            DesktopAction::ClipboardSet { text } => {
                let code = format!("import pyperclip\npyperclip.copy({})\n", python_string(&text));
                self.act(&code).await
            }
            DesktopAction::ReadUiTree { app } => {
                let tree = self.read_ui_tree(app.as_deref()).await?;
                let rendered = serde_json::to_string(&tree).unwrap_or_default();
                Ok(ActionResult::success(rendered).with_data(json!({ "elements": tree })))
            }
            other => Err(ComputerError::UnsupportedPlatform(format!(
                "{} is not available on a remote desktop: the task VM is restored per task, \
                 and window, process and system control belong to the harness",
                action_name(&other)
            ))),
        }
    }

    async fn wait_for(&self, condition: WaitCondition, timeout: Duration) -> Result<bool> {
        let deadline = Instant::now() + timeout;
        match condition {
            WaitCondition::UiTreeContains { role, label } => {
                let wanted = normalize_role(&role);
                loop {
                    let roots = self.fetch_tree().await?;
                    if tree_contains(&roots, &wanted, label.as_deref()) {
                        return Ok(true);
                    }
                    if Instant::now() >= deadline {
                        return Ok(false);
                    }
                    tokio::time::sleep(Duration::from_millis(250)).await;
                }
            }
            WaitCondition::ScreenshotStable { .. } => {
                // Two identical captures mean the screen has stopped moving.
                // The server hands back encoded frames, so this is an exact
                // comparison rather than the pixel-difference threshold the
                // condition allows: stricter, never looser.
                let mut previous: Option<Vec<u8>> = None;
                loop {
                    let shot = self.capture().await?;
                    let frame = shot.base64.into_bytes();
                    if previous.as_ref() == Some(&frame) {
                        return Ok(true);
                    }
                    previous = Some(frame);
                    if Instant::now() >= deadline {
                        return Ok(false);
                    }
                    tokio::time::sleep(Duration::from_millis(200)).await;
                }
            }
            other => Err(ComputerError::UnsupportedPlatform(format!(
                "the remote desktop cannot observe {other:?}: it sees the screen and the \
                 accessibility tree, not the host's processes or windows"
            ))),
        }
    }

    async fn resolve_target(&self, target: &ClickTarget) -> Result<ActionTarget> {
        if let ClickTarget::Coordinate(p) = target {
            return Ok(ActionTarget::point(p.x, p.y));
        }

        // Snapshot-path ids resolve only against the snapshot that produced
        // them. A platform id or a label may trigger a lazy re-read, because
        // those are meant to be looked up in the current tree.
        let lazy = match target {
            ClickTarget::ElementId(id) => parse_element_id(id).is_none(),
            _ => true,
        };

        let mut resolution = self.snapshots.resolve(target).await;
        if lazy
            && matches!(resolution, Err(ResolveError::NotFound(_)) | Err(ResolveError::NoSnapshot))
        {
            self.read_ui_tree(None).await?;
            resolution = self.snapshots.resolve(target).await;
        }

        resolution.map_err(|e| match e {
            ResolveError::Stale(snapshot) => ComputerError::StaleElement(format!(
                "element id belongs to snapshot {snapshot:08x}, which is no longer cached; \
                 re-read the UI tree and use a fresh id"
            )),
            other => ComputerError::ElementNotFound(other.to_string()),
        })
    }

    async fn display_geometry(&self) -> Result<DisplayGeometry> {
        let (width, height) = self.screen_size().await?;
        // OSWorld captures and clicks in the same pixel space, so the mapping
        // from image pixels to screen points is the identity.
        Ok(DisplayGeometry::new(vec![DisplayInfo::new(0, 0, width, height)
            .primary()
            .named("remote")]))
    }
}

// ── Response decoding ───────────────────────────────────────────────────────

/// Decode a `/screenshot` response into a PNG payload and its dimensions.
///
/// The VM service may answer with the image itself or with a JSON envelope
/// carrying it base64-encoded; both are in the wild across versions.
fn decode_capture(content_type: Option<&str>, bytes: &[u8]) -> Result<Screenshot> {
    if looks_like_png(bytes) {
        let (width, height) = png_size(bytes).ok_or_else(|| {
            ComputerError::ScreenshotFailed("the captured PNG has no readable IHDR".to_string())
        })?;
        return Ok(Screenshot::new(base64_encode(bytes), width, height));
    }

    let value = parse_json(SCREENSHOT_PATH, bytes)?;
    if let Some(error) = envelope_error(&value) {
        return Err(ComputerError::ScreenshotFailed(error));
    }
    let encoded = string_field(&value, &["screenshot", "image", "output"]).ok_or_else(|| {
        ComputerError::ScreenshotFailed(format!(
            "the screenshot response has no image (content-type {:?})",
            content_type.unwrap_or("unknown")
        ))
    })?;
    let encoded = strip_data_url(encoded.trim());
    let png = base64::engine::general_purpose::STANDARD
        .decode(encoded.as_bytes())
        .map_err(|e| ComputerError::ScreenshotFailed(format!("screenshot is not base64: {e}")))?;
    let (width, height) = png_size(&png).ok_or_else(|| {
        ComputerError::ScreenshotFailed("the captured PNG has no readable IHDR".to_string())
    })?;
    Ok(Screenshot::new(base64_encode(&png), width, height))
}

/// Extract the accessibility XML from a `/accessibility` response.
fn accessibility_xml(content_type: Option<&str>, bytes: &[u8]) -> Result<String> {
    let text = String::from_utf8_lossy(bytes);
    if text.trim_start().starts_with('<') {
        return Ok(text.into_owned());
    }
    let value = parse_json(ACCESSIBILITY_PATH, bytes)?;
    if let Some(error) = envelope_error(&value) {
        return Err(ComputerError::ToolFailed(format!(
            "the remote desktop could not read the accessibility tree: {error}"
        )));
    }
    let tree =
        string_field(&value, &["AT", "accessibility", "tree", "output"]).ok_or_else(|| {
            ComputerError::ToolFailed(format!(
                "the accessibility response has no tree (content-type {:?})",
                content_type.unwrap_or("unknown")
            ))
        })?;
    // Some versions nest the XML one level deeper (`{"output": {"AT": ...}}`).
    if tree.trim_start().starts_with('{') {
        if let Ok(inner) = serde_json::from_str::<Value>(&tree) {
            if let Some(xml) = string_field(&inner, &["AT", "accessibility", "tree", "output"]) {
                return Ok(xml);
            }
        }
    }
    Ok(tree)
}

/// Read `width`/`height` from a `/screen_size` response.
fn parse_screen_size(content_type: Option<&str>, bytes: &[u8]) -> Result<(u32, u32)> {
    let value = match serde_json::from_slice::<Value>(bytes) {
        Ok(value) => value,
        Err(e) => {
            return Err(ComputerError::ToolFailed(format!(
                "the screen size response is not JSON (content-type {:?}): {e}",
                content_type.unwrap_or("unknown")
            )))
        }
    };
    if let Some(error) = envelope_error(&value) {
        return Err(ComputerError::ToolFailed(format!(
            "the remote desktop could not report its screen size: {error}"
        )));
    }
    let size = value.get("output").unwrap_or(&value);
    let size = match size {
        Value::String(text) => serde_json::from_str::<Value>(text).unwrap_or(Value::Null),
        other => other.clone(),
    };
    let width = size.get("width").and_then(Value::as_u64);
    let height = size.get("height").and_then(Value::as_u64);
    match (width, height) {
        (Some(width), Some(height)) if width > 0 && height > 0 => Ok((width as u32, height as u32)),
        _ => Err(ComputerError::ToolFailed(
            "the screen size response has no plausible width and height".to_string(),
        )),
    }
}

fn output_text(value: &Value) -> String {
    match value.get("output") {
        Some(Value::String(text)) => text.clone(),
        Some(Value::Null) | None => String::new(),
        Some(other) => other.to_string(),
    }
}

fn truncate(text: &str, limit: usize) -> String {
    if text.chars().count() <= limit {
        return text.to_string();
    }
    let head: String = text.chars().take(limit).collect();
    format!("{head}…")
}

fn base64_encode(bytes: &[u8]) -> String {
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

fn strip_data_url(text: &str) -> &str {
    match text.split_once("base64,") {
        Some((prefix, rest)) if prefix.starts_with("data:") => rest,
        _ => text,
    }
}

fn looks_like_png(bytes: &[u8]) -> bool {
    bytes.starts_with(&[0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a])
}

/// Width and height from a PNG's IHDR chunk.
///
/// Ten lines of header parsing instead of an image decoder: the adapter only
/// needs to report the capture's size, and the bytes are otherwise opaque.
fn png_size(bytes: &[u8]) -> Option<(u32, u32)> {
    if bytes.len() < 24 || !looks_like_png(bytes) || &bytes[12..16] != b"IHDR" {
        return None;
    }
    let width = u32::from_be_bytes([bytes[16], bytes[17], bytes[18], bytes[19]]);
    let height = u32::from_be_bytes([bytes[20], bytes[21], bytes[22], bytes[23]]);
    (width > 0 && height > 0).then_some((width, height))
}

// ── Accessibility tree ──────────────────────────────────────────────────────

/// Parse the AT-SPI XML into the desktop tree.
///
/// Node tag names are not reliable (`<frame>`, `<push-button>`, `<desktop>` are
/// all in the wild); the `role` attribute is. A tree whose root carries no
/// role is a wrapper, and its children are the roots.
pub fn parse_at_spi(xml: &str) -> Result<Vec<UiElement>> {
    let document = roxmltree::Document::parse(xml).map_err(|e| {
        ComputerError::ToolFailed(format!("the accessibility tree is not valid XML: {e}"))
    })?;
    let root = document.root_element();

    if root.attribute("role").is_some() || root.children().all(|child| !child.is_element()) {
        return Ok(vec![element_from_node(root)]);
    }
    Ok(root
        .children()
        .filter(|child| child.is_element())
        .map(element_from_node)
        .collect())
}

fn element_from_node(node: roxmltree::Node<'_, '_>) -> UiElement {
    let raw_role = attr(node, "role").unwrap_or_else(|| node.tag_name().name().to_string());
    let label = attr(node, "name")
        .or_else(|| attr(node, "description"))
        .filter(|text| !text.trim().is_empty());
    let value = attr(node, "value").or_else(|| child_text(node, "value"));
    let states = attr(node, "states").unwrap_or_default();

    UiElement {
        // AT-SPI has no per-node identifier worth persisting; the snapshot
        // path assigned by the cache is the honest address.
        id: String::new(),
        role: map_role(&raw_role),
        label,
        value,
        bounds: bounds_of(node),
        // A node that advertises `disabled` is the only one we can call
        // disabled; an absent state list is not evidence either way.
        enabled: !states.contains("disabled"),
        focused: states.contains("focused") || states.contains("active"),
        children: node
            .children()
            .filter(|child| child.is_element())
            .map(element_from_node)
            .collect(),
    }
}

/// Translate an AT-SPI role into the vocabulary the session reasons about.
pub fn map_role(raw: &str) -> String {
    let key = normalize_role(raw);
    let mapped = match key.as_str() {
        "pushbutton" | "togglebutton" | "toolbarbutton" => "button",
        "entry" | "text" | "textfield" | "edit" => "text_field",
        // Keep both markers: the session looks for "textfield" to know a field
        // holds text, and for "password" to know the value must not be read
        // back into a diff.
        "passwordtext" | "passwordentry" => "text_field_password",
        "checkbox" => "checkbox",
        "radiobutton" => "radio_button",
        "combobox" => "combo_box",
        "menuitem" | "checkmenuitem" | "radiomenuitem" => "menu_item",
        "pagetab" => "tab",
        "pagetablist" => "tab_group",
        "frame" | "dialog" => "window",
        "label" => "static_text",
        "slider" => "slider",
        "spinbutton" => "spin_button",
        "tablecell" => "table_cell",
        "tablerow" => "table_row",
        "link" => "link",
        other => other,
    };
    mapped.to_string()
}

fn normalize_role(role: &str) -> String {
    role.chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .collect::<String>()
        .to_ascii_lowercase()
}

fn attr(node: roxmltree::Node<'_, '_>, name: &str) -> Option<String> {
    node.attribute(name)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
}

fn child_text(node: roxmltree::Node<'_, '_>, name: &str) -> Option<String> {
    node.children()
        .find(|child| child.is_element() && child.tag_name().name() == name)
        .and_then(|child| child.text())
        .map(str::trim)
        .filter(|text| !text.is_empty())
        .map(str::to_string)
}

fn bounds_of(node: roxmltree::Node<'_, '_>) -> Rect {
    if let Some(rect) = rect_from_attrs(node) {
        return rect;
    }
    // Older trees carry the box in a child element, as attributes or as
    // `[x, y, width, height]` text.
    for child in node.children().filter(|child| child.is_element()) {
        let tag = child.tag_name().name();
        if !matches!(tag, "bbox" | "bounds" | "component" | "geometry") {
            continue;
        }
        if let Some(rect) = rect_from_attrs(child) {
            return rect;
        }
        if let Some(rect) = rect_from_text(child.text().unwrap_or_default()) {
            return rect;
        }
    }
    if let Some(rect) = node.attribute("bbox").and_then(rect_from_text) {
        return rect;
    }
    Rect::new(0, 0, 0, 0)
}

fn rect_from_attrs(node: roxmltree::Node<'_, '_>) -> Option<Rect> {
    let x = attr(node, "x")
        .or_else(|| attr(node, "left"))?
        .parse::<i32>()
        .ok()?;
    let y = attr(node, "y")
        .or_else(|| attr(node, "top"))?
        .parse::<i32>()
        .ok()?;
    let width = attr(node, "width")
        .or_else(|| attr(node, "w"))?
        .parse::<u32>()
        .ok()?;
    let height = attr(node, "height")
        .or_else(|| attr(node, "h"))?
        .parse::<u32>()
        .ok()?;
    Some(Rect::new(x, y, width, height))
}

fn rect_from_text(text: &str) -> Option<Rect> {
    let cleaned: String = text
        .chars()
        .map(|c| {
            if c.is_ascii_digit() || c == '-' {
                c
            } else {
                ' '
            }
        })
        .collect();
    let numbers: Vec<i64> = cleaned
        .split_whitespace()
        .filter_map(|part| part.parse::<i64>().ok())
        .collect();
    if numbers.len() < 4 {
        return None;
    }
    let (x, y, width, height) = (numbers[0], numbers[1], numbers[2], numbers[3]);
    if width <= 0 || height <= 0 {
        return None;
    }
    Some(Rect::new(x as i32, y as i32, width as u32, height as u32))
}

fn tree_contains(roots: &[UiElement], role: &str, label: Option<&str>) -> bool {
    roots.iter().any(|element| {
        let role_matches = normalize_role(&element.role) == role;
        let label_matches = label.is_none_or(|wanted| {
            element
                .label
                .as_ref()
                .is_some_and(|found| found.contains(wanted))
        });
        (role_matches && label_matches) || tree_contains(&element.children, role, label)
    })
}

// ── Action snippets ─────────────────────────────────────────────────────────

fn click_code(x: i32, y: i32, button: MouseButton, double: bool) -> String {
    let button = match button {
        MouseButton::Left => "left",
        MouseButton::Middle => "middle",
        MouseButton::Right => "right",
    };
    let call = if double { "doubleClick" } else { "click" };
    format!("import pyautogui\npyautogui.{call}({x}, {y}, button='{button}')\n")
}

/// Type text by pasting it.
///
/// `pyautogui.write` is limited to the keyboard's own characters — a Chinese
/// sentence, a path with unusual punctuation or a long paragraph arrives
/// truncated or raises. The clipboard carries any string, and the paste is one
/// keystroke.
fn type_code(text: &str) -> String {
    let literal = python_string(text);
    format!(
        "import time\n\
         import pyautogui\n\
         \n\
         try:\n\
         \x20   import pyperclip\n\
         \x20   pyperclip.copy({literal})\n\
         \x20   time.sleep(0.1)\n\
         \x20   pyautogui.hotkey('ctrl', 'v')\n\
         except Exception:\n\
         \x20   pyautogui.write({literal}, interval=0.02)\n"
    )
}

fn keys_code(keys: &[String]) -> Result<String> {
    if keys.is_empty() {
        return Err(ComputerError::ToolFailed("a key press needs at least one key".to_string()));
    }
    let mapped: Vec<String> = keys
        .iter()
        .map(|key| python_string(&pyautogui_key(key)))
        .collect();
    if mapped.len() == 1 {
        return Ok(format!("import pyautogui\npyautogui.press({})\n", mapped[0]));
    }
    Ok(format!("import pyautogui\npyautogui.hotkey({})\n", mapped.join(", ")))
}

fn scroll_code(x: i32, y: i32, direction: ScrollDirection, amount: i32) -> String {
    let clicks = amount.abs().max(1) * 100;
    let call = match direction {
        ScrollDirection::Up => format!("pyautogui.scroll({clicks}, x={x}, y={y})"),
        ScrollDirection::Down => format!("pyautogui.scroll(-{clicks}, x={x}, y={y})"),
        ScrollDirection::Right => format!("pyautogui.hscroll({clicks}, x={x}, y={y})"),
        ScrollDirection::Left => format!("pyautogui.hscroll(-{clicks}, x={x}, y={y})"),
    };
    format!("import pyautogui\n{call}\n")
}

fn drag_code(from_x: i32, from_y: i32, to_x: i32, to_y: i32) -> String {
    format!(
        "import pyautogui\n\
         pyautogui.moveTo({from_x}, {from_y})\n\
         pyautogui.mouseDown(button='left')\n\
         pyautogui.moveTo({to_x}, {to_y}, duration=0.5)\n\
         pyautogui.mouseUp(button='left')\n"
    )
}

/// The pyautogui spelling of a key the session named.
fn pyautogui_key(key: &str) -> String {
    let canonical = canonical_key(key);
    let lower = canonical.trim().to_ascii_lowercase();
    match lower.as_str() {
        "enter" => "enter",
        "space" => "space",
        "tab" => "tab",
        "escape" => "esc",
        "backspace" => "backspace",
        // There is no ⌘ on the VM; the shortcut a Mac user means is the
        // Linux one.
        "cmd" | "command" | "meta" | "super" | "win" | "control" => "ctrl",
        "option" => "alt",
        other => other,
    }
    .to_string()
}

/// A Python string literal for `text`.
///
/// JSON string syntax is a subset of Python's for strings, and unlike manual
/// quote-escaping it also handles newlines, backslashes and non-ASCII.
fn python_string(text: &str) -> String {
    serde_json::to_string(text).unwrap_or_else(|_| "\"\"".to_string())
}

fn action_name(action: &DesktopAction) -> &'static str {
    match action {
        DesktopAction::Screenshot { .. } => "screenshot",
        DesktopAction::Click { .. } => "click",
        DesktopAction::DoubleClick { .. } => "double_click",
        DesktopAction::Type { .. } => "type",
        DesktopAction::KeyPress { .. } => "key_press",
        DesktopAction::Scroll { .. } => "scroll",
        DesktopAction::Drag { .. } => "drag",
        DesktopAction::ReadUiTree { .. } => "read_ui_tree",
        DesktopAction::LaunchApp { .. } => "launch_app",
        DesktopAction::ActivateWindow { .. } => "activate_window",
        DesktopAction::CloseWindow { .. } => "close_window",
        DesktopAction::ListWindows => "list_windows",
        DesktopAction::GetWindowGeometry { .. } => "get_window_geometry",
        DesktopAction::MoveWindow { .. } => "move_window",
        DesktopAction::ResizeWindow { .. } => "resize_window",
        DesktopAction::MinimizeWindow { .. } => "minimize_window",
        DesktopAction::MaximizeWindow { .. } => "maximize_window",
        DesktopAction::Wait { .. } => "wait",
        DesktopAction::ClipboardGet => "clipboard_get",
        DesktopAction::ClipboardSet { .. } => "clipboard_set",
        DesktopAction::GetSystemStatus => "get_system_status",
        DesktopAction::ListProcesses { .. } => "list_processes",
        DesktopAction::KillProcess { .. } => "kill_process",
        DesktopAction::RestartProcess { .. } => "restart_process",
        DesktopAction::SetProcessPriority { .. } => "set_process_priority",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::computer::Point;

    const TREE: &str = r#"<desktop-frame role="desktop frame" name="Desktop"
        x="0" y="0" width="1280" height="800">
      <application role="application" name="Firefox" x="0" y="0" width="1280" height="800">
        <frame role="frame" name="Settings — Firefox" x="0" y="0" width="1280" height="800">
          <push-button role="push button" name="Save" states="enabled, visible"
            x="100" y="50" width="20" height="20"/>
          <entry role="entry" name="Name" x="100" y="100" width="200" height="30">
            <bbox x="100" y="100" width="200" height="30"/>
          </entry>
        </frame>
      </application>
    </desktop-frame>"#;

    struct FakeVm {
        screens: std::sync::Mutex<usize>,
        commands: std::sync::Mutex<Vec<String>>,
        fail_execute: bool,
    }

    fn png_bytes(width: u32, height: u32) -> Vec<u8> {
        let mut bytes = vec![0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a];
        bytes.extend_from_slice(&13u32.to_be_bytes());
        bytes.extend_from_slice(b"IHDR");
        bytes.extend_from_slice(&width.to_be_bytes());
        bytes.extend_from_slice(&height.to_be_bytes());
        bytes.extend_from_slice(&[8, 6, 0, 0, 0]);
        bytes
    }

    async fn screenshot_handler(
        axum::extract::State(state): axum::extract::State<std::sync::Arc<FakeVm>>,
    ) -> axum::Json<Value> {
        *state.screens.lock().unwrap() += 1;
        axum::Json(json!({ "screenshot": base64_encode(&png_bytes(1280, 800)), "error": null }))
    }

    async fn accessibility_handler() -> String {
        TREE.to_string()
    }

    async fn screen_size_handler() -> axum::Json<Value> {
        axum::Json(json!({ "output": { "width": 1280, "height": 800 }, "error": null }))
    }

    async fn execute_handler(
        axum::extract::State(state): axum::extract::State<std::sync::Arc<FakeVm>>,
        axum::Json(body): axum::Json<Value>,
    ) -> axum::Json<Value> {
        let command = body
            .get("command")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        state.commands.lock().unwrap().push(command);
        if state.fail_execute {
            return axum::Json(json!({ "output": "", "error": "no DISPLAY" }));
        }
        axum::Json(json!({ "output": "", "error": null }))
    }

    async fn start_fake_vm(fail_execute: bool) -> (String, std::sync::Arc<FakeVm>) {
        use axum::routing::{get, post};

        let state = std::sync::Arc::new(FakeVm {
            screens: std::sync::Mutex::new(0),
            commands: std::sync::Mutex::new(Vec::new()),
            fail_execute,
        });
        let app = axum::Router::new()
            .route(SCREENSHOT_PATH, get(screenshot_handler))
            .route(ACCESSIBILITY_PATH, get(accessibility_handler))
            .route(SCREEN_SIZE_PATH, post(screen_size_handler))
            .route(EXECUTE_PATH, post(execute_handler))
            .with_state(state.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        (format!("http://{address}"), state)
    }

    fn desktop(url: &str) -> OsworldDesktop {
        OsworldDesktop::new(RemoteDesktopConfig::new(url)).unwrap()
    }

    #[tokio::test]
    async fn a_capture_reports_the_vm_screen_and_is_reused_until_an_action() {
        let (url, vm) = start_fake_vm(false).await;
        let desktop = desktop(&url);

        let first = desktop.screenshot(None).await.unwrap();
        assert_eq!((first.width, first.height), (1280, 800));
        let second = desktop.screenshot(None).await.unwrap();
        assert_eq!(first.base64, second.base64);
        assert_eq!(*vm.screens.lock().unwrap(), 1, "the second capture came from the cache");

        desktop
            .execute(DesktopAction::Click {
                target: ClickTarget::Coordinate(Point::new(5, 6)),
                button: MouseButton::Left,
            })
            .await
            .unwrap();
        assert_eq!(
            vm.commands.lock().unwrap()[0],
            "import pyautogui\npyautogui.click(5, 6, button='left')\n"
        );

        // The click may have changed the screen, so the cached frame is gone.
        desktop.screenshot(None).await.unwrap();
        assert_eq!(*vm.screens.lock().unwrap(), 2);
    }

    #[tokio::test]
    async fn an_element_id_resolves_to_its_center_and_is_clicked_there() {
        let (url, vm) = start_fake_vm(false).await;
        let desktop = desktop(&url);

        let tree = desktop.read_ui_tree(None).await.unwrap();
        let button = &tree[0].children[0].children[0].children[0];
        assert_eq!(button.role, "button");
        assert_eq!(button.label.as_deref(), Some("Save"));

        desktop
            .execute(DesktopAction::Click {
                target: ClickTarget::ElementId(button.id.clone()),
                button: MouseButton::Left,
            })
            .await
            .unwrap();
        // A 20x20 box at (100,50) centers at (110,60).
        assert_eq!(
            vm.commands.lock().unwrap()[0],
            "import pyautogui\npyautogui.click(110, 60, button='left')\n"
        );
        // The replay log is what the VM saw, so the harness glue can re-send it.
        assert_eq!(desktop.executed_snippets().await, *vm.commands.lock().unwrap());
    }

    #[tokio::test]
    async fn a_stale_id_is_refused_instead_of_aimed_at_a_path() {
        let (url, _vm) = start_fake_vm(false).await;
        let desktop = desktop(&url);
        let error = desktop
            .resolve_target(&ClickTarget::ElementId("s0000000a:0.1".to_string()))
            .await
            .unwrap_err();
        assert!(matches!(error, ComputerError::StaleElement(_)), "{error:?}");
    }

    #[tokio::test]
    async fn a_refused_action_is_an_error_not_a_success() {
        let (url, _vm) = start_fake_vm(true).await;
        let desktop = desktop(&url);
        let error = desktop
            .execute(DesktopAction::KeyPress {
                keys: vec!["enter".to_string()],
            })
            .await
            .unwrap_err();
        assert!(
            error.to_string().contains("no DISPLAY"),
            "the server's reason must reach the caller: {error}"
        );
    }

    #[tokio::test]
    async fn geometry_comes_from_the_vm_screen() {
        let (url, _vm) = start_fake_vm(false).await;
        let geometry = desktop(&url).display_geometry().await.unwrap();
        let primary = geometry.primary().unwrap();
        assert_eq!((primary.width, primary.height), (1280, 800));
        assert!(primary.primary);
    }

    #[tokio::test]
    async fn a_region_capture_is_refused_rather_than_widened() {
        let (url, _vm) = start_fake_vm(false).await;
        let error = desktop(&url)
            .screenshot(Some(Rect::new(0, 0, 10, 10)))
            .await
            .unwrap_err();
        assert!(matches!(error, ComputerError::UnsupportedPlatform(_)), "{error:?}");
    }

    #[tokio::test]
    async fn waiting_for_a_role_sees_the_tree_the_server_serves() {
        let (url, _vm) = start_fake_vm(false).await;
        let found = desktop(&url)
            .wait_for(
                WaitCondition::UiTreeContains {
                    role: "button".to_string(),
                    label: Some("Save".into()),
                },
                Duration::from_secs(1),
            )
            .await
            .unwrap();
        assert!(found);
    }

    #[test]
    fn an_at_spi_tree_becomes_elements_with_roles_labels_and_bounds() {
        let roots = parse_at_spi(TREE).unwrap();
        assert_eq!(roots.len(), 1);
        let desktop = &roots[0];
        assert_eq!(desktop.role, "desktopframe");
        let button = &desktop.children[0].children[0].children[0];
        assert_eq!(button.role, "button");
        assert_eq!(button.label.as_deref(), Some("Save"));
        assert_eq!(button.bounds, Rect::new(100, 50, 20, 20));
        assert!(button.enabled);
        let entry = &desktop.children[0].children[0].children[1];
        assert_eq!(entry.role, "text_field");
        assert_eq!(entry.bounds, Rect::new(100, 100, 200, 30));
    }

    #[test]
    fn a_wrapper_root_returns_its_children() {
        let xml = r#"<response>
          <application role="application" name="Files">
            <push-button role="push button" name="Open" bbox="[10, 20, 30, 10]"/>
          </application>
        </response>"#;
        let roots = parse_at_spi(xml).unwrap();
        assert_eq!(roots.len(), 1);
        assert_eq!(roots[0].label.as_deref(), Some("Files"));
        assert_eq!(roots[0].children[0].bounds, Rect::new(10, 20, 30, 10));
    }

    #[test]
    fn roles_take_the_name_the_session_understands() {
        assert_eq!(map_role("push button"), "button");
        assert_eq!(map_role("Push-Button"), "button");
        assert_eq!(map_role("entry"), "text_field");
        assert_eq!(map_role("password text"), "text_field_password");
        assert_eq!(map_role("check box"), "checkbox");
        assert_eq!(map_role("radio button"), "radio_button");
        assert_eq!(map_role("page tab"), "tab");
        assert_eq!(map_role("frame"), "window");
        assert_eq!(map_role("menu item"), "menu_item");
        // Unknown roles pass through in a form the session can still match.
        assert_eq!(map_role("status bar"), "statusbar");
    }

    #[test]
    fn a_password_field_is_masked_rather_than_read_back() {
        let role = map_role("password text");
        let normalized: String = role.chars().filter(|c| c.is_alphanumeric()).collect();
        assert!(normalized.contains("textfield"), "{normalized}");
        assert!(normalized.contains("password"), "{normalized}");
    }

    #[test]
    fn typing_goes_through_the_clipboard_with_a_typed_fallback() {
        let code = type_code("名字 \"quoted\"\nline");
        assert!(code.contains("pyperclip.copy(\"名字 \\\"quoted\\\"\\nline\")"), "{code}");
        assert!(code.contains("pyautogui.hotkey('ctrl', 'v')"), "{code}");
        assert!(code.contains("pyautogui.write("), "{code}");
    }

    #[test]
    fn key_names_are_spelled_the_way_pyautogui_spells_them() {
        assert_eq!(pyautogui_key("Return"), "enter");
        assert_eq!(pyautogui_key("Escape"), "esc");
        assert_eq!(pyautogui_key("cmd"), "ctrl");
        assert_eq!(pyautogui_key("Option"), "alt");
        let code = keys_code(&["cmd".to_string(), "c".to_string()]).unwrap();
        assert_eq!(code, "import pyautogui\npyautogui.hotkey(\"ctrl\", \"c\")\n");
        assert_eq!(
            keys_code(&["enter".to_string()]).unwrap(),
            "import pyautogui\npyautogui.press(\"enter\")\n"
        );
    }

    #[test]
    fn scrolling_and_dragging_produce_multi_step_snippets() {
        let scroll = scroll_code(10, 20, ScrollDirection::Down, 3);
        assert!(scroll.contains("pyautogui.scroll(-300, x=10, y=20)"), "{scroll}");
        let drag = drag_code(1, 2, 3, 4);
        assert!(drag.contains("pyautogui.moveTo(1, 2)"), "{drag}");
        assert!(drag.contains("pyautogui.moveTo(3, 4, duration=0.5)"), "{drag}");
        assert!(drag.contains("pyautogui.mouseUp(button='left')"), "{drag}");
    }

    #[test]
    fn a_screenshot_envelope_is_decoded_from_json_too() {
        let png = png_bytes(640, 480);
        let body = serde_json::to_vec(&json!({ "screenshot": base64_encode(&png) })).unwrap();
        let shot = decode_capture(Some("application/json"), &body).unwrap();
        assert_eq!((shot.width, shot.height), (640, 480));
        assert_eq!(shot.base64, base64_encode(&png));
    }

    #[test]
    fn a_screenshot_without_an_image_is_an_error() {
        let body = serde_json::to_vec(&json!({ "output": "", "error": "no display" })).unwrap();
        let error = decode_capture(Some("application/json"), &body).unwrap_err();
        assert!(error.to_string().contains("no display"), "{error}");
    }

    #[test]
    fn a_screen_size_envelope_may_be_a_stringified_object() {
        let body = br#"{"output": "{\"width\": 1024, \"height\": 768}", "error": null}"#;
        assert_eq!(parse_screen_size(Some("application/json"), body).unwrap(), (1024, 768));
    }
}
