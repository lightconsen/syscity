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

/// A three-state answer about one element property.
///
/// Two states cannot hold the truth about an accessibility attribute: a read
/// that failed is not the same answer as a read that said "no", and folding
/// them together invents a fact. `Unknown` is the property this type exists
/// for — the platform did not tell us, and the caller must decide what that
/// means for *its* question (skip a rung, refuse, or proceed and say so).
///
/// `Absent` is an answer, not a gap: the element does not publish this
/// property (a static label has no `enabled`), so a gate that asks "is it
/// disabled?" is satisfied — nothing says it is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Tri {
    /// The property was read, and has this value.
    Known(bool),
    /// The element does not publish this property.
    Absent,
    /// The read failed, or the platform does not expose the property at all.
    Unknown,
}

impl std::fmt::Display for Tri {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Known(value) => write!(f, "{value}"),
            Self::Absent => write!(f, "absent"),
            Self::Unknown => write!(f, "unknown"),
        }
    }
}

impl Tri {
    /// The value when the read succeeded.
    pub fn known(self) -> Option<bool> {
        match self {
            Self::Known(value) => Some(value),
            Self::Absent | Self::Unknown => None,
        }
    }

    /// Whether this answer positively says the gate would fail.
    ///
    /// Only `Known(false)` blocks: `Absent` means the question does not apply
    /// to this element, and `Unknown` means it was never asked.
    pub fn is_false(self) -> bool {
        matches!(self, Self::Known(false))
    }

    /// Whether anything at all is known.
    pub fn is_known(self) -> bool {
        matches!(self, Self::Known(_))
    }
}

impl From<Option<bool>> for Tri {
    /// `None` is "the platform did not answer" — `Unknown`, never `Absent`:
    /// a parser that returns `None` has not established that the element
    /// declines to publish the property.
    fn from(value: Option<bool>) -> Self {
        match value {
            Some(value) => Self::Known(value),
            None => Self::Unknown,
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
    /// Bounding box in logical coordinates, when the platform published one.
    ///
    /// `None` is a read failure, and is deliberately not (0,0,0,0): an element
    /// with no geometry cannot be clicked, and pretending it sits at the
    /// origin makes it look like an invisible-but-valid target.
    pub bounds: Option<Rect>,
    /// Whether the element is enabled for interaction.
    pub enabled: Tri,
    /// Whether the element currently has keyboard focus.
    pub focused: Tri,
    /// The actions the element itself says it supports (`AXActions` on macOS,
    /// `GetSupportedPatterns()` on Windows, the AT-SPI action names).
    ///
    /// `None` means the platform did not report them; `Some(vec![])` means it
    /// answered "none", which is a different and more useful fact. This is
    /// what the accessibility rung gates on — see
    /// [`UiElement::supports_ax_action`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub actions: Option<Vec<String>>,
    /// Child elements (tree structure).
    pub children: Vec<UiElement>,
    /// How many children the platform said this element has, when it said.
    ///
    /// This is what makes "the tree is complete" a decidable proposition
    /// rather than a hope: a node whose `children_count` is larger than
    /// `children.len()` was cut by the walker's depth or line budget, and a
    /// caller looking for an element under it is not looking at everything.
    /// `None` means the platform did not report a count — which is not the
    /// same as "no children".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub children_count: Option<u32>,
    /// Whether this node's children were cut short by the read.
    ///
    /// Distinct from a short `children` list: an element can have no children
    /// *and* be complete. Only a platform that knows it stopped early sets
    /// this; for trees read through a budget the walker stamps it itself.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub truncated: bool,
}

/// Parse a UI element from accessibility tool JSON output.
/// Handles the format produced by platform accessibility tools:
/// `{ role, name, value, enabled, x, y, width, height, actions, children }`.
///
/// A missing coordinate or an unreadable `enabled` no longer drops the element
/// or invents a value: the geometry becomes `None` and the flag `Unknown`, so
/// the element still exists and still says what the platform did not answer.
/// Only `role` is required — an element with no role cannot be described at
/// all.
pub fn ui_element_from_accessibility_json(value: &serde_json::Value) -> Option<UiElement> {
    let obj = value.as_object()?;
    let role = obj.get("role")?.as_str()?.to_string();
    let name = obj.get("name").and_then(|v| v.as_str()).unwrap_or("");
    let x = obj.get("x").and_then(|v| v.as_i64()).map(|v| v as i32);
    let y = obj.get("y").and_then(|v| v.as_i64()).map(|v| v as i32);
    let width = obj
        .get("width")
        .and_then(|v| v.as_i64())
        .map(|v| v.max(0) as u32);
    let height = obj
        .get("height")
        .and_then(|v| v.as_i64())
        .map(|v| v.max(0) as u32);
    let bounds = match (x, y, width, height) {
        (Some(x), Some(y), Some(width), Some(height)) => Some(Rect::new(x, y, width, height)),
        _ => None,
    };
    let enabled = match obj.get("enabled") {
        // A key that is present but not a boolean is a read failure, not a
        // silent `true`: the caller decides, and `Unknown` says so.
        Some(value) => Tri::from(value.as_bool()),
        None => Tri::Unknown,
    };
    let focused = Tri::from(obj.get("focused").and_then(|v| v.as_bool()));
    let value = obj.get("value").and_then(|v| v.as_str()).map(String::from);
    let actions = obj.get("actions").and_then(|v| v.as_array()).map(|arr| {
        arr.iter()
            .filter_map(|v| v.as_str().map(String::from))
            .collect()
    });

    let children = obj
        .get("children")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(ui_element_from_accessibility_json)
                .collect()
        })
        .unwrap_or_default();

