//! The agent's own browser: a WebView this process owns, driven by the DOM.
//!
//! A page rendered inside *another* app's WebView is a black box to every other
//! tool here: the accessibility tree names the native view ("WebView", a scroll
//! bar) and none of the text, so a JavaScript-drawn login form is invisible to
//! `android_ui_tree`, `android_query` and OCR alike. This module opens the page
//! in a WebView Syscity owns and asks the page itself.
//!
//! # Why not the desktop CDP stack
//!
//! The `browser` feature (and `chromiumoxide` behind it) is deliberately off in
//! the `mobile` profile, and the plan says to measure before assuming it can be
//! turned on. Two measurements settle it:
//!
//! * **Size.** Enabling `browser` adds twenty crates to the phone build
//!   (`chromiumoxide{,_cdp,_pdl,_types}`, `async-tungstenite`, `which`, `dunce`,
//!   and the whole `sourcemap` chain) on top of the ~500 the profile already
//!   has.
//! * **Reach.** CDP reaches a WebView only when the app is debuggable
//!   (`setWebContentsDebuggingEnabled`) and only through a
//!   `localabstract:webview_devtools_remote_<pid>` forward. Release APKs — the
//!   build that runs unattended — have no CDP endpoint at all, so the CDP route
//!   would be correct in exactly the build that does not ship.
//!
//! Evaluating a small script in the page works in both builds and needs no
//! dependency that is not already there. That is the route the plan names as
//! the fallback, and it is the one that holds up.
//!
//! # Elements belong to one read
//!
//! `read` numbers the controls it found; `click` and `type` address that
//! numbering. A navigation replaces the document, so the same number names a
//! different control — exactly the stale-`ref` case the accessibility path
//! already handles, and handled the same way: the caller re-reads, and the
//! result says so when an index is gone.
//!
//! # What still applies
//!
//! A press in this browser is still a press: `click` on a "发送" button is
//! classified by [`risk`](super::risk), put to a person through
//! [`gate`](super::gate) when one is reachable, and counted by the repetition
//! ladder. `open` walks the same SSRF guard `web_fetch` does. Nothing about a
//! page being drawn by JavaScript makes it less able to send a message.

use async_trait::async_trait;
use serde_json::{json, Value};

use crate::browser::navigation_guard::{assert_navigation_allowed, NavigationPolicy};
use crate::device::{DeviceBridgeRef, CMD_WEB_VIEW};
use crate::tools::{create_schema, Tool, ToolContext, ToolExecutionResult};

use super::gate;
use super::risk;

/// `page.js`, injected into every document the agent's browser loads.
///
/// One copy, in this crate: the Rust side owns the protocol and the Kotlin
/// side receives the source with `open`, so the two cannot disagree about what
/// a snapshot looks like.
const PAGE_JS: &str = include_str!("page.js");

/// The bridge answered with something this client does not understand.
pub const WEB_BAD_RESPONSE: &str = "WEB_BAD_RESPONSE";
/// No WebView could be created on this device.
pub const WEB_UNAVAILABLE: &str = "WEB_UNAVAILABLE";
/// The element a caller numbered is not in the read it came from.
pub const WEB_ELEMENT_GONE: &str = "WEB_ELEMENT_GONE";
/// The page never finished loading.
pub const WEB_LOAD_FAILED: &str = "WEB_LOAD_FAILED";
/// The executed script threw.
pub const WEB_SCRIPT_FAILED: &str = "WEB_SCRIPT_FAILED";
/// Another page operation is still in flight.
pub const WEB_BUSY: &str = "WEB_BUSY";

/// Why a browser operation did not happen.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WebError {
    /// One of the constants above, or the bridge's own code.
    pub code: String,
    pub message: String,
}

impl WebError {
    fn new(code: &str, message: impl Into<String>) -> Self {
        Self {
            code: code.to_string(),
            message: message.into(),
        }
    }
}

impl std::fmt::Display for WebError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.message)
    }
}

/// One control the page offered.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WebElement {
    /// The number `click` / `type` take.
    pub index: i64,
    /// `button`, `link`, `input`, `select`, or whatever `role` the page set.
    pub role: String,
    pub name: String,
    /// Where it sits in the page, for a caller that wants to describe it.
    pub selector: String,
    /// Its centre in CSS pixels — the page's own space, not the screen's.
    pub x: i32,
    pub y: i32,
    /// The `type` attribute of a field (`password`, `email`, …), when it has one.
    pub kind: String,
}

