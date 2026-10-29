//! Cross-platform types for Physical AI desktop abstraction.
//!
//! These types hide platform differences (xdotool vs SendKeys vs AXUIElement)
//! behind a unified interface used by the Agent / Planner layers.

use serde::{Deserialize, Serialize};

/// A point on screen in logical coordinates (0-1920 range, DPI independent).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Point {
    pub x: i32,
    pub y: i32,
}

impl Point {
    pub fn new(x: i32, y: i32) -> Self {
        Self { x, y }
    }

    /// Scale this point by the given DPI factor (logical → physical).
    pub fn to_physical(&self, dpi_scale: f32) -> Self {
        Self {
            x: (self.x as f32 * dpi_scale).round() as i32,
            y: (self.y as f32 * dpi_scale).round() as i32,
        }
    }

    /// Scale this point by the given DPI factor (physical → logical).
    pub fn to_logical(&self, dpi_scale: f32) -> Self {
        Self {
            x: (self.x as f32 / dpi_scale).round() as i32,
            y: (self.y as f32 / dpi_scale).round() as i32,
        }
    }
}

/// Platform DPI scale factor for converting between logical and physical
/// coordinates.
///
/// Logical coordinates are what the Agent reasons about (e.g., "click at
/// 960, 540" on a 1920x1080 logical canvas). Physical coordinates are the
/// actual pixels on screen, which differ on HiDPI / Retina displays.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct DpiScale {
    /// Scale factor: physical = logical × scale.
    /// 1.0 = standard DPI (96 on Windows, 72 on macOS).
    /// 2.0 = Retina / HiDPI.
    pub scale: f32,
}

impl DpiScale {
    /// Standard 1:1 scale (no HiDPI).
    pub const STANDARD: Self = Self { scale: 1.0 };

    pub fn new(scale: f32) -> Self {
        Self { scale }
    }

    /// Detect the current platform's DPI scale at runtime.
    ///
    /// Returns a best-effort estimate. On some platforms this may
    /// require display-server queries (X11, Wayland, macOS, Windows).
    pub fn detect() -> Self {
        #[cfg(target_os = "macos")]
        {
            // macOS: try via CoreGraphics display scale
            Self::detect_macos()
        }
        #[cfg(target_os = "windows")]
        {
            Self::detect_windows()
        }
        #[cfg(target_os = "linux")]
        {
            Self::detect_linux()
        }
        #[cfg(not(any(target_os = "macos", target_os = "windows", target_os = "linux")))]
        {
            Self::STANDARD
        }
    }

