//! The accessibility tree: the element model and the `uiautomator` parse.

// The set, the device targeting, and the siblings' items: this file used
// to be part of `android`, and names them exactly as it did there.
use super::*;

/// Dump the Android accessibility UI tree via `uiautomator`.
#[derive(Debug)]
pub struct AdbUiTreeTool {
    // Read by `dump`, which runs the tree tool by hand for its own reads.
    pub(super) device: Option<String>,
}

impl Default for AdbUiTreeTool {
    fn default() -> Self {
        Self::new()
    }
}

impl AdbUiTreeTool {
    pub fn new() -> Self {
        Self { device: None }
    }

    pub fn with_device(mut self, device: String) -> Self {
        self.device = Some(device);
        self
    }
}

/// What a node *is*, as opposed to what it says.
///
/// Separate from [`UiElement`] and deliberately outside [`element_key`]: these
/// are states that change while the element stays the same element (a checkbox
/// gets ticked, a field takes focus). Identity must not move when they do.
///
/// Two sources fill this in: the accessibility service hands most of it over
/// directly (`A11yBridge.kt`), and `uiautomator dump` carries the same
/// attributes for everything except `editable`, which is inferred from the
/// class because the XML has no such attribute.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize)]
pub struct NodeState {
    /// The node itself scrolls — a list, a pager, a wheel picker.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub scrollable: bool,
    /// Text can be typed into it.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub editable: bool,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub checkable: bool,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub checked: bool,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub selected: bool,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub focused: bool,
    /// A password field: its contents must never be summarised to the model.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub password: bool,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub long_clickable: bool,
    /// `ACTION_SCROLL_FORWARD` / `ACTION_SCROLL_BACKWARD` are available. A
    /// scrollable node without them can only be moved by a gesture.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub scroll_forward: bool,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub scroll_backward: bool,
    /// This node reads as a wheel picker: a short, centred, evenly spaced run
    /// of values (a time, a date, a region) rather than a list.
    ///
    /// Decided by the source that has the children, not by this struct — see
    /// `wheel::looks_like_wheel`. The flag is what lets
    /// `android_set_wheel` find a picker without the model having to describe
    /// its geometry, and its absence is why a wheel used to be driven by
    /// guessing which way a swipe turns it.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub wheel: bool,
    /// Distance from the root of the read it came from.
    ///
    /// The element list is flat, so depth is the only surviving trace of the
    /// hierarchy: it is what lets a scroll aim at the innermost list containing
    /// the point rather than at any scrollable container on the screen.
    #[serde(skip_serializing_if = "is_zero")]
    pub depth: u32,
}

pub(super) fn is_zero(value: &u32) -> bool {
    *value == 0
}

impl NodeState {
    /// The node is structurally meaningful without carrying a name.
    ///
    /// An unnamed clickable `View` is a tap target nobody described; an unnamed
    /// list, field or toggle still tells the reader where it is and what can be
    /// done there. [`assess_tree`] uses this so a blank-looking list screen is
    /// not written off as undescribable.
    pub fn is_structural(&self) -> bool {
        self.scrollable || self.editable || self.checkable || self.wheel
    }
}

/// One node of the Android accessibility tree, flattened for the model.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct UiElement {
    /// 1-based index the model refers to this element by.
    pub index: usize,
    /// What this element *is*, independent of where it sits.
    ///
    /// An index is only meaningful against the read it came from: the screen
    /// scrolls, a row appears, and "element 7" is now something else. The key
    /// is built from what identifies the element (its resource id, label and
    /// class) and not from its position, so a later call can find the same
    /// element again in a tree it has never seen. Duplicates are possible and
    /// are reported rather than guessed at — see [`relocate`].
    #[serde(skip_serializing_if = "String::is_empty")]
    pub key: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content_desc: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resource_id: Option<String>,
    /// Fully-qualified class, shortened to the last segment.
    pub class: String,
    pub clickable: bool,
    pub enabled: bool,
    /// What the node is beyond its label — never part of its identity.
    #[serde(flatten)]
    pub state: NodeState,
    /// `(x1, y1, x2, y2)` in device pixels.
    pub bounds: (i32, i32, i32, i32),
    /// Centre of `bounds` — what a tap should target.
    pub center: (i32, i32),
    /// The accessibility node this element came from, when the source keeps
    /// one (`a11y::NodeRef`).
    ///
    /// Not part of the element's identity and never sent to the model: it is a
    /// handle into the phone's own process that expires with the node, whereas
    /// `key` is what survives a read. It exists so an action can be addressed to
    /// the control itself instead of to a point on the screen.
    #[serde(skip)]
    pub node_ref: Option<super::a11y::NodeRef>,
}

