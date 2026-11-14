//! Who actually looks, once the resolver has said who may.
//!
//! [`VisionResolver`] answers a policy question — local model, API model, or
//! nobody — from configuration and what the machine has on disk. This module
//! is the other half: it holds the model handles and turns a screenshot into
//! **elements**, not prose.
//!
//! Why elements rather than "the button is at (812, 460)": the accessibility
//! tree's reliability comes from the model naming a control that can be
//! addressed again (`element_id`), and a visual scan that only reports pixels
//! throws that away. A local detector already answers with a role and a box,
//! so it is converted to the same [`UiElement`] shape the tree uses and given
//! the same kind of snapshot id. The one thing the run must do for it is
//! translate the id back to a point before dispatch — the adapter's snapshot
//! cache never held these elements — which
//! [`retarget`](crate::computer::run_executor::retarget) does.
//!
//! Two rules from the plan are load-bearing here:
//!
//! - **Never block the loop.** Every lookup is bounded by
//!   [`VisionResolver::timeout`]. A backend that misses it is treated as a
//!   failed rung, not as a reason to wait.
//! - **`local` is a promise.** The ladder is `local → api` only when the
//!   resolver says the fallback exists ([`VisionResolver::fallback_after`]);
//!   under `mode = "local"` a failure degrades to the tree, never to a network
//!   call.
//!
//! Nothing here downloads a model. A run is not the place to discover that an
//! 88 MB download is needed: [`VisionResolver::from_config`] is told whether
//! the file is already present, and a missing one simply means "this rung is
//! not available".

use std::sync::Arc;
use std::time::Duration;

use tokio::sync::Mutex;
use tracing::debug;

use crate::computer::display::{CoordinateSpace, DisplayGeometry};
use crate::computer::element_index::{assign_ids, next_snapshot_id, UiSnapshot};
use crate::computer::image_budget::{encode_or_passthrough, ImageLimit};
use crate::computer::run_budget::RunBudget;
use crate::computer::types::{Point, Rect, Screenshot, Tri, UiElement};
use crate::computer::vision::resolver::{
    LocalModel, UnavailableReason, VisionResolver, VisionSource,
};
use crate::providers::{CompletionRequest, Message, Provider};

/// How long loading a local model may take.
///
/// Deliberately *not* [`VisionResolver::timeout`]: that bounds a lookup, and a
/// lookup that has to read 12 MB of weights off disk once per process is a
/// different kind of wait. It is still bounded, because a corrupt model must
/// not hold a step open forever.
const MODEL_LOAD_TIMEOUT: Duration = Duration::from_secs(45);

/// Largest number of controls accepted from an API answer.
///
/// The prompt asks for at most this many; the cap here is because a model can
/// answer with more, and an outline of two hundred guessed boxes is worse than
/// no outline at all.
const MAX_API_ELEMENTS: usize = 20;

const VISION_MAX_TOKENS: u32 = 900;

/// The narrow question asked of an API vision model.
///
/// Deliberately not the main decision prompt with an image attached: that puts
/// the whole conversation, and every image in it, back on the bill. The answer
/// is a list of controls, which is exactly what a local detector produces, so
/// the two rungs feed the same downstream code.
const API_PROMPT: &str = "\
You look at a screenshot of a desktop to help an agent operate it.

List the interactive controls you can see — buttons, text fields, checkboxes, \
menus, links, icons that do something when clicked. Ignore decoration and plain \
text that cannot be clicked.