    #[cfg(target_os = "macos")]
    fn detect_macos() -> Self {
        // macOS Retina: typically 2.0, can be 1.0 on external displays.
        // Without linking CoreGraphics, we shell out to `system_profiler`.
        if let Ok(output) = std::process::Command::new("system_profiler")
            .args(["SPDisplaysDataType", "-json"])
            .output()
        {
            if let Ok(json) = serde_json::from_slice::<serde_json::Value>(&output.stdout) {
                if let Some(displays) = json
                    .get("SPDisplaysDataType")
                    .and_then(|v| v.as_array())
                    .and_then(|arr| arr.first())
                    .and_then(|v| v.get("spdisplays_ndrvs"))
                    .and_then(|v| v.as_array())
                {
                    if let Some(first) = displays.first() {
                        if let Some(res) = first.get("_spdisplays_resolution") {
                            if let Some(res_str) = res.as_str() {
                                // "Retina" in the resolution string indicates HiDPI
                                if res_str.contains("Retina") {
                                    return Self { scale: 2.0 };
                                }
                            }
                        }
                        if let Some(pixels) = first.get("_spdisplays_pixels") {
                            if let Some(px_str) = pixels.as_str() {
                                // e.g. "3840 x 2160" vs "1920 x 1080"
                                let parts: Vec<&str> = px_str.split(" x ").collect();
                                if parts.len() == 2 {
                                    if let (Ok(px_w), Ok(px_h)) = (
                                        parts[0].trim().parse::<u32>(),
                                        parts[1].trim().parse::<u32>(),
                                    ) {
                                        // Rough heuristic: if pixel dimensions are
                                        // double common logical sizes, assume 2x
                                        if (px_w >= 3840 && px_h >= 2160)
                                            || (px_w >= 2880 && px_h >= 1800)
                                        {
                                            return Self { scale: 2.0 };
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
        Self::STANDARD
    }

    #[cfg(target_os = "windows")]
    fn detect_windows() -> Self {
        // Windows: try via PowerShell Get-CimInstance
        if let Ok(output) = std::process::Command::new("powershell")
            .args([
                "-Command",
                "Add-Type -AssemblyName System.Windows.Forms; \
                 [System.Windows.Forms.Screen]::PrimaryScreen.DeviceDpi",
            ])
            .output()
        {
            let text = String::from_utf8_lossy(&output.stdout);
            if let Ok(dpi) = text.trim().parse::<f32>() {
                if dpi > 0.0 {
                    return Self { scale: dpi / 96.0 };
                }
            }
        }
        Self::STANDARD
    }

    #[cfg(target_os = "linux")]
    fn detect_linux() -> Self {
        // Try X11 via xdpyinfo
        if let Ok(output) = std::process::Command::new("xdpyinfo").output() {
            let text = String::from_utf8_lossy(&output.stdout);
            let mut screen_w_mm = 0f32;
            let mut screen_w_px = 0f32;
            let mut found = false;
            for line in text.lines() {
                if line.contains("dimensions:") {
                    // "  dimensions:    3840x2160 pixels (1016x572 millimeters)"
                    if let Some(px_part) = line.split("pixels").next() {
                        if let Some(x) = px_part.split('x').next() {
                            if let Ok(w) = x.split_whitespace().last().unwrap_or("0").parse::<f32>()
                            {
                                screen_w_px = w;
                            }
                        }
                    }
                    if let Some(mm_part) = line.split('(').nth(1) {
                        if let Some(x) = mm_part.split('x').next() {
                            if let Ok(w) = x.trim().parse::<f32>() {
                                screen_w_mm = w;
                                found = true;
                            }
                        }
                    }
                }
            }
            if found && screen_w_mm > 0.0 {
                // DPI = pixels / inches, inches = mm / 25.4
                let dpi = (screen_w_px / (screen_w_mm / 25.4)).round();
                if dpi > 96.0 {
                    return Self {
                        scale: (dpi / 96.0).clamp(1.0, 4.0),
                    };
                }
            }
        }

        // Try Wayland via gsettings (GNOME)
        if let Ok(output) = std::process::Command::new("gsettings")
            .args(["get", "org.gnome.desktop.interface", "scaling-factor"])
            .output()
        {
            let text = String::from_utf8_lossy(&output.stdout);
            if let Ok(scale) = text.trim().replace("uint32 ", "").parse::<f32>() {
                if scale >= 1.0 {
                    return Self { scale };
                }
            }
        }

        Self::STANDARD
    }
}

/// A rectangle on screen in logical coordinates.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Rect {
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
}

impl Rect {
    pub fn new(x: i32, y: i32, width: u32, height: u32) -> Self {
        Self { x, y, width, height }
    }

    /// Scale this rect by the given DPI factor (logical → physical).
    pub fn to_physical(&self, dpi_scale: f32) -> Self {
        Self {
            x: (self.x as f32 * dpi_scale).round() as i32,
            y: (self.y as f32 * dpi_scale).round() as i32,
            width: (self.width as f32 * dpi_scale).round() as u32,
            height: (self.height as f32 * dpi_scale).round() as u32,
        }
    }

    /// Scale this rect by the given DPI factor (physical → logical).
    pub fn to_logical(&self, dpi_scale: f32) -> Self {
        Self {
            x: (self.x as f32 / dpi_scale).round() as i32,
            y: (self.y as f32 / dpi_scale).round() as i32,
            width: (self.width as f32 / dpi_scale).round() as u32,
            height: (self.height as f32 / dpi_scale).round() as u32,
        }
    }
}

/// A UI element as seen by the accessibility / UI automation layer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UiElement {
    /// Platform-specific unique identifier (may be empty if unavailable).
    pub id: String,
    /// Role: "button", "text_field", "window", "menu", "checkbox", etc.
    pub role: String,
    /// Human-readable label / text.
    pub label: Option<String>,
    /// Current value (for inputs, checkboxes, sliders).
    pub value: Option<String>,
    /// Bounding box in logical coordinates.
    pub bounds: Rect,
    /// Whether the element is enabled for interaction.
    pub enabled: bool,
    /// Whether the element currently has keyboard focus.
    pub focused: bool,
    /// Child elements (tree structure).
    pub children: Vec<UiElement>,
}

/// Parse a UI element from accessibility tool JSON output.
/// Handles the format produced by platform accessibility tools:
/// `{ role, name, value, enabled, x, y, width, height, children }`.
pub fn ui_element_from_accessibility_json(value: &serde_json::Value) -> Option<UiElement> {
    let obj = value.as_object()?;
    let role = obj.get("role")?.as_str()?.to_string();
    let name = obj.get("name")?.as_str().unwrap_or("").to_string();
    let x = obj.get("x")?.as_i64()? as i32;
    let y = obj.get("y")?.as_i64()? as i32;
    let width = obj.get("width")?.as_i64()? as u32;
    let height = obj.get("height")?.as_i64()? as u32;
    let enabled = obj.get("enabled")?.as_bool().unwrap_or(true);
    let value = obj.get("value").and_then(|v| v.as_str()).map(String::from);

    let children = obj
        .get("children")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(ui_element_from_accessibility_json)
                .collect()
        })
        .unwrap_or_default();

    Some(UiElement {
        // `AutomationId` when the application publishes one; snapshot-path
        // ids are filled in by `element_index::assign_ids` for the rest.
        id: obj
            .get("identifier")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string(),
        role,
        label: if name.is_empty() { None } else { Some(name) },
        value,
        bounds: Rect::new(x, y, width, height),
        enabled,
        focused: false,
        children,
    })
}

/// Parse a list of UI elements from accessibility tool result data.
pub fn parse_accessibility_elements(data: Option<&serde_json::Value>) -> Vec<UiElement> {
    data.and_then(|d| d.get("elements"))
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(ui_element_from_accessibility_json)
                .collect()
        })
        .unwrap_or_default()
}

impl UiElement {
    /// Find the first element matching the given role (depth-first).
    pub fn find_by_role(&self, role: &str) -> Option<&UiElement> {
        if self.role == role {
            return Some(self);
        }
        for child in &self.children {
            if let found @ Some(_) = child.find_by_role(role) {
                return found;
            }
        }
        None
    }

    /// Find the first element whose label contains the given text.
    pub fn find_by_label_contains(&self, text: &str) -> Option<&UiElement> {
        if self
            .label
            .as_ref()
            .map(|l| l.contains(text))
            .unwrap_or(false)
        {
            return Some(self);
        }
        for child in &self.children {
            if let found @ Some(_) = child.find_by_label_contains(text) {
                return found;
            }
        }
        None
    }

    /// Compute the center point of this element's bounds.
    pub fn center(&self) -> Point {
        Point::new(
            self.bounds.x + self.bounds.width as i32 / 2,
            self.bounds.y + self.bounds.height as i32 / 2,
        )
    }
}

/// A captured screenshot.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Screenshot {
    /// Base64-encoded PNG image.
    #[serde(skip_serializing_if = "String::is_empty")]
    pub base64: String,
    /// Width in pixels.
    pub width: u32,
    /// Height in pixels.
    pub height: u32,
    /// File path on disk (if screenshot was persisted). Preferred over base64
    /// for large images to avoid serialization overhead.
    #[serde(skip)]
    pub file_path: Option<std::path::PathBuf>,
    /// Wall-clock capture timestamp. Skipped in serialization since Instant is
    /// not natively supported by serde.
    #[serde(skip, default = "instant_now")]
    pub timestamp: std::time::Instant,
}

impl Screenshot {
    /// Construct with an in-memory base64 payload (no file backing).
    pub fn new(base64: String, width: u32, height: u32) -> Self {
        Self {
            base64,
            width,
            height,
            file_path: None,
            timestamp: std::time::Instant::now(),
        }
    }

    /// Construct with a file path reference (no base64 payload).
    pub fn with_file(file_path: std::path::PathBuf, width: u32, height: u32) -> Self {
        Self {
            base64: String::new(),
            width,
            height,
            file_path: Some(file_path),
            timestamp: std::time::Instant::now(),
        }
    }
}

/// Serde helper: default value for `#[serde(skip)]` timestamp fields.
fn instant_now() -> std::time::Instant {
    std::time::Instant::now()
}

/// Mouse button for click / drag actions.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MouseButton {
    Left,
    Middle,
    Right,
}

/// Scroll direction.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ScrollDirection {
    Up,
    Down,
    Left,
    Right,
}

