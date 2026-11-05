//! Platform tool sets for Syscity
//!
//! This module defines `PlatformToolSet` — a way to group platform-specific
//! tools by environment (Linux Server, macOS Desktop, etc.). Tools are
//! registered individually into `ToolRegistry`; `PlatformToolSet` is only an
//! organizational unit that controls *which* tools are exposed for the current
//! environment.

use std::collections::HashSet;

use serde::{Deserialize, Serialize};

use crate::tools::Tool;

/// OS control permission scope.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OsControlScope {
    /// Read-only observation (processes, logs, configs).
    ReadOnly = 0,
    /// User-space operations (user processes, user configs).
    UserSpace = 1,
    /// System-level operations (services, packages, system configs).
    System = 2,
    /// Full control (kernel params, firewall, user management).
    Root = 3,
}

/// Platform constraints that determine whether a `PlatformToolSet` is
/// available.
#[derive(Debug, Clone, Default)]
pub struct PlatformConstraints {
    /// Target operating systems (e.g. `["linux"]`, `["macos"]`).
    pub target_os: Vec<String>,
    /// Whether a GUI/display server is required.
    pub requires_gui: bool,
    /// Optional services that must be available (e.g. `["systemd"]`).
    pub requires_services: Vec<String>,
}

impl PlatformConstraints {
    /// Check whether the current environment satisfies all constraints.
    pub fn check(&self) -> bool {
        let current_os = std::env::consts::OS;
        if !self.target_os.is_empty() && !self.target_os.iter().any(|os| os == current_os) {
            return false;
        }

        if self.requires_gui && !crate::computer::has_display_server() {
            return false;
        }

        for svc in &self.requires_services {
            if !is_service_available(svc) {
                return false;
            }
        }

        true
    }
}

/// An optional group of tools a set offers only when asked.
///
/// A set with twenty tools and a model that has not yet decided to use the
/// phone is twenty schemas of prompt for no decision. The groups split them:
/// everything in `core_tools` is offered always, and a tool named here is
/// offered only after someone opens its group (see
/// [`crate::tools::list_capabilities`]). What belongs together is a fact about
/// the toolset — "locating a control", "reading a list as data" — so it is
/// declared here rather than on each tool.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ToolGroupSpec {
    /// Stable key the caller names when it opens the group, e.g.
    /// `"android.locate"`.
    pub id: &'static str,
    /// One line for the catalog: what opening the group gives you.
    pub about: &'static str,
    /// The tool names in this group.
    pub tools: &'static [&'static str],
}

/// A set of tools scoped to a specific platform / environment.
pub trait PlatformToolSet: Send + Sync {
    /// Unique identifier, e.g. `"linux-server"`.
    fn id(&self) -> &str;

    /// Human-readable name.
    fn name(&self) -> &str;

    /// Description shown to users / in logs.
    fn description(&self) -> &str;

    /// Platform constraints for runtime availability checks.
    fn constraints(&self) -> &PlatformConstraints;

    /// Maximum permission scope this set requires.
    fn scope(&self) -> OsControlScope {
        OsControlScope::ReadOnly
    }

    /// Tools provided by this set.
    fn tools(&self) -> Vec<Box<dyn Tool>>;

    /// Tools offered without opening anything.
    ///
    /// Only meaningful together with [`tool_groups`](Self::tool_groups): this
    /// list names the tools that must stay visible when the rest are held
    /// back. A set that declares no groups has no tiering at all, whatever it
    /// returns here.
    fn core_tools(&self) -> &'static [&'static str] {
        &[]
    }

    /// The optional groups this set offers, each opened by name.
    fn tool_groups(&self) -> &'static [ToolGroupSpec] {
        &[]
    }

    /// Which of the shared action judgments this set actually makes.
    ///
    /// The vocabularies — the five effect words, the three read-back verdicts,
    /// the event rule — are defined once in [`screen`] so that a second set
    /// inherits them instead of re-deriving them. This is where a set states
    /// which of them it can honestly answer: a caller told `unknown` deserves
    /// to know whether that is "the screen could not be read" or "this device
    /// never reads a field back".
    ///
    /// The default is [`ScreenSemantics::NONE`], because a set that has not
    /// said it makes these judgments should not be assumed to.
    fn screen_semantics(&self) -> screen::ScreenSemantics {
        screen::ScreenSemantics::NONE
    }

    /// Whether this set is available on the current platform.
    fn is_available(&self) -> bool {
        self.constraints().check()
    }
}

