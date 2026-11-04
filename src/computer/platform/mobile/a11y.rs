//! The accessibility tree, read in-process instead of over ADB.
//!
//! `uiautomator dump` is a subprocess that writes an XML file to `/sdcard` and
//! reads it back: hundreds of milliseconds, a file that has to be pulled, and
//! only on the default display. The platform's own accessibility API is the
//! same data one call away — when the host is the phone itself and Syscity's
//! [`SyscityAccessibilityService`] is enabled, the Kotlin side hands the tree
//! over through the device bridge.
//!
//! What comes back is deliberately converted into the *same* [`UiElement`] list
//! the ADB path produces. Everything downstream — the indexed tap safety net,
//! the effect check, the tree-quality verdict, the OCR fallback — then works
//! unchanged, and switching sources changes nothing but latency and coverage.
//!
//! Unavailable is a normal answer, not an error: the service may be off, the
//! screen may be locked, or the caller may be driving a *different* device over
//! ADB. Each of those falls back to the uiautomator path.

use serde_json::Value;

use super::android::{NodeState, UiElement};
use crate::device::{DeviceBridgeRef, CMD_A11Y_TREE};

/// The service is not enabled (or not connected yet).
pub const A11Y_DISABLED: &str = "A11Y_DISABLED";
/// The bridge answered, but not with a tree this code understands.
pub const A11Y_BAD_PAYLOAD: &str = "A11Y_BAD_PAYLOAD";
/// The bridge call itself failed.
pub const A11Y_BRIDGE_ERROR: &str = "A11Y_BRIDGE_ERROR";

/// Why the tree could not be read from the service.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct A11yUnavailable {
    /// One of the constants above.
    pub code: &'static str,
    pub message: String,
}

impl A11yUnavailable {
    fn new(code: &'static str, message: impl Into<String>) -> Self {
        Self { code, message: message.into() }
    }
}

/// The handle for one node of a read.
///
/// `ref` names the node itself — the same integer the Kotlin side keeps in its
/// table — which is what lets an action be addressed to a control rather than
/// to a point. It is deliberately *not* part of [`UiElement`]: a ref lives in
/// the phone's process and expires with the node, while the key is the
/// element's identity and survives across reads.
pub type NodeRef = u32;

/// One read of the screen through the accessibility service.
#[derive(Debug, Clone, PartialEq)]
pub struct A11ySnapshot {
    /// The app that owns the focused window, when the platform said.
    pub package: Option<String>,
    /// Full display size, for the coordinate-space check.
    pub display: Option<(i32, i32)>,
    /// Indices are assigned here: `elements[i].index == i + 1`.
    pub elements: Vec<UiElement>,
    /// Nodes the walk dropped at the cap, so the model knows the list is short.
    pub truncated: bool,
}

impl A11ySnapshot {
    /// A screen the model can address.
    pub fn is_addressable(&self) -> bool {
        !self.elements.is_empty()
    }
}

/// Whether a node is worth handing to the model.
///
/// The same rule the ADB path applies to `uiautomator` nodes: a node earns its
/// place by being actionable or by saying something. Bare layout containers
/// carry neither and cost context on every turn.
fn keep(node: &Value) -> bool {
    let text = node
        .get("text")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim();
    let desc = node
        .get("desc")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim();
    let rid = node.get("rid").and_then(Value::as_str).unwrap_or("").trim();
    let clickable = node
        .get("clickable")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    // A structural node is worth keeping even when it says nothing: an unnamed
    // list is where the scroll has to go, and an unnamed field is where a write
    // goes. Dropping them here would throw away the only handle on either.
    clickable
        || !text.is_empty()
        || !desc.is_empty()
        || !rid.is_empty()
        || state_of(node).is_structural()
}

