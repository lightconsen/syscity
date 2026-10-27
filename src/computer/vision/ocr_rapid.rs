//! RapidOCR integration — local text detection + recognition via ONNX.
//!
//! RapidOCR is a lightweight OCR pipeline (DBNet text detection + CRNN
//! recognition) exported to ONNX.  It runs entirely locally with no
//! external service dependencies.
//!
//! Model files required:
//! - `det.onnx` — text detection (DBNet)
//! - `rec_ppocrv3.onnx` — text recognition (SVTR/CRNN), PP-OCRv3 english
//! - `rec_dict_ppocrv3.txt` — that model's character dictionary
//!
//! These can be downloaded from the RapidOCR releases or HuggingFace hub;
//! [`super::resolve_or_download_vision_models`] fetches them on first use.

use super::{Rect, TextBlock};
use crate::computer::types::Screenshot;
use crate::computer::vision::{decode_screenshot, image_to_nchw_tensor, normalize_image};
use crate::error::SyscityError;

/// RapidOCR engine using ONNX Runtime.
///
/// Loads two ONNX models and runs the OCR pipeline:
/// 1. Detection — find text regions in the image
/// 2. Recognition — read the characters in each region
#[derive(Debug)]
pub struct RapidOcr {
    det_model: ort::session::Session,
    rec_model: ort::session::Session,
    /// Character dictionary: output class `i` decodes to `dict[i - 1]`.
    dict: Vec<char>,
    /// Detection threshold for the DBNet probability map.
    det_threshold: f32,
    /// Box expansion ratio (dilate detected boxes by this factor).
    box_expand: f32,
    /// Recognition model input height (CRNN typically uses 48).
    rec_height: u32,
    /// Maximum recognition width. The model accepts a dynamic width, so this
    /// only bounds the cost of a pathologically wide line.
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
    /// * `dict_path` — path to the recognition model's character dictionary
    ///
    /// The dictionary is required: the recognition model emits class indices,
    /// and decoding them with a guessed alphabet produces confident garbage.
    pub fn new(det_path: &str, rec_path: &str, dict_path: &str) -> crate::Result<Self> {
        let dict = Self::load_dict(dict_path)?;
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

        Ok(Self {
            det_model,
            rec_model,
            dict,
            det_threshold: 0.3,
            box_expand: 1.5,
            rec_height: 48,
            rec_max_width: 4096,
            // Block confidence is detection × recognition, and a clean line
            // often scores ~0.6–0.7 on the recognition side alone; 0.5 threw
            // away readable text.
            min_confidence: 0.3,
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
        let dict = paths
            .rec_dict
            .to_str()
            .ok_or_else(|| SyscityError::Validation("dict path is not valid UTF-8".into()))?;
        Self::new(det, rec, dict)
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

    /// Set minimum confidence for text blocks (default 0.3).
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

                // Scale back to original image coordinates. DBNet predicts a
                // region smaller than the glyphs, so unclip it — by
                // `ratio * area / perimeter` on every side, the standard
                // expansion. Scaling width and height by a factor instead
                // stretches a long line sideways while leaving it too short
                // vertically, and the recognition crop then cuts the glyphs.
                let x1 = min_x as f32 * scale_x;
                let y1 = min_y as f32 * scale_y;
                let x2 = (max_x + 1) as f32 * scale_x;
                let y2 = (max_y + 1) as f32 * scale_y;
                let (w0, h0) = ((x2 - x1).max(1.0), (y2 - y1).max(1.0));
                let pad = self.box_expand * (w0 * h0) / (2.0 * (w0 + h0));

                let left = (x1 - pad).clamp(0.0, orig_w as f32);
                let top = (y1 - pad).clamp(0.0, orig_h as f32);
                let right = (x2 + pad).clamp(left + 1.0, orig_w as f32);
                let bottom = (y2 + pad).clamp(top + 1.0, orig_h as f32);

                let bounds = Rect::new(
                    left as i32,
                    top as i32,
                    (right - left) as u32,
                    (bottom - top) as u32,
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
        let content_w = ((crop_w as f32 * scale) as u32).clamp(1, self.rec_max_width);
        let new_h = self.rec_height;
        // Pad the width up to a multiple of 32: ONNX Runtime's handling of a
        // free dynamic width depends on the build (the same tensor reads
        // correctly under one onnxruntime and loses characters under
        // another), while a stride-aligned width is handled identically
        // everywhere. Padding is black, matching what PaddleOCR pads with.
        let new_w = content_w.div_ceil(32) * 32;

        let resized = crop.resize_exact(content_w, new_h, image::imageops::FilterType::Triangle);
        let resized = if new_w > content_w {
            let mut canvas = image::RgbImage::new(new_w, new_h);
            image::imageops::replace(&mut canvas, &resized.to_rgb8(), 0, 0);
            image::DynamicImage::ImageRgb8(canvas)
        } else {
            resized
        };

        // Plain [0, 1] scaling: these PP-OCR recognition exports are trained
        // without mean/std normalization (feeding [-1, 1] instead degrades
        // recognition to fragments of the right letters).
        let normalized = normalize_image(&resized, [0.0; 3], [1.0; 3]);
        let input_tensor = image_to_nchw_tensor(normalized, new_w as usize, new_h as usize)?;

        let input = ort::value::Tensor::from_array(input_tensor)
            .map_err(|e| SyscityError::Internal(format!("Tensor creation failed: {}", e)))?;

        // Greedy CTC decoding. The scoped block releases the borrow on
        // `self.rec_model` before the dictionary lookup below needs `self`.
        let dict_len = self.dict.len();
        let (text_chars, total_confidence, valid_steps) = {
            let outputs = self
                .rec_model
                .run(ort::inputs!["x" => input])
                .map_err(|e| {
                    SyscityError::Internal(format!("Recognition inference failed: {}", e))
                })?;

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

            // A model emits one class per dictionary entry, plus the CTC
            // blank, plus the appended space when it was trained with
            // `use_space_char`. Any other count means this dictionary belongs
            // to a different model — and decoding with it would return
            // fragments of the right characters rather than nothing.
            if num_classes != dict_len + 1 && num_classes != dict_len + 2 {
                return Err(SyscityError::Internal(format!(
                    "OCR dictionary has {} entries but the recognition model emits {} classes",
                    dict_len, num_classes
                )));
            }

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
                    // Class 0 is the CTC blank; the output is already softmaxed.
                    text_chars.push(best_class);
                    total_confidence += max_prob;
                    valid_steps += 1;
                }
                prev_class = best_class;
            }

            (text_chars, total_confidence, valid_steps)
        };

        // Map class indices through the model's dictionary.
        let text = text_chars
            .iter()
            .filter_map(|&idx| self.class_to_char(idx))
            .collect::<String>();

        let confidence = if valid_steps > 0 {
            (total_confidence / valid_steps as f32).min(1.0)
        } else {
            0.0
        };

        Ok((text, confidence))
    }

    /// Read the dictionary file the recognition model was trained with.
    fn load_dict(path: &str) -> crate::Result<Vec<char>> {
        let raw = std::fs::read_to_string(path).map_err(|e| {
            SyscityError::Internal(format!("Failed to read OCR dictionary {}: {}", path, e))
        })?;
        let dict = Self::parse_dict(&raw);
        if dict.is_empty() {
            return Err(SyscityError::Internal(format!("OCR dictionary {} is empty", path)));
        }
        Ok(dict)
    }

    /// Parse a PaddleOCR character dictionary: one entry per line, in the
    /// model's output-class order.
    ///
    /// Class index `i` (1-based) decodes to entry `i - 1`; class 0 is the CTC
    /// blank and has no entry. Only the first character of a line is used —
    /// the english dictionary is one character per line, and a multi-character
    /// entry would mean a word-level model this pipeline does not support.
    /// A genuinely empty line keeps its slot (as a space) so every later
    /// index stays aligned with the model's classes.
    fn parse_dict(raw: &str) -> Vec<char> {
        raw.lines()
            .map(|line| line.chars().next().unwrap_or(' '))
            .collect()
    }

    /// Map a CRNN class index to a character via the loaded dictionary.
    fn class_to_char(&self, idx: usize) -> Option<char> {
        decode_class(&self.dict, idx)
    }
}

/// Decode a recognition class index against a dictionary.
///
/// Class 0 is the CTC blank. Class `i` (1-based) is dictionary entry `i - 1`.
/// PaddleOCR trains recognition models with `use_space_char`, which appends a
/// space after the dictionary, so the one class past the last entry is `' '`;
/// anything beyond that is not a real class.
fn decode_class(dict: &[char], idx: usize) -> Option<char> {
    match idx.checked_sub(1) {
        None => None,
        Some(i) if i < dict.len() => dict.get(i).copied(),
        Some(i) if i == dict.len() => Some(' '),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_dict_keeps_class_order() {
        // The english PP-OCR dictionary starts with the digits, then upper
        // case, then lower case — not the other way round.
        let dict = RapidOcr::parse_dict("0\n1\n9\nA\nZ\na\nz\n/\n");
        assert_eq!(dict, vec!['0', '1', '9', 'A', 'Z', 'a', 'z', '/']);
    }

    #[test]
    fn decode_class_maps_blank_dict_and_appended_space() {
        let dict = RapidOcr::parse_dict("0\n1\nA\na\n");
        assert_eq!(decode_class(&dict, 0), None, "class 0 is the CTC blank");
        assert_eq!(decode_class(&dict, 1), Some('0'));
        assert_eq!(decode_class(&dict, 3), Some('A'));
        assert_eq!(decode_class(&dict, 4), Some('a'));
        assert_eq!(decode_class(&dict, 5), Some(' '), "use_space_char appends one");
        assert_eq!(decode_class(&dict, 6), None);
    }

    #[test]
    fn parse_dict_preserves_index_alignment() {
        // A blank line still occupies its class slot.
        let dict = RapidOcr::parse_dict("0\n\nA\n");
        assert_eq!(dict.len(), 3);
        assert_eq!(dict[1], ' ');
        assert_eq!(dict[2], 'A');
    }

    #[test]
    fn test_rapid_ocr_creation_stub() {
        // Stub test until models are available in CI
        assert!(true);
    }
}