    // A platform that reports how many children it has, and reports a
    // `truncated` flag, is the only one that can make "the tree is complete"
    // decidable. Both are optional, and neither is invented.
    let children_count = obj
        .get("children_count")
        .and_then(|v| v.as_u64())
        .map(|v| v.min(u32::MAX as u64) as u32);
    let truncated = obj
        .get("truncated")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);

    Some(UiElement {
        // `AutomationId` when the application publishes one; snapshot-path
        // ids are filled in by `element_index::assign_ids` for the rest.
        id: obj
            .get("identifier")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string(),
        role,
        label: if name.is_empty() {
            None
        } else {
            Some(name.to_string())
        },
        value,
        bounds,
        enabled,
        focused,
        actions,
        children,
        children_count,
        truncated,
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

/// The error a failed accessibility read should surface as.
///
/// A read can fail three ways, and they are not interchangeable:
///
/// - the OS refused it (Accessibility permission missing) — retrying is
///   pointless until a human grants it;
/// - it ran out of time — the tree was too deep or the platform too slow,
///   and a shallower read or a retry may work;
/// - it said something else — a spawn error, malformed output.
///
/// Reporting all three as "no permission" sent callers to the permission pane
/// for a tree that was merely too deep, which is the macOS bug this replaces.
pub fn accessibility_read_error(
    data: Option<&serde_json::Value>,
    message: &str,
    timed_out: bool,
) -> crate::computer::ComputerError {
    let needs_permission = data
        .and_then(|d| d.get("needs_permission"))
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    if needs_permission {
        crate::computer::ComputerError::AccessibilityDenied
    } else if timed_out || message.to_ascii_lowercase().contains("timed out") {
        crate::computer::ComputerError::Timeout
    } else {
        crate::computer::ComputerError::ToolFailed(message.to_string())
    }
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

    /// Compute the center point of this element's bounds, when it has any.
    ///
    /// `None` for an element whose geometry could not be read: aiming a click
    /// at a made-up point is worse than saying the element cannot be clicked.
    pub fn center(&self) -> Option<Point> {
        let bounds = self.bounds?;
        Some(Point::new(
            bounds.x + bounds.width as i32 / 2,
            bounds.y + bounds.height as i32 / 2,
        ))
    }

    /// Whether this node's children are known to be cut short.
    ///
    /// True when the read said so, or when the platform reported more children
    /// than it handed over. Both mean the same thing to a caller: an element
    /// that cannot be found here may still exist below.
    pub fn children_incomplete(&self) -> bool {
        self.truncated
            || self
                .children_count
                .is_some_and(|count| (count as usize) > self.children.len())
    }

    /// Whether this subtree and all of its descendants are complete.
    pub fn subtree_is_complete(&self) -> bool {
        !self.children_incomplete() && self.children.iter().all(UiElement::subtree_is_complete)
    }

    /// Whether every element in `roots` was read in full.
    ///
    /// This is the proposition a resolution result needs: "not found" is only
    /// conclusive when the tree that was searched was whole.
    pub fn tree_is_complete(roots: &[UiElement]) -> bool {
        roots.iter().all(UiElement::subtree_is_complete)
    }

    /// Whether the element says it supports this accessibility action.
    ///
    /// The answer is three-state because the platform's answer is: an element
    /// that does not publish its action list is not an element that supports
    /// nothing. `Absent` is the only answer that closes the rung; `Unknown`
    /// leaves the decision to the caller, which today means "try it anyway" —
    /// the same behaviour as before this gate existed.
    pub fn supports_ax_action(&self, action: &AxAction) -> Tri {
        let Some(actions) = self.actions.as_ref() else {
            return Tri::Unknown;
        };
        let wanted = action.declared_names();
        let supported = actions.iter().any(|declared| {
            wanted
                .iter()
                .any(|name| declared.eq_ignore_ascii_case(name))
        });
        Tri::Known(supported)
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

    /// The names a platform may publish for "this element supports me".
    ///
    /// Three vocabularies answer the same question: macOS `AXActions`
    /// (`AXPress`, `AXValueChanged`), UIA patterns (`Invoke`, `Value`,
    /// `SelectionItem`), and AT-SPI action names (`click`, `set-value`,
    /// `grab-focus`). Matching is case-insensitive and ignores punctuation,
    /// because each platform spells its own list differently.
    pub fn declared_names(&self) -> &'static [&'static str] {
        match self {
            Self::Press => &[
                // macOS
                "AXPress",
                // UIA patterns
                "Invoke",
                "Toggle",
                "ExpandCollapse",
                "SelectionItem",
                // AT-SPI
                "click",
                "press",
                "activate",
                "toggle",
            ],
            Self::SetValue { .. } => &[
                // macOS
                "AXValueChanged",
                "AXConfirm",
                // UIA patterns
                "Value",
                "ExpandCollapse",
                "Text",
                // AT-SPI
                "set-value",
                "set_text",
                "set_text_contents",
            ],
            Self::Focus => &[
                // macOS
                "AXFocused",
                // UIA patterns
                "SelectionItem",
                "Value",
                // AT-SPI
                "grab-focus",
                "grab_focus",
            ],
        }
    }
}

/// What is known about whether an action reached the element at all.
///
/// Separate from [`ActionEffect`], which answers a different question. A
/// platform can deliver an action reliably and still tell you nothing about
/// what happened there — that is `DeliveredUnverified`, and it is the honest
/// answer for an accessibility action that returned no error but was never
/// observed. Collapsing it into `Confirmed` is how "sent" quietly becomes
/// "done".
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DeliveryDisposition {
    /// Nothing was established: the rung could not run, or did not report.
    #[default]
    Unknown,
    /// The action never reached the element.
    NotDelivered,
    /// The request was sent and the platform's answer cannot be trusted.
    DeliveryUncertain,
    /// The action reached the element but its effect was not observed.
    DeliveredUnverified,
    /// The action reached the element and its effect was observed.
    DeliveredVerified,
}

impl DeliveryDisposition {
    /// Machine-readable name, for the tool result payload.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Unknown => "unknown",
            Self::NotDelivered => "not_delivered",
            Self::DeliveryUncertain => "delivery_uncertain",
            Self::DeliveredUnverified => "delivered_unverified",
            Self::DeliveredVerified => "delivered_verified",
        }
    }

    /// Whether repeating the action is safe.
    ///
    /// Only a delivery that never happened is safe to repeat. Anything that
    /// reached the element may or may not have taken, so a retry either
    /// doubles the effect or is pointless.
    pub fn retry(self) -> RetryDisposition {
        match self {
            Self::NotDelivered => RetryDisposition::Safe,
            Self::Unknown => RetryDisposition::Unknown,
            Self::DeliveryUncertain | Self::DeliveredUnverified | Self::DeliveredVerified => {
                RetryDisposition::Unsafe
            }
        }
    }

    /// Whether the effect was observed.
    pub fn verified(self) -> bool {
        matches!(self, Self::DeliveredVerified)
    }
}

