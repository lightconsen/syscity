//! E2E tests for ComputerAdapter trait and ComputerUseLoop orchestration.
//!
//! Tests the full perceive → decide → act → verify cycle through
//! HeadlessComputerAdapter (no real display needed).  Real E2E tests that
//! exercise DesktopAction dispatch, loop state machine, rollback, and
//! verification integration.
//!
//! Run: cargo test --test integrations_test -- computer_adapter

use std::sync::Arc;
use std::time::Duration;

use syscity::computer::headless::HeadlessComputerAdapter;
use syscity::computer::types::{MouseButton, Point, Screenshot};
use syscity::computer::use_loop::{ComputerUseLoop, LoopConfig, LoopDecision, LoopState};
use syscity::computer::{ComputerAdapter, ComputerError, DesktopAction};

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn headless_adapter() -> HeadlessComputerAdapter {
    HeadlessComputerAdapter::new()
}

fn default_loop_config() -> LoopConfig {
    LoopConfig {
        max_steps: 10,
        settle_delay_ms: 5,
        verify_after_each: false,
        screenshot_region: None,
        ..Default::default()
    }
}

// ---------------------------------------------------------------------------
// ComputerAdapter — DesktopAction execution (headless, no display needed)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_adapter_get_system_status() {
    let adapter = headless_adapter();
    let result = adapter
        .execute(DesktopAction::GetSystemStatus)
        .await
        .expect("GetSystemStatus should succeed headlessly");
    assert!(result.success, "system status should succeed");
    assert!(!result.message.is_empty(), "should have status message");
    assert!(result.data.is_some(), "should have status data");

    let data = result.data.unwrap();
    assert!(
        data.get("hostname").is_some() || data.get("os").is_some(),
        "status data should contain system info keys: {:?}",
        data
    );
}

#[tokio::test]
async fn test_adapter_list_processes() {
    let adapter = headless_adapter();
    let result = adapter
        .execute(DesktopAction::ListProcesses { filter: None, limit: None })
        .await
        .expect("ListProcesses should succeed");
    assert!(result.success, "process list should succeed");
}

#[tokio::test]
async fn test_adapter_list_processes_with_filter() {
    let adapter = headless_adapter();
    let result = adapter
        .execute(DesktopAction::ListProcesses {
            filter: Some("ssh".to_string()),
            limit: Some(5),
        })
        .await
        .expect("ListProcesses with filter should succeed");
    assert!(result.success);
}

#[tokio::test]
async fn test_adapter_wait_action() {
    let adapter = headless_adapter();
    let start = std::time::Instant::now();
    let result = adapter
        .execute(DesktopAction::Wait { milliseconds: 20 })
        .await
        .expect("Wait should succeed");
    let elapsed = start.elapsed();
    assert!(result.success, "wait should succeed");
    assert!(elapsed >= Duration::from_millis(20), "should wait at least 20ms");
}

#[tokio::test]
async fn test_adapter_screenshot_fails_headlessly() {
    let adapter = headless_adapter();
    let result = adapter.screenshot(None).await;
    assert!(result.is_err(), "screenshot should fail without a display");
    match result {
        Err(ComputerError::NoDisplay) => {} // expected
        Err(e) => panic!("expected NoDisplay error, got: {:?}", e),
        Ok(_) => panic!("expected error, got Ok"),
    }
}

#[tokio::test]
async fn test_adapter_read_ui_tree_headless() {
    let adapter = headless_adapter();
    let elements = adapter
        .read_ui_tree(None)
        .await
        .expect("read_ui_tree should not error headlessly");
    assert!(elements.is_empty(), "headless mode should return empty tree");
}

#[tokio::test]
async fn test_adapter_convenience_click_at() {
    let adapter = headless_adapter();
    let result = adapter
        .click_at(Point::new(100, 200), MouseButton::Left)
        .await;
    // Click requires a display — expect error, but not a panic
    if let Err(e) = result {
        // Any error is acceptable — the key is it didn't panic
        assert!(!e.to_string().is_empty());
    }
}

