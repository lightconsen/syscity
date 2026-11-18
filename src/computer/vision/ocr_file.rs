//! OCR for an image the caller already holds.
//!
//! The screen-OCR pipeline in [`super::ocr_rapid`] works on `Screenshot`s,
//! which carry a base64 body. This module feeds it from both shapes a caller
//! actually has: a file on disk ([`ocr_image_file`]) and a capture taken for
//! one step ([`ocr_screenshot`]). No network access is needed once the models
//! are cached under `~/.syscity/models/vision/`.
//!
//! The public entry points differ in exactly one way: a user who asks to OCR a
//! file is asking for a result and may be given the models, so that path
//! downloads when they are missing. A step that reads a field back is not — it
//! already has a picture and a fallback, and spending a minute of somebody's
//! link inside a typing step is not a trade it may make, so that path only
//! looks at what is installed.
//!
//! The engine is process-wide and lazily initialized: loading the ONNX models
//! costs seconds, and a single mutex also serializes inference.

use std::path::Path;
use std::sync::OnceLock;

use super::ocr_rapid::RapidOcr;
use crate::computer::types::Screenshot;

/// Lazily initialized, process-wide OCR engine.
fn shared_engine() -> &'static tokio::sync::Mutex<Option<RapidOcr>> {
    static ENGINE: OnceLock<tokio::sync::Mutex<Option<RapidOcr>>> = OnceLock::new();
    ENGINE.get_or_init(|| tokio::sync::Mutex::new(None))
}

/// Initialize the shared engine from the cached RapidOCR models.
async fn engine() -> crate::Result<RapidOcr> {
    let paths = super::model_download::resolve_or_download_ocr_models()
        .await
        .map_err(|e| crate::error::SyscityError::Internal(format!("OCR models: {}", e)))?;
    RapidOcr::new(
        paths.det.to_str().ok_or_else(|| not_utf8("det"))?,
        paths.rec.to_str().ok_or_else(|| not_utf8("rec"))?,
        paths
            .rec_dict
            .to_str()
            .ok_or_else(|| not_utf8("dictionary"))?,
    )
}

fn not_utf8(which: &str) -> crate::error::SyscityError {
    crate::error::SyscityError::Validation(format!("OCR {which} model path is not UTF-8"))
}

/// Initialize the shared engine from models that are **already installed**.
///
/// Never downloads, and never creates the model directory: this is the path a
/// live step takes, and "the models were never fetched" has to stay a cheap
/// answer rather than becoming an 88 MB side effect of typing into a field.
async fn engine_installed(model_dir: &str) -> crate::Result<RapidOcr> {
    let Some((det, rec, dict)) = super::model_download::ocr_model_paths(model_dir) else {
        return Err(crate::error::SyscityError::Internal(format!(
            "no OCR models under {}",
            super::model_download::vision_model_dir(model_dir).display()
        )));
    };
    let (det, rec, dict) = (
        det.to_str().ok_or_else(|| not_utf8("det"))?.to_string(),
        rec.to_str().ok_or_else(|| not_utf8("rec"))?.to_string(),
        dict.to_str()
            .ok_or_else(|| not_utf8("dictionary"))?
            .to_string(),
    );
    // Loading the two sessions takes seconds of CPU. It runs on a blocking
    // thread so the runtime keeps serving other steps while it happens.
    tokio::task::spawn_blocking(move || RapidOcr::new(&det, &rec, &dict))
        .await
        .map_err(|e| {
            crate::error::SyscityError::Internal(format!("the OCR model load task failed: {e}"))
        })?
}

/// Reorder blocks into reading order: line by line, left to right.
///
/// [`RapidOcr::detect_text`] sorts by `y` only, which interleaves the columns
/// of a table onto separate lines. Bucketing by the height of the smallest
/// block groups text that shares a baseline while staying a total order.
fn sort_reading_order(blocks: &mut [crate::computer::vision::TextBlock]) {
    let row_height = blocks
        .iter()
        .map(|b| b.bounds.height.max(1))
        .min()
        .unwrap_or(1) as f32;
    blocks.sort_by_key(|b| ((b.bounds.y as f32 / row_height).round() as i32, b.bounds.x));
}

/// Text read from an image, with the quality signal a caller needs to decide
/// how much to trust it.
#[derive(Debug, Clone, PartialEq)]
pub struct OcrText {
    /// Recognized text, one detected block per line, in reading order.
    pub text: String,
    /// Mean per-block confidence (0.0 when nothing was detected).
    pub mean_confidence: f32,
    /// Number of text blocks that went into `text`.
    pub blocks: usize,
}

