//! The 0–999 coordinate space, alongside the pixels everything already uses.
//!
//! A phone's screen is not one size. The same tap written as `540, 1200` is
//! correct on the device it was measured on and lands somewhere else entirely
//! on one with a different resolution — which matters here because a caller may
//! be driving a second display, a tablet, or a phone whose frame it has seen but
//! whose tree it has not. The 0–999 space fixes that: `500, 500` is the middle
//! of whatever screen is there.
//!
//! It is an addition, not a replacement. Every tool still takes pixels, and a
//! pixel argument wins when both are given: coordinates read off a tree are
//! exact, and rounding them onto a thousand-step grid would move the finger.
//! The percent form is for callers that know the *shape* of the screen — "the
//! bottom right" — and not its size.
//!
//! Results carry both: the pixel point that was dispatched and the percent that
//! names it, so a caller can go on using either and the two cannot silently
//! disagree about where the finger went.
//!
//! Which arguments take a percent (the `x`-family scalars, and `steps` items):
//! `android_input` taps, swipes and sequences, `android_display` taps and
//! swipes, and `android_run` swipe steps. `gesture` paths stay in pixels: a
//! path is a list of positions measured off a frame, and the frame carries its
//! own size.

use serde_json::{json, Value};

/// The last step of an axis, inclusive: `0` is the near edge, `999` the far
/// one. A thousand steps is what the screenshot-based tools already reason in
/// (a 12×24 brightness grid is coarser still), so a percent the model reads off
/// a picture is a percent it can send back.
pub const PCT_MAX: i64 = 999;

/// Which arguments name one point.
#[derive(Debug, Clone, Copy)]
pub struct Keys {
    /// The pixel argument for the horizontal axis.
    pub x: &'static str,
    /// The pixel argument for the vertical axis.
    pub y: &'static str,
    /// The 0–999 argument for the horizontal axis.
    pub x_pct: &'static str,
    /// The 0–999 argument for the vertical axis.
    pub y_pct: &'static str,
}

/// The primary point: `x`, `y`, `x_pct`, `y_pct`.
pub const TAP: Keys = Keys {
    x: "x",
    y: "y",
    x_pct: "x_pct",
    y_pct: "y_pct",
};

/// The far end of a swipe: `x2`, `y2`, `x2_pct`, `y2_pct`.
pub const END: Keys = Keys {
    x: "x2",
    y: "y2",
    x_pct: "x2_pct",
    y_pct: "y2_pct",
};

/// A point on a screen, in both spaces once the size is known.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Point {
    /// Device pixels — what the input dispatch takes.
    pub x: i32,
    pub y: i32,
    /// The 0–999 position, when the screen size is known.
    ///
    /// For a point that arrived as a percent this is the caller's own value,
    /// so feeding it back lands on the same pixel. For a point that arrived in
    /// pixels it is the nearest step, which may round by a pixel.
    pub x_pct: Option<i32>,
    pub y_pct: Option<i32>,
}

impl Point {
    /// The pixel pair, as the dispatch layer wants it.
    pub fn pixels(&self) -> (i32, i32) {
        (self.x, self.y)
    }

    /// The point as a tool result reports it: pixels, and the percent beside
    /// them when there is a size to compute it from.
    pub fn json(&self) -> Value {
        let mut value = json!({ "x": self.x, "y": self.y });
        if let (Some(x), Some(y)) = (self.x_pct, self.y_pct) {
            value["x_pct"] = json!(x);
            value["y_pct"] = json!(y);
        }
        value
    }
}

/// A percent of one axis as a pixel.
///
/// The scale is `[0, PCT_MAX] → [0, extent - 1]`, so `999` is the last
/// addressable pixel rather than one past it — a tap at the far edge has to
/// land *on* the screen, not outside it. Values outside `0–999` are clamped
/// here; [`resolve`] is where an out-of-range argument is refused, because this
/// function is also used to describe what was dispatched.
pub fn to_px(pct: i64, extent: u32) -> i32 {
    if extent <= 1 {
        return 0;
    }
    let span = i64::from(extent - 1);
    let scaled = (pct.clamp(0, PCT_MAX) * span + PCT_MAX / 2) / PCT_MAX;
    i32::try_from(scaled.clamp(0, span)).unwrap_or(0)
}

/// A pixel as the nearest 0–999 step.
pub fn to_pct(px: i32, extent: u32) -> i32 {
    if extent <= 1 {
        return 0;
    }
    let span = i64::from(extent - 1);
    let scaled = (i64::from(px.max(0)) * PCT_MAX + span / 2) / span;
    i32::try_from(scaled.clamp(0, PCT_MAX)).unwrap_or(0)
}

