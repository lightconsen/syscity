//! Computer adapter — cross-platform unified desktop interface.
//!
//! This layer sits **above** `CapabilitySet` + `ToolRegistry` and provides
//! a platform-agnostic API for screenshots, UI-tree reading, and desktop
//! actions.  The LLM still interacts with individual `Tool`s via
//! `ToolRegistry`; `ComputerAdapter` is consumed by the higher-level
//! `GoalPlanner` and `ComputerUseLoop`.
//!
//! Architecture:
//! ```text
//! Agent / Planner
//!       |
//! ComputerAdapter  ←── 统一接口（你在这里）
//!       |
//! ToolRegistry ──→ CapabilitySet ──→ xdotool / SendKeys / AXUIElement
//! ```
// INVARIANTS-NONE: device-automation drivers; every action is request-scoped and leaves no local persistent state.

use std::sync::Arc;
use std::time::Duration;

use crate::tools::ToolRegistry;

pub mod audio;
pub mod budget;
pub mod display;
pub mod element_index;
pub mod fs_watch;
pub mod headless;
pub mod log_aggregator;
pub mod network;
pub mod page;
pub mod reflection;
/// Clients for a desktop that lives on another machine — the benchmark
/// harnesses (OSWorld, Cua Bench) own the VM and this side drives it.
pub mod remote;
pub mod remote_control;
pub mod rollback;
pub mod screen_recorder;
pub mod screenshot_encoder;
pub mod sensitive_ui;
pub mod session;
pub mod system;
pub mod types;
pub mod use_loop;
pub mod verification;
// The vision module itself is always compiled: its heavy ONNX/OCR
// submodules are feature-gated internally, while `screen_state` (used by
// `use_loop` and the computer/screen_state tools) is pure data structures.
pub mod vision;

// Platform adapters
#[cfg(target_os = "linux")]
pub mod platform_linux;
#[cfg(target_os = "macos")]
pub mod platform_macos;
#[cfg(target_os = "windows")]
pub mod platform_windows;

pub mod platform;

pub use audio::{AudioCapture, AudioSegment, AudioSource, DetectedAudioEvent};
pub use budget::StepBudget;
pub use display::{CoordinateSpace, DisplayGeometry, DisplayInfo};
pub use element_index::{ActionTarget, ResolveError, ResolvedElement, SnapshotCache, UiSnapshot};
pub use fs_watch::{FileChangeEvent, FileChangeKind, FileWatchResult, FileWatcher};
pub use headless::{HeadlessComputerAdapter, VirtualDisplay};
pub use log_aggregator::{
    AlertAction, AlertEvent, LogAggregator, LogAlertRule, LogEntry, LogLevel, LogSource,
};
pub use network::{FirewallRule, NetworkInspector, PingResult, PortEntry, TcpConnectResult};
pub use page::{PageAction, PageActor, PageTarget};
pub use remote_control::{RemoteControlAdapter, RemoteControlConfig, RemoteProtocol};
pub use rollback::{RollbackManager, Snapshot};
pub use screen_recorder::{
    resolve_or_download_ffmpeg, RecorderConfig, Rect as RecorderRect, ScreenRecorder, VideoFrame,
};
pub use types::*;
pub use use_loop::{ComputerUseLoop, LoopConfig, LoopDecision, LoopResult, LoopState, StepRecord};
pub use verification::{VerificationConfig, VerificationCriteria, VerificationEngine};

