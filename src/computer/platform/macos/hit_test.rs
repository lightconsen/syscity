//! macOS hit testing: which element would receive an event at a point.
//!
//! Everything else the macOS adapter does goes through System Events, which
//! is one AppleScript per query. The hit test is the one call worth making
//! natively: it runs before every pixel-rung click, and a `osascript`
//! round-trip there would cost more than the click itself.
//!
//! `AXUIElementCopyElementAtPosition` is the same query the system performs
//! when it routes a click, so it answers the question exactly — including the
//! case this exists for, where a dialog or a notification is drawn over the
//! element the caller named.
//!
//! # Verification status
//!
//! Compile-verified on every macOS build; the runtime path needs a real
//! Accessibility grant, which a test run on CI does not have. Every failure
//! path reports [`HitTestResult::Unknown`] rather than a guess, so a missing
//! grant weakens the check to "not performed" instead of making it wrong.
//!
//! FFI surface: hand-written, following the `#![allow(unsafe_code)]`
//! precedent set by `src/rag/sqlite_vec_store.rs` and
//! `src/tools/win_appcontainer.rs`. The types mirrored here are the
//! `AXUIElement` quartet documented in `AXUIElement.h`; they are
//! stable ABI, but the crate pulls no binding for them (the workspace has no
//! `accessibility-sys` dependency and adding one for four functions is not
//! worth the third-party surface).

#![allow(unsafe_code)]

use std::ffi::{c_char, c_void, CString};

use crate::computer::element_index::ActionTarget;
use crate::computer::types::{HitTestResult, Occluder, Point, Rect};

/// An `AXUIElementRef`. Opaque to everything outside Accessibility.
type AxElementRef = *const c_void;

/// The system-wide element: the entry point for a positional query.
type AxSystemWide = AxElementRef;

#[link(name = "ApplicationServices", kind = "framework")]
extern "C" {
    /// The system-wide accessibility element.
    fn AXUIElementCreateSystemWide() -> AxSystemWide;
    /// The element at a screen position, as the system's own hit test sees it.
    fn AXUIElementCopyElementAtPosition(
        application: AxSystemWide,
        x: f32,
        y: f32,
        element: *mut AxElementRef,
    ) -> i32;
    /// One attribute of an element, as a `CFTypeRef`.
    fn AXUIElementCopyAttributeValue(
        element: AxElementRef,
        attribute: *const c_char,
        value: *mut *const c_void,
    ) -> i32;
    /// `CFString` / `CFArray` release, from Core Foundation.
    fn CFRelease(value: *const c_void);
}

#[link(name = "CoreFoundation", kind = "framework")]
extern "C" {
    /// Length of a `CFString`, in UTF-16 units.
    fn CFStringGetLength(string: *const c_void) -> isize;
    /// Copy characters out of a `CFString`.
    fn CFStringGetCString(
        string: *const c_void,
        buffer: *mut c_char,
        buffer_size: isize,
        encoding: u32,
    ) -> u8;
}

/// `kCFStringEncodingUTF8`.
const CF_STRING_ENCODING_UTF8: u32 = 0x0800_0100;

/// `kAXErrorSuccess`.
const AX_ERROR_SUCCESS: i32 = 0;

/// One element as the hit test reported it.
#[derive(Debug, Clone)]
pub struct HitElement {
    /// `AXRole`, e.g. `"AXButton"`.
    pub role: String,
    /// `AXTitle`, then `AXDescription` — whichever is non-empty.
    pub label: Option<String>,
    /// `AXPosition` + `AXSize`, when both were readable.
    pub bounds: Option<Rect>,
}

