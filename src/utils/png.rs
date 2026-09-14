//! PNG header parsing.
//!
//! Three private readers of the same chunk already exist — `office/docx.rs`,
//! `computer/platform/mobile/android.rs`, and an inline one in
//! `computer/platform/macos/screenshot.rs`. They are deliberately left alone:
//! their contracts differ, so folding them together is a decision about which
//! one wins rather than a move. The macOS reader also accepts JPEG and never
//! fails; the docx one skips the `IHDR` check. This is the version new callers
//! should use, and the one that says what it checked.

/// Width and height from a PNG's `IHDR` chunk.
///
/// `None` for anything that is not a PNG, or whose first chunk is not `IHDR`.
/// A caller about to treat these bytes as an image wants to be told that,
/// rather than handed a plausible-looking pair of numbers read out of a
/// non-image payload.
pub fn dimensions(bytes: &[u8]) -> Option<(u32, u32)> {
    const SIGNATURE: [u8; 8] = [0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];
    if bytes.len() < 24 || bytes[..8] != SIGNATURE || &bytes[12..16] != b"IHDR" {
        return None;
    }
    let width = u32::from_be_bytes(bytes[16..20].try_into().ok()?);
    let height = u32::from_be_bytes(bytes[20..24].try_into().ok()?);
    Some((width, height))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn png(width: u32, height: u32) -> Vec<u8> {
        let mut bytes = vec![0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];
        bytes.extend_from_slice(&13u32.to_be_bytes()); // IHDR length (unused here)
        bytes.extend_from_slice(b"IHDR");
        bytes.extend_from_slice(&width.to_be_bytes());
        bytes.extend_from_slice(&height.to_be_bytes());
        bytes
    }

    #[test]
    fn reads_the_ihdr_chunk() {
        assert_eq!(dimensions(&png(1280, 720)), Some((1280, 720)));
    }

    #[test]
    fn anything_that_is_not_a_png_is_refused() {
        assert_eq!(dimensions(b""), None);
        assert_eq!(dimensions(b"not an image at all, but long enough"), None);
        // Right length, wrong signature.
        let mut wrong = png(1, 1);
        wrong[1] = b'X';
        assert_eq!(dimensions(&wrong), None);
    }

    #[test]
    fn a_png_whose_first_chunk_is_not_ihdr_is_refused() {
        // A truncated or re-chunked payload still carries the signature; the
        // offsets below only mean anything when IHDR is where it is expected.
        let mut reordered = png(1, 1);
        reordered[12..16].copy_from_slice(b"IDAT");
        assert_eq!(dimensions(&reordered), None);
    }
}