/// Read the node's semantics, whichever fields the bridge sent.
///
/// Older bridge builds carry none of these, so every field defaults to `false`
/// and the element behaves exactly as it did before.
fn state_of(node: &Value) -> NodeState {
    let flag = |key: &str| node.get(key).and_then(Value::as_bool).unwrap_or(false);
    let actions = node.get("scrollActions").and_then(Value::as_array);
    NodeState {
        scrollable: flag("scrollable"),
        editable: flag("editable"),
        checkable: flag("checkable"),
        checked: flag("checked"),
        selected: flag("selected"),
        focused: flag("focused"),
        password: flag("password"),
        long_clickable: flag("longClickable"),
        // `scrollActions` is `[forward, backward]`. Absent (an older bridge, or
        // a source without action info) falls back to plain scrollability,
        // which is the pre-existing behaviour: try, and let the result say.
        scroll_forward: actions
            .and_then(|a| a.first())
            .and_then(Value::as_bool)
            .unwrap_or_else(|| flag("scrollable")),
        scroll_backward: actions
            .and_then(|a| a.get(1))
            .and_then(Value::as_bool)
            .unwrap_or_else(|| flag("scrollable")),
        depth: node
            .get("depth")
            .and_then(Value::as_i64)
            .and_then(|depth| u32::try_from(depth).ok())
            .unwrap_or(0),
        // Only the bridge can decide this: the judgement is about the node's
        // children, and `uiautomator` drops that hierarchy. A bridge that does
        // not send the flag says nothing, which is the safe answer.
        wheel: flag("wheel"),
    }
}

/// The node handle a bridge node carried, if it sent one.
///
/// `None` for a source that does not hand out refs (`uiautomator`), which is
/// exactly the case where an action has to be a gesture instead.
pub fn node_ref(node: &Value) -> Option<NodeRef> {
    node.get("ref")
        .and_then(Value::as_u64)
        .and_then(|value| NodeRef::try_from(value).ok())
        .filter(|value| *value > 0)
}

/// `[x1, y1, x2, y2]`, however the platform spelled it.
fn bounds_of(node: &Value) -> Option<(i32, i32, i32, i32)> {
    let values = node.get("bounds")?.as_array()?;
    if values.len() != 4 {
        return None;
    }
    let mut out = [0i32; 4];
    for (slot, value) in out.iter_mut().zip(values) {
        *slot = value.as_i64()?.try_into().ok()?;
    }
    Some((out[0], out[1], out[2], out[3]))
}

/// The class name as the model sees it in the ADB path: last segment only.
///
/// Both sources feed the same `UiElement` list, so a row must look the same
/// whichever one produced it — otherwise a model that learned "Button" from one
/// read would not recognise the same control on the next.
fn short_class(node: &Value) -> String {
    string_of(node, "cls")
        .map(|class| {
            class
                .rsplit('.')
                .next()
                .unwrap_or(class.as_str())
                .to_string()
        })
        .unwrap_or_else(|| "View".to_string())
}

fn string_of(node: &Value, key: &str) -> Option<String> {
    node.get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
}

/// Parse the bridge's answer into the shape the tools use.
pub fn parse(value: &Value) -> Result<A11ySnapshot, A11yUnavailable> {
    let enabled = value
        .get("enabled")
        .and_then(Value::as_bool)
        .ok_or_else(|| {
            A11yUnavailable::new(
                A11Y_BAD_PAYLOAD,
                "the accessibility service answered without saying whether it is enabled",
            )
        })?;
    if !enabled {
        return Err(A11yUnavailable::new(
            A11Y_DISABLED,
            "Syscity's accessibility service is not enabled — turn it on in Settings › \
             Accessibility › Syscity (it is what lets the agent read and operate other apps)",
        ));
    }

    let display = value.get("display").and_then(|display| {
        let width = display.get("width")?.as_i64()?.try_into().ok()?;
        let height = display.get("height")?.as_i64()?.try_into().ok()?;
        Some((width, height))
    });

    let nodes = value
        .get("nodes")
        .and_then(Value::as_array)
        .ok_or_else(|| {
            A11yUnavailable::new(A11Y_BAD_PAYLOAD, "the accessibility tree carried no node list")
        })?;

    let mut elements = Vec::new();
    for node in nodes.iter().filter(|node| keep(node)) {
        let Some(bounds) = bounds_of(node) else {
            continue;
        };
        let (x1, y1, x2, y2) = bounds;
        if x2 <= x1 || y2 <= y1 {
            // A zero-area node cannot be tapped and says nothing about layout.
            continue;
        }
        let mut element = UiElement::with_state(
            elements.len() + 1,
            string_of(node, "text"),
            string_of(node, "desc"),
            string_of(node, "rid"),
            short_class(node),
            node.get("clickable")
                .and_then(Value::as_bool)
                .unwrap_or(false),
            node.get("enabled").and_then(Value::as_bool).unwrap_or(true),
            state_of(node),
            bounds,
        );
        element.node_ref = node_ref(node);
        elements.push(element);
    }

    Ok(A11ySnapshot {
        package: string_of(value, "package"),
        display,
        elements,
        truncated: value
            .get("truncated")
            .and_then(Value::as_bool)
            .unwrap_or(false),
    })
}