/// Unified error type for computer operations.
#[derive(Debug, thiserror::Error)]
pub enum ComputerError {
    #[error("Platform not supported: {0}")]
    UnsupportedPlatform(String),
    #[error("No GUI/display server available")]
    NoDisplay,
    #[error("Accessibility permission not granted")]
    AccessibilityDenied,
    #[error("Background input is not available: {0}")]
    BackgroundInputUnavailable(String),
    #[error("Tool execution failed: {0}")]
    ToolFailed(String),
    #[error("Element not found: {0}")]
    ElementNotFound(String),
    /// More than one element matched, and picking one would be a guess.
    #[error("Ambiguous target: {0}")]
    AmbiguousTarget(String),
    /// The step ran out of its time budget.
    ///
    /// Distinct from [`ComputerError::Timeout`], which is a wait that ended
    /// the way waits do: this is the *caller's* ceiling being reached in the
    /// middle of a step, so the answer is a larger budget rather than a
    /// longer wait.
    #[error("Step budget exhausted: {0}")]
    BudgetExhausted(String),
    /// Nothing matched, and the tree was not read in full.
    ///
    /// Separate from [`ComputerError::ElementNotFound`] on purpose: one says
    /// the element is not there, the other says this read did not see all of
    /// it. Only the second is worth a deeper retry, and telling a caller
    /// "re-aim" when it should "re-read" wastes the turn.
    #[error("{0}")]
    TreeIncomplete(String),
    #[error("{0}")]
    StaleElement(String),
    #[error("Timeout waiting for condition")]
    Timeout,
    #[error("Screenshot failed: {0}")]
    ScreenshotFailed(String),
    #[error("Adapter not initialized")]
    NotInitialized,
    #[error("Process not found: {0}")]
    ProcessNotFound(String),
    #[error("Failed to kill process: {0}")]
    KillFailed(String),
    /// No rung could run, and the rungs that tried left evidence behind.
    ///
    /// The ladder's failure is not one error, it is "these rungs could not do
    /// this, and here is what each of them learned". Flattening it to a string
    /// loses the very facts a caller would retry or re-aim on.
    #[error("{error}")]
    NoRung {
        /// The last error, or a generic explanation when a rung reported none.
        error: String,
        /// What the rungs established before giving up.
        evidence: Box<crate::computer::ActionEvidence>,
    },
    #[error("Other: {0}")]
    Other(String),
}

pub type Result<T> = std::result::Result<T, ComputerError>;

/// Cross-platform adapter for desktop perception and action.
///
/// Implementors wrap platform-specific `CapabilitySet` tools (xdotool,
/// SendKeys, AXUIElement, UIAutomation, etc.) and expose a uniform
/// interface to the Agent layer.
#[async_trait::async_trait]
pub trait ComputerAdapter: Send + Sync {
    /// Capture a screenshot of the full screen or a sub-region.
    async fn screenshot(&self, region: Option<Rect>) -> Result<Screenshot>;

    /// Capture the screen, bypassing any reuse window the adapter keeps.
    ///
    /// The default is [`screenshot`](Self::screenshot): an adapter that
    /// captures on every call needs nothing more. An adapter that serves a
    /// recent capture from a cache must override this, because the settle loop
    /// compares frames taken tens of milliseconds apart around an action, and
    /// a cache that answers both from one stored frame makes "the screen has
    /// not changed yet" and "the screen has not started changing" look the
    /// same — the second being read as the first is how a click that worked
    /// gets reported as a no-op.
    async fn screenshot_fresh(&self, region: Option<Rect>) -> Result<Screenshot> {
        self.screenshot(region).await
    }

    /// Read the accessibility UI tree of the active application.
    async fn read_ui_tree(&self, app: Option<&str>) -> Result<Vec<UiElement>>;

    /// Execute a desktop action.
    async fn execute(&self, action: DesktopAction) -> Result<ActionResult>;

    /// Wait until a condition becomes true or timeout expires.
    async fn wait_for(&self, condition: WaitCondition, timeout: Duration) -> Result<bool>;

    /// Resolve a click target to concrete coordinates and, when the target
    /// names an element, to the element itself.
    ///
    /// This is what makes an element id usable: the adapter that handed out
    /// the id is the one that can resolve it back. The default implementation
    /// handles bare coordinates and refuses everything else, so an adapter
    /// opts into semantic addressing by overriding it.
    async fn resolve_target(&self, target: &ClickTarget) -> Result<ActionTarget> {
        match target {
            ClickTarget::Coordinate(p) => Ok(ActionTarget::point(p.x, p.y)),
            other => Err(ComputerError::UnsupportedPlatform(format!(
                "element targeting is not supported by this adapter ({other:?})"
            ))),
        }
    }

