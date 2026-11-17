//! Stage D′ — a failure becomes evidence, and the evidence gates the next change.
//!
//! The plan's last verification is the whole loop, end to end: a desktop run
//! stops short, its failure is collected, a person confirms what "done" would
//! have looked like, that confirmed case becomes a regression task, and a
//! candidate change is measured against it — with a regression reverting the
//! change instead of shipping it.
//!
//! The links are real here, not stubs:
//!
//! - the run is a real `computer.run` on a fake desktop that cannot see, so the
//!   gateway's own producer writes the badcase row;
//! - the confirmation goes through `ExpectedEndState`, the vacuous-green guard,
//!   and lands as a YAML file the harness loader reads;
//! - the measurement is `HarnessCandidateVerifier` over that corpus, i.e. the
//!   production verdict gate, not a hand-rolled comparison;
//! - the rollback is the optimizer's Gate 3, the same path an online-signal
//!   anomaly takes in production.
//!
//! The one thing that is artificial is the *cause* of the regression: this
//! model exports at temperature ≥ 0.75 and gives up below it, so the optimizer
//! has a real, measurable effect to find. A genuine "L2 trigger rule" change has
//! no scalar field (see `docs/computer-use.md` §5.3), so the gate can only be
//! driven through a scalar — which is also the only thing the existing
//! optimizer is allowed to move.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;

use syscity::computer::display::DisplayGeometry;
use syscity::computer::types::{Rect, Screenshot, UiElement};
use syscity::computer::{ActionResult, ComputerAdapter, DesktopAction, WaitCondition};
use syscity::eval::{
    ExpectedEndState, HarnessCandidateVerifier, OptimizerRunParams, PendingBadcaseStore,
    ScalarCandidate, ScalarOptimizer, ShadowEvaluator, TraceKind,
};
use syscity::goal::GoalCondition;
use syscity::model_router::{Credential, ModelRouter};
use syscity::providers::{
    CompletionChunk, CompletionRequest, CompletionResponse, CompletionStream, Provider, Usage,
};

use super::*;

/// A one-pixel PNG: the capture the run can take but nobody can look at.
const ONE_PIXEL_PNG: &str =
    "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAIAAACQd1PeAAAADElEQVR4nGP4z8AAAAMBAQDJ/pLvAAAAAElFTkSuQmCC";

/// The goal both runs are given. Short on purpose: the harness turns a
/// confirmed badcase into an eval task whose `input` is the run's goal, and a
/// short input keeps the agent from spending provider calls on the cache check
/// and the planner — the only decision left is the one under test.
const GOAL_A: &str = "export report";
const GOAL_B: &str = "export invoice";

/// A desktop whose accessibility tree is truncated and whose screen is blank.
/// With no local or API vision backend there is nobody to look, so every run
/// ends asking for a person — the failure D′ collects.
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
            label: Some("Report".to_string()),
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

/// The model behind both the run and the eval trials.
///
/// It has two jobs, told apart by what it is asked:
///
/// - the desktop driver asks it to decide, and it cannot — the tree is
///   truncated and there is no screen — so it answers `help`;
/// - an eval trial asks it to act on the confirmed end state, and whether it
///   does depends on the one scalar the optimizer can move. That dependency is
///   what makes "candidate vs baseline" a measurable difference rather than a
///   coin flip.
struct TempSensitiveProvider {
    /// Every temperature the provider was asked to run at, in order. Lets the
    /// test prove the harness really ran both the baseline and the candidate.
    temperatures: Mutex<Vec<Option<f32>>>,
}

impl TempSensitiveProvider {
    fn new() -> Self {
        Self {
            temperatures: Mutex::new(Vec::new()),
        }
    }

    fn answer(&self, request: &CompletionRequest) -> ProviderMessage {
        self.temperatures
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(request.temperature);

        let drives_a_computer = request
            .messages
            .iter()
            .any(|m| m.role == Role::System && m.content.contains("You drive a computer"));
        if drives_a_computer {
            return ProviderMessage::assistant(
                r#"{"decision": "help", "reason": "the accessibility tree is truncated and no vision backend can look at the screen"}"#,
            );
        }

        if request.temperature.unwrap_or(0.7) >= 0.75 {
            ProviderMessage::assistant("EXPORTED the report to /tmp/report.csv")
        } else {
            ProviderMessage::assistant("I could not export the report")
        }
    }

