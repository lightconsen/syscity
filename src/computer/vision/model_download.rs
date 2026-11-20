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
use std::sync::{Arc, RwLock};

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

/// Where the models live when nothing overrides the directory.
///
/// Test-only since the settings handle landed: production callers ask
/// [`vision_model_dir`] for the configured directory, and this is the
/// configured-with-nothing reference the probe tests compare against.
#[cfg(test)]
fn model_data_dir() -> PathBuf {
    vision_model_dir(&shared_settings().model_dir())
}

/// `[computer.vision]`, as every vision caller reads it.
///
/// A process-wide handle rather than a value carried by each caller, because
/// there is one model directory and one mode: a second directory would be a
/// second 88 MB download, and a second "off" would be a promise that was only
/// kept in some rungs.
///
/// It exists because the callers are not all in a position to be handed the
/// config. The run loop's resolver is built from `[computer.vision]` directly,
/// but `screen_ocr`, `screen_ui_detect`, the `image` tool, a step's read-back
/// and the phone's OCR have no config in hand — they only know "load the
/// installed models", which used to mean "load them from the default
/// directory", *ignoring a `model_dir` the user had set*. The setting is
/// edited at runtime too, so a value captured at startup would be the wrong
/// answer the first time someone moved it.
///
/// This is not a cache: nothing here holds an engine. It holds the two
/// settings, so the loaders look where the operator said.
#[derive(Debug, Clone, Default)]
pub struct VisionSettings {
    /// `[computer.vision].model_dir`, empty for the default location.
    configured_dir: Arc<RwLock<String>>,
    /// `[computer.vision].mode`, so `"off"` reaches a reader built earlier.
    mode: Arc<RwLock<String>>,
    /// `[computer.vision].ocr`, so a preset change reaches a reader built
    /// earlier rather than only the next process.
    ocr: Arc<RwLock<String>>,
}

impl VisionSettings {
    /// Settings for a configured directory and mode, with the default preset.
    pub fn new(configured_dir: impl Into<String>, mode: impl Into<String>) -> Self {
        Self {
            configured_dir: Arc::new(RwLock::new(configured_dir.into())),
            mode: Arc::new(RwLock::new(mode.into())),
            ocr: Arc::new(RwLock::new(DEFAULT_OCR_PRESET.key().to_string())),
        }
    }

    /// Settings as `[computer.vision]` says.
    pub fn from_config(config: &crate::config::VisionConfig) -> Self {
        let settings = Self::new(config.model_dir.clone(), config.mode.clone());
        *settings.ocr.write().unwrap_or_else(|e| e.into_inner()) = config.ocr.clone();
        settings
    }

    /// Adopt a new `[computer.vision]` (a config edit, a hot reload).
    pub fn replace(&self, config: &crate::config::VisionConfig) {
        *self
            .configured_dir
            .write()
            .unwrap_or_else(|e| e.into_inner()) = config.model_dir.clone();
        *self.mode.write().unwrap_or_else(|e| e.into_inner()) = config.mode.clone();
        *self.ocr.write().unwrap_or_else(|e| e.into_inner()) = config.ocr.clone();
    }

    /// The configured model directory, empty for the default location.
    pub fn model_dir(&self) -> String {
        self.configured_dir
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    /// The configured recognition preset spelling (`[computer.vision].ocr`).
    pub fn ocr_preset(&self) -> String {
        self.ocr.read().unwrap_or_else(|e| e.into_inner()).clone()
    }

    /// Where the models live, configured or not.
    pub fn model_path(&self) -> PathBuf {
        vision_model_dir(&self.model_dir())
    }

    /// Whether vision was turned off (`[computer.vision].mode = "off"`).
    ///
    /// Read rather than captured for the same reason as the directory: it is a
    /// promise that the tree is the only input, and a session built before the
    /// setting changed must not keep looking at pixels.
    pub fn disabled(&self) -> bool {
        self.mode
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .trim()
            .eq_ignore_ascii_case("off")
    }
}

/// The process-wide settings handle, defaulting to the built-in locations.
///
/// The gateway replaces it at startup and on every config change. A library
/// caller that never installed one gets exactly the historical behaviour: the
/// default directory, vision on.
pub fn shared_settings() -> &'static VisionSettings {
    static SHARED: std::sync::OnceLock<VisionSettings> = std::sync::OnceLock::new();
    SHARED.get_or_init(VisionSettings::default)
}

