//! OCR for still image files.
//!
//! The screen-OCR pipeline in [`super::ocr_rapid`] works on `Screenshot`s,
//! which carry a base64 body; this module loads an image from disk into that
//! shape and runs the same local RapidOCR models over it. No network access is
//! needed once the models are cached under `~/.syscity/models/vision/`.
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
    let det = paths.det.to_str().ok_or_else(|| {
        crate::error::SyscityError::Validation("det model path is not UTF-8".into())
    })?;
    let rec = paths.rec.to_str().ok_or_else(|| {
        crate::error::SyscityError::Validation("rec model path is not UTF-8".into())
    })?;
    let dict = paths
        .rec_dict
        .to_str()
        .ok_or_else(|| crate::error::SyscityError::Validation("dict path is not UTF-8".into()))?;
    RapidOcr::new(det, rec, dict)
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
}

/// Read the text in an image file, in reading order.
///
/// Returns empty text when the detector finds no text at all — that is a
/// meaningful answer for a photo or diagram, distinct from an OCR failure,
/// which is an `Err`.
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
    let Some(ocr) = guard.as_mut() else {
        return Err(crate::error::SyscityError::Internal(
            "OCR engine is not initialized".to_string(),
        ));
    };

    let mut blocks = ocr.detect_text(&screenshot).await?;
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

    #[tokio::test]
    async fn unreadable_image_is_an_error() {
        let dir = std::env::temp_dir().join(format!("syscity-ocr-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("not-an-image.png");
        std::fs::write(&path, b"definitely not a png").unwrap();
        let err = ocr_image_file(&path).await.unwrap_err();
        std::fs::remove_dir_all(&dir).ok();
        assert!(err.to_string().contains("Cannot decode image"), "got: {err}");
    }
}
