//! `syscity bench probe` — ask the desktop four questions before paying a model.
//!
//! Every expensive path in this benchmark begins with the same two questions:
//! can the gateway reach the VM, and does the VM answer in the shape the
//! adapter expects? `bench run` answers them from inside a model run, after the
//! provider has already been paid for it and after the VM has been booked. This
//! subcommand asks first, in seconds, and names the endpoint that is wrong.
//!
//! It does not act. A probe that clicked something could not be re-run between
//! tasks without changing what it measures, and the VM is restored per task
//! anyway. The one optional write is a read-only snippet through `/execute`
//! (`--execute`), because that is the path every action takes and a VM whose
//! pyautogui is broken looks perfectly healthy to the other three endpoints.

use std::time::Instant;

use serde::Serialize;

use crate::computer::remote::{OsworldDesktop, RemoteDesktopConfig};
use crate::computer::{ComputerAdapter, UiElement};
use crate::Result;

/// Version of the report shape. Bump it when a field's meaning changes.
pub const PROBE_SCHEMA: u32 = 1;

/// How many top-level elements the summary names.
const NAMED_TOP_LEVEL: usize = 8;

/// What `bench probe` needs from the command line.
#[derive(Debug, Clone, Default)]
pub struct ProbeOptions {
    /// Base URL of the desktop service (`http://<vm_ip>:5000` for OSWorld).
    pub computer_url: String,
    /// Bearer token, for a service that asks for one.
    pub auth_token: Option<String>,
    /// Also run a read-only snippet through `/execute`.
    pub execute: bool,
}