/// Publish a `[computer.vision]` for every reader to see.
///
/// Called at tool-init and on every write that touches the section, so the
/// loaders follow a setting that changed after they were built. The settings
/// object itself is a `OnceLock` and is never replaced: the readers hold a
/// `&'static`, and the fields behind it are the part that changes.
pub fn install_settings(config: &crate::config::VisionConfig) {
    let settings = shared_settings();
    // The first install must carry the startup value rather than install an
    // empty one and then replace it — a reader between the two calls would see
    // the default directory.
    settings.replace(config);
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
///
/// Shared by every preset: DBNet finds *where* text is, which does not depend
/// on the language it is written in. Only the recogniser and its dictionary
/// differ between presets.
const DET_URL: &str =
    "https://hf-mirror.com/monkt/paddleocr-onnx/resolve/main/detection/v5/det.onnx";

/// A recognition model this build has been taught end to end, named.
///
/// The recogniser is not a plug-in point. The pipeline hands it a tensor named
/// `x`, takes the first output, assumes a `[T, 1, classes]` or
/// `[1, T, classes]` shape, resizes every crop to a fixed height, normalises to
/// `[0, 1]`, greeds over a CTC decode and maps class `i` to line `i + 1` of
/// one exact dictionary. Any one of those being different for a file picked by
/// hand produces a load that succeeds and decodes to confident garbage — and
/// the dictionary check only catches a *length* mismatch, never a wrong order.
///
/// So a preset is a model set that has actually been run against this pipeline,
/// and [`OcrPreset::parse`] is the only door: a name it does not know is
/// refused rather than reinterpreted as a path.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum OcrPreset {
    /// PP-OCRv3 english recognition, 97 output classes over a 95-line
    /// dictionary (CTC blank, then the space `use_space_char` appends).
    ///
    /// The default, and the model the detector's thresholds were calibrated
    /// against. It reads ASCII — UI labels, invoice text, log output — and
    /// renders anything else as nearest-neighbour noise.
    #[default]
    PpOcrV3En,
    /// PP-OCRv3 chinese recognition: 6625 output classes over PaddleOCR's
    /// 6 623-line `ppocr_keys_v1`.
    ///
    /// Reads Chinese *and* the ASCII a Chinese UI still shows (digits,
    /// punctuation, Latin words), which is the case the english model cannot
    /// do at all. Verified against this pipeline before being offered: the
    /// same crop that the english model renders as `??????` reads
    /// `发票总计：1234.50元` here.
    PpOcrV3Ch,
}