Answer with one JSON object and nothing else:
{\"elements\": [{\"role\": \"button\", \"label\": \"Save\", \"x\": 0, \"y\": 0, \"width\": 0, \"height\": 0}]}

The box is the control's whole visible extent in pixels of the image, with the \
origin at its top-left corner. \"label\" is the text on the control, or a few \
words naming it. List at most 20 controls, most relevant to the goal first. \
Answer {\"elements\": []} when the screenshot shows no interactive controls.";

/// A provider handle for the API rung, with the names the log needs.
pub struct ApiVision {
    pub provider: Arc<dyn Provider>,
    pub provider_name: String,
    pub model: String,
}

impl ApiVision {
    /// `provider/model`, for the run's vision status.
    pub fn describe(&self) -> String {
        format!("{}/{}", self.provider_name, self.model)
    }
}

/// What one visual lookup produced.
#[derive(Debug, Clone)]
pub struct VisionElements {
    /// `"local"` or `"api"` — where the elements came from.
    pub source: &'static str,
    /// The detected controls, with ids assigned.
    ///
    /// A [`UiSnapshot`] rather than a bare `Vec<UiElement>` so an id handed to
    /// the model can be resolved back to a point with the same code that
    /// resolves accessibility ids — one addressing scheme, not two.
    pub snapshot: UiSnapshot,
    /// Whether the boxes were mapped into screen points.
    ///
    /// `false` when display geometry was unavailable: the boxes are image
    /// pixels, so they are dropped (an element with no bounds cannot be
    /// clicked) and the model is told why. Guessing a 1:1 mapping is the
    /// Retina mis-click this codebase already spent an incident on.
    pub mapped: bool,
}

impl VisionElements {
    /// How many controls the scan added.
    pub fn len(&self) -> usize {
        self.snapshot.roots.len()
    }

    pub fn is_empty(&self) -> bool {
        self.snapshot.roots.is_empty()
    }

    /// The screen point an element id refers to, when it is one of ours.
    pub fn resolve(&self, id: &str) -> Option<Point> {
        self.snapshot.resolve_id(id)?.point
    }

    /// The sentence that goes into the decision prompt.
    pub fn note(&self) -> String {
        let source = match self.source {
            "local" => "a local visual scan",
            _ => "a vision model",
        };
        let mut note = format!(
            "( {source} added {} controls to the tree above; they were inferred from pixels, \
             so treat their roles and labels as guesses and verify them against the screen )",
            self.len()
        );
        if !self.mapped {
            note.push_str(
                " (their boxes could not be mapped to screen points — display geometry was \
                 unavailable — so aim at them by label, not by coordinates )",
            );
        }
        note
    }
}

/// What a lookup established.
pub enum LookOutcome {
    /// Somebody looked, and found controls.
    Found(VisionElements),
    /// Nobody can look, and why. Never a silent fallback.
    Unavailable(UnavailableReason),
    /// Somebody was supposed to look and could not.
    Failed { why: String },
}

impl LookOutcome {
    /// One line for the run's status, saying what the step was shown.
    pub fn status(&self) -> String {
        match self {
            LookOutcome::Found(found) => {
                format!("{}: {} controls", found.source, found.len())
            }
            LookOutcome::Unavailable(reason) => {
                format!("unavailable: {}", reason.explain())
            }
            LookOutcome::Failed { why } => format!("failed: {why}"),
        }
    }
}

/// The rung handles, plus the policy that chose between them.
pub struct VisionLook {
    resolver: VisionResolver,
    api: Option<ApiVision>,
    image_limit: ImageLimit,
    #[cfg(feature = "vision")]
    detector: SharedDetector,
}

#[cfg(feature = "vision")]
type SharedDetector = Arc<Mutex<Option<crate::computer::vision::ui_onnx::OmniParserDetector>>>;

/// The process-wide detector, loaded at most once.
///
/// Shared rather than per-run because loading the weights costs seconds and a
/// gateway runs goals back to back; per-run would pay it every time. A failed
/// load leaves the slot empty, so the next run tries again instead of the
/// process remembering a transient failure forever.
#[cfg(feature = "vision")]
fn shared_detector() -> SharedDetector {
    use std::sync::OnceLock;
    static SHARED: OnceLock<SharedDetector> = OnceLock::new();
    SHARED.get_or_init(|| Arc::new(Mutex::new(None))).clone()
}

impl VisionLook {
    pub fn new(resolver: VisionResolver, api: Option<ApiVision>) -> Self {
        Self {
            resolver,
            api,
            image_limit: ImageLimit::default(),
            #[cfg(feature = "vision")]
            detector: shared_detector(),
        }
    }

    /// Size and format for the image sent to an API vision model.
    ///
    /// The same ceiling the decision prompt uses: the constraint is the model's
    /// context, not a network link, so both images are sized by the same knob.
    pub fn with_image_limit(mut self, limit: ImageLimit) -> Self {
        self.image_limit = limit;
        self
    }

    pub fn resolver(&self) -> &VisionResolver {
        &self.resolver
    }

    /// Which rung would answer right now, without looking.
    pub fn source(&self) -> VisionSource {
        self.resolver.resolve()
    }

    /// Look at `shot` and return the controls on it.
    ///
    /// The ladder is walked once: the resolved rung, then its configured
    /// fallback if it fails. `budget` is charged for an API call *before* the
    /// request, because a vision call is a model call and the run's cost
    /// ceiling is the only thing that can refuse it.
    pub async fn look(
        &self,
        shot: &Screenshot,
        goal: &str,
        geometry: &DisplayGeometry,
        budget: &Arc<Mutex<RunBudget>>,
    ) -> LookOutcome {
        let source = self.resolver.resolve();
        if let VisionSource::Unavailable(reason) = source {
            debug!("no vision backend: {}", reason.explain());
            return LookOutcome::Unavailable(reason);
        }

        let why = match self.attempt(&source, shot, goal, geometry, budget).await {
            Ok(found) => return LookOutcome::Found(found),
            Err(why) => why,
        };

        match self.resolver.fallback_after(&source) {
            Some(fallback) => match self.attempt(&fallback, shot, goal, geometry, budget).await {
                Ok(found) => {
                    debug!(
                        "{} could not look ({why}); {} answered instead",
                        describe(&source),
                        describe(&fallback)
                    );
                    LookOutcome::Found(found)
                }
                Err(fallback_why) => LookOutcome::Failed {
                    why: format!(
                        "{} failed ({why}) and {} failed ({fallback_why})",
                        describe(&source),
                        describe(&fallback)
                    ),
                },
            },
            None => LookOutcome::Failed { why },
        }
    }

    async fn attempt(
        &self,
        source: &VisionSource,
        shot: &Screenshot,
        goal: &str,
        geometry: &DisplayGeometry,
        budget: &Arc<Mutex<RunBudget>>,
    ) -> Result<VisionElements, String> {
        match source {
            VisionSource::Unavailable(reason) => Err(reason.explain().to_string()),
            VisionSource::Local(_) => {
                let elements = self.local(shot).await?;
                Ok(self.finish(elements, shot, geometry, "local"))
            }
            VisionSource::Api { .. } => {
                // Charge the call before making it. A vision call is a model
                // call; letting it bypass the ceiling would make the ceiling
                // untrue exactly when it matters.
                {
                    let mut b = budget.lock().await;
                    if let Err(exhausted) = b.record_call() {
                        return Err(format!("{exhausted}"));
                    }
                }
                let elements = self.api_look(shot, goal).await?;
                Ok(self.finish(elements, shot, geometry, "api"))
            }
        }
    }

    /// Convert image-space controls into the canonical element form.
    ///
    /// The mapping happens here, once, so every rung produces the same
    /// contract: `bounds` are screen points, which is what a click uses.
    fn finish(
        &self,
        elements: Vec<UiElement>,
        shot: &Screenshot,
        geometry: &DisplayGeometry,
        source: &'static str,
    ) -> VisionElements {
        let space = CoordinateSpace::from_screenshot(shot, geometry);
        let mut roots = elements;
        match &space {
            Some(space) => {
                for element in &mut roots {
                    if let Some(bounds) = element.bounds {
                        element.bounds = Some(map_rect(space, bounds));
                    }
                }
            }
            None => {
                // An image pixel is not a screen point unless the capture and
                // the click space are known to coincide. Dropping the box keeps
                // the control nameable by label and makes it *unresolvable* by
                // id, which is the honest outcome — a click derived from an
                // unverified mapping is a click in the wrong place.
                for element in &mut roots {
                    element.bounds = None;
                }
            }
        }

        let id = next_snapshot_id();
        assign_ids(&mut roots, id);
        VisionElements {
            source,
            snapshot: UiSnapshot { id, app: None, roots },
            mapped: space.is_some(),
        }
    }

    /// The local ONNX rung.
    #[cfg(feature = "vision")]
    async fn local(&self, shot: &Screenshot) -> Result<Vec<UiElement>, String> {
        use crate::computer::vision::ui_onnx::OmniParserDetector;

        let mut detector = self.detector_handle().await?;
        let shared = self.detector.clone();
        let shot = shot.clone();
        // On its own task, not awaited inline: `detect_elements` is synchronous
        // ONNX work despite its async signature, so inline it would hold the
        // whole loop for as long as inference takes and the timeout below
        // could never fire. The task returns the detector to the cache even
        // when the step above has given up on it.
        let task = tokio::spawn(async move {
            let outcome = detector.detect_elements(&shot).await;
            *shared.lock().await = Some(detector);
            outcome
        });

        let detected = match tokio::time::timeout(self.resolver.timeout(), task).await {
            Ok(Ok(Ok(detected))) => detected,
            Ok(Ok(Err(e))) => return Err(format!("local detection failed: {e}")),
            Ok(Err(e)) => return Err(format!("local detection task failed: {e}")),
            Err(_) => {
                return Err(format!(
                    "local detection did not finish within {} s",
                    self.resolver.timeout().as_secs()
                ))
            }
        };
        if detected.is_empty() {
            // Not a hard failure — the screen may genuinely hold nothing —
            // but it is an answer that does not help, and the ladder has a
            // second rung for exactly this.
            return Err("the local model found no controls on this screen".to_string());
        }
        Ok(OmniParserDetector::to_ui_elements(detected))
    }

    /// A build without the `vision` feature cannot run a local model.
    #[cfg(not(feature = "vision"))]
    async fn local(&self, _shot: &Screenshot) -> Result<Vec<UiElement>, String> {
        Err("this build has no 'vision' feature, so no local model can run".to_string())
    }

    /// Take the cached detector, loading it if this is the first use.
    #[cfg(feature = "vision")]
    async fn detector_handle(
        &self,
    ) -> Result<crate::computer::vision::ui_onnx::OmniParserDetector, String> {
        use crate::computer::vision::model_download::vision_model_dir;
        use crate::computer::vision::ui_onnx::OmniParserDetector;

        {
            let mut guard = self.detector.lock().await;
            if let Some(detector) = guard.take() {
                return Ok(detector);
            }
        }

        let path = vision_model_dir(&self.resolver.config().model_dir).join("omniparser.onnx");
        if !path.exists() {
            // A run never downloads: the resolver was told whether the file is
            // present, so reaching here means the file went away or the
            // resolver was built with a stale answer. Either way the rung is
            // unavailable, not a reason to start an 88 MB download mid-step.
            return Err(format!("no local model at {}", path.display()));
        }

        let path_string = path.to_string_lossy().to_string();
        let loaded = tokio::time::timeout(
            MODEL_LOAD_TIMEOUT,
            tokio::task::spawn_blocking(move || OmniParserDetector::new(&path_string)),
        )
        .await
        .map_err(|_| {
            format!(
                "loading {} did not finish within {} s",
                path.display(),
                MODEL_LOAD_TIMEOUT.as_secs()
            )
        })?
        .map_err(|e| format!("the model load task failed: {e}"))?
        .map_err(|e| format!("could not load {}: {e}", path.display()))?;

        debug!("loaded the local vision model from {}", path.display());
        Ok(loaded)
    }

    /// The API rung: one narrow question, one JSON answer.
    async fn api_look(&self, shot: &Screenshot, goal: &str) -> Result<Vec<UiElement>, String> {
        let Some(api) = self.api.as_ref() else {
            return Err("no provider is configured for the API vision rung".to_string());
        };
        if shot.base64.is_empty() {
            return Err("the capture carries no image to send".to_string());
        }
        use base64::Engine as _;
        let raw = base64::engine::general_purpose::STANDARD
            .decode(&shot.base64)
            .map_err(|e| format!("the capture is not valid base64: {e}"))?;
        let encoded = encode_or_passthrough(&raw, &self.image_limit)
            .map_err(|e| format!("could not prepare the screenshot: {e}"))?;

        let request = CompletionRequest {
            messages: vec![
                Message::system(API_PROMPT),
                Message::user(format!(
                    "The agent's goal: {goal}\nList the screen's interactive controls."
                ))
                .with_image(encoded.base64, encoded.mime),
            ],
            // Where a control is must not depend on the sampling temperature.
            temperature: Some(0.0),
            max_tokens: Some(VISION_MAX_TOKENS),
            stream: false,
            model: Some(api.model.clone()),
            requires_vision: true,
            ..Default::default()
        };

        let response =
            match tokio::time::timeout(self.resolver.timeout(), api.provider.complete(request))
                .await
            {
                Ok(Ok(response)) => response,
                Ok(Err(e)) => return Err(format!("{} could not answer: {e}", api.provider_name)),
                Err(_) => {
                    return Err(format!(
                        "{} did not answer within {} s",
                        api.provider_name,
                        self.resolver.timeout().as_secs()
                    ))
                }
            };
        parse_api_elements(&response.message.content)
    }
}

fn describe(source: &VisionSource) -> String {
    match source {
        VisionSource::Local(LocalModel::Ocr) => "the local OCR model".to_string(),
        VisionSource::Local(LocalModel::UiDetect) => "the local UI detector".to_string(),
        VisionSource::Api { provider, model } => format!("the API model {provider}/{model}"),
        VisionSource::Unavailable(reason) => format!("no backend ({})", reason.explain()),
    }
}

/// Map an image-pixel box into screen points.
fn map_rect(space: &CoordinateSpace, bounds: Rect) -> Rect {
    let (left, top) = space.image_to_screen(bounds.x, bounds.y);
    let (right, bottom) = space.image_to_screen(
        bounds.x.saturating_add(bounds.width as i32),
        bounds.y.saturating_add(bounds.height as i32),
    );
    Rect::new(left, top, (right - left).max(1) as u32, (bottom - top).max(1) as u32)
}

/// Read the API answer into elements.
///
/// Image-space bounds; the caller maps them once. A malformed entry is skipped
/// rather than failing the answer — one unreadable box must not throw away
/// nineteen good ones — but an answer with no usable box at all is an error,
/// because "the model saw nothing" and "the model's answer was unusable" lead
/// to different next steps.
fn parse_api_elements(content: &str) -> Result<Vec<UiElement>, String> {
    let object = crate::computer::driver::extract_json_object(content)
        .ok_or_else(|| "the vision model answered without a JSON object".to_string())?;
    let value: serde_json::Value = serde_json::from_str(&object)
        .map_err(|e| format!("the vision model's answer is not JSON: {e}"))?;
    let list = value
        .get("elements")
        .and_then(|v| v.as_array())
        .ok_or_else(|| "the vision model's answer has no `elements` list".to_string())?;

    let mut elements = Vec::new();
    for item in list.iter().take(MAX_API_ELEMENTS) {
        let number = |key: &str| item.get(key).and_then(|v| v.as_i64());
        let (Some(x), Some(y), Some(width), Some(height)) =
            (number("x"), number("y"), number("width"), number("height"))
        else {
            continue;
        };
        let (Ok(x), Ok(y)) = (i32::try_from(x), i32::try_from(y)) else {
            continue;
        };
        let (Ok(width), Ok(height)) = (u32::try_from(width), u32::try_from(height)) else {
            continue;
        };
        if width == 0 || height == 0 {
            continue;
        }
        let label = item
            .get("label")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|l| !l.is_empty())
            .map(str::to_string);
        let role = item
            .get("role")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|r| !r.is_empty())
            .unwrap_or("control")
            .to_string();
        elements.push(UiElement {
            id: String::new(),
            role,
            label,
            value: None,
            bounds: Some(Rect::new(x, y, width, height)),
            // The same reason `to_ui_elements` answers `Unknown`: a picture
            // does not say whether a control accepts input, and claiming
            // `enabled` would turn a guess into a pre-check that passes.
            enabled: Tri::Unknown,
            focused: Tri::Unknown,
            actions: None,
            children: Vec::new(),
            children_count: None,
            truncated: false,
        });
    }
    if elements.is_empty() {
        return Err("the vision model named no usable controls".to_string());
    }
    Ok(elements)
}