/// What the page says about itself.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WebPage {
    pub url: String,
    pub title: String,
    /// The visible text, whitespace-collapsed and bounded.
    pub text: String,
    pub elements: Vec<WebElement>,
}

impl WebPage {
    /// Read a snapshot the bridge produced.
    ///
    /// A shape this does not recognise is an error rather than an empty page:
    /// "the page has no text" and "the page could not be described" lead an
    /// agent to different next steps, and only one of them is true.
    pub fn parse(value: &Value) -> Result<Self, WebError> {
        let page = value.get("page").unwrap_or(value);
        let url = page
            .get("url")
            .and_then(Value::as_str)
            .ok_or_else(|| WebError::new(WEB_BAD_RESPONSE, "the page snapshot carries no url"))?;
        let title = page
            .get("title")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        let text = page
            .get("text")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        let elements = page
            .get("elements")
            .and_then(Value::as_array)
            .map(|items| items.iter().filter_map(parse_element).collect())
            .unwrap_or_default();
        Ok(Self {
            url: url.to_string(),
            title,
            text,
            elements,
        })
    }

    /// The control a caller numbered, or a refusal naming the re-read.
    pub fn element(&self, index: i64) -> Result<&WebElement, WebError> {
        self.elements
            .iter()
            .find(|element| element.index == index)
            .ok_or_else(|| {
                WebError::new(
                    WEB_ELEMENT_GONE,
                    format!(
                    "element {index} is not in this page's last read ({} controls). Read the page \
                     again — the numbers belong to one read, and a navigation or a re-render moves \
                     them.",
                    self.elements.len()
                ),
                )
            })
    }
}

fn parse_element(item: &Value) -> Option<WebElement> {
    Some(WebElement {
        index: item.get("i").and_then(Value::as_i64)?,
        role: item
            .get("role")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string(),
        name: item
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string(),
        selector: item
            .get("selector")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string(),
        x: item
            .get("x")
            .and_then(Value::as_i64)
            .and_then(|v| i32::try_from(v).ok())
            .unwrap_or(0),
        y: item
            .get("y")
            .and_then(Value::as_i64)
            .and_then(|v| i32::try_from(v).ok())
            .unwrap_or(0),
        kind: item
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string(),
    })
}

/// One call to the on-device browser, with its answer checked.
///
/// A bridge that is missing is the same answer as a device without a WebView:
/// this tool exists only inside the phone build, and a desktop driving a remote
/// phone has no browser to offer — the caller is told to use the page's
/// accessibility view instead of being left with a transport error.
async fn call(bridge: &DeviceBridgeRef, request: Value) -> Result<Value, WebError> {
    let answer = bridge.call(CMD_WEB_VIEW, request).await.map_err(|e| {
        WebError::new(
            WEB_UNAVAILABLE,
            format!(
                "the phone's browser could not be reached ({e}). It only exists inside the \
                 Syscity app on the phone itself; a desktop driving a remote device can read the \
                 page through the accessibility tree instead."
            ),
        )
    })?;
    match answer.get("ok").and_then(Value::as_bool) {
        Some(true) => Ok(answer),
        Some(false) => Err(WebError::new(
            answer
                .get("code")
                .and_then(Value::as_str)
                .unwrap_or(WEB_BAD_RESPONSE),
            answer
                .get("error")
                .and_then(Value::as_str)
                .unwrap_or("the browser refused without saying why")
                .to_string(),
        )),
        None => Err(WebError::new(WEB_BAD_RESPONSE, "the browser answered without `ok`")),
    }
}

/// Where a URL is allowed to point.
///
/// The same guard `web_fetch` and the desktop browser walk with: private
/// ranges are refused unless the deployment allowed them, and the hostname
/// lists apply. `data:` is admitted for the same reason it is admitted there —
/// it has no origin to resolve and no request to forge.
pub(crate) async fn check_target(url: &str, policy: &NavigationPolicy) -> Result<(), String> {
    if url
        .get(..5)
        .is_some_and(|scheme| scheme.eq_ignore_ascii_case("data:"))
    {
        return Ok(());
    }
    assert_navigation_allowed(url, policy)
        .await
        .map_err(|e| format!("navigation to '{url}' is blocked by the SSRF guard: {e}"))
}