impl HitElement {
    /// Whether the element the hit test reports is the one the caller named.
    ///
    /// The two sides come from different reads (a tree walk and a positional
    /// query), so identity is decided by the facts both reads publish. A
    /// disagreement about role or name is a different element, and the
    /// refusal that follows is the point of this check.
    ///
    /// The rectangle is used only as a tie-breaker when *neither* side has a
    /// name: that is the case where geometry is the only evidence there is
    /// (an unnamed image, an icon). When a name is available on either side,
    /// comparing rectangles as well would refuse the caller's own target over
    /// a rounding difference between two reads, which is worse than the
    /// occlusion it was meant to catch.
    pub fn is_the_same_element(
        &self,
        role: &str,
        label: Option<&str>,
        bounds: Option<Rect>,
    ) -> bool {
        if !self.role.eq_ignore_ascii_case(role) {
            return false;
        }
        match (self.label.as_deref(), label) {
            (Some(hit), Some(target)) => hit == target,
            (None, None) => match (self.bounds, bounds) {
                (Some(hit), Some(target)) => hit == target,
                // No geometry on one side: nothing says they differ.
                _ => true,
            },
            // A name only one side knows is silence, not disagreement.
            _ => true,
        }
    }

    /// The evidence form of this element.
    pub fn to_occluder(&self) -> Occluder {
        let mut occluder = Occluder::new(self.role.clone(), self.label.clone());
        if let Some(bounds) = self.bounds {
            occluder = occluder.with_bounds(bounds);
        }
        occluder
    }
}

/// The element at `point`, or `None` when the system could not say.
///
/// `None` covers every failure the same way on purpose: no Accessibility
/// permission (`kAXErrorAPIDisabled`), a point outside every window
/// (`kAXErrorInvalidUIElement`), and a missing attribute all mean "the check
/// did not happen", and the caller must treat that as unknown rather than as
/// clearance.
pub fn element_at(point: Point) -> Option<HitElement> {
    // SAFETY: the four calls below are the documented `AXUIElement` API with
    // out-parameters this function owns. Every non-success result leaves the
    // out-parameter untouched (`AXUIElementCopyElementAtPosition` and
    // `AXUIElementCopyAttributeValue` follow the CF "Copy" convention and
    // only write on success), so nothing is dereferenced uninitialised.
    unsafe {
        let system = AXUIElementCreateSystemWide();
        if system.is_null() {
            return None;
        }
        let mut element: AxElementRef = std::ptr::null();
        let status =
            AXUIElementCopyElementAtPosition(system, point.x as f32, point.y as f32, &mut element);
        CFRelease(system);
        if status != AX_ERROR_SUCCESS || element.is_null() {
            return None;
        }

        let role = attribute_string(element, "AXRole").unwrap_or_default();
        let label = attribute_string(element, "AXTitle")
            .filter(|s| !s.is_empty())
            .or_else(|| attribute_string(element, "AXDescription").filter(|s| !s.is_empty()));
        let bounds = attribute_rect(element);
        CFRelease(element);

        if role.is_empty() {
            // An element the system would not describe is one this check
            // cannot reason about.
            return None;
        }
        Some(HitElement { role, label, bounds })
    }
}

/// Ask the system what would receive an event at `point`, and turn that into
/// an answer the session can act on.
///
/// `target` is the element the caller named. When it is present, a hit on a
/// *different* element is a refusal; a hit on the target itself is clearance.
/// A hit on an element the caller did not name happens whenever a sheet, a
/// menu, or another window is on top, which is exactly what this must catch.
///
/// Without a target (a bare coordinate) there is nothing to disagree with, so
/// the point is reported as reaching what is there.
pub fn check(point: Point, target: Option<&ActionTarget>) -> HitTestResult {
    let Some(hit) = element_at(point) else {
        return HitTestResult::Unknown;
    };
    let Some(element) = target.and_then(|t| t.element.as_ref()) else {
        return HitTestResult::ReachesTarget;
    };
    if hit.is_the_same_element(&element.role, element.label.as_deref(), element.bounds) {
        HitTestResult::ReachesTarget
    } else {
        HitTestResult::InterceptedBy(hit.to_occluder())
    }
}

/// Read one string attribute.
///
/// # Safety
///
/// `element` must be a live `AXUIElementRef` that the caller owns; the
/// returned string is copied out before the value is released.
unsafe fn attribute_string(element: AxElementRef, name: &str) -> Option<String> {
    let value = copy_attribute(element, name)?;
    let text = cf_string_to_utf8(value);
    CFRelease(value);
    text
}

