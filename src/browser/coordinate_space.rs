//! Which coordinate space a browser screenshot is in.
//!
//! `ClickAt` takes *viewport* CSS pixels. A screenshot is not always in that
//! space: a `full_page` capture is document-sized, a `selector` capture is that
//! element's box, and the device pixel ratio scales all three. So reading
//! `(x, y)` off an image and clicking it is silently wrong whenever the two
//! disagree — and both numbers look like numbers, so nothing downstream can
//! notice.
//!
//! This is the browser counterpart of the Android path's coordinate-space
//! check: state what was actually captured, and refuse to pretend when the
//! capture and the live page disagree.

use chromiumoxide::Page;
use serde::Deserialize;

/// What the live page looked like when a capture was taken.
///
/// All lengths are CSS pixels; `dpr` is what turns them into image pixels.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct PageMetrics {
    pub width: f64,
    pub height: f64,
    pub dpr: f64,
    pub scroll_x: f64,
    pub scroll_y: f64,
    pub doc_width: f64,
    pub doc_height: f64,
}

impl PageMetrics {
    /// The factor that turns CSS pixels into image pixels, guarded against a
    /// ratio the page never reports sensibly.
    pub fn ratio(&self) -> f64 {
        if self.dpr > 0.0 && self.dpr.is_finite() {
            self.dpr
        } else {
            1.0
        }
    }
}

/// The page's own account of itself, read in one round trip.
///
/// `innerWidth`/`innerHeight` are the layout viewport the screenshot captures;
/// `devicePixelRatio` carries browser zoom, which is why the image is bigger
/// than the viewport in CSS terms.
const METRICS_SCRIPT: &str = r#"() => ({
    width: window.innerWidth,
    height: window.innerHeight,
    dpr: window.devicePixelRatio || 1,
    scroll_x: window.scrollX,
    scroll_y: window.scrollY,
    doc_width: Math.max(document.documentElement.scrollWidth, window.innerWidth),
    doc_height: Math.max(document.documentElement.scrollHeight, window.innerHeight)
})"#;

/// Read the live page's metrics. `None` when the page cannot be asked (a
/// closed target, a detached frame) — callers treat that as "unknown", never as
/// "unchanged".
pub async fn read(page: &Page) -> Option<PageMetrics> {
    let result = page.evaluate(METRICS_SCRIPT).await.ok()?;
    result.into_value::<PageMetrics>().ok()
}

/// What kind of capture was asked for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Capture {
    Viewport,
    FullPage,
    Element,
}

/// What the returned image actually is.
#[derive(Debug, Clone, PartialEq)]
pub enum Space {
    /// The viewport, at the capture's device pixel ratio.
    Viewport,
    /// The whole document. Points are usable only after conversion, and most of
    /// them are outside the current viewport.
    FullPage,
    /// One element's box. Its coordinates are relative to that element and mean
    /// nothing to a click.
    Element,
    /// The image matches neither the viewport nor the document at the reported
    /// ratio — a scaled capture, a page that moved between the reads, or a
    /// viewport that changed. Coordinates taken from it cannot be trusted.
    Unexplained { detail: String },
}

impl Space {
    /// Stable wire value: the model branches on this rather than on prose.
    pub fn label(&self) -> &'static str {
        match self {
            Space::Viewport => "viewport",
            Space::FullPage => "full_page",
            Space::Element => "element",
            Space::Unexplained { .. } => "unexplained",
        }
    }
}

/// Sub-pixel layout and fractional ratios make the product inexact; two pixels
/// is the slack that rounding needs and far less than any real mismatch.
const ROUNDING_SLACK: f64 = 2.0;

fn within_rounding(actual: f64, expected: f64) -> bool {
    (actual - expected).abs() <= ROUNDING_SLACK
}

/// Decide what a capture is, from the image's real size and the page's account
/// of itself.
///
/// The image size must come from the bytes, not from the parameters used to
/// request them: the point of the check is to catch the case where the two
/// disagree.
pub fn classify(capture: Capture, image: (u32, u32), metrics: &PageMetrics) -> Space {
    if capture == Capture::Element {
        return Space::Element;
    }

    let ratio = metrics.ratio();
    let (w, h) = (f64::from(image.0), f64::from(image.1));
    let expected_width = metrics.width * ratio;

    if within_rounding(w, expected_width) {
        if within_rounding(h, metrics.height * ratio) {
            return Space::Viewport;
        }
        if within_rounding(h, metrics.doc_height * ratio) {
            return Space::FullPage;
        }
    }

    Space::Unexplained {
        detail: format!(
            "the capture is {w}x{h} image pixels; the viewport is {}x{} CSS pixels at ratio {}, \
             so a viewport capture would be about {}x{} and a full-page one about {}x{}",
            metrics.width,
            metrics.height,
            ratio,
            expected_width.round(),
            (metrics.height * ratio).round(),
            expected_width.round(),
            (metrics.doc_height * ratio).round(),
        ),
    }
}