/// Ask the running accessibility service for a screen.
///
/// `display` selects a virtual display — the agent's own screen, which the
/// service finds through the live window list rather than as the "active
/// window". `None` is the user's screen.
pub async fn fetch(
    bridge: &DeviceBridgeRef,
    display: Option<i32>,
) -> Result<A11ySnapshot, A11yUnavailable> {
    let payload = match display {
        Some(id) => serde_json::json!({ "display": id }),
        None => serde_json::json!({}),
    };
    let answer = bridge.call(CMD_A11Y_TREE, payload).await.map_err(|e| {
        A11yUnavailable::new(
            A11Y_BRIDGE_ERROR,
            format!("the accessibility service could not be reached ({e})"),
        )
    })?;
    parse(&answer)
}

// ── Actions on a node ──────────────────────────────────────────────────────

/// The node is gone: the reference names something that is no longer on screen.
pub const ACTION_NODE_GONE: &str = "node_gone";
/// The node is there but does not offer this action.
pub const ACTION_UNKNOWN: &str = "unknown_action";
/// The node offered the action and refused it.
pub const ACTION_REFUSED: &str = "action_refused";
/// The service stopped part-way through a repeated action.
pub const ACTION_STOPPED_EARLY: &str = "stopped_early";
/// The service is not running, so a gesture cannot be dispatched at all.
pub const ACTION_NO_SERVICE: &str = "no_service";

/// What to do to a node, in the bridge's vocabulary.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NodeAction {
    Click,
    LongClick,
    Focus,
    ScrollForward,
    ScrollBackward,
    SetText,
}

impl NodeAction {
    /// The string the Kotlin side switches on.
    pub fn wire(self) -> &'static str {
        match self {
            Self::Click => "click",
            Self::LongClick => "longClick",
            Self::Focus => "focus",
            Self::ScrollForward => "scrollForward",
            Self::ScrollBackward => "scrollBackward",
            Self::SetText => "setText",
        }
    }

    /// The action a press on a control is, when the control offers one.
    pub fn press(long: bool) -> Self {
        if long {
            Self::LongClick
        } else {
            Self::Click
        }
    }
}

/// What the bridge said about one action.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActionAnswer {
    /// The node performed the action — all `accepted` times.
    pub ok: bool,
    /// How many times it was accepted (one for everything but a repeated scroll).
    pub accepted: i64,
    /// Why not, when it was not.
    pub reason: Option<String>,
}

impl ActionAnswer {
    /// Read the bridge's answer, tolerating an older build.
    ///
    /// A missing `ok` is "not done": the one thing this answer must never do is
    /// claim an action happened when the answer did not say so.
    pub fn parse(value: &Value) -> Self {
        Self {
            ok: value.get("ok").and_then(Value::as_bool).unwrap_or(false),
            accepted: value.get("accepted").and_then(Value::as_i64).unwrap_or(0),
            reason: value
                .get("reason")
                .and_then(Value::as_str)
                .map(str::to_string),
        }
    }

    /// The node is gone — the screen moved under the reference.
    ///
    /// This is deliberately *not* a fall-through: pressing the same
    /// coordinates would now hit whatever has taken the node's place, which is
    /// exactly the mistake the reference existed to prevent. The caller
    /// re-reads instead.
    pub fn node_is_gone(&self) -> bool {
        self.reason.as_deref() == Some(ACTION_NODE_GONE)
    }

