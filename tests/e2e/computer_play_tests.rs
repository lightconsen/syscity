//! Record a task once, then replay it with different inputs.
//!
//! Stage E's verification, from the plan: record "open Finder → new folder →
//! name it", change the parameter, replay, and assert the two runs did the
//! same thing modulo the bound value.
//!
//! Everything is hermetic. The recording is driven the way a person would
//! drive it — `computer.record_start`, then an ordinary chat turn whose model
//! calls the `computer` tool, then `computer.record_stop` — so this also pins
//! the wiring the plan depends on: the tool an agent turn calls and the
//! recording a WS method starts are two objects sharing one handle. A fake
//! desktop supplies the screen, the tree and the action log; no real window is
//! touched and no real screenshot is taken.
//!
//! The fake changes its capture on every action and updates the named field on
//! a `Type`, which is what makes each step verify as `ok`. Without that, the
//! replay's default `skip` strategy would turn steps that actually worked into
//! `Skipped`, and the assertion would be about the strategy instead of about
//! the workflow.

use std::sync::Mutex;

use async_trait::async_trait;

use syscity::computer::display::DisplayGeometry;
use syscity::computer::types::{ClickTarget, Point, Rect, Screenshot, UiElement};
use syscity::computer::{
    ActionResult, ActionTarget, ComputerAdapter, ComputerError, DesktopAction, ResolvedElement,
    Tri, WaitCondition,
};
use syscity::planner::workflow::{WorkflowAction, WorkflowStore};
use syscity::tools::computer::ComputerTool;

use super::*;

/// The screen and the tree a Finder window would have, and the record of what
/// was asked of it.
#[derive(Default)]
struct FakeState {
    /// Bumped by every action, so the post-action capture differs from the
    /// baseline and the step's diff verifies.
    tick: usize,
    /// The value of the `Name` field, so `type` has a visible effect in the
    /// tree and `typed_value_hint` can confirm it.
    folder_name: String,
    /// Every `Type` action's text, in order — the evidence that the replay
    /// typed the value it was bound to.
    typed: Vec<String>,
}

/// A desktop with one window, one button and one text field.
struct FinderDesktop {
    state: Mutex<FakeState>,
}

impl FinderDesktop {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            state: Mutex::new(FakeState::default()),
        })
    }

    fn typed(&self) -> Vec<String> {
        self.state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .typed
            .clone()
    }

    fn field_value(&self) -> String {
        self.state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .folder_name
            .clone()
    }

    /// The two elements a recorded and replayed target can name.
    fn element(id: &str) -> Option<(&'static str, &'static str, Point)> {
        match id {
            "btn-new-folder" => Some(("button", "New Folder", Point::new(450, 55))),
            "field-name" => Some(("text_field", "Name", Point::new(500, 95))),
            _ => None,
        }
    }
}

#[async_trait]
impl ComputerAdapter for FinderDesktop {
    async fn screenshot(&self, _region: Option<Rect>) -> syscity::computer::Result<Screenshot> {
        // Two frames that differ at nearly every byte. This is not a decodable
        // image, so `pixel_diff` falls back to sampling bytes directly — and a
        // pair that differed only in a short prefix would sample as identical,
        // turning every action into a "no visible effect" and sending the
        // ladder off to its keyboard rung.
        let odd = self.state.lock().unwrap_or_else(|e| e.into_inner()).tick % 2 == 1;
        let frame = if odd {
            "B".repeat(1200)
        } else {
            "A".repeat(1200)
        };
        Ok(Screenshot::new(frame, 100, 100))
    }

    async fn read_ui_tree(&self, _app: Option<&str>) -> syscity::computer::Result<Vec<UiElement>> {
        let value = self.field_value();
        Ok(vec![UiElement {
            id: "win-finder".to_string(),
            role: "window".to_string(),
            label: Some("Finder".to_string()),
            value: None,
            bounds: Some(Rect::new(0, 0, 800, 600)),
            enabled: Tri::Known(true),
            focused: Tri::Known(true),
            actions: None,
            children: vec![
                UiElement {
                    id: "btn-new-folder".to_string(),
                    role: "button".to_string(),
                    label: Some("New Folder".to_string()),
                    value: None,
                    bounds: Some(Rect::new(400, 40, 100, 30)),
                    enabled: Tri::Known(true),
                    focused: Tri::Known(false),
                    actions: None,
                    children: Vec::new(),
                    children_count: None,
                    truncated: false,
                },
                UiElement {
                    id: "field-name".to_string(),
                    role: "text_field".to_string(),
                    label: Some("Name".to_string()),
                    value: Some(value),
                    bounds: Some(Rect::new(400, 80, 200, 30)),
                    enabled: Tri::Known(true),
                    focused: Tri::Known(false),
                    actions: None,
                    children: Vec::new(),
                    children_count: None,
                    truncated: false,
                },
            ],
            children_count: None,
            truncated: false,
        }])
    }