/// Whether the capture is what was asked for. The one case worth naming is a
/// full-page request that came back viewport-sized, because then the caller's
/// assumption is inverted rather than merely imprecise.
pub fn requested_mismatch(capture: Capture, space: &Space) -> Option<String> {
    match (capture, space) {
        (Capture::FullPage, Space::Viewport) => Some(
            "full_page was requested but the capture is viewport-sized — the page may not scroll, \
             or the capture was clipped. Coordinates are viewport CSS pixels."
                .to_string(),
        ),
        (Capture::Viewport, Space::FullPage) => Some(
            "a viewport capture came back document-sized. The image needs converting before its \
             points can be clicked."
                .to_string(),
        ),
        _ => None,
    }
}

/// Convert a point taken from a screenshot into viewport CSS pixels.
///
/// Returns the conversion rather than performing it silently at click time: the
/// caller has to decide that the point is still worth clicking, because a
/// full-page point below the fold lands outside the current viewport.
pub fn to_viewport(
    point: (f64, f64),
    space: &Space,
    metrics: &PageMetrics,
) -> Result<(f64, f64), String> {
    let ratio = metrics.ratio();
    match space {
        Space::Viewport => Ok((point.0 / ratio, point.1 / ratio)),
        // A full-page image starts at the document top, so the scroll offset is
        // what separates the two origins.
        Space::FullPage => Ok((point.0 / ratio, point.1 / ratio - metrics.scroll_y)),
        Space::Element => Err(
            "coordinates in an element screenshot are relative to that element; take a viewport or \
             full-page screenshot to get clickable coordinates"
                .to_string(),
        ),
        Space::Unexplained { detail } => Err(format!(
            "this screenshot's coordinate space could not be established ({detail}); take a fresh \
             screenshot before clicking"
        )),
    }
}

/// Whether a click at these viewport CSS coordinates can land on anything.
///
/// A point outside the viewport can only come from a document-space reading, so
/// the refusal says where it came from and what to do instead. Dispatching it
/// anyway would put the event wherever the browser decides, which is the
/// silent-wrong-target failure this module exists to prevent.
pub fn check_click(point: (f64, f64), metrics: &PageMetrics) -> Result<(), String> {
    let (x, y) = point;
    let finite = x.is_finite() && y.is_finite();
    let inside = finite && x >= 0.0 && y >= 0.0 && x < metrics.width && y < metrics.height;
    if inside {
        return Ok(());
    }

    let where_it_is = if !finite {
        "not a finite point".to_string()
    } else {
        format!("outside the {}x{} viewport", metrics.width, metrics.height)
    };
    Err(format!(
        "({x}, {y}) is {where_it_is}. Clicks take viewport CSS pixels: if the point came from a \
         full-page screenshot, subtract the scroll offset ({}, {}) and divide by the device pixel \
         ratio ({}) first; if it is below the fold, scroll the page and take a fresh screenshot.",
        metrics.scroll_x,
        metrics.scroll_y,
        metrics.ratio()
    ))
}

