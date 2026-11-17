//! ONNX model auto-download — resolve vision models from disk or download.
//!
//! Follows the same pattern as `screen_recorder::resolve_or_download_ffmpeg`:
//! 1. Check data directory for each model
//! 2. Download missing models from HuggingFace
//! 3. Return paths to all resolved model files
//!
//! Models are stored at `~/.syscity/models/vision/`:
//! - macOS:   `~/.syscity/models/vision/`
//! - Linux:   `~/.syscity/models/vision/`
//! - Windows: `~\.syscity\models\vision\`

use std::path::{Path, PathBuf};

/// Paths to all resolved ONNX model files.
#[derive(Debug, Clone)]
pub struct VisionModelPaths {
    /// OmniParser UI element detection model.
    pub omniparser: PathBuf,
    /// RapidOCR text detection model (DBNet).
    pub det: PathBuf,
    /// RapidOCR text recognition model (CRNN).
    pub rec: PathBuf,
    /// Character dictionary the recognition model emits indices into.
    pub rec_dict: PathBuf,
}

/// Paths to the RapidOCR models.
#[derive(Debug, Clone)]
pub struct OcrModelPaths {
    /// Text detection model (DBNet).
    pub det: PathBuf,
    /// Text recognition model (CRNN).
    pub rec: PathBuf,
    /// Character dictionary the recognition model emits indices into.
    pub rec_dict: PathBuf,
}

/// Data directory for ONNX models: `~/.syscity/models/vision/`.
fn model_data_dir() -> PathBuf {
    vision_model_dir("")
}

/// Where vision models live.
///
/// `[computer.vision].model_dir` when it is set, else
/// `<layout root>/models/vision` (`~/.syscity/models/vision` by default).
/// Resolved through [`crate::dirs::paths`] rather than `home_dir()`, so
/// `SYSCITY_HOME`, a named profile, or a test's hermetic root all move the
/// models with everything else — a run must not probe the developer's real
/// home from inside a gateway that was deliberately rooted elsewhere.
/// Downloads always write here, so a caller that wants to know whether a model
/// is *present* uses the same answer rather than probing a directory the
/// loader would not read.
pub fn vision_model_dir(configured: &str) -> PathBuf {
    let configured = configured.trim();
    if !configured.is_empty() {
        return PathBuf::from(configured);
    }
    // `installed_paths` and not `paths`: a library caller that never installed
    // a layout root (a bare tool invocation, a unit test) must not be turned
    // into a panic just for asking where the models are. With no root, the
    // historical `~/.syscity` answer stands.
    match crate::dirs::installed_paths() {
        Some(paths) => paths.models_dir().join("vision"),
        None => {
            let base = dirs::home_dir().unwrap_or_else(|| PathBuf::from("/tmp"));
            base.join(".syscity").join("models").join("vision")
        }
    }
}

/// RapidOCR text detection (DBNet) model.
const DET_URL: &str =
    "https://hf-mirror.com/monkt/paddleocr-onnx/resolve/main/detection/v5/det.onnx";
/// RapidOCR text recognition (CRNN) model.
///
/// PP-OCRv3 english, not the newer PP-OCRv5 mobile model: under the ONNX
/// Runtime build this crate links, v5 confuses characters it is nonetheless
/// confident about (`In)Ce tOtal: 1234.5O USD` for a clean
/// `Invoice total: 1234.50 USD`), while v3 reads the same crops correctly.
const REC_URL: &str =
    "https://hf-mirror.com/SWHL/RapidOCR/resolve/main/PP-OCRv3/en_PP-OCRv3_rec_infer.onnx";
/// Character dictionary for [`REC_URL`], from the PaddleOCR release the model
/// was exported from: one character per line, in the model's output-class
/// order. Class `i` is line `i + 1`; class 0 is the CTC blank, and the model
/// is trained with `use_space_char`, which appends a space after the last
/// entry. Decoding with any other dictionary is garbage, so this is pinned to
/// the model rather than discovered.
const REC_DICT_URL: &str =
    "https://raw.githubusercontent.com/PaddlePaddle/PaddleOCR/main/ppocr/utils/en_dict.txt";
/// Same file over jsDelivr's CDN. GitHub raw is unreachable from some
/// networks the model mirror (hf-mirror) is chosen for, and a missing
/// dictionary takes OCR down entirely, so try both.
const REC_DICT_MIRROR_URL: &str =
    "https://cdn.jsdelivr.net/gh/PaddlePaddle/PaddleOCR@main/ppocr/utils/en_dict.txt";