/// Read `AXPosition` and `AXSize` together; both are needed for a rectangle.
///
/// # Safety
///
/// As [`attribute_string`].
unsafe fn attribute_rect(element: AxElementRef) -> Option<Rect> {
    let position = copy_attribute(element, "AXPosition")?;
    let size = copy_attribute(element, "AXSize")?;
    let origin = point_from_ax_value(position);
    let extent = size_from_ax_value(size);
    CFRelease(position);
    CFRelease(size);
    match (origin, extent) {
        (Some((x, y)), Some((w, h))) => Some(Rect::new(
            x as i32,
            y as i32,
            w.max(0.0).round() as u32,
            h.max(0.0).round() as u32,
        )),
        _ => None,
    }
}

/// Copy one attribute, or `None` when it is missing or not a value.
///
/// # Safety
///
/// As [`attribute_string`].
unsafe fn copy_attribute(element: AxElementRef, name: &str) -> Option<*const c_void> {
    let name = CString::new(name).ok()?;
    let mut value: *const c_void = std::ptr::null();
    let status = AXUIElementCopyAttributeValue(element, name.as_ptr(), &mut value);
    if status != AX_ERROR_SUCCESS || value.is_null() {
        return None;
    }
    Some(value)
}

/// A `CFString` as Rust text, or `None` when it is not one.
///
/// # Safety
///
/// `value` must be a live Core Foundation object.
unsafe fn cf_string_to_utf8(value: *const c_void) -> Option<String> {
    // A bundle-like object has no character content; asking one for its
    // length is not meaningful, and a zero length is the cheap guard.
    let length = CFStringGetLength(value);
    if length <= 0 {
        return None;
    }
    // The encoding is UTF-8, so the byte buffer is bounded by the UTF-16
    // length × 4 plus the terminator; `CFStringGetCString` refuses to
    // overflow it and reports failure instead.
    let capacity = (length as usize).saturating_mul(4).saturating_add(1);
    let mut buffer = vec![0u8; capacity];
    let wrote = CFStringGetCString(
        value,
        buffer.as_mut_ptr() as *mut c_char,
        buffer.len() as isize,
        CF_STRING_ENCODING_UTF8,
    );
    if wrote == 0 {
        return None;
    }
    let end = buffer.iter().position(|b| *b == 0).unwrap_or(buffer.len());
    String::from_utf8(buffer[..end].to_vec()).ok()
}

/// `AXValue` holding a `CGPoint`: type 1.
const AX_VALUE_CG_POINT: u32 = 1;
/// `AXValue` holding a `CGSize`: type 2.
const AX_VALUE_CG_SIZE: u32 = 2;

/// Read the numeric payload of an `AXValue`.
///
/// # Safety
///
/// `value` must be a live `AXValue` object of the requested type.
unsafe fn ax_value_payload(value: *const c_void, kind: u32) -> Option<(f64, f64)> {
    // `AXValueGetValue` is not exported by Core Foundation alone; it lives in
    // the same ApplicationServices framework as the AXUIElement calls.
    #[link(name = "ApplicationServices", kind = "framework")]
    extern "C" {
        fn AXValueGetValue(value: *const c_void, kind: u32, out: *mut c_void) -> u8;
    }
    #[repr(C)]
    struct Pair {
        a: f64,
        b: f64,
    }
    let mut out = Pair { a: 0.0, b: 0.0 };
    let ok = AXValueGetValue(value, kind, &mut out as *mut Pair as *mut c_void);
    (ok != 0).then_some((out.a, out.b))
}

/// # Safety
///
/// `value` must be a live `AXValue` holding a `CGPoint`.
unsafe fn point_from_ax_value(value: *const c_void) -> Option<(f64, f64)> {
    ax_value_payload(value, AX_VALUE_CG_POINT)
}

