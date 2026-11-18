//! Shared image preprocessing utilities for vision models.

use base64::Engine;

use crate::computer::types::Screenshot;
use crate::error::SyscityError;

/// Decode a base64-encoded PNG screenshot into a `DynamicImage`.
pub fn decode_screenshot(screenshot: &Screenshot) -> crate::Result<image::DynamicImage> {
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(&screenshot.base64)
        .map_err(|e| SyscityError::Internal(format!("Base64 decode failed: {}", e)))?;

    image::load_from_memory(&bytes)
        .map_err(|e| SyscityError::Internal(format!("Image decode failed: {}", e)))
}

/// Crop a rectangle out of a decoded image, clamped to what the image holds.
///
/// A field's published bounds can run past the edge of a capture — a window
/// wider than the display, a control mid-scroll — and cropping beyond the
/// image is an error in `image`, not a short answer. Clamping keeps the useful
/// part; an intersection that is empty returns `None`, because a crop of
/// nothing cannot be read and the caller must not mistake a black rectangle
/// for an empty field.
#[cfg(feature = "vision")]
pub fn crop_clamped(
    img: &image::DynamicImage,
    rect: crate::computer::types::Rect,
) -> Option<image::DynamicImage> {
    let (width, height) = (img.width(), img.height());
    let x0 = rect.x.max(0).min(width as i32) as u32;
    let y0 = rect.y.max(0).min(height as i32) as u32;
    // The far corner in the same clamped space, so a rectangle with a negative
    // origin loses the part that is off the edge instead of gaining a second
    // width from the saturating add.
    let x1 = rect
        .x
        .saturating_add(rect.width as i32)
        .max(0)
        .min(width as i32) as u32;
    let y1 = rect
        .y
        .saturating_add(rect.height as i32)
        .max(0)
        .min(height as i32) as u32;
    if x1 <= x0 || y1 <= y0 {
        return None;
    }
    Some(img.crop_imm(x0, y0, x1 - x0, y1 - y0))
}

/// Resize an image to a target size, maintaining aspect ratio with
/// letterboxing.
///
/// Returns the resized image and the padding offsets (pad_x, pad_y) needed to
/// map model output coordinates back to the original image.
pub fn resize_with_pad(
    img: &image::DynamicImage,
    target_w: u32,
    target_h: u32,
) -> (image::DynamicImage, f32, f32, f32, f32) {
    let (orig_w, orig_h) = (img.width() as f32, img.height() as f32);
    let scale = (target_w as f32 / orig_w).min(target_h as f32 / orig_h);
    let new_w = (orig_w * scale) as u32;
    let new_h = (orig_h * scale) as u32;

    let resized = img.resize_exact(new_w, new_h, image::imageops::FilterType::Triangle);

    let pad_x = (target_w - new_w) / 2;
    let pad_y = (target_h - new_h) / 2;

    let mut canvas = image::DynamicImage::new_rgb8(target_w, target_h);
    image::imageops::overlay(&mut canvas, &resized, pad_x as i64, pad_y as i64);

    (canvas, scale, pad_x as f32, pad_y as f32, orig_w)
}

/// Normalize pixel values from [0, 255] to target range.
///
/// * `mean` — per-channel means (RGB order)
/// * `std`  — per-channel standard deviations (RGB order)
///
/// Output: `(pixel / 255.0 - mean) / std`
pub fn normalize_image(img: &image::DynamicImage, mean: [f32; 3], std: [f32; 3]) -> Vec<f32> {
    let rgb = img.to_rgb8();
    let mut data = Vec::with_capacity((rgb.width() * rgb.height() * 3) as usize);

    for pixel in rgb.pixels() {
        data.push((pixel[0] as f32 / 255.0 - mean[0]) / std[0]);
        data.push((pixel[1] as f32 / 255.0 - mean[1]) / std[1]);
        data.push((pixel[2] as f32 / 255.0 - mean[2]) / std[2]);
    }

    data
}

/// Convert a normalized image into an NCHW tensor.
///
/// `data` must have length `width * height * 3` in HWC order.
/// Returns an `Array4<f32>` with shape `[1, 3, H, W]`.
pub fn image_to_nchw_tensor(
    data: Vec<f32>,
    width: usize,
    height: usize,
) -> crate::Result<ndarray::Array4<f32>> {
    if data.len() != width * height * 3 {
        return Err(SyscityError::Internal(format!(
            "Data length {} does not match expected {} ({}x{}x3)",
            data.len(),
            width * height * 3,
            width,
            height,
        )));
    }

    let mut tensor = ndarray::Array4::<f32>::zeros((1, 3, height, width));

    for y in 0..height {
        for x in 0..width {
            let idx = (y * width + x) * 3;
            tensor[[0, 0, y, x]] = data[idx];
            tensor[[0, 1, y, x]] = data[idx + 1];
            tensor[[0, 2, y, x]] = data[idx + 2];
        }
    }

    Ok(tensor)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_normalize_image() {
        let img = image::DynamicImage::new_rgb8(2, 2);
        let normalized = normalize_image(&img, [0.5; 3], [0.5; 3]);
        assert_eq!(normalized.len(), 12);
        // Black pixel (0) → (0/255 - 0.5) / 0.5 = -1.0
        assert!((normalized[0] - -1.0).abs() < 0.001);
    }

    #[test]
    fn test_image_to_nchw_tensor() {
        let data = vec![
            1.0, 2.0, 3.0, 4.0, 5.0, 6.0, 7.0, 8.0, 9.0, 10.0, 11.0, 12.0,
        ];
        let tensor = image_to_nchw_tensor(data, 2, 2).unwrap();
        assert_eq!(tensor.shape(), &[1, 3, 2, 2]);
        assert_eq!(tensor[[0, 0, 0, 0]], 1.0);
        assert_eq!(tensor[[0, 1, 0, 0]], 2.0);
        assert_eq!(tensor[[0, 2, 0, 0]], 3.0);
    }
}
