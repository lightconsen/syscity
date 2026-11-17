//! A computer run that outlives the process that started it.
//!
//! Stage C's verification, from the plan: kill the gateway mid-run, start a new
//! one over the same state, and the run is still there — listed by
//! `computer.runs`, continuable with `computer.resume` from the step after the
//! last one it completed.
//!
//! Both gateways share one layout root and one database, which is what a
//! restart over the same machine gives you. The first one is stopped without
//! letting the run conclude, so what it leaves behind is exactly what a process
//! death leaves: a checkpoint of the steps that completed, with no conclusion
//! written and no answer.
//!
//! The fake desktop changes its capture after every click, so a step's
//! verification sees a real effect and the run keeps making progress. The two
//! gateways get different mock behaviours: the first never finishes (it is
//! meant to be interrupted), the second finishes after one more click — so the
//! resumed run's `done` is the resumed process's answer, not a replay of the
//! first one.

use async_trait::async_trait;

use syscity::computer::display::DisplayGeometry;
use syscity::computer::types::{Rect, Screenshot, UiElement};
use syscity::computer::{ActionResult, ComputerAdapter, DesktopAction, WaitCondition};

use super::*;

/// Two captured frames that differ everywhere, so a click's effect is visible
/// to the verification diff on any build (with or without the `vision`
/// feature, which decides whether pixels are decoded or sampled).
fn frame(odd: bool) -> String {
    if odd {
        "B".repeat(1200)
    } else {
        "A".repeat(1200)
    }
}

/// A desktop whose screen changes on every click.
///
/// Without the change, every step verifies as `Failed`, the failure streak ends
/// the run on its own, and the test would be measuring the streak instead of
/// the checkpoint.
struct ClickingDesktop {
    clicks: std::sync::atomic::AtomicUsize,
}

impl ClickingDesktop {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            clicks: std::sync::atomic::AtomicUsize::new(0),
        })
    }
}

#[async_trait]
impl ComputerAdapter for ClickingDesktop {
    async fn screenshot(&self, _region: Option<Rect>) -> syscity::computer::Result<Screenshot> {
        let odd = self.clicks.load(std::sync::atomic::Ordering::SeqCst) % 2 == 1;
        Ok(Screenshot::new(frame(odd), 100, 100))
    }

