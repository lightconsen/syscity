//! A computer run on a machine that cannot see.
//!
//! The plan's last B″ verification: a host with no local vision model and no
//! API vision model, running a goal that needs eyes, must end as `need_help`
//! **and say why** — not silently fail, and not spin through an empty screen.
//!
//! Everything here is hermetic: a fake desktop supplies the screen and the
//! tree, a mock provider supplies the decisions, and `[computer.vision]
//! model_dir` points at an empty directory so the run genuinely has nobody to
//! look, on any machine — including one that happens to have downloaded the
//! ONNX model into its real `~/.syscity`.

use async_trait::async_trait;

use syscity::computer::display::DisplayGeometry;
use syscity::computer::types::{Rect, Screenshot, UiElement};
use syscity::computer::{ActionResult, ComputerAdapter, DesktopAction, WaitCondition};

use super::*;

/// A one-pixel PNG: a capture the loop can hand to a vision rung.
const ONE_PIXEL_PNG: &str =
    "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAIAAACQd1PeAAAADElEQVR4nGP4z8AAAAMBAQDJ/pLvAAAAAElFTkSuQmCC";

/// A desktop whose accessibility tree was cut short, so the step wants eyes.
///
/// `truncated: true` is the fact that makes the vision trigger fire: the read
/// does not claim to be everything on screen, so a model deciding from it
/// alone would be deciding against a partial picture.
struct BlindDesktop;

#[async_trait]
impl ComputerAdapter for BlindDesktop {
    async fn screenshot(&self, _region: Option<Rect>) -> syscity::computer::Result<Screenshot> {
        Ok(Screenshot::new(ONE_PIXEL_PNG.to_string(), 1, 1))
    }

    async fn read_ui_tree(&self, _app: Option<&str>) -> syscity::computer::Result<Vec<UiElement>> {
        Ok(vec![UiElement {
            id: "w1".to_string(),
            role: "window".to_string(),
            label: Some("Preferences".to_string()),
            value: None,
            bounds: Some(Rect::new(0, 0, 100, 100)),
            enabled: syscity::computer::Tri::Known(true),
            focused: syscity::computer::Tri::Known(true),
            actions: None,
            children: Vec::new(),
            children_count: None,
            truncated: true,
        }])
    }

    async fn execute(&self, _action: DesktopAction) -> syscity::computer::Result<ActionResult> {
        Ok(ActionResult::success("nothing to do"))
    }

    async fn wait_for(
        &self,
        _condition: WaitCondition,
        _timeout: Duration,
    ) -> syscity::computer::Result<bool> {
        Ok(true)
    }

    async fn display_geometry(&self) -> syscity::computer::Result<DisplayGeometry> {
        Ok(DisplayGeometry::new(vec![
            syscity::computer::DisplayInfo::new(0, 0, 100, 100).primary()
        ]))
    }
}

/// Build the run's config: computer use on, nobody able to look.
fn blind_config(port: u16) -> (GatewayConfig, tempfile::TempDir) {
    let mut config = test_config(port, false);
    config.model_provider = "mock".to_string();
    config.model = "mock-model".to_string();
    config.computer.enabled = true;
    config.computer.settle_delay_ms = 1;
    config.computer.max_steps = 4;

    // `auto` with no local model and no API model is the honest "nobody can
    // look" case: the resolver reports it rather than inventing a backend.
    config.computer.vision.mode = "auto".to_string();
    config.computer.vision.provider = None;
    config.computer.vision.model = None;
    // An empty directory belongs to this test alone, so the run cannot find
    // the developer's downloaded model and quietly succeed at looking.
    let dir = tempfile::tempdir().expect("empty model dir");
    config.computer.vision.model_dir = dir.path().display().to_string();
    (config, dir)
}