impl OcrPreset {
    /// Every preset this build offers, the default first.
    pub fn all() -> &'static [OcrPreset] {
        &[OcrPreset::PpOcrV3En, OcrPreset::PpOcrV3Ch]
    }

    /// The config spelling (`[computer.vision].ocr`).
    pub fn key(&self) -> &'static str {
        match self {
            OcrPreset::PpOcrV3En => "ppocrv3-en",
            OcrPreset::PpOcrV3Ch => "ppocrv3-ch",
        }
    }

    /// What the preset reads, for an error message that names the choices.
    pub fn label(&self) -> &'static str {
        match self {
            OcrPreset::PpOcrV3En => "english (ASCII)",
            OcrPreset::PpOcrV3Ch => "chinese + ASCII",
        }
    }

    /// The preset a config value names, or `None`.
    ///
    /// Empty is the default rather than an error: a config written before the
    /// key existed, and a `VisionSettings` built from a path, both carry no
    /// spelling and must not be refused. Matching is case-insensitive and
    /// tolerates the underscore spelling a user may type for the hyphen.
    pub fn parse(name: &str) -> Option<Self> {
        let name = name.trim();
        if name.is_empty() {
            return Some(Self::default());
        }
        let normalised = name.to_ascii_lowercase().replace('_', "-");
        Self::all()
            .iter()
            .copied()
            .find(|preset| preset.key() == normalised)
    }

    /// Recognition model file name.
    ///
    /// Versioned by preset: the cache is a directory shared by every preset,
    /// so two presets must not write one file. An older release's
    /// `rec_ppocrv3.onnx` is the english model under the pre-preset name and
    /// is still read, which is what keeps an existing install from
    /// re-downloading 9 MB for selecting the default it already had.
    pub fn rec_file(&self) -> &'static str {
        match self {
            OcrPreset::PpOcrV3En => "rec_ppocrv3.onnx",
            OcrPreset::PpOcrV3Ch => "rec_ppocrv3_ch.onnx",
        }
    }

    /// Character dictionary file name, decoded class `i` → line `i + 1`.
    pub fn dict_file(&self) -> &'static str {
        match self {
            OcrPreset::PpOcrV3En => "rec_dict_ppocrv3.txt",
            OcrPreset::PpOcrV3Ch => "rec_dict_ppocrv3_ch.txt",
        }
    }

    /// Where the recognition model is fetched from.
    ///
    /// PP-OCRv3, not the newer PP-OCRv5 mobile model: under the ONNX Runtime
    /// build this crate links, v5 confuses characters it is nonetheless
    /// confident about (`In)Ce tOtal: 1234.5O USD` for a clean
    /// `Invoice total: 1234.50 USD`), while v3 reads the same crops correctly.
    fn rec_url(&self) -> &'static str {
        match self {
            OcrPreset::PpOcrV3En => {
                "https://hf-mirror.com/SWHL/RapidOCR/resolve/main/PP-OCRv3/en_PP-OCRv3_rec_infer.onnx"
            }
            OcrPreset::PpOcrV3Ch => {
                "https://hf-mirror.com/SWHL/RapidOCR/resolve/main/PP-OCRv3/ch_PP-OCRv3_rec_infer.onnx"
            }
        }
    }

    /// Dictionary mirrors, tried in order.
    ///
    /// From the PaddleOCR release the model was exported from: one character
    /// per line, in the model's output-class order. Class `i` is line `i + 1`;
    /// class 0 is the CTC blank, and `use_space_char` appends a space after
    /// the last entry. Decoding with any other dictionary is garbage, so this
    /// is pinned to the model rather than discovered.
    ///
    /// jsDelivr is the second mirror because GitHub raw is unreachable from
    /// some networks the model mirror (hf-mirror) is chosen for, and a missing
    /// dictionary takes OCR down entirely.
    fn dict_urls(&self) -> &'static [&'static str] {
        match self {
            OcrPreset::PpOcrV3En => &[
                "https://raw.githubusercontent.com/PaddlePaddle/PaddleOCR/main/ppocr/utils/en_dict.txt",
                "https://cdn.jsdelivr.net/gh/PaddlePaddle/PaddleOCR@main/ppocr/utils/en_dict.txt",
            ],
            OcrPreset::PpOcrV3Ch => &[
                "https://raw.githubusercontent.com/PaddlePaddle/PaddleOCR/main/ppocr/utils/ppocr_keys_v1.txt",
                "https://cdn.jsdelivr.net/gh/PaddlePaddle/PaddleOCR@main/ppocr/utils/ppocr_keys_v1.txt",
            ],
        }
    }
}

/// The preset a fresh install reads text with.
pub const DEFAULT_OCR_PRESET: OcrPreset = OcrPreset::PpOcrV3En;

/// The preset `[computer.vision].ocr` names, as the process now has it.
///
/// Read through [`shared_settings`] rather than captured: every caller here is
/// reached from a tool built before a config edit, and a captured value would
/// make "switch to Chinese" take effect on the next launch. An unknown
/// spelling falls back to the default — `config.set` refuses it at write time,
/// so reaching here with one means a file edited by hand, and the honest
/// answer is the model that was calibrated rather than an error mid-read.
pub fn configured_preset() -> OcrPreset {
    OcrPreset::parse(&shared_settings().ocr_preset()).unwrap_or(DEFAULT_OCR_PRESET)
}