/// Does this argument object name any point in the 0–999 space?
///
/// Asked before a read that exists only to learn the screen size: a call that
/// stays in pixels must not pay for it. Nested objects and arrays are searched
/// too, because a `sequence` step's point lives inside the `steps` array.
pub fn uses_percent(value: &Value) -> bool {
    match value {
        Value::Object(map) => map.iter().any(|(key, item)| {
            (key.ends_with("_pct") && item.as_i64().is_some()) || uses_percent(item)
        }),
        Value::Array(items) => items.iter().any(uses_percent),
        _ => false,
    }
}

/// Resolve one point from `args`.
///
/// `size` is the screen the point is a point *of*, in pixels; `None` means the
/// caller cannot learn it, which is a refusal for a percent and harmless for a
/// pixel pair (the reply then carries no percent echo).
///
/// Rules, in the order they apply:
/// 1. A pixel argument wins, even when a percent is beside it. Pixels are what
///    every existing caller sends, and this whole module is an addition.
/// 2. A percent pair is converted against `size`, and refused when there is no
///    size or the value is outside `0–999`.
/// 3. Neither is the origin — the coordinate-less tap every caller meant
///    before any of this existed.
pub fn resolve(args: &Value, keys: Keys, size: Option<(i32, i32)>) -> Result<Point, String> {
    let pixel = |key: &str| args.get(key).and_then(Value::as_i64);
    let x = pixel(keys.x);
    let y = pixel(keys.y);
    let x_pct = pixel(keys.x_pct);
    let y_pct = pixel(keys.y_pct);

    if x.is_some() || y.is_some() {
        let x = i32::try_from(x.unwrap_or(0)).unwrap_or(0);
        let y = i32::try_from(y.unwrap_or(0)).unwrap_or(0);
        return Ok(from_pixels(x, y, size));
    }

    if x_pct.is_some() || y_pct.is_some() {
        let Some(size) = size.filter(|(width, height)| *width > 0 && *height > 0) else {
            return Err(format!(
                "`{}` / `{}` need the screen size to become pixels, and this screen's size \
                 could not be read — pass `{}` / `{}` in pixels instead",
                keys.x_pct, keys.y_pct, keys.x, keys.y
            ));
        };
        let x_pct = match x_pct {
            Some(value) => checked_pct(value, keys.x_pct)?,
            None => 0,
        };
        let y_pct = match y_pct {
            Some(value) => checked_pct(value, keys.y_pct)?,
            None => 0,
        };
        let (width, height) = (size.0 as u32, size.1 as u32);
        return Ok(Point {
            x: to_px(x_pct, width),
            y: to_px(y_pct, height),
            // The caller's own steps, so feeding them back lands on the same
            // pixel: `to_px` is deterministic in the same size.
            x_pct: Some(x_pct as i32),
            y_pct: Some(y_pct as i32),
        });
    }

    Ok(from_pixels(0, 0, size))
}

/// A pixel point, described in both spaces when the size is known.
///
/// The public entry for a point the caller did not spell out — an element's
/// centre from a tree, a scroll's computed start — so that a result can report
/// the percent beside the pixel it dispatched without going through argument
/// parsing.
pub fn from_pixels(x: i32, y: i32, size: Option<(i32, i32)>) -> Point {
    match size.filter(|(width, height)| *width > 0 && *height > 0) {
        Some((width, height)) => Point {
            x,
            y,
            x_pct: Some(to_pct(x, width as u32)),
            y_pct: Some(to_pct(y, height as u32)),
        },
        None => Point { x, y, x_pct: None, y_pct: None },
    }
}