/// Strategy when two sets register tools with the same name.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolConflictStrategy {
    /// Panic on conflict.
    Reject,
    /// Later registration overrides earlier.
    Override,
}

/// Pre-defined platform tool profiles.
#[derive(Debug, Clone)]
pub enum CapabilityProfile {
    /// Only built-in generic tools, no OS-specific sets.
    Minimal,
    /// Read-only observation only.
    Observer,
    /// Server-oriented sets (no GUI required).
    Server,
    /// Desktop-oriented sets (GUI required).
    Desktop,
    /// All available sets.
    Full,
    /// Explicit list of set IDs.
    Custom(Vec<String>),
}

impl CapabilityProfile {
    /// Apply the profile by disabling sets that don't match.
    pub fn apply(&self, registry: &mut PlatformCapabilityRegistry) {
        match self {
            CapabilityProfile::Minimal => {
                let ids: Vec<String> = registry
                    .all_sets()
                    .iter()
                    .map(|s| s.id().to_string())
                    .collect();
                for id in ids {
                    registry.disable(&id);
                }
            }
            CapabilityProfile::Observer => {
                let ids: Vec<String> = registry
                    .all_sets()
                    .iter()
                    .filter(|s| s.scope() != OsControlScope::ReadOnly)
                    .map(|s| s.id().to_string())
                    .collect();
                for id in ids {
                    registry.disable(&id);
                }
            }
            CapabilityProfile::Server => {
                let ids: Vec<String> = registry
                    .all_sets()
                    .iter()
                    .filter(|s| s.constraints().requires_gui)
                    .map(|s| s.id().to_string())
                    .collect();
                for id in ids {
                    registry.disable(&id);
                }
            }
            CapabilityProfile::Desktop => {
                let ids: Vec<String> = registry
                    .all_sets()
                    .iter()
                    .filter(|s| !s.constraints().requires_gui)
                    .map(|s| s.id().to_string())
                    .collect();
                for id in ids {
                    registry.disable(&id);
                }
            }
            CapabilityProfile::Full => {
                // Default — enable everything that passes env checks.
            }
            CapabilityProfile::Custom(ids) => {
                let id_set: HashSet<String> = ids.iter().cloned().collect();
                let to_disable: Vec<String> = registry
                    .all_sets()
                    .iter()
                    .filter(|s| !id_set.contains(s.id()))
                    .map(|s| s.id().to_string())
                    .collect();
                for id in to_disable {
                    registry.disable(&id);
                }
            }
        }
    }
}

/// Detect whether a system service is available.
fn is_service_available(name: &str) -> bool {
    match name {
        "systemd" => std::path::Path::new("/run/systemd/system").exists(),
        _ => false,
    }
}

pub mod android;
pub mod common;
pub mod ios;
pub mod linux;
#[cfg(target_os = "macos")]
pub mod macos;
pub mod registry;
pub mod screen;
pub mod server_operator;
pub mod wayland;
pub mod windows;
pub mod x11;

pub use android::AndroidToolset;
pub use ios::IosToolset;
pub use linux::LinuxToolset;
#[cfg(target_os = "macos")]
pub use macos::MacosToolset;
pub use registry::PlatformCapabilityRegistry;
pub use screen::{
    changed_threshold, classify as classify_effect, content_event_matters, place_changed, Effect,
    Judgment, ReadbackVerdict, ScreenSemantics, Signal, TreeDelta, EFFECT_CHANGED,
    EFFECT_PLACE_CHANGED, EFFECT_SMALL_CHANGE, EFFECT_UNCHANGED, EFFECT_UNKNOWN, READBACK_MATCH,
    READBACK_MISMATCH, READBACK_UNKNOWN,
};
pub use server_operator::{Diagnosis, ServerOperator, SystemInspector, SystemSnapshot};
pub use wayland::LinuxDesktopWaylandToolset;
pub use windows::WindowsToolset;
pub use x11::LinuxDesktopX11Toolset;