#[tokio::test]
async fn test_adapter_convenience_type_text() {
    let adapter = headless_adapter();
    let result = adapter.type_text("hello").await;
    if let Err(e) = result {
        assert!(!e.to_string().is_empty(), "error should have a message");
    }
}

// ---------------------------------------------------------------------------
// ComputerUseLoop — orchestration loop (no real display needed)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_loop_immediate_done() {
    let adapter = Arc::new(headless_adapter());
    let ctrl = syscity::acp::ExecutionController::new();
    let loop_ = ComputerUseLoop::new(adapter)
        .with_config(default_loop_config())
        .with_execution_controller(ctrl);

    let result = loop_
        .run("do nothing", |_state: LoopState| async move {
            Ok(LoopDecision::Done {
                message: "nothing to do".into(),
            })
        })
        .await
        .expect("loop should complete without error");

    assert!(result.success, "immediate Done should be success");
    assert_eq!(result.steps_taken, 0, "no steps should be taken");
    assert_eq!(result.message, "nothing to do");
}

#[tokio::test]
async fn test_loop_single_action_then_done() {
    let adapter = Arc::new(headless_adapter());
    let ctrl = syscity::acp::ExecutionController::new();
    let mut step_count = 0;

    let loop_ = ComputerUseLoop::new(adapter)
        .with_config(default_loop_config())
        .with_execution_controller(ctrl);

    let result = loop_
        .run("wait a bit", |_state: LoopState| {
            let count = step_count;
            step_count += 1;
            async move {
                if count == 0 {
                    Ok(LoopDecision::Action(DesktopAction::Wait { milliseconds: 5 }))
                } else {
                    Ok(LoopDecision::Done { message: "done waiting".into() })
                }
            }
        })
        .await
        .expect("loop should complete without error");

    assert!(result.success);
    assert_eq!(result.steps_taken, 1, "should have taken 1 step");
    assert_eq!(result.history.len(), 1, "history should have 1 record");
    assert_eq!(result.history[0].action, DesktopAction::Wait { milliseconds: 5 },);
    assert!(result.history[0].result.success);
}

#[tokio::test]
async fn test_loop_max_steps_reached() {
    let adapter = Arc::new(headless_adapter());
    let ctrl = syscity::acp::ExecutionController::new();
    let config = LoopConfig {
        max_steps: 3,
        settle_delay_ms: 1,
        verify_after_each: false,
        screenshot_region: None,
        ..Default::default()
    };

    let loop_ = ComputerUseLoop::new(adapter)
        .with_config(config)
        .with_execution_controller(ctrl);

    let result = loop_
        .run("keep waiting", |_state: LoopState| async move {
            Ok(LoopDecision::Action(DesktopAction::Wait { milliseconds: 1 }))
        })
        .await
        .expect("loop should complete without error");

    assert!(!result.success, "max steps should result in failure");
    assert_eq!(result.steps_taken, 3, "should have taken 3 steps");
    assert_eq!(result.history.len(), 3, "history should have 3 records");
}

#[tokio::test]
async fn test_loop_need_help() {
    let adapter = Arc::new(headless_adapter());
    let ctrl = syscity::acp::ExecutionController::new();

    let loop_ = ComputerUseLoop::new(adapter)
        .with_config(default_loop_config())
        .with_execution_controller(ctrl);

    let result = loop_
        .run("impossible task", |_state: LoopState| async move {
            Ok(LoopDecision::NeedHelp {
                reason: "cannot complete this without human input".into(),
            })
        })
        .await
        .expect("loop should complete without error");

    assert!(!result.success, "NeedHelp should result in failure");
    assert!(
        result.message.contains("cannot complete"),
        "message should contain the reason: {}",
        result.message
    );
    assert_eq!(result.steps_taken, 0);
}