// ── The tool ───────────────────────────────────────────────────────────────

/// How much of a page's text is quoted back to the model.
///
/// A long article is not the point of this tool — reading one page's controls
/// and pressing one of them is — so the text is bounded and the result says
/// whether it was cut.
const TEXT_CHARS: usize = 4000;
/// The most controls one read returns.
const ELEMENT_LIMIT: usize = 200;

/// The agent's browser: `android_web`.
pub struct AdbWebTool {
    device: Option<String>,
}

impl Default for AdbWebTool {
    fn default() -> Self {
        Self::new()
    }
}

impl AdbWebTool {
    pub fn new() -> Self {
        Self { device: None }
    }

    pub fn with_device(mut self, device: String) -> Self {
        self.device = Some(device);
        self
    }
}

#[async_trait]
impl Tool for AdbWebTool {
    fn name(&self) -> &str {
        "android_web"
    }

    fn description(&self) -> &str {
        "Open a page in a browser the agent owns on the phone and drive it by the page itself — \
         for sites that need JavaScript and draw their form in it, where android_ui_tree sees only \
         an empty WebView. `open` loads a URL and returns the page (title, text, controls); `read` \
         re-reads it; `click` and `type` address a control by the number a read gave it; `script` \
         evaluates an expression and returns its JSON value; `back` / `forward` / `reload` move \
         through history. This browser is the agent's own: it has its own cookies and no session \
         from the user's browser, and a click on it is judged and gated like any other. `open` \
         refuses private addresses and non-http schemes."
    }

    fn parameters_schema(&self) -> Value {
        create_schema(
            "Open and drive a web page in the phone's own browser",
            json!({
                "action": {
                    "type": "string",
                    "enum": [
                        "open", "read", "click", "type", "script", "back", "forward", "reload",
                        "close", "status"
                    ],
                    "description": "open loads a URL and answers with the page; read re-reads what is loaded; click/type need the index from a read; script returns an expression's value; status says whether a page is open. close discards the page and its session — the next open starts logged out."
                },
                "url": {
                    "type": "string",
                    "description": "open: the address, http or https. Private addresses and other schemes are refused."
                },
                "index": {
                    "type": "integer",
                    "description": "click / type: the control number from the last read (1-based). Numbers belong to one read — re-read after the page changes."
                },
                "value": {
                    "type": "string",
                    "description": "type: the text to put in the field, replacing what is there."
                },
                "script": {
                    "type": "string",
                    "description": "script: a JavaScript expression, evaluated in the page, whose value is returned as JSON. Use it for a question the snapshot cannot answer (a computed value, an attribute); reading and clicking need no script."
                },
                "verify": {
                    "type": "boolean",
                    "description": "click: read the page again afterwards and say what changed. Default true; false saves a round trip when the page is known not to change."
                },
                "device": { "type": "string", "description": super::DEVICE_PARAM }
            }),
            vec!["action"],
        )
    }

    async fn execute(
        &self,
        args: Value,
        context: &ToolContext,
    ) -> crate::Result<ToolExecutionResult> {
        let device = match super::resolve_device(&args, self.device.as_deref()) {
            Ok(device) => device,
            Err(refusal) => return Ok(ToolExecutionResult::error(refusal)),
        };
        let Some(bridge) = crate::device::bridge().cloned() else {
            return Ok(ToolExecutionResult::error(
                "The phone's own browser only exists inside the Syscity app on the phone itself. \
                 On a desktop driving a remote device, read the page through android_ui_tree (the \
                 WebView node) or android_screenshot instead.",
            ));
        };
        let action = args.get("action").and_then(Value::as_str).unwrap_or("");
        match action {
            "open" => Ok(self.open(&bridge, &device, &args, context).await),
            "read" => Ok(self.read(&bridge, &device).await),
            "click" => Ok(self.click(&bridge, &device, &args, context).await),
            "type" => Ok(self.type_into(&bridge, &device, &args).await),
            "script" => Ok(self.script(&bridge, &device, &args).await),
            "back" | "forward" | "reload" | "close" | "status" => {
                Ok(self.simple(&bridge, &device, action).await)
            }
            other => Ok(ToolExecutionResult::error(format!(
                "Unknown action `{other}`. Use open, read, click, type, script, back, forward, \
                 reload, close or status."
            ))),
        }
    }
}

