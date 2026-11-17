//! Rendering an accessibility tree as a line-oriented outline.
//!
//! Extracted from the `screen_state` tool because the computer-use driver
//! needs exactly the same rendering: a model that decides by reading an
//! outline must see the same text whether it was asked through the tool or by
//! the loop. Two renderers would drift, and the drift would show up as a
//! decision that works in a chat turn and not in a run.
//!
//! The definition of "complete" lives with the data
//! ([`UiElement::tree_is_complete`]); this module only draws what is there and
//! says plainly which way the drawing is short.

use std::collections::HashMap;

use crate::computer::types::UiElement;

/// The rendered tree section of a `screen_state` result.
///
/// Kept apart from the adapter so the two ways a tree can be short — this
/// renderer's line cap and a read that stopped early — can be tested without
/// a live desktop. They mean different things to the model: the first is
/// fixed by asking for fewer elements, the second only by re-reading deeper.
#[derive(Debug, Default)]
pub struct TreeOutline {
    /// The rendered outline, header included.
    pub text: String,
    /// The line cap stopped the render.
    pub cut_by_cap: bool,
    /// The accessibility read itself stopped early.
    pub incomplete: bool,
    /// The read produced no roots at all.
    ///
    /// Kept apart from `incomplete`, which is also true here: "the read was
    /// cut at its depth or element limit" and "the read produced nothing" call
    /// for different advice, and telling the model to look deeper at a screen
    /// nobody could read is the misdirection this flag exists to prevent.
    pub no_roots: bool,
}

impl TreeOutline {
    pub fn push_tree(
        &mut self,
        roots: &[UiElement],
        ui_source: &str,
        max_lines: usize,
        want_fallback: bool,
        fallback_count: usize,
        sensitive: &HashMap<String, &'static str>,
    ) {
        self.text.push_str("UI tree:\n");
        let mut lines = 0usize;
        for root in roots {
            format_element(root, 0, &mut self.text, &mut lines, max_lines, sensitive);
        }
        if roots.is_empty() {
            // "The read found no elements", not "the screen is empty": the
            // header describes what the read produced, and the note below says
            // what that means.
            self.text
                .push_str("(empty — the accessibility read found no elements)\n");
            // Nothing was read, so nothing here claims the desktop is bare.
            // `tree_is_complete(&[])` used to be `true`, which made this the
            // one case where "the read produced nothing" was drawn as "the
            // tree is whole". A caller deciding whether to fall back to pixels
            // needs the honest answer, and it is the same one the renderer
            // already gives for a cut read.
            self.incomplete = !UiElement::tree_is_complete(roots);
            self.no_roots = roots.is_empty();
            if cfg!(not(feature = "vision")) && want_fallback {
                self.text
                    .push_str("(UI detection unavailable: built without 'vision' feature)\n");
            }
            return;
        }
        if ui_source == "omniparser" {
            self.text.push_str(&format!(
                "(accessibility tree empty — {fallback_count} UI elements detected via OmniParser)\n"
            ));
        }
        // The renderer's own cap. Saying so is what lets the model reason
        // about a list it asked to be short.
        self.cut_by_cap = lines >= max_lines;
        if self.cut_by_cap {
            self.text.push_str("… (tree truncated)\n");
        }
        self.incomplete = !UiElement::tree_is_complete(roots);
    }

    /// The sentence to append when the read — not the renderer — stopped
    /// short, and `None` when the tree is whole.
    pub fn incomplete_note(&self) -> Option<&'static str> {
        if self.no_roots {
            return Some(
                "(⚠ no accessibility tree was read — this desktop may publish none, or the \
                 accessibility bridge may be missing; an element you cannot see may exist, and \
                 neither a deeper read nor a retry will show it. Use the screenshot or screen \
                 coordinates.)\n",
            );
        }
        self.incomplete.then_some(
            "(⚠ tree incomplete — the accessibility read stopped at its depth or element \
             limit; an element you cannot see may still exist)\n",
        )
    }
}