    async fn execute(&self, action: DesktopAction) -> syscity::computer::Result<ActionResult> {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        state.tick += 1;
        match action {
            DesktopAction::LaunchApp { name, .. } => {
                Ok(ActionResult::success(format!("launched {name}")))
            }
            DesktopAction::Click { .. } => Ok(ActionResult::success("clicked")),
            DesktopAction::Type { text, .. } => {
                state.folder_name = text.clone();
                state.typed.push(text.clone());
                Ok(ActionResult::success(format!("typed {text:?}")))
            }
            _ => Ok(ActionResult::success("ok")),
        }
    }

    async fn wait_for(
        &self,
        _condition: WaitCondition,
        _timeout: Duration,
    ) -> syscity::computer::Result<bool> {
        Ok(true)
    }

    async fn resolve_target(
        &self,
        target: &ClickTarget,
    ) -> syscity::computer::Result<ActionTarget> {
        // Both spellings a target can take after a recording: the id the
        // model used, and the role+label a replay is recorded with.
        let named = match target {
            ClickTarget::ElementId(id) => Some(id.clone()),
            ClickTarget::ElementLabel(label) => Some(match label.as_str() {
                "New Folder" => "btn-new-folder".to_string(),
                "Name" => "field-name".to_string(),
                other => other.to_string(),
            }),
            ClickTarget::ElementRoleLabel { label, .. } => Some(match label.as_str() {
                "New Folder" => "btn-new-folder".to_string(),
                "Name" => "field-name".to_string(),
                other => other.to_string(),
            }),
            ClickTarget::Coordinate(point) => {
                return Ok(ActionTarget::point(point.x, point.y));
            }
        };
        let id = named.unwrap_or_default();
        let Some((role, label, center)) = Self::element(&id) else {
            return Err(ComputerError::ElementNotFound(format!(
                "no element named {target:?} in the Finder window"
            )));
        };
        Ok(ActionTarget {
            point: Some(center),
            element: Some(ResolvedElement {
                id: id.clone(),
                role: role.to_string(),
                label: Some(label.to_string()),
                center: Some(center),
                bounds: None,
                // A non-empty path keeps `supports_ax_action` at `Unknown`
                // rather than `Absent`, which is the honest answer for an
                // adapter whose rungs are all pixel-level.
                path: vec![0, 1],
                actions: None,
                enabled: Tri::Known(true),
            }),
            app: Some("Finder".to_string()),
            snapshot: None,
        })
    }

    async fn display_geometry(&self) -> syscity::computer::Result<DisplayGeometry> {
        Ok(DisplayGeometry::new(vec![
            syscity::computer::DisplayInfo::new(0, 0, 100, 100).primary()
        ]))
    }
}

/// Computer use on, with the vision rung switched off so no run in this test
/// reaches for a model on the machine.
fn recording_config(port: u16) -> GatewayConfig {
    let mut config = test_config(port, false);
    config.model_provider = "mock".to_string();
    config.model = "mock-model".to_string();
    config.computer.enabled = true;
    config.computer.vision.mode = "off".to_string();
    config
}