    /// The node refused, so the next way of pressing can be tried.
    ///
    /// Unlike a gone node, the control is still where the read said it was, so
    /// a gesture or a shell injection lands on the thing that was meant.
    pub fn falls_through(&self) -> bool {
        !self.ok && !self.node_is_gone()
    }

    /// The answer as the result line says it.
    pub fn describe(&self) -> String {
        match self.reason.as_deref() {
            None if self.ok => "the control reported the press as done".to_string(),
            None => "the bridge did not say what the press did".to_string(),
            Some(ACTION_NODE_GONE) => {
                "the control it named is gone — the screen changed under the reference, so it \
                 was not pressed by coordinate instead"
                    .to_string()
            }
            Some(ACTION_UNKNOWN) => {
                "the control does not offer that action, so it was pressed as a point instead"
                    .to_string()
            }
            Some(ACTION_REFUSED) => {
                "the control has the action but did not take it, so it was pressed as a point \
                 instead"
                    .to_string()
            }
            Some(other) => format!("the control declined ({other})"),
        }
    }
}

/// Run one action on the node `node` names.
///
/// `text` is what `setText` writes; every other action ignores it. The answer
/// is always an answer — see [`ActionAnswer`] — because the caller has a next
/// way of pressing and needs to know why this one did not work.
pub async fn act(
    bridge: &DeviceBridgeRef,
    node: NodeRef,
    action: NodeAction,
    text: Option<&str>,
    count: i32,
) -> Option<ActionAnswer> {
    let bridge = bridge.clone();
    let mut payload = serde_json::json!({
        "ref": node,
        "action": action.wire(),
        "count": count.max(1),
    });
    if let Some(text) = text {
        payload["text"] = Value::String(text.to_string());
    }
    bridge
        .call(crate::device::CMD_A11Y_ACTION, payload)
        .await
        .ok()
        .map(|answer| ActionAnswer::parse(&answer))
}

/// Whether the service took a gesture it was handed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GestureAnswer {
    /// The platform completed the gesture.
    pub ok: bool,
    /// Why not — `no_service`, `cancelled`, `timeout`, `not_dispatched`.
    pub reason: Option<String>,
}

impl GestureAnswer {
    pub fn parse(value: &Value) -> Self {
        Self {
            ok: value.get("ok").and_then(Value::as_bool).unwrap_or(false),
            reason: value
                .get("reason")
                .and_then(Value::as_str)
                .map(str::to_string),
        }
    }

    /// The service is not running, so this rung does not exist here.
    pub fn no_service(&self) -> bool {
        !self.ok && self.reason.as_deref() == Some(ACTION_NO_SERVICE)
    }
}