impl UiElement {
    /// An element with its key computed from the fields that identify it.
    ///
    /// Construction goes through here so a key can never be a stale copy of
    /// fields that were changed after it was computed.
    ///
    /// Test-only: both real sources know something about what the node is, so
    /// they go through [`with_state`](Self::with_state).
    #[cfg(test)]
    #[allow(clippy::too_many_arguments)]
    pub(super) fn new(
        index: usize,
        text: Option<String>,
        content_desc: Option<String>,
        resource_id: Option<String>,
        class: String,
        clickable: bool,
        enabled: bool,
        bounds: (i32, i32, i32, i32),
    ) -> Self {
        Self::with_state(
            index,
            text,
            content_desc,
            resource_id,
            class,
            clickable,
            enabled,
            NodeState::default(),
            bounds,
        )
    }

    /// [`new`](Self::new) for a source that also reports what the node is.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn with_state(
        index: usize,
        text: Option<String>,
        content_desc: Option<String>,
        resource_id: Option<String>,
        class: String,
        clickable: bool,
        enabled: bool,
        state: NodeState,
        bounds: (i32, i32, i32, i32),
    ) -> Self {
        let (x1, y1, x2, y2) = bounds;
        let mut element = Self {
            index,
            key: String::new(),
            text,
            content_desc,
            resource_id,
            class,
            clickable,
            enabled,
            state,
            bounds,
            center: ((x1 + x2) / 2, (y1 + y2) / 2),
            node_ref: None,
        };
        element.key = element_key(&element);
        element
    }
}

/// Which rung of the delivery ladder pressed something.
///
/// The order is fixed and it matters: a node action tells the *control* it was
/// pressed and reaches widgets that ignore injected events; a service gesture
/// is a real touch stream and reaches canvases and sliders; a shell injection
/// is the last resort and works everywhere `adb` does. The result reports
/// which one was used, because the three fail in different ways and a caller
/// reading "the screen did not change" needs to know what was tried.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Via {
    /// `AccessibilityNodeInfo.performAction` on the node the read named.
    Semantic,
    /// `dispatchGesture` through the accessibility service.
    Gesture,
    /// `input tap` / the display helper's `MotionEvent`.
    Shell,
    /// Nothing was dispatched: the caller was refused.
    None,
}

impl Via {
    /// The word the result carries.
    pub(super) fn as_str(self) -> &'static str {
        match self {
            Self::Semantic => "semantic",
            Self::Gesture => "gesture",
            Self::Shell => "shell",
            Self::None => "none",
        }
    }

    /// The clause the result sentence carries.
    pub(super) fn explain(self) -> &'static str {
        match self {
            Self::Semantic => {
                "It was pressed as a control (an accessibility action on the node), \
                              not as a point on the screen."
            }
            Self::Gesture => {
                "It was pressed with a touch gesture sent by the accessibility \
                              service, not by shell injection."
            }
            Self::Shell => "It was pressed by shell injection at its coordinates.",
            Self::None => "",
        }
    }
    /// The clause the sentence carries, when the rung is news.
    ///
    /// A shell press is what every result was before this ladder existed and
    /// says nothing new; a control action or a service gesture is the case a
    /// caller reading "the screen did not change" needs to see.
    pub(super) fn note(self) -> Option<&'static str> {
        match self {
            Self::Semantic | Self::Gesture => Some(self.explain()),
            Self::Shell | Self::None => None,
        }
    }
}