    /// Perform a native accessibility action on an already-resolved element.
    ///
    /// Returning an error is the contract for "this platform cannot do it":
    /// the action ladder treats that as "the rung was unavailable" and drops
    /// to the next one, rather than as a failed action.
    async fn perform_ax_action(
        &self,
        _target: &ActionTarget,
        _action: AxAction,
    ) -> Result<ActionResult> {
        Err(ComputerError::UnsupportedPlatform(
            "accessibility actions are not supported by this adapter".to_string(),
        ))
    }

    /// Which element would receive an event at a screen point.
    ///
    /// The pixel rung aims at a point, and a point is not an element: a
    /// dialog, a notification, or another window can be drawn over it. The
    /// platform's own hit test answers "what is at this point", so a click
    /// that would land somewhere else can be refused with the occluder named
    /// instead of dispatched and reported as a success the wrong window got.
    ///
    /// `target` is what the caller meant to click, when it knows; a platform
    /// that can only report the element under the point uses it to decide
    /// whether the two are the same. The default reports `Unknown`: an
    /// adapter that has no hit test must not pretend the point is clear.
    async fn hit_test(&self, _point: Point, _target: Option<&ActionTarget>) -> HitTestResult {
        HitTestResult::Unknown
    }

    /// Whether this adapter can deliver input without taking the foreground.
    ///
    /// Accessibility actions and page actions are not input-layer delivery:
    /// they act on an element and are background by nature on every platform.
    /// This answers the narrower question — can a *click or keystroke* reach
    /// the app while another window holds the focus?
    fn supports_background_input(&self) -> bool {
        false
    }

    /// Perform the action without taking the foreground or moving the
    /// pointer.
    ///
    /// `target` is the element the session resolved for the action, when it
    /// has one; an action that names no element cannot usually be delivered in
    /// the background, because there is nothing to aim at but a screen point.
    ///
    /// The default refuses. An adapter that cannot do this must say so: the
    /// alternative is silently doing it in the foreground, which is exactly
    /// what a background-delivery session asked it not to do.
    async fn perform_background(
        &self,
        _action: &DesktopAction,
        _target: Option<&ActionTarget>,
    ) -> Result<ActionResult> {
        Err(ComputerError::BackgroundInputUnavailable(format!(
            "{} has no way to deliver input to an app that is not in front",
            std::env::consts::OS
        )))
    }

    /// Geometry of the displays this adapter drives, in screen points.
    ///
    /// Screenshot pixels and click coordinates are not the same space — a
    /// Retina capture is twice the point size — so a caller that wants to act
    /// on a pixel it saw must know the conversion. The default probes the
    /// local machine; adapters that drive a *different* machine (remote
    /// control) must override this to report unknown rather than convert with
    /// the wrong geometry.
    async fn display_geometry(&self) -> Result<crate::computer::display::DisplayGeometry> {
        Ok(crate::computer::display::detect().await)
    }

    /// Convenience: click at a logical coordinate.
    async fn click_at(&self, point: Point, button: MouseButton) -> Result<ActionResult> {
        self.execute(DesktopAction::Click {
            target: ClickTarget::Coordinate(point),
            button,
        })
        .await
    }

    /// Convenience: type text.
    async fn type_text(&self, text: &str) -> Result<ActionResult> {
        self.execute(DesktopAction::Type {
            text: text.to_string(),
            target: None,
        })
        .await
    }

    /// Convenience: press a key combination.
    async fn key_press(&self, keys: Vec<String>) -> Result<ActionResult> {
        self.execute(DesktopAction::KeyPress { keys }).await
    }

    /// Convenience: get clipboard content.
    async fn clipboard_get(&self) -> Result<String> {
        let result = self.execute(DesktopAction::ClipboardGet).await?;
        if result.success {
            Ok(result.message)
        } else {
            Err(ComputerError::ToolFailed(result.message))
        }
    }

    /// Convenience: set clipboard content.
    async fn clipboard_set(&self, text: &str) -> Result<ActionResult> {
        self.execute(DesktopAction::ClipboardSet { text: text.to_string() })
            .await
    }

