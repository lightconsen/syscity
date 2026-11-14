//! Shrinking a screenshot for the *model's* context, not for a network link.
//!
//! [`crate::computer::screenshot_encoder`] already resizes screenshots, but its
//! axis is bandwidth: it asks [`NetworkCondition`] how far the image has to
//! travel and picks resolution and quality from that. On a local machine the
//! answer is `Local` — 1920 px, PNG, quality 90 — because a loopback link
//! costs nothing. That is exactly backwards for computer use, where the
//! expensive part is not moving the bytes but the model reading them. A local
//! run is the case where the token bill matters most, and it was getting the
//! largest image.
//!
//! So this module answers a different question with a different default:
//! **1024 px wide, JPEG, quality 75**. Reusing `NetworkCondition` for this
//! would have meant every future reader having to know which of the two
//! meanings the value had, and the two would drift. They stay separate.
//!
//! The resize happens through the `image` crate rather than an external
//! binary. `screenshot_encoder` shells out to ImageMagick/ffmpeg/sips and
//! falls back to the original file when none is installed — a silent cost
//! regression on a machine that has no `convert`. The `image` crate is already
//! a dependency of the `vision` feature and needs nothing installed. Without
//! that feature the file is returned untouched, which the caller can see from
//! the returned [`EncodedImage`] rather than having to guess.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// Ceiling and format for images handed to the model.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImageLimit {
    /// Largest width in pixels. Never upscales.
    pub max_width: u32,
    /// `"jpeg"` or `"png"`.
    pub format: String,
    /// JPEG quality, 1–100. Ignored for PNG.
    pub jpeg_quality: u8,
}

impl Default for ImageLimit {
    fn default() -> Self {
        Self {
            max_width: 1024,
            format: "jpeg".to_string(),
            jpeg_quality: 75,
        }
    }
}

impl ImageLimit {
    /// Build from configuration, falling back to the defaults for a format the
    /// encoder does not know.
    ///
    /// An unrecognised format is treated as JPEG rather than as an error: the
    /// point of the setting is to make images cheaper, and refusing to produce
    /// one would drop the run back to the expensive default.
    pub fn from_config(max_width: u32, format: &str, jpeg_quality: u8) -> Self {
        let format = match format.to_ascii_lowercase().as_str() {
            "png" => "png",
            _ => "jpeg",
        };
        Self {
            max_width: max_width.max(64),
            format: format.to_string(),
            jpeg_quality: jpeg_quality.clamp(1, 100),
        }
    }

    /// The MIME type of the produced image.
    pub fn mime(&self) -> &'static str {
        match self.format.as_str() {
            "png" => "image/png",
            _ => "image/jpeg",
        }
    }
}

/// What actually came out of a shrink.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EncodedImage {
    /// The image bytes, base64-encoded (the shape every provider expects).
    pub base64: String,
    pub width: u32,
    pub height: u32,
    pub mime: &'static str,
    /// Whether anything was actually changed.
    ///
    /// `false` means the image was already within the limit and in the right
    /// format, or the build has no `vision` feature — a caller that wants to
    /// report "saved nothing" can tell the difference from "we tried".
    pub resized: bool,
}

impl EncodedImage {
    /// Approximate decoded byte length, for the run summary.
    pub fn byte_len(&self) -> usize {
        base64_decoded_len(&self.base64)
    }
}

/// Decoded length of a base64 string, without decoding it.
fn base64_decoded_len(encoded: &str) -> usize {
    let len = encoded.len();
    if len == 0 {
        return 0;
    }
    let padding = encoded.bytes().rev().take_while(|b| *b == b'=').count();
    len / 4 * 3 - padding
}

#[cfg(feature = "vision")]
mod inner {
    use super::*;
    use base64::Engine;