/// Press the element the read named, the best way this phone can.
///
/// The ladder is tried in order and each rung says whether the next one should
/// be tried. A node that is *gone* ends the ladder: pressing its old
/// coordinates would hit whatever has since moved into place, which is the
/// mistake the reference exists to prevent — the caller re-reads instead.
/// A node that merely refuses (no such action, the action did not take) falls
/// through, because the control is still there and a gesture or a tap lands on
/// the thing that was meant.
pub(super) async fn press_element(
    element: &UiElement,
    long: bool,
    allow_gesture: bool,
) -> (Via, Option<String>) {
    // Rung one: the control itself. Only possible when the read came from the
    // accessibility service — `uiautomator` has no way to address a node.
    if let Some(node) = element.node_ref {
        if let Some(bridge) = crate::device::bridge() {
            let action = super::a11y::NodeAction::press(long);
            if let Some(answer) = super::a11y::act(bridge, node, action, None, 1).await {
                if answer.ok {
                    return (Via::Semantic, None);
                }
                if answer.node_is_gone() {
                    return (
                        Via::None,
                        Some(format!(
                            "Refused: {} was found in the live screen, but the control it named \
                             is gone — the screen changed between the read and the press. Nothing \
                             was pressed: a coordinate tap would have hit whatever took its \
                             place. Re-read the screen with android_ui_tree and retry.",
                            element_label(element).unwrap_or("that element")
                        )),
                    );
                }
                // Rung two: a real touch stream at the element, which reaches a
                // view that ignores `input tap` (a canvas, a game surface).
                // Only the *user's* screen: a virtual display's gestures are
                // the display helper's job, and the service's gesture injection
                // is not tied to a display.
                if allow_gesture {
                    if let Some(gesture) =
                        super::a11y::gesture(bridge, &press_paths(element), PRESS_GESTURE_MS).await
                    {
                        if gesture.ok {
                            return (Via::Gesture, None);
                        }
                        if gesture.no_service() {
                            // The service stopped between the read and now; the
                            // shell route below still works.
                            tracing::debug!(
                                "accessibility gesture unavailable: the service is not running"
                            );
                        }
                    }
                }
            }
        }
    }
    (Via::Shell, None)
}

/// How long a press sent as a gesture takes.
///
/// Short enough to read as a tap rather than a hold (`ViewConfiguration`'s own
/// long-press threshold is 500 ms), long enough that the platform sees more
/// than one frame of contact.
pub(super) const PRESS_GESTURE_MS: u32 = 80;

/// The finger path for a press on `element`: down and up on the same point.
///
/// A gesture of one point would be a contact the platform sees once; three
/// points over the same coordinate read as a press the way a finger does.
pub(super) fn press_paths(element: &UiElement) -> Vec<Vec<(i32, i32)>> {
    let point = element.center;
    vec![vec![point, point, point]]
}

/// The identity of an element, for finding it again on a later read.
///
/// Deliberately not a hash of the whole element: bounds and index are position,
/// and an element that scrolled is the same element. Two elements that agree on
/// everything here are interchangeable to a tap, which is why [`relocate`]
/// refuses to choose between them.
pub fn element_key(element: &UiElement) -> String {
    use std::hash::{Hash, Hasher};

    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    element.resource_id.hash(&mut hasher);
    element.text.hash(&mut hasher);
    element.content_desc.hash(&mut hasher);
    element.class.hash(&mut hasher);
    format!("{:x}", hasher.finish())
}

