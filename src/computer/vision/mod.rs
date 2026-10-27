//! Visual perception layer — OCR and UI element detection from screenshots.
//!
//! This module provides computer-vision-based fallbacks when native
//! accessibility APIs return empty or incomplete results (games, image-based
//! UIs, remote desktops, etc.).
//!
//! Features:
//! - **RapidOCR** (`ocr_rapid`): ONNX-based text detection + recognition
//! - **OmniParser** (`ui_onnx`): ONNX-based UI element detection (buttons,
//!   inputs, checkboxes, icons, etc.)
//!
//! Both are gated behind the `vision` Cargo feature.

use crate::computer::types::Rect;

/// A block of detected text with its location and confidence.
#[derive(Debug, Clone, PartialEq)]
pub struct TextBlock {
    pub text: String,
    pub confidence: f32,
    pub bounds: Rect,
}

/// A UI element detected visually (as opposed to via accessibility API).
#[derive(Debug, Clone, PartialEq)]
pub struct DetectedElement {
    pub role: String, // "button", "text_field", "checkbox", "icon", etc.
    pub label: Option<String>,
    pub bounds: Rect,
    pub confidence: f32,
}

#[cfg(feature = "vision")]
pub mod model_download;
#[cfg(feature = "vision")]
pub mod ocr_file;
#[cfg(feature = "vision")]
pub mod ocr_rapid;
pub mod screen_state;
#[cfg(feature = "vision")]
pub mod ui_onnx;

#[cfg(feature = "vision")]
mod preprocess;

/// The process-wide ONNX Runtime environment.
///
/// ort 2.0.0-rc.14 made the environment an explicit argument to
/// `Session::builder`, and ONNX Runtime allows only one environment per
/// process, so both vision backends share this one.
#[cfg(feature = "vision")]
pub(crate) fn ort_environment() -> crate::Result<&'static ort::environment::Environment> {
    use std::sync::OnceLock;

    use crate::error::SyscityError;

    static ENV: OnceLock<ort::environment::Environment> = OnceLock::new();

    if ENV.get().is_none() {
        let env = ort::init()
            .build()
            .map_err(|e| SyscityError::Internal(format!("ORT environment error: {}", e)))?;
        // Err means another thread initialized the environment first; that
        // one is the process-wide environment either way.
        let _ = ENV.set(env);
    }

    ENV.get()
        .ok_or_else(|| SyscityError::Internal("ORT environment not initialized".to_string()))
}

#[cfg(feature = "vision")]
pub use model_download::{resolve_or_download_vision_models, VisionModelPaths};
#[cfg(feature = "vision")]
pub use preprocess::{decode_screenshot, image_to_nchw_tensor, normalize_image, resize_with_pad};
pub use screen_state::{
    is_screen_mutating_action, pixel_diff, tree_diff, PixelDiffResult, ScreenDiff, ScreenState,
    TextBlockSer, TreeDiffResult,
};