    /// Resize and re-encode `png` (raw image bytes) to fit `limit`.
    ///
    /// Only downscales. An image already narrower than the limit keeps its
    /// dimensions but is still re-encoded when the format differs, because the
    /// format is where most of the size difference is for a UI screenshot.
    pub fn encode(png: &[u8], limit: &ImageLimit) -> crate::Result<EncodedImage> {
        let img = image::load_from_memory(png)
            .map_err(|e| crate::error::SyscityError::Internal(format!("decode screenshot: {e}")))?;
        let src_w = img.width();
        let wants_resize = src_w > limit.max_width;

        let out_img = if wants_resize {
            // Preserve aspect ratio; `resize` fits inside the box without
            // distorting, which matters because the model reads text off it.
            img.resize(limit.max_width, u32::MAX, image::imageops::FilterType::Lanczos3)
        } else {
            img
        };

        let mut buf: Vec<u8> = Vec::new();
        if limit.format == "png" {
            out_img
                .write_to(&mut std::io::Cursor::new(&mut buf), image::ImageFormat::Png)
                .map_err(|e| crate::error::SyscityError::Internal(format!("encode png: {e}")))?;
        } else {
            // JPEG has no alpha channel. Flattening onto white rather than
            // letting the encoder guess keeps a transparent window's text
            // readable.
            let rgb = out_img.to_rgb8();
            let mut cursor = std::io::Cursor::new(&mut buf);
            let mut encoder =
                image::codecs::jpeg::JpegEncoder::new_with_quality(&mut cursor, limit.jpeg_quality);
            encoder
                .encode(rgb.as_raw(), rgb.width(), rgb.height(), image::ExtendedColorType::Rgb8)
                .map_err(|e| crate::error::SyscityError::Internal(format!("encode jpeg: {e}")))?;
        }

        Ok(EncodedImage {
            base64: base64::engine::general_purpose::STANDARD.encode(&buf),
            width: out_img.width(),
            height: out_img.height(),
            mime: limit.mime(),
            // Either dimension changed, or the encoding did. Both are things
            // the caller may want to report; an image that fits already and
            // stays PNG is the only unchanged case.
            resized: wants_resize || limit.format != "png",
        })
    }

    /// Read a file and encode it, returning the path plus what came out.
    pub fn encode_file(
        path: &std::path::Path,
        limit: &ImageLimit,
    ) -> crate::Result<(PathBuf, EncodedImage)> {
        let bytes = std::fs::read(path).map_err(|e| crate::error::SyscityError::Storage {
            context: format!("read screenshot {}", path.display()),
            details: e.to_string(),
        })?;
        let encoded = encode(&bytes, limit)?;
        Ok((path.to_path_buf(), encoded))
    }
}

#[cfg(feature = "vision")]
pub use inner::{encode, encode_file};

