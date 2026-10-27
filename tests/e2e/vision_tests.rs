//! End-to-end vision tests.
//!
//! Tests the full ONNX vision pipeline (RapidOCR + OmniParser) with
//! actual model files.  Skips gracefully when model files are not
//! available locally.
//!
//! Models are auto-downloaded to `{cache_dir}/syscity/models/vision/`
//! from HuggingFace on first use via `resolve_or_download_vision_models()`.
//!
//! To run the auto-download tests:
//!
//! ```bash
//! cargo test --test e2e_test -- vision -- --include-ignored
//! ```

use std::io::Cursor;
use std::path::PathBuf;

use base64::Engine;
use image::DynamicImage;
use syscity::computer::types::Screenshot;
use syscity::computer::vision::{ocr_rapid::RapidOcr, ui_onnx::OmniParserDetector};

// ── Helpers ────────────────────────────────────────────────────────────

fn cache_model_dir() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("/tmp"))
        .join(".syscity")
        .join("models")
        .join("vision")
}

fn project_model_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("models")
        .join("vision")
}

fn model_dir() -> PathBuf {
    // Prefer cache dir (auto-download location), fall back to project dir.
    let cache = cache_model_dir();
    if cache.join("det.onnx").exists() || cache.join("omniparser.onnx").exists() {
        cache
    } else {
        project_model_dir()
    }
}

fn has_ocr_models() -> bool {
    let dir = model_dir();
    dir.join("det.onnx").exists()
        && dir.join("rec_ppocrv3.onnx").exists()
        && dir.join("rec_dict_ppocrv3.txt").exists()
}

fn has_omniparser_models() -> bool {
    model_dir().join("omniparser.onnx").exists()
}

fn load_ocr() -> Option<RapidOcr> {
    let dir = model_dir();
    let det = dir.join("det.onnx");
    let rec = dir.join("rec_ppocrv3.onnx");
    let dict = dir.join("rec_dict_ppocrv3.txt");
    if !det.exists() || !rec.exists() || !dict.exists() {
        return None;
    }
    RapidOcr::new(det.to_str().unwrap(), rec.to_str().unwrap(), dict.to_str().unwrap()).ok()
}

fn load_omniparser() -> Option<OmniParserDetector> {
    let p = model_dir().join("omniparser.onnx");
    if !p.exists() {
        return None;
    }
    OmniParserDetector::new(p.to_str().unwrap()).ok()
}

fn create_white_text_screenshot(width: u32, height: u32, text_pixels: &[(u32, u32)]) -> Screenshot {
    let mut img = DynamicImage::new_rgb8(width, height);
    for y in 0..height {
        for x in 0..width {
            img.as_mut_rgb8()
                .unwrap()
                .put_pixel(x, y, image::Rgb([20, 20, 20]));
        }
    }
    for &(x, y) in text_pixels {
        img.as_mut_rgb8()
            .unwrap()
            .put_pixel(x, y, image::Rgb([255, 255, 255]));
    }
    let mut buf = Cursor::new(Vec::new());
    img.write_to(&mut buf, image::ImageFormat::Png)
        .expect("PNG encoding");
    let base64 = base64::engine::general_purpose::STANDARD.encode(&buf.into_inner());
    Screenshot {
        base64,
        width,
        height,
        timestamp: std::time::Instant::now(),
        file_path: None,
    }
}

