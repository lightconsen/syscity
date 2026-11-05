//! The safety net in front of a tap: the element is still the one named.

// The set, the device targeting, and the siblings' items: this file used
// to be part of `android`, and names them exactly as it did there.
use super::*;

/// Loose label comparison for the safety net.
///
/// The model's description is free text, so accept a case-insensitive substring
/// match in either direction. An element with no label at all cannot be checked
/// and passes — refusing there would block icon buttons the model identified
/// from the screenshot.
pub(super) fn label_matches(label: &str, description: &str) -> bool {
    let label = label.trim();
    if label.is_empty() {
        return true;
    }
    let description = description.trim();
    if description.is_empty() {
        return false;
    }
    let (l, d) = (label.to_lowercase(), description.to_lowercase());
    l.contains(&d) || d.contains(&l)
}

/// Resolve an element index against a freshly-read tree, without any I/O.
///
/// This is the safety net: the index came from an earlier `android_ui_tree`
/// result, and the screen may have changed since. Tapping the *coordinates* of a
/// stale read silently hits whatever has since moved into place, so an indexed
/// tap re-reads and validates first, and refuses with a reason the model can act
/// on rather than clicking blind.
pub(super) fn validate_target<'a>(
    elements: &'a [UiElement],
    index: usize,
    description: Option<&str>,
) -> Result<&'a UiElement, String> {
    if index == 0 {
        return Err("Element indices are 1-based; 0 does not exist.".to_string());
    }
    let element = elements.get(index - 1).ok_or_else(|| {
        format!(
            "No element {index} on screen now — the UI has {} element(s). Re-read it with \
             android_ui_tree.",
            elements.len()
        )
    })?;

    if let Some(description) = description {
        let label = element
            .text
            .as_deref()
            .or(element.content_desc.as_deref())
            .unwrap_or("");
        if !label_matches(label, description) {
            return Err(format!(
                "Refused: element {index} is {label:?}, not {description:?} — the screen changed. \
                 Re-read it with android_ui_tree rather than tapping stale coordinates."
            ));
        }
    }
    check_tappable(element, index)
}

/// The part of a target check that does not depend on how it was named.
pub(super) fn check_tappable(element: &UiElement, index: usize) -> Result<&UiElement, String> {
    if !element.clickable {
        return Err(format!("Refused: element {index} ({}) is not clickable.", element.class));
    }
    if !element.enabled {
        return Err(format!("Refused: element {index} is disabled."));
    }
    Ok(element)
}