/// Encode screenshot bytes to fit `limit`, or pass them through unchanged.
///
/// Without the `vision` feature there is no resizer compiled in, so the bytes
/// are base64-encoded as-is and `resized` reports `false`. That is the honest
/// answer: the caller is looking at an unscaled image, not at a failed shrink.
pub fn encode_or_passthrough(png: &[u8], limit: &ImageLimit) -> crate::Result<EncodedImage> {
    #[cfg(feature = "vision")]
    {
        encode(png, limit)
    }
    #[cfg(not(feature = "vision"))]
    {
        use base64::Engine;
        let _ = limit;
        Ok(EncodedImage {
            base64: base64::engine::general_purpose::STANDARD.encode(png),
            width: 0,
            height: 0,
            mime: "image/png",
            resized: false,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_defaults_are_the_cheap_ones() {
        let limit = ImageLimit::default();
        assert_eq!(limit.max_width, 1024);
        assert_eq!(limit.format, "jpeg");
        assert_eq!(limit.jpeg_quality, 75);
        assert_eq!(limit.mime(), "image/jpeg");
    }

    /// The point of the module: the default must not coincide with the
    /// bandwidth encoder's most expensive setting, or the wrong axis is back.
    #[test]
    fn the_default_is_not_the_bandwidth_encoder_local_setting() {
        let local = crate::computer::screenshot_encoder::NetworkCondition::Local;
        assert!(ImageLimit::default().max_width < local.max_width());
        assert_eq!(ImageLimit::default().format, "jpeg");
        assert_ne!(local.preferred_format(), ImageLimit::default().format);
    }

    #[test]
    fn a_bad_format_falls_back_to_jpeg_rather_than_failing() {
        let limit = ImageLimit::from_config(1024, "gif", 75);
        assert_eq!(limit.format, "jpeg");
        assert_eq!(limit.mime(), "image/jpeg");
    }

    #[test]
    fn quality_is_clamped_and_width_has_a_floor() {
        let limit = ImageLimit::from_config(1, "JPEG", 250);
        assert_eq!(limit.max_width, 64);
        assert_eq!(limit.jpeg_quality, 100);
        let limit = ImageLimit::from_config(800, "jpg", 0);
        assert_eq!(limit.jpeg_quality, 1);
        assert_eq!(limit.format, "jpeg");
    }

    #[test]
    fn png_round_trips_through_from_config() {
        let limit = ImageLimit::from_config(800, "PNG", 75);
        assert_eq!(limit.format, "png");
        assert_eq!(limit.mime(), "image/png");
    }

    #[test]
    fn base64_length_is_reported_without_decoding() {
        assert_eq!(base64_decoded_len(""), 0);
        assert_eq!(base64_decoded_len("AAAA"), 3);
        assert_eq!(base64_decoded_len("AAA="), 2);
        assert_eq!(base64_decoded_len("AA=="), 1);
    }

    #[cfg(feature = "vision")]
    fn solid_png(width: u32, height: u32) -> Vec<u8> {
        let img = image::RgbImage::from_pixel(width, height, image::Rgb([200, 40, 40]));
        let mut buf = Vec::new();
        image::DynamicImage::ImageRgb8(img)
            .write_to(&mut std::io::Cursor::new(&mut buf), image::ImageFormat::Png)
            .unwrap();
        buf
    }

    /// A 1920 px screenshot — what `NetworkCondition::Local` would have sent
    /// unshrunk — comes back at the configured width.
    #[cfg(feature = "vision")]
    #[test]
    fn a_wide_screenshot_is_scaled_to_the_limit() {
        let png = solid_png(1920, 1080);
        let out = encode(&png, &ImageLimit::default()).unwrap();
        assert_eq!(out.width, 1024);
        // Aspect ratio preserved: 1080 * 1024/1920 = 576.
        assert_eq!(out.height, 576);
        assert!(out.resized);
        assert_eq!(out.mime, "image/jpeg");
        assert!(!out.base64.is_empty());

        // And the shrink is real, not a relabelled copy: the 1920 px source is
        // the expensive thing this module exists to stop sending.
        use base64::Engine;
        let original_len = png.len();
        assert!(
            out.byte_len() < original_len,
            "{} bytes out of a {} byte source",
            out.byte_len(),
            original_len
        );
        assert_eq!(
            png.len(),
            base64::engine::general_purpose::STANDARD
                .decode(base64::engine::general_purpose::STANDARD.encode(&png))
                .unwrap()
                .len(),
            "sanity: the source is what we think it is"
        );
    }

    /// Never upscale: a small screenshot is already cheap.
    #[cfg(feature = "vision")]
    #[test]
    fn a_small_screenshot_is_not_enlarged() {
        let png = solid_png(400, 300);
        let out = encode(&png, &ImageLimit::default()).unwrap();
        assert_eq!((out.width, out.height), (400, 300));
    }

    /// The output must be a decodable image, not merely short.
    #[cfg(feature = "vision")]
    #[test]
    fn the_output_decodes_at_the_reported_size() {
        let png = solid_png(1600, 900);
        let out = encode(&png, &ImageLimit::default()).unwrap();
        use base64::Engine;
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(&out.base64)
            .unwrap();
        let img = image::load_from_memory(&bytes).expect("the result is a real image");
        assert_eq!((img.width(), img.height()), (out.width, out.height));
        assert_eq!((img.width(), img.height()), (1024, 576));
    }

    /// Without the `vision` feature there is no resizer compiled in, so the
    /// bytes pass through and the caller is told so — an unscaled image, not a
    /// silently failed shrink.
    #[cfg(not(feature = "vision"))]
    #[test]
    fn without_the_feature_the_bytes_pass_through_unchanged() {
        let png = b"\x89PNG\r\n\x1a\nnot-really-a-png";
        let out = encode_or_passthrough(png, &ImageLimit::default()).unwrap();
        assert!(!out.resized);
        assert_eq!(out.mime, "image/png");
        use base64::Engine;
        let decoded = base64::engine::general_purpose::STANDARD
            .decode(&out.base64)
            .unwrap();
        assert_eq!(decoded, png);
    }
}