/// Whether a cached OCR engine slot holds an engine for `preset`.
///
/// The engines are expensive to build (two ONNX sessions) and a process may
/// see more than one preset over its life, so every cache tags its slot and
/// reloads on a mismatch. This is that one predicate: a plain `None` slot and a
/// slot holding another preset's engine are the same answer — load — while a
/// matching slot must be reused, because reloading per frame would cost
/// seconds per read.
pub fn engine_matches<S>(slot: &Option<(OcrPreset, S)>, preset: OcrPreset) -> bool {
    matches!(slot, Some((tag, _)) if *tag == preset)
}

/// Model files this build knows how to look for.
///
/// Named once so the probe, the loader and the size estimate cannot drift:
/// the probe is what decides whether a rung exists, and a name that disagrees
/// with the loader turns a present model into "absent" and a run that refuses
/// to look at a screen it could have read.
///
/// The recognition names live on [`OcrPreset`] because they differ per preset;
/// the detector is the same file for all of them.
pub const OMNIPARSER_FILE: &str = "omniparser.onnx";
pub const DET_FILE: &str = "det.onnx";
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
    local_models_present_for(configured_dir, DEFAULT_OCR_PRESET)
}

/// Which models are present for one OCR preset, under a directory.
///
/// The preset matters: its recognition model and dictionary are its own files,
/// so "OCR is installed" is a question about *that* preset's vocabulary. A
/// machine holding only the english pair cannot read a Chinese screen, and
/// reporting it as capable would be the rung checking a file the loader never
/// opens.
pub fn local_models_present_for(configured_dir: &str, preset: OcrPreset) -> LocalModels {
    let dir = vision_model_dir(configured_dir);
    LocalModels {
        ui_detect: dir.join(OMNIPARSER_FILE).is_file(),
        // The recogniser is useless without its dictionary, so all three files
        // have to be there before OCR is claimed present — a partial download
        // must read as "absent", not as a run that fails halfway through.
        ocr: dir.join(DET_FILE).is_file()
            && dir.join(preset.rec_file()).is_file()
            && dir.join(preset.dict_file()).is_file(),
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
    ocr_model_paths_for(configured_dir, DEFAULT_OCR_PRESET)
}

/// Paths to one preset's OCR models under a directory, when all exist.
pub fn ocr_model_paths_for(
    configured_dir: &str,
    preset: OcrPreset,
) -> Option<(PathBuf, PathBuf, PathBuf)> {
    let dir = vision_model_dir(configured_dir);
    let det = dir.join(DET_FILE);
    let rec = dir.join(preset.rec_file());
    let dict = dir.join(preset.dict_file());
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
///
/// Follows the shared settings, like [`resolve_or_download_vision_models`]:
/// the files fetched are the configured preset's, not the default's.
pub async fn resolve_or_download_ocr_models() -> crate::computer::Result<OcrModelPaths> {
    let settings = shared_settings();
    let preset = OcrPreset::parse(&settings.ocr_preset()).unwrap_or(DEFAULT_OCR_PRESET);
    resolve_or_download_ocr_models_for(preset, &settings.model_dir()).await
}

/// Resolve (or download) one preset's OCR models under a directory.
pub async fn resolve_or_download_ocr_models_for(
    preset: OcrPreset,
    configured_dir: &str,
) -> crate::computer::Result<OcrModelPaths> {
    let dir = vision_model_dir(configured_dir);

    // Ensure data directory exists
    tokio::fs::create_dir_all(&dir).await.map_err(|e| {
        crate::computer::ComputerError::Other(format!(
            "Cannot create model data dir {}: {}",
            dir.display(),
            e
        ))
    })?;

    let det = resolve_single_model(&dir, DET_FILE, DET_URL).await?;
    let rec = resolve_single_model(&dir, preset.rec_file(), preset.rec_url()).await?;
    let rec_dict =
        resolve_single_model_mirrored(&dir, preset.dict_file(), preset.dict_urls()).await?;

    Ok(OcrModelPaths { det, rec, rec_dict })
}

/// Returns paths to all required model files.  Models that are already
/// stored are not re-downloaded.
///
/// Follows the shared settings: `[computer.vision].model_dir` and `.ocr`, so a
/// caller that has no config in hand — [`RapidOcr::new_auto`] behind
/// `screen_ocr` and the phone bridge — fetches the model the configuration
/// names rather than the default one.
///
/// [`RapidOcr::new_auto`]: crate::computer::vision::ocr_rapid::RapidOcr::new_auto
pub async fn resolve_or_download_vision_models() -> crate::computer::Result<VisionModelPaths> {
    let settings = shared_settings();
    let configured_dir = settings.model_dir();
    let preset = OcrPreset::parse(&settings.ocr_preset()).unwrap_or(DEFAULT_OCR_PRESET);
    let dir = vision_model_dir(&configured_dir);

    // Ensure data directory exists
    tokio::fs::create_dir_all(&dir).await.map_err(|e| {
        crate::computer::ComputerError::Other(format!(
            "Cannot create model data dir {}: {}",
            dir.display(),
            e
        ))
    })?;

    let omniparser = resolve_single_model(&dir, OMNIPARSER_FILE, OMNIPARSER_URL).await?;

    let OcrModelPaths { det, rec, rec_dict } =
        resolve_or_download_ocr_models_for(preset, &configured_dir).await?;

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
        f if f == OcrPreset::PpOcrV3En.rec_file() => "9",
        f if f == OcrPreset::PpOcrV3En.dict_file() => "<1",
        f if f == OcrPreset::PpOcrV3Ch.rec_file() => "10",
        f if f == OcrPreset::PpOcrV3Ch.dict_file() => "<1",
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
        assert_eq!(estimated_size(OcrPreset::PpOcrV3En.rec_file()), "9");
        assert_eq!(estimated_size(OcrPreset::PpOcrV3En.dict_file()), "<1");
        // Every offered preset must have a size, or the download prompt quotes
        // "?" for a model it is about to fetch.
        for preset in OcrPreset::all() {
            assert_ne!(estimated_size(preset.rec_file()), "?", "{}", preset.key());
            assert_ne!(estimated_size(preset.dict_file()), "?", "{}", preset.key());
        }
    }

    #[test]
    fn test_estimated_size_unknown() {
        assert_eq!(estimated_size("unknown.onnx"), "?");
    }

    /// The config spelling round-trips, and empty means the default rather than
    /// an error: a config written before the key existed carries no spelling.
    #[test]
    fn ocr_preset_parse_roundtrips() {
        for preset in OcrPreset::all() {
            assert_eq!(OcrPreset::parse(preset.key()), Some(*preset));
            assert_eq!(OcrPreset::parse(&preset.key().to_uppercase()), Some(*preset));
            assert_eq!(
                OcrPreset::parse(&preset.key().replace('-', "_")),
                Some(*preset),
                "a typed underscore must not be a different preset"
            );
        }
        assert_eq!(OcrPreset::parse(""), Some(DEFAULT_OCR_PRESET));
        assert_eq!(OcrPreset::parse("   "), Some(DEFAULT_OCR_PRESET));
    }

    /// An unknown name is `None`, never the default: the caller must be able to
    /// refuse it at write time instead of storing a setting that reads english.
    #[test]
    fn ocr_preset_parse_refuses_unknown() {
        assert_eq!(OcrPreset::parse("ppocrv4"), None);
        // The underscore is tolerated; a stray trailing space is not what the
        // tolerance is for, but trimming is what parse does — so the spelling
        // that must fail is a name no preset has.
        assert_eq!(OcrPreset::parse("ppocrv3-de"), None);
        assert_eq!(OcrPreset::parse("rec_ppocrv3_ch.onnx"), None);
    }

    /// The presets must not share a file: one cache directory holds both, so a
    /// single name would make "switch to chinese" load the english weights.
    #[test]
    fn every_preset_has_its_own_files() {
        let recs: Vec<&str> = OcrPreset::all().iter().map(|p| p.rec_file()).collect();
        let dicts: Vec<&str> = OcrPreset::all().iter().map(|p| p.dict_file()).collect();
        assert_eq!(recs.len(), 2, "this test compares the offered presets pairwise");
        assert_ne!(recs[0], recs[1]);
        assert_ne!(dicts[0], dicts[1]);
        for preset in OcrPreset::all() {
            assert_ne!(preset.rec_file(), preset.dict_file());
            assert!(!preset.label().is_empty());
        }
    }

    /// "OCR is installed" is a question about the *configured* preset's files.
    /// A machine holding only the english pair cannot read a Chinese screen,
    /// and reporting it as capable picks a rung whose load then fails.
    #[test]
    fn presence_is_per_preset() {
        let dir = tempfile::tempdir().expect("tempdir");
        // A configured directory is used as given: it *is* the models dir.
        let models = dir.path();
        for name in [
            DET_FILE,
            OcrPreset::PpOcrV3En.rec_file(),
            OcrPreset::PpOcrV3En.dict_file(),
        ] {
            std::fs::write(models.join(name), b"x").expect("write model");
        }
        let configured = models.to_string_lossy().to_string();

        assert!(local_models_present_for(&configured, OcrPreset::PpOcrV3En).ocr);
        assert!(
            !local_models_present_for(&configured, OcrPreset::PpOcrV3Ch).ocr,
            "the chinese pair is not installed, whatever the english one is"
        );
        assert!(ocr_model_paths_for(&configured, OcrPreset::PpOcrV3En).is_some());
        assert!(ocr_model_paths_for(&configured, OcrPreset::PpOcrV3Ch).is_none());
    }

    /// The four engine caches share this predicate; getting it wrong is either
    /// a silent wrong-language read (a mismatch treated as a match) or a model
    /// reload on every frame (a match treated as a mismatch).
    #[test]
    fn engine_matches_only_the_same_preset() {
        assert!(!engine_matches::<u8>(&None, OcrPreset::PpOcrV3En));
        assert!(!engine_matches::<u8>(&None, OcrPreset::PpOcrV3Ch));
        assert!(engine_matches(&Some((OcrPreset::PpOcrV3En, 1u8)), OcrPreset::PpOcrV3En));
        assert!(!engine_matches(&Some((OcrPreset::PpOcrV3En, 1u8)), OcrPreset::PpOcrV3Ch));
    }

    /// The settings handle is what a loader built at startup reads at use
    /// time; `replace` is therefore the whole mechanism by which a config edit
    /// takes effect without a restart. Tested on a private instance so a
    /// process-wide `install_settings` in another test cannot race this one.
    #[test]
    fn settings_replace_reaches_readers() {
        let settings = VisionSettings::new("", "auto");
        assert_eq!(settings.model_dir(), "");
        assert_eq!(settings.ocr_preset(), DEFAULT_OCR_PRESET.key());
        assert!(!settings.disabled());

        let mut config = crate::config::VisionConfig::default();
        config.model_dir = "/tmp/models".to_string();
        config.mode = "off".to_string();
        config.ocr = OcrPreset::PpOcrV3Ch.key().to_string();
        settings.replace(&config);
        assert_eq!(settings.model_dir(), "/tmp/models");
        assert_eq!(settings.ocr_preset(), "ppocrv3-ch");
        assert!(settings.disabled(), "mode off must reach a reader built earlier");
        assert_eq!(settings.model_path(), vision_model_dir("/tmp/models"));

        // `from_config` must agree with `replace` — the gateway builds one at
        // init and patches it afterwards, and a disagreement would mean the
        // startup value differs from every later one.
        let fresh = VisionSettings::from_config(&config);
        assert_eq!(fresh.model_dir(), settings.model_dir());
        assert_eq!(fresh.ocr_preset(), settings.ocr_preset());
        assert_eq!(fresh.disabled(), settings.disabled());
    }
}
