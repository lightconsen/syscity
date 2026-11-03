//! Cross-platform action contract, driven by a fake adapter.
//!
//! The desktop ladder makes promises that must hold on every platform, not
//! just the one in front of us: an ambiguous label is refused rather than
//! guessed, a stale id is reported as stale (not "not found"), a disabled
//! control is refused before anything is dispatched, an accessibility action
//! nobody observed is not reported as confirmed, a point the platform's hit
//! test says is covered is refused with the occluder named, a miss in a tree
//! that was not read in full is not a miss in a whole one, and an action the
//! delivery mode cannot carry is refused instead of being delivered some
//! other way.
//!
//! This suite pins those promises against one `ContractAdapter`. A real
//! platform adapter can be pointed at the same assertions by implementing the
//! same fixture shape; until then this is the contract, not evidence that all
//! three platforms pass it.

use std::sync::Arc;

use async_trait::async_trait;
use syscity::computer::element_index::{assign_ids, ResolveError, SnapshotCache, UiSnapshot};
use syscity::computer::element_index::{parse_element_id, ActionTarget, ResolvedElement};
use syscity::computer::session::{ComputerSession, SessionConfig, SettleConfig};
use syscity::computer::{
    ActionEffect, ActionResult, AxAction, ClickTarget, ComputerAdapter, ComputerError,
    DeliveryDisposition, DeliveryMode, DesktopAction, HitTestResult, MouseButton, Occluder, Point,
    Rect, Result as CResult, RetryDisposition, Screenshot, Tri, UiElement, WaitCondition,
};

/// What a contract adapter's element says about itself.
#[derive(Clone, Copy)]
struct ElementState {
    enabled: Tri,
    actions: Option<&'static [&'static str]>,
}

const ENABLED: ElementState = ElementState {
    enabled: Tri::Known(true),
    actions: Some(&["press"]),
};

/// An element that declares no actions: the accessibility rung is not in its
/// ladder, so a click must take the pixel rung.
const NO_AX: ElementState = ElementState {
    enabled: Tri::Known(true),
    actions: Some(&[]),
};

/// An adapter whose behaviour is fully described by its constructor.
struct ContractAdapter {
    /// Whether the platform can deliver input without the foreground.
    supports_background: bool,
    /// What the platform's hit test says, when it has one.
    hit_test: Option<HitTestResult>,
    /// The elements a tree read returns, under one window root.
    elements: Vec<UiElement>,
    /// Whether the tree read stopped before the tree was whole.
    tree_truncated: bool,
    /// Every call that reached the adapter, for "did anything happen" checks.
    delivered: std::sync::Mutex<Vec<String>>,
    /// How long every call takes. Zero unless a test is about the budget: a
    /// real platform call is not instantaneous, and a ceiling test needs
    /// calls that can outlast it.
    delay: std::time::Duration,
}

impl ContractAdapter {
    fn new(state: ElementState, supports_background: bool) -> Self {
        Self::with_hit_test(state, supports_background, None)
    }

    /// Make every adapter call take `ms` milliseconds, as a slow desktop
    /// would. The budget test is the only caller.
    fn slow(mut self, ms: u64) -> Self {
        self.delay = std::time::Duration::from_millis(ms);
        self
    }

    async fn pause(&self) {
        if !self.delay.is_zero() {
            tokio::time::sleep(self.delay).await;
        }
    }