/// Constructors for other modules' tests.
///
/// A scan has two halves — a payload and the coordinate mapping that produced
/// it — and a caller that wants to test how a scan is *rendered or dispatched*
/// should not have to run a provider to get one. Kept out of the public API so
/// no production path can build a scan without going through [`VisionLook`].
#[cfg(test)]
pub(crate) mod test_support {
    use super::*;

    /// A finished scan, standing in for what [`VisionLook::look`] returns.
    pub(crate) type Scan = VisionElements;

    /// A look backed by a scripted API provider, answering `answer` verbatim.
    ///
    /// This is the same path a real API rung takes — parse, map, charge — so a
    /// run test exercises the code that ships rather than a stub of it.
    pub(crate) fn api_look(answer: &str) -> Arc<VisionLook> {
        use crate::providers::{CompletionResponse, CompletionStream};

        struct Scripted(String);

        #[async_trait::async_trait]
        impl Provider for Scripted {
            fn name(&self) -> &str {
                "scripted-vision"
            }
            fn default_model(&self) -> &str {
                "scripted-vision-model"
            }
            fn supports_tools(&self) -> bool {
                false
            }
            fn max_context(&self) -> usize {
                8_192
            }
            async fn complete(
                &self,
                _request: CompletionRequest,
            ) -> crate::Result<CompletionResponse> {
                Ok(CompletionResponse {
                    message: Message::assistant(self.0.clone()),
                    usage: None,
                    model: "scripted-vision-model".to_string(),
                    finish_reason: None,
                })
            }
            async fn stream(&self, _request: CompletionRequest) -> crate::Result<CompletionStream> {
                Ok(Box::pin(tokio_stream::iter(vec![])))
            }
            async fn health_check(&self) -> crate::Result<bool> {
                Ok(true)
            }
            async fn set_credential(
                &self,
                _credential: crate::model_router::Credential,
            ) -> crate::Result<()> {
                Ok(())
            }
        }

        let resolver = VisionResolver::new(
            crate::config::VisionConfig {
                mode: "api".to_string(),
                provider: Some("scripted-vision".to_string()),
                model: Some("scripted-vision-model".to_string()),
                ..crate::config::VisionConfig::default()
            },
            crate::computer::vision::resolver::Availability {
                local_model: false,
                feature_enabled: true,
            },
        );
        Arc::new(VisionLook::new(
            resolver,
            Some(ApiVision {
                provider: Arc::new(Scripted(answer.to_string())),
                provider_name: "scripted-vision".to_string(),
                model: "scripted-vision-model".to_string(),
            }),
        ))
    }

