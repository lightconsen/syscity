//! Planner DAG execution journey.
//!
//! `src/planner/` decomposes goals into task DAGs, executes them with real
//! concurrency, verifies outcomes and rolls back on failure — and had no test
//! above unit level. The only live end-to-end planner test asserted the
//! *fallback* branch (no adapter → normal chat); the DAG executor itself was
//! never driven through `tests/`.
//!
//! These tests drive `TaskExecutor` against the headless adapter. The only
//! action usable without a display is `Wait` (sleeps); everything else wants
//! `xdotool`/Xvfb, so the DAGs below model dependency ordering and failure
//! propagation without pretending to click anything.

use std::sync::Arc;

use syscity::computer::headless::HeadlessComputerAdapter;
use syscity::computer::{DesktopAction, VerificationEngine};
use syscity::planner::dag::DagValidation;
use syscity::planner::executor::ExecutorConfig;
use syscity::planner::{DagScheduler, Plan, Task, TaskExecutor, TaskStatus};

fn executor(concurrency: usize, rollback: bool) -> TaskExecutor {
    let adapter = Arc::new(HeadlessComputerAdapter::new());
    let verifier = VerificationEngine::new(adapter.clone());
    TaskExecutor::new(adapter, verifier).with_config(ExecutorConfig {
        max_concurrency: concurrency,
        enable_rollback: rollback,
        ..Default::default()
    })
}

/// Case 1 — a dependency chain runs to completion in order.
#[tokio::test]
async fn a_dependency_chain_completes() {
    let executor = executor(2, false);

    let mut plan = Plan::new("three-step no-op chain");
    plan.add_task(Task::new("a", "first", DesktopAction::Wait { milliseconds: 0 }));
    plan.add_task(
        Task::new("b", "second", DesktopAction::Wait { milliseconds: 0 }).depends_on("a"),
    );
    plan.add_task(Task::new("c", "third", DesktopAction::Wait { milliseconds: 0 }).depends_on("b"));

    let result = executor.execute(&mut plan).await.expect("execution");

    assert!(result.success, "the chain must succeed: {result:?}");
    assert_eq!(result.tasks_completed, 3);
    assert_eq!(result.tasks_failed, 0);
    assert!(plan.is_complete());
    for id in ["a", "b", "c"] {
        assert_eq!(
            plan.get_task(id).map(|t| t.status.clone()),
            Some(TaskStatus::Completed),
            "task {id} must be completed"
        );
    }
}

/// Case 2 — the DAG scheduler agrees with the plan's dependencies: nothing is
/// ready until its dependencies completed, and a cycle is refused outright.
#[tokio::test]
async fn scheduler_orders_tasks_by_dependency_and_refuses_cycles() {
    let mut plan = Plan::new("diamond");
    plan.add_task(Task::new("a", "root", DesktopAction::Wait { milliseconds: 0 }));
    plan.add_task(Task::new("b", "left", DesktopAction::Wait { milliseconds: 0 }).depends_on("a"));
    plan.add_task(Task::new("c", "right", DesktopAction::Wait { milliseconds: 0 }).depends_on("a"));
    plan.add_task(Task::new("d", "join", DesktopAction::Wait { milliseconds: 0 }).depends_on("b"));

    let scheduler = DagScheduler::from_plan(&plan).expect("an acyclic diamond must validate");
    let order = scheduler.order();
    let pos = |id: &str| order.iter().position(|t| t == id).expect("task in order");
    assert!(pos("a") < pos("b") && pos("a") < pos("c"), "root precedes its children");
    assert!(pos("b") < pos("d"), "join follows the branch it depends on");

    // A cycle is refused.
    let mut cyclic = Plan::new("cycle");
    cyclic.add_task(Task::new("x", "x", DesktopAction::Wait { milliseconds: 0 }).depends_on("y"));
    cyclic.add_task(Task::new("y", "y", DesktopAction::Wait { milliseconds: 0 }).depends_on("x"));
    assert!(
        matches!(DagScheduler::from_plan(&cyclic), Err(DagValidation::Cycle { .. })),
        "a cycle must be reported, not scheduled"
    );
}

/// Case 3 — a failed task does not silently pass: the plan reports the
/// failure count, the failed task keeps its status, and its dependents are
/// not executed.
#[tokio::test]
async fn a_failed_task_marks_the_plan_and_skips_dependents() {
    // A wait with a non-zero budget against a zeroed verification budget would
    // be flaky; instead fail deterministically by making the task's action
    // unsupported: `Click` on the headless adapter returns `NoDisplay`.
    let executor = executor(1, false);

    let mut plan = Plan::new("failure propagation");
    plan.add_task(Task::new(
        "bad",
        "unsupported on a headless host",
        DesktopAction::Click {
            target: syscity::computer::ClickTarget::Coordinate(syscity::computer::Point {
                x: 1,
                y: 1,
            }),
            button: syscity::computer::MouseButton::Left,
        },
    ));
    plan.add_task(
        Task::new("child", "depends on the failure", DesktopAction::Wait { milliseconds: 0 })
            .depends_on("bad"),
    );

    let result = executor
        .execute(&mut plan)
        .await
        .expect("execution returns a result");

    assert!(!result.success, "a failed task must not report success: {result:?}");
    assert!(result.tasks_failed >= 1, "the failed task is counted: {result:?}");
    assert_eq!(
        plan.get_task("bad").map(|t| t.status.clone()),
        Some(TaskStatus::Failed),
        "the failed task keeps its status"
    );
    assert_eq!(
        plan.get_task("child").map(|t| t.status.clone()),
        Some(TaskStatus::Pending),
        "a dependent of a failed task must not run"
    );
    assert!(!plan.is_complete(), "an incomplete plan must not claim completion");
}