impl OcrText {
    /// Whether the recognition is solid enough to rely on. Below this the
    /// text is often fragments of the right characters in the wrong order —
    /// useful as a hint, dangerous as evidence.
    pub fn is_reliable(&self) -> bool {
        self.mean_confidence >= 0.75
    }

    /// The reading of an image whose detector found no text at all.
    ///
    /// Distinct from an error: a photo of a landscape and an empty text field
    /// both read as nothing, and both are answers. [`is_reliable`] is false for
    /// it, so a caller comparing text will treat it as "could not answer"
    /// rather than as "the field is empty".
    ///
    /// [`is_reliable`]: OcrText::is_reliable
    pub(crate) fn nothing_found() -> Self {
        Self {
            text: String::new(),
            mean_confidence: 0.0,
            blocks: 0,
        }
    }
}

/// Read the text in an image file, in reading order.
///
/// Returns empty text when the detector finds no text at all — that is a
/// meaningful answer for a photo or diagram, distinct from an OCR failure,
/// which is an `Err`.
///
/// Downloads the models when they are not installed. That is what a person
/// asking "what does this image say" wants, and they are waiting on the answer.
pub async fn ocr_image_file(path: &Path) -> crate::Result<OcrText> {
    let bytes = tokio::fs::read(path).await.map_err(|e| {
        crate::error::SyscityError::Internal(format!("Cannot read image {}: {}", path.display(), e))
    })?;
    let (width, height) = image::image_dimensions(path).map_err(|e| {
        crate::error::SyscityError::Internal(format!(
            "Cannot decode image {}: {}",
            path.display(),
            e
        ))
    })?;

    let base64 = base64::Engine::encode(&base64::engine::general_purpose::STANDARD, &bytes);
    let screenshot = Screenshot::new(base64, width, height);

    let mut guard = shared_engine().lock().await;
    if guard.is_none() {
        *guard = Some(engine().await?);
    }
    read_with(&mut guard, &screenshot).await
}

/// Read the text in an already-captured screenshot, in reading order.
///
/// The image a step just took is the image a person would lean in to read; it
/// is already in hand, so no file is written to hand it over. Only the models
/// that are installed are used — see [`engine_installed`].
///
/// `crop` is in the image's own pixels: the caller has a region in screen
/// points and the frame's scale, and turning one into the other is
/// [`CoordinateSpace::screen_rect_to_image`]'s job, not this function's. The
/// whole frame is read when it is absent.
///
/// `Err` means OCR could not run at all (no models, an undecodable image, a
/// failed session). Empty text is different: the detector ran and found
/// nothing, which is what an empty field looks like.
///
/// [`CoordinateSpace::screen_rect_to_image`]: crate::computer::display::CoordinateSpace::screen_rect_to_image
pub async fn ocr_screenshot(
    screenshot: &Screenshot,
    crop: Option<crate::computer::types::Rect>,
    model_dir: &str,
) -> crate::Result<OcrText> {
    // Crop first, engine second. A box that shares no pixels with the frame
    // is answered without loading anything — the field was outside the
    // capture, and a detector run over the whole screen would answer a
    // question nobody asked, with the screen's text instead of the field's.
    let source = match crop {
        Some(rect) => match crop_screenshot(screenshot, rect)? {
            Some(source) => source,
            None => return Ok(OcrText::nothing_found()),
        },
        None => screenshot.clone(),
    };
    let mut guard = shared_engine().lock().await;
    if guard.is_none() {
        *guard = Some(engine_installed(model_dir).await?);
    }
    read_with(&mut guard, &source).await
}

/// Run the shared engine over one image and package the answer.
async fn read_with(
    guard: &mut tokio::sync::MutexGuard<'_, Option<RapidOcr>>,
    screenshot: &Screenshot,
) -> crate::Result<OcrText> {
    let Some(ocr) = guard.as_mut() else {
        return Err(crate::error::SyscityError::Internal(
            "OCR engine is not initialized".to_string(),
        ));
    };

    let mut blocks = ocr.detect_text(screenshot).await?;
    sort_reading_order(&mut blocks);

    let mean_confidence = if blocks.is_empty() {
        0.0
    } else {
        blocks.iter().map(|b| b.confidence).sum::<f32>() / blocks.len() as f32
    };
    let count = blocks.len();
    Ok(OcrText {
        text: blocks
            .into_iter()
            .map(|b| b.text)
            .collect::<Vec<_>>()
            .join("\n"),
        mean_confidence,
        blocks: count,
    })
}

