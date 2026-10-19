//! OmniParser integration — UI element detection from screenshots via ONNX.
//!
//! Uses Microsoft's OmniParser model (or a similar YOLO-based GUI parsing
//! model) exported to ONNX.  Detects interactive elements such as buttons,
//! text fields, checkboxes, icons, and images from a raw screenshot.
//!
//! This serves as a visual fallback when native accessibility APIs
//! (AXUIElement, UIAutomation, AT-SPI) return empty or incomplete results.
//!
//! Model file required:
//! - `omniparser.onnx` — element detection model
//!
//! Download from: https://hf-mirror.com/onnx-community/OmniParser-icon_detect_640x640

use super::{DetectedElement, Rect};
use crate::computer::types::Screenshot;
use crate::computer::vision::{
    decode_screenshot, image_to_nchw_tensor, normalize_image, resize_with_pad,
};
use crate::error::SyscityError;

/// OmniParser-based UI element detector.
#[derive(Debug)]
pub struct OmniParserDetector {
    model: ort::session::Session,
    icon_threshold: f32,
    text_threshold: f32,
    /// Input size expected by the model (default 640 for YOLO models).
    input_size: u32,
    /// IoU threshold for Non-Maximum Suppression.
    nms_iou_threshold: f32,
}

/// A detection box in center format (cx, cy, w, h) — the geometry OmniParser
/// emits and NMS compares.
#[derive(Clone, Copy)]
struct CenterBox {
    cx: f32,
    cy: f32,
    w: f32,
    h: f32,
}

impl CenterBox {
    /// Extract the box fields (indices 2..6) from a raw detection tuple.
    fn of(d: &(usize, f32, f32, f32, f32, f32)) -> Self {
        Self {
            cx: d.2,
            cy: d.3,
            w: d.4,
            h: d.5,
        }
    }
}

/// Role mapping from model class IDs to UiElement roles.
const CLASS_ROLES: [&str; 10] = [
    "button",
    "checkbox",
    "text_field",
    "icon",
    "image",
    "text",
    "menu",
    "scrollbar",
    "slider",
    "link",
];

impl OmniParserDetector {
    /// Create a detector from an ONNX model file.
    pub fn new(model_path: &str) -> crate::Result<Self> {
        let env = super::ort_environment()?;
        let model = ort::session::Session::builder(env)
            .map_err(|e| SyscityError::Internal(format!("ORT builder error: {}", e)))?
            .commit_from_file(model_path)
            .map_err(|e| {
                SyscityError::Internal(format!("Failed to load OmniParser model: {}", e))
            })?;

        Ok(Self {
            model,
            icon_threshold: 0.5,
            text_threshold: 0.3,
            input_size: 640,
            nms_iou_threshold: 0.5,
        })
    }

    /// Auto-download the OmniParser model and create a detector.
    ///
    /// Downloads from HuggingFace to `{cache_dir}/syscity/models/vision/`
    /// on first call; reuses cached file on subsequent calls.
    pub async fn new_auto() -> crate::Result<Self> {
        let paths = super::resolve_or_download_vision_models().await?;
        let path = paths.omniparser.to_str().ok_or_else(|| {
            SyscityError::Validation("omniparser model path is not valid UTF-8".into())
        })?;
        Self::new(path)
    }

    /// Set confidence thresholds.
    pub fn with_thresholds(mut self, icon: f32, text: f32) -> Self {
        self.icon_threshold = icon;
        self.text_threshold = text;
        self
    }

    /// Set input size (default 640).
    pub fn with_input_size(mut self, size: u32) -> Self {
        self.input_size = size;
        self
    }

    /// Set NMS IoU threshold (default 0.5).
    pub fn with_nms_threshold(mut self, iou: f32) -> Self {
        self.nms_iou_threshold = iou;
        self
    }