/// How far the model can trust the tree it was shown.
pub fn trust_line(ui_source: &str) -> String {
    format!(
        "(tree source: {}; {})\n",
        ui_source,
        match ui_source {
            "omniparser" => "inferred from pixels — treat roles/names as guesses, verify visually",
            // OCR read these characters; they are on the screen, but what they
            // *are* (a button, a label, a sentence) is still a guess.
            "ocr" =>
                "transcribed from pixels — the words are on screen, but what you can \
                      click is a guess",
            // A vision model answering the narrow "list the controls" question:
            // same standing as a local detector, and it is the one source where
            // saying "authoritative" would be the opposite of the truth.
            "api-vision" => "proposed from pixels — treat roles/names as guesses, verify visually",
            _ => "authoritative — element ids address real accessibility elements",
        }
    )
}

/// Longest element value rendered inline before it is shortened.
pub const MAX_INLINE_VALUE_CHARS: usize = 120;

/// Shorten a value to a single readable line.
///
/// A value is one line of an outline, not a document: a newline in a text
/// area's contents would break the indentation the tree depends on, and a
/// 4 KB paste would spend the outline's line budget on itself.
pub fn clamp_inline(value: &str, max: usize) -> String {
    let flattened = value.replace(['\n', '\r'], " ");
    let mut chars = flattened.chars();
    let head: String = chars.by_ref().take(max).collect();
    if chars.next().is_some() {
        format!("{head}…")
    } else {
        head
    }
}

/// Render a UI element and its children as an indented outline.
/// Returns `false` when the line cap was hit (tree truncated).
pub fn format_element(
    el: &UiElement,
    depth: usize,
    out: &mut String,
    lines: &mut usize,
    max_lines: usize,
    sensitive: &HashMap<String, &'static str>,
) {
    if *lines >= max_lines {
        return;
    }
    *lines += 1;
    let indent = "  ".repeat(depth);
    // Omit the quoted label entirely when it is empty (e.g. OmniParser-detected
    // elements carry no accessibility label), instead of rendering `""`.
    let label = el.label.as_deref().filter(|l| !l.is_empty());
    let label_part = label.map(|l| format!(" {:?}", l)).unwrap_or_default();
    // Only a positive "disabled" is worth a mark. An unread state stays
    // silent rather than being drawn as enabled — the model should not build
    // confidence on an answer nobody gave.
    let state = if el.enabled.is_false() {
        " [disabled]"
    } else {
        ""
    };
    // Focus is the same three-state fact as `enabled`, and it is the one that
    // decides where a bare `type` or `key_press` lands. An unread focus stays
    // unmarked, like an unread enabled state.
    let focus_part = if el.focused.known() == Some(true) {
        " [focused]"
    } else {
        ""
    };
    // The id is what lets the model act on this exact element later: it is
    // snapshot-bound, and `computer` resolves it or reports it as stale.
    let id_part = if el.id.is_empty() {
        String::new()
    } else {
        format!(" [id={}]", el.id)
    };
    // The current value is what makes a segmented control readable at all. A
    // date input is three anonymous spin buttons to AT-SPI — `month`,
    // `day`, `year` — and the only thing that says whether the typing landed
    // is their values. Leaving them out of the outline meant the model could
    // not confirm a date it had just entered, and the full JSON that does
    // carry them was being cut from the `[data]` fold by its byte budget.
    //
    // A sensitive field is the one exception: its value is the secret the
    // scan just warned about, so it stays off the line. Long values are
    // shortened rather than printing a paragraph.
    let value_part = match (sensitive.get(&el.id), el.value.as_deref()) {
        (Some(_), _) => String::new(),
        (None, Some(value)) if !value.is_empty() => {
            format!(" = {:?}", clamp_inline(value, MAX_INLINE_VALUE_CHARS))
        }
        _ => String::new(),
    };
    // Inline the reason the model must slow down on *this* line, rather than
    // leaving it to a header it may skim past.
    let sensitive_part = sensitive
        .get(&el.id)
        .map(|kind| format!(" [sensitive: {kind}]"))
        .unwrap_or_default();
    // An element whose geometry the read did not publish has no point to
    // click. Saying nothing is honest; printing `(0,0)` would invite a click
    // at the top-left corner of the screen.
    let geometry_part = el
        .bounds
        .map(|b| format!(" at ({},{}) {}x{}", b.x, b.y, b.width, b.height))
        .unwrap_or_default();
    out.push_str(&format!(
        "{}{}{}{}{}{}{}{}{}\n",
        indent,
        el.role,
        label_part,
        state,
        focus_part,
        geometry_part,
        id_part,
        value_part,
        sensitive_part
    ));
    for child in &el.children {
        format_element(child, depth + 1, out, lines, max_lines, sensitive);
    }
}