/// Model files this build knows how to look for.
///
/// Named once so the probe, the loader and the size estimate cannot drift:
/// the probe is what decides whether a rung exists, and a name that disagrees
/// with the loader turns a present model into "absent" and a run that refuses
/// to look at a screen it could have read.
///
/// The OCR names are versioned because a cache written by an earlier release
/// holds a different recognition model under the old shared name, and reusing
/// it would silently restore the misreads above.
pub const OMNIPARSER_FILE: &str = "omniparser.onnx";
pub const DET_FILE: &str = "det.onnx";
const REC_FILE: &str = "rec_ppocrv3.onnx";
const REC_DICT_FILE: &str = "rec_dict_ppocrv3.txt";
/// OmniParser UI-element detector.
const OMNIPARSER_URL: &str =
    "https://hf-mirror.com/onnx-community/OmniParser-icon_detect_640x640/resolve/main/onnx/model.onnx";

/// Which local models are already on this machine.
///
/// A `stat` probe, never a download: a run start is not the place to discover
/// that an 88 MB fetch is needed. Kept as its own type because the two models
/// do different jobs — the detector finds controls, OCR reads text the tree
/// cannot — and a machine may have one without the other.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct LocalModels {
    /// `omniparser.onnx` is present: controls can be detected.
    pub ui_detect: bool,
    /// `det.onnx`, the recognition model and its dictionary are all present:
    /// text can be read.
    pub ocr: bool,
}

impl LocalModels {
    pub fn ui_detect() -> Self {
        Self { ui_detect: true, ocr: false }
    }

    pub fn ocr() -> Self {
        Self { ui_detect: false, ocr: true }
    }

    pub fn both() -> Self {
        Self { ui_detect: true, ocr: true }
    }

    /// Whether any local model could answer.
    pub fn any(&self) -> bool {
        self.ui_detect || self.ocr
    }
}

/// Which models are present under the configured (or default) model directory.
///
/// Deliberately separate from [`vision_model_dir`]: the directory is a
/// configuration question, this is a fact about the disk, and the run loop
/// needs the fact before it decides which rung exists.
pub fn local_models_present(configured_dir: &str) -> LocalModels {
    let dir = vision_model_dir(configured_dir);
    LocalModels {
        ui_detect: dir.join(OMNIPARSER_FILE).is_file(),
        // The recogniser is useless without its dictionary, so all three files
        // have to be there before OCR is claimed present — a partial download
        // must read as "absent", not as a run that fails halfway through.
        ocr: dir.join(DET_FILE).is_file()
            && dir.join(REC_FILE).is_file()
            && dir.join(REC_DICT_FILE).is_file(),
    }
}

/// Paths to the OCR models under the configured directory, when all exist.
///
/// `None` when any is missing: [`RapidOcr::new`] needs all three, and handing
/// it a path that does not exist turns "this rung is unavailable" into a load
/// error in the middle of a step.
///
/// [`RapidOcr::new`]: crate::computer::vision::ocr_rapid::RapidOcr::new
pub fn ocr_model_paths(configured_dir: &str) -> Option<(PathBuf, PathBuf, PathBuf)> {
    let dir = vision_model_dir(configured_dir);
    let det = dir.join(DET_FILE);
    let rec = dir.join(REC_FILE);
    let dict = dir.join(REC_DICT_FILE);
    if det.is_file() && rec.is_file() && dict.is_file() {
        Some((det, rec, dict))
    } else {
        None
    }
}

/// Resolve the RapidOCR models only: detection, recognition and its
/// dictionary.
///
/// Text extraction does not need the OmniParser detector, so this skips it —
/// the detector is a separate download for a different job.
pub async fn resolve_or_download_ocr_models() -> crate::computer::Result<OcrModelPaths> {
    let dir = model_data_dir();

    // Ensure data directory exists
    tokio::fs::create_dir_all(&dir).await.map_err(|e| {
        crate::computer::ComputerError::Other(format!(
            "Cannot create model data dir {}: {}",
            dir.display(),
            e
        ))
    })?;

    let det = resolve_single_model(&dir, DET_FILE, DET_URL).await?;
    let rec = resolve_single_model(&dir, REC_FILE, REC_URL).await?;
    let rec_dict =
        resolve_single_model_mirrored(&dir, REC_DICT_FILE, &[REC_DICT_URL, REC_DICT_MIRROR_URL])
            .await?;

    Ok(OcrModelPaths { det, rec, rec_dict })
}

/// Returns paths to all required model files.  Models that are already
/// stored are not re-downloaded.
pub async fn resolve_or_download_vision_models() -> crate::computer::Result<VisionModelPaths> {
    let dir = model_data_dir();

    // Ensure data directory exists
    tokio::fs::create_dir_all(&dir).await.map_err(|e| {
        crate::computer::ComputerError::Other(format!(
            "Cannot create model data dir {}: {}",
            dir.display(),
            e
        ))
    })?;

    let omniparser = resolve_single_model(&dir, OMNIPARSER_FILE, OMNIPARSER_URL).await?;

    let OcrModelPaths { det, rec, rec_dict } = resolve_or_download_ocr_models().await?;

    Ok(VisionModelPaths { omniparser, det, rec, rec_dict })
}

