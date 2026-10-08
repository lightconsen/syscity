use super::*;

/// E2E for the workspace browser WS methods (workspace.list / workspace.read).
///
/// The default agent's workspace is pointed at a temp dir so the test never
/// touches the real ~/.syscity/workspace.

fn ws_dir(port: u16) -> std::path::PathBuf {
    std::env::temp_dir().join(format!("syscity_ws_e2e_{}", port))
}

/// Start a test gateway whose default-agent workspace is a temp dir pre-seeded
/// with `a.md` and `sub/b.txt`.
async fn start_workspace_gateway(port: u16) -> std::path::PathBuf {
    let dir = ws_dir(port);
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("sub")).expect("create temp workspace");
    std::fs::write(dir.join("a.md"), "# hello e2e").expect("seed a.md");
    std::fs::write(dir.join("sub").join("b.txt"), "nested").expect("seed b.txt");

    let mut config = test_config(port, false);
    config.default_agent.workspace_dir = Some(dir.clone());
    let gateway = new_test_gateway(config).await;
    start_gateway_and_wait(port, gateway).await;
    dir
}

#[tokio::test]
#[serial]
async fn test_workspace_list_root_dirs_first() {
    let port = free_port();
    let dir = start_workspace_gateway(port).await;
    let mut client = FrontendSimulator::connect(port).await;

    let resp = client.request("workspace.list", json!({})).await;
    assert!(resp.get("ok").and_then(|v| v.as_bool()) == Some(true));
    let payload = resp.get("payload").cloned().unwrap_or_default();
    let entries = payload
        .get("entries")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    assert_eq!(entries.len(), 2, "entries: {:?}", entries);
    assert_eq!(entries[0].get("name").and_then(|v| v.as_str()), Some("sub"));
    assert_eq!(entries[0].get("kind").and_then(|v| v.as_str()), Some("dir"));
    assert_eq!(entries[1].get("name").and_then(|v| v.as_str()), Some("a.md"));
    assert_eq!(entries[1].get("kind").and_then(|v| v.as_str()), Some("file"));

    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
#[serial]
async fn test_workspace_list_subdir_and_read_file() {
    let port = free_port();
    let dir = start_workspace_gateway(port).await;
    let mut client = FrontendSimulator::connect(port).await;

    let resp = client
        .request("workspace.list", json!({"path": "sub"}))
        .await;
    assert!(resp.get("ok").and_then(|v| v.as_bool()) == Some(true));
    let entries = resp
        .get("payload")
        .and_then(|p| p.get("entries"))
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].get("path").and_then(|v| v.as_str()), Some("sub/b.txt"));

    let resp = client
        .request("workspace.read", json!({"path": "a.md"}))
        .await;
    assert!(resp.get("ok").and_then(|v| v.as_bool()) == Some(true));
    let payload = resp.get("payload").cloned().unwrap_or_default();
    assert_eq!(payload.get("content").and_then(|v| v.as_str()), Some("# hello e2e"));
    assert_eq!(payload.get("binary").and_then(|v| v.as_bool()), Some(false));

    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
#[serial]
async fn test_workspace_read_rejects_traversal() {
    let port = free_port();
    let dir = start_workspace_gateway(port).await;
    let mut client = FrontendSimulator::connect(port).await;

    let resp = client
        .request("workspace.read", json!({"path": "../escape"}))
        .await;
    assert!(
        resp.get("ok").and_then(|v| v.as_bool()) == Some(false),
        "expected traversal rejection, got: {:?}",
        resp
    );

    let _ = std::fs::remove_dir_all(&dir);
}