    fn with_hit_test(
        state: ElementState,
        supports_background: bool,
        hit_test: Option<HitTestResult>,
    ) -> Self {
        Self {
            supports_background,
            hit_test,
            tree_truncated: false,
            delivered: std::sync::Mutex::new(Vec::new()),
            delay: std::time::Duration::ZERO,
            elements: vec![UiElement {
                id: String::new(),
                role: "window".to_string(),
                label: Some("Contract".to_string()),
                value: None,
                bounds: Some(Rect::new(0, 0, 400, 300)),
                enabled: Tri::Known(true),
                focused: Tri::Unknown,
                actions: None,
                children: vec![
                    UiElement {
                        id: "save".to_string(),
                        role: "button".to_string(),
                        label: Some("Save".to_string()),
                        value: None,
                        bounds: Some(Rect::new(10, 10, 40, 20)),
                        enabled: state.enabled,
                        focused: Tri::Unknown,
                        actions: state
                            .actions
                            .map(|a| a.iter().map(|s| s.to_string()).collect()),
                        children: vec![],
                        children_count: None,
                        truncated: false,
                    },
                    UiElement {
                        id: "save_as".to_string(),
                        role: "button".to_string(),
                        label: Some("Save As…".to_string()),
                        value: None,
                        bounds: Some(Rect::new(60, 10, 80, 20)),
                        enabled: Tri::Known(true),
                        focused: Tri::Unknown,
                        actions: None,
                        children: vec![],
                        children_count: None,
                        truncated: false,
                    },
                    UiElement {
                        id: String::new(),
                        role: "button".to_string(),
                        label: Some("Duplicate".to_string()),
                        value: None,
                        bounds: Some(Rect::new(10, 40, 40, 20)),
                        enabled: Tri::Known(true),
                        focused: Tri::Unknown,
                        actions: None,
                        children: vec![],
                        children_count: None,
                        truncated: false,
                    },
                    UiElement {
                        id: String::new(),
                        role: "button".to_string(),
                        label: Some("Duplicate".to_string()),
                        value: None,
                        bounds: Some(Rect::new(60, 40, 40, 20)),
                        enabled: Tri::Known(true),
                        focused: Tri::Unknown,
                        actions: None,
                        children: vec![],
                        children_count: None,
                        truncated: false,
                    },
                ],
                children_count: None,
                truncated: false,
            }],
        }
    }

    fn delivered(&self) -> Vec<String> {
        self.delivered.lock().expect("lock").clone()
    }
}

#[async_trait]
impl ComputerAdapter for ContractAdapter {
    async fn screenshot(&self, _region: Option<Rect>) -> CResult<Screenshot> {
        self.pause().await;
        Ok(Screenshot::new(String::new(), 400, 300))
    }

    async fn read_ui_tree(&self, _app: Option<&str>) -> CResult<Vec<UiElement>> {
        self.pause().await;
        let mut roots = self.elements.clone();
        // A truncated read shows the same partial tree, but says so.
        for root in &mut roots {
            if self.tree_truncated {
                root.truncated = true;
            }
        }
        Ok(roots)
    }

    async fn resolve_target(&self, target: &ClickTarget) -> CResult<ActionTarget> {
        // A real adapter resolves against its own snapshot cache, where
        // snapshot-path ids carry their provenance. This one holds one tree,
        // so it answers live platform ids directly and treats every
        // snapshot-path id as one from a tree it no longer has.
        match target {
            ClickTarget::Coordinate(p) => Ok(ActionTarget::point(p.x, p.y)),
            ClickTarget::ElementId(id) if parse_element_id(id).is_none() && id == "live-save" => {
                Ok(self.element_target("save"))
            }
            ClickTarget::ElementId(id) => Err(ComputerError::StaleElement(format!(
                "element id {id} belongs to a snapshot that is no longer cached"
            ))),
            ClickTarget::ElementLabel(label) | ClickTarget::ElementRoleLabel { label, .. } => {
                self.resolve_label(label)
            }
        }
    }

    async fn perform_ax_action(
        &self,
        target: &ActionTarget,
        action: AxAction,
    ) -> CResult<ActionResult> {
        if target.element.is_none() {
            return Err(ComputerError::UnsupportedPlatform(
                "an accessibility action needs an element target".to_string(),
            ));
        }
        self.pause().await;
        self.delivered
            .lock()
            .expect("lock")
            .push(format!("ax:{action:?}"));
        // The driver sent the action; it did not observe the effect.
        Ok(ActionResult::success("sent").with_evidence(
            syscity::computer::ActionEvidence::new(
                syscity::computer::Rung::AxAction,
                ActionEffect::Unverifiable,
            )
            .with_delivery(DeliveryDisposition::DeliveredUnverified),
        ))
    }

    async fn execute(&self, action: DesktopAction) -> CResult<ActionResult> {
        if matches!(action, DesktopAction::Screenshot { .. }) {
            return Ok(ActionResult::success("screenshot captured"));
        }
        // The input-layer rungs act on a point, and a real adapter gets that
        // point by resolving the target. Resolution failures are therefore
        // this call's failures too — a fake that skipped them would make a
        // stale id look like a delivered click.
        if let DesktopAction::Click { target, .. }
        | DesktopAction::DoubleClick { target, .. }
        | DesktopAction::Scroll { target, .. } = &action
        {
            self.resolve_target(target).await?;
        }
        self.delivered
            .lock()
            .expect("lock")
            .push(format!("foreground:{action:?}"));
        Ok(ActionResult::success("delivered"))
    }