/// The part of `screenshot` named by `rect`, re-encoded as its own capture.
///
/// `None` when the rectangle shares no pixels with the frame. The crop keeps
/// the frame's own encoding, so the result is the same kind of object the
/// detector was already given rather than a second decode path.
fn crop_screenshot(
    screenshot: &Screenshot,
    rect: crate::computer::types::Rect,
) -> crate::Result<Option<Screenshot>> {
    let decoded = super::preprocess::decode_screenshot(screenshot)?;
    let Some(cropped) = super::preprocess::crop_clamped(&decoded, rect) else {
        return Ok(None);
    };
    let mut png = std::io::Cursor::new(Vec::new());
    cropped
        .write_to(&mut png, image::ImageFormat::Png)
        .map_err(|e| {
            crate::error::SyscityError::Internal(format!("Cannot encode the crop: {e}"))
        })?;
    let base64 =
        base64::Engine::encode(&base64::engine::general_purpose::STANDARD, png.into_inner());
    Ok(Some(Screenshot::new(base64, cropped.width(), cropped.height())))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::computer::types::Rect;
    use crate::computer::vision::TextBlock;

    fn block(text: &str, x: i32, y: i32, height: u32) -> TextBlock {
        TextBlock {
            text: text.to_string(),
            confidence: 0.99,
            bounds: Rect::new(x, y, 40, height),
        }
    }

    #[test]
    fn reading_order_reads_rows_left_to_right() {
        // Two rows of a table, delivered column-major by the detector.
        let mut blocks = vec![
            block("1/2", 0, 0, 20),
            block("3/4", 0, 30, 20),
            block("5/6", 100, 0, 20),
            block("7/8", 100, 30, 20),
        ];
        sort_reading_order(&mut blocks);
        let texts: Vec<&str> = blocks.iter().map(|b| b.text.as_str()).collect();
        assert_eq!(texts, ["1/2", "5/6", "3/4", "7/8"]);
    }

    #[test]
    fn reading_order_keeps_blocks_above_below_apart() {
        let mut blocks = vec![block("bottom", 0, 200, 20), block("top", 0, 0, 20)];
        sort_reading_order(&mut blocks);
        let texts: Vec<&str> = blocks.iter().map(|b| b.text.as_str()).collect();
        assert_eq!(texts, ["top", "bottom"]);
    }

    #[tokio::test]
    async fn missing_file_is_an_error() {
        let err = ocr_image_file(Path::new("/nonexistent/syscity-ocr-test.png"))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("Cannot read image"), "got: {err}");
    }

    #[test]
    fn a_crop_keeps_the_field_and_nothing_around_it() {
        // A 200×100 frame with the field at (20, 10) 80×20. The crop the
        // read-back hands the recognizer is that box and only that box — the
        // rest of the screen is exactly what must not be read.
        let frame = crate::computer::vision::preprocess::crop_clamped(
            &image::DynamicImage::new_rgb8(200, 100),
            Rect::new(20, 10, 80, 20),
        )
        .expect("an area inside the frame");
        assert_eq!((frame.width(), frame.height()), (80, 20));
    }

    #[test]
    fn a_crop_that_runs_off_the_edge_is_shortened_not_refused() {
        // A control wider than the display, or one mid-scroll: the part that
        // exists is still worth reading.
        let image = image::DynamicImage::new_rgb8(200, 100);
        let clipped =
            crate::computer::vision::preprocess::crop_clamped(&image, Rect::new(150, 80, 100, 100))
                .expect("the overlapping part");
        assert_eq!((clipped.width(), clipped.height()), (50, 20));
    }

    #[test]
    fn a_crop_outside_the_frame_names_no_pixels() {
        // `None`, not a black rectangle: a frame that does not contain the box
        // cannot answer a question about it, and reading the whole screen
        // would answer with the wrong field's text.
        let image = image::DynamicImage::new_rgb8(200, 100);
        assert!(crate::computer::vision::preprocess::crop_clamped(
            &image,
            Rect::new(300, 300, 80, 20)
        )
        .is_none());
        assert!(
            crate::computer::vision::preprocess::crop_clamped(&image, Rect::new(10, 10, 0, 0))
                .is_none()
        );
    }

    #[test]
    fn an_empty_reading_is_not_a_reliable_one() {
        // What a crop outside the frame returns, and what an empty field's
        // crop returns. `is_reliable` false is what keeps the read-back from
        // reading either as "the field is empty, so the typing failed".
        let nothing = OcrText::nothing_found();
        assert!(nothing.text.is_empty());
        assert_eq!(nothing.blocks, 0);
        assert!(!nothing.is_reliable());
    }

    /// The whole read of a frame, against the models on this machine.
    ///
    /// Falsifiable before being kept: it failed while `ocr_screenshot` ignored
    /// its `crop` argument, because the reading came back with the text outside
    /// the box as well as the text inside it.
    ///
    /// The frame is drawn at runtime from 5×7 glyph bitmaps — no font is
    /// available in a test and none should be. The glyphs are shaped like text
    /// because the detector looks for text shapes; what the recognizer spells
    /// them is up to the model, and asserting a spelling here would be testing
    /// the model rather than this code.
    ///
    /// The skip is loud on stderr: a build machine without the optional models
    /// should see that this never ran rather than assume a green tick means it
    /// did.
    #[tokio::test]
    async fn a_cropped_frame_reads_nothing_from_outside_the_box() {
        use image::{Rgb, RgbImage};

        /// 5×7 bitmaps, one per row, most significant bit leftmost.
        const GLYPHS: &[[u8; 7]] = &[
            // H
            [
                0b10001, 0b10001, 0b10001, 0b11111, 0b10001, 0b10001, 0b10001,
            ],
            // E
            [
                0b11111, 0b10000, 0b10000, 0b11110, 0b10000, 0b10000, 0b11111,
            ],
            // L
            [
                0b10000, 0b10000, 0b10000, 0b10000, 0b10000, 0b10000, 0b11111,
            ],
            // O
            [
                0b01110, 0b10001, 0b10001, 0b10001, 0b10001, 0b10001, 0b01110,
            ],
        ];
        /// One glyph cell in pixels; large enough that the strokes are strokes
        /// rather than noise to the detector.
        const SCALE: u32 = 12;

        let mut frame = RgbImage::from_pixel(400, 200, Rgb([255, 255, 255]));
        let (left, top) = (20, 50);
        for (index, glyph) in GLYPHS.iter().enumerate() {
            let origin = left + index as u32 * (5 * SCALE + SCALE);
            for (row, bits) in glyph.iter().enumerate() {
                for column in 0..5u32 {
                    if bits & (1 << (4 - column)) == 0 {
                        continue;
                    }
                    for dy in 0..SCALE {
                        for dx in 0..SCALE {
                            frame.put_pixel(
                                origin + column * SCALE + dx,
                                top + row as u32 * SCALE + dy,
                                Rgb([0, 0, 0]),
                            );
                        }
                    }
                }
            }
        }
        let mut png = std::io::Cursor::new(Vec::new());
        image::DynamicImage::ImageRgb8(frame)
            .write_to(&mut png, image::ImageFormat::Png)
            .expect("encode the fixture");
        let base64 =
            base64::Engine::encode(&base64::engine::general_purpose::STANDARD, png.into_inner());
        let shot = Screenshot::new(base64, 400, 200);

        if crate::computer::vision::model_download::ocr_model_paths("").is_none() {
            eprintln!(
                "SKIPPED a_cropped_frame_reads_nothing_from_outside_the_box: no OCR models under {}",
                crate::computer::vision::model_download::vision_model_dir("").display()
            );
            return;
        }

        // The glyphs are in the top band (y 50..134); this box is below them.
        let empty = ocr_screenshot(&shot, Some(Rect::new(10, 150, 200, 40)), "")
            .await
            .expect("the models are installed");
        assert!(
            empty.text.trim().is_empty(),
            "the text outside the box was read: {:?}",
            empty.text
        );
        assert!(!empty.is_reliable(), "nothing found cannot be a reliable reading");

        // The whole frame does see the glyphs, so the assertion above is about
        // the box and not about a detector that found nothing anywhere.
        let whole = ocr_screenshot(&shot, None, "")
            .await
            .expect("the models are installed");
        assert!(
            !whole.text.trim().is_empty(),
            "the fixture drew nothing the detector could find, so the empty-box \
             assertion above proves nothing"
        );

        // And a box outside the frame is answered without reading at all.
        let outside = ocr_screenshot(&shot, Some(Rect::new(900, 900, 10, 10)), "")
            .await
            .expect("a box outside the frame is a reading of nothing");
        assert!(outside.text.is_empty());
    }
}