impl AdbWebTool {
    async fn open(
        &self,
        bridge: &DeviceBridgeRef,
        device: &Option<String>,
        args: &Value,
        _context: &ToolContext,
    ) -> ToolExecutionResult {
        let Some(url) = args.get("url").and_then(Value::as_str).map(str::trim) else {
            return ToolExecutionResult::error("`open` needs a `url`");
        };
        if let Err(refused) = check_target(url, &NavigationPolicy::default()).await {
            return ToolExecutionResult::error(refused);
        }
        let _target = super::lock_device(device).await;
        let request = json!({ "op": "open", "url": url, "shim": PAGE_JS });
        match call(bridge, request).await {
            Ok(answer) => match WebPage::parse(&answer) {
                Ok(page) => describe_page(&page, "Opened"),
                Err(failed) => ToolExecutionResult::error(failed.message),
            },
            Err(failed) => ToolExecutionResult::error(explain(failed)),
        }
    }

    async fn read(&self, bridge: &DeviceBridgeRef, device: &Option<String>) -> ToolExecutionResult {
        let _target = super::lock_device(device).await;
        match call(bridge, json!({ "op": "read" })).await {
            Ok(answer) => match WebPage::parse(&answer) {
                Ok(page) => describe_page(&page, "Read"),
                Err(failed) => ToolExecutionResult::error(failed.message),
            },
            Err(failed) => ToolExecutionResult::error(explain(failed)),
        }
    }

    /// Press a control, through the same judgement a tap gets.
    ///
    /// A page's "发送" is the same send a native button is: it reaches another
    /// person. The label comes from the read the index belongs to, so the
    /// classification costs no extra round trip — and the gate is asked before
    /// the press, not after it.
    async fn click(
        &self,
        bridge: &DeviceBridgeRef,
        device: &Option<String>,
        args: &Value,
        context: &ToolContext,
    ) -> ToolExecutionResult {
        let Some(index) = index_arg(args) else {
            return ToolExecutionResult::error(
                "`click` needs `index`: the number a read gave the control",
            );
        };
        let _target = super::lock_device(device).await;
        let before = match self.read_page(bridge).await {
            Ok(page) => page,
            Err(failed) => return ToolExecutionResult::error(explain(failed)),
        };
        let element = match before.element(index) {
            Ok(element) => element.clone(),
            Err(failed) => return ToolExecutionResult::error(failed.message),
        };

        let offer = risk::classify_label(&element.name).map(|(risk, matched)| risk::Offer {
            risk,
            matched,
            element: Some(element.name.clone()),
        });
        let subject = format!(
            "press \"{}\" on {}{}",
            element.name,
            if before.title.is_empty() {
                before.url.clone()
            } else {
                before.title.clone()
            },
            origin_note(&before.url)
        );
        let package = host_of(&before.url);
        let clearance = match gate::check(offer, context, &subject, package.as_deref()).await {
            Ok(clearance) => clearance,
            Err(refusal) => return ToolExecutionResult::error(refusal.reason),
        };

        let signature = format!("web:click:{}", element.name);
        let guard_key = super::guard::Guard::bucket_key(&context.conversation_id, "web");
        if let Some(reason) = super::guard::Guard::global()
            .check(&guard_key, &signature)
            .refusal()
        {
            return ToolExecutionResult::error(reason.to_string());
        }

        let pressed = match call(bridge, json!({ "op": "click", "index": index })).await {
            Ok(answer) => answer,
            Err(failed) => {
                super::guard::Guard::global().record(
                    &guard_key,
                    &signature,
                    super::guard::Outcome::Unknown,
                );
                return ToolExecutionResult::error(explain(failed));
            }
        };

        // The press happened. Whether the page moved is the next question, and
        // it is the same one a tap has: a control that changes nothing is how
        // an agent ends up pressing it again.
        let verify = args.get("verify").and_then(Value::as_bool).unwrap_or(true);
        let after = if verify {
            // A read that failed after a press says nothing about whether the
            // press worked; the guard records it as `Unknown` rather than
            // counting it as a repeat.
            self.read_page(bridge).await.ok()
        } else {
            None
        };
        let outcome = match &after {
            Some(after) if after.url != before.url => super::guard::Outcome::Changed,
            Some(after) if a11y_text(after) != a11y_text(&before) => super::guard::Outcome::Changed,
            Some(_) => super::guard::Outcome::Unchanged,
            None => super::guard::Outcome::Unknown,
        };
        super::guard::Guard::global().record(&guard_key, &signature, outcome);

        let mut text = format!("Pressed \"{}\"", element.name);
        if let Some(answer) = pressed.get("name").and_then(Value::as_str) {
            if answer != element.name {
                text = format!("Pressed \"{answer}\"");
            }
        }
        match &after {
            Some(after) if after.url != before.url => {
                text.push_str(&format!("; the page went to {}", after.url));
            }
            Some(_) if outcome == super::guard::Outcome::Unchanged => {
                text.push_str("; the page did not change");
            }
            Some(_) => text.push_str("; the page changed"),
            None => {}
        }
        if let Some(note) = clearance.note() {
            text.push_str(&format!(" Judged {note}."));
        }
        match after {
            Some(page) => ToolExecutionResult::success(text).with_data(json!({
                "element": { "index": element.index, "name": element.name, "role": element.role },
                "url": page.url,
                "title": page.title,
                "text": truncated(&page.text),
                "elements": elements_json(&page),
                "changed": outcome == super::guard::Outcome::Changed,
                "risk": clearance.note(),
                "risk_approved": clearance.asked,
            })),
            None => ToolExecutionResult::success(text).with_data(json!({
                "element": { "index": element.index, "name": element.name, "role": element.role },
                "risk": clearance.note(),
                "risk_approved": clearance.asked,
            })),
        }
    }

