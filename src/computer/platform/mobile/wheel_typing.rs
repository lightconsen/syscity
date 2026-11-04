//! Starting a wheel turn: the two steps that are not geometry.
//!
//! Most of turning a wheel is arithmetic on what the screen says — that is
//! [`super::wheel`]. What needs a phone is starting it: typing a value into a
//! picker's editable row, or telling the platform to step the wheel. Both are
//! *actions*, so they go through the device bridge, and both are best-effort:
//! a picker that cannot be typed into, or a wheel whose node has scrolled
//! away, falls through to the next method rather than failing the call.

use serde_json::Value;

use super::android::{adb_args, Target, UiElement};
use super::run_cmd;

/// The default time a typed value is given to settle.
const TYPE_SETTLE_MS: u64 = 250;

/// What happened when a value was typed at the wheel.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Typing {
    /// The value was written into the picker's editable row.
    Done,
    /// Nothing on this wheel accepts text, so the drag path is next.
    Unsupported,
}

/// Write `value` into an editable row inside the wheel.
///
/// Two routes, because the two pickers that accept text differ:
///
/// * the accessibility service, when it is on: `ACTION_SET_TEXT` is exact and
///   does not depend on an input method being active;
/// * otherwise, focus the field through the tree and type with the same
///   `input text` the rest of the tool set uses.
///
/// Neither route proves success — the caller re-reads and compares, which is
/// the only thing that can.
pub(super) async fn type_into(
    device: &Option<String>,
    target: Target,
    args: &Value,
    value: &str,
) -> Typing {
    // The field to write into: an editable node on the wheel's own line of the
    // screen. A picker that has one is a `NumberPicker`, whose middle row is
    // an `EditText`.
    let Some(field) = read_editable_row(device, target, args).await else {
        return Typing::Unsupported;
    };
    let Some(node_ref) = field.node_ref else {
        // A source without node handles cannot be addressed node-first; the
        // ADB route below types into the focused field instead.
        return type_over_adb(device, &field, value).await;
    };

    if let Some(bridge) = crate::device::bridge().cloned() {
        let payload = serde_json::json!({
            "ref": node_ref,
            "action": "setText",
            "text": value,
        });
        match bridge.call(crate::device::CMD_A11Y_ACTION, payload).await {
            Ok(answer) if answer.get("ok").and_then(Value::as_bool).unwrap_or(false) => {
                tokio::time::sleep(std::time::Duration::from_millis(TYPE_SETTLE_MS)).await;
                return Typing::Done;
            }
            Ok(_) | Err(_) => {
                // The bridge answered "no" — no service, no node, or a node
                // without the action. The ADB route below still works.
            }
        }
    }

    type_over_adb(device, &field, value).await
}

/// Type into a field by tapping it and sending the text to the device.
///
/// The tap is what focuses it; a tree read cannot focus a field on a device
/// reached only over ADB. The keyboard is then sent Enter and Back so a picker
/// commits its edit and the keyboard does not stay over the wheel.
async fn type_over_adb(device: &Option<String>, field: &UiElement, value: &str) -> Typing {
    let tap = format!("input tap {} {}", field.center.0, field.center.1);
    let argv = adb_args(device, &["shell", &tap]);
    if let Ok((status, _, _)) =
        run_cmd("adb", &argv.iter().map(|s| s.as_str()).collect::<Vec<_>>()).await
    {
        if !status.success() {
            return Typing::Unsupported;
        }
    } else {
        return Typing::Unsupported;
    }
    tokio::time::sleep(std::time::Duration::from_millis(TYPE_SETTLE_MS)).await;

    let text =
        format!("input text {}", super::android::sh_quote(&super::android::input_encode(value)));
    let argv = adb_args(device, &["shell", &text]);
    let typed = matches!(
        run_cmd("adb", &argv.iter().map(|s| s.as_str()).collect::<Vec<_>>()).await,
        Ok((status, _, _)) if status.success()
    );
    if !typed {
        return Typing::Unsupported;
    }
    // Enter is what commits a `NumberPicker`'s edit; the keyboard then goes
    // away on its own in almost every build, and the back key closes it where
    // it does not.
    let argv = adb_args(device, &["shell", "input keyevent 66"]);
    let _ = run_cmd("adb", &argv.iter().map(|s| s.as_str()).collect::<Vec<_>>()).await;
    tokio::time::sleep(std::time::Duration::from_millis(TYPE_SETTLE_MS)).await;
    Typing::Done
}