/// What the four endpoints said.
#[derive(Debug, Clone, Serialize)]
pub struct ProbeReport {
    pub schema: u32,
    pub computer_url: String,
    /// `/screen_size`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub screen: Option<ScreenCheck>,
    /// `/screenshot`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub screenshot: Option<ScreenshotCheck>,
    /// `/accessibility`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tree: Option<TreeCheck>,
    /// `/execute`, only when `--execute` was given.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub execute: Option<ExecuteCheck>,
    /// Checks that passed but deserve saying out loud.
    pub warnings: Vec<String>,
    /// Checks that failed.
    pub failures: Vec<String>,
    /// True when every check that ran succeeded.
    pub ok: bool,
    pub elapsed_ms: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct ScreenCheck {
    pub width: u32,
    pub height: u32,
    pub ms: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct ScreenshotCheck {
    pub width: u32,
    pub height: u32,
    /// Size of the base64 payload the service sent.
    pub encoded_bytes: usize,
    /// Whether the capture's own size equals what `/screen_size` reported.
    pub matches_screen: Option<bool>,
    pub ms: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct TreeCheck {
    /// Every element, at every depth.
    pub nodes: usize,
    /// The elements at the root, described for a human.
    pub top_level: Vec<String>,
    pub ms: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct ExecuteCheck {
    /// What the snippet printed, trimmed.
    pub output: String,
    pub ms: u64,
}

/// Probe the desktop described by `opts`. The report says what failed; the
/// caller decides what to do about it.
pub async fn probe(opts: &ProbeOptions) -> Result<ProbeReport> {
    let started = Instant::now();
    let mut config = RemoteDesktopConfig::new(opts.computer_url.trim());
    if let Some(token) = &opts.auth_token {
        config = config.with_token(token.clone());
    }
    let desktop = OsworldDesktop::new(config)?;

    let mut report = ProbeReport {
        schema: PROBE_SCHEMA,
        computer_url: opts.computer_url.trim().to_string(),
        screen: None,
        screenshot: None,
        tree: None,
        execute: None,
        warnings: Vec::new(),
        failures: Vec::new(),
        ok: false,
        elapsed_ms: 0,
    };

    let at = Instant::now();
    match desktop.display_geometry().await {
        Ok(geometry) => match geometry.primary() {
            Some(display) => {
                report.screen = Some(ScreenCheck {
                    width: display.width,
                    height: display.height,
                    ms: at.elapsed().as_millis() as u64,
                });
            }
            // A service that answers with no display still answered; that is a
            // warning, because the agent can work from the screenshot alone.
            None => report
                .warnings
                .push("/screen_size returned no display; geometry is unknown".to_string()),
        },
        Err(error) => report.failures.push(format!("/screen_size: {error}")),
    }

    let at = Instant::now();
    match desktop.screenshot(None).await {
        Ok(shot) => {
            let matches_screen = report
                .screen
                .as_ref()
                .map(|screen| shot.width == screen.width && shot.height == screen.height);
            if matches_screen == Some(false) {
                // Not fatal: a scaled capture is still a picture of the screen,
                // but every coordinate the agent computes from it is off.
                report.warnings.push(format!(
                    "/screenshot is {}x{} while /screen_size says {}x{}; \
                     coordinates would land in the wrong place",
                    shot.width,
                    shot.height,
                    report.screen.as_ref().map_or(0, |s| s.width),
                    report.screen.as_ref().map_or(0, |s| s.height),
                ));
            }
            report.screenshot = Some(ScreenshotCheck {
                width: shot.width,
                height: shot.height,
                encoded_bytes: shot.base64.len(),
                matches_screen,
                ms: at.elapsed().as_millis() as u64,
            });
        }
        Err(error) => report.failures.push(format!("/screenshot: {error}")),
    }

    let at = Instant::now();
    match desktop.read_ui_tree(None).await {
        Ok(nodes) => {
            let total = count_nodes(&nodes);
            if total == 0 {
                // Real, and common for Electron/Canvas apps: the agent can
                // still act, but only on pixels.
                report.warnings.push(
                    "/accessibility returned no elements; the agent can only use coordinates"
                        .to_string(),
                );
            }
            report.tree = Some(TreeCheck {
                nodes: total,
                top_level: nodes.iter().take(NAMED_TOP_LEVEL).map(describe).collect(),
                ms: at.elapsed().as_millis() as u64,
            });
        }
        Err(error) => report.failures.push(format!("/accessibility: {error}")),
    }

    if opts.execute {
        let at = Instant::now();
        // Read-only on purpose: the point is whether the service runs Python
        // and pyautogui answers, not whether it can change anything.
        match desktop
            .probe_execute("import pyautogui\nprint(pyautogui.size())\n")
            .await
        {
            Ok(output) => {
                report.execute = Some(ExecuteCheck {
                    output: output.trim().to_string(),
                    ms: at.elapsed().as_millis() as u64,
                });
            }
            Err(error) => report.failures.push(format!("/execute: {error}")),
        }
    }

    report.ok = report.failures.is_empty();
    report.elapsed_ms = started.elapsed().as_millis() as u64;
    Ok(report)
}

/// Render the report for a terminal: one line per endpoint, failures last.
pub fn render(report: &ProbeReport) -> String {
    let mut out = String::new();
    out.push_str(&format!("desktop  {}\n", report.computer_url));

    match &report.screen {
        Some(screen) => out
            .push_str(&format!("screen   {}x{} ({} ms)\n", screen.width, screen.height, screen.ms)),
        None => out.push_str("screen   FAILED\n"),
    }
    match &report.screenshot {
        Some(shot) => {
            out.push_str(&format!(
                "shot     {}x{}, {} KiB encoded ({} ms)\n",
                shot.width,
                shot.height,
                shot.encoded_bytes / 1024,
                shot.ms
            ));
        }
        None => out.push_str("shot     FAILED\n"),
    }
    match &report.tree {
        Some(tree) => {
            out.push_str(&format!(
                "tree     {} element(s) at {} top-level ({} ms)\n",
                tree.nodes,
                tree.top_level.len(),
                tree.ms
            ));
            for name in &tree.top_level {
                out.push_str(&format!("         {name}\n"));
            }
        }
        None => out.push_str("tree     FAILED\n"),
    }
    if let Some(execute) = &report.execute {
        out.push_str(&format!(
            "execute  {} ({} ms)\n",
            if execute.output.is_empty() {
                "ran, printed nothing"
            } else {
                execute.output.as_str()
            },
            execute.ms
        ));
    }

    for warning in &report.warnings {
        out.push_str(&format!("warning  {warning}\n"));
    }
    for failure in &report.failures {
        out.push_str(&format!("FAILED   {failure}\n"));
    }
    out.push_str(if report.ok { "ok\n" } else { "not ready\n" });
    out
}

fn count_nodes(nodes: &[UiElement]) -> usize {
    nodes
        .iter()
        .map(|node| 1 + count_nodes(&node.children))
        .sum()
}

/// `push button "Save"`, or just the role when it is unnamed.
fn describe(node: &UiElement) -> String {
    match node
        .label
        .as_deref()
        .filter(|label| !label.trim().is_empty())
    {
        Some(label) => format!("{} {:?}", node.role, label),
        None => node.role.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bench::testing::start_fake_desktop;

    async fn probe_fake(execute: bool) -> ProbeReport {
        let (url, _vm) = start_fake_desktop().await;
        probe(&ProbeOptions {
            computer_url: url,
            auth_token: None,
            execute,
        })
        .await
        .expect("the probe reports rather than errors")
    }

    #[tokio::test]
    async fn a_healthy_desktop_passes_every_check() {
        let report = probe_fake(true).await;

        assert!(report.ok, "failures: {:?}", report.failures);
        assert!(report.failures.is_empty());
        assert!(report.warnings.is_empty(), "{:?}", report.warnings);

        let screen = report.screen.expect("a screen");
        assert_eq!((screen.width, screen.height), (1280, 800));

        let shot = report.screenshot.expect("a screenshot");
        assert_eq!((shot.width, shot.height), (1280, 800));
        assert_eq!(shot.matches_screen, Some(true));

        let tree = report.tree.expect("a tree");
        assert_eq!(tree.nodes, 2, "the frame and its button");
        assert_eq!(tree.top_level.len(), 1, "only the frame is a root");
        assert!(tree.top_level[0].contains("Desktop"), "{:?}", tree.top_level);

        assert!(report.execute.is_some(), "--execute ran a snippet");
    }

    #[tokio::test]
    async fn without_execute_nothing_is_run_on_the_desktop() {
        let report = probe_fake(false).await;
        assert!(report.ok);
        assert!(report.execute.is_none());
    }

    #[tokio::test]
    async fn an_unreachable_desktop_fails_every_check_and_says_which() {
        let report = probe(&ProbeOptions {
            // A port nothing listens on: the failure must be reported, not
            // raised, so one run lists all four endpoints.
            computer_url: "http://127.0.0.1:1".to_string(),
            auth_token: None,
            execute: true,
        })
        .await
        .expect("the probe reports");

        assert!(!report.ok);
        assert_eq!(report.failures.len(), 4, "{:?}", report.failures);
        assert!(
            report.failures.iter().all(|line| line.starts_with('/')),
            "each failure names its endpoint: {:?}",
            report.failures
        );
        assert!(report.screen.is_none() && report.tree.is_none());
    }

    #[test]
    fn the_rendered_report_says_not_ready_when_a_check_failed() {
        let report = ProbeReport {
            schema: PROBE_SCHEMA,
            computer_url: "http://127.0.0.1:1".into(),
            screen: None,
            screenshot: None,
            tree: None,
            execute: None,
            warnings: vec![],
            failures: vec!["/screen_size: connection refused".into()],
            ok: false,
            elapsed_ms: 1,
        };
        let text = render(&report);
        assert!(text.contains("FAILED   /screen_size: connection refused"));
        assert!(text.ends_with("not ready\n"));
    }

    #[test]
    fn a_report_round_trips_through_json() {
        let report = ProbeReport {
            schema: PROBE_SCHEMA,
            computer_url: "http://10.0.0.5:5000".into(),
            screen: Some(ScreenCheck {
                width: 1920,
                height: 1080,
                ms: 12,
            }),
            screenshot: Some(ScreenshotCheck {
                width: 1920,
                height: 1080,
                encoded_bytes: 4096,
                matches_screen: Some(true),
                ms: 140,
            }),
            tree: Some(TreeCheck {
                nodes: 3,
                top_level: vec!["desktop frame \"Desktop\"".into()],
                ms: 90,
            }),
            execute: None,
            warnings: vec![],
            failures: vec![],
            ok: true,
            elapsed_ms: 242,
        };
        let json = serde_json::to_string(&report).expect("serializes");
        let back: serde_json::Value = serde_json::from_str(&json).expect("parses");
        assert_eq!(back["schema"], PROBE_SCHEMA);
        assert_eq!(back["screen"]["width"], 1920);
        assert_eq!(back["tree"]["top_level"][0], "desktop frame \"Desktop\"");
        assert!(back.get("execute").is_none(), "omitted when it did not run");
    }
}