    async fn type_into(
        &self,
        bridge: &DeviceBridgeRef,
        device: &Option<String>,
        args: &Value,
    ) -> ToolExecutionResult {
        let Some(index) = index_arg(args) else {
            return ToolExecutionResult::error(
                "`type` needs `index`: the number a read gave the field",
            );
        };
        let Some(value) = args.get("value").and_then(Value::as_str) else {
            return ToolExecutionResult::error("`type` needs `value`");
        };
        let _target = super::lock_device(device).await;
        // The field is looked up before it is typed into, so a stale number is
        // a refusal that names the re-read rather than a write into whatever
        // took its place.
        let page = match self.read_page(bridge).await {
            Ok(page) => page,
            Err(failed) => return ToolExecutionResult::error(explain(failed)),
        };
        let element = match page.element(index) {
            Ok(element) => element.clone(),
            Err(failed) => return ToolExecutionResult::error(failed.message),
        };
        match call(bridge, json!({ "op": "type", "index": index, "value": value })).await {
            Ok(answer) => {
                let landed = answer.get("value").and_then(Value::as_str).unwrap_or("");
                let mut text = format!("Typed into \"{}\"", element.name);
                if landed != value {
                    text.push_str(&format!(
                        "; the field now reads \"{landed}\" — the page may have reformatted it"
                    ));
                }
                ToolExecutionResult::success(text).with_data(json!({
                    "element": { "index": element.index, "name": element.name },
                    "value": landed,
                }))
            }
            Err(failed) => ToolExecutionResult::error(explain(failed)),
        }
    }

    async fn script(
        &self,
        bridge: &DeviceBridgeRef,
        device: &Option<String>,
        args: &Value,
    ) -> ToolExecutionResult {
        let Some(source) = args.get("script").and_then(Value::as_str) else {
            return ToolExecutionResult::error("`script` needs the expression to evaluate");
        };
        let _target = super::lock_device(device).await;
        match call(bridge, json!({ "op": "script", "script": source })).await {
            Ok(answer) => {
                let result = answer.get("result").cloned().unwrap_or(Value::Null);
                ToolExecutionResult::success(format!("Script returned: {result}"))
                    .with_data(json!({ "result": result }))
            }
            Err(failed) => ToolExecutionResult::error(explain(failed)),
        }
    }

    async fn simple(
        &self,
        bridge: &DeviceBridgeRef,
        device: &Option<String>,
        action: &str,
    ) -> ToolExecutionResult {
        let _target = super::lock_device(device).await;
        match call(bridge, json!({ "op": action })).await {
            Ok(answer) => match action {
                "status" => {
                    let open = answer.get("open").and_then(Value::as_bool).unwrap_or(false);
                    let url = answer.get("url").and_then(Value::as_str).unwrap_or("");
                    let text = if open {
                        format!("A page is open at {url}.")
                    } else {
                        "No page is open; `open` starts one.".to_string()
                    };
                    ToolExecutionResult::success(text)
                        .with_data(json!({ "open": open, "url": url }))
                }
                "close" => ToolExecutionResult::success(
                    "Closed the page. The next `open` starts a fresh session — cookies from this \
                     one are gone, so a login does not carry over.",
                ),
                _ => {
                    let url = answer.get("url").and_then(Value::as_str).unwrap_or("");
                    match self.read_page(bridge).await {
                        Ok(page) => describe_page(&page, &format!("Went {action}; now at {url}")),
                        Err(_) => {
                            ToolExecutionResult::success(format!("Went {action}; now at {url}"))
                        }
                    }
                }
            },
            Err(failed) => ToolExecutionResult::error(explain(failed)),
        }
    }