/// How to locate a click target.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ClickTarget {
    /// Absolute logical coordinate.
    Coordinate(Point),
    /// Platform-specific accessibility element ID.
    ElementId(String),
    /// Find element by label text (partial match).
    ElementLabel(String),
    /// Find element by role, then by label.
    ElementRoleLabel { role: String, label: String },
}

/// A desktop action the agent wants to perform.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DesktopAction {
    /// Capture a screenshot (optionally of a sub-region).
    Screenshot { region: Option<Rect> },
    /// Click somewhere on screen.
    Click {
        target: ClickTarget,
        button: MouseButton,
    },
    /// Double-click.
    DoubleClick {
        target: ClickTarget,
        button: MouseButton,
    },
    /// Type text (as if typed by keyboard).
    ///
    /// `target` names the field to type into when the caller has one. The
    /// session then prefers to set the field's value through accessibility
    /// (`AXSetValue` / UIA `ValuePattern`) — no focus, no keystroke, no race —
    /// and falls back to focusing it and typing, and finally to whatever holds
    /// focus. Without a target there is nothing to set, so it types blind.
    Type {
        text: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        target: Option<ClickTarget>,
    },
    /// Press key combination (e.g. ["ctrl", "c"]).
    KeyPress { keys: Vec<String> },
    /// Scroll at a location.
    Scroll {
        target: ClickTarget,
        direction: ScrollDirection,
        amount: i32,
    },
    /// Drag from one point to another.
    Drag { from: ClickTarget, to: ClickTarget },
    /// Read the accessibility UI tree of the active (or named) app.
    ReadUiTree { app: Option<String> },
    /// Launch an application.
    LaunchApp {
        name: String,
        args: Vec<String>,
        /// Wait for the app to be ready (window appears).
        wait_for_ready: bool,
    },
    /// Activate / focus a window by title pattern.
    ActivateWindow { title_pattern: String },
    /// Close a window by title pattern.
    CloseWindow { title_pattern: String },
    /// List all visible windows on the desktop.
    ListWindows,
    /// Get the geometry (position + size) of a specific window.
    GetWindowGeometry { title_pattern: String },
    /// Move a window to an absolute screen position.
    MoveWindow {
        title_pattern: String,
        x: i32,
        y: i32,
    },
    /// Resize a window to an absolute width and height.
    ResizeWindow {
        title_pattern: String,
        width: u32,
        height: u32,
    },
    /// Minimize (iconify) a window.
    MinimizeWindow { title_pattern: String },
    /// Maximize (zoom) a window.
    MaximizeWindow { title_pattern: String },
    /// Wait for a duration.
    Wait { milliseconds: u64 },
    /// Read clipboard.
    ClipboardGet,
    /// Write to clipboard.
    ClipboardSet { text: String },
    /// Query system resource status (CPU, memory, disk, network, uptime).
    GetSystemStatus,
    /// List running processes, optionally filtered by name.
    ListProcesses {
        filter: Option<String>,
        limit: Option<usize>,
    },
    /// Kill a process by PID or name.
    KillProcess {
        pid: Option<u32>,
        name: Option<String>,
        force: bool,
    },
    /// Restart a process by PID or name.
    RestartProcess {
        pid: Option<u32>,
        name: Option<String>,
        force: bool,
    },
    /// Set process priority (nice value on Unix, priority class on Windows).
    SetProcessPriority {
        pid: Option<u32>,
        name: Option<String>,
        /// Unix nice value: -20 (highest) to 19 (lowest).
        /// Windows priority class: 0=Idle, 1=BelowNormal, 2=Normal,
        /// 3=AboveNormal, 4=High, 5=Realtime.
        priority: i32,
    },
}