    /// Detect UI elements from a screenshot.
    pub async fn detect_elements(
        &mut self,
        screenshot: &Screenshot,
    ) -> crate::Result<Vec<DetectedElement>> {
        let img = decode_screenshot(screenshot)?;

        // Resize with padding to maintain aspect ratio
        let (padded, scale, pad_x, pad_y, orig_w) =
            resize_with_pad(&img, self.input_size, self.input_size);
        let orig_h = img.height();

        // Simple normalization: divide by 255 (standard for YOLO)
        let normalized = normalize_image(&padded, [0.0; 3], [1.0; 3]);
        let input_tensor =
            image_to_nchw_tensor(normalized, self.input_size as usize, self.input_size as usize)?;

        let input = ort::value::Tensor::from_array(input_tensor)
            .map_err(|e| SyscityError::Internal(format!("Tensor creation failed: {}", e)))?;

        let output = {
            let outputs = self
                .model
                .run(ort::inputs!["images" => input])
                .map_err(|e| {
                    SyscityError::Internal(format!("Detection inference failed: {}", e))
                })?;

            // YOLO outputs are typically named "output0" or similar.
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

        // YOLOv8-style output shape: [1, 84, 8400] or [1, 8400, 84]
        // where 84 = 4 (box) + 80 (classes) for COCO, or similar for custom models.
        // OmniParser uses 10 classes, so it might be [1, 14, N] or [1, N, 14].
        let shape = output.shape();

        // Parse detections from the output tensor
        let detections = self.parse_yolo_output(&output.view(), shape)?;

        // Apply Non-Maximum Suppression
        let nms_boxes = self.non_max_suppression(detections);

        // Scale boxes back to original image coordinates
        let elements: Vec<DetectedElement> = nms_boxes
            .into_iter()
            .map(|(class_id, conf, cx, cy, w, h)| {
                // Remove padding and scale back
                let cx_orig = ((cx - pad_x) / scale).clamp(0.0, orig_w);
                let cy_orig = ((cy - pad_y) / scale).clamp(0.0, orig_h as f32);
                let w_orig = (w / scale).clamp(0.0, orig_w);
                let h_orig = (h / scale).clamp(0.0, orig_h as f32);

                let x = (cx_orig - w_orig / 2.0) as i32;
                let y = (cy_orig - h_orig / 2.0) as i32;
                let width = w_orig as u32;
                let height = h_orig as u32;

                DetectedElement {
                    role: CLASS_ROLES
                        .get(class_id)
                        .copied()
                        .unwrap_or("unknown")
                        .to_string(),
                    label: None,
                    bounds: Rect::new(x, y, width, height),
                    confidence: conf,
                }
            })
            .collect();

        Ok(elements)
    }

    /// Parse YOLO-style output tensor into raw detections.
    ///
    /// Supports both [batch, attrs, num_anchors] and [batch, num_anchors,
    /// attrs] layouts.
    #[allow(clippy::type_complexity)]
    fn parse_yolo_output(
        &self,
        output: &ndarray::ArrayViewD<f32>,
        shape: &[usize],
    ) -> crate::Result<Vec<(usize, f32, f32, f32, f32, f32)>> {
        // Determine layout: (batch, features, anchors) vs (batch, anchors, features)
        // YOLO features dimension: 4 (bbox) + num_classes (typically 1-80).
        // The features dimension is always ≤ 85; anchors is typically 8400.
        if shape.len() != 3 {
            return Err(SyscityError::Internal(format!(
                "Unexpected YOLO output shape: {:?}",
                shape
            )));
        }

        let is_transposed = shape[1] <= 85; // small dim → features at dim 1
        let (num_anchors, num_features) = if is_transposed {
            // shape: [batch, features, anchors]
            (shape[2], shape[1])
        } else {
            // shape: [batch, anchors, features]
            (shape[1], shape[2])
        };

        let num_classes = num_features.saturating_sub(4); // 4 box coords + N classes

        let mut detections = Vec::new();

        for i in 0..num_anchors {
            // Extract box center, width, height
            let (cx, cy, w, h) = if is_transposed {
                (output[[0, 0, i]], output[[0, 1, i]], output[[0, 2, i]], output[[0, 3, i]])
            } else {
                (output[[0, i, 0]], output[[0, i, 1]], output[[0, i, 2]], output[[0, i, 3]])
            };

            // Skip invalid boxes
            if w <= 0.0 || h <= 0.0 || cx < 0.0 || cy < 0.0 {
                continue;
            }

            // Find best class
            let mut best_class = 0usize;
            let mut best_score = f32::NEG_INFINITY;

            for c in 0..num_classes {
                let score = if is_transposed {
                    output[[0, 4 + c, i]]
                } else {
                    output[[0, i, 4 + c]]
                };

                if score > best_score {
                    best_score = score;
                    best_class = c;
                }
            }

            // Apply class-specific threshold
            let threshold = if best_class == 3 || best_class == 4 {
                // icon or image
                self.icon_threshold
            } else if best_class == 5 {
                // text
                self.text_threshold
            } else {
                self.icon_threshold
            };

            if best_score >= threshold {
                detections.push((best_class, best_score, cx, cy, w, h));
            }
        }

        Ok(detections)
    }

    /// Apply Non-Maximum Suppression to remove overlapping boxes.
    fn non_max_suppression(
        &self,
        mut detections: Vec<(usize, f32, f32, f32, f32, f32)>,
    ) -> Vec<(usize, f32, f32, f32, f32, f32)> {
        // Sort by confidence descending
        detections.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));

