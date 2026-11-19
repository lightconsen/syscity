//! Capability declarations for platform tools.
//!
//! Every platform tool used to inherit [`ToolCapabilities::default`], which is
//! `requires_approval: false` / `RiskLevel::Low`. That is a safe default for a
//! tool nobody has classified, and it was the wrong answer for desktop input:
//! `macos_desktop_control` moves the real pointer and types into whatever holds
//! focus, and it inherited the permissive default while the aggregate `computer`
//! tool was correctly marked `requires_approval: true` + `Medium`. The same
//! action was therefore gated or not depending on which tool the model happened
//! to reach for.
//!
//! This module is the single place that answers "what does this tool do to the
//! world". Tools declare it by calling one of the constructors; the regression
//! test in `tests` walks every platform tool set and fails when an input-capable
//! tool claims not to need approval.
//!
//! Two *different* gates exist, and conflating them is how the hole appeared:
//!
//! - **approval** (`requires_approval`) — a person is asked before the call.
//! - **privilege** (`ToolRegistry::mark_privileged`) — a community-trust skill
//!   may only reach unprivileged tools at all.
//!
//! A desktop click needs both, and they live in different files. This module
//! covers the first; `gateway::agent_spawn` registers the second.

use crate::tools::approval::RiskLevel;
use crate::tools::sdk::ToolCapabilities;

/// Reads the screen or the accessibility tree, and changes nothing.
///
/// Safe to repeat, safe to run unattended: a second screenshot of the same
/// screen costs time and shows the same thing.
pub fn observe(categories: &[&str]) -> ToolCapabilities {
    ToolCapabilities {
        read_only: true,
        risk_level: RiskLevel::Low,
        categories: categories.iter().map(|c| c.to_string()).collect(),
        ..Default::default()
    }
}

/// Delivers input to the live desktop: pointer moves, keystrokes, focus changes.
///
/// `requires_approval` is true because these act on whatever is on screen *now*;
/// a repeat is a second click, not a retry. `deduplicate_identical_calls` is off
/// for the same reason — the generic "same tool, same arguments" guard would end
/// a turn on the second click of a carousel arrow and report the task unsolved.
pub fn desktop_input(categories: &[&str]) -> ToolCapabilities {
    ToolCapabilities {
        requires_approval: true,
        risk_level: RiskLevel::Medium,
        categories: categories.iter().map(|c| c.to_string()).collect(),
        idempotent: false,
        deduplicate_identical_calls: false,
        ..Default::default()
    }
}

/// Runs arbitrary code or commands on the host (AppleScript, PowerShell, a shell).
///
/// `High`, not `Medium`: there is no bound on what the payload can do, and it is
/// not limited to the screen.
pub fn arbitrary_code(categories: &[&str]) -> ToolCapabilities {
    ToolCapabilities {
        requires_approval: true,
        risk_level: RiskLevel::High,
        categories: categories.iter().map(|c| c.to_string()).collect(),
        idempotent: false,
        deduplicate_identical_calls: false,
        ..Default::default()
    }
}

/// Writes to the clipboard.
///
/// The clipboard is shared with the person at the machine: overwriting it
/// destroys what they copied, and whatever is put there may be pasted into
/// something that trusts its source. Medium + approval.
pub fn clipboard_write(categories: &[&str]) -> ToolCapabilities {
    ToolCapabilities {
        requires_approval: true,
        risk_level: RiskLevel::Medium,
        categories: categories.iter().map(|c| c.to_string()).collect(),
        ..Default::default()
    }
}

/// Changes system state without touching the pointer (services, packages, users).
///
/// `High`: a wrong argument here is not undoable with Ctrl-Z and can take the
/// machine down.
pub fn system_change(categories: &[&str]) -> ToolCapabilities {
    ToolCapabilities {
        requires_approval: true,
        risk_level: RiskLevel::High,
        categories: categories.iter().map(|c| c.to_string()).collect(),
        idempotent: false,
        ..Default::default()
    }
}

