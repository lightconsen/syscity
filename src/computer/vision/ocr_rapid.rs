//! RapidOCR integration — local text detection + recognition via ONNX.
//!
//! RapidOCR is a lightweight OCR pipeline (DBNet text detection + CRNN
//! recognition) exported to ONNX.  It runs entirely locally with no
//! external service dependencies.
//!
//! Model files required:
//! - `det.onnx` — text detection (DBNet)
//! - `rec.onnx` — text recognition (CRNN)
//! - `cls.onnx` — optional direction classification
//!
//! These can be downloaded from the RapidOCR releases or HuggingFace hub.

use super::{Rect, TextBlock};
use crate::computer::types::Screenshot;
use crate::computer::vision::{decode_screenshot, image_to_nchw_tensor, normalize_image};
use crate::error::SyscityError;

/// RapidOCR engine using ONNX Runtime.
///
/// Loads three ONNX models and runs a full OCR pipeline:
/// 1. Detection — find text regions in the image
/// 2. (Optional) Classification — correct text orientation
/// 3. Recognition — read the characters in each region
#[derive(Debug)]
pub struct RapidOcr {
    det_model: ort::session::Session,
    rec_model: ort::session::Session,
    #[allow(dead_code)]
    cls_model: Option<ort::session::Session>,
    /// Detection threshold for the DBNet probability map.
    det_threshold: f32,
    /// Box expansion ratio (dilate detected boxes by this factor).
    box_expand: f32,
    /// Recognition model input height (CRNN typically uses 48).
    rec_height: u32,
    /// Maximum recognition width.
    rec_max_width: u32,
    /// Minimum confidence for a text block to be included.
    min_confidence: f32,
}

impl RapidOcr {
    /// Create a new RapidOcr engine from ONNX model paths.
    ///
    /// # Arguments
    /// * `det_path` — path to the detection ONNX model
    /// * `rec_path` — path to the recognition ONNX model
    /// * `cls_path` — optional path to the classification ONNX model
    pub fn new(det_path: &str, rec_path: &str, cls_path: Option<&str>) -> crate::Result<Self> {
        let env = super::ort_environment()?;
        let det_model = ort::session::Session::builder(env)
            .map_err(|e| SyscityError::Internal(format!("ORT builder error: {}", e)))?
            .commit_from_file(det_path)
            .map_err(|e| {
                SyscityError::Internal(format!("Failed to load detection model: {}", e))
            })?;

        let rec_model = ort::session::Session::builder(env)
            .map_err(|e| SyscityError::Internal(format!("ORT builder error: {}", e)))?
            .commit_from_file(rec_path)
            .map_err(|e| {
                SyscityError::Internal(format!("Failed to load recognition model: {}", e))
            })?;

        let cls_model = match cls_path {
            Some(p) => Some(
                ort::session::Session::builder(env)
                    .map_err(|e| SyscityError::Internal(format!("ORT builder error: {}", e)))?
                    .commit_from_file(p)
                    .map_err(|e| {
                        SyscityError::Internal(format!("Failed to load cls model: {}", e))
                    })?,
            ),
            None => None,
        };

        Ok(Self {
            det_model,
            rec_model,
            cls_model,
            det_threshold: 0.3,
            box_expand: 1.5,
            rec_height: 48,
            rec_max_width: 320,
            min_confidence: 0.5,
        })
    }

    /// Auto-download all RapidOCR models and create an engine.
    ///
    /// Downloads from HuggingFace to `{cache_dir}/syscity/models/vision/`
    /// on first call; reuses cached files on subsequent calls.
    pub async fn new_auto() -> crate::Result<Self> {
        let paths = super::resolve_or_download_vision_models().await?;
        let det = paths
            .det
            .to_str()
            .ok_or_else(|| SyscityError::Validation("det model path is not valid UTF-8".into()))?;
        let rec = paths
            .rec
            .to_str()
            .ok_or_else(|| SyscityError::Validation("rec model path is not valid UTF-8".into()))?;
        Self::new(det, rec, paths.cls.as_ref().and_then(|p| p.to_str()))
    }

    /// Set detection threshold (default 0.3).
    pub fn with_det_threshold(mut self, threshold: f32) -> Self {
        self.det_threshold = threshold;
        self
    }