    async fn read_page(&self, bridge: &DeviceBridgeRef) -> Result<WebPage, WebError> {
        let answer = call(bridge, json!({ "op": "read" })).await?;
        WebPage::parse(&answer)
    }
}

/// The `index` argument, when it is a positive number.
fn index_arg(args: &Value) -> Option<i64> {
    args.get("index")
        .and_then(Value::as_i64)
        .filter(|index| *index > 0)
}

/// The host a page came from, as the app a press on it belongs to.
///
/// The approval binds to a package; a page is not an app, and the closest true
/// answer is the origin it was served from — a person reading "on example.com"
/// is reading the fact that matters anyway.
fn host_of(url: &str) -> Option<String> {
    url::Url::parse(url)
        .ok()
        .and_then(|parsed| parsed.host_str().map(str::to_string))
}

fn origin_note(url: &str) -> String {
    host_of(url)
        .map(|host| format!(" ({host})"))
        .unwrap_or_default()
}

/// The text a "did the page move" comparison uses.
///
/// Not the raw body: a clock, a cursor or an animated banner changes without
/// anything having happened. Title, url and the controls are what a press is
/// trying to change.
fn a11y_text(page: &WebPage) -> String {
    let mut out = format!("{}\n{}", page.url, page.title);
    for element in &page.elements {
        out.push('\n');
        out.push_str(&element.name);
        out.push('|');
        out.push_str(&element.role);
    }
    out
}

/// If the page's error code is a "start again" answer, add the instruction.
fn explain(failed: WebError) -> String {
    match failed.code.as_str() {
        WEB_ELEMENT_GONE => failed.message,
        WEB_LOAD_FAILED => format!(
            "{} The page did not load. Check the address, or whether the site needs the user's own \
             session — this browser is the agent's, not theirs.",
            failed.message
        ),
        WEB_UNAVAILABLE => failed.message,
        WEB_BUSY => format!("{} Wait for it to finish, then retry.", failed.message),
        WEB_SCRIPT_FAILED => {
            format!("{} The expression threw; the page is unchanged.", failed.message)
        }
        _ => format!("{} (code {})", failed.message, failed.code),
    }
}

fn truncated(text: &str) -> String {
    if text.chars().count() <= TEXT_CHARS {
        return text.to_string();
    }
    let cut: String = text.chars().take(TEXT_CHARS).collect();
    format!("{cut}\n… (text truncated; use `script` for a specific value)")
}

fn elements_json(page: &WebPage) -> Value {
    Value::Array(
        page.elements
            .iter()
            .take(ELEMENT_LIMIT)
            .map(|element| {
                json!({
                    "index": element.index,
                    "role": element.role,
                    "name": element.name,
                    "selector": element.selector,
                    "x": element.x,
                    "y": element.y,
                })
            })
            .collect(),
    )
}