/// Whether the caller may repeat an action that did not visibly succeed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RetryDisposition {
    /// Nothing is known about delivery, so a retry is a guess.
    #[default]
    Unknown,
    /// The action never reached anything; repeating it is safe.
    Safe,
    /// The action may have taken; repeating it compounds the effect.
    Unsafe,
}

impl RetryDisposition {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Unknown => "unknown",
            Self::Safe => "safe",
            Self::Unsafe => "unsafe",
        }
    }

    pub fn is_safe(self) -> bool {
        matches!(self, Self::Safe)
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
    /// Whether the action reached the element, as the driver reported it.
    ///
    /// Kept apart from `effect` because the platform usually knows this and
    /// rarely knows the effect: "delivered, unverified" must not be told as
    /// "confirmed", and must not be told as "failed" either.
    #[serde(default)]
    pub delivery: DeliveryDisposition,
    /// Whether repeating the action is safe — derived from `delivery`, unless
    /// the adapter explicitly overrides it.
    #[serde(default)]
    pub retry: RetryDisposition,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub escalation: Option<Escalation>,
    /// Rungs that were tried and rejected, lowest first.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tried: Vec<Rung>,
    /// What the caller must change for a refused (or half-applied) action to
    /// succeed — e.g. "re-read the UI tree and use a fresh element id".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recovery: Option<String>,
    /// The element that was in the way, when the platform could name it.
    ///
    /// Present when a hit test found something other than the target at the
    /// point the action would land on. This is the difference between "the
    /// click did nothing" and "the click went to the dialog on top".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub occluder: Option<Occluder>,
}

