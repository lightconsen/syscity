//! Screenshot, PDF and screencast actions.

use super::{BrowserAction, BrowserScreenshot};
use serde_json::{json, Map, Value};
use tracing::warn;

use base64::Engine;
use chromiumoxide::cdp::browser_protocol::page::CaptureScreenshotFormat;
use chromiumoxide::page::ScreenshotParamsBuilder;

use super::screencast::{screencast_start, screencast_stop};
use crate::browser::coordinate_space::{self, Capture, PageMetrics, Space};

pub(super) async fn execute_screenshot_actions(
    action: BrowserAction,
    page: &chromiumoxide::Page,
    _browser: Option<&chromiumoxide::Browser>,
    screenshot_data: &mut Option<BrowserScreenshot>,
) -> Result<serde_json::Value, String> {
    match action {
        BrowserAction::Screenshot { full_page, selector } => {
            let capture = match (&selector, full_page.unwrap_or(false)) {
                (Some(_), _) => Capture::Element,
                (None, true) => Capture::FullPage,
                (None, false) => Capture::Viewport,
            };
            // Two reads cannot be one, so the page's account of itself is taken
            // either side of the capture: a page that scrolled in between makes
            // the image and the metrics describe different moments, and that is
            // worth saying rather than averaging over.
            let metrics_before = coordinate_space::read(page).await;

            let result = match selector {
                Some(sel) => match page.find_element(&sel).await {
                    Ok(elem) => elem.screenshot(CaptureScreenshotFormat::Png).await,
                    Err(e) => Err(e),
                },
                None => {
                    let params = if full_page.unwrap_or(false) {
                        ScreenshotParamsBuilder::default()
                            .format(CaptureScreenshotFormat::Png)
                            .full_page(true)
                            .build()
                    } else {
                        ScreenshotParamsBuilder::default()
                            .format(CaptureScreenshotFormat::Png)
                            .build()
                    };
                    page.screenshot(params).await
                }
            };

            let metrics_after = coordinate_space::read(page).await;

            match result {
                Ok(data) => {
                    // CAS-first: store the PNG bytes once and hand back a
                    // compact reference; fall back to inline base64 when the
                    // store is unavailable (fail-open — see attachments docs).
                    // The clone keeps the raw bytes available for the fallback.
                    match crate::attachments::store_bytes_async(data.clone(), "image/png").await {
                        Ok(aref) => {
                            let note = format!(
                                "Screenshot captured ({} bytes, image/png), stored as {}",
                                aref.size,
                                crate::attachments::short_id(&aref.digest)
                            );
                            *screenshot_data = Some(BrowserScreenshot::Ref(aref.clone()));
                            Ok(with_coordinates(
                                json!({
                                    "success": true,
                                    "format": "png",
                                    "size": aref.size,
                                    "image_ref": aref.to_json(),
                                    "note": note,
                                }),
                                capture_fields(
                                    capture,
                                    &data,
                                    metrics_before.as_ref(),
                                    metrics_after.as_ref(),
                                ),
                            ))
                        }
                        Err(e) => {
                            warn!(
                                "attachment store write failed ({}); falling back to inline base64",
                                e
                            );
                            let base64 = base64::engine::general_purpose::STANDARD.encode(&data);
                            *screenshot_data = Some(BrowserScreenshot::Inline(base64.clone()));
                            Ok(with_coordinates(
                                json!({
                                    "success": true,
                                    "format": "png",
                                    "base64_length": base64.len(),
                                    "data": format!("data:image/png;base64,{}", base64)
                                }),
                                capture_fields(
                                    capture,
                                    &data,
                                    metrics_before.as_ref(),
                                    metrics_after.as_ref(),
                                ),
                            ))
                        }
                    }
                }
                Err(e) => Err(format!("Failed to take screenshot: {}", e)),
            }
        }

        BrowserAction::PrintToPdf {
            landscape,
            display_header_footer,
            print_background,
            scale,
            paper_width,
            paper_height,
            margin_top,
            margin_bottom,
            margin_left,
            margin_right,
            page_ranges,
        } => {
            use chromiumoxide::cdp::browser_protocol::page::PrintToPdfParams;

            let mut params = PrintToPdfParams::default();
            if let Some(v) = landscape {
                params.landscape = Some(v);
            }
            if let Some(v) = display_header_footer {
                params.display_header_footer = Some(v);
            }
            if let Some(v) = print_background {
                params.print_background = Some(v);
            }
            if let Some(v) = scale {
                params.scale = Some(v);
            }
            if let Some(v) = paper_width {
                params.paper_width = Some(v);
            }
            if let Some(v) = paper_height {
                params.paper_height = Some(v);
            }
            if let Some(v) = margin_top {
                params.margin_top = Some(v);
            }
            if let Some(v) = margin_bottom {
                params.margin_bottom = Some(v);
            }
            if let Some(v) = margin_left {
                params.margin_left = Some(v);
            }
            if let Some(v) = margin_right {
                params.margin_right = Some(v);
            }
            if let Some(ref v) = page_ranges {
                params.page_ranges = Some(v.clone());
            }

            match page.pdf(params).await {
                Ok(data) => {
                    let base64 = base64::engine::general_purpose::STANDARD.encode(&data);
                    Ok(json!({
                        "success": true,
                        "format": "pdf",
                        "base64_length": base64.len(),
                        "data": format!("data:application/pdf;base64,{}", base64)
                    }))
                }
                Err(e) => Err(format!("Failed to print PDF: {}", e)),
            }
        }

        BrowserAction::ScreencastStart { quality, every_nth_frame } => {
            screencast_start(page, quality, every_nth_frame).await
        }

        BrowserAction::ScreencastStop => screencast_stop(page).await,
        _ => Err("browser: action not handled by this group".to_string()),
    }
}