/// Find the element a key names in the tree as it is now.
///
/// Exactly one match relocates. None, or several identical ones, does not: the
/// alternative is tapping the first of two rows that look the same, which is
/// precisely the silent wrong-target failure the key exists to prevent. The
/// refusal names the candidates so the caller can pick by index from a fresh
/// read instead of guessing.
pub fn relocate<'a>(elements: &'a [UiElement], key: &str) -> Result<&'a UiElement, String> {
    let matches: Vec<&UiElement> = elements
        .iter()
        .filter(|element| !element.key.is_empty() && element.key == key)
        .collect();
    match matches.as_slice() {
        [one] => Ok(one),
        [] => Err(
            "STALE_REF: nothing with that key is on the screen now. The element was replaced, \
             scrolled away, or the screen changed — re-read it with android_ui_tree and use the \
             new index or key."
                .to_string(),
        ),
        many => {
            let choices: Vec<String> = many
                .iter()
                .map(|element| {
                    format!(
                        "[{}] {} {:?} center=({},{})",
                        element.index,
                        element.class,
                        element_label(element).unwrap_or("-"),
                        element.center.0,
                        element.center.1
                    )
                })
                .collect();
            Err(format!(
                "STALE_REF: {} elements match that key and none of them is distinguishably the \
                 one you meant:\n{}\nUse the index of the right one from android_ui_tree.",
                many.len(),
                choices.join("\n")
            ))
        }
    }
}

/// Parse a `bounds="[x1,y1][x2,y2]"` attribute.
pub(super) fn parse_bounds(raw: &str) -> Option<(i32, i32, i32, i32)> {
    let rest = raw.strip_prefix('[')?;
    let (first, rest) = rest.split_once("][")?;
    let second = rest.strip_suffix(']')?;
    let mut a = first.split(',');
    let (x1, y1) = (a.next()?.trim().parse().ok()?, a.next()?.trim().parse().ok()?);
    let mut b = second.split(',');
    let (x2, y2) = (b.next()?.trim().parse().ok()?, b.next()?.trim().parse().ok()?);
    Some((x1, y1, x2, y2))
}

/// Flatten a `uiautomator dump` document into an indexed element list.
///
/// Only nodes the model can act on or read are kept — those with text, a
/// content description or a resource id, plus the ones that are actionable or
/// structural (clickable, scrollable, editable, checkable). Bare layout
/// containers carry nothing actionable and cost context, and the raw XML is
/// ~10x the size of this list.
///
/// `index` is assigned here, and is what `android_input` should be pointed at
/// via `center` rather than making the model read `bounds` itself.
pub fn parse_uiautomator_xml(xml: &str) -> Result<Vec<UiElement>, String> {
    let doc = roxmltree::Document::parse(xml).map_err(|e| format!("invalid UI tree XML: {e}"))?;

    let mut out = Vec::new();
    for node in doc.descendants().filter(|n| n.has_tag_name("node")) {
        let attr = |key: &str| {
            node.attribute(key)
                .map(str::trim)
                .filter(|v| !v.is_empty())
                .map(str::to_string)
        };
        let flag = |key: &str| attr(key).as_deref() == Some("true");
        let text = attr("text");
        let content_desc = attr("content-desc");
        let resource_id = attr("resource-id");
        let clickable = flag("clickable");
        let long_clickable = flag("long-clickable");
        let scrollable = flag("scrollable");
        let checkable = flag("checkable");
        let password = flag("password");
        let class = attr("class")
            .map(|c| c.rsplit('.').next().unwrap_or(&c).to_string())
            .unwrap_or_default();
        // The dump has no `editable` attribute; the class is the honest signal,
        // and the same list `A11yBridge` reports through `isEditable` in
        // practice. Text *content* is not evidence — a label is not a field.
        let editable = matches!(
            class.as_str(),
            "EditText" | "AutoCompleteTextView" | "TextInputEditText" | "SearchView"
        );

        let actionable = clickable || long_clickable || scrollable || checkable || editable;
        if text.is_none() && content_desc.is_none() && resource_id.is_none() && !actionable {
            continue;
        }
        let Some(bounds) = attr("bounds").as_deref().and_then(parse_bounds) else {
            continue;
        };

        let state = NodeState {
            scrollable,
            editable,
            checkable,
            checked: flag("checked"),
            selected: flag("selected"),
            focused: flag("focused"),
            password,
            long_clickable,
            // uiautomator reports `scrollable` but not which directions, so a
            // node it calls scrollable is assumed to accept both.
            scroll_forward: scrollable,
            scroll_backward: scrollable,
            // `uiautomator` says nothing about wheels, and the dump has no
            // children to judge a run of values by; the class is the one
            // honest signal, and it is the same one the bridge trusts first.
            wheel: class.ends_with("NumberPicker"),
            depth: node_depth(node),
        };

        out.push(UiElement::with_state(
            out.len() + 1,
            text,
            content_desc,
            resource_id,
            class,
            clickable,
            attr("enabled").as_deref() != Some("false"),
            state,
            bounds,
        ));
    }
    Ok(out)
}