/// Something other than the target that receives events at the action point.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Occluder {
    /// Role of the element that would receive the click / key event.
    pub role: String,
    /// Its accessibility label, when it has one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    /// Where it is.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bounds: Option<Rect>,
}

/// What the platform's hit test says about a point.
///
/// This is the answer to "what would receive a click here", which is a
/// different question from "where is the element I named". An action aimed at
/// a point is only the action the caller intended when the two are the same
/// element, and only the platform can say whether they are.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HitTestResult {
    /// The target itself receives the event.
    ReachesTarget,
    /// Something else does; it is named so the refusal can say what.
    InterceptedBy(Occluder),
    /// The platform could not answer — no hit test, no permission, or the
    /// point is outside every window. Never treated as "clear": an answer
    /// nobody got is not a yes.
    Unknown,
}

impl HitTestResult {
    /// The occluder, when the point was intercepted.
    pub fn occluder(&self) -> Option<&Occluder> {
        match self {
            Self::InterceptedBy(occluder) => Some(occluder),
            _ => None,
        }
    }

    /// Whether this result positively says the point is clear.
    ///
    /// Only `ReachesTarget` does. `Unknown` is not clearance.
    pub fn is_clear(&self) -> bool {
        matches!(self, Self::ReachesTarget)
    }
}

impl Occluder {
    pub fn new(role: impl Into<String>, label: Option<String>) -> Self {
        Self {
            role: role.into(),
            label,
            bounds: None,
        }
    }

    pub fn with_bounds(mut self, bounds: Rect) -> Self {
        self.bounds = Some(bounds);
        self
    }

    /// One-line rendering, for evidence summaries.
    pub fn summary(&self) -> String {
        match self.label.as_deref().filter(|l| !l.is_empty()) {
            Some(label) => format!("{} {:?}", self.role, label),
            None => self.role.clone(),
        }
    }
}

impl ActionEvidence {
    pub fn new(rung: Rung, effect: ActionEffect) -> Self {
        Self {
            rung,
            effect,
            delivery: DeliveryDisposition::Unknown,
            retry: RetryDisposition::Unknown,
            escalation: None,
            tried: Vec::new(),
            recovery: None,
            occluder: None,
        }
    }

    /// Record how far the action got, and derive retry safety from it.
    pub fn with_delivery(mut self, delivery: DeliveryDisposition) -> Self {
        self.delivery = delivery;
        self.retry = delivery.retry();
        self
    }

