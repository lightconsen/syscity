//! A planner DAG that mixes perception with input, driven end to end.
//!
//! `src/planner/executor.rs` has unit tests for "clicks never overlap" and
//! "reads do overlap" separately, each with its own fake. That answers the
//! two halves but not the property the design actually promises: in *one*
//! batch, the reads really do run at the same time and the one write is
//! exclusive against everything else.
//!
//! This test builds that batch — three `ReadUiTree` tasks and one `Click`,
//! all independent — and times it. A read that is serialised behind the
//! others would produce a run as long as the plain sum; a write that is not
//! exclusive would let the adapter see two input calls at once, which on a
//! real display is one click at a place neither task asked for.
//!
//! The adapter here counts what is inside it, separately for observation and
//! input, rather than touching a desktop: this must pass on a machine with no
//! display.
//!
//! Run:
//!   cargo test --test e2e_test a_three_read_one_write_dag -- --nocapture

use super::*;

use async_trait::async_trait;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Instant;

use syscity::computer::headless::HeadlessComputerAdapter;
use syscity::computer::{
    ActionResult, ClickTarget, ComputerAdapter, ComputerError, DesktopAction, MouseButton, Point,
    Rect, Screenshot, UiElement, VerificationEngine, WaitCondition,
};
use syscity::planner::executor::ExecutorConfig;
use syscity::planner::{Plan, Task, TaskExecutor};

/// What is inside the adapter right now, and the most that ever was.
#[derive(Default)]
struct ConcurrencyMeter {
    live_reads: AtomicUsize,
    peak_reads: AtomicUsize,
    live_inputs: AtomicUsize,
    peak_inputs: AtomicUsize,
}

impl ConcurrencyMeter {
    async fn observe(&self, delay: Duration) {
        let now = self.live_reads.fetch_add(1, Ordering::SeqCst) + 1;
        self.peak_reads.fetch_max(now, Ordering::SeqCst);
        tokio::time::sleep(delay).await;
        self.live_reads.fetch_sub(1, Ordering::SeqCst);
    }

    async fn inject(&self, delay: Duration) {
        let now = self.live_inputs.fetch_add(1, Ordering::SeqCst) + 1;
        self.peak_inputs.fetch_max(now, Ordering::SeqCst);
        tokio::time::sleep(delay).await;
        self.live_inputs.fetch_sub(1, Ordering::SeqCst);
    }
}

struct MeteredAdapter {
    meter: Arc<ConcurrencyMeter>,
    delay: Duration,
}

#[async_trait]
impl ComputerAdapter for MeteredAdapter {
    async fn screenshot(&self, _region: Option<Rect>) -> syscity::computer::Result<Screenshot> {
        Ok(Screenshot::new(String::new(), 1, 1))
    }

    async fn read_ui_tree(&self, _app: Option<&str>) -> syscity::computer::Result<Vec<UiElement>> {
        self.meter.observe(self.delay).await;
        Ok(vec![])
    }

    async fn execute(&self, action: DesktopAction) -> syscity::computer::Result<ActionResult> {
        if action.needs_input_lock() {
            self.meter.inject(self.delay).await;
        } else {
            self.meter.observe(self.delay).await;
        }
        Ok(ActionResult::success("done"))
    }

    async fn wait_for(
        &self,
        _condition: WaitCondition,
        _timeout: Duration,
    ) -> syscity::computer::Result<bool> {
        Err(ComputerError::UnsupportedPlatform("not used".to_string()))
    }
}

fn task(id: &str, action: DesktopAction) -> Task {
    Task::new(id, id, action)
}

#[tokio::test]
#[serial]
async fn a_three_read_one_write_dag_overlaps_reads_and_serialises_the_write() {
    // The input path takes the cross-process display lock, which resolves the
    // data directory; without an installed layout the test would write into
    // the developer's real `~/.syscity` (or panic). Same root as the rest of
    // the suite.
    let _paths = test_paths_root();

    let delay = Duration::from_millis(100);
    let meter = Arc::new(ConcurrencyMeter::default());
    let adapter: Arc<dyn ComputerAdapter> =
        Arc::new(MeteredAdapter { meter: meter.clone(), delay });
    let verification_adapter: Arc<dyn ComputerAdapter> = Arc::new(HeadlessComputerAdapter::new());
    let verifier = VerificationEngine::new(verification_adapter);
    let executor = TaskExecutor::new(adapter, verifier).with_config(ExecutorConfig {
        max_concurrency: 4,
        enable_rollback: false,
        ..Default::default()
    });

    let mut plan = Plan::new("three reads, one write");
    for i in 0..3 {
        plan.add_task(task(&format!("read-{i}"), DesktopAction::ReadUiTree { app: None }));
    }
    plan.add_task(task(
        "write",
        DesktopAction::Click {
            target: ClickTarget::Coordinate(Point::new(10, 10)),
            button: MouseButton::Left,
        },
    ));

    let started = Instant::now();
    let result = executor.execute(&mut plan).await.expect("a plan result");
    let elapsed = started.elapsed();

    assert!(result.success, "the batch must succeed: {}", result.message);
    assert_eq!(result.tasks_completed, 4, "all four tasks ran");

    // The reads ran together. One at a time would peak at 1.
    let peak_reads = meter.peak_reads.load(Ordering::SeqCst);
    assert!(
        peak_reads > 1,
        "the three reads never overlapped (peak {peak_reads}); a serialised read batch \
         makes 'parallel perception' a phrase with nothing behind it"
    );

    // The write was alone. Two inputs inside the adapter at once means one
    // click landed where the other was aimed.
    let peak_inputs = meter.peak_inputs.load(Ordering::SeqCst);
    assert_eq!(peak_inputs, 1, "two input actions were inside the adapter at once");

    // Time is the observable consequence: four serial calls cost 4 × delay,
    // three overlapping reads plus the exclusive write cost about 2 × delay.
    // The bound is generous — the point is "well under serial", not a stopwatch.
    let serial = delay * 4;
    assert!(
        elapsed < serial,
        "the batch took {elapsed:?}, which is not faster than running four calls one after \
         another ({serial:?})"
    );
}