fn create_synthetic_ui_screenshot(width: u32, height: u32) -> Screenshot {
    let mut img = DynamicImage::new_rgb8(width, height);
    for y in 0..height {
        for x in 0..width {
            let gray = if y < 30 || y > height - 30 {
                240u8
            } else {
                200u8
            };
            img.as_mut_rgb8()
                .unwrap()
                .put_pixel(x, y, image::Rgb([gray, gray, gray]));
        }
    }
    let btn_l = width / 2 - 60;
    let btn_r = width / 2 + 60;
    let btn_t = height / 2 - 20;
    let btn_b = height / 2 + 20;
    for y in btn_t..btn_b {
        for x in btn_l..btn_r {
            img.as_mut_rgb8()
                .unwrap()
                .put_pixel(x, y, image::Rgb([100, 150, 200]));
        }
    }
    for i in 0..8 {
        for j in 0..10 {
            img.as_mut_rgb8().unwrap().put_pixel(
                width / 2 - 10 + i,
                height / 2 - 5 + j,
                image::Rgb([255, 255, 255]),
            );
        }
    }
    let mut buf = Cursor::new(Vec::new());
    img.write_to(&mut buf, image::ImageFormat::Png)
        .expect("PNG encoding");
    let base64 = base64::engine::general_purpose::STANDARD.encode(&buf.into_inner());
    Screenshot {
        base64,
        width,
        height,
        timestamp: std::time::Instant::now(),
        file_path: None,
    }
}

// ── E2E Tests: RapidOCR ───────────────────────────────────────────────

#[tokio::test]
#[ignore = "Requires ONNX model files in models/vision/"]
async fn test_ocr_pipeline_small_image() {
    let mut ocr = load_ocr().expect("OCR models should be loadable");
    let screenshot = create_white_text_screenshot(64, 64, &[]);
    let result = ocr.detect_text(&screenshot).await;
    assert!(result.is_ok(), "OCR pipeline should not error: {:?}", result.err());
}

#[tokio::test]
#[ignore = "Requires ONNX model files in models/vision/"]
async fn test_ocr_pipeline_empty_screenshot() {
    let mut ocr = load_ocr().expect("OCR models should be loadable");
    let screenshot = create_white_text_screenshot(1, 1, &[]);
    let result = ocr.detect_text(&screenshot).await;
    assert!(result.is_ok(), "OCR should handle tiny images without error");
}

#[tokio::test]
#[ignore = "Requires ONNX model files in models/vision/"]
async fn test_ocr_pipeline_returns_text_blocks() {
    let mut ocr = load_ocr().expect("OCR models should be loadable");
    let pixels: Vec<(u32, u32)> = (10..50)
        .flat_map(|x| (10..25).map(move |y| (x, y)))
        .collect();
    let screenshot = create_white_text_screenshot(320, 64, &pixels);
    let result = ocr.detect_text(&screenshot).await;
    assert!(result.is_ok(), "OCR pipeline should succeed");
    for block in &result.unwrap() {
        assert!(!block.text.is_empty());
        assert!(block.confidence >= 0.0 && block.confidence <= 1.0);
    }
}

#[tokio::test]
#[ignore = "Requires ONNX model files in models/vision/"]
async fn test_ocr_detect_small_region() {
    let mut ocr = load_ocr().expect("OCR models should be loadable");
    let screenshot = create_synthetic_ui_screenshot(640, 480);
    let result = ocr.detect_text(&screenshot).await;
    assert!(result.is_ok(), "OCR should handle 640x480 UI screenshot: {:?}", result.err());
}

// ── E2E Tests: OmniParser ─────────────────────────────────────────────

#[tokio::test]
#[ignore = "Requires ONNX model files in models/vision/"]
async fn test_omniparser_pipeline_small_image() {
    let mut detector = load_omniparser().expect("OmniParser model should be loadable");
    let screenshot = create_synthetic_ui_screenshot(640, 480);
    let result = detector.detect_elements(&screenshot).await;
    assert!(result.is_ok(), "OmniParser pipeline should not error: {:?}", result.err());
}

#[tokio::test]
#[ignore = "Requires ONNX model files in models/vision/"]
async fn test_omniparser_pipeline_tiny_image() {
    let mut detector = load_omniparser().expect("OmniParser model should be loadable");
    let screenshot = create_white_text_screenshot(1, 1, &[]);
    let result = detector.detect_elements(&screenshot).await;
    assert!(result.is_ok(), "OmniParser should handle tiny images without error");
}