    async fn perform_background(
        &self,
        action: &DesktopAction,
        _target: Option<&ActionTarget>,
    ) -> CResult<ActionResult> {
        if !self.supports_background {
            return Err(ComputerError::BackgroundInputUnavailable(
                "this platform has no background input path".to_string(),
            ));
        }
        self.delivered
            .lock()
            .expect("lock")
            .push(format!("background:{action:?}"));
        Ok(ActionResult::success("delivered in the background"))
    }

    fn supports_background_input(&self) -> bool {
        self.supports_background
    }

    async fn hit_test(&self, _point: Point, _target: Option<&ActionTarget>) -> HitTestResult {
        // `None` is "this platform has no hit test", which must behave like
        // the trait default: unknown, and never a refusal.
        self.hit_test.clone().unwrap_or(HitTestResult::Unknown)
    }

    async fn wait_for(
        &self,
        _condition: WaitCondition,
        _timeout: std::time::Duration,
    ) -> CResult<bool> {
        Ok(true)
    }
}

impl ContractAdapter {
    /// Resolve a label the way the snapshot cache does: exact match first,
    /// then substring, and never a pick among equals.
    fn resolve_label(&self, label: &str) -> CResult<ActionTarget> {
        let leaves: Vec<&UiElement> = self
            .elements
            .iter()
            .flat_map(|root| root.children.iter())
            .collect();
        let exact: Vec<&&UiElement> = leaves
            .iter()
            .filter(|el| el.label.as_deref() == Some(label))
            .collect();
        let matches: Vec<&&UiElement> = if exact.is_empty() {
            leaves
                .iter()
                .filter(|el| el.label.as_deref().is_some_and(|l| l.contains(label)))
                .collect()
        } else {
            exact
        };
        match matches.as_slice() {
            [] if self.tree_truncated => Err(ComputerError::TreeIncomplete(format!(
                "label~{label}: no match, but the tree read stopped early"
            ))),
            [] => Err(ComputerError::ElementNotFound(label.to_string())),
            [only] => Ok(self.element_target(&only.id)),
            many => Err(ComputerError::AmbiguousTarget(format!(
                "{} elements match {label}; refusing to pick one",
                many.len()
            ))),
        }
    }

    fn element_target(&self, id: &str) -> ActionTarget {
        let element = self
            .elements
            .iter()
            .flat_map(|root| root.children.iter())
            .find(|el| el.id == id)
            .expect("fixture element");
        ActionTarget {
            point: element.center(),
            element: Some(ResolvedElement {
                id: element.id.clone(),
                role: element.role.clone(),
                label: element.label.clone(),
                center: element.center(),
                bounds: element.bounds,
                path: vec![0, 0],
                actions: element.actions.clone(),
                enabled: element.enabled,
            }),
            app: Some("Contract".to_string()),
            snapshot: None,
        }
    }
}

fn session(adapter: Arc<ContractAdapter>) -> ComputerSession {
    ComputerSession::new(adapter).with_config(SessionConfig {
        settle: SettleConfig {
            initial_delay_ms: 0,
            poll_interval_ms: 0,
            max_rounds: 1,
            stable_threshold: 0.5,
        },
        ..SessionConfig::default()
    })
}

fn background_session(adapter: Arc<ContractAdapter>) -> ComputerSession {
    ComputerSession::new(adapter).with_config(SessionConfig {
        delivery: DeliveryMode::Background,
        settle: SettleConfig {
            initial_delay_ms: 0,
            poll_interval_ms: 0,
            max_rounds: 1,
            stable_threshold: 0.5,
        },
        ..SessionConfig::default()
    })
}

fn click(target: ClickTarget) -> DesktopAction {
    DesktopAction::Click {
        target,
        button: MouseButton::Left,
    }
}

// ── Contract: addressing ────────────────────────────────────────────────