/// Merge the capture's coordinate-space fields into an action result.
fn with_coordinates(payload: Value, fields: Map<String, Value>) -> Value {
    let mut payload = payload;
    if let Some(object) = payload.as_object_mut() {
        object.extend(fields);
    }
    payload
}

/// What a screenshot actually is, in terms the caller can act on.
///
/// The image's size is read from its bytes rather than from the parameters that
/// asked for it — catching the case where those disagree is the entire point.
fn capture_fields(
    capture: Capture,
    image_bytes: &[u8],
    before: Option<&PageMetrics>,
    after: Option<&PageMetrics>,
) -> Map<String, Value> {
    let mut fields = Map::new();
    let image = crate::utils::png::dimensions(image_bytes);
    if let Some((width, height)) = image {
        fields.insert("image".to_string(), json!({ "width": width, "height": height }));
    }

    let Some(metrics) = before else {
        fields.insert("coordinate_space".to_string(), json!("unexplained"));
        fields.insert(
            "warning".to_string(),
            json!(
                "the page could not be asked for its viewport size, so no coordinate space can be \
                 stated for this image — take a fresh screenshot before clicking anything read \
                 from it"
            ),
        );
        return fields;
    };

    fields.insert(
        "viewport".to_string(),
        json!({ "width": metrics.width, "height": metrics.height }),
    );
    fields.insert("device_pixel_ratio".to_string(), json!(metrics.dpr));
    fields.insert("scroll".to_string(), json!({ "x": metrics.scroll_x, "y": metrics.scroll_y }));

    let space = match image {
        Some(size) => coordinate_space::classify(capture, size, metrics),
        None => Space::Unexplained {
            detail: "the capture is not a readable PNG".to_string(),
        },
    };
    fields.insert("coordinate_space".to_string(), json!(space.label()));
    fields.insert("coordinate_space_note".to_string(), json!(note_for(&space, metrics)));

    let mut warnings: Vec<String> = Vec::new();
    if let Space::Unexplained { detail } = &space {
        warnings.push(format!(
            "this screenshot's coordinate space could not be established ({detail})"
        ));
    }
    if let Some(mismatch) = coordinate_space::requested_mismatch(capture, &space) {
        warnings.push(mismatch);
    }
    if let (Some(before), Some(after)) = (before, after) {
        if let Some(moved) = coordinate_space::drift(before, after) {
            warnings.push(moved);
        }
    }
    if !warnings.is_empty() {
        fields.insert("warning".to_string(), json!(warnings.join(" ")));
    }

    fields
}

/// How to read coordinates off this particular image.
fn note_for(space: &Space, metrics: &PageMetrics) -> String {
    let ratio = metrics.ratio();
    match space {
        Space::Viewport => format!(
            "Coordinates are viewport CSS pixels: read image pixels and divide by {ratio} before \
             clicking."
        ),
        Space::FullPage => format!(
            "This image is the whole document, {} CSS pixels tall. Image pixels divided by {ratio} \
             are document coordinates; subtract the scroll offset ({}, {}) to get viewport \
             coordinates, and scroll before clicking anything below the fold.",
            metrics.doc_height, metrics.scroll_x, metrics.scroll_y
        ),
        Space::Element => {
            "This image is one element's box, so its coordinates are relative to that \
             element and cannot be clicked directly. Capture the viewport or the full page to get \
             clickable coordinates."
                .to_string()
        }
        Space::Unexplained { .. } => {
            "Coordinates from this image cannot be used; see the warning.".to_string()
        }
    }
}