#[tokio::test]
async fn test_loop_decide_returns_error() {
    let adapter = Arc::new(headless_adapter());
    let ctrl = syscity::acp::ExecutionController::new();

    let loop_ = ComputerUseLoop::new(adapter)
        .with_config(default_loop_config())
        .with_execution_controller(ctrl);

    let result = loop_
        .run("will fail", |_state: LoopState| async move {
            Err(ComputerError::Other("decide crashed".into()))
        })
        .await;

    assert!(result.is_err(), "decide error should propagate");
    if let Err(ComputerError::Other(msg)) = result {
        assert!(msg.contains("decide"), "error should reference decide: {}", msg);
    } else {
        panic!("expected Other error, got: {:?}", result);
    }
}

#[tokio::test]
async fn test_loop_verify_after_each_disabled_does_not_crash() {
    // With verify_after_each=false, no VerificationEngine crash should occur
    let adapter = Arc::new(headless_adapter());
    let ctrl = syscity::acp::ExecutionController::new();
    let config = LoopConfig {
        max_steps: 2,
        settle_delay_ms: 1,
        verify_after_each: false,
        screenshot_region: None,
        verification: syscity::computer::VerificationConfig {
            max_retries: 0,
            retry_delay_ms: 1,
            ..Default::default()
        },
    };

    let loop_ = ComputerUseLoop::new(adapter)
        .with_config(config)
        .with_execution_controller(ctrl);

    let mut step_count = 0;
    let result = loop_
        .run("test", |_state: LoopState| {
            let count = step_count;
            step_count += 1;
            async move {
                if count == 0 {
                    Ok(LoopDecision::Action(DesktopAction::Wait { milliseconds: 1 }))
                } else {
                    Ok(LoopDecision::Done { message: "done".into() })
                }
            }
        })
        .await
        .expect("loop should complete");

    assert!(result.success);
    assert_eq!(result.steps_taken, 1);
}

#[tokio::test]
async fn test_loop_records_screenshot_in_loop_state() {
    // Even when screenshot fails, the loop should provide a fallback screenshot
    let adapter = Arc::new(headless_adapter());
    let ctrl = syscity::acp::ExecutionController::new();

    let loop_ = ComputerUseLoop::new(adapter.clone())
        .with_config(default_loop_config())
        .with_execution_controller(ctrl);

    let loop_arc = Arc::new(loop_);
    let state_screenshot: Arc<tokio::sync::Mutex<Option<Screenshot>>> =
        Arc::new(tokio::sync::Mutex::new(None));

    let captured = state_screenshot.clone();
    let result = loop_arc
        .run("check screenshot", move |state: LoopState| {
            let cap = captured.clone();
            async move {
                // Record the screenshot the loop provided
                *cap.lock().await = Some(state.screenshot);
                Ok(LoopDecision::Done { message: "checked".into() })
            }
        })
        .await
        .expect("loop should complete");

    assert!(result.success);
    let ss = state_screenshot.lock().await;
    let ss = ss.as_ref().expect("loop should provide a screenshot");
    // Without a display, screenshot is empty fallback
    assert!(ss.base64.is_empty(), "headless fallback screenshot should be empty");
    assert_eq!(ss.width, 0);
    assert_eq!(ss.height, 0);
}

// ---------------------------------------------------------------------------
// ComputerUseLoop — rollback tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_loop_rollback_snapshot_directory() {
    let tmp = std::env::temp_dir().join("syscity-rollback-snapshot-test");
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(&tmp).unwrap();
    let test_file = tmp.join("test.txt");
    std::fs::write(&test_file, b"hello").unwrap();

    // Create manager with specific backup dir, then snapshot the tmp dir
    let mut mgr = syscity::computer::RollbackManager::with_backup_dir(
        std::env::temp_dir().join("syscity-rollback-snapshot-test-backup"),
    );
    let snapshot = mgr.snapshot_directory(&tmp).await;
    assert!(snapshot.is_ok(), "snapshot should succeed: {:?}", snapshot.err());

    // Modify the file
    std::fs::write(&test_file, b"modified").unwrap();

    // Rollback
    let rolled_back = mgr.rollback_last(1).await;
    assert!(rolled_back.is_ok(), "rollback should succeed: {:?}", rolled_back.err());

    // Verify original content
    let content = std::fs::read_to_string(&test_file).unwrap_or_default();
    assert_eq!(content, "hello", "rollback should restore original content");

    // Cleanup
    let _ = std::fs::remove_dir_all(&tmp);
}