/// The model's side of the recording: three `computer` calls — launch, click,
/// type — then a closing answer.
///
/// Each turn takes exactly one tool call, because that is how the recording
/// sees them arrive: one executed step per call, in the order a person would
/// work.
fn recording_mock() -> MockProvider {
    MockProvider::new().with_callback(|messages| {
        if messages.len() == 1 && messages[0].content.contains("NOCACHE") {
            return ProviderMessage::assistant("NOCACHE");
        }
        let done = messages.iter().filter(|m| m.role == Role::Tool).count();
        let call = |id: &str, arguments: &str| {
            ProviderMessage::assistant("Working.").with_tool_calls(vec![ToolCall {
                id: id.to_string(),
                call_type: "function".to_string(),
                function: FunctionCall {
                    name: "computer".to_string(),
                    arguments: arguments.to_string(),
                },
                index: None,
                result: None,
            }])
        };
        match done {
            0 => call("call_launch", r#"{"action":"launch_app","app_name":"Finder"}"#),
            1 => call("call_new_folder", r#"{"action":"click","element_id":"btn-new-folder"}"#),
            2 => {
                call("call_name", r#"{"action":"type","text":"Reports","element_id":"field-name"}"#)
            }
            _ => ProviderMessage::assistant("Created a new folder named Reports."),
        }
    })
}

/// Poll a run to its conclusion, returning its last `computer.status` view.
async fn run_to_finish(client: &mut FrontendSimulator, run_id: &str) -> serde_json::Value {
    let mut view = serde_json::Value::Null;
    for _ in 0..300 {
        let status = client
            .request("computer.status", json!({ "run_id": run_id }))
            .await;
        assert_eq!(status["ok"], true, "{status}");
        view = status["payload"].clone();
        if view["finished"] == true {
            return view;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    view
}

#[tokio::test]
#[serial]
async fn a_recorded_task_replays_with_a_different_name() {
    let port = free_port();
    let desktop = FinderDesktop::new();
    let gateway = new_test_gateway(recording_config(port)).await;
    register_mock_provider_with_model(&gateway.model_router(), recording_mock(), "mock-model")
        .await;

    // Two handles, both replaced: the tool an agent turn calls, and the
    // adapter `computer.play` builds its session from. The tool registered at
    // init captured whatever platform adapter this host has, so leaving it in
    // place would let the chat turn drive the real screen.
    let state = gateway.state();
    let fake: Arc<dyn ComputerAdapter> = desktop.clone();
    state
        .tools
        .computer_adapter
        .write()
        .await
        .replace(fake.clone());
    state.tools.registry.register_dynamic(Arc::new(
        ComputerTool::new(Some(fake)).with_recording(state.tools.computer_recording.clone()),
    ));

    start_gateway_and_wait_opts(port, gateway, true).await;

    let mut client = FrontendSimulator::connect(port).await;
    let sid = client.create_session().await;
    client.subscribe(vec![sid.clone()]).await;

    // ── Record ──
    let started = client.request("computer.record_start", json!({})).await;
    assert_eq!(started["ok"], true, "{started}");
    assert_eq!(started["payload"]["recording"], true, "{started}");

    client
        .send_chat(&sid, "In Finder, create a new folder and name it Reports.")
        .await;
    client
        .wait_for_event("chat.final", 120)
        .await
        .expect("the recording turn must finish");

    let stopped = client
        .request("computer.record_stop", json!({ "name": "tidy-finder" }))
        .await;
    assert_eq!(stopped["ok"], true, "{stopped}");
    let payload = &stopped["payload"];
    assert_eq!(payload["steps"], 3, "launch, click, type were recorded: {stopped}");

    // The typed value became a parameter named after the field it went into.
    assert_eq!(payload["parameters"], json!(["name"]), "{stopped}");
    assert_eq!(desktop.typed(), vec!["Reports".to_string()], "the turn typed once");

    // The saved workflow is the recording, parameterised: the literal is gone
    // from the action and the description alike, so a replay neither types
    // braces nor prints them to whoever reads the run.
    let store = WorkflowStore::for_paths(&test_paths_root());
    let saved = store
        .load("tidy-finder")
        .await
        .expect("the workflow was saved");
    assert_eq!(saved.steps.len(), 3);
    match &saved.steps[2].action {
        WorkflowAction::Desktop(DesktopAction::Type { text, .. }) => {
            assert_eq!(text, "{{name}}", "the typed value is a parameter");
        }
        other => panic!("the third step should be a type, got {other:?}"),
    }
    assert!(
        saved.steps[2].description.contains("{{name}}"),
        "the step description carries the placeholder too: {:?}",
        saved.steps[2].description
    );

    // ── Replay with a different value ──
    let played = client
        .request(
            "computer.play",
            json!({
                "name": "tidy-finder",
                "parameters": { "name": "Invoices" },
                "session_id": sid,
            }),
        )
        .await;
    assert_eq!(played["ok"], true, "{played}");
    let run_id = played["payload"]["run_id"]
        .as_str()
        .expect("a run id")
        .to_string();

    let view = run_to_finish(&mut client, &run_id).await;
    assert_eq!(view["finished"], true, "the replay never finished: {view}");
    assert_eq!(view["phase"], "done", "{view}");
    assert_eq!(view["last_verified"], true, "{view}");

    // ── The two runs are the same task ──
    let lines: Vec<String> = view["completed_steps"]
        .as_array()
        .expect("completed steps")
        .iter()
        .map(|line| line.as_str().unwrap_or_default().to_string())
        .collect();
    assert_eq!(lines.len(), 3, "every recorded step replayed: {view}");
    assert!(lines[0].contains("launch Finder"), "{lines:?}");
    assert!(lines[1].contains("New Folder"), "{lines:?}");
    assert!(lines[2].contains("Invoices"), "the bound value reached the screen: {lines:?}");
    assert!(!lines[2].contains("{{name}}"), "no placeholder survived: {lines:?}");
    assert!(!lines[2].contains("Reports"), "the recorded literal was rebound: {lines:?}");

    // Same actions, same order, one value different — which is what "the two
    // runs are equivalent" means for a parameterised workflow.
    assert_eq!(
        desktop.typed(),
        vec!["Reports".to_string(), "Invoices".to_string()],
        "the replay typed its own value, not the recording's"
    );
    assert_eq!(desktop.field_value(), "Invoices");
}

// ── The video half of a recording ───────────────────────────────────────────
//
// The plan's E-4: a recording can also keep the picture, so a replay that goes
// wrong can be read against what the original did. The capture is a real
// screen recording — ffmpeg reading the display — so this needs Screen
// Recording permission on macOS and a display that can be read.
//
// Two halves, and both matter:
//
// 1. **The wiring, hermetically.** Turning the option on must make
//    `record_start` report a video path, `record_stop` must store it on the
//    workflow and write a playable file, and the capture must stop on its own
//    when the recording does. This runs on any host, because the fake desktop
//    above is the screen and no permission is needed to see it.
// 2. **A real capture**, skipped loudly when the host cannot read its display.

/// The file a video path names must exist, decode, and carry actual frames.
///
/// "The file is non-empty" would pass for a container whose video stream never
/// got a frame (ffmpeg writes the header first). `ffprobe` is asked for the
/// stream description instead, which is the difference between "a recording
/// happened" and "a file appeared".
fn assert_playable_video(path: &std::path::Path) {
    assert!(path.exists(), "the recording video must exist at {}", path.display());
    let output = std::process::Command::new("ffprobe")
        .args([
            "-v",
            "error",
            "-select_streams",
            "v:0",
            "-show_entries",
            "stream=width,height,nb_frames",
        ])
        .arg("-of")
        .arg("default=noprint_wrappers=1")
        .arg(path)
        .output()
        .expect("ffprobe must be on PATH");
    assert!(
        output.status.success(),
        "ffprobe rejected {}: {}",
        path.display(),
        String::from_utf8_lossy(&output.stderr)
    );
    let report = String::from_utf8_lossy(&output.stdout);
    assert!(
        report.contains("width=") && report.contains("height="),
        "the video must have a picture stream: {report}"
    );
}

/// Recording video must switch on with the option and end with the recording.
///
/// Asserts the whole seam: the path is reported at `start`, the workflow names
/// it at `stop`, the file is a real video, and a second recording gets its own
/// file rather than overwriting the first.
#[tokio::test]
#[serial]
async fn a_recording_captures_a_video_when_the_option_is_on() {
    let Some(_) = super::skip_if_no_ffmpeg() else {
        return;
    };
    if !super::macos_screen_capture_available() {
        eprintln!(
            "Skipping the recorded-video E2E: this display cannot be captured (Screen Recording \
             permission, or no session). The wiring below it is exercised on every host by \
             `a_recording_without_video_still_saves_its_workflow`."
        );
        return;
    }

    let port = free_port();
    let desktop = FinderDesktop::new();
    let mut config = recording_config(port);
    config.computer.record_video = true;
    let gateway = new_test_gateway(config).await;
    register_mock_provider_with_model(&gateway.model_router(), recording_mock(), "mock-model")
        .await;

    let state = gateway.state();
    let fake: Arc<dyn ComputerAdapter> = desktop.clone();
    state
        .tools
        .computer_adapter
        .write()
        .await
        .replace(fake.clone());
    state.tools.registry.register_dynamic(Arc::new(
        ComputerTool::new(Some(fake)).with_recording(state.tools.computer_recording.clone()),
    ));

    start_gateway_and_wait(port, gateway).await;

    let mut client = FrontendSimulator::connect(port).await;
    let sid = client.create_session().await;
    client.subscribe(vec![sid.clone()]).await;

    let started = client.request("computer.record_start", json!({})).await;
    assert_eq!(started["ok"], true, "{started}");
    let video_path = started["payload"]["video"]
        .as_str()
        .unwrap_or_else(|| panic!("the recording must name its video: {started}"))
        .to_string();
    assert!(video_path.ends_with(".mp4"), "the capture writes an mp4: {video_path}");

    // Some steps, so the recording is not empty — a workflow with no steps is
    // refused by `record_stop` and there would be nothing to attach a video to.
    client
        .send_chat(&sid, "In Finder, create a new folder and name it Reports.")
        .await;
    client
        .wait_for_event("chat.final", 120)
        .await
        .expect("the recording turn must finish");

    let stopped = client
        .request("computer.record_stop", json!({ "name": "recorded-with-video" }))
        .await;
    assert_eq!(stopped["ok"], true, "{stopped}");
    assert_eq!(
        stopped["payload"]["video"].as_str(),
        Some(video_path.as_str()),
        "the stop reply names the same file: {stopped}"
    );

    // The saved workflow points at the video, relative to the artifacts dir, so
    // moving `~/.syscity` does not break the reference.
    let store = WorkflowStore::for_paths(&test_paths_root());
    let saved = store
        .load("recorded-with-video")
        .await
        .expect("the workflow was saved");
    let stored = saved
        .video
        .as_deref()
        .expect("the workflow names its video");
    assert!(
        !stored.starts_with('/'),
        "the path is stored relative to the artifact directory: {stored}"
    );

    // The capture ends when the recording does — that is a poll plus the last
    // frames still being written, so wait for the file rather than race it.
    let path = std::path::PathBuf::from(&video_path);
    let deadline = std::time::Instant::now() + Duration::from_secs(30);
    while !path.exists() && std::time::Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    assert_playable_video(&path);
}

/// With the option off, a recording saves its workflow and reports no video.
///
/// The other half of the seam, and the one that runs everywhere: the picture is
/// optional, and its absence must not be reported as a failure or leave a stale
/// path behind for the next recording to inherit.
#[tokio::test]
#[serial]
async fn a_recording_without_video_still_saves_its_workflow() {
    let port = free_port();
    let desktop = FinderDesktop::new();
    let gateway = new_test_gateway(recording_config(port)).await;
    register_mock_provider_with_model(&gateway.model_router(), recording_mock(), "mock-model")
        .await;

    let state = gateway.state();
    let fake: Arc<dyn ComputerAdapter> = desktop.clone();
    state
        .tools
        .computer_adapter
        .write()
        .await
        .replace(fake.clone());
    state.tools.registry.register_dynamic(Arc::new(
        ComputerTool::new(Some(fake)).with_recording(state.tools.computer_recording.clone()),
    ));

    start_gateway_and_wait(port, gateway).await;

    let mut client = FrontendSimulator::connect(port).await;
    let sid = client.create_session().await;
    client.subscribe(vec![sid.clone()]).await;

    let started = client.request("computer.record_start", json!({})).await;
    assert_eq!(started["ok"], true, "{started}");
    assert!(
        started["payload"]["video"].is_null(),
        "with the option off there is no capture, and the reply says so: {started}"
    );

    client
        .send_chat(&sid, "In Finder, create a new folder and name it Reports.")
        .await;
    client
        .wait_for_event("chat.final", 120)
        .await
        .expect("the recording turn must finish");

    let stopped = client
        .request("computer.record_stop", json!({ "name": "recorded-without-video" }))
        .await;
    assert_eq!(stopped["ok"], true, "{stopped}");
    assert!(
        stopped["payload"]["video"].is_null(),
        "no video means no path, not an empty one: {stopped}"
    );

    let store = WorkflowStore::for_paths(&test_paths_root());
    let saved = store
        .load("recorded-without-video")
        .await
        .expect("the workflow was saved regardless");
    assert!(saved.video.is_none(), "the workflow names no video: {:?}", saved.video);
    assert_eq!(saved.steps.len(), 3, "the actions were recorded as usual");
}