/// How many `node` ancestors a node has.
///
/// `uiautomator` nests every element in `<node>` elements all the way up, so
/// counting ancestors is the depth. The bridge reports depth directly; both
/// have to mean the same thing or a scroll would pick a different container
/// depending on which source answered.
pub(super) fn node_depth(node: roxmltree::Node<'_, '_>) -> u32 {
    node.ancestors()
        // `ancestors()` starts at the node itself; the first real ancestor is
        // its parent, and the root `node` element is depth 1 — the same as the
        // bridge reports, where the root window is 0.
        .skip(1)
        .filter(|ancestor| ancestor.has_tag_name("node"))
        .count() as u32
}

/// Capture the device screen as raw PNG bytes.
///
/// Uses the byte-safe runner: the PNG has to reach the encoder exactly as the
/// device produced it, and the `String` variant of `run_cmd` would replace every
/// non-UTF-8 byte with U+FFFD and corrupt the image.
///
/// `pub(super)` because a tool that reasons about the picture itself rather
/// than only handing it to the model needs the same bytes: the visual search
/// takes one before and one after its match, and drawing a marker on a second
/// capture is what turns "here is roughly where" into "this is the control".
pub(super) async fn capture_png(device: &Option<String>, target: Target) -> crate::Result<Vec<u8>> {
    if let Target::Display(id) = target {
        // The helper owns the display's surface, so the frame comes from it
        // rather than from `screencap` (which would photograph the user's
        // screen). It is a PNG either way.
        let bridge = display_bridge().map_err(|message| {
            crate::error::SyscityError::ExternalService { source: message, cause: None }
        })?;
        let display = super::display::VirtualDisplay::attach(bridge, id);
        return display
            .screenshot()
            .await
            .map(|frame| frame.png)
            .map_err(|e| crate::error::SyscityError::ExternalService {
                source: e.to_string(),
                cause: None,
            });
    }
    let args = adb_args(device, &["exec-out", "screencap", "-p"]);

    let (status, stdout, stderr) =
        run_cmd_bytes("adb", &args.iter().map(|s| s.as_str()).collect::<Vec<_>>())
            .await
            .map_err(|e| crate::error::SyscityError::ExternalService {
                source: "adb screencap failed".to_string(),
                cause: Some(Box::new(e)),
            })?;
    if !status.success() {
        return Err(crate::error::SyscityError::ExternalService {
            source: format!("adb screencap failed: {stderr}"),
            cause: None,
        });
    }
    Ok(stdout)
}

/// The user's screen as a brightness grid, for screens the tree cannot see.
///
/// The helper's `frameSignature` only covers displays it created, so display 0
/// has to come from `screencap` — and deliberately **without** `-p`. Without
/// it `screencap` emits the raw framebuffer: some little-endian `u32` header
/// fields (width, height, format, and from API 28 a colorspace) followed by
/// `width * height * 4` bytes of RGBA. That is exactly what a 12×24 luma grid
/// needs, and it costs no PNG decoder — which matters because the phone build
/// has no `vision` feature and therefore no `image` crate.
///
/// The header length is derived rather than assumed (`12` on API 26/27, `16`
/// on 28+), and the `format` field is checked: a non-RGBA_8888 frame would have
/// the same length with a different byte layout, and reading it as RGBA would
/// produce a grid of noise that "differs" every time.
///
/// This is a second ADB round trip, not free: 1080×2400 is ~10 MB uncompressed.
/// Callers must treat it as the expensive path it is.
pub(super) async fn user_screen_signature(
    device: &Option<String>,
) -> Option<super::display::FrameSignature> {
    let args = adb_args(device, &["exec-out", "screencap"]);
    let (status, bytes, _stderr) =
        run_cmd_bytes("adb", &args.iter().map(|s| s.as_str()).collect::<Vec<_>>())
            .await
            .ok()?;
    if !status.success() {
        return None;
    }
    raw_screencap_signature(&bytes)
}