/// # Safety
///
/// `value` must be a live `AXValue` holding a `CGSize`.
unsafe fn size_from_ax_value(value: *const c_void) -> Option<(f64, f64)> {
    ax_value_payload(value, AX_VALUE_CG_SIZE)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The occluder this reports is what a refusal shows, so the mapping is
    /// worth pinning without a live accessibility grant.
    #[test]
    fn a_hit_element_becomes_evidence() {
        let element = HitElement {
            role: "AXSheet".to_string(),
            label: Some("Save changes?".to_string()),
            bounds: Some(Rect::new(100, 80, 400, 200)),
        };
        let occluder = element.to_occluder();
        assert_eq!(occluder.role, "AXSheet");
        assert_eq!(occluder.label.as_deref(), Some("Save changes?"));
        assert_eq!(occluder.bounds, Some(Rect::new(100, 80, 400, 200)));
        assert!(occluder.summary().contains("Save changes?"));
    }

    fn hit(role: &str, label: Option<&str>, bounds: Option<Rect>) -> HitElement {
        HitElement {
            role: role.to_string(),
            label: label.map(str::to_string),
            bounds,
        }
    }

    /// A hit on a point no window covers leaves the session with `Unknown`,
    /// which is "the check did not happen", not clearance.
    #[test]
    fn a_point_outside_every_window_is_unknown() {
        let target = ActionTarget::point(-100_000, -100_000);
        assert_eq!(check(Point::new(-100_000, -100_000), Some(&target)), HitTestResult::Unknown);
    }

    /// Same role and same name is the element the caller named.
    #[test]
    fn a_matching_role_and_label_is_the_target() {
        let element = hit("AXButton", Some("Save"), None);
        assert!(element.is_the_same_element("AXButton", Some("Save"), None));
        // Roles come from different spellings on different platforms.
        assert!(element.is_the_same_element("axbutton", Some("Save"), None));
    }

    /// A dialog drawn over the element is a different element, whatever the
    /// geometry says.
    #[test]
    fn a_different_label_is_somewhere_else() {
        let element = hit("AXButton", Some("Cancel"), None);
        assert!(!element.is_the_same_element("AXButton", Some("Save"), None));
    }

    /// A menu covering a button is a different role, which is the clearest
    /// answer the hit test gives.
    #[test]
    fn a_different_role_is_somewhere_else() {
        let element = hit("AXMenu", Some("Save"), None);
        assert!(!element.is_the_same_element("AXButton", Some("Save"), None));
    }

    /// A name on one side and none on the other is silence, not disagreement:
    /// the tree read and the hit test use different attributes (`AXTitle` vs
    /// `AXDescription`), so either side can come back unnamed for the element
    /// itself. Reporting that as an occluder would refuse the caller's own
    /// target.
    #[test]
    fn a_missing_name_on_one_side_is_not_a_contradiction() {
        assert!(hit("AXButton", None, None).is_the_same_element("AXButton", Some("Save"), None));
        assert!(hit("AXButton", Some("Save"), None).is_the_same_element("AXButton", None, None));
    }

    /// A name only the tree knows about, on both sides matching, never turns
    /// into a refusal — pinned because the check runs before every pixel
    /// click.
    #[test]
    fn both_sides_unnamed_with_the_same_role_is_the_target() {
        assert!(hit("AXImage", None, None).is_the_same_element("AXImage", None, None));
    }

    /// Two unnamed elements are compared by rectangle, because that is the
    /// only evidence either read has. An icon the tree and the hit test agree
    /// on is the caller's target.
    #[test]
    fn unnamed_elements_are_compared_by_geometry() {
        let rect = Rect::new(10, 20, 30, 40);
        assert!(hit("AXImage", None, Some(rect)).is_the_same_element("AXImage", None, Some(rect)));
        assert!(!hit("AXImage", None, Some(rect)).is_the_same_element(
            "AXImage",
            None,
            Some(Rect::new(0, 0, 30, 40))
        ));
        // No geometry on one side says nothing about a difference.
        assert!(hit("AXImage", None, Some(rect)).is_the_same_element("AXImage", None, None));
    }

    /// Without a live grant the query reports nothing, and nothing is not a
    /// clearance. This is the property the session relies on: an unanswered
    /// hit test must not reject the action.
    #[test]
    fn an_unanswerable_point_is_not_reported_as_a_target() {
        // A point no window can occupy. Whether the grant exists or not, the
        // answer is `None` — never a fabricated element.
        let far = Point::new(-100_000, -100_000);
        assert!(element_at(far).is_none());
    }
}