/// A visible side effect that cannot touch the pointer or run code: a toast.
///
/// Not `read_only` (it changes what the person sees), but not worth an approval
/// prompt either — a notification is the mildest thing a desktop tool can do,
/// and gating it would train people to approve without reading.
pub fn notice(categories: &[&str]) -> ToolCapabilities {
    ToolCapabilities {
        risk_level: RiskLevel::Low,
        categories: categories.iter().map(|c| c.to_string()).collect(),
        ..Default::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn observe_is_read_only_and_never_asks() {
        let caps = observe(&["accessibility"]);
        assert!(caps.read_only);
        assert!(!caps.requires_approval);
        assert_eq!(caps.risk_level, RiskLevel::Low);
    }

    #[test]
    fn every_writing_constructor_requires_approval() {
        // The hole this module closes: a tool that acts on the world must not
        // inherit `requires_approval: false`.
        for caps in [
            desktop_input(&["computer"]),
            arbitrary_code(&["shell"]),
            clipboard_write(&["clipboard"]),
            system_change(&["system"]),
        ] {
            assert!(caps.requires_approval, "{caps:?} must require approval");
            assert!(!caps.read_only);
            assert!(!caps.idempotent);
        }
    }

    #[test]
    fn desktop_input_does_not_deduplicate() {
        // Clicking the same coordinates twice advances a carousel twice. The
        // dedup guard would swallow the second click and report the task done.
        let caps = desktop_input(&["computer", "desktop"]);
        assert!(!caps.deduplicate_identical_calls);
        assert_eq!(caps.risk_level, RiskLevel::Medium);
    }

    #[test]
    fn arbitrary_code_outranks_plain_input() {
        assert_eq!(arbitrary_code(&["shell"]).risk_level, RiskLevel::High);
        assert_eq!(desktop_input(&["computer"]).risk_level, RiskLevel::Medium);
    }

    #[test]
    fn a_notice_is_mild_but_not_read_only() {
        // It changes what the person sees, so `read_only` would be a lie — and
        // an approval prompt for a toast would train people to click through.
        let caps = notice(&["notification"]);
        assert!(!caps.read_only);
        assert!(!caps.requires_approval);
        assert_eq!(caps.risk_level, RiskLevel::Low);
    }

    /// Every compiled set, not just the ones this host would register.
    ///
    /// `all_known_toolsets()` filters by `cfg(target_os)`, so on a macOS runner
    /// it would leave the Windows and Linux sets unexamined — exactly the ones
    /// whose declarations a macOS developer cannot otherwise check. The modules
    /// are compiled on every platform, so the sets can be built directly.
    fn every_compiled_toolset() -> Vec<Box<dyn crate::computer::platform::PlatformToolSet>> {
        use crate::computer::platform as p;
        vec![
            Box::new(p::LinuxToolset::new()),
            Box::new(p::LinuxDesktopX11Toolset::new()),
            Box::new(p::LinuxDesktopWaylandToolset::new()),
            Box::new(p::WindowsToolset::new()),
            #[cfg(target_os = "macos")]
            Box::new(p::MacosToolset::new()),
            Box::new(p::AndroidToolset::new()),
            Box::new(p::IosToolset::new()),
        ]
    }

    fn compiled_tool(name: &str) -> Box<dyn crate::tools::Tool> {
        for set in every_compiled_toolset() {
            for tool in set.tools() {
                if tool.name() == name {
                    return tool;
                }
            }
        }
        panic!("no compiled platform tool is named {name}");
    }

    /// The declaration only matters if it stops the call.
    ///
    /// Every other test here reads `capabilities()`. This one puts a real
    /// desktop tool through the real registry with nobody available to answer,
    /// and asserts the body never runs. A tool that advertises
    /// `requires_approval` and a registry that ignores it would pass all the
    /// others and still take over the pointer unattended.
    #[tokio::test]
    async fn a_desktop_input_tool_does_not_run_unattended() {
        // A `system` user in a `cron:` conversation is the plan's unattended
        // shape: no window, nobody to ask, a job that must not silently type
        // into whatever holds focus.
        const UNATTENDED: &str = "macos_desktop_control";
        if !cfg!(target_os = "macos") {
            return; // only the host's own tool can be exercised end to end
        }

        let tool = compiled_tool(UNATTENDED);
        assert!(
            tool.capabilities().requires_approval,
            "{UNATTENDED} must advertise approval before this test can mean anything"
        );

        let mut registry = crate::tools::ToolRegistry::new();
        registry.register(tool);
        let ctx = crate::tools::ToolContext::new("system", "cron:capability-gate")
            .with_ask_queue(std::sync::Arc::new(crate::tools::ask_user::AskQueue::new()));

        // `inspect` is a read; it must be refused anyway, because the tool can
        // also click and type and the gate is per tool, not per argument.
        let call = crate::providers::FunctionCall {
            name: UNATTENDED.to_string(),
            arguments: serde_json::json!({ "action": "inspect" }).to_string(),
        };

        let outcome = tokio::time::timeout(
            std::time::Duration::from_secs(10),
            registry.execute_call(&call, &ctx),
        )
        .await
        .expect("an unattended call must be refused promptly, not wait on a timeout");

        match outcome {
            Err(e) => {
                assert!(e.to_string().contains("no human is present"), "the deny must say why: {e}")
            }
            Ok(r) => assert!(
                !r.success,
                "a requires_approval tool must not succeed unattended: {:?}",
                r.output
            ),
        }
    }

    /// The gate: every tool a platform set can hand to the model, classified.
    ///
    /// This is the regression that would have caught the original hole. It walks
    /// the sets compiled for the current host and fails on any tool that can
    /// deliver input, run code, or write the clipboard while advertising
    /// `requires_approval: false`.
    ///
    /// A tool name is classified by what it does, not by how it is spelled. The
    /// lists below are the contract; adding a new input tool without adding it
    /// here is exactly the failure mode this test exists to make loud.
    ///
    /// Android is deliberately absent: it gates inside the tool, per action,
    /// because a per-tool `requires_approval` would either ask before every
    /// scroll or before none. See `computer::platform::android::gate`.
    #[test]
    fn no_platform_tool_delivers_input_without_approval() {
        // Names whose actions move the pointer, inject keys, change focus, run
        // arbitrary code, write the clipboard, or change system state.
        const MUST_ASK: &[&str] = &[
            "applescript",
            "windows_powershell",
            "macos_desktop_control",
            "windows_desktop_control",
            "linux_x11_desktop_control",
            "linux_wayland_desktop_control",
            "windows_clipboard",
            "linux_x11_clipboard",
            "linux_wayland_clipboard",
            "service_manager",
            "package_manager",
            "firewall_manager",
            "user_manager",
            "cron_manager",
            "ios_app_manager",
        ];

        let mut checked = 0;
        for set in every_compiled_toolset() {
            for tool in set.tools() {
                let name = tool.name().to_string();
                let caps = tool.capabilities();
                if !MUST_ASK.contains(&name.as_str()) {
                    continue;
                }
                checked += 1;
                assert!(
                    caps.requires_approval,
                    "{name} (in set {}) delivers input or changes the system but \
                     advertises requires_approval=false — it would run unattended",
                    set.id()
                );
                assert!(!caps.read_only, "{name} changes the world but claims read_only");
            }
        }
        // A set that stopped listing a name would make the loop vacuous and the
        // test green for the wrong reason. Fifteen names are expected above;
        // the floor is loose so a platform-specific set dropping out does not
        // force an unrelated edit, but a wholesale regression is caught.
        assert!(
            checked >= 8,
            "only {checked} input tools were found — the gate is not seeing the sets"
        );

        // And a listed name that no tool answers to is a stale entry: the tool
        // was renamed (or the set stopped exposing it) and the gate has been
        // quietly watching a name that does not exist.
        //
        // Except where the absence is the build rather than a rename: the
        // macOS toolset is the one set compiled only on its own host
        // (`platform::macos` is `cfg(target_os = "macos")` in the lib), so on
        // any other host its names answer to nothing by construction. They are
        // still checked for staleness — on a macOS checkout, where the set
        // exists.
        #[cfg(not(target_os = "macos"))]
        const ABSENT_BY_BUILD: &[&str] = &["applescript", "macos_desktop_control"];
        #[cfg(target_os = "macos")]
        const ABSENT_BY_BUILD: &[&str] = &[];

        let present: std::collections::HashSet<String> = every_compiled_toolset()
            .iter()
            .flat_map(|set| set.tools())
            .map(|t| t.name().to_string())
            .collect();
        let missing: Vec<&str> = MUST_ASK
            .iter()
            .copied()
            .filter(|n| !present.contains(*n))
            .filter(|n| !ABSENT_BY_BUILD.contains(n))
            .collect();
        assert!(
            missing.is_empty(),
            "these names are gated here but no platform tool answers to them: \
             {missing:?} — rename the entry, or the tool escaped the gate"
        );
    }

    /// The reverse direction: a tool that only observes must not demand approval.
    ///
    /// Without this, the fix for the first test would be "mark everything
    /// requires_approval", which passes the gate and makes the product unusable.
    #[test]
    fn observation_tools_stay_unapproved() {
        const READS_ONLY: &[&str] = &[
            "macos_accessibility",
            "windows_accessibility",
            "linux_x11_accessibility",
            "linux_wayland_accessibility",
            "macos_screenshot",
            "windows_screenshot",
            "linux_x11_screenshot",
            "linux_wayland_screenshot",
            "ios_screenshot",
            "ios_list_devices",
            "system_inspect",
            "log_analyzer",
            "network_diag",
        ];

        let mut checked = 0;
        for set in every_compiled_toolset() {
            for tool in set.tools() {
                let name = tool.name().to_string();
                if !READS_ONLY.contains(&name.as_str()) {
                    continue;
                }
                checked += 1;
                let caps = tool.capabilities();
                assert!(
                    !caps.requires_approval,
                    "{name} only observes but asks for approval — that trains \
                     people to approve without reading"
                );
                assert!(caps.read_only, "{name} only observes but is not read_only");
            }
        }
        assert!(checked >= 8, "only {checked} observation tools were found");
    }
}