    /// A look that can see nothing: `mode = "off"`.
    pub(crate) fn off_look() -> Arc<VisionLook> {
        let resolver = VisionResolver::new(
            crate::config::VisionConfig {
                mode: "off".to_string(),
                ..crate::config::VisionConfig::default()
            },
            crate::computer::vision::resolver::Availability {
                local_model: false,
                feature_enabled: true,
            },
        );
        Arc::new(VisionLook::new(resolver, None))
    }

    /// One control with a label, at a box already in screen points.
    pub(crate) fn scan(label: &str, (x, y, width, height): (i32, i32, u32, u32)) -> Scan {
        let mut roots = vec![UiElement {
            id: String::new(),
            role: "button".to_string(),
            label: Some(label.to_string()),
            value: None,
            bounds: Some(Rect::new(x, y, width, height)),
            enabled: Tri::Unknown,
            focused: Tri::Unknown,
            actions: None,
            children: Vec::new(),
            children_count: None,
            truncated: false,
        }];
        let id = next_snapshot_id();
        assign_ids(&mut roots, id);
        Scan {
            source: "local",
            snapshot: UiSnapshot { id, app: None, roots },
            mapped: true,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::computer::display::DisplayInfo;
    use crate::providers::{CompletionResponse, CompletionStream};
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// A one-pixel PNG: the API rung refuses a capture with no bytes, so an
    /// empty payload would make every API assertion vacuous.
    const ONE_PIXEL_PNG: &str = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAIAAACQd1PeAAAADElEQVR4nGP4z8AAAAMBAQDJ/pLvAAAAAElFTkSuQmCC";

    fn shot(width: u32, height: u32) -> Screenshot {
        Screenshot::new(ONE_PIXEL_PNG.to_string(), width, height)
    }

    fn geometry() -> DisplayGeometry {
        DisplayGeometry::new(vec![DisplayInfo::new(0, 0, 100, 100).primary()])
    }

    fn unlimited() -> Arc<Mutex<RunBudget>> {
        Arc::new(Mutex::new(RunBudget::unlimited()))
    }

    fn resolver(mode: &str, api: bool, local: bool) -> VisionResolver {
        VisionResolver::new(
            crate::config::VisionConfig {
                mode: mode.to_string(),
                provider: api.then(|| "openai".to_string()),
                model: api.then(|| "gpt-4o".to_string()),
                ..crate::config::VisionConfig::default()
            },
            crate::computer::vision::resolver::Availability {
                local_model: local,
                feature_enabled: true,
            },
        )
    }

    /// Counts calls and answers with a fixed string.
    struct CountingProvider {
        answer: String,
        calls: AtomicUsize,
    }

    impl CountingProvider {
        fn new(answer: &str) -> Arc<Self> {
            Arc::new(Self {
                answer: answer.to_string(),
                calls: AtomicUsize::new(0),
            })
        }

        fn calls(&self) -> usize {
            self.calls.load(Ordering::SeqCst)
        }
    }

    #[async_trait::async_trait]
    impl Provider for CountingProvider {
        fn name(&self) -> &str {
            "counting"
        }

        fn default_model(&self) -> &str {
            "count-model"
        }

        fn supports_tools(&self) -> bool {
            false
        }

        fn max_context(&self) -> usize {
            8_192
        }

        async fn complete(&self, _request: CompletionRequest) -> crate::Result<CompletionResponse> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(CompletionResponse {
                message: Message::assistant(self.answer.clone()),
                usage: None,
                model: "count-model".to_string(),
                finish_reason: None,
            })
        }

        async fn stream(&self, _request: CompletionRequest) -> crate::Result<CompletionStream> {
            Ok(Box::pin(tokio_stream::iter(vec![])))
        }

        async fn health_check(&self) -> crate::Result<bool> {
            Ok(true)
        }

        async fn set_credential(
            &self,
            _credential: crate::model_router::Credential,
        ) -> crate::Result<()> {
            Ok(())
        }
    }

    fn api(
        provider_name: &str,
        model: &str,
        answer: &str,
    ) -> (Option<ApiVision>, Arc<CountingProvider>) {
        let provider = CountingProvider::new(answer);
        let handle = ApiVision {
            provider: provider.clone(),
            provider_name: provider_name.to_string(),
            model: model.to_string(),
        };
        (Some(handle), provider)
    }

    const ONE_CONTROL: &str = r#"{"elements": [{"role": "button", "label": "Save", "x": 10, "y": 20, "width": 30, "height": 10}]}"#;

    /// `off` is the "no vision" setting, and it must not reach the network —
    /// the counter is the assertion because "no request was made" cannot be
    /// seen from the return value.
    #[tokio::test]
    async fn off_never_asks_anybody() {
        let (api, provider) = api("openai", "gpt-4o", ONE_CONTROL);
        let look = VisionLook::new(resolver("off", true, true), api);

        let outcome = look
            .look(&shot(100, 100), "open the calculator", &geometry(), &unlimited())
            .await;

        assert!(matches!(outcome, LookOutcome::Unavailable(UnavailableReason::DisabledByConfig)));
        assert_eq!(provider.calls(), 0, "mode = off made a network call");
    }

    /// `local` is the privacy setting: a missing local model degrades to
    /// nothing, never to the API it was chosen instead of.
    #[tokio::test]
    async fn local_without_a_model_never_falls_through_to_the_api() {
        let (api, provider) = api("openai", "gpt-4o", ONE_CONTROL);
        let look = VisionLook::new(resolver("local", true, false), api);

        let outcome = look
            .look(&shot(100, 100), "open the calculator", &geometry(), &unlimited())
            .await;

        assert!(matches!(
            outcome,
            LookOutcome::Unavailable(UnavailableReason::LocalModelMissing)
        ));
        assert_eq!(provider.calls(), 0, "mode = local called the API");
    }

    /// `auto` with a local model that cannot answer: the API is the next rung,
    /// and its answer comes back as elements.
    #[tokio::test]
    async fn auto_falls_back_to_the_api_when_local_fails() {
        let (api, provider) = api("openai", "gpt-4o", ONE_CONTROL);
        // A resolver that believes a local model exists, with no model file on
        // disk: the local attempt fails to load and the ladder moves on. This
        // is also the case where a run must not start a download.
        let mut resolver = resolver("auto", true, true);
        let mut config = resolver.config().clone();
        config.model_dir = std::env::temp_dir()
            .join(format!("syscity-vision-empty-{}", std::process::id()))
            .to_string_lossy()
            .to_string();
        // `config()` is a getter; rebuild with the empty directory instead.
        resolver = VisionResolver::new(
            config,
            crate::computer::vision::resolver::Availability {
                local_model: true,
                feature_enabled: cfg!(feature = "vision"),
            },
        );
        let look = VisionLook::new(resolver, api);

        let outcome = look
            .look(&shot(100, 100), "open the calculator", &geometry(), &unlimited())
            .await;

        match outcome {
            LookOutcome::Found(found) => {
                assert_eq!(found.source, "api");
                assert_eq!(found.len(), 1);
            }
            LookOutcome::Unavailable(reason) => {
                // A build without the vision feature cannot attempt the local
                // rung at all, so the fallback would be unreachable rather
                // than broken. Fail loudly instead of pretending it passed.
                panic!("auto should reach the API rung, got unavailable: {}", reason.explain());
            }
            LookOutcome::Failed { why } => panic!("auto should reach the API rung, got: {why}"),
        }
        assert_eq!(provider.calls(), 1);
    }

    /// Both rungs failing is reported as a failure with both reasons, not as a
    /// quiet "nothing found".
    #[tokio::test]
    async fn a_failing_api_after_a_failing_local_rung_is_reported() {
        let (api, provider) = api("openai", "gpt-4o", "I cannot see any controls.");
        let mut config = crate::config::VisionConfig {
            mode: "auto".to_string(),
            provider: Some("openai".to_string()),
            model: Some("gpt-4o".to_string()),
            ..crate::config::VisionConfig::default()
        };
        config.model_dir = std::env::temp_dir()
            .join(format!("syscity-vision-empty-{}", std::process::id()))
            .to_string_lossy()
            .to_string();
        let resolver = VisionResolver::new(
            config,
            crate::computer::vision::resolver::Availability {
                local_model: true,
                feature_enabled: cfg!(feature = "vision"),
            },
        );
        let look = VisionLook::new(resolver, api);

        let outcome = look
            .look(&shot(100, 100), "open the calculator", &geometry(), &unlimited())
            .await;

        match outcome {
            LookOutcome::Failed { why } => {
                assert!(
                    why.contains("API") || why.contains("openai") || why.contains("no usable"),
                    "the reason names what failed: {why}"
                );
            }
            other => panic!("expected a failure, got {}", other.status()),
        }
        assert_eq!(provider.calls(), 1, "the API rung was tried exactly once");
    }

    /// The API rung is a model call, and a model call is what the run's
    /// ceiling counts. Without this the budget would be untrue on exactly the
    /// steps that cost the most.
    #[tokio::test]
    async fn an_api_lookup_is_charged_to_the_run_budget() {
        let (api, provider) = api("openai", "gpt-4o", ONE_CONTROL);
        let look = VisionLook::new(resolver("api", true, false), api);
        let budget = Arc::new(Mutex::new(RunBudget::new(crate::config::RunBudgetConfig {
            max_calls_per_run: 1,
            max_images_per_run: 5,
        })));

        let outcome = look
            .look(&shot(100, 100), "open the calculator", &geometry(), &budget)
            .await;
        assert!(matches!(outcome, LookOutcome::Found(_)));
        assert_eq!(budget.lock().await.model_calls(), 1);

        // The same run cannot pay for a second one.
        let again = look
            .look(&shot(100, 100), "open the calculator", &geometry(), &budget)
            .await;
        assert!(matches!(again, LookOutcome::Failed { .. }), "{}", again.status());
        assert_eq!(provider.calls(), 1);
    }

    /// The contract the plan asks for: a detected control gets an id that the
    /// same snapshot can resolve, so the visual path and the tree path are
    /// isomorphic.
    #[test]
    fn detected_controls_carry_resolvable_ids() {
        let (api, _provider) = api("openai", "gpt-4o", ONE_CONTROL);
        let look = VisionLook::new(resolver("api", true, false), api);
        let detected = parse_api_elements(ONE_CONTROL).expect("the answer parses");
        let found = look.finish(detected, &shot(100, 100), &geometry(), "api");

        assert_eq!(found.len(), 1);
        let element = &found.snapshot.roots[0];
        assert!(!element.id.is_empty(), "a control must be addressable by id");
        let point = found
            .resolve(&element.id)
            .expect("the id resolves inside the snapshot it came from");
        // Image pixel (10,20)+(30x10) on a 100x100 -> 100x100 display: identity.
        assert_eq!(point, Point::new(25, 25));
    }

    /// Retina: a 200x200 image over a 100x100 point display. The box must move
    /// with the pixels, or the click lands at twice the distance from the
    /// origin — the mis-click this mapping exists to prevent.
    #[test]
    fn boxes_are_mapped_from_image_pixels_to_screen_points() {
        let (api, _provider) = api("openai", "gpt-4o", ONE_CONTROL);
        let look = VisionLook::new(resolver("api", true, false), api);
        let detected = parse_api_elements(ONE_CONTROL).expect("the answer parses");
        let found = look.finish(detected, &shot(200, 200), &geometry(), "api");

        let element = &found.snapshot.roots[0];
        let bounds = element.bounds.expect("the box survived");
        assert_eq!(bounds, Rect::new(5, 10, 15, 5), "half the pixels: 2x scale");
        assert!(found.mapped);
    }

    /// Without display geometry there is no honest mapping, so the box is
    /// dropped: the control can still be named by label, but an id derived
    /// from an unverified mapping resolves to nothing.
    #[test]
    fn without_geometry_a_control_has_no_clickable_box() {
        let (api, _provider) = api("openai", "gpt-4o", ONE_CONTROL);
        let look = VisionLook::new(resolver("api", true, false), api);
        let detected = parse_api_elements(ONE_CONTROL).expect("the answer parses");
        let found = look.finish(detected, &shot(100, 100), &DisplayGeometry::default(), "api");

        let element = &found.snapshot.roots[0];
        assert!(element.bounds.is_none());
        assert!(!found.mapped);
        assert!(found.note().contains("display geometry was unavailable"));
        // It is still there, still labelled, still countable.
        assert_eq!(element.label.as_deref(), Some("Save"));
    }

    #[test]
    fn a_unusable_api_answer_is_an_error_not_an_empty_screen() {
        assert!(parse_api_elements("no json here").is_err());
        assert!(parse_api_elements(r#"{"elements": []}"#).is_err());
        // One good entry among bad ones keeps the good one.
        let mixed = r#"{"elements": [
            {"role": "button", "label": "OK", "x": 1, "y": 2, "width": 3, "height": 4},
            {"role": "button", "label": "Half", "x": 1, "y": 2},
            {"role": "button", "label": "Empty", "x": 0, "y": 0, "width": 0, "height": 9}
        ]}"#;
        let parsed = parse_api_elements(mixed).expect("one usable control");
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0].label.as_deref(), Some("OK"));
    }

    /// The note is what the deciding model reads, so it must say the source and
    /// that the labels are guesses.
    #[test]
    fn the_note_names_the_source_and_its_uncertainty() {
        let (api, _provider) = api("openai", "gpt-4o", ONE_CONTROL);
        let look = VisionLook::new(resolver("api", true, false), api);
        let detected = parse_api_elements(ONE_CONTROL).expect("the answer parses");
        let found = look.finish(detected, &shot(100, 100), &geometry(), "local");
        let note = found.note();
        assert!(note.contains("local visual scan"), "{note}");
        assert!(note.contains("guesses"), "{note}");
    }
}