/// Turn a raw `screencap` frame into a brightness grid.
///
/// `None` when the frame is truncated, its format is not RGBA_8888, or its
/// dimensions are too small to divide into a grid — every one of those is a
/// reason not to guess about the picture rather than a reason to fail the
/// action.
pub(super) fn raw_screencap_signature(frame: &[u8]) -> Option<super::display::FrameSignature> {
    // API 26/27 write 12 bytes, API 28+ write 16. Both are ruled out by the
    // pixel count, so the header length is whatever is left over. Sixteen is
    // tried first: a 12-byte frame under a 16-byte reading is four bytes short
    // and is rejected, while a 16-byte frame under a 12-byte reading is not —
    // the leftover four bytes just shift the pixels, which is the one wrong
    // answer this check exists to avoid.
    const RGBA_8888: u32 = 1;
    let read_u32 = |offset: usize| -> Option<u32> {
        frame
            .get(offset..offset + 4)
            .map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
    };
    for header in [16usize, 12] {
        if frame.len() < header {
            continue;
        }
        let Some(width) = read_u32(0) else { continue };
        let Some(height) = read_u32(4) else { continue };
        let Some(format) = read_u32(8) else { continue };
        if width == 0 || height == 0 || format != RGBA_8888 {
            continue;
        }
        let pixels = (width as usize)
            .checked_mul(height as usize)?
            .checked_mul(4)?;
        if frame.len() != header + pixels {
            continue;
        }
        return luma_grid_from_rgba(&frame[header..], width, height);
    }
    None
}

/// Average luminance per cell, sampled the way the helper samples it.
///
/// Same grid, same 4×4 sample points inside each cell, same `(3r + 6g + b) / 10`
/// weights as `FrameSignature.lumaGrid` on the Kotlin side. The two producers
/// have to agree: a grid from `screencap` is compared against one the helper
/// produced for a virtual display whenever an action on a display is checked.
pub fn luma_grid_from_rgba(
    rgba: &[u8],
    width: u32,
    height: u32,
) -> Option<super::display::FrameSignature> {
    const GRID_W: u32 = 12;
    const GRID_H: u32 = 24;
    const SAMPLES: u32 = 4;
    let cell_width = width / GRID_W;
    let cell_height = height / GRID_H;
    if cell_width == 0 || cell_height == 0 {
        return None;
    }
    if rgba.len() < (width as usize) * (height as usize) * 4 {
        return None;
    }
    let luma_at = |x: u32, y: u32| -> u32 {
        let offset = ((y as usize) * (width as usize) + (x as usize)) * 4;
        let red = u32::from(rgba[offset]);
        let green = u32::from(rgba[offset + 1]);
        let blue = u32::from(rgba[offset + 2]);
        (red * 3 + green * 6 + blue) / 10
    };
    let mut cells = Vec::with_capacity((GRID_W * GRID_H) as usize);
    for gy in 0..GRID_H {
        for gx in 0..GRID_W {
            let mut sum = 0;
            for sy in 0..SAMPLES {
                for sx in 0..SAMPLES {
                    sum += luma_at(
                        gx * cell_width + sx * cell_width / SAMPLES,
                        gy * cell_height + sy * cell_height / SAMPLES,
                    );
                }
            }
            cells.push((sum / (SAMPLES * SAMPLES)).min(255) as u8);
        }
    }
    Some(super::display::FrameSignature {
        columns: GRID_W,
        rows: GRID_H,
        cells,
    })
}