/// A file entry returned by BrowseFiles.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileEntry {
    pub path: String,
    pub name: String,
    pub size_bytes: u64,
    pub modified_secs: u64,
    pub is_directory: bool,
}

/// Which rung of the action ladder produced a result.
///
/// The ladder is about *how the action was aimed*, not about how the screen
/// was captured: the same snapshot supports an accessibility action on an
/// element and a click on its center.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Rung {
    /// Accessibility action on the element itself
    /// (`AXPress` / UIA `Invoke` / AT-SPI `doAction`). Independent of
    /// z-order and of where the window happens to be.
    AxAction,
    /// A click at the element's center, or at a coordinate.
    Pixel,
    /// Keyboard: focus the element, then drive it with keys.
    Keyboard,
    /// The page itself, through the browser's protocol: the element is found
    /// in the document by role and name instead of by picture or by focus.
    /// Only web content has this rung.
    Page,
}

/// A native accessibility action to perform on an element.
///
/// These map onto the platform's own action vocabulary: `AXPress` /
/// UIA `Invoke` / AT-SPI `doAction("click")`, and the value-setting action
/// that lets a text field be filled without keystrokes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AxAction {
    /// Press the element as its control type understands it.
    Press,
    /// Set the element's value (text fields, editable combos).
    SetValue { value: String },
    /// Give the element keyboard focus.
    Focus,
}