/// A stale snapshot id is reported as stale, not as "not found": the first
/// tells the caller to re-read and aim again, the second says the thing does
/// not exist.
#[tokio::test]
async fn a_stale_id_is_refused_as_stale() {
    let adapter = Arc::new(ContractAdapter::new(ENABLED, true));
    let outcome = session(adapter.clone())
        .execute(click(ClickTarget::ElementId("s00000001:0.1".to_string())))
        .await
        .expect("a refusal is an outcome");

    assert!(adapter.delivered().is_empty(), "nothing was dispatched");
    let evidence = outcome.evidence.expect("evidence");
    assert_eq!(evidence.effect, ActionEffect::Refused);
    assert_eq!(evidence.delivery, DeliveryDisposition::NotDelivered);
    assert_eq!(evidence.retry, RetryDisposition::Safe);
    assert!(!outcome.result.success);
}

/// Two elements with the same label are an error, not a choice. Picking the
/// first one is how a click lands on the wrong button and reports success.
#[tokio::test]
async fn an_ambiguous_label_is_refused() {
    let adapter = Arc::new(ContractAdapter::new(ENABLED, true));
    let session = session(adapter.clone());
    // A tree read seeds the snapshot the label is resolved against.
    session
        .execute(DesktopAction::Screenshot { region: None })
        .await
        .expect("screenshot");
    let outcome = session
        .execute(click(ClickTarget::ElementLabel("Duplicate".to_string())))
        .await
        .expect("a refusal is an outcome");
    assert!(!outcome.result.success);
    let message = outcome.result.message;
    assert!(message.contains("refusing to pick one"), "{message}");
    let evidence = outcome.evidence.expect("evidence");
    assert_eq!(evidence.effect, ActionEffect::Refused);
    assert_eq!(evidence.delivery, DeliveryDisposition::NotDelivered);
    let recovery = evidence.recovery.expect("a refusal carries a hint");
    assert!(recovery.contains("element id"), "{recovery}");
    assert!(adapter.delivered().is_empty(), "nothing was dispatched");
}

/// A disabled control is refused before any rung runs: clicking it would click
/// whatever draws on top of it.
#[tokio::test]
async fn a_disabled_control_is_refused_before_dispatch() {
    let state = ElementState {
        enabled: Tri::Known(false),
        actions: Some(&["press"]),
    };
    let adapter = Arc::new(ContractAdapter::new(state, true));
    let outcome = session(adapter.clone())
        .execute(click(ClickTarget::ElementId("live-save".to_string())))
        .await
        .expect("a refusal is an outcome");

    assert!(adapter.delivered().is_empty(), "{:?}", adapter.delivered());
    let evidence = outcome.evidence.expect("evidence");
    assert_eq!(evidence.effect, ActionEffect::Refused);
    assert!(outcome.result.message.contains("disabled"), "{}", outcome.result.message);
}

/// An action the delivery mode cannot carry is refused, not escalated into the
/// foreground: the caller asked for background delivery for a reason.
#[tokio::test]
async fn background_undeliverable_actions_are_refused() {
    let adapter = Arc::new(ContractAdapter::new(ENABLED, false));
    let outcome = background_session(adapter.clone())
        .execute(click(ClickTarget::Coordinate(Point::new(20, 20))))
        .await
        .expect("a refusal is an outcome");

    assert!(adapter.delivered().is_empty(), "{:?}", adapter.delivered());
    let evidence = outcome.evidence.expect("evidence");
    assert_eq!(evidence.effect, ActionEffect::Refused);
    assert!(evidence.recovery.unwrap_or_default().contains("foreground"));
}

/// An ax success is a delivery, not a confirmation: the driver told us it sent
/// the action, not that the effect happened.
#[tokio::test]
async fn an_unobserved_ax_success_is_not_confirmed() {
    let adapter = Arc::new(ContractAdapter::new(ENABLED, true));
    let outcome = session(adapter.clone())
        .execute(click(ClickTarget::ElementId("live-save".to_string())))
        .await
        .expect("an outcome");
    let evidence = outcome.evidence.expect("evidence");
    assert_ne!(evidence.effect, ActionEffect::Confirmed);
    assert_eq!(evidence.delivery, DeliveryDisposition::DeliveredUnverified);
    assert_eq!(
        evidence.retry,
        RetryDisposition::Unsafe,
        "a delivered action is not retried blindly"
    );
}

// ── Contract: reachability ──────────────────────────────────────────────