    /// Set box expansion ratio (default 1.5).
    pub fn with_box_expand(mut self, ratio: f32) -> Self {
        self.box_expand = ratio;
        self
    }

    /// Set minimum confidence for text blocks (default 0.5).
    pub fn with_min_confidence(mut self, min: f32) -> Self {
        self.min_confidence = min;
        self
    }

    /// Detect all text blocks in a screenshot.
    pub async fn detect_text(&mut self, screenshot: &Screenshot) -> crate::Result<Vec<TextBlock>> {
        let img = decode_screenshot(screenshot)?;

        // 1. Run detection to find text regions
        let regions = self.detect_regions(&img).await?;
        if regions.is_empty() {
            return Ok(vec![]);
        }

        // 2. Recognize text in each region
        let mut blocks = Vec::new();
        for (bounds, det_score) in regions {
            let cropped =
                img.crop_imm(bounds.x as u32, bounds.y as u32, bounds.width, bounds.height);

            let (text, rec_score) = self.recognize_text(&cropped).await?;

            let confidence = det_score * rec_score;
            if confidence >= self.min_confidence {
                blocks.push(TextBlock { text, confidence, bounds });
            }
        }

        // Sort by vertical position (top-to-bottom reading order)
        blocks.sort_by_key(|a| a.bounds.y);

        Ok(blocks)
    }

    /// Run the DBNet detection model to find text region bounding boxes.
    async fn detect_regions(
        &mut self,
        img: &image::DynamicImage,
    ) -> crate::Result<Vec<(Rect, f32)>> {
        let (orig_w, orig_h) = (img.width(), img.height());

        // DBNet expects input dimensions to be multiples of 32
        let target_w = ((orig_w as f32 / 32.0).ceil() * 32.0) as u32;
        let target_h = ((orig_h as f32 / 32.0).ceil() * 32.0) as u32;

        let resized = img.resize_exact(target_w, target_h, image::imageops::FilterType::Triangle);

        // ImageNet normalization (standard for DBNet)
        let normalized = normalize_image(&resized, [0.485, 0.456, 0.406], [0.229, 0.224, 0.225]);

        let input_tensor = image_to_nchw_tensor(normalized, target_w as usize, target_h as usize)?;
        let input = ort::value::Tensor::from_array(input_tensor)
            .map_err(|e| SyscityError::Internal(format!("Tensor creation failed: {}", e)))?;

        let prob_map = {
            let outputs = self
                .det_model
                .run(ort::inputs!["x" => input])
                .map_err(|e| {
                    SyscityError::Internal(format!("Detection inference failed: {}", e))
                })?;

            // DBNet outputs a probability map. The exact output name depends on the ONNX
            // export; common names are "output" or "sigmoid_0.tmp_0".
            let output_name = outputs
                .iter()
                .next()
                .map(|(name, _)| name.to_string())
                .ok_or_else(|| {
                    SyscityError::Internal("Detection model produced no outputs".to_string())
                })?;

            outputs[output_name.as_str()]
                .try_extract_array::<f32>()
                .map_err(|e| SyscityError::Internal(format!("Tensor extraction failed: {}", e)))?
                .to_owned()
        };

        // prob_map shape is typically [1, 1, H/4, W/4] or [1, H/4, W/4]
        let shape = prob_map.shape();
        let (map_h, map_w) = if shape.len() == 4 {
            (shape[2], shape[3])
        } else if shape.len() == 3 {
            (shape[1], shape[2])
        } else {
            return Err(SyscityError::Internal(format!(
                "Unexpected detection output shape: {:?}",
                shape
            )));
        };

        // Threshold probability map and find bounding boxes
        let scale_x = orig_w as f32 / map_w as f32;
        let scale_y = orig_h as f32 / map_h as f32;

        let mut boxes = Vec::new();
        let mut visited = vec![vec![false; map_w]; map_h];

        for y in 0..map_h {
            for x in 0..map_w {
                let prob = if shape.len() == 4 {
                    prob_map[[0, 0, y, x]]
                } else {
                    prob_map[[0, y, x]]
                };

                if prob < self.det_threshold || visited[y][x] {
                    continue;
                }

                // Flood-fill to find connected component
                let (min_x, min_y, max_x, max_y, sum_prob, count) =
                    self.flood_fill(&prob_map.view(), &mut visited, x, y);

                if count < 10 {
                    // Too small, likely noise
                    continue;
                }

                let avg_prob = sum_prob / count as f32;

                // Scale back to original image coordinates
                let x1 = (min_x as f32 * scale_x) as i32;
                let y1 = (min_y as f32 * scale_y) as i32;
                let x2 = ((max_x + 1) as f32 * scale_x) as i32;
                let y2 = ((max_y + 1) as f32 * scale_y) as i32;

                let cx = (x1 + x2) / 2;
                let cy = (y1 + y2) / 2;
                let w = ((x2 - x1) as f32 * self.box_expand) as u32;
                let h = ((y2 - y1) as f32 * self.box_expand) as u32;

                let bounds = Rect::new(
                    (cx - w as i32 / 2).max(0),
                    (cy - h as i32 / 2).max(0),
                    w.min(orig_w),
                    h.min(orig_h),
                );

                boxes.push((bounds, avg_prob));
            }
        }

        Ok(boxes)
    }