/// A percent that is allowed to become a pixel.
fn checked_pct(value: i64, key: &str) -> Result<i64, String> {
    if (0..=PCT_MAX).contains(&value) {
        Ok(value)
    } else {
        Err(format!(
            "`{key}` is {value}, outside 0–999 — the axes run from 0 (near edge) to 999 \
             (far edge)"
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const PHONE: Option<(i32, i32)> = Some((1080, 2400));

    #[test]
    fn the_ends_of_the_axis_are_the_ends_of_the_screen() {
        // 999 is the last pixel, not one past it: a tap at the far edge has to
        // land on the screen.
        assert_eq!(to_px(0, 1080), 0);
        assert_eq!(to_px(999, 1080), 1079);
        assert_eq!(to_px(0, 2400), 0);
        assert_eq!(to_px(999, 2400), 2399);
    }

    #[test]
    fn the_middle_is_the_middle() {
        // A percent the model read off a picture is a percent it can send back
        // and expect the middle.
        assert_eq!(to_px(500, 1080), 540);
        assert_eq!(to_px(500, 2400), 1201);
        assert_eq!(to_px(250, 1000), 250);
        assert_eq!(to_px(750, 1000), 750);
    }

    #[test]
    fn a_one_pixel_screen_is_the_origin_whatever_the_step() {
        assert_eq!(to_px(999, 1), 0);
        assert_eq!(to_px(500, 1), 0);
        assert_eq!(to_px(999, 0), 0);
        assert_eq!(to_pct(0, 1080), 0);
    }

    #[test]
    fn a_pixel_maps_back_to_the_nearest_step() {
        assert_eq!(to_pct(0, 1080), 0);
        assert_eq!(to_pct(1079, 1080), 999);
        assert_eq!(to_pct(540, 1080), 500);
        // Past the edge clamps rather than overflowing the axis.
        assert_eq!(to_pct(5000, 1080), 999);
        assert_eq!(to_pct(-10, 1080), 0);
    }

    #[test]
    fn a_percent_round_trip_lands_on_the_same_pixel() {
        // What a caller echoes back is the step it sent, so the second tap hits
        // the same place as the first even after the size is re-read.
        for step in [0, 1, 250, 500, 777, 998, 999] {
            let point = resolve(&json!({ "x_pct": step, "y_pct": step }), TAP, PHONE)
                .expect("a sized percent");
            let again = resolve(&json!({ "x_pct": point.x_pct, "y_pct": point.y_pct }), TAP, PHONE)
                .expect("its own echo");
            assert_eq!((point.x, point.y), (again.x, again.y), "step {step}");
        }
    }

    #[test]
    fn pixels_are_untouched_when_both_spaces_are_given() {
        // "不动既有像素语义": a tree's coordinates are exact, and a percent
        // beside them must not round them.
        let point = resolve(&json!({ "x": 540, "y": 1200, "x_pct": 0, "y_pct": 0 }), TAP, PHONE)
            .expect("a pixel point");
        assert_eq!(point.pixels(), (540, 1200));
    }

    #[test]
    fn a_pixel_point_still_comes_back_with_its_percent() {
        let point = resolve(&json!({ "x": 540, "y": 1200 }), TAP, PHONE).expect("a pixel point");
        assert_eq!(point.x_pct, Some(500));
        assert_eq!(point.y_pct, Some(500));
    }

    #[test]
    fn no_coordinates_at_all_is_still_the_origin() {
        // Every tap before this module meant (0, 0) when it named nothing, and
        // callers that never mention a point must keep meaning it.
        let point = resolve(&json!({}), TAP, PHONE).expect("the origin");
        assert_eq!(point.pixels(), (0, 0));
        assert_eq!(point.x_pct, Some(0));
    }

    #[test]
    fn a_percent_without_a_size_is_refused_not_guessed() {
        let refused =
            resolve(&json!({ "x_pct": 500, "y_pct": 500 }), TAP, None).expect_err("no size");
        assert!(refused.contains("x_pct"), "{refused}");
        assert!(refused.contains("pixels"), "{refused}");
        // A pixel point on the same call is fine — it does not need the size.
        assert!(resolve(&json!({ "x": 1, "y": 2 }), TAP, None).is_ok());
    }

    #[test]
    fn a_step_past_the_end_of_the_axis_is_refused() {
        for step in [-1, 1000] {
            let refused = resolve(&json!({ "x_pct": step, "y_pct": 0 }), TAP, PHONE)
                .expect_err("out of range");
            assert!(refused.contains("0–999"), "{refused}");
        }
    }

    #[test]
    fn the_far_end_has_its_own_keys() {
        let end = resolve(&json!({ "x2_pct": 999, "y2_pct": 0 }), END, PHONE).expect("the end");
        assert_eq!(end.pixels(), (1079, 0));
        // The primary point's keys are not read as the end's.
        let end = resolve(&json!({ "x_pct": 500, "y_pct": 500 }), END, PHONE).expect("origin end");
        assert_eq!(end.pixels(), (0, 0));
    }

    #[test]
    fn percent_use_is_found_inside_a_sequence() {
        // The one read that learns the screen size is paid for only when a
        // percent is in play — including one nested in a `sequence` step.
        assert!(!uses_percent(&json!({ "action": "tap", "x": 1, "y": 2 })));
        assert!(uses_percent(&json!({ "action": "tap", "x_pct": 1, "y_pct": 2 })));
        assert!(!uses_percent(&json!({ "action": "sequence", "steps": [{ "x": 1, "y": 2 }] })));
        assert!(uses_percent(
            &json!({ "action": "sequence", "steps": [{ "x_pct": 1, "y_pct": 2 }] })
        ));
        // A `_pct` key holding something that is not a number names no point.
        assert!(!uses_percent(&json!({ "x_pct": "half" })));
    }

    #[test]
    fn the_reply_carries_both_spaces() {
        let point = resolve(&json!({ "x_pct": 500, "y_pct": 999 }), TAP, PHONE).expect("a point");
        let json = point.json();
        assert_eq!(json["x"], 540);
        assert_eq!(json["y"], 2399);
        assert_eq!(json["x_pct"], 500);
        assert_eq!(json["y_pct"], 999);
        // Without a size the reply is honest about the half it knows.
        let json = resolve(&json!({ "x": 7, "y": 9 }), TAP, None)
            .expect("pixels")
            .json();
        assert_eq!(json["x"], 7);
        assert!(json.get("x_pct").is_none());
    }
}
