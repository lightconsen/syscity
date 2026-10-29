//! Local GUI regression baseline.
//!
//! The ladder's page rung is the part of computer use that can be checked
//! honestly on one machine: a real browser, a real DOM, a real click. Each
//! scenario below is a small page plus the action a user would describe
//! ("click Sign in", "type the email") and the state the page must be in
//! afterwards. The runner reports a pass rate, so a change that makes the
//! rung stricter (or sloppier) shows up as a number rather than a feeling.
//!
//! These drive a real headless Chrome, so they only run when asked for:
//!
//! ```text
//! SYSCITY_GUI_BASELINE=1 cargo test --test gui_baseline -- --nocapture
//! ```
//!
//! Ordinary `cargo test` runs compile them but skip the bodies — no Chrome
//! needed. The pages are `data:` URLs: nothing leaves the machine. A machine
//! without Chrome is not a regression, so a launch failure skips the run
//! instead of failing it.

#![cfg(feature = "browser")]

use std::sync::Arc;

use serde_json::json;

use syscity::browser::page_actor::BrowserPageActor;
use syscity::browser::pool::BrowserPool;
use syscity::browser::profile::{BrowserPoolConfig, BrowserProfile};
use syscity::computer::{ActionResult, PageAction, PageActor, PageTarget};
use syscity::tools::browser::BrowserTool;
use syscity::tools::{Tool, ToolContext, ToolIdentity, ToolModel, ToolSandbox};

fn baseline_enabled() -> bool {
    std::env::var("SYSCITY_GUI_BASELINE").is_ok_and(|v| v == "1")
}

fn tool_context(conversation: &str) -> ToolContext {
    ToolContext {
        media_provider: None,
        identity: ToolIdentity {
            user_id: "gui-baseline".to_string(),
            conversation_id: conversation.to_string(),
            sender_id: None,
        },
        sandbox: ToolSandbox::default(),
        model: ToolModel::default(),
        delegation: None,
        ask_queue: None,
        approval_queue: None,
        skill_trust_ceiling: None,
    }
}

/// A tool and pool with a private profile directory: Chrome refuses to start
/// twice on one profile, and the baseline may run beside the browser e2e
/// suite.
fn pooled_tool(name: &str) -> (BrowserTool, Arc<BrowserPool>) {
    let dir =
        std::env::temp_dir().join(format!("syscity-gui-baseline-{name}-{}", std::process::id()));
    let mut profile = BrowserProfile::new(name);
    profile.user_data_dir = Some(dir);
    let config = BrowserPoolConfig {
        idle_timeout_secs: 600,
        default_profile: name.to_string(),
        ..BrowserPoolConfig::default()
    };
    let pool = Arc::new(BrowserPool::with_profiles(config, vec![profile]));
    let tool = BrowserTool::new()
        .with_profile(name)
        .with_pool(Arc::clone(&pool));
    (tool, pool)
}

/// One action, described the way a user describes it.
enum Step {
    Click {
        role: &'static str,
        name: &'static str,
    },
    Fill {
        role: &'static str,
        name: &'static str,
        text: &'static str,
    },
}

/// What the page must look like once the steps have run.
enum Expect {
    /// The script reads `value` afterwards.
    Value(&'static str, &'static str),
    /// The steps are refused, and the script still reads `value` — nothing
    /// else was clicked in place of the element that was named.
    Refused(&'static str, &'static str),
}

struct Scenario {
    id: &'static str,
    html: &'static str,
    steps: &'static [Step],
    expect: Expect,
}

const PAGE_HEAD: &str =
    "<html><head><title></title></head><body style=\"font-family:sans-serif;padding:16px\">";

fn scenarios() -> Vec<Scenario> {
    vec![
        Scenario {
            id: "click_by_name",
            html: "<button onclick=\"document.title='signed-in'\">Sign in</button>",
            steps: &[Step::Click {
                role: "button",
                name: "Sign in",
            }],
            expect: Expect::Value("return document.title;", "signed-in"),
        },
        Scenario {
            id: "fill_by_name",
            html: "<label for=\"email\">Email</label>\
                   <input id=\"email\" type=\"text\" aria-label=\"Email\">",
            steps: &[Step::Fill {
                role: "text field",
                name: "Email",
                text: "a@b.c",
            }],
            expect: Expect::Value("return document.getElementById('email').value;", "a@b.c"),
        },
        Scenario {
            id: "checkbox_by_name",
            html: "<input id=\"remember\" type=\"checkbox\" aria-label=\"Remember me\" \
                   onchange=\"document.title=this.checked?'on':'off'\">",
            steps: &[Step::Click {
                role: "check box",
                name: "Remember me",
            }],
            expect: Expect::Value(
                "return document.getElementById('remember').checked ? 'on' : 'off';",
                "on",
            ),
        },
        Scenario {
            // A plain `href="#settings"` does not move a `data:` URL — the
            // fragment navigation is refused on an opaque origin — so the link
            // records the click itself.
            id: "link_by_name",
            html: "<a href=\"#settings\" onclick=\"document.title='settings'\">Settings</a>",
            steps: &[Step::Click { role: "link", name: "Settings" }],
            expect: Expect::Value("return document.title;", "settings"),
        },
        Scenario {
            // The failure this guards against: clicking a plausible neighbour
            // when the named element does not exist.
            id: "an_absent_name_is_refused",
            html: "<button onclick=\"document.title='deleted'\">Delete draft</button>",
            steps: &[Step::Click {
                role: "button",
                name: "Delete account",
            }],
            expect: Expect::Refused("return document.title;", ""),
        },
    ]
}