/// Dispatch one or two fingers through the accessibility service.
///
/// This is the middle rung: a gesture the platform sends as a real touch
/// stream, which reaches a view that ignores injected shell events. It only
/// exists while the service is running, and only on the display the service
/// sees — a virtual display's gestures go through the display helper instead.
pub async fn gesture(
    bridge: &DeviceBridgeRef,
    paths: &[Vec<(i32, i32)>],
    duration_ms: u32,
) -> Option<GestureAnswer> {
    let bridge = bridge.clone();
    let paths: Vec<Value> = paths
        .iter()
        .map(|path| {
            Value::Array(
                path.iter()
                    .map(|(x, y)| serde_json::json!([x, y]))
                    .collect(),
            )
        })
        .collect();
    let payload = serde_json::json!({ "paths": paths, "duration_ms": duration_ms });
    bridge
        .call(crate::device::CMD_A11Y_GESTURE, payload)
        .await
        .ok()
        .map(|answer| GestureAnswer::parse(&answer))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn node(cls: &str, text: &str, bounds: [i64; 4], clickable: bool) -> Value {
        serde_json::json!({
            "cls": cls,
            "text": text,
            "desc": "",
            "rid": "",
            "clickable": clickable,
            "enabled": true,
            "bounds": bounds,
        })
    }

    #[test]
    fn a_disabled_service_is_a_fallback_not_an_error() {
        let refused = parse(&serde_json::json!({ "enabled": false })).expect_err("disabled");
        assert_eq!(refused.code, A11Y_DISABLED);
        assert!(refused.message.contains("Settings"), "{}", refused.message);
    }

    #[test]
    fn an_answer_without_the_enabled_flag_is_a_bad_payload() {
        let bad = parse(&serde_json::json!({ "nodes": [] })).expect_err("bad");
        assert_eq!(bad.code, A11Y_BAD_PAYLOAD);
    }

    #[test]
    fn nodes_are_numbered_from_one_in_document_order() {
        let payload = serde_json::json!({
            "enabled": true,
            "display": { "width": 1080, "height": 2400 },
            "package": "com.android.settings",
            "nodes": [
                node("android.widget.TextView", "Wi-Fi", [0, 100, 200, 160], false),
                node("android.widget.Button", "OK", [900, 2200, 1060, 2320], true),
            ],
        });
        let snapshot = parse(&payload).expect("tree");
        assert_eq!(snapshot.package.as_deref(), Some("com.android.settings"));
        assert_eq!(snapshot.display, Some((1080, 2400)));
        assert_eq!(snapshot.elements.len(), 2);
        assert_eq!(snapshot.elements[0].index, 1);
        assert_eq!(snapshot.elements[1].index, 2);
        assert_eq!(snapshot.elements[1].center, (980, 2260));
        assert!(snapshot.elements[1].clickable);
    }

    #[test]
    fn anonymous_containers_are_dropped() {
        let payload = serde_json::json!({
            "enabled": true,
            "nodes": [
                node("android.widget.FrameLayout", "", [0, 0, 100, 100], false),
                node("android.widget.TextView", "shown", [0, 0, 100, 100], false),
            ],
        });
        let snapshot = parse(&payload).expect("tree");
        assert_eq!(snapshot.elements.len(), 1);
        assert_eq!(snapshot.elements[0].text.as_deref(), Some("shown"));
        // Re-numbered after filtering: the model's [1] is the row it can see.
        assert_eq!(snapshot.elements[0].index, 1);
    }

    #[test]
    fn a_content_description_keeps_an_icon_button() {
        let payload = serde_json::json!({
            "enabled": true,
            "nodes": [{
                "cls": "android.widget.ImageButton",
                "text": "", "desc": "Back", "rid": "",
                "clickable": true, "enabled": true, "bounds": [0, 0, 96, 96],
            }],
        });
        let snapshot = parse(&payload).expect("tree");
        assert_eq!(snapshot.elements[0].content_desc.as_deref(), Some("Back"));
    }

    #[test]
    fn zero_area_and_malformed_nodes_are_skipped() {
        let payload = serde_json::json!({
            "enabled": true,
            "nodes": [
                node("android.widget.TextView", "flat", [10, 10, 10, 10], false),
                { "cls": "android.widget.TextView", "text": "no bounds" },
                node("android.widget.TextView", "fine", [0, 0, 10, 10], false),
            ],
        });
        let snapshot = parse(&payload).expect("tree");
        assert_eq!(snapshot.elements.len(), 1);
        assert_eq!(snapshot.elements[0].text.as_deref(), Some("fine"));
    }

    #[test]
    fn truncation_is_reported() {
        let payload = serde_json::json!({ "enabled": true, "truncated": true, "nodes": [] });
        assert!(parse(&payload).expect("tree").truncated);
    }

    #[test]
    fn semantics_reach_the_element() {
        let payload = serde_json::json!({
            "enabled": true,
            "nodes": [{
                "cls": "android.widget.EditText", "text": "hunter2", "desc": "", "rid": "id/pw",
                "clickable": true, "enabled": true, "depth": 4,
                "scrollable": true, "editable": true, "checkable": true, "checked": true,
                "selected": true, "focused": true, "password": true, "longClickable": true,
                "scrollActions": [true, false],
                "bounds": [0, 0, 100, 40],
            }],
        });
        let element = &parse(&payload).expect("tree").elements[0];
        assert!(element.state.scrollable);
        assert!(element.state.editable);
        assert!(element.state.checkable);
        assert!(element.state.checked);
        assert!(element.state.selected);
        assert!(element.state.focused);
        assert!(element.state.password);
        assert!(element.state.long_clickable);
        assert!(element.state.scroll_forward);
        assert!(!element.state.scroll_backward);
        assert_eq!(element.state.depth, 4);
    }

    #[test]
    fn an_unnamed_scroll_list_survives_the_filter() {
        // The list is where a scroll has to go; dropping it for having no text
        // is what made every scroll a blind swipe down the middle.
        let payload = serde_json::json!({
            "enabled": true,
            "nodes": [
                { "cls": "androidx.recyclerview.widget.RecyclerView", "text": "", "desc": "",
                  "rid": "", "clickable": false, "enabled": true, "scrollable": true,
                  "bounds": [0, 200, 1080, 2000] },
                node("android.widget.FrameLayout", "", [0, 0, 1080, 100], false),
            ],
        });
        let elements = parse(&payload).expect("tree").elements;
        assert_eq!(elements.len(), 1);
        assert_eq!(elements[0].class, "RecyclerView");
        assert!(elements[0].state.scrollable);
    }

    #[test]
    fn an_older_bridge_without_semantics_still_parses() {
        let payload = serde_json::json!({
            "enabled": true,
            "nodes": [{
                "cls": "android.widget.Button", "text": "OK", "desc": "", "rid": "",
                "clickable": true, "enabled": true, "bounds": [0, 0, 100, 50],
            }],
        });
        let element = &parse(&payload).expect("tree").elements[0];
        assert_eq!(element.state, NodeState::default());
    }

    #[test]
    fn semantics_do_not_change_identity() {
        let base = serde_json::json!({
            "enabled": true,
            "nodes": [
                { "cls": "android.widget.CheckBox", "text": "Remember me", "desc": "",
                  "rid": "id/remember", "clickable": true, "enabled": true,
                  "bounds": [0, 0, 100, 50] },
                { "cls": "android.widget.CheckBox", "text": "Remember me", "desc": "",
                  "rid": "id/remember", "clickable": true, "enabled": true,
                  "bounds": [0, 0, 100, 50], "checkable": true, "checked": true },
            ],
        });
        let elements = parse(&base).expect("tree").elements;
        // Ticking the box is a state change, not a different element.
        assert_eq!(elements[0].key, elements[1].key);
    }

    #[tokio::test]
    async fn fetch_uses_the_tree_command_and_parses_the_answer() {
        let bridge = crate::device::tests::MockDeviceBridge::new(serde_json::json!({
            "enabled": true,
            "display": { "width": 100, "height": 200 },
            "nodes": [node("android.widget.Button", "Go", [0, 0, 50, 50], true)],
        }));
        let reference: DeviceBridgeRef = std::sync::Arc::new(bridge);
        let snapshot = fetch(&reference, None).await.expect("snapshot");
        assert_eq!(snapshot.elements.len(), 1);
        assert!(snapshot.is_addressable());
    }

    #[tokio::test]
    async fn fetch_reports_an_unreachable_bridge() {
        struct Broken;
        #[async_trait::async_trait]
        impl crate::device::DeviceBridge for Broken {
            async fn call(
                &self,
                _command: &str,
                _payload: serde_json::Value,
            ) -> crate::Result<serde_json::Value> {
                Err(crate::error::SyscityError::Internal("no plugin".into()))
            }
        }
        let reference: DeviceBridgeRef = std::sync::Arc::new(Broken);
        let failed = fetch(&reference, None).await.expect_err("unreachable");
        assert_eq!(failed.code, A11Y_BRIDGE_ERROR);
    }

    // ── Node actions ──────────────────────────────────────────────────

    #[test]
    fn a_press_answer_says_whether_the_next_rung_should_try() {
        let done = ActionAnswer::parse(&serde_json::json!({ "ok": true, "accepted": 1 }));
        assert!(done.ok);
        assert!(!done.falls_through());
        assert!(!done.node_is_gone());

        // A control without the action: the control is still there, so a
        // gesture or a tap lands on the thing that was meant.
        let unknown = ActionAnswer::parse(
            &serde_json::json!({ "ok": false, "accepted": 0, "reason": "unknown_action" }),
        );
        assert!(unknown.falls_through());
        assert!(!unknown.node_is_gone());

        // A control that offered it and refused: the same answer, for the same
        // reason.
        let refused = ActionAnswer::parse(
            &serde_json::json!({ "ok": false, "accepted": 0, "reason": "action_refused" }),
        );
        assert!(refused.falls_through());
    }

    #[test]
    fn a_gone_node_ends_the_ladder_rather_than_falling_through() {
        // The one case that must NOT fall back to coordinates: a tap at the old
        // centre would hit whatever took the node's place.
        let gone = ActionAnswer::parse(
            &serde_json::json!({ "ok": false, "accepted": 0, "reason": "node_gone" }),
        );
        assert!(gone.node_is_gone());
        assert!(!gone.falls_through());
        assert!(gone.describe().contains("gone"), "{}", gone.describe());
    }

    #[test]
    fn an_answer_that_says_nothing_is_not_a_success() {
        // An older bridge, or a malformed reply: the only safe reading is that
        // the action did not happen.
        let silent = ActionAnswer::parse(&serde_json::json!({}));
        assert!(!silent.ok);
        assert!(silent.falls_through());
    }

    #[test]
    fn node_actions_carry_the_names_the_kotlin_side_switches_on() {
        let wire: Vec<&str> = [
            NodeAction::Click,
            NodeAction::LongClick,
            NodeAction::Focus,
            NodeAction::ScrollForward,
            NodeAction::ScrollBackward,
            NodeAction::SetText,
        ]
        .iter()
        .map(|action| action.wire())
        .collect();
        assert_eq!(
            wire,
            vec![
                "click",
                "longClick",
                "focus",
                "scrollForward",
                "scrollBackward",
                "setText"
            ]
        );
        assert_eq!(NodeAction::press(false), NodeAction::Click);
        assert_eq!(NodeAction::press(true), NodeAction::LongClick);
    }

    #[tokio::test]
    async fn an_action_names_the_node_by_ref_and_carries_the_text() {
        let bridge = std::sync::Arc::new(crate::device::tests::MockDeviceBridge::new(
            serde_json::json!({ "ok": true, "accepted": 1 }),
        ));
        let reference: DeviceBridgeRef = bridge.clone();
        let answer = act(&reference, 7, NodeAction::SetText, Some("09:30"), 1)
            .await
            .expect("answer");
        assert!(answer.ok);
        let calls = bridge.calls();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].0, crate::device::CMD_A11Y_ACTION);
        assert_eq!(calls[0].1["ref"], 7);
        assert_eq!(calls[0].1["action"], "setText");
        assert_eq!(calls[0].1["text"], "09:30");
    }

    #[tokio::test]
    async fn a_gesture_goes_out_as_json_paths() {
        let bridge = std::sync::Arc::new(crate::device::tests::MockDeviceBridge::new(
            serde_json::json!({ "ok": true }),
        ));
        let reference: DeviceBridgeRef = bridge.clone();
        let answer = gesture(&reference, &[vec![(10, 20), (10, 20)]], 80)
            .await
            .expect("answer");
        assert!(answer.ok);
        let calls = bridge.calls();
        assert_eq!(calls[0].0, crate::device::CMD_A11Y_GESTURE);
        assert_eq!(calls[0].1["paths"], serde_json::json!([[[10, 20], [10, 20]]]));
        assert_eq!(calls[0].1["duration_ms"], 80);
    }

    #[test]
    fn a_service_that_is_off_is_how_a_gesture_says_it_does_not_exist() {
        let absent =
            GestureAnswer::parse(&serde_json::json!({ "ok": false, "reason": "no_service" }));
        assert!(absent.no_service());
        let cancelled =
            GestureAnswer::parse(&serde_json::json!({ "ok": false, "reason": "cancelled" }));
        assert!(!cancelled.no_service());
    }
}