    /// Convenience: restart a process by PID or name.
    async fn restart_process(
        &self,
        pid: Option<u32>,
        name: Option<&str>,
        force: bool,
    ) -> Result<ActionResult> {
        self.execute(DesktopAction::RestartProcess {
            pid,
            name: name.map(String::from),
            force,
        })
        .await
    }

    /// Convenience: set process priority.
    async fn set_process_priority(
        &self,
        pid: Option<u32>,
        name: Option<&str>,
        priority: i32,
    ) -> Result<ActionResult> {
        self.execute(DesktopAction::SetProcessPriority {
            pid,
            name: name.map(String::from),
            priority,
        })
        .await
    }
}

/// Create the appropriate adapter for the current platform.
///
/// - macOS → `MacosPhysicalAdapter`
/// - Windows → `WindowsPhysicalAdapter`
/// - Linux + X11 → `X11PhysicalAdapter`
/// - Linux + Wayland → `WaylandPhysicalAdapter`
/// - Linux headless → `HeadlessPhysicalAdapter`
#[allow(unreachable_code, unused_variables)] // mobile targets take none of the platform arms
pub async fn create_adapter(registry: Arc<ToolRegistry>) -> Result<Box<dyn ComputerAdapter>> {
    #[cfg(target_os = "macos")]
    {
        return platform_macos::create(registry).await;
    }

    #[cfg(target_os = "windows")]
    {
        return platform_windows::create(registry).await;
    }

    #[cfg(target_os = "linux")]
    {
        return platform_linux::create(registry).await;
    }

    Err(ComputerError::UnsupportedPlatform(std::env::consts::OS.to_string()))
}

/// Detect whether a display server is available at runtime.
pub fn has_display_server() -> bool {
    has_x11() || has_wayland() || cfg!(target_os = "macos") || cfg!(target_os = "windows")
}

/// Detect X11.
pub fn has_x11() -> bool {
    std::env::var("DISPLAY").is_ok() && std::env::var("WAYLAND_DISPLAY").is_err()
}

/// Detect Wayland.
pub fn has_wayland() -> bool {
    std::env::var("WAYLAND_DISPLAY").is_ok()
}

/// Canonical name for a key, so callers can say "enter" and each backend gets
/// the spelling it understands.
///
/// The session picks activation keys by element role and hands over lowercase
/// tokens; xdotool, ydotool and SendKeys each spell those differently, and a
/// mistranslation is not a no-op — the literal word ends up typed into the
/// focused field.
pub fn canonical_key(key: &str) -> &str {
    match key.trim().to_ascii_lowercase().as_str() {
        "return" | "enter" => "enter",
        "space" | "spacebar" => "space",
        "tab" => "tab",
        "esc" | "escape" => "escape",
        "backspace" | "delete" => "backspace",
        _ => key,
    }
}