/// Resolve a single required model: already on disk → return, or download.
async fn resolve_single_model(
    model_dir: &Path,
    filename: &str,
    url: &str,
) -> crate::computer::Result<PathBuf> {
    let path = model_dir.join(filename);

    // Already exists on disk
    if path.exists() {
        tracing::debug!("ONNX model on disk: {}", path.display());
        return Ok(path);
    }

    // Not on disk: download
    tracing::info!(
        "Downloading ONNX model '{}' from {} (~{} MB)",
        filename,
        url,
        estimated_size(filename)
    );
    let data = download_bytes(url).await?;

    tokio::fs::write(&path, &data).await.map_err(|e| {
        crate::computer::ComputerError::Other(format!(
            "Failed to write model '{}' to {}: {}",
            filename,
            path.display(),
            e
        ))
    })?;

    tracing::info!("ONNX model '{}' saved to {} ({} bytes)", filename, path.display(), data.len());

    Ok(path)
}

/// Resolve a single required model, trying each URL in turn.
///
/// The first success wins; if every mirror fails, the first error is returned
/// — it names the canonical source, which is the one worth investigating.
async fn resolve_single_model_mirrored(
    model_dir: &Path,
    filename: &str,
    urls: &[&str],
) -> crate::computer::Result<PathBuf> {
    let mut first_error = None;
    for url in urls {
        match resolve_single_model(model_dir, filename, url).await {
            Ok(path) => return Ok(path),
            Err(e) => {
                if first_error.is_none() {
                    first_error = Some(e);
                }
            }
        }
    }
    match first_error {
        Some(e) => Err(e),
        None => Err(crate::computer::ComputerError::Other(format!(
            "No download URL configured for '{}'",
            filename
        ))),
    }
}

/// Download raw bytes from a URL.
///
/// Five minutes total and fifteen seconds to connect: the detector alone is
/// 88 MB, and a mirror can serve it at well under a megabyte a second without
/// being down — the short cap this replaces cut such downloads off mid-body.
async fn download_bytes(url: &str) -> crate::computer::Result<Vec<u8>> {
    let response = reqwest::Client::builder()
        .connect_timeout(std::time::Duration::from_secs(15))
        .timeout(std::time::Duration::from_secs(300))
        .user_agent("syscity/0.1")
        .build()
        .map_err(|e| crate::computer::ComputerError::Other(format!("HTTP client: {}", e)))?
        .get(url)
        .send()
        .await
        .map_err(|e| {
            crate::computer::ComputerError::Other(format!("Download failed ({}): {}", url, e))
        })?;
    if !response.status().is_success() {
        return Err(crate::computer::ComputerError::Other(format!(
            "Download returned HTTP {} for {}",
            response.status(),
            url
        )));
    }
    response
        .bytes()
        .await
        .map(|b| b.to_vec())
        .map_err(|e| crate::computer::ComputerError::Other(format!("Download body: {}", e)))
}

/// Rough size estimate for display purposes.
fn estimated_size(filename: &str) -> &str {
    match filename {
        "omniparser.onnx" => "12",
        "det.onnx" => "88",
        REC_FILE => "9",
        REC_DICT_FILE => "<1",
        _ => "?",
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_model_data_dir_is_absolute() {
        let dir = model_data_dir();
        assert!(dir.is_absolute(), "data dir should be absolute: {:?}", dir);
        // Under a layout root it is `<root>/models/vision`; with none installed
        // it is the historical `~/.syscity/models/vision`. Both end the same
        // way, which is the part that must not drift.
        assert!(dir.ends_with("models/vision"), "should end with models/vision: {:?}", dir);
    }

    /// A configured directory wins, verbatim — the probe and the loader must
    /// agree on the one path a caller asked for.
    #[test]
    fn a_configured_dir_is_used_as_given() {
        assert_eq!(vision_model_dir("  /tmp/models  "), PathBuf::from("/tmp/models"));
        assert_eq!(vision_model_dir(""), model_data_dir());
    }

    #[test]
    fn test_estimated_size_known_models() {
        assert_eq!(estimated_size("omniparser.onnx"), "12");
        assert_eq!(estimated_size("det.onnx"), "88");
        assert_eq!(estimated_size(REC_FILE), "9");
        assert_eq!(estimated_size(REC_DICT_FILE), "<1");
    }

    #[test]
    fn test_estimated_size_unknown() {
        assert_eq!(estimated_size("unknown.onnx"), "?");
    }
}
