use syscity::agent::session_store::AppendMessageParams;

use super::*;

#[tokio::test]
async fn acp_spawn_tool_executes_without_agent_builder() {
    let acp = Arc::new(syscity::acp::AcpControlPlane::new(10));
    let tool = AcpSpawnTool::new(acp, None);
    let ctx = test_context();
    let result = tool
        .execute(json!({"task": "test task"}), &ctx)
        .await
        .unwrap();
    assert!(!result.success, "Expected failure without agent builder");
    assert!(
        result
            .error
            .unwrap()
            .contains("No agent builder configured"),
        "Expected 'No agent builder configured' error"
    );
}

#[tokio::test]
async fn acp_session_tool_lists_sessions() {
    let acp = Arc::new(syscity::acp::AcpControlPlane::new(10));
    let tool = AcpSessionTool::new(acp);
    let ctx = test_context();
    let result = tool.execute(json!({"action": "list"}), &ctx).await.unwrap();
    assert!(result.success, "list action should succeed");
    assert!(
        result.output.contains("0 active subagent"),
        "Expected empty list, got: {}",
        result.output
    );
    let data = result.data.unwrap();
    let subagents = data.get("subagents").unwrap().as_array().unwrap();
    assert_eq!(subagents.len(), 0);
}

#[tokio::test]
async fn sessions_list_tool_lists_sessions() {
    let store = Arc::new(
        syscity::agent::session_store::SessionStore::new(":memory:")
            .await
            .unwrap(),
    );
    let tool = SessionsListTool::new(Some(store));
    let ctx = test_context();
    let result = tool.execute(json!({}), &ctx).await.unwrap();
    assert!(result.success, "sessions_list should succeed");
    assert!(
        result.output.contains("0 session"),
        "Expected empty list, got: {}",
        result.output
    );
    let data = result.data.unwrap();
    let sessions = data.get("sessions").unwrap().as_array().unwrap();
    assert_eq!(sessions.len(), 0);
}

#[tokio::test]
async fn sessions_history_tool_returns_history() {
    let store = Arc::new(
        syscity::agent::session_store::SessionStore::new(":memory:")
            .await
            .unwrap(),
    );
    let session_id = "test-session-history";
    let meta =
        syscity::agent::session_store::SessionMetadata::new(session_id, "main", "ws", "anon");
    store.save_session(session_id, &meta, "{}").await.unwrap();
    store
        .append_message(&AppendMessageParams {
            session_id,
            role: "user",
            content: "Hello",
            ..Default::default()
        })
        .await
        .unwrap();
    store
        .append_message(&AppendMessageParams {
            session_id,
            role: "assistant",
            content: "Hi there!",
            ..Default::default()
        })
        .await
        .unwrap();

    let tool = SessionsHistoryTool::new(Some(store));
    let ctx = test_context();
    let result = tool
        .execute(json!({"session_id": session_id}), &ctx)
        .await
        .unwrap();
    assert!(result.success, "sessions_history should succeed");
    assert!(
        result.output.contains("2 message"),
        "Expected message count in output, got: {}",
        result.output
    );
    let data = result.data.unwrap();
    let messages = data.get("messages").unwrap().as_array().unwrap();
    assert_eq!(messages.len(), 2);
}

#[tokio::test]
async fn sessions_send_tool_fails_for_missing_subagent() {
    let acp = Arc::new(syscity::acp::AcpControlPlane::new(10));
    let tool = SessionsSendTool::new(acp);
    let ctx = test_context();
    let result = tool
        .execute(
            json!({
                "session_id": "test-session",
                "subagent_id": "nonexistent-subagent",
                "message": "hello"
            }),
            &ctx,
        )
        .await
        .unwrap();
    assert!(!result.success, "Expected failure for missing subagent");
    assert!(
        result.error.unwrap().contains("Failed to send message"),
        "Expected send failure error"
    );
}

#[tokio::test]
async fn sessions_yield_tool_fails_for_missing_subagent() {
    let acp = Arc::new(syscity::acp::AcpControlPlane::new(10));
    let tool = SessionsYieldTool::new(acp);
    let ctx = test_context();
    let result = tool
        .execute(json!({"subagent_id": "nonexistent-subagent"}), &ctx)
        .await
        .unwrap();
    assert!(!result.success, "Expected failure for missing subagent");
    assert!(result.error.unwrap().contains("not found"), "Expected 'not found' error");
}