        let mut kept = Vec::new();
        let mut suppressed = vec![false; detections.len()];

        for i in 0..detections.len() {
            if suppressed[i] {
                continue;
            }

            kept.push(detections[i]);

            for j in (i + 1)..detections.len() {
                if suppressed[j] {
                    continue;
                }

                // Only suppress boxes of the same class
                if detections[i].0 != detections[j].0 {
                    continue;
                }

                let iou =
                    Self::compute_iou(CenterBox::of(&detections[i]), CenterBox::of(&detections[j]));

                if iou >= self.nms_iou_threshold {
                    suppressed[j] = true;
                }
            }
        }

        kept
    }

    /// Compute Intersection over Union for two center-format boxes.
    fn compute_iou(a: CenterBox, b: CenterBox) -> f32 {
        let x1_min = a.cx - a.w / 2.0;
        let y1_min = a.cy - a.h / 2.0;
        let x1_max = a.cx + a.w / 2.0;
        let y1_max = a.cy + a.h / 2.0;

        let x2_min = b.cx - b.w / 2.0;
        let y2_min = b.cy - b.h / 2.0;
        let x2_max = b.cx + b.w / 2.0;
        let y2_max = b.cy + b.h / 2.0;

        let inter_x1 = x1_min.max(x2_min);
        let inter_y1 = y1_min.max(y2_min);
        let inter_x2 = x1_max.min(x2_max);
        let inter_y2 = y1_max.min(y2_max);

        let inter_w = (inter_x2 - inter_x1).max(0.0);
        let inter_h = (inter_y2 - inter_y1).max(0.0);
        let inter_area = inter_w * inter_h;

        let area1 = a.w * a.h;
        let area2 = b.w * b.h;
        let union_area = area1 + area2 - inter_area;

        if union_area <= 0.0 {
            0.0
        } else {
            inter_area / union_area
        }
    }

    /// Convert DetectedElement list to the canonical UiElement format.
    pub fn to_ui_elements(
        detected: Vec<DetectedElement>,
    ) -> Vec<crate::computer::types::UiElement> {
        detected
            .into_iter()
            .map(|d| crate::computer::types::UiElement {
                id: String::new(),
                role: d.role,
                label: d.label,
                value: None,
                bounds: d.bounds,
                enabled: true,
                focused: false,
                children: vec![],
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_role_mapping() {
        assert_eq!(CLASS_ROLES[0], "button");
        assert_eq!(CLASS_ROLES[3], "icon");
    }

    #[test]
    fn test_compute_iou() {
        fn box_at(cx: f32, cy: f32) -> CenterBox {
            CenterBox { cx, cy, w: 20.0, h: 20.0 }
        }

        // Identical boxes → IoU = 1.0
        let iou = OmniParserDetector::compute_iou(box_at(50.0, 50.0), box_at(50.0, 50.0));
        assert!((iou - 1.0).abs() < 0.001);

        // Non-overlapping (10-wide boxes) → IoU = 0.0
        let a = CenterBox {
            cx: 10.0,
            cy: 10.0,
            w: 10.0,
            h: 10.0,
        };
        let b = CenterBox {
            cx: 50.0,
            cy: 50.0,
            w: 10.0,
            h: 10.0,
        };
        assert_eq!(OmniParserDetector::compute_iou(a, b), 0.0);

        // Partial overlap
        let iou = OmniParserDetector::compute_iou(box_at(10.0, 10.0), box_at(15.0, 15.0));
        assert!(iou > 0.0 && iou < 1.0);
    }

    #[test]
    fn test_nms_logic() {
        // Test NMS without constructing a real detector
        let detections = vec![
            (0, 0.9, 50.0, 50.0, 20.0, 20.0),   // High confidence
            (0, 0.8, 51.0, 51.0, 20.0, 20.0),   // Same class, high overlap → should be suppressed
            (1, 0.7, 100.0, 100.0, 30.0, 30.0), // Different class
        ];

        let iou_threshold = 0.5f32;
        let mut kept = Vec::new();
        let mut suppressed = vec![false; detections.len()];

        for i in 0..detections.len() {
            if suppressed[i] {
                continue;
            }
            kept.push(detections[i]);
            for j in (i + 1)..detections.len() {
                if suppressed[j] {
                    continue;
                }
                if detections[i].0 != detections[j].0 {
                    continue;
                }
                let iou = OmniParserDetector::compute_iou(
                    CenterBox::of(&detections[i]),
                    CenterBox::of(&detections[j]),
                );
                if iou >= iou_threshold {
                    suppressed[j] = true;
                }
            }
        }

        assert_eq!(kept.len(), 2);
        assert_eq!(kept[0].0, 0);
        assert_eq!(kept[1].0, 1);
    }
}