impl AxAction {
    /// Short name, for evidence and logs.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Press => "press",
            Self::SetValue { .. } => "set_value",
            Self::Focus => "focus",
        }
    }
}

/// What is known about whether an action took effect.
///
/// Five values, because fewer cannot hold the truth. `Confirmed` is reserved
/// for an effect that was actually observed: an accessibility action that
/// reported no error, or a screen that changed as intended. A pixel click that
/// "succeeded" in the sense that the pointer moved is `Unverifiable`.
///
/// `Partial` and `Refused` separate two failures that used to look alike:
/// an action that was delivered but only partly applied (the click landed in
/// the field, only some of the text arrived) must not be retried blindly —
/// retrying compounds it — while one the platform refused never ran at all,
/// and needs the *caller* to change something (a fresh element id, a
/// permission, a different rung) rather than another attempt.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ActionEffect {
    /// The effect was observed.
    Confirmed,
    /// Nothing was established: no read-back was possible.
    Unverifiable,
    /// The action was delivered but the screen did not change when it should
    /// have.
    SuspectedNoop,
    /// The action was delivered and something changed, but not all of what
    /// was asked for — inspect before retrying, or the retry doubles it.
    Partial,
    /// The action never ran: the platform rejected it (stale element,
    /// missing permission, unsupported control). The caller must change the
    /// request; repeating it produces the same refusal.
    Refused,
}