    fn temperatures_seen(&self) -> Vec<Option<f32>> {
        self.temperatures
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }
}

#[async_trait]
impl Provider for TempSensitiveProvider {
    fn name(&self) -> &str {
        "mock"
    }

    fn default_model(&self) -> &str {
        "mock-model"
    }

    fn supports_tools(&self) -> bool {
        true
    }

    fn max_context(&self) -> usize {
        128_000
    }

    async fn complete(&self, request: CompletionRequest) -> syscity::Result<CompletionResponse> {
        Ok(CompletionResponse {
            message: self.answer(&request),
            model: self.default_model().to_string(),
            usage: Some(Usage::default()),
            finish_reason: Some("stop".to_string()),
        })
    }

    async fn stream(&self, request: CompletionRequest) -> syscity::Result<CompletionStream> {
        let message = self.answer(&request);
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let _ = tx.send(CompletionChunk {
            content: Some(message.content.clone()),
            reasoning_content: None,
            tool_calls: message.tool_calls.clone(),
            is_done: false,
            error: None,
            usage: None,
        });
        let _ = tx.send(CompletionChunk {
            content: None,
            reasoning_content: None,
            tool_calls: None,
            is_done: true,
            error: None,
            usage: Some(Usage::default()),
        });
        Ok(Box::pin(tokio_stream::wrappers::UnboundedReceiverStream::new(rx)))
    }

    async fn health_check(&self) -> syscity::Result<bool> {
        Ok(true)
    }

    async fn set_credential(&self, _credential: Credential) -> syscity::Result<()> {
        Ok(())
    }
}

/// Passes the pre-apply shadow gate, then reports an online-signal anomaly once
/// a candidate has been evaluated — the smallest honest stand-in for "the
/// change looked fine, then the signals turned".
#[derive(Default)]
struct AnomalyAfterApply {
    evaluated: AtomicBool,
}

#[async_trait]
impl ShadowEvaluator for AnomalyAfterApply {
    async fn evaluate(&self, _candidate: &ScalarCandidate) -> Result<bool, String> {
        self.evaluated.store(true, Ordering::SeqCst);
        Ok(true)
    }

    async fn is_anomalous(&self) -> Result<bool, String> {
        Ok(self.evaluated.load(Ordering::SeqCst))
    }
}

/// The run's gateway, the database its badcases live in, and the empty vision
/// directory that keeps every machine honest about "nobody can look".
fn loop_config(port: u16) -> (GatewayConfig, tempfile::TempDir, String) {
    let mut config = test_config(port, false);
    config.model_provider = "mock".to_string();
    config.model = "mock-model".to_string();

    config.computer.enabled = true;
    config.computer.settle_delay_ms = 1;
    config.computer.max_steps = 4;
    config.computer.vision.mode = "auto".to_string();
    config.computer.vision.provider = None;
    config.computer.vision.model = None;
    let vision_dir = tempfile::tempdir().expect("empty vision model dir");
    config.computer.vision.model_dir = vision_dir.path().display().to_string();

    // The optimizer gate this test drives. `manual` cadence keeps the
    // scheduler from running a pass of its own while the test does.
    config.eval.optimizer.enabled = true;
    config.eval.optimizer.cadence = "manual".to_string();
    config.eval.optimizer.max_steps = 1;
    config.eval.optimizer.guardrails.enabled = true;

    let db_path = std::env::temp_dir().join(format!("syscity_e2e_ws_test_{port}.db"));
    let db_url = format!("sqlite:{}", db_path.display());
    (config, vision_dir, db_url)
}

/// Register a provider instance and the model it owns, so routing resolves
/// `mock-model` back to it (the harness's agents go through the router).
async fn register_provider(router: &Arc<ModelRouter>, provider: Arc<TempSensitiveProvider>) {
    router
        .add_provider_instance("mock", provider)
        .await
        .expect("register the test provider");
    router
        .model_catalog
        .add_source(Box::new(syscity::model_router::model_catalog::StaticModelSource::new(
            "mock",
            vec![("mock".to_string(), "mock-model".to_string())],
        )))
        .await;
    let _ = router.model_catalog.discover().await;
}