/// A pixel-rung click only reaches its element if nothing covers the point.
/// When the platform says something else is there, that is a refusal with the
/// occluder named — not a click that silently lands on the dialog.
#[tokio::test]
async fn an_intercepted_point_is_refused_with_the_occluder() {
    // No declared actions: the accessibility rung is not in the ladder, so
    // the pixel rung is where the action would go.
    let blind = NO_AX;
    let occluder = Occluder::new("dialog", Some("Save changes?".to_string()))
        .with_bounds(Rect::new(0, 0, 200, 100));
    let adapter = Arc::new(ContractAdapter::with_hit_test(
        blind,
        true,
        Some(HitTestResult::InterceptedBy(occluder.clone())),
    ));
    let outcome = session(adapter.clone())
        .execute(click(ClickTarget::ElementId("live-save".to_string())))
        .await
        .expect("a refusal is an outcome");

    assert!(adapter.delivered().is_empty(), "{:?}", adapter.delivered());
    let evidence = outcome.evidence.expect("evidence");
    assert_eq!(evidence.effect, ActionEffect::Refused);
    assert_eq!(evidence.delivery, DeliveryDisposition::NotDelivered);
    assert_eq!(evidence.retry, RetryDisposition::Safe);
    assert_eq!(evidence.occluder, Some(occluder.clone()));
    assert!(evidence
        .recovery
        .expect("recovery")
        .contains("Save changes?"));
    assert!(!outcome.result.success);
    assert!(outcome.result.message.contains("Save changes?"), "{}", outcome.result.message);
}

/// An unanswered hit test is "the check did not happen", not "clear" — but it
/// is not a refusal either. A platform without a hit test, or without an
/// accessibility grant, must keep working exactly as before.
#[tokio::test]
async fn an_unknown_hit_test_does_not_block_the_action() {
    let blind = NO_AX;
    let adapter =
        Arc::new(ContractAdapter::with_hit_test(blind, true, Some(HitTestResult::Unknown)));
    let outcome = session(adapter.clone())
        .execute(click(ClickTarget::ElementId("live-save".to_string())))
        .await
        .expect("an outcome");

    assert!(
        adapter
            .delivered()
            .iter()
            .any(|call| call.starts_with("foreground:")),
        "the pixel rung still ran: {:?}",
        adapter.delivered()
    );
    let evidence = outcome.evidence.expect("evidence");
    assert_ne!(evidence.effect, ActionEffect::Refused);
    assert!(evidence.occluder.is_none());
}

/// A clear hit test lets the action through, and carries no occluder.
#[tokio::test]
async fn a_clear_hit_test_lets_the_action_through() {
    let blind = NO_AX;
    let adapter =
        Arc::new(ContractAdapter::with_hit_test(blind, true, Some(HitTestResult::ReachesTarget)));
    let outcome = session(adapter.clone())
        .execute(click(ClickTarget::ElementId("live-save".to_string())))
        .await
        .expect("an outcome");

    assert!(
        adapter
            .delivered()
            .iter()
            .any(|call| call.starts_with("foreground:")),
        "{:?}",
        adapter.delivered()
    );
    let evidence = outcome.evidence.expect("evidence");
    assert_ne!(evidence.effect, ActionEffect::Refused);
    assert!(evidence.occluder.is_none());
}

/// The hit test runs only where a point is aimed at. An accessibility action
/// names the element itself and cannot land on a dialog, so a hit test must
/// not refuse it — and, having nothing to check, must not even run.
#[tokio::test]
async fn an_element_action_is_not_gated_on_the_hit_test() {
    let occluder = Occluder::new("dialog", Some("Save changes?".to_string()));
    let adapter = Arc::new(ContractAdapter::with_hit_test(
        ENABLED,
        true,
        Some(HitTestResult::InterceptedBy(occluder)),
    ));
    let outcome = session(adapter.clone())
        .execute(click(ClickTarget::ElementId("live-save".to_string())))
        .await
        .expect("an outcome");

    assert!(
        adapter.delivered().iter().any(|call| call == "ax:Press"),
        "the ax rung ran: {:?}",
        adapter.delivered()
    );
    let evidence = outcome.evidence.expect("evidence");
    assert_ne!(evidence.effect, ActionEffect::Refused);
    assert!(evidence.occluder.is_none());
}

// ── Contract: a partial tree cannot say "not found" ─────────────────────