/// The editable row inside the wheel, read from the live screen.
///
/// "Inside" is the geometry again: the flat element list has no parentage, so
/// an editable node counts as part of the wheel when it sits on the wheel's
/// line of the screen — within the referenced element's horizontal span and
/// inside its vertical span.
async fn read_editable_row(
    device: &Option<String>,
    target: Target,
    args: &Value,
) -> Option<UiElement> {
    let dump = super::android::dump_ui(device, target).await.ok()?;
    let anchor = locate_anchor(&dump.elements, args)?;
    let (left, top, right, bottom) = anchor.bounds;
    dump.elements
        .iter()
        .filter(|element| element.state.editable && !element.state.password)
        .filter(|element| {
            let (x1, y1, x2, y2) = element.bounds;
            x2 > x1 && y2 > y1 && element.center.0 >= left && element.center.0 <= right
                && element.center.1 >= top && element.center.1 <= bottom
        })
        // The middle row is the one nearest the wheel's own centre.
        .min_by_key(|element| (element.center.1 - anchor.center.1).abs())
        .cloned()
}

/// The element the caller referenced, resolved against the live screen.
fn locate_anchor<'a>(elements: &'a [UiElement], args: &Value) -> Option<&'a UiElement> {
    if let Some(key) = args
        .get("key")
        .and_then(|v| v.as_str())
        .filter(|key| !key.trim().is_empty())
    {
        return super::android::relocate(elements, key.trim()).ok();
    }
    let index = args.get("target").and_then(|v| v.as_u64())?;
    elements.get(usize::try_from(index).ok()?.checked_sub(1)?)
}

/// Ask the platform to step the wheel `count` times, returning how many steps
/// it accepted (0 means it refused, which reads as "at the end").
///
/// The accessibility action is the exact way to move a picker one item: a
/// scroll sent this way does not depend on where a finger would land, and on a
/// `NumberPicker` each action is precisely one row. When the service is off
/// there is no way to address the node at all, and the caller falls back to a
/// drag.
pub(super) async fn perform_scroll(wheel_element: &UiElement, forward: bool, count: i32) -> i32 {
    let Some(node_ref) = wheel_element.node_ref else {
        // No node handle: this read came from `uiautomator`, which has no way
        // to address a node. A drag is the only remaining route.
        return 0;
    };
    let Some(bridge) = crate::device::bridge().cloned() else {
        return 0;
    };
    let action = if forward {
        "scrollForward"
    } else {
        "scrollBackward"
    };
    let payload = serde_json::json!({
        "ref": node_ref,
        "action": action,
        "count": count.max(1),
    });
    match bridge.call(crate::device::CMD_A11Y_ACTION, payload).await {
        Ok(answer) => answer
            .get("accepted")
            .and_then(Value::as_i64)
            .and_then(|count| i32::try_from(count).ok())
            .unwrap_or(0),
        Err(_) => 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::computer::platform::mobile::android::NodeState;

    fn element(index: usize, text: &str, bounds: (i32, i32, i32, i32)) -> UiElement {
        let mut element = UiElement::with_state(
            index,
            Some(text.to_string()),
            None,
            None,
            "TextView".to_string(),
            false,
            true,
            NodeState::default(),
            bounds,
        );
        element.key = format!("key-{index}");
        element
    }

    #[test]
    fn the_anchor_is_the_element_named_by_its_key() {
        let elements = vec![element(1, "09", (500, 1000, 580, 1080))];
        let args = serde_json::json!({ "key": "key-1" });
        let found = locate_anchor(&elements, &args).expect("the element");
        assert_eq!(found.index, 1);
    }

    #[test]
    fn the_anchor_falls_back_to_the_element_index() {
        let elements = vec![element(1, "09", (500, 1000, 580, 1080))];
        let args = serde_json::json!({ "target": 1 });
        assert_eq!(locate_anchor(&elements, &args).map(|e| e.index), Some(1));
        // Indices are 1-based: zero names nothing.
        let zero = serde_json::json!({ "target": 0 });
        assert!(locate_anchor(&elements, &zero).is_none());
    }
}