impl ActionEffect {
    /// Machine-readable effect name, for the tool result payload.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Confirmed => "confirmed",
            Self::Unverifiable => "unverifiable",
            Self::SuspectedNoop => "suspected_noop",
            Self::Partial => "partial",
            Self::Refused => "refused",
        }
    }

    /// Whether this effect means the caller should try a higher rung.
    ///
    /// Not for `Partial` or `Refused`: a partial action already changed
    /// something (escalating would apply it twice), and a refused one was
    /// never delivered (escalating re-sends the same refused request).
    pub fn should_escalate(self) -> bool {
        matches!(self, Self::SuspectedNoop | Self::Unverifiable)
    }

    /// Whether the action needs a human/model decision before anything else
    /// happens: the screen may now be in a half-applied state.
    pub fn needs_inspection(self) -> bool {
        matches!(self, Self::Partial | Self::Refused)
    }
}

/// The next rung to try when an action did not take.
///
/// Mirrors the escalation hints Cua emits: a pixel rung is offered when an
/// accessibility action failed, a foreground rung when a pixel click may have
/// gone to the wrong window, and a page rung when neither worked and the
/// caller should re-observe instead of retrying blindly.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Escalation {
    /// Retry aimed at the element's pixels.
    Px,
    /// Bring the window to the front, then retry.
    Foreground,
    /// Stop retrying this action; re-read the page/tree.
    Page,
}

/// How input may be delivered to the machine.
///
/// Accessibility actions and the page rung are background by nature: they act
/// on an element, not on the pointer or the focus. The input-layer rungs
/// (pixel, keyboard) are not, and on a desktop someone else is using they are
/// the difference between doing the work and taking over the screen. A
/// session asked for `Background` must refuse an input-layer action that has
/// no background mechanism rather than quietly deliver it in the foreground.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DeliveryMode {
    /// Deliver wherever the action lands: the pointer may move and the focus
    /// may change.
    #[default]
    Foreground,
    /// Deliver only if the action can reach the app without taking the
    /// foreground; otherwise refuse and say why.
    Background,
}

/// Evidence attached to an action result: which rung ran and what it
/// established.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ActionEvidence {
    pub rung: Rung,
    pub effect: ActionEffect,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub escalation: Option<Escalation>,
    /// Rungs that were tried and rejected, lowest first.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tried: Vec<Rung>,
    /// What the caller must change for a refused (or half-applied) action to
    /// succeed — e.g. "re-read the UI tree and use a fresh element id".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recovery: Option<String>,
}

impl ActionEvidence {
    pub fn new(rung: Rung, effect: ActionEffect) -> Self {
        Self {
            rung,
            effect,
            escalation: None,
            tried: Vec::new(),
            recovery: None,
        }
    }

    pub fn with_recovery(mut self, recovery: impl Into<String>) -> Self {
        self.recovery = Some(recovery.into());
        self
    }

    pub fn with_escalation(mut self, escalation: Escalation) -> Self {
        self.escalation = Some(escalation);
        self
    }

    pub fn with_tried(mut self, tried: Vec<Rung>) -> Self {
        self.tried = tried;
        self
    }

    /// One-line rendering for the tool output the model reads.
    pub fn summary(&self) -> String {
        let mut line = format!("rung={} effect={}", rung_name(self.rung), self.effect.as_str());
        if !self.tried.is_empty() {
            let previous: Vec<&str> = self.tried.iter().map(|r| rung_name(*r)).collect();
            line.push_str(&format!(" tried={}", previous.join(",")));
        }
        if let Some(next) = self.escalation {
            let next = match next {
                Escalation::Px => "px",
                Escalation::Foreground => "foreground",
                Escalation::Page => "page",
            };
            line.push_str(&format!(" escalate={next}"));
        }
        if let Some(recovery) = &self.recovery {
            line.push_str(&format!(" recovery={recovery}"));
        }
        line
    }
}