    /// An adapter that knows better than the delivery table may say so.
    pub fn with_retry(mut self, retry: RetryDisposition) -> Self {
        self.retry = retry;
        self
    }

    pub fn with_occluder(mut self, occluder: Occluder) -> Self {
        self.occluder = Some(occluder);
        self
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
        if self.delivery != DeliveryDisposition::Unknown {
            line.push_str(&format!(" delivered={}", self.delivery.as_str()));
            line.push_str(&format!(" retry={}", self.retry.as_str()));
        }
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
        if let Some(occluder) = &self.occluder {
            line.push_str(&format!(" occluded_by={}", occluder.summary()));
        }
        if let Some(recovery) = &self.recovery {
            line.push_str(&format!(" recovery={recovery}"));
        }
        line
    }

    /// The rung's stable name.
    pub fn rung_name(&self) -> &'static str {
        rung_name(self.rung)
    }

    /// The stable name of any rung.
    pub fn rung_of(rung: Rung) -> &'static str {
        rung_name(rung)
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

fn escalation_name(escalation: Escalation) -> &'static str {
    match escalation {
        Escalation::Px => "px",
        Escalation::Foreground => "foreground",
        Escalation::Page => "page",
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

    /// This result, with everything it established in `data`.
    ///
    /// The model does not read the typed fields — it reads the tool result's
    /// text and, when the harness lets it through, the `data` object. The
    /// structured evidence is written here so `ActionEvidence`'s machine
    /// vocabulary survives the trip to the caller.
    pub fn with_evidence(mut self, evidence: ActionEvidence) -> Self {
        let data = self.data.get_or_insert_with(|| serde_json::json!({}));
        if let Some(obj) = data.as_object_mut() {
            obj.insert("rung".to_string(), serde_json::json!(rung_name(evidence.rung)));
            obj.insert("effect".to_string(), serde_json::json!(evidence.effect.as_str()));
            obj.insert("delivered".to_string(), serde_json::json!(evidence.delivery.as_str()));
            obj.insert("retry".to_string(), serde_json::json!(evidence.retry.as_str()));
            if let Some(next) = evidence.escalation {
                obj.insert("escalation".to_string(), serde_json::json!(escalation_name(next)));
            }
            if !evidence.tried.is_empty() {
                obj.insert(
                    "tried".to_string(),
                    serde_json::json!(evidence
                        .tried
                        .iter()
                        .map(|r| rung_name(*r))
                        .collect::<Vec<_>>()),
                );
            }
            if let Some(recovery) = &evidence.recovery {
                obj.insert("recovery".to_string(), serde_json::json!(recovery));
            }
            if let Some(occluder) = &evidence.occluder {
                obj.insert(
                    "occluder".to_string(),
                    serde_json::to_value(occluder).unwrap_or_default(),
                );
            }
        }
        self.evidence = Some(evidence);
        self
    }

    pub fn with_screenshot(mut self, screenshot: Screenshot) -> Self {
        self.screenshot_after = Some(screenshot);
        self
    }

    pub fn with_data(mut self, data: serde_json::Value) -> Self {
        self.data = Some(data);
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_missing_permission_is_not_a_timeout() {
        let data = serde_json::json!({ "needs_permission": true });
        let err = accessibility_read_error(Some(&data), "timed out", true);
        assert!(matches!(err, crate::computer::ComputerError::AccessibilityDenied), "{err:?}");
    }

    #[test]
    fn a_read_that_ran_out_of_time_is_a_timeout() {
        let err = accessibility_read_error(None, "osascript timed out after 15 seconds", false);
        assert!(matches!(err, crate::computer::ComputerError::Timeout), "{err:?}");
        // Many producers only put the fact in the flag.
        let err = accessibility_read_error(None, "", true);
        assert!(matches!(err, crate::computer::ComputerError::Timeout), "{err:?}");
    }

    #[test]
    fn any_other_read_failure_keeps_its_message() {
        let err = accessibility_read_error(None, "PowerShell spawn error: not found", false);
        match err {
            crate::computer::ComputerError::ToolFailed(message) => {
                assert!(message.contains("PowerShell spawn error"), "{message}");
            }
            other => panic!("expected ToolFailed, got {other:?}"),
        }
    }
}