/// Ask for a decision, then give up: the model demonstrates what a person
/// would do, and the run must carry that answer out rather than spin.
fn answering_help() -> MockProvider {
    MockProvider::new().with_callback(|messages| {
        let is_driver = messages
            .iter()
            .any(|m| m.role == Role::System && m.content.contains("You drive a computer"));
        if is_driver {
            return ProviderMessage::assistant(
                r#"{"decision": "help", "reason": "the accessibility tree has no Save button and I cannot see the screen"}"#,
            );
        }
        ProviderMessage::assistant("NOCACHE")
    })
}

#[tokio::test]
#[serial]
async fn a_run_with_no_vision_ends_asking_for_a_person_and_names_the_reason() {
    let port = free_port();
    let (config, _model_dir) = blind_config(port);
    let gateway = new_test_gateway(config).await;

    register_mock_provider_with_model(&gateway.model_router(), answering_help(), "mock-model")
        .await;

    // The fake desktop is installed on the shared tool state, which is the
    // same handle `computer.run` reads.
    gateway
        .state()
        .tools
        .computer_adapter
        .write()
        .await
        .replace(Arc::new(BlindDesktop));

    start_gateway_and_wait_opts(port, gateway, true).await;

    let mut client = FrontendSimulator::connect(port).await;
    let started = client
        .request("computer.run", json!({ "goal": "click Save in Preferences" }))
        .await;
    assert_eq!(started["ok"], true, "{started}");
    let run_id = started["payload"]["run_id"]
        .as_str()
        .expect("a run id")
        .to_string();

    // The run is spawned, so poll it rather than guessing a sleep.
    let mut view = serde_json::Value::Null;
    for _ in 0..100 {
        let status = client
            .request("computer.status", json!({ "run_id": &run_id }))
            .await;
        assert_eq!(status["ok"], true, "{status}");
        view = status["payload"].clone();
        if view["finished"] == true {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    assert_eq!(view["finished"], true, "the run never finished: {view}");
    assert_eq!(view["phase"], "failed", "{view}");
    assert_eq!(view["summary"]["stop_reason"], "need_help", "{view}");

    // The plan's actual requirement: the run names what it could not do. A
    // bare "needs a person" would leave the reader guessing whether it was
    // the tree, the model, or a missing feature.
    let vision = view["vision"]
        .as_str()
        .unwrap_or_else(|| panic!("the run log says why nobody could look: {view}"));
    assert!(vision.starts_with("unavailable:"), "the scan's verdict is recorded: {vision}");
    assert!(vision.contains("vision"), "the reason names the capability: {vision}");

    // And the model's own reason survives into the run's closing message.
    let message = view["message"].as_str().unwrap_or_default();
    assert!(message.contains("Save"), "the blocking reason reaches status: {message}");

    // The handoff note is what asks a person to take over, so it must exist as
    // a real pending approval rather than only as a field on the run.
    let approvals = client.request("approvals.list", json!({})).await;
    let notes = approvals["payload"]["approvals"]
        .as_array()
        .or_else(|| approvals["payload"].as_array())
        .unwrap_or_else(|| panic!("approvals.list answered {approvals}"));
    assert!(
        notes.iter().any(|a| a["tool_name"] == "computer_run"
            || a["source"] == "computer-run"
            || a["message"]
                .as_str()
                .is_some_and(|m| m.contains("Computer run stopped"))),
        "a handoff note is pending for a person: {approvals}"
    );
}

// ── The agent-facing entry point ─────────────────────────────────────────────
//
// `computer.run` is reachable from a client that knows it wants a run. The
// agent already looking at a screen has no such client — it has a tool
// registry — and `computer_run` is that half. The part worth proving end to
// end is not that the tool starts a run (the unit tests cover its refusals)
// but that the run it starts *knows which turn started it*: the tool takes the
// turn id from the turn's trace, the checkpoint records it, and `turns.replay`
// folds that run's steps into that turn. Without that last link the tool would
// work and the timeline would still show nothing.

/// A stand-in for the turn trace a real turn hands its tools: it names a turn
/// and files nothing, because this test asserts the id, not the trace sink.
#[derive(Debug)]
struct TurnTraceForTest {
    turn_id: String,
}

impl syscity::tools::TurnTrace for TurnTraceForTest {
    fn record_action(&self, _rec: &syscity::observe::ActionTraceEvent) {}

    fn turn_id(&self) -> &str {
        &self.turn_id
    }
}

/// The agent's path: a tool call inside turn `t1` starts the run, and the run's
/// steps come back out of `turns.replay` for that same turn.
#[tokio::test]
#[serial]
async fn a_tool_call_inside_a_turn_records_the_run_under_that_turn() {
    use syscity::tools::{Tool, ToolContext};

    let port = free_port();
    let mut config = test_config(port, false);
    config.model_provider = "mock".to_string();
    config.model = "mock-model".to_string();
    config.computer.enabled = true;
    config.computer.settle_delay_ms = 1;
    config.computer.max_steps = 4;
    config.computer.vision.mode = "off".to_string();

    let gateway = new_test_gateway(config).await;
    register_mock_provider_with_model(&gateway.model_router(), clicking_then_done(2), "mock-model")
        .await;
    gateway
        .state()
        .tools
        .computer_adapter
        .write()
        .await
        .replace(ChangingDesktop::new());

    // The turn this call belongs to. A real turn's trace is the collector's;
    // the tool only ever reads the id off it.
    let turn_id = "turn-from-agent";
    // The same context shape the agent loop builds: identity, conversation and
    // trace. The conversation matters — it is where the run's events go and
    // what its inherited desktop approval is scoped to.
    let context = ToolContext::new("user", "conv-agent")
        .with_trace(Some(Arc::new(TurnTraceForTest { turn_id: turn_id.to_string() })));

    let tool = syscity::tools::ComputerRunTool::new(gateway.state());
    let result = tool
        .execute(json!({ "goal": "click through Preferences" }), &context)
        .await
        .expect("a run inside a turn is not an error");
    assert!(result.success, "{result:?}");
    let run_id = result.data.as_ref().expect("data")["run_id"]
        .as_str()
        .expect("a run id")
        .to_string();

    start_gateway_and_wait_opts(port, gateway, true).await;

    // Wait for the run to conclude; the checkpoint is written per step and
    // again at the end, so a finished run is the point at which the file has
    // every step in it.
    let mut client = FrontendSimulator::connect(port).await;
    let mut view = serde_json::Value::Null;
    for _ in 0..200 {
        let status = client
            .request("computer.status", json!({ "run_id": &run_id }))
            .await;
        assert_eq!(status["ok"], true, "{status}");
        view = status["payload"].clone();
        if view["finished"] == true {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert_eq!(view["finished"], true, "the run never finished: {view}");
    assert_eq!(view["phase"], "done", "{view}");

    // The join the whole change exists for: the turn's replay carries the run's
    // steps, keyed by the turn id the tool read off its context.
    let replay = client
        .request("turns.replay", json!({ "turn_id": turn_id }))
        .await;
    assert_eq!(replay["ok"], true, "the turn replays: {replay}");
    let payload = &replay["payload"];
    let steps = payload["steps"].as_array().expect("steps");
    assert_eq!(
        steps.len(),
        view["summary"]["steps"].as_u64().unwrap_or(0) as usize,
        "every step the run took is in the timeline: {payload}"
    );
    for (i, step) in steps.iter().enumerate() {
        assert_eq!(step["source"], "run", "{step}");
        assert_eq!(step["run_id"], run_id.as_str(), "{step}");
        assert_eq!(step["step"], i as u64, "positions run 0..n: {payload}");
    }
    let runs = payload["runs"].as_array().expect("runs");
    assert_eq!(runs.len(), 1, "{payload}");
    assert_eq!(runs[0]["run_id"], run_id.as_str(), "{payload}");
}

//
// The plan's second verification gap: a scripted desktop and a scripted model,
// three steps and an answer, and the `computer.step` stream must be exactly
// what a timeline needs — one ordered event per completed step, carrying the
// step number the run's own status counts from, then one `finished`.
//
// Asserted through the public surface (`computer.run` → events →
// `computer.status`) rather than against the executor directly, because the
// event is what the UI actually renders and the status is what the next
// client polls. A test of the loop alone would not catch a gateway that
// numbered one and reported the other.

/// A desktop whose screen changes on every click.
///
/// Verification compares the frame before a step with the frame after it. A
/// desktop that never changes makes every click read as `Failed`, the failure
/// streak ends the run early, and the test would be measuring the streak.
struct ChangingDesktop {
    clicks: std::sync::atomic::AtomicUsize,
}

impl ChangingDesktop {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            clicks: std::sync::atomic::AtomicUsize::new(0),
        })
    }
}

#[async_trait]
impl ComputerAdapter for ChangingDesktop {
    async fn screenshot(&self, _region: Option<Rect>) -> syscity::computer::Result<Screenshot> {
        let n = self.clicks.load(std::sync::atomic::Ordering::SeqCst);
        // Two alternating byte patterns: different everywhere, so the diff is
        // visible whether the build decodes pixels or samples them.
        let fill = if n % 2 == 0 { b'A' } else { b'B' };
        let data = vec![fill; 10_000];
        Ok(Screenshot::new(String::from_utf8(data).unwrap_or_default(), 100, 100))
    }

    async fn read_ui_tree(&self, _app: Option<&str>) -> syscity::computer::Result<Vec<UiElement>> {
        // Complete, so the loop decides from text and never reaches for a
        // vision rung this test has switched off.
        Ok(vec![UiElement {
            id: "w1".to_string(),
            role: "window".to_string(),
            label: Some("Preferences".to_string()),
            value: None,
            bounds: Some(Rect::new(0, 0, 100, 100)),
            enabled: syscity::computer::Tri::Known(true),
            focused: syscity::computer::Tri::Known(true),
            actions: None,
            children: Vec::new(),
            children_count: None,
            truncated: false,
        }])
    }

    async fn execute(&self, action: DesktopAction) -> syscity::computer::Result<ActionResult> {
        if matches!(action, DesktopAction::Click { .. }) {
            self.clicks
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        }
        Ok(ActionResult::success("clicked"))
    }

    async fn wait_for(
        &self,
        _condition: WaitCondition,
        _timeout: Duration,
    ) -> syscity::computer::Result<bool> {
        Ok(true)
    }

    async fn display_geometry(&self) -> syscity::computer::Result<DisplayGeometry> {
        Ok(DisplayGeometry::new(vec![
            syscity::computer::DisplayInfo::new(0, 0, 100, 100).primary()
        ]))
    }
}

/// Click for the first `clicks` decisions, then declare the goal done.
fn clicking_then_done(clicks: usize) -> MockProvider {
    let seen = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    MockProvider::new().with_callback(move |messages| {
        let is_driver = messages
            .iter()
            .any(|m| m.role == Role::System && m.content.contains("You drive a computer"));
        if !is_driver {
            return ProviderMessage::assistant("NOCACHE");
        }
        let n = seen.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        if n < clicks {
            ProviderMessage::assistant(
                r#"{"decision": "act", "action": {"type": "click", "x": 10, "y": 10}}"#,
            )
        } else {
            ProviderMessage::assistant(r#"{"decision": "done", "message": "finished"}"#)
        }
    })
}

/// Three acted steps then an answer: the stream must be three ordered steps
/// followed by one `finished`, and the status must agree about how many there
/// were.
#[tokio::test]
#[serial]
async fn a_three_step_run_streams_one_ordered_event_per_step_then_finished() {
    let port = free_port();
    let mut config = test_config(port, false);
    config.model_provider = "mock".to_string();
    config.model = "mock-model".to_string();
    config.computer.enabled = true;
    config.computer.settle_delay_ms = 1;
    // Room for the three clicks and the decision that answers; one more would
    // let the loop overshoot and the count assertion would be about the bound
    // rather than about the run.
    config.computer.max_steps = 4;
    config.computer.vision.mode = "off".to_string();

    let gateway = new_test_gateway(config).await;
    register_mock_provider_with_model(&gateway.model_router(), clicking_then_done(3), "mock-model")
        .await;
    gateway
        .state()
        .tools
        .computer_adapter
        .write()
        .await
        .replace(ChangingDesktop::new());

    start_gateway_and_wait_opts(port, gateway, true).await;

    let mut client = FrontendSimulator::connect(port).await;
    let sid = client.create_session().await;
    client.subscribe(vec![sid.clone()]).await;

    let started = client
        .request(
            "computer.run",
            json!({ "goal": "click through Preferences", "session_id": &sid }),
        )
        .await;
    assert_eq!(started["ok"], true, "{started}");
    let run_id = started["payload"]["run_id"]
        .as_str()
        .expect("a run id")
        .to_string();

    // Three steps, in order, each naming an action the adapter actually ran.
    //
    // `event.step` is zero-based, matching `turns.replay` and a run's own
    // events; the human-facing `completed_steps` lines are 1-based. A stream
    // that numbered from one here would draw every replay's timeline one step
    // out of step with the run that recorded it.
    let mut seen_steps = Vec::new();
    for expected in 0..3usize {
        let event = client
            .wait_for_event("computer.step", 30)
            .await
            .unwrap_or_else(|| panic!("no computer.step for step {expected}"));
        assert_eq!(event["run_id"], run_id.as_str(), "{event}");
        assert_eq!(event["session_id"], sid.as_str(), "{event}");
        let inner = &event["event"];
        assert_eq!(inner["kind"], "step", "{event}");
        assert_eq!(
            inner["step"],
            expected,
            "steps must arrive in order, one event each, numbered from zero: {}",
            serde_json::json!(seen_steps)
        );
        let action = inner["action"].as_str().unwrap_or_default();
        assert!(
            action.contains("click") || action.contains("Click"),
            "the event says what was done, got {action:?} in {event}"
        );
        // The frame changed under the click, so the step is verified — an event
        // stream that reported `null` here would leave a timeline unable to
        // tell a step that landed from one that did not.
        assert_eq!(inner["verified"], true, "{event}");
        seen_steps.push(inner["step"].as_u64());
    }

    // Then exactly one conclusion, carrying the reason the run ended.
    let finished = client
        .wait_for_event("computer.step", 30)
        .await
        .expect("the run must report that it finished");
    assert_eq!(finished["run_id"], run_id.as_str(), "{finished}");
    let inner = &finished["event"];
    assert_eq!(inner["kind"], "finished", "{finished}");
    assert_eq!(inner["stop_reason"], "done", "{finished}");
    assert_eq!(inner["phase"], "done", "{finished}");
    assert_eq!(inner["steps"], 3, "the conclusion counts the same steps: {finished}");

    // And the status a later client polls agrees with the stream it missed.
    let mut view = serde_json::Value::Null;
    for _ in 0..100 {
        let status = client
            .request("computer.status", json!({ "run_id": &run_id }))
            .await;
        assert_eq!(status["ok"], true, "{status}");
        view = status["payload"].clone();
        if view["finished"] == true {
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    assert_eq!(view["finished"], true, "the run never finished: {view}");
    assert_eq!(view["phase"], "done", "{view}");
    assert_eq!(
        view["summary"]["steps"], 3,
        "status and the event stream must count the same steps: {view}"
    );
    let completed = view["completed_steps"].as_array().expect("completed steps");
    assert_eq!(completed.len(), 3, "{view}");
    for (i, line) in completed.iter().enumerate() {
        let line = line.as_str().unwrap_or_default();
        assert!(
            line.starts_with(&format!("{}. ", i + 1)),
            "step {i} is numbered {i} in the visible history: {line}"
        );
    }
}
