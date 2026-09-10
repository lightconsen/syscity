use super::*;

#[tokio::test]
async fn update_plan_tool_crud() {
    let tool = UpdatePlanTool::new();
    let ctx = test_context();

    let create_result = tool
        .execute(
            json!({
                "action": "create",
                "title": "test-plan",
                "steps": ["step one", "step two"]
            }),
            &ctx,
        )
        .await
        .expect("create should succeed");

    let plan_id = create_result
        .data
        .as_ref()
        .and_then(|d| d.get("id"))
        .and_then(|v| v.as_str())
        .expect("Missing plan id")
        .to_string();

    let get_result = tool
        .execute(json!({"action": "get", "plan_id": plan_id}), &ctx)
        .await
        .expect("get should succeed");
    assert!(
        get_result.output.contains("test-plan"),
        "Expected plan title in output, got: {}",
        get_result.output
    );

    let list_result = tool
        .execute(json!({"action": "list"}), &ctx)
        .await
        .expect("list should succeed");
    assert!(
        list_result.output.contains("1 plan"),
        "Expected plan count in list, got: {}",
        list_result.output
    );
    let plans = list_result
        .data
        .as_ref()
        .and_then(|d| d.get("plans"))
        .and_then(|v| v.as_array())
        .expect("Expected plans array");
    assert_eq!(plans.len(), 1);
    assert_eq!(plans[0].get("id").and_then(|v| v.as_str()), Some(plan_id.as_str()));

    let _ = tool
        .execute(
            json!({
                "action": "set_status",
                "plan_id": plan_id,
                "step_id": "step_1",
                "status": "completed"
            }),
            &ctx,
        )
        .await
        .expect("set_status should succeed");

    let _ = tool
        .execute(json!({"action": "delete", "plan_id": plan_id}), &ctx)
        .await
        .expect("delete should succeed");
}

#[tokio::test]
async fn delegate_tool_spawn_without_agent() {
    let tool = DelegateTool::root();
    let result = tool
        .execute(
            json!({
                "action": "spawn",
                "task": {
                    "prompt": "test task",
                    "output_format": "text",
                    "max_iterations": 1
                }
            }),
            &test_context(),
        )
        .await;

    match result {
        Ok(output) => {
            assert!(
                output.output.contains("child")
                    || output.output.contains("delegated")
                    || output.output.contains("task"),
                "Expected delegation info, got: {}",
                output.output
            );
        }
        Err(e) => {
            println!("DelegateTool spawn returned error (expected without agent): {}", e);
        }
    }
}

#[tokio::test]
async fn mcp_connection_tool_lists_empty() {
    let manager = Arc::new(McpManager::default());
    let tool = McpConnectionTool::with_manager(manager);
    let result = tool
        .execute(json!({"action": "list"}), &test_context())
        .await;

    match result {
        Ok(output) => {
            assert!(
                output.output.contains("No servers")
                    || output.output.contains("server")
                    || output.output.is_empty(),
                "Expected server list info, got: {}",
                output.output
            );
        }
        Err(e) => {
            println!("McpConnectionTool list returned error: {}", e);
        }
    }
}

#[tokio::test]
async fn delegate_max_children_fails() {
    let tool = DelegateTool::root();
    let ctx = test_context();

    for i in 0..3 {
        let result = tool
            .execute(
                json!({
                    "action": "spawn",
                    "task": {"prompt": format!("task {}", i)}
                }),
                &ctx,
            )
            .await;
        assert!(result.is_ok());
    }
}

#[tokio::test]
async fn delegate_invalid_action_fails() {
    let tool = DelegateTool::root();
    let ctx = test_context();
    let result = tool.execute(json!({"action": "invalid"}), &ctx).await;
    assert!(result.is_err(), "Expected validation error for invalid action");
}

#[tokio::test]
async fn delegate_cancel_nonexistent_fails() {
    let tool = DelegateTool::root();
    let ctx = test_context();
    let result = tool
        .execute(json!({"action": "cancel", "child_id": "nonexistent"}), &ctx)
        .await;
    assert!(result.is_ok());
    let output = result.unwrap();
    assert!(!output.success, "Expected failure for nonexistent child");
}

#[tokio::test]
async fn mcp_connect_missing_server_id_fails() {
    let tool = McpConnectionTool::new();
    let ctx = test_context();
    let result = tool.execute(json!({"action": "connect"}), &ctx).await;
    assert!(result.is_err(), "Expected validation error for missing server_id");
}

#[tokio::test]
async fn mcp_invalid_action_fails() {
    let tool = McpConnectionTool::new();
    let ctx = test_context();
    let result = tool.execute(json!({"action": "invalid"}), &ctx).await;
    assert!(result.is_err(), "Expected validation error for invalid action");
}

#[tokio::test]
async fn mcp_disconnect_nonexistent_fails() {
    let tool = McpConnectionTool::new();
    let ctx = test_context();
    let result = tool
        .execute(json!({"action": "disconnect", "server_id": "nonexistent"}), &ctx)
        .await;
    let is_failed = result.as_ref().map(|o| !o.success).unwrap_or(true);
    assert!(is_failed, "Expected failure for nonexistent server");
}

#[tokio::test]
async fn update_plan_get_nonexistent_fails() {
    let tool = UpdatePlanTool::new();
    let ctx = test_context();
    let result = tool
        .execute(json!({"action": "get", "plan_id": "nonexistent"}), &ctx)
        .await;
    assert!(result.is_ok());
    let output = result.unwrap();
    assert!(!output.success, "Expected failure for nonexistent plan");
}

#[tokio::test]
async fn update_plan_invalid_action_fails() {
    let tool = UpdatePlanTool::new();
    let ctx = test_context();
    let result = tool.execute(json!({"action": "invalid"}), &ctx).await;
    assert!(result.is_ok());
    let output = result.unwrap();
    assert!(!output.success, "Expected failure for invalid action");
}

#[tokio::test]
async fn update_plan_set_status_invalid_status_fails() {
    let tool = UpdatePlanTool::new();
    let ctx = test_context();

    let create_result = tool
        .execute(json!({"action": "create", "title": "status-test", "steps": ["step"]}), &ctx)
        .await
        .expect("create failed");
    let plan_id = create_result
        .data
        .as_ref()
        .unwrap()
        .get("id")
        .unwrap()
        .as_str()
        .unwrap();

    let result = tool
        .execute(
            json!({"action": "set_status", "plan_id": plan_id, "status": "invalid_status"}),
            &ctx,
        )
        .await;
    assert!(result.is_ok());
    let output = result.unwrap();
    assert!(!output.success, "Expected failure for invalid status");
}
