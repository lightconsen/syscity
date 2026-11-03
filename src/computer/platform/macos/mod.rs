//! macOS capability set — GUI automation and desktop control tools.

pub mod accessibility;
pub mod applescript;
pub mod desktop_control;
pub mod hit_test;
pub mod notification;
pub mod permissions;
pub mod screenshot;

pub use accessibility::AccessibilityTool;
pub use applescript::AppleScriptTool;
pub use desktop_control::DesktopControlTool;
pub use notification::NotificationTool;
pub use screenshot::ScreenshotTool;
use tracing::{info, warn};

use super::{OsControlScope, PlatformConstraints, PlatformToolSet};
use crate::tools::Tool;

/// macOS platform tool set — provides GUI automation, accessibility
/// querying, screenshots, and AppleScript execution for macOS environments.
pub struct MacosToolset;

impl MacosToolset {
    pub fn new() -> Self {
        Self
    }
}

impl Default for MacosToolset {
    fn default() -> Self {
        Self::new()
    }
}

impl PlatformToolSet for MacosToolset {
    fn id(&self) -> &str {
        "macos"
    }

    fn name(&self) -> &str {
        "macOS Control"
    }

    fn description(&self) -> &str {
        "macOS desktop automation: accessibility UI tree queries, screenshots, AppleScript \
         execution, and hybrid desktop control (inspect, click, type, keyboard shortcuts)."
    }

    fn constraints(&self) -> &PlatformConstraints {
        static CONSTRAINTS: std::sync::OnceLock<PlatformConstraints> = std::sync::OnceLock::new();
        CONSTRAINTS.get_or_init(|| PlatformConstraints {
            target_os: vec!["macos".to_string()],
            requires_gui: true,
            requires_services: Vec::new(),
        })
    }

    fn scope(&self) -> OsControlScope {
        OsControlScope::UserSpace
    }

    fn tools(&self) -> Vec<Box<dyn Tool>> {
        vec![
            Box::new(AccessibilityTool::new()),
            Box::new(ScreenshotTool::new()),
            Box::new(AppleScriptTool::new()),
            Box::new(DesktopControlTool::new()),
            Box::new(NotificationTool::new()),
        ]
    }

    fn is_available(&self) -> bool {
        let base_ok = self.constraints().check();
        if !base_ok {
            return false;
        }

        // Check accessibility permissions on first call.
        if !permissions::has_accessibility_permission() {
            warn!(
                "macOS Accessibility permission not granted. Desktop control tools \
                 (accessibility, click, type) will not work."
            );
            info!("{}", permissions::accessibility_permission_guide());
            // Trigger the system permission dialog once.
            permissions::trigger_accessibility_prompt();
            // Still return true because screenshot and basic AppleScript
            // work without accessibility; only UI-tree tools need it.
        }

        true
    }
}