/// Start a run and wait for it to finish, returning the run's status view.
async fn run_to_completion(client: &mut FrontendSimulator, goal: &str) -> serde_json::Value {
    let started = client
        .request("computer.run", json!({ "goal": goal }))
        .await;
    assert_eq!(started["ok"], true, "{started}");
    let run_id = started["payload"]["run_id"]
        .as_str()
        .expect("a run id")
        .to_string();

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
    view
}

#[tokio::test]
#[serial]
async fn a_failed_run_becomes_a_regression_case_and_a_regression_reverts_the_change() {
    let port = free_port();
    let (config, _vision_dir, db_url) = loop_config(port);
    let gateway = new_test_gateway(config).await;

    // The provider must be registered before the gateway starts: the run and
    // the harness both resolve `mock-model` through the router, and a provider
    // added after startup would race the scheduler.
    let provider = Arc::new(TempSensitiveProvider::new());
    register_provider(&gateway.model_router(), provider.clone()).await;

    // The fake desktop goes on the shared tool state — the same handle
    // `computer.run` reads.
    gateway
        .state()
        .tools
        .computer_adapter
        .write()
        .await
        .replace(Arc::new(BlindDesktop));

    // Held across the gateway's move into the run loop below: `state` is the
    // same handle the optimizer and the trace store are reachable through.
    let state = gateway.state();

    start_gateway_and_wait_opts(port, gateway, true).await;

    let mut client = FrontendSimulator::connect(port).await;

    // ── 1. Two runs stop short, and the gateway collects both ─────────────
    for goal in [GOAL_A, GOAL_B] {
        let view = run_to_completion(&mut client, goal).await;
        assert_eq!(view["phase"], "failed", "the run must stop short: {view}");
        assert_eq!(view["summary"]["stop_reason"], "need_help", "{view}");
    }

    // The producer runs after the run concludes, so poll rather than guess.
    let mut badcases = Vec::new();
    for _ in 0..100 {
        let listed = client
            .request("eval.badcase.list", json!({ "source": "computer:run" }))
            .await;
        assert_eq!(listed["ok"], true, "{listed}");
        badcases = listed["payload"]["badcases"]
            .as_array()
            .cloned()
            .unwrap_or_default();
        if badcases.len() >= 2 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert_eq!(badcases.len(), 2, "both failed runs landed one badcase each: {badcases:?}");

    // The plan's field list, at the row a person actually reads: the goal, why
    // it stopped, the ladder distribution, and the handoff they were shown.
    for row in &badcases {
        assert_eq!(row["source"], "computer:run", "{row}");
        assert_eq!(row["response"], json!("need_help: no failed step after 0 completed step(s)"));
        let detail = &row["detail"];
        assert_eq!(detail["stop_reason"], "need_help", "{detail}");
        assert!(detail["goal"].is_string(), "the goal survives: {detail}");
        assert!(
            detail["rung_distribution"].is_object(),
            "the ladder distribution is recorded even when empty: {detail}"
        );
        assert!(detail["handoff"].is_object(), "the handoff note is kept: {detail}");
    }

    // ── 2. A person confirms what "done" would have looked like ───────────
    //
    // A second handle on the same database, pointed at a temp corpus: the
    // gateway's own store commits into the checkout's `evals/badcases`, and a
    // test must not edit the tree it is measuring.
    let corpus_root = tempfile::tempdir().expect("temp evals root");
    let badcases_dir = corpus_root.path().join("badcases");
    let confirming_store = PendingBadcaseStore::new(&db_url)
        .await
        .expect("second handle on the badcase pool")
        .with_commit_dir(badcases_dir.clone());

    let mut confirmed = Vec::new();
    for row in &badcases {
        let id = row["id"].as_str().expect("a row id").to_string();
        let expected = ExpectedEndState::new(
            "e2e-reviewer",
            vec![GoalCondition::Pattern {
                // The harness materialises the trial directory for us; the
                // condition checks the effect the model was asked to produce.
                command: "cat ${trial_dir}/response.txt".to_string(),
                must_contain: "EXPORTED".to_string(),
            }],
            "the report is exported",
        )
        .expect("a confirmed end state with a checkable condition");
        let path = confirming_store
            .confirm_and_commit(&id, &expected)
            .await
            .expect("commit the confirmed badcase");
        assert!(path.starts_with(&badcases_dir), "committed into the temp corpus: {path:?}");
        confirmed.push(path);
    }
    assert_eq!(confirmed.len(), 2, "both rows became regression cases");

    // The corpus the verdict gate will read is the one we just wrote.
    let suite =
        syscity::eval::load_badcase_suite(corpus_root.path()).expect("load the confirmed corpus");
    assert_eq!(suite.tasks.len(), 2, "both confirmed cases are tasks");

    // ── 3. Measure a candidate against that corpus, and revert it ─────────
    let before = { state.config.read().await.default_agent.temperature };

    // The production verdict gate, reading the confirmed corpus: two tasks per
    // side is what the bootstrap needs to decide anything at all.
    let verifier =
        HarnessCandidateVerifier::new(state.clone(), "badcases".to_string(), 2, 1_000, 0.95, None)
            .with_evals_dir(corpus_root.path().to_path_buf());

    let report = ScalarOptimizer::new(state.infra.optimizer.clone())
        .run(
            state.clone(),
            OptimizerRunParams {
                verifier: Some(Arc::new(verifier)),
                shadow: Some(Arc::new(AnomalyAfterApply::default())),
                ..Default::default()
            },
        )
        .await;

    // The candidate was measured `Improved` on the confirmed corpus, applied,
    // then reverted when the post-apply signals turned: the plan's "变差则回滚".
    assert_eq!(
        report.reason, "online_anomaly_rollback",
        "a regression must revert, not ship: {report:?}"
    );
    assert!(report.applied.is_empty(), "the rolled-back patch is not applied: {report:?}");
    assert_eq!(report.rejected.len(), 1, "{report:?}");
    assert_eq!(report.rejected[0].path, "default_agent.temperature");
    assert_eq!(report.rejected[0].reason, "online_anomaly_rollback");

    // The scalar is back where it started — that is the revert, not a promise.
    let after = { state.config.read().await.default_agent.temperature };
    assert!(
        (after - before).abs() < 1e-6,
        "temperature reverted to its pre-run value: {before} -> {after}"
    );

    // Both sides ran: the harness evaluated the baseline at 0.7 and the
    // candidate at 0.8, which is only possible if the confirmed corpus was the
    // thing being measured.
    let temperatures = provider.temperatures_seen();
    let saw = |want: f32| {
        temperatures
            .iter()
            .flatten()
            .any(|t| (*t - want).abs() < 1e-3)
    };
    assert!(saw(0.7), "the baseline ran at the configured temperature: {temperatures:?}");
    assert!(saw(0.8), "the candidate ran at its proposed temperature: {temperatures:?}");

    // And the audit trail says so in the same terms: an apply carrying the
    // verdict that let it through, and a rollback carrying the reason.
    let traces = state
        .infra
        .decision_trace_store
        .as_ref()
        .expect("the gateway wires a decision-trace store")
        .list(None, 20)
        .await
        .expect("list decision traces");

    let apply = traces
        .iter()
        .find(|t| t.kind == TraceKind::OptimizerApply)
        .expect("the candidate was applied before it was reverted");
    assert_eq!(
        apply.evidence["verdict"]["comparison"]["verdict"], "Improved",
        "the apply was earned by the confirmed corpus: {apply:?}"
    );
    assert_eq!(
        apply.evidence["verdict"]["comparison"]["old_pass_rate"], 0.0,
        "the baseline could not export: {apply:?}"
    );
    assert_eq!(
        apply.evidence["verdict"]["comparison"]["new_pass_rate"], 1.0,
        "the candidate exported every trial: {apply:?}"
    );

    let rollback = traces
        .iter()
        .find(|t| t.kind == TraceKind::Rollback)
        .expect("a rollback trace is recorded");
    assert_eq!(rollback.subject, "default_agent.temperature");
    assert_eq!(rollback.evidence["reason"], "online_anomaly_rollback");
    let reverted_to = rollback.payload["to"]
        .as_f64()
        .expect("the rollback records the value it restored");
    assert!(
        (reverted_to - before as f64).abs() < 1e-6,
        "the rollback restored the pre-candidate value: {rollback:?}"
    );
}