/// The same tree, read whole and read in part, must answer a miss
/// differently: one says the element is not there, the other says this read
/// did not see it. The two lead to different next actions.
#[test]
fn a_miss_in_a_partial_tree_is_not_a_miss_in_a_whole_one() {
    let adapter = ContractAdapter::new(ENABLED, true);
    let whole: Vec<UiElement> = vec![adapter.elements[0].clone()];
    let mut partial = whole.clone();
    partial[0].children_count = Some(40);
    partial[0].truncated = true;

    let whole_snapshot = UiSnapshot { id: 1, app: None, roots: whole };
    let partial_snapshot = UiSnapshot {
        id: 2,
        app: None,
        roots: partial,
    };
    let query = ClickTarget::ElementLabel("Quit".to_string());

    assert!(matches!(whole_snapshot.resolve(&query), Err(ResolveError::NotFound(_))));
    match partial_snapshot.resolve(&query) {
        Err(ResolveError::Incomplete { searched, .. }) => {
            assert_eq!(searched, 5, "the error counts what was read");
        }
        other => panic!("expected Incomplete, got {other:?}"),
    }
}

/// A partial tree still resolves what it did read: the new state must not
/// turn every lookup into a refusal.
#[test]
fn a_partial_tree_still_resolves_what_it_holds() {
    let adapter = ContractAdapter::new(ENABLED, true);
    let mut roots = vec![adapter.elements[0].clone()];
    roots[0].truncated = true;
    assign_ids(&mut roots, 3);
    let snapshot = UiSnapshot { id: 3, app: None, roots };
    let target = snapshot
        .resolve(&ClickTarget::ElementLabel("Save".to_string()))
        .expect("a present element resolves even in a partial tree");
    assert_eq!(target.element.expect("element").id, "save");
}

/// The refusal tells the model which of the two it is. "Re-read deeper" is
/// only useful if it is not confused with "aim somewhere else".
#[tokio::test]
async fn a_partial_tree_refusal_says_to_read_deeper() {
    let mut adapter = ContractAdapter::new(ENABLED, true);
    adapter.tree_truncated = true;
    let adapter = Arc::new(adapter);
    let session = session(adapter.clone());
    let outcome = session
        .execute(click(ClickTarget::ElementLabel("Quit".to_string())))
        .await
        .expect("a refusal is an outcome");

    assert!(adapter.delivered().is_empty(), "nothing was dispatched");
    assert!(!outcome.result.success);
    let evidence = outcome.evidence.expect("evidence");
    assert_eq!(evidence.effect, ActionEffect::Refused);
    assert_eq!(evidence.retry, RetryDisposition::Safe);
    let recovery = evidence.recovery.expect("a refusal carries a hint");
    assert!(recovery.contains("max_depth"), "{recovery}");
    assert!(!recovery.contains("screen coordinates"), "{recovery}");
}

// ── Contract: the step budget ───────────────────────────────────────────

/// A ceiling set on the session ends the step as a refusal that names what it
/// ran out of. This is a contract because every platform spends time
/// differently — an AppleScript round trip, a PowerShell start-up, a pyatspi
/// tree walk — and none of them may hand the caller a bare "timed out".
#[tokio::test]
async fn a_spent_budget_is_a_refusal_that_names_the_budget() {
    let adapter = Arc::new(ContractAdapter::new(ENABLED, true).slow(50));
    let spent = session(adapter.clone()).with_config(SessionConfig {
        settle: SettleConfig {
            initial_delay_ms: 0,
            poll_interval_ms: 0,
            max_rounds: 1,
            stable_threshold: 0.5,
        },
        step_budget: Some(std::time::Duration::from_millis(20)),
        ..SessionConfig::default()
    });
    let outcome = spent
        .execute(click(ClickTarget::ElementLabel("Save".to_string())))
        .await
        .expect("a spent budget is an outcome");

    assert!(!outcome.result.success);
    assert!(
        adapter.delivered().is_empty(),
        "nothing was dispatched after the ceiling: {:?}",
        adapter.delivered()
    );
    let evidence = outcome.evidence.expect("evidence");
    assert_eq!(evidence.effect, ActionEffect::Refused);
    assert!(
        evidence
            .recovery
            .as_deref()
            .is_some_and(|r| r.contains("budget")),
        "{:?}",
        evidence.recovery
    );
    let budget = outcome.budget.expect("budget reported");
    assert_eq!(budget["total_ms"], 20);
    assert_eq!(budget["remaining_ms"], 0);
}