#[tokio::test]
async fn session_status_tool_requires_id() {
    let store = Arc::new(
        syscity::agent::session_store::SessionStore::new(":memory:")
            .await
            .unwrap(),
    );
    let tool = SessionStatusTool::new(Some(store));
    let mut ctx = test_context();
    ctx.identity.conversation_id = String::new();
    let result = tool.execute(json!({}), &ctx).await.unwrap();
    assert!(!result.success, "Expected failure without id");
    let err = result.error.unwrap();
    assert!(
        err.contains("No current session available") || err.contains("missing"),
        "Expected session-related error, got: {}",
        err
    );
}

#[tokio::test]
async fn apply_patch_tool_validates_patch() {
    let tool = ApplyPatchTool::new();
    let ctx = test_context();
    let result = tool
        .execute(
            json!({
                "patch": "not a valid unified diff patch"
            }),
            &ctx,
        )
        .await
        .unwrap();
    assert!(!result.success, "Expected failure for invalid patch");
    let err = result.error.unwrap();
    assert!(
        err.contains("Patch does not apply")
            || err.contains("No valid patches")
            || err.contains("patch"),
        "Expected patch validation error, got: {}",
        err
    );
}

#[tokio::test]
async fn acp_session_invalid_action_fails() {
    let acp = Arc::new(syscity::acp::AcpControlPlane::new(10));
    let tool = AcpSessionTool::new(acp);
    let ctx = test_context();
    let result = tool.execute(json!({"action": "invalid"}), &ctx).await;
    assert!(result.is_ok());
    let output = result.unwrap();
    assert!(!output.success, "Expected failure for invalid action");
}

#[tokio::test]
async fn sessions_history_empty_session_returns_zero_messages() {
    let store = Arc::new(
        syscity::agent::session_store::SessionStore::new(":memory:")
            .await
            .unwrap(),
    );
    let tool = SessionsHistoryTool::new(Some(store));
    let ctx = test_context();
    let result = tool
        .execute(json!({"session_id": "nonexistent"}), &ctx)
        .await
        .unwrap();
    assert!(result.success, "Expected success even for unknown session");
    assert!(
        result.output.contains("0 message"),
        "Expected 0 messages, got: {}",
        result.output
    );
    let data = result.data.unwrap();
    let messages = data.get("messages").unwrap().as_array().unwrap();
    assert!(messages.is_empty());
}

#[tokio::test]
async fn sessions_send_missing_args_fails() {
    let acp = Arc::new(syscity::acp::AcpControlPlane::new(10));
    let tool = SessionsSendTool::new(acp);
    let ctx = test_context();
    let result = tool.execute(json!({"session_id": "x"}), &ctx).await;
    assert!(result.is_err() || !result.unwrap().success, "Expected failure for missing args");
}

#[tokio::test]
async fn sessions_yield_missing_subagent_id_fails() {
    let acp = Arc::new(syscity::acp::AcpControlPlane::new(10));
    let tool = SessionsYieldTool::new(acp);
    let ctx = test_context();
    let result = tool.execute(json!({}), &ctx).await;
    assert!(
        result.is_err() || !result.unwrap().success,
        "Expected failure for missing subagent_id"
    );
}

#[tokio::test]
async fn session_status_not_found_fails() {
    let store = Arc::new(
        syscity::agent::session_store::SessionStore::new(":memory:")
            .await
            .unwrap(),
    );
    let tool = SessionStatusTool::new(Some(store));
    let ctx = test_context();
    let result = tool
        .execute(json!({"session_id": "nonexistent"}), &ctx)
        .await;
    assert!(result.is_ok());
    let output = result.unwrap();
    assert!(!output.success, "Expected failure for nonexistent session");
}

#[tokio::test]
async fn apply_patch_applies_valid_patch() {
    let temp_dir = tempfile::tempdir().unwrap();
    let file_path = temp_dir.path().join("patch_target.txt");
    std::fs::write(&file_path, "old line\nsecond line\n").unwrap();

    let patch = format!(
        "--- a/patch_target.txt\n+++ b/patch_target.txt\n@@ -1,2 +1,2 @@\n-old line\n+new line\n \
         second line\n"
    );

    let tool = ApplyPatchTool::new();
    let mut ctx = test_context();
    ctx.sandbox.working_directory = temp_dir.path().to_path_buf();
    let result = tool
        .execute(
            json!({
                "patch": patch
            }),
            &ctx,
        )
        .await;
    assert!(result.is_ok());
    let output = result.unwrap();
    assert!(
        output.success,
        "Expected patch to apply successfully, got error: {:?}",
        output.error
    );

    let content = std::fs::read_to_string(&file_path).unwrap();
    assert!(content.contains("new line"), "Expected file to be patched");
}