/// Spelling an X11/Wayland key injector (xdotool, ydotool) accepts.
pub fn evdev_key_name(key: &str) -> String {
    match canonical_key(key) {
        "enter" => "Return".to_string(),
        "space" => "space".to_string(),
        "tab" => "Tab".to_string(),
        "escape" => "Escape".to_string(),
        "backspace" => "BackSpace".to_string(),
        other => other.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_ui_element_find_by_role() {
        let tree = UiElement {
            id: "root".to_string(),
            role: "window".to_string(),
            label: Some("Main".to_string()),
            value: None,
            bounds: Some(Rect::new(0, 0, 100, 100)),
            enabled: crate::computer::Tri::Known(true),
            focused: crate::computer::Tri::Unknown,
            actions: None,
            children: vec![UiElement {
                id: "btn".to_string(),
                role: "button".to_string(),
                label: Some("OK".to_string()),
                value: None,
                bounds: Some(Rect::new(10, 10, 50, 20)),
                enabled: crate::computer::Tri::Known(true),
                focused: crate::computer::Tri::Unknown,
                actions: None,
                children: vec![],
                children_count: None,
                truncated: false,
            }],
            children_count: None,
            truncated: false,
        };

        let btn = tree.find_by_role("button");
        assert!(btn.is_some());
        assert_eq!(btn.unwrap().label, Some("OK".to_string()));
    }

    #[test]
    fn test_ui_element_center() {
        let el = UiElement {
            id: "test".to_string(),
            role: "button".to_string(),
            label: None,
            value: None,
            bounds: Some(Rect::new(10, 20, 100, 50)),
            enabled: crate::computer::Tri::Known(true),
            focused: crate::computer::Tri::Unknown,
            actions: None,
            children: vec![],
            children_count: None,
            truncated: false,
        };
        assert_eq!(el.center(), Some(Point::new(60, 45)));
    }

    fn node(children: Vec<UiElement>, children_count: Option<u32>, truncated: bool) -> UiElement {
        UiElement {
            id: String::new(),
            role: "window".to_string(),
            label: None,
            value: None,
            bounds: None,
            enabled: crate::computer::Tri::Unknown,
            focused: crate::computer::Tri::Unknown,
            actions: None,
            children,
            children_count,
            truncated,
        }
    }

    /// A node that reports more children than it carries was cut. The count
    /// is the evidence; without a count there is nothing to contradict.
    #[test]
    fn a_short_child_list_against_a_larger_count_is_incomplete() {
        let cut = node(vec![node(vec![], None, false)], Some(3), false);
        assert!(cut.children_incomplete());
        assert!(!cut.subtree_is_complete());

        let whole = node(vec![node(vec![], None, false)], Some(1), false);
        assert!(!whole.children_incomplete());
        assert!(whole.subtree_is_complete());

        // A platform that does not report counts says nothing about
        // completeness, and nothing is all we can honestly claim.
        let unknown = node(vec![], None, false);
        assert!(unknown.subtree_is_complete());
    }

    /// A cut below the root makes the whole tree incomplete, even though the
    /// root's own child list is exactly what the platform said it would be.
    #[test]
    fn incompleteness_propagates_from_a_deep_node() {
        let root = node(vec![node(vec![node(vec![], None, true)], Some(1), false)], Some(1), false);
        assert!(!root.children_incomplete());
        assert!(!root.subtree_is_complete());
        assert!(!UiElement::tree_is_complete(&[root]));
    }

    /// The flag and the count are independent answers, and either one is
    /// enough to say the read stopped short.
    #[test]
    fn a_truncation_flag_alone_is_incomplete() {
        let flagged = node(vec![], None, true);
        assert!(flagged.children_incomplete());
        assert!(!UiElement::tree_is_complete(&[flagged]));
    }

    #[test]
    fn test_action_result_helpers() {
        let r = ActionResult::success("done").with_data(serde_json::json!({"x": 1}));
        assert!(r.success);
        assert_eq!(r.message, "done");
        assert!(r.data.is_some());
    }

    #[test]
    fn named_keys_have_one_canonical_spelling() {
        assert_eq!(canonical_key("Return"), "enter");
        assert_eq!(canonical_key("SPACE"), "space");
        assert_eq!(canonical_key("Esc"), "escape");
        assert_eq!(canonical_key("Backspace"), "backspace");
        // Anything else passes through: a printable character is a character.
        assert_eq!(canonical_key("a"), "a");
        assert_eq!(canonical_key("cmd"), "cmd");
    }

    #[test]
    fn x11_and_wayland_get_the_spelling_their_injector_understands() {
        assert_eq!(evdev_key_name("enter"), "Return");
        assert_eq!(evdev_key_name("space"), "space");
        assert_eq!(evdev_key_name("tab"), "Tab");
        assert_eq!(evdev_key_name("escape"), "Escape");
        assert_eq!(evdev_key_name("Backspace"), "BackSpace");
        assert_eq!(evdev_key_name("a"), "a");
    }

    #[test]
    fn test_click_target_serde() {
        let target = ClickTarget::Coordinate(Point::new(100, 200));
        let json = serde_json::to_string(&target).unwrap();
        assert!(json.contains("coordinate"));
    }
}