    /// Flood-fill on the probability map to find a connected component.
    /// The map's own shape supplies the dimensionality and extents.
    fn flood_fill(
        &self,
        prob_map: &ndarray::ArrayViewD<f32>,
        visited: &mut [Vec<bool>],
        start_x: usize,
        start_y: usize,
    ) -> (usize, usize, usize, usize, f32, usize) {
        let ndim = prob_map.ndim();
        let (map_h, map_w) = if ndim == 4 {
            (prob_map.shape()[2], prob_map.shape()[3])
        } else {
            (prob_map.shape()[1], prob_map.shape()[2])
        };
        let mut stack = vec![(start_x, start_y)];
        let mut min_x = start_x;
        let mut min_y = start_y;
        let mut max_x = start_x;
        let mut max_y = start_y;
        let mut sum_prob = 0.0;
        let mut count = 0;

        while let Some((x, y)) = stack.pop() {
            if visited[y][x] {
                continue;
            }
            visited[y][x] = true;

            let prob = if ndim == 4 {
                prob_map[[0, 0, y, x]]
            } else {
                prob_map[[0, y, x]]
            };

            sum_prob += prob;
            count += 1;

            min_x = min_x.min(x);
            min_y = min_y.min(y);
            max_x = max_x.max(x);
            max_y = max_y.max(y);

            // 4-connected neighbors
            for (dx, dy) in [(-1, 0), (1, 0), (0, -1), (0, 1)] {
                let nx = x as i32 + dx;
                let ny = y as i32 + dy;
                if nx >= 0
                    && nx < map_w as i32
                    && ny >= 0
                    && ny < map_h as i32
                    && !visited[ny as usize][nx as usize]
                {
                    let nprob = if ndim == 4 {
                        prob_map[[0, 0, ny as usize, nx as usize]]
                    } else {
                        prob_map[[0, ny as usize, nx as usize]]
                    };
                    if nprob >= self.det_threshold {
                        stack.push((nx as usize, ny as usize));
                    }
                }
            }
        }

        (min_x, min_y, max_x, max_y, sum_prob, count)
    }