#[tokio::test]
async fn apply_patch_missing_patch_fails() {
    let tool = ApplyPatchTool::new();
    let ctx = test_context();
    let result = tool.execute(json!({}), &ctx).await;
    assert!(
        result.is_err() || !result.unwrap().success,
        "Expected failure for missing patch"
    );
}

#[tokio::test]
async fn acp_spawn_missing_task_fails() {
    let acp = Arc::new(syscity::acp::AcpControlPlane::new(10));
    let tool = AcpSpawnTool::new(acp, None);
    let ctx = test_context();
    let result = tool.execute(json!({"mode": "run"}), &ctx).await;
    assert!(result.is_ok());
    let output = result.unwrap();
    assert!(!output.success, "Expected failure for missing task");
    let err = output.error.unwrap();
    assert!(
        err.contains("task") || err.contains("missing"),
        "Expected task-related error, got: {}",
        err
    );
}

#[tokio::test]
async fn acp_spawn_with_timeout_accepted() {
    let acp = Arc::new(syscity::acp::AcpControlPlane::new(10));
    let tool = AcpSpawnTool::new(acp, None);
    let ctx = test_context();
    let result = tool
        .execute(json!({"task": "test", "mode": "run", "timeout_seconds": 10}), &ctx)
        .await
        .unwrap();
    assert!(!result.success, "Expected failure without agent builder");
    assert!(
        result
            .error
            .unwrap()
            .contains("No agent builder configured"),
        "Expected 'No agent builder configured' error"
    );
}

#[tokio::test]
async fn acp_session_get_nonexistent_fails() {
    let acp = Arc::new(syscity::acp::AcpControlPlane::new(10));
    let tool = AcpSessionTool::new(acp);
    let ctx = test_context();
    let result = tool
        .execute(json!({"action": "get", "session_id": "nonexistent"}), &ctx)
        .await;
    assert!(result.is_ok());
    let output = result.unwrap();
    assert!(!output.success, "Expected failure for nonexistent session");
    assert!(output.error.unwrap().contains("not found"), "Expected 'not found' error");
}

#[tokio::test]
async fn acp_session_terminate_nonexistent_fails() {
    let acp = Arc::new(syscity::acp::AcpControlPlane::new(10));
    let tool = AcpSessionTool::new(acp);
    let ctx = test_context();
    let result = tool
        .execute(json!({"action": "terminate", "session_id": "nonexistent"}), &ctx)
        .await;
    assert!(result.is_ok());
    let output = result.unwrap();
    assert!(!output.success, "Expected failure for nonexistent session");
}

#[tokio::test]
async fn sessions_send_missing_session_id_fails() {
    let acp = Arc::new(syscity::acp::AcpControlPlane::new(10));
    let tool = SessionsSendTool::new(acp);
    let ctx = test_context();
    let result = tool
        .execute(json!({"subagent_id": "x", "message": "y"}), &ctx)
        .await;
    assert!(
        result.is_err() || !result.unwrap().success,
        "Expected failure for missing session_id"
    );
}

#[tokio::test]
async fn acp_session_kill_nonexistent_fails() {
    let acp = Arc::new(syscity::acp::AcpControlPlane::new(10));
    let tool = AcpSessionTool::new(acp);
    let ctx = test_context();
    let result = tool
        .execute(json!({"action": "kill", "subagent_id": "nonexistent"}), &ctx)
        .await;
    assert!(result.is_ok());
    let output = result.unwrap();
    assert!(!output.success, "Expected failure for nonexistent subagent");
    assert!(output.error.unwrap().contains("not found"), "Expected 'not found' error");
}

#[tokio::test]
async fn acp_session_steer_nonexistent_fails() {
    let acp = Arc::new(syscity::acp::AcpControlPlane::new(10));
    let tool = AcpSessionTool::new(acp);
    let ctx = test_context();
    let result = tool
        .execute(
            json!({"action": "steer", "subagent_id": "nonexistent", "message": "change direction"}),
            &ctx,
        )
        .await;
    assert!(result.is_ok());
    let output = result.unwrap();
    assert!(!output.success, "Expected failure for nonexistent subagent");
    assert!(output.error.unwrap().contains("not found"), "Expected 'not found' error");
}