    async fn read_ui_tree(&self, _app: Option<&str>) -> syscity::computer::Result<Vec<UiElement>> {
        // A complete tree: the run is meant to decide from text, not to spend
        // its turns asking a vision rung that this test has switched off.
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
        // A beat per action, so the run is still going when the test looks at
        // it rather than finishing between two polls.
        tokio::time::sleep(Duration::from_millis(20)).await;
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

/// Config for a run: long enough to interrupt, and with vision switched off so
/// the test never reaches for a model on this machine.
fn run_config(port: u16, db_path: &Path) -> GatewayConfig {
    let mut config = test_config(port, false);
    // One database for both gateways: the conversation that asked for the run
    // is the conversation the restart must notify.
    config.storage.database_url = Some(format!("sqlite:{}", db_path.display()));
    config.model_provider = "mock".to_string();
    config.model = "mock-model".to_string();
    config.computer.enabled = true;
    config.computer.settle_delay_ms = 1;
    config.computer.max_steps = 8;
    config.computer.vision.mode = "off".to_string();
    config
}

/// Click for the first `clicks` decisions, then declare the goal done.
///
/// The counter is local to this mock, so the second gateway's run decides from
/// its own first call: a resumed run that was replaying the first one's
/// decisions would never produce the `done` this assertion needs.
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

/// Boot a gateway with the fake desktop and a mock driver, returning it so the
/// test can stop it.
async fn boot(port: u16, db_path: &Path, mock: MockProvider) -> Arc<Gateway> {
    let config = run_config(port, db_path);
    let gateway = Arc::new(new_test_gateway(config).await);
    register_mock_provider_with_model(&gateway.model_router(), mock, "mock-model").await;
    gateway
        .state()
        .tools
        .computer_adapter
        .write()
        .await
        .replace(ClickingDesktop::new());

    let starter = gateway.clone();
    tokio::spawn(async move {
        let _ = starter.start().await;
    });

    let url = format!("ws://127.0.0.1:{port}/ws");
    let deadline = tokio::time::Instant::now() + GATEWAY_START_TIMEOUT;
    while tokio::time::Instant::now() < deadline {
        // `Ok(Ok(_))`, not `is_ok()`: the outer result is the timeout, and a
        // refused connection is still a completed attempt.
        if matches!(timeout(Duration::from_secs(5), connect_async(&url)).await, Ok(Ok(_))) {
            return gateway;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic_gateway_start_for(port);
}

fn panic_gateway_start_for(port: u16) -> ! {
    dump_captured_logs();
    panic!("gateway on {port} never accepted a WS connection");
}

#[tokio::test]
#[serial]
async fn an_interrupted_run_is_listed_and_resumes_from_the_next_step() {
    let db_dir = tempfile::tempdir().expect("temp db dir");
    let db_path = db_dir.path().join("resume.db");

    // ── First process: start a run, then die mid-run ──
    let port_a = free_port();
    let gateway_a = boot(port_a, &db_path, clicking_then_done(100)).await;
    let mut client = FrontendSimulator::connect(port_a).await;
    let sid = client.create_session().await;

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

    // Let it take a couple of real steps before pulling the plug.
    let mut steps_done = 0;
    for _ in 0..200 {
        let status = client
            .request("computer.status", json!({ "run_id": &run_id }))
            .await;
        steps_done = status["payload"]["step"].as_u64().unwrap_or(0) as usize;
        if steps_done >= 2 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    assert!(steps_done >= 2, "the run never reached two steps ({steps_done})");

    // Stop without a conclusion: the run's task is aborted rather than allowed
    // to finish, and the gateway goes away underneath it. From the checkpoint's
    // point of view this is a process death.
    let checkpoint_dir = gateway_a.state().paths.computer_runs_dir();
    gateway_a
        .state()
        .task_registry
        .abort_matching("computer:")
        .await;
    gateway_a.stop().await.expect("gateway A stops");
    assert!(
        checkpoint_dir.join(format!("{run_id}.json")).is_file(),
        "a step was checkpointed before the interruption: {checkpoint_dir:?}"
    );

    // ── Second process: same layout root, same database, new port ──
    let port_b = free_port();
    let gateway_b = boot(port_b, &db_path, clicking_then_done(1)).await;
    let mut client_b = FrontendSimulator::connect(port_b).await;

    // The conversation is told about the interrupted run without being asked:
    // the run is not re-armed, but "there is work to continue" must not be a
    // fact only the log knows. Polled because the notice is written during
    // startup, which the listener accepting a connection does not wait for.
    let mut told = false;
    for _ in 0..100 {
        let history = client_b
            .request("chat.history", json!({ "session_id": &sid, "limit": 20 }))
            .await;
        let messages = history["payload"]["messages"]
            .as_array()
            .unwrap_or_else(|| panic!("chat.history answered {history}"));
        told = messages.iter().any(|m| {
            m["content"]
                .as_str()
                .is_some_and(|c| c.contains(&run_id) && c.contains("computer.resume"))
        });
        if told {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(told, "the session was told which run to continue: {sid}");

    let listed = client_b.request("computer.runs", json!({})).await;
    assert_eq!(listed["ok"], true, "{listed}");
    let runs = listed["payload"]["runs"].as_array().expect("runs");
    let row = runs
        .iter()
        .find(|r| r["run_id"] == run_id)
        .unwrap_or_else(|| panic!("the interrupted run is listed: {listed}"));
    assert_eq!(row["interrupted"], true, "{row}");
    assert_eq!(row["goal"], "click through Preferences", "{row}");
    let next_step = row["next_step"].as_u64().expect("next step") as usize;
    assert!(next_step >= 2, "the listing counts what was done: {row}");
    assert_eq!(
        row["completed_steps"].as_array().map(|s| s.len()),
        Some(next_step),
        "the listing shows the steps it will not replay: {row}"
    );

    // Resume: the file says where to continue, and the run continues there.
    let resumed = client_b
        .request("computer.resume", json!({ "run_id": &run_id }))
        .await;
    assert_eq!(resumed["ok"], true, "{resumed}");
    assert_eq!(
        resumed["payload"]["resumed_from_step"]
            .as_u64()
            .unwrap_or(0) as usize,
        next_step,
        "the response names where it continues: {resumed}"
    );

    let mut view = serde_json::Value::Null;
    for _ in 0..300 {
        let status = client_b
            .request("computer.status", json!({ "run_id": &run_id }))
            .await;
        assert_eq!(status["ok"], true, "{status}");
        view = status["payload"].clone();
        if view["finished"] == true {
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    assert_eq!(view["finished"], true, "the resumed run finished: {view}");
    assert_eq!(view["summary"]["stop_reason"], "done", "{view}");

    // The visible history is one run, not two: the steps the first process took
    // are still numbered from one, and the resumed run added its own after
    // them.
    let completed = view["completed_steps"].as_array().expect("completed steps");
    assert!(
        completed.len() > next_step,
        "the resumed run added steps after the interruption: {view}"
    );
    let first = completed[0].as_str().unwrap_or_default();
    assert!(first.starts_with("1. "), "the resumed run keeps the earlier steps: {view}");

    // A run that reached an answer has nothing to continue, so it is not
    // offered — but the file stays: it is the only record of what the run did
    // once the in-memory state and the live event stream are gone. Deleting it
    // is how a *successful* run used to leave no trace at all.
    tokio::time::sleep(Duration::from_millis(100)).await;
    let finished_file = checkpoint_dir.join(format!("{run_id}.json"));
    assert!(finished_file.is_file(), "a finished run keeps its record (the plan's 过程可查)");
    let listed_again = client_b.request("computer.runs", json!({})).await;
    assert!(
        !listed_again["payload"]["runs"]
            .as_array()
            .unwrap()
            .iter()
            .any(|r| r["run_id"] == run_id),
        "the finished run is not offered for resume: {listed_again}"
    );
    // ...and it is reachable when the caller asks for the records, not the offers.
    let with_finished = client_b
        .request("computer.runs", json!({ "include_finished": true }))
        .await;
    let recorded = with_finished["payload"]["runs"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["run_id"] == run_id)
        .unwrap_or_else(|| panic!("the record is listed on request: {with_finished}"));
    assert_eq!(recorded["stop_reason"], "done", "{recorded}");
    assert_eq!(recorded["interrupted"], false, "{recorded}");
    assert!(recorded["next_step"].as_u64().unwrap_or(0) >= next_step as u64, "{recorded}");

    gateway_b.stop().await.expect("gateway B stops");
}
