//! Reading text off the screenshot, for screens the tree cannot describe.
use serde_json::Value;

// The set, the device targeting, and the siblings' items: this file used
// to be part of `android`, and names them exactly as it did there.
use super::*;

/// A line of text read from the screenshot, for screens the accessibility tree
/// cannot describe.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct OcrLine {
    pub text: String,
    /// Centre in device pixels — the same space as [`UiElement::center`], so a
    /// line can be tapped by passing these coordinates.
    pub center: (i32, i32),
    /// `(x1, y1, x2, y2)` in device pixels.
    pub bounds: (i32, i32, i32, i32),
    pub confidence: f32,
}

/// Turn OCR blocks into the tap-addressable shape above.
#[cfg(feature = "vision")]
pub(super) fn ocr_lines(blocks: Vec<crate::computer::vision::TextBlock>) -> Vec<OcrLine> {
    blocks
        .into_iter()
        .map(|block| {
            let rect = block.bounds;
            let x1 = rect.x;
            let y1 = rect.y;
            let x2 = rect.x + rect.width as i32;
            let y2 = rect.y + rect.height as i32;
            OcrLine {
                text: block.text,
                center: ((x1 + x2) / 2, (y1 + y2) / 2),
                bounds: (x1, y1, x2, y2),
                confidence: block.confidence,
            }
        })
        .collect()
}

/// Read text out of an already-captured screenshot with the ONNX OCR model.
///
/// The engine is loaded once and kept: constructing it reads two model files,
/// and the models download on first use.
#[cfg(feature = "vision")]
pub async fn ocr_png(png: &[u8]) -> Result<Vec<OcrLine>, String> {
    use crate::computer::types::Screenshot;
    use crate::computer::vision::ocr_rapid::RapidOcr;
    use base64::Engine as _;

    static ENGINE: tokio::sync::Mutex<Option<RapidOcr>> = tokio::sync::Mutex::const_new(None);

    let (width, height) =
        png_dimensions(png).ok_or_else(|| "the screenshot's size could not be read".to_string())?;
    let encoded = base64::engine::general_purpose::STANDARD.encode(png);
    let screenshot = Screenshot::new(encoded, width, height);

    let mut guard = ENGINE.lock().await;
    if guard.is_none() {
        let engine = RapidOcr::new_auto()
            .await
            .map_err(|e| format!("the OCR models could not be loaded ({e})"))?;
        *guard = Some(engine);
    }
    let engine = guard
        .as_mut()
        .ok_or_else(|| "the OCR engine is unavailable".to_string())?;
    let blocks = engine
        .detect_text(&screenshot)
        .await
        .map_err(|e| format!("OCR failed ({e})"))?;
    Ok(ocr_lines(blocks))
}

/// Read text out of a screenshot in a build without an ONNX engine.
///
/// On the phone itself there is a perfectly good recogniser in the platform,
/// so this is only "no OCR" when *neither* path exists: the mobile feature
/// profile drops `vision` (no ORT, no `image` crate), and the phone build
/// answers through its own ML Kit bridge instead. A desktop build without
/// `vision` installs no bridge, so it still fails here — explicitly, rather
/// than returning an empty list, which would read like "the screen is blank"
/// and send the model looking for a problem that is not there.
#[cfg(not(feature = "vision"))]
pub async fn ocr_png(png: &[u8]) -> Result<Vec<OcrLine>, String> {
    let Some(bridge) = crate::device::bridge() else {
        return Err("this build has no OCR engine (compiled without the `vision` feature, and no \
             on-device bridge)"
            .to_string());
    };
    use base64::Engine as _;
    let payload = serde_json::json!({
        "pngBase64": base64::engine::general_purpose::STANDARD.encode(png),
    });
    // The bridge round trip is a JNI hop into a blocking recogniser. It has no
    // timeout of its own, and a stuck model would otherwise freeze the whole
    // read for the rest of the turn.
    let answer = match tokio::time::timeout(
        OCR_BRIDGE_TIMEOUT,
        bridge.call(crate::device::CMD_OCR, payload),
    )
    .await
    {
        Err(_) => {
            return Err(format!(
                "{}: the on-device OCR did not answer within {} s",
                OCR_UNAVAILABLE,
                OCR_BRIDGE_TIMEOUT.as_secs()
            ))
        }
        Ok(Err(error)) => return Err(format!("{OCR_UNAVAILABLE}: {error}")),
        Ok(Ok(answer)) => answer,
    };
    parse_ocr_answer(&answer)
}