fn rung_name(rung: Rung) -> &'static str {
    match rung {
        Rung::AxAction => "ax_action",
        Rung::Pixel => "pixel",
        Rung::Keyboard => "keyboard",
        Rung::Page => "page",
    }
}

/// Result of executing a desktop action.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ActionResult {
    pub success: bool,
    pub message: String,
    /// Screenshot taken after the action (for verification).
    pub screenshot_after: Option<Screenshot>,
    /// Structured data (e.g. UI tree after the action).
    pub data: Option<serde_json::Value>,
    /// Which rung ran and what it established, when the adapter can say.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub evidence: Option<ActionEvidence>,
}

impl ActionResult {
    pub fn success(message: impl Into<String>) -> Self {
        Self {
            success: true,
            message: message.into(),
            screenshot_after: None,
            data: None,
            evidence: None,
        }
    }

    pub fn error(message: impl Into<String>) -> Self {
        Self {
            success: false,
            message: message.into(),
            screenshot_after: None,
            data: None,
            evidence: None,
        }
    }

    pub fn with_screenshot(mut self, screenshot: Screenshot) -> Self {
        self.screenshot_after = Some(screenshot);
        self
    }

    pub fn with_data(mut self, data: serde_json::Value) -> Self {
        self.data = Some(data);
        self
    }

    pub fn with_evidence(mut self, evidence: ActionEvidence) -> Self {
        self.evidence = Some(evidence);
        self
    }
}

/// Condition to wait for.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WaitCondition {
    /// Wait for an element with the given role to appear in the UI tree.
    UiTreeContains { role: String, label: Option<String> },
    /// Wait for the window title to contain a pattern.
    WindowTitleContains { pattern: String },
    /// Wait for a process with the given name to be running.
    ProcessRunning { name: String },
    /// Wait for a process with the given name to exit.
    ProcessExited { name: String },
    /// Wait for the screenshot to differ from the baseline by less than a
    /// threshold.
    ScreenshotStable {
        max_pixel_diff: u32,
        timeout_ms: u64,
    },
    /// Wait for a file to appear.
    FileExists { path: String },
}

/// Cross-platform system resource snapshot.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SystemStatus {
    pub hostname: String,
    pub os_name: String,
    pub os_version: String,
    pub kernel_version: String,
    pub uptime_seconds: u64,
    pub cpu_usage_percent: f32,
    pub cpu_count: usize,
    pub memory_total_mb: u64,
    pub memory_used_mb: u64,
    pub memory_available_mb: u64,
    pub swap_total_mb: u64,
    pub swap_used_mb: u64,
    pub disks: Vec<DiskStatus>,
    pub networks: Vec<NetworkStatus>,
    /// Wall-clock snapshot timestamp. Skipped in serialization since Instant is
    /// not natively supported by serde.
    #[serde(skip, default = "instant_now")]
    pub timestamp: std::time::Instant,
}

/// Disk usage information.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiskStatus {
    pub name: String,
    pub mount_point: String,
    pub total_gb: f64,
    pub used_gb: f64,
    pub available_gb: f64,
}

/// Network interface statistics.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NetworkStatus {
    pub name: String,
    pub rx_bytes: u64,
    pub tx_bytes: u64,
}

/// A single running process entry.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProcessEntry {
    pub pid: u32,
    pub name: String,
    pub cpu_percent: f32,
    pub memory_mb: u64,
    pub status: String,
    pub start_time: Option<String>,
}

/// A window discovered on the desktop.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WindowInfo {
    /// Platform-specific window identifier.
    pub id: String,
    /// Human-readable window title.
    pub title: String,
    /// Application / process name that owns this window.
    pub app_name: Option<String>,
    /// Process ID of the owning process.
    pub pid: Option<u32>,
    /// Current position and size.
    pub bounds: Option<Rect>,
    /// Whether the window is minimized / iconic.
    pub minimized: bool,
    /// Whether the window is maximized / zoomed.
    pub maximized: bool,
    /// Whether this window is the currently focused window.
    pub focused: bool,
}