fn data_url(html: &str) -> String {
    format!(
        "data:text/html;charset=utf-8,{}",
        urlencoding::encode(&format!("{PAGE_HEAD}{html}</body></html>"))
    )
}

/// Run the steps through the page actor, the way the ladder reaches it.
async fn run_steps(
    actor: &BrowserPageActor,
    owner: &str,
    steps: &[Step],
) -> Result<Vec<ActionResult>, String> {
    let mut results = Vec::new();
    for step in steps {
        let (role, name, action) = match step {
            Step::Click { role, name } => (*role, *name, PageAction::Click),
            Step::Fill { role, name, text } => (*role, *name, PageAction::Type((*text).into())),
        };
        let target = PageTarget::new(role, Some(name.to_string()));
        match actor.act(owner, &target, action).await {
            Ok(result) => results.push(result),
            Err(error) => return Err(format!("{role} \"{name}\": {error}")),
        }
    }
    Ok(results)
}

/// Read a value out of the page through the browser tool.
async fn read_value(
    tool: &BrowserTool,
    context: &ToolContext,
    script: &str,
) -> Result<String, String> {
    let result = tool
        .execute(json!({ "actions": [{ "ExecuteScript": { "script": script } }] }), context)
        .await
        .map_err(|e| format!("script: {e}"))?;
    if !result.success {
        return Err(result.error.unwrap_or_else(|| "script failed".to_string()));
    }
    // Each action result is serialized as `{"Ok": {...}}` / `{"Err": "..."}`.
    let action = result
        .data
        .as_ref()
        .and_then(|data| data.get("results"))
        .and_then(|results| results.as_array())
        .and_then(|results| results.first())
        .cloned()
        .unwrap_or(json!(null));
    if let Some(error) = action.get("Err").and_then(|error| error.as_str()) {
        return Err(error.to_string());
    }
    let value = action
        .get("Ok")
        .and_then(|ok| ok.get("result"))
        .cloned()
        .unwrap_or(json!(null));
    Ok(match value {
        serde_json::Value::String(text) => text,
        other => other.to_string(),
    })
}

async fn run_scenario(
    tool: &BrowserTool,
    actor: &BrowserPageActor,
    scenario: &Scenario,
) -> Result<(), String> {
    let context = tool_context(scenario.id);
    let owner = context.identity.conversation_id.clone();

    let navigate = tool
        .execute(
            json!({ "actions": [{ "Navigate": { "url": data_url(scenario.html) } }] }),
            &context,
        )
        .await
        .map_err(|e| format!("navigate: {e}"))?;
    if !navigate.success {
        return Err(format!(
            "navigate: {}",
            navigate.error.unwrap_or_else(|| navigate.output.clone())
        ));
    }

    let outcome = run_steps(actor, &owner, scenario.steps).await;

    match scenario.expect {
        Expect::Value(script, expected) => {
            outcome?;
            let actual = read_value(tool, &context, script).await?;
            if actual == expected {
                Ok(())
            } else {
                Err(format!("expected {expected:?}, page reads {actual:?}"))
            }
        }
        Expect::Refused(script, expected) => match outcome {
            Ok(_) => Err("the action was performed instead of refused".to_string()),
            Err(_refusal) => {
                let actual = read_value(tool, &context, script).await?;
                if actual == expected {
                    Ok(())
                } else {
                    Err(format!(
                        "refused, but the page changed: expected {expected:?}, reads {actual:?}"
                    ))
                }
            }
        },
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn the_page_rung_holds_its_baseline() {
    if !baseline_enabled() {
        eprintln!("skipped: set SYSCITY_GUI_BASELINE=1 to run the GUI baseline");
        return;
    }

    let (tool, pool) = pooled_tool("baseline");
    let actor = BrowserPageActor::new(Arc::clone(&pool), "baseline");

    let scenarios = scenarios();
    let total = scenarios.len();
    let mut passed = 0;
    let mut failures = Vec::new();

    for scenario in &scenarios {
        match run_scenario(&tool, &actor, scenario).await {
            Ok(()) => {
                passed += 1;
                eprintln!("  ok   {}", scenario.id);
            }
            Err(reason) => {
                eprintln!("  FAIL {}: {reason}", scenario.id);
                failures.push(format!("{}: {reason}", scenario.id));
            }
        }
    }

    let rate = passed as f64 / total as f64 * 100.0;
    eprintln!("GUI baseline: {passed}/{total} ({rate:.0}%)");

    assert!(failures.is_empty(), "GUI baseline regressed:\n{}", failures.join("\n"));
}