/// Return all platform tool sets compiled into this binary.
///
/// This is the single source of truth for what *could* be available;
/// runtime detection (`PlatformConstraints::check`) decides what *is*
/// available on the current host.
pub fn all_known_toolsets() -> Vec<Box<dyn PlatformToolSet>> {
    let mut sets: Vec<Box<dyn PlatformToolSet>> = Vec::new();

    #[cfg(target_os = "linux")]
    {
        sets.push(Box::new(LinuxToolset::new()));
        sets.push(Box::new(LinuxDesktopX11Toolset::new()));
        sets.push(Box::new(LinuxDesktopWaylandToolset::new()));
    }

    #[cfg(target_os = "macos")]
    {
        sets.push(Box::new(MacosToolset::new()));
    }

    #[cfg(target_os = "windows")]
    {
        sets.push(Box::new(WindowsToolset::new()));
    }

    // Mobile bridges are platform-agnostic (depend on external CLI tools)
    sets.push(Box::new(AndroidToolset::new()));
    sets.push(Box::new(IosToolset::new()));

    sets
}

/// Build a human-readable summary of the current host environment
/// and which capability sets are available.
pub fn host_environment_summary() -> String {
    use std::fmt::Write;

    let mut buf = String::new();
    let _ = writeln!(&mut buf, "Host: {} ({})", std::env::consts::OS, std::env::consts::ARCH);

    let sets = all_known_toolsets();
    if sets.is_empty() {
        let _ = writeln!(&mut buf, "No OS-specific platform tool sets compiled.");
        return buf;
    }

    let mut available = Vec::new();

    for set in sets {
        if set.is_available() {
            available.push(capability_row(set.id(), set.name(), set.tool_groups()));
        }
    }

    if !available.is_empty() {
        let _ = writeln!(&mut buf, "Available capabilities:");
        for row in &available {
            let _ = write!(&mut buf, "{row}");
        }
    }

    buf
}

/// One set's row in the host summary, with the groups it holds back.
///
/// Most of a large set is not advertised, and a model that cannot see a tool
/// cannot ask for it. Naming the groups is what turns the tiering into a
/// choice instead of a blind spot: the tool itself is withheld, but the fact
/// that it exists is not.
fn capability_row(id: &str, name: &str, groups: &[ToolGroupSpec]) -> String {
    use std::fmt::Write;

    let mut row = format!("  - {name} ({id})\n");
    for group in groups {
        let _ = writeln!(
            &mut row,
            "      · {} — {} (not advertised yet; open with \
             `list_capabilities {{\"open\": \"{}\"}}`)",
            group.id, group.about, group.id
        );
    }
    row
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_set_row_names_the_groups_it_holds_back() {
        // A tool nobody advertises is only usable if something says it exists.
        // The summary must name the groups — and the open-call that reveals
        // them — or the tiering is a blind spot rather than a choice.
        let android = all_known_toolsets()
            .into_iter()
            .find(|set| set.id() == "android")
            .expect("the android set is always known");
        assert!(!android.tool_groups().is_empty(), "android declares no groups");

        let row = capability_row("android", "Android Device Bridge", android.tool_groups());
        for group in android.tool_groups() {
            assert!(row.contains(group.id), "{} missing from:\n{row}", group.id);
            assert!(row.contains(group.about), "{} about missing", group.id);
        }
        assert!(row.contains("list_capabilities"), "{row}");
    }

    #[test]
    fn a_set_with_no_groups_adds_no_lines() {
        let row = capability_row("ios", "iOS Device Bridge", &[]);
        assert_eq!(row.lines().count(), 1, "{row}");
        assert!(!row.contains("list_capabilities"), "{row}");
    }
}