/// The screen rectangle a dump describes: the root `<node>`'s bounds.///
/// `uiautomator dump` wraps everything in `<hierarchy>` with a single root node
/// whose bounds are the full display. Comparing that against the screenshot's
/// pixel size is how a coordinate-space mismatch is caught.
pub fn parse_screen_bounds(xml: &str) -> Option<(i32, i32)> {
    let doc = roxmltree::Document::parse(xml).ok()?;
    let root = doc.descendants().find(|n| {
        n.has_tag_name("node") && n.parent().is_some_and(|p| p.has_tag_name("hierarchy"))
    })?;
    parse_bounds(root.attribute("bounds")?).map(|(_, _, x2, y2)| (x2, y2))
}

/// The package the root node belongs to, from a `uiautomator` dump.
///
/// Read from the first node that names one: the hierarchy element sometimes
/// carries the package itself, and the root node always does.
pub(super) fn parse_package(xml: &str) -> Option<String> {
    let doc = roxmltree::Document::parse(xml).ok()?;
    doc.descendants()
        .filter(|n| n.has_tag_name("node"))
        .find_map(|n| n.attribute("package"))
        .map(str::to_string)
        .filter(|p| !p.is_empty())
}

/// Width and height from a PNG's IHDR chunk.
pub(super) fn png_dimensions(bytes: &[u8]) -> Option<(u32, u32)> {
    const SIGNATURE: [u8; 8] = [0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];
    if bytes.len() < 24 || bytes[..8] != SIGNATURE || &bytes[12..16] != b"IHDR" {
        return None;
    }
    let width = u32::from_be_bytes(bytes[16..20].try_into().ok()?);
    let height = u32::from_be_bytes(bytes[20..24].try_into().ok()?);
    Some((width, height))
}

/// Whether the element coordinates and the screenshot describe the same space.
///
/// They come from two separate reads, so a rotation — or a scaled capture —
/// between them makes every coordinate in the tree wrong for the image the model
/// is looking at, and taps land somewhere else. Comparing the sizes is cheap and
/// catches it; a rotation is expected to simply swap the axes.
///
/// When either side is unknown there is nothing to compare, and claiming a
/// mismatch would be worse than staying quiet.
pub(super) fn coordinate_space_consistent(
    screen: Option<(i32, i32)>,
    shot: Option<(u32, u32)>,
) -> bool {
    match (screen, shot) {
        (Some((sw, sh)), Some((iw, ih))) => {
            let (sw, sh) = (sw.max(0) as u32, sh.max(0) as u32);
            (sw, sh) == (iw, ih) || (sh, sw) == (iw, ih)
        }
        _ => true,
    }
}

/// A UI dump that could not be read, with a stable code for the cause.
///
/// The code is the part a caller branches on; the message is what the model
/// reads. `WebErrorCode` in `tools::web` is the same idea, and the wire values
/// belong to the tool contract in the same way: they ride on the result as
/// `data.code` so a caller does not have to parse the sentence.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DumpFailure {
    /// One of the constants below.
    pub code: &'static str,
    /// What happened, and what to do instead.
    pub message: String,
}

/// The device is gone: unplugged, offline, or not authorised.
pub const DEVICE_GONE: &str = "DEVICE_GONE";
/// The screen never stopped changing, so uiautomator could not settle it.
pub const SCREEN_NEVER_IDLE: &str = "SCREEN_NEVER_IDLE";
/// No accessibility root: a secure window, or another holder of UiAutomation.
pub const SECURE_WINDOW: &str = "SECURE_WINDOW";
/// The accessibility service is not enabled.
pub const ACCESSIBILITY_DISABLED: &str = "ACCESSIBILITY_DISABLED";
/// The dump failed for a reason this code does not name.
pub const DUMP_FAILED: &str = "DUMP_FAILED";
/// A virtual display's tree could not be read (no service, or API < 30).
pub const DISPLAY_TREE_UNAVAILABLE: &str = "DISPLAY_TREE_UNAVAILABLE";

impl DumpFailure {
    pub(super) fn new(code: &'static str, message: String) -> Self {
        Self { code, message }
    }