/// Whether the page moved between the two reads that bracket a capture.
///
/// Two reads cannot be one, so a page that scrolled or resized in between makes
/// the image and the metrics describe different moments — worth saying rather
/// than averaging over.
pub fn drift(before: &PageMetrics, after: &PageMetrics) -> Option<String> {
    let mut differences = Vec::new();
    for (name, a, b) in [
        ("device pixel ratio", before.dpr, after.dpr),
        ("scroll x", before.scroll_x, after.scroll_x),
        ("scroll y", before.scroll_y, after.scroll_y),
        ("viewport width", before.width, after.width),
        ("viewport height", before.height, after.height),
    ] {
        if !within_rounding(a, b) {
            differences.push(format!("{name} {a} -> {b}"));
        }
    }

    if differences.is_empty() {
        None
    } else {
        Some(format!(
            "the page changed while the screenshot was taken ({}); the image and the viewport \
             sizes above describe different moments, so re-take the screenshot before acting on \
             coordinates read from it",
            differences.join(", ")
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn metrics() -> PageMetrics {
        PageMetrics {
            width: 1280.0,
            height: 720.0,
            dpr: 2.0,
            scroll_x: 0.0,
            scroll_y: 100.0,
            doc_width: 1280.0,
            doc_height: 3000.0,
        }
    }

    #[test]
    fn a_viewport_capture_is_recognised() {
        // 1280x720 CSS at ratio 2 is 2560x1440 image pixels.
        assert_eq!(classify(Capture::Viewport, (2560, 1440), &metrics()), Space::Viewport);
    }

    #[test]
    fn a_full_page_capture_is_recognised_even_when_not_asked_for() {
        assert_eq!(classify(Capture::Viewport, (2560, 6000), &metrics()), Space::FullPage);
    }

    #[test]
    fn a_selector_capture_is_never_a_clickable_space() {
        // Its size is whatever the element is; classifying it by page geometry
        // would be a coincidence at best.
        assert_eq!(classify(Capture::Element, (2560, 1440), &metrics()), Space::Element);
    }

    #[test]
    fn a_capture_that_matches_no_expectation_is_reported_rather_than_guessed() {
        // A scaled capture: 1280x720 image pixels at ratio 2 is half the viewport.
        let space = classify(Capture::Viewport, (1280, 720), &metrics());
        match space {
            Space::Unexplained { detail } => {
                assert!(detail.contains("1280x720"), "{detail}");
                assert!(detail.contains("ratio 2"), "{detail}");
            }
            other => panic!("expected Unexplained, got {other:?}"),
        }
    }

    #[test]
    fn rounding_slack_covers_fractional_ratios() {
        // 1.25 ratio on an odd width leaves a fraction that rounds to a pixel.
        let m = PageMetrics {
            width: 1001.0,
            height: 701.0,
            dpr: 1.25,
            ..metrics()
        };
        // True products are 1251.25 and 876.25.
        assert_eq!(classify(Capture::Viewport, (1251, 876), &m), Space::Viewport);
    }

    #[test]
    fn a_full_page_request_that_came_back_viewport_sized_says_so() {
        let said = requested_mismatch(Capture::FullPage, &Space::Viewport).unwrap();
        assert!(said.contains("viewport-sized"), "{said}");
        assert!(requested_mismatch(Capture::FullPage, &Space::FullPage).is_none());
        assert!(requested_mismatch(Capture::Viewport, &Space::Viewport).is_none());
        assert!(requested_mismatch(Capture::Element, &Space::Element).is_none());
    }

    #[test]
    fn converting_a_viewport_point_undoes_the_pixel_ratio() {
        let m = metrics();
        assert_eq!(to_viewport((256.0, 144.0), &Space::Viewport, &m).unwrap(), (128.0, 72.0));
    }

    #[test]
    fn converting_a_full_page_point_also_undoes_the_scroll() {
        let m = metrics();
        // Document y=600 image px -> CSS y=300 -> viewport y=200 after scrolling 100.
        assert_eq!(to_viewport((256.0, 600.0), &Space::FullPage, &m).unwrap(), (128.0, 200.0));
    }

    #[test]
    fn points_that_cannot_be_converted_are_refused_with_a_reason() {
        let m = metrics();
        let element = to_viewport((1.0, 1.0), &Space::Element, &m).unwrap_err();
        assert!(element.contains("relative to that element"), "{element}");

        let unknown = classify(Capture::Viewport, (7, 7), &m);
        let err = to_viewport((1.0, 1.0), &unknown, &m).unwrap_err();
        assert!(err.contains("could not be established"), "{err}");
    }

    #[test]
    fn a_click_inside_the_viewport_passes() {
        assert!(check_click((0.0, 0.0), &metrics()).is_ok());
        assert!(check_click((1279.0, 719.0), &metrics()).is_ok());
    }

    #[test]
    fn a_click_outside_the_viewport_is_refused_and_told_how_to_convert() {
        // The classic case: a y read off a full-page image, used as if viewport.
        let refused = check_click((600.0, 2400.0), &metrics()).unwrap_err();
        assert!(refused.contains("outside the 1280x720 viewport"), "{refused}");
        assert!(refused.contains("subtract the scroll offset"), "{refused}");
        assert!(refused.contains("scroll the page"), "{refused}");

        // The exact boundary is outside: the last addressable pixel is width-1.
        assert!(check_click((1280.0, 10.0), &metrics()).is_err());
        assert!(check_click((-1.0, 10.0), &metrics()).is_err());
        assert!(check_click((f64::NAN, 10.0), &metrics()).is_err());
    }

    #[test]
    fn drift_names_what_moved() {
        let before = metrics();
        assert!(drift(&before, &before.clone()).is_none());

        let after = PageMetrics {
            scroll_y: 900.0,
            ..before.clone()
        };
        let said = drift(&before, &after).unwrap();
        assert!(said.contains("scroll y 100 -> 900"), "{said}");
        assert!(said.contains("re-take the screenshot"), "{said}");
    }
}
