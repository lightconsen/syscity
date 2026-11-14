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