#[tokio::test]
async fn test_loop_rollback_multiple_snapshots() {
    let tmp = std::env::temp_dir().join("syscity-rollback-multi-test");
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(&tmp).unwrap();
    let test_file = tmp.join("data.txt");

    std::fs::write(&test_file, b"v1").unwrap();
    let mut mgr = syscity::computer::RollbackManager::with_backup_dir(
        std::env::temp_dir().join("syscity-rollback-multi-backup"),
    );
    mgr.snapshot_directory(&tmp).await.expect("snapshot v1");

    // Modify and snapshot again
    std::fs::write(&test_file, b"v2").unwrap();
    mgr.snapshot_directory(&tmp).await.expect("snapshot v2");

    // Modify again (no snapshot after this)
    std::fs::write(&test_file, b"v3").unwrap();

    // Rollback 1 step: restores v2 snapshot
    mgr.rollback_last(1).await.expect("rollback 1 step");
    let content = std::fs::read_to_string(&test_file).unwrap_or_default();
    assert_eq!(content, "v2", "should restore to v2 after rolling back 1 step");

    // Rollback remaining: restores v1 snapshot
    mgr.rollback_last(1).await.expect("rollback second step");
    let content = std::fs::read_to_string(&test_file).unwrap_or_default();
    assert_eq!(content, "v1", "should restore to v1 after rolling back another step");

    let _ = std::fs::remove_dir_all(&tmp);
}

#[tokio::test]
async fn test_loop_rollback_clear() {
    let mut mgr = syscity::computer::RollbackManager::with_backup_dir(
        std::env::temp_dir().join("syscity-rollback-clear-test"),
    );
    let tmp = std::env::temp_dir().join("syscity-rollback-clear-data");
    let _ = std::fs::create_dir_all(&tmp);
    mgr.snapshot_directory(&tmp).await.expect("snapshot");
    assert!(mgr.has_snapshots(), "should have snapshots after snapshot");
    mgr.clear().await.expect("clear");
    assert!(!mgr.has_snapshots(), "should have no snapshots after clear");
    let _ = std::fs::remove_dir_all(&tmp);
}

// ---------------------------------------------------------------------------
// ComputerUseLoop — settle delay edge cases
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_loop_zero_settle_delay() {
    let adapter = Arc::new(headless_adapter());
    let ctrl = syscity::acp::ExecutionController::new();
    let config = LoopConfig {
        max_steps: 2,
        settle_delay_ms: 0,
        verify_after_each: false,
        screenshot_region: None,
        ..Default::default()
    };

    let loop_ = ComputerUseLoop::new(adapter)
        .with_config(config)
        .with_execution_controller(ctrl);

    let mut step_count = 0;
    let result = loop_
        .run("zero settle", |_state: LoopState| {
            let count = step_count;
            step_count += 1;
            async move {
                if count == 0 {
                    Ok(LoopDecision::Action(DesktopAction::Wait { milliseconds: 1 }))
                } else {
                    Ok(LoopDecision::Done {
                        message: "done with zero settle".into(),
                    })
                }
            }
        })
        .await
        .expect("loop should handle zero settle delay");

    assert!(result.success);
    assert_eq!(result.steps_taken, 1);
}

// ---------------------------------------------------------------------------
// LoopConfig validation
// ---------------------------------------------------------------------------

#[test]
fn test_loop_config_default_values() {
    let config = LoopConfig::default();
    assert_eq!(config.max_steps, 30);
    assert!(config.settle_delay_ms > 0);
    assert!(config.verify_after_each);
}

#[test]
fn test_loop_config_custom() {
    let config = LoopConfig {
        max_steps: 5,
        settle_delay_ms: 100,
        verify_after_each: false,
        ..Default::default()
    };
    assert_eq!(config.max_steps, 5);
    assert_eq!(config.settle_delay_ms, 100);
    assert!(!config.verify_after_each);
}