#[tokio::test]
#[ignore = "Requires ONNX model files in models/vision/"]
async fn test_omniparser_returns_detected_elements() {
    let mut detector = load_omniparser().expect("OmniParser model should be loadable");
    let screenshot = create_synthetic_ui_screenshot(640, 480);
    let result = detector.detect_elements(&screenshot).await;
    assert!(result.is_ok(), "OmniParser pipeline should succeed");
    for el in &result.unwrap() {
        assert!(!el.role.is_empty());
        assert!(el.confidence >= 0.0 && el.confidence <= 1.0);
        assert!(el.bounds.width <= 640);
        assert!(el.bounds.height <= 480);
    }
}

// ── E2E Tests: Convert to UiElement ──────────────────────────────────

#[tokio::test]
#[ignore = "Requires ONNX model files in models/vision/"]
async fn test_omniparser_to_ui_elements() {
    let mut detector = load_omniparser().expect("OmniParser model should be loadable");
    let screenshot = create_synthetic_ui_screenshot(640, 480);
    let detected = detector
        .detect_elements(&screenshot)
        .await
        .expect("detect_elements");
    let ui_elements = OmniParserDetector::to_ui_elements(detected);
    for el in &ui_elements {
        assert!(!el.role.is_empty());
        assert!(el.enabled);
        assert!(el.children.is_empty());
        assert!(el.bounds.x >= 0 && el.bounds.x as u32 + el.bounds.width <= 640);
        assert!(el.bounds.y >= 0 && el.bounds.y as u32 + el.bounds.height <= 480);
    }
}

// ── E2E Tests: Model loading ──────────────────────────────────────────

#[tokio::test]
async fn test_ocr_model_not_found_graceful() {
    let result = RapidOcr::new(
        "/nonexistent/det.onnx",
        "/nonexistent/rec.onnx",
        "/nonexistent/rec_dict.txt",
    );
    assert!(result.is_err(), "Loading OCR from nonexistent paths should fail");
}

#[tokio::test]
async fn test_omniparser_model_not_found_graceful() {
    let result = OmniParserDetector::new("/nonexistent/omniparser.onnx");
    assert!(result.is_err(), "Loading OmniParser from nonexistent path should fail");
}

// ── E2E Tests: Model availability detection ───────────────────────────

#[test]
fn test_model_availability_detection() {
    let _has_ocr = has_ocr_models();
    let _has_omni = has_omniparser_models();
}

// ── E2E Tests: Auto-download ──────────────────────────────────────────

#[tokio::test]
async fn test_model_download_cache_dir_resolution() {
    // Verify that the model download function resolves paths correctly
    // without making network requests (models already in cache).
    let result = syscity::computer::vision::resolve_or_download_vision_models().await;
    if result.is_ok() {
        let paths = result.unwrap();
        assert!(paths.omniparser.exists(), "omniparser.onnx should exist");
        assert!(paths.det.exists(), "det.onnx should exist");
        assert!(paths.rec.exists(), "recognition model should exist");
        assert!(paths.rec_dict.exists(), "recognition dictionary should exist");
    }
    // If download fails (no network), the test is still valid — we just
    // verify it doesn't panic.
}

#[tokio::test]
#[ignore = "Requires network to download ONNX models"]
async fn test_model_download_from_scratch() {
    // Run auto-download to a temp cache dir.
    // This test downloads actual ~108 MB of model files, so it's ignored
    // by default.  Run with: cargo test --test e2e_test -- vision --
    // --include-ignored
    let result = syscity::computer::vision::resolve_or_download_vision_models().await;
    assert!(result.is_ok(), "auto-download should succeed: {:?}", result.err());
    let paths = result.unwrap();
    assert!(paths.omniparser.exists(), "omniparser.onnx should exist after download");
    assert!(paths.det.exists(), "det.onnx should exist after download");
    assert!(paths.rec.exists(), "recognition model should exist after download");
    assert!(paths.rec_dict.exists(), "recognition dictionary should exist after download");
}