/// How long the on-device OCR may take before the read is abandoned.
///
/// A full-screen recognition takes a few hundred milliseconds on a mid-range
/// phone; anything near five seconds means the model is wedged, and waiting
/// longer only makes the turn slower to fail.
#[cfg_attr(feature = "vision", allow(dead_code))]
pub(super) const OCR_BRIDGE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

/// The error code the bridge uses for "the engine could not run".
#[cfg_attr(feature = "vision", allow(dead_code))]
pub(super) const OCR_UNAVAILABLE: &str = "ocr_unavailable";

/// Parse the bridge's answer into lines, refusing the "no engine" sentinel.
///
/// `ok: false` carries `code: "ocr_unavailable"`; turning that into an empty
/// list would tell the model the screen is blank, which is a different (and
/// wrong) statement. A missing `lines` array is a malformed payload and fails
/// the same way despite `ok: true`.
///
/// Lives outside the `not(vision)` gate so the default test run exercises it;
/// the phone build is the one that calls it.
#[cfg_attr(feature = "vision", allow(dead_code))]
pub(super) fn parse_ocr_answer(answer: &serde_json::Value) -> Result<Vec<OcrLine>, String> {
    if !answer.get("ok").and_then(Value::as_bool).unwrap_or(false) {
        let code = answer
            .get("code")
            .and_then(Value::as_str)
            .unwrap_or(OCR_UNAVAILABLE);
        let detail = answer
            .get("error")
            .and_then(Value::as_str)
            .unwrap_or("the on-device OCR engine could not run");
        return Err(format!("{code}: {detail}"));
    }
    let lines = answer
        .get("lines")
        .and_then(Value::as_array)
        .ok_or_else(|| format!("{OCR_UNAVAILABLE}: the OCR answer carried no line list"))?;
    let mut out = Vec::with_capacity(lines.len());
    for line in lines {
        let Some(text) = line.get("text").and_then(Value::as_str) else {
            continue;
        };
        let text = text.trim();
        if text.is_empty() {
            continue;
        }
        let Some(bounds) = line.get("bounds").and_then(Value::as_array) else {
            continue;
        };
        if bounds.len() != 4 {
            continue;
        }
        let mut rect = [0i32; 4];
        let mut readable = true;
        for (slot, value) in rect.iter_mut().zip(bounds) {
            match value.as_i64().and_then(|v| i32::try_from(v).ok()) {
                Some(v) => *slot = v,
                None => {
                    readable = false;
                    break;
                }
            }
        }
        if !readable {
            continue;
        }
        let (x1, y1, x2, y2) = (rect[0], rect[1], rect[2], rect[3]);
        if x2 <= x1 || y2 <= y1 {
            // A zero-area box cannot be tapped, so it is not a usable line.
            continue;
        }
        out.push(OcrLine {
            text: text.to_string(),
            center: ((x1 + x2) / 2, (y1 + y2) / 2),
            bounds: (x1, y1, x2, y2),
            confidence: line
                .get("confidence")
                .and_then(Value::as_f64)
                .map(|c| c as f32)
                .unwrap_or(0.0),
        });
    }
    Ok(out)
}

/// Read text out of whatever `target` points at.
pub async fn ocr_screen(device: &Option<String>, target: Target) -> Result<Vec<OcrLine>, String> {
    let png = capture_png(device, target)
        .await
        .map_err(|e| e.to_string())?;
    ocr_png(&png).await
}

/// One line per recognised text block, for the model to read.
pub fn format_ocr_lines(lines: &[OcrLine]) -> String {
    if lines.is_empty() {
        return "OCR read no text from this screen.".to_string();
    }
    let mut out = vec![format!(
        "{} text line(s) read from the screenshot:",
        lines.len()
    )];
    for line in lines {
        out.push(format!("{} (center=({},{}))", line.text, line.center.0, line.center.1));
    }
    out.join("\n")
}