    /// Run the CRNN recognition model on a cropped text region.
    async fn recognize_text(&mut self, crop: &image::DynamicImage) -> crate::Result<(String, f32)> {
        let (crop_w, crop_h) = (crop.width(), crop.height());

        // Maintain aspect ratio, resize to fixed height
        let scale = self.rec_height as f32 / crop_h as f32;
        let new_w = ((crop_w as f32 * scale) as u32).clamp(1, self.rec_max_width);
        let new_h = self.rec_height;

        let resized = crop.resize_exact(new_w, new_h, image::imageops::FilterType::Triangle);

        // CRNN typically uses grayscale or RGB with simple normalization
        let normalized = normalize_image(&resized, [0.5; 3], [0.5; 3]);
        let input_tensor = image_to_nchw_tensor(normalized, new_w as usize, new_h as usize)?;

        let input = ort::value::Tensor::from_array(input_tensor)
            .map_err(|e| SyscityError::Internal(format!("Tensor creation failed: {}", e)))?;

        let outputs = self
            .rec_model
            .run(ort::inputs!["x" => input])
            .map_err(|e| SyscityError::Internal(format!("Recognition inference failed: {}", e)))?;

        let output_name = outputs
            .iter()
            .next()
            .map(|(name, _)| name.to_string())
            .ok_or_else(|| {
                SyscityError::Internal("Recognition model produced no outputs".to_string())
            })?;

        let output = outputs[output_name.as_str()]
            .try_extract_array::<f32>()
            .map_err(|e| SyscityError::Internal(format!("Tensor extraction failed: {}", e)))?;

        // CRNN output shape: [T, 1, num_classes] or [1, T, num_classes]
        let shape = output.shape();
        let (seq_len, num_classes) = if shape.len() == 3 && shape[1] == 1 {
            (shape[0], shape[2])
        } else if shape.len() == 3 {
            (shape[1], shape[2])
        } else {
            return Err(SyscityError::Internal(format!(
                "Unexpected recognition output shape: {:?}",
                shape
            )));
        };

        // Greedy CTC decoding
        let mut prev_class = 0usize;
        let mut text_chars = Vec::new();
        let mut total_confidence = 0.0;
        let mut valid_steps = 0;

        for t in 0..seq_len {
            let mut max_prob = f32::NEG_INFINITY;
            let mut best_class = 0usize;

            for c in 0..num_classes {
                let prob = if shape.len() == 3 && shape[1] == 1 {
                    output[[t, 0, c]]
                } else {
                    output[[0, t, c]]
                };

                if prob > max_prob {
                    max_prob = prob;
                    best_class = c;
                }
            }

            if best_class != 0 && best_class != prev_class {
                // 0 is typically the CTC blank token
                text_chars.push(best_class);
                total_confidence += max_prob.exp();
                valid_steps += 1;
            }
            prev_class = best_class;
        }

        // Map class indices to characters using a basic ASCII alphabet
        // In a real deployment, this should come from the model's dictionary file.
        let text = text_chars
            .iter()
            .filter_map(|&idx| Self::class_to_char(idx))
            .collect::<String>();

        let confidence = if valid_steps > 0 {
            (total_confidence / valid_steps as f32).min(1.0)
        } else {
            0.0
        };

        Ok((text, confidence))
    }

    /// Map a CRNN class index to a character.
    ///
    /// This uses a basic alphanumeric + punctuation alphabet.
    /// For production use, load the exact dictionary file that matches the
    /// model.
    fn class_to_char(idx: usize) -> Option<char> {
        // Standard RapidOCR/PP-OCR dictionary mapping:
        // 0 = blank (CTC)
        // 1-10 = '0'-'9'
        // 11-36 = 'a'-'z'
        // 37-62 = 'A'-'Z'
        // 63+ = punctuation and special chars
        match idx {
            1..=10 => Some((b'0' + (idx - 1) as u8) as char),
            11..=36 => Some((b'a' + (idx - 11) as u8) as char),
            37..=62 => Some((b'A' + (idx - 37) as u8) as char),
            63 => Some(' '),
            64 => Some('.'),
            65 => Some(','),
            66 => Some('!'),
            67 => Some('?'),
            68 => Some('-'),
            69 => Some('_'),
            70 => Some(':'),
            71 => Some(';'),
            72 => Some('\''),
            73 => Some('"'),
            74 => Some('('),
            75 => Some(')'),
            76 => Some('['),
            77 => Some(']'),
            78 => Some('{'),
            79 => Some('}'),
            80 => Some('/'),
            81 => Some('\\'),
            82 => Some('@'),
            83 => Some('#'),
            84 => Some('$'),
            85 => Some('%'),
            86 => Some('&'),
            87 => Some('*'),
            88 => Some('+'),
            89 => Some('='),
            90 => Some('<'),
            91 => Some('>'),
            92 => Some('~'),
            93 => Some('`'),
            94 => Some('^'),
            95 => Some('|'),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_class_to_char() {
        assert_eq!(RapidOcr::class_to_char(1), Some('0'));
        assert_eq!(RapidOcr::class_to_char(10), Some('9'));
        assert_eq!(RapidOcr::class_to_char(11), Some('a'));
        assert_eq!(RapidOcr::class_to_char(36), Some('z'));
        assert_eq!(RapidOcr::class_to_char(37), Some('A'));
        assert_eq!(RapidOcr::class_to_char(62), Some('Z'));
        assert_eq!(RapidOcr::class_to_char(63), Some(' '));
    }

    #[test]
    fn test_rapid_ocr_creation_stub() {
        // Stub test until models are available in CI
        assert!(true);
    }
}