    /// Classify what `uiautomator` said.
    ///
    /// Its stderr is cryptic and looks the same across causes that need
    /// completely different responses — a device that is gone, a screen that
    /// never settles, and a secure window all just "fail". The fallback is
    /// always available though: the screenshot still works when the element
    /// tree does not.
    pub(super) fn classify(stderr: &str) -> Self {
        let lowered = stderr.to_lowercase();
        // adb puts the serial in the middle: `device 'emulator-5554' not found`.
        let device_gone = (lowered.contains("device") && lowered.contains("not found"))
            || lowered.contains("device offline")
            || lowered.contains("no devices/emulators found")
            || lowered.contains("unauthorized");
        let (code, symptom) = if device_gone {
            (
                DEVICE_GONE,
                "the device is not connected (or has gone offline) — reconnect it and try again",
            )
        } else if lowered.contains("could not get idle state") {
            (SCREEN_NEVER_IDLE, "the screen never settled (something on it keeps animating)")
        } else if lowered.contains("null root node") || lowered.contains("could not get root node")
        {
            (
                SECURE_WINDOW,
                "no accessibility root node was available — likely a secure window (a password or \
                 banking screen), or another app is holding the UiAutomation connection",
            )
        } else if lowered.contains("permission denial") {
            (ACCESSIBILITY_DISABLED, "the accessibility service is not enabled")
        } else {
            (DUMP_FAILED, "uiautomator reported an error")
        };

        let raw = stderr.trim();
        let message = if raw.is_empty() {
            format!(
                "Could not read the UI tree: {symptom}. Work from a screenshot instead — \
                 android_observe returns one alongside the tree."
            )
        } else {
            format!(
                "Could not read the UI tree: {symptom}. Raw output: {raw}. Work from a screenshot \
                 instead — android_observe returns one alongside the tree."
            )
        };
        Self::new(code, message)
    }
}

impl std::fmt::Display for DumpFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

/// One line per element, for the model to read directly.
pub(super) fn format_ui_summary(elements: &[UiElement]) -> String {
    if elements.is_empty() {
        // A dump that succeeds but yields nothing is not a failure to report —
        // it is a screen the accessibility tree cannot describe (a game, a
        // canvas, a secure surface). Saying so, and pointing at the screenshot,
        // is the difference between the model retrying the tree forever and it
        // switching approach.
        return "The UI tree has no actionable elements — this screen is probably a game, a \
                canvas, or a secure surface that exposes no accessibility nodes. Work from the \
                screenshot instead (android_observe returns one alongside this list)."
            .to_string();
    }
    let mut lines = vec![format!("{} actionable element(s):", elements.len())];
    lines.extend(elements.iter().map(format_ui_element));
    lines.join("\n")
}

/// One element as the model reads it: `[3] Button "Send" clickable center=(1,2)`.
///
/// The index is the element's own, so a line rendered on its own from a diff
/// is still something a later call can point at.
pub(super) fn format_ui_element(e: &UiElement) -> String {
    // A password field's contents must not reach the model — not as a label,
    // and not as a resource id it could be read back through.
    let label = if e.state.password {
        "[password]".to_string()
    } else {
        e.text
            .as_deref()
            .or(e.content_desc.as_deref())
            .map(|s| format!("{s:?}"))
            .or_else(|| e.resource_id.clone())
            .unwrap_or_else(|| "-".to_string())
    };
    let mut flags = String::new();
    if e.clickable {
        flags.push_str(" clickable");
    }
    if !e.enabled {
        flags.push_str(" disabled");
    }
    if e.state.scrollable {
        flags.push_str(" scrollable");
    }
    if e.state.editable {
        flags.push_str(" editable");
    }
    if e.state.checkable {
        flags.push_str(if e.state.checked {
            " checked"
        } else {
            " unchecked"
        });
    }
    if e.state.wheel {
        flags.push_str(" wheel");
    }
    if e.state.selected {
        flags.push_str(" selected");
    }
    if e.state.focused {
        flags.push_str(" focused");
    }
    format!(
        "[{}] {} {}{} center=({},{})",
        e.index, e.class, label, flags, e.center.0, e.center.1
    )
}