/// One result for a page that was just read or opened.
fn describe_page(page: &WebPage, lead: &str) -> ToolExecutionResult {
    let title = if page.title.is_empty() {
        "(untitled)"
    } else {
        &page.title
    };
    let mut text = format!("{lead} {title} — {}", page.url);
    if !page.text.is_empty() {
        text.push_str("\n\n");
        text.push_str(&truncated(&page.text));
    }
    if page.elements.is_empty() {
        text.push_str(
            "\n\n(no controls found — the page may still be rendering, or it is a \
                       document with nothing to press)",
        );
    } else {
        text.push_str(&format!("\n\nControls ({}):", page.elements.len()));
        for element in page.elements.iter().take(ELEMENT_LIMIT) {
            let kind = if element.kind.is_empty() {
                String::new()
            } else {
                format!("[{}]", element.kind)
            };
            text.push_str(&format!(
                "\n  {:>3}. {}{kind} \"{}\"",
                element.index, element.role, element.name
            ));
        }
    }
    ToolExecutionResult::success(text).with_data(json!({
        "url": page.url,
        "title": page.title,
        "text": truncated(&page.text),
        "elements": elements_json(page),
        "count": page.elements.len(),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::device::tests::MockDeviceBridge;
    use std::sync::Arc;

    /// A page with one button and one field, as the bridge answers a read.
    fn snapshot() -> Value {
        json!({
            "ok": true,
            "page": {
                "url": "https://chat.example.com/login",
                "title": "Sign in",
                "text": "Welcome back",
                "elements": [
                    { "i": 1, "role": "input", "name": "Email", "selector": "#email",
                      "x": 10, "y": 20, "type": "email" },
                    { "i": 2, "role": "input", "name": "Password", "selector": "#pw",
                      "x": 10, "y": 40, "type": "password" },
                    { "i": 3, "role": "button", "name": "Sign in", "selector": "#go",
                      "x": 10, "y": 60, "type": "" }
                ]
            }
        })
    }

    fn page() -> WebPage {
        WebPage::parse(&snapshot()).expect("a page")
    }

    #[test]
    fn a_snapshot_reads_as_controls_and_text() {
        let page = page();
        assert_eq!(page.title, "Sign in");
        assert_eq!(page.text, "Welcome back");
        assert_eq!(page.elements.len(), 3);
        assert_eq!(page.elements[1].kind, "password");
        assert_eq!(page.element(3).expect("the button").name, "Sign in");
    }

    #[test]
    fn an_index_from_another_read_is_refused_with_the_re_read() {
        // Numbers belong to one read; an element that is not in this one must
        // not silently press whatever now sits at that number.
        let failed = page().element(9).expect_err("no such element");
        assert_eq!(failed.code, WEB_ELEMENT_GONE);
        assert!(failed.message.contains("again"), "{}", failed.message);
    }

    #[test]
    fn a_snapshot_without_a_url_is_not_an_empty_page() {
        // "Nothing here" and "this is not a page" are different answers.
        let failed = WebPage::parse(&json!({ "ok": true })).expect_err("no page");
        assert_eq!(failed.code, WEB_BAD_RESPONSE);
    }

    #[test]
    fn a_refusal_keeps_its_own_code() {
        // An empty page is still a page; a refusal is not.
        let blank = WebPage::parse(&json!({ "page": { "url": "about:blank" } })).expect("a page");
        assert!(blank.elements.is_empty());
        let failed = WebError::new(WEB_LOAD_FAILED, "net::ERR_NAME_NOT_RESOLVED");
        assert!(explain(failed).contains("Check the address"));
    }

    #[tokio::test]
    async fn a_private_address_is_refused_before_the_bridge_is_asked() {
        // The same SSRF guard web_fetch walks with. This one is a literal, so
        // it costs no DNS lookup.
        let refused = check_target("http://127.0.0.1:8080/", &NavigationPolicy::default())
            .await
            .expect_err("loopback");
        assert!(refused.contains("SSRF"), "{refused}");
        let refused = check_target("file:///etc/passwd", &NavigationPolicy::default())
            .await
            .expect_err("file scheme");
        assert!(refused.contains("SSRF"), "{refused}");
    }

    #[tokio::test]
    async fn a_public_address_is_admitted() {
        // A literal public address: no resolution, so this does not depend on
        // the machine having a network.
        check_target("https://93.184.216.34/", &NavigationPolicy::default())
            .await
            .expect("a public address");
        check_target("data:text/html,<p>hi</p>", &NavigationPolicy::default())
            .await
            .expect("an inline document has no origin");
    }

    #[tokio::test]
    async fn open_sends_the_shim_with_the_url() {
        let mock = Arc::new(MockDeviceBridge::new(snapshot()));
        let bridge: DeviceBridgeRef = mock.clone();
        let tool = AdbWebTool::new();
        let result = tool
            .open(
                &bridge,
                &None,
                &json!({ "url": "https://chat.example.com/login" }),
                &ToolContext::new("user-1", "conv-1"),
            )
            .await;
        assert!(result.success, "{}", result.output);
        let calls = mock.calls();
        assert_eq!(calls[0].0, CMD_WEB_VIEW);
        assert_eq!(calls[0].1["op"], "open");
        assert_eq!(calls[0].1["url"], "https://chat.example.com/login");
        // The page shim travels with the call: one copy of it, in the crate
        // that owns the protocol.
        assert!(calls[0].1["shim"]
            .as_str()
            .is_some_and(|js| js.contains("__syscity")));
    }

    #[tokio::test]
    async fn a_click_names_the_control_from_the_read() {
        let mock = Arc::new(MockDeviceBridge::new(snapshot()));
        let bridge: DeviceBridgeRef = mock.clone();
        let tool = AdbWebTool::new();
        let result = tool
            .click(
                &bridge,
                &None,
                &json!({ "index": 3, "verify": false }),
                &ToolContext::new("user-1", "conv-1"),
            )
            .await;
        assert!(result.success, "{}", result.output);
        // read, then click — never a click by a number from nowhere.
        let calls = mock.calls();
        assert_eq!(calls[0].1["op"], "read");
        assert_eq!(calls[1].1["op"], "click");
        assert_eq!(calls[1].1["index"], 3);
    }

    #[tokio::test]
    async fn a_stale_index_is_refused_without_pressing_anything() {
        let mock = Arc::new(MockDeviceBridge::new(snapshot()));
        let bridge: DeviceBridgeRef = mock.clone();
        let result = AdbWebTool::new()
            .click(&bridge, &None, &json!({ "index": 9 }), &ToolContext::new("user-1", "conv-1"))
            .await;
        assert!(!result.success);
        // `error()` puts the reason in `error`, not `output`.
        let reason = result.error.as_deref().unwrap_or_default();
        assert!(reason.contains("again"), "{reason}");
        // Only the read happened: nothing was pressed at a number that names
        // no control in it.
        let ops: Vec<String> = mock
            .calls()
            .into_iter()
            .filter_map(|(_, payload)| {
                payload
                    .get("op")
                    .and_then(Value::as_str)
                    .map(str::to_string)
            })
            .collect();
        assert_eq!(ops, vec!["read".to_string()]);
    }

    #[tokio::test]
    async fn typing_replaces_the_field_and_says_what_landed() {
        let mut answer = snapshot();
        answer["value"] = json!("alice@example.com");
        let mock = Arc::new(MockDeviceBridge::new(answer));
        let bridge: DeviceBridgeRef = mock.clone();
        let result = AdbWebTool::new()
            .type_into(&bridge, &None, &json!({ "index": 1, "value": "alice@example.com" }))
            .await;
        assert!(result.success, "{}", result.output);
        assert_eq!(
            result
                .data
                .as_ref()
                .and_then(|d| d.get("value"))
                .and_then(Value::as_str),
            Some("alice@example.com")
        );
    }

    #[tokio::test]
    async fn a_page_that_did_not_change_is_said_so() {
        // The same snapshot twice: the press did nothing, and the result says
        // it rather than leaving the model to press again.
        let mock = Arc::new(MockDeviceBridge::new(snapshot()));
        let bridge: DeviceBridgeRef = mock.clone();
        let result = AdbWebTool::new()
            .click(&bridge, &None, &json!({ "index": 3 }), &ToolContext::new("user-1", "conv-1"))
            .await;
        assert!(result.success, "{}", result.output);
        assert!(result.output.contains("did not change"), "{}", result.output);
        assert_eq!(result.data.as_ref().and_then(|d| d.get("changed")), Some(&json!(false)));
    }

    #[test]
    fn the_host_a_page_came_from_is_the_app_the_press_belongs_to() {
        assert_eq!(host_of("https://chat.example.com/login"), Some("chat.example.com".to_string()));
        assert_eq!(host_of("about:blank"), None);
        assert_eq!(origin_note("https://chat.example.com/x"), " (chat.example.com)");
    }

    #[test]
    fn a_long_page_is_truncated_and_says_so() {
        let long = "x".repeat(TEXT_CHARS + 10);
        let cut = truncated(&long);
        // The quoted body is bounded; the note about it is not part of the cut.
        let body = cut.split('\n').next().unwrap_or_default();
        assert_eq!(body.chars().count(), TEXT_CHARS);
        assert!(cut.ends_with("truncated; use `script` for a specific value)"));
        assert_eq!(truncated("short"), "short");
    }

    #[test]
    fn the_tool_is_named_for_what_it_opens() {
        assert_eq!(AdbWebTool::new().name(), "android_web");
    }
}
