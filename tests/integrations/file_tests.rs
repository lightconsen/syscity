use super::*;

#[tokio::test]
async fn file_read_write_cycle() {
    let temp_dir = tempfile::tempdir().unwrap();
    let file_path = temp_dir.path().join("test_file.txt");
    let path_str = file_path.to_str().unwrap();

    let write_tool = FileWriteTool::new();
    let _ = write_tool
        .execute(json!({"path": path_str, "content": "hello world"}), &test_context())
        .await
        .expect("file_write should succeed");

    let read_tool = FileReadTool::new();
    let result = read_tool
        .execute(json!({"path": path_str}), &test_context())
        .await
        .expect("file_read should succeed");

    assert!(
        result.output.contains("hello world"),
        "Expected 'hello world' in file content, got: {}",
        result.output
    );
}

#[tokio::test]
async fn file_edit_tool_replaces_content() {
    let temp_dir = tempfile::tempdir().unwrap();
    let file_path = temp_dir.path().join("edit_test.txt");
    let path_str = file_path.to_str().unwrap();

    let write_tool = FileWriteTool::new();
    let _ = write_tool
        .execute(json!({"path": path_str, "content": "old content here"}), &test_context())
        .await
        .unwrap();

    let edit_tool = FileEditTool::new();
    let _ = edit_tool
        .execute(
            json!({
                "path": path_str,
                "old_string": "old content",
                "new_string": "new content"
            }),
            &test_context(),
        )
        .await
        .expect("file_edit should succeed");

    let read_tool = FileReadTool::new();
    let result = read_tool
        .execute(json!({"path": path_str}), &test_context())
        .await
        .unwrap();

    assert!(
        result.output.contains("new content here"),
        "Expected edited content, got: {}",
        result.output
    );
}

#[tokio::test]
async fn glob_tool_lists_files() {
    let temp_dir = tempfile::tempdir().unwrap();
    let base = temp_dir.path();

    tokio::fs::write(base.join("a.rs"), "").await.unwrap();
    tokio::fs::write(base.join("b.rs"), "").await.unwrap();
    tokio::fs::write(base.join("c.txt"), "").await.unwrap();

    let tool = GlobTool::new();
    let result = tool
        .execute(
            json!({
                "pattern": "*.rs",
                "path": base.to_str().unwrap()
            }),
            &test_context(),
        )
        .await
        .expect("glob tool should succeed");

    assert!(
        result.output.contains("a.rs") && result.output.contains("b.rs"),
        "Expected .rs files in glob output, got: {}",
        result.output
    );
}

#[tokio::test]
async fn grep_tool_finds_patterns() {
    let temp_dir = tempfile::tempdir().unwrap();
    let file_path = temp_dir.path().join("search.txt");
    let path_str = file_path.to_str().unwrap();

    tokio::fs::write(&file_path, "fn main() {}\nfn helper() {}\n")
        .await
        .unwrap();

    let tool = GrepTool::new();
    let result = tool
        .execute(
            json!({
                "pattern": "fn main",
                "path": path_str
            }),
            &test_context(),
        )
        .await
        .expect("grep tool should succeed");

    assert!(
        result.output.contains("fn main"),
        "Expected 'fn main' in grep output, got: {}",
        result.output
    );
}

#[tokio::test]
async fn file_read_not_found_fails() {
    let tool = FileReadTool::new();
    let ctx = test_context();
    let result = tool
        .execute(json!({"path": "/tmp/syscity-nonexistent-file-xyz.txt"}), &ctx)
        .await;
    assert!(result.is_ok(), "Tool should return Ok");
    let output = result.unwrap();
    assert!(!output.success, "Expected failure for nonexistent file");
    assert!(
        output.error.as_ref().unwrap().contains("does not exist"),
        "Expected 'does not exist' error, got: {:?}",
        output.error
    );
}

#[tokio::test]
async fn file_read_binary_returns_placeholder() {
    let temp_dir = tempfile::tempdir().unwrap();
    let file_path = temp_dir.path().join("binary.bin");
    std::fs::write(&file_path, vec![0u8, 1, 2, 255, 0, 3]).unwrap();

    let tool = FileReadTool::new();
    let ctx = test_context();
    let result = tool
        .execute(json!({"path": file_path.to_str().unwrap()}), &ctx)
        .await;
    assert!(result.is_ok());
    let output = result.unwrap();
    assert!(output.success, "Binary read should succeed with placeholder");
    assert!(
        output.output.contains("Binary file"),
        "Expected binary placeholder, got: {}",
        output.output
    );
}

#[tokio::test]
async fn file_read_missing_path_validation_error() {
    let tool = FileReadTool::new();
    let ctx = test_context();
    let result = tool.execute(json!({}), &ctx).await;
    assert!(result.is_err(), "Expected validation error for missing path");
}

#[tokio::test]
async fn file_write_missing_path_validation_error() {
    let tool = FileWriteTool::new();
    let ctx = test_context();
    let result = tool.execute(json!({"content": "test"}), &ctx).await;
    assert!(result.is_err(), "Expected validation error for missing path");
}

#[tokio::test]
async fn file_write_missing_content_validation_error() {
    let tool = FileWriteTool::new();
    let ctx = test_context();
    let result = tool.execute(json!({"path": "/tmp/test.txt"}), &ctx).await;
    assert!(result.is_err(), "Expected validation error for missing content");
}

#[tokio::test]
async fn file_write_creates_parent_dirs() {
    let temp_dir = tempfile::tempdir().unwrap();
    let nested_path = temp_dir.path().join("a/b/c/nested.txt");

    let tool = FileWriteTool::new();
    let ctx = test_context();
    let result = tool
        .execute(
            json!({
                "path": nested_path.to_str().unwrap(),
                "content": "nested content"
            }),
            &ctx,
        )
        .await;
    assert!(result.is_ok());
    let output = result.unwrap();
    assert!(output.success);
    assert!(nested_path.exists(), "Expected parent directories to be created");
    let content = std::fs::read_to_string(&nested_path).unwrap();
    assert_eq!(content, "nested content");
}

#[tokio::test]
async fn file_edit_old_string_not_found_fails() {
    let temp_dir = tempfile::tempdir().unwrap();
    let file_path = temp_dir.path().join("edit_test.txt");
    std::fs::write(&file_path, "original content").unwrap();

    let tool = FileEditTool::new();
    let ctx = test_context();
    let result = tool
        .execute(
            json!({
                "path": file_path.to_str().unwrap(),
                "old_string": "nonexistent text",
                "new_string": "replacement"
            }),
            &ctx,
        )
        .await;
    assert!(result.is_ok());
    let output = result.unwrap();
    assert!(!output.success, "Expected failure when old_string not found");
    assert!(
        output.error.unwrap().contains("Could not find text"),
        "Expected 'Could not find text' error"
    );
}

#[tokio::test]
async fn file_edit_missing_args_validation_error() {
    let tool = FileEditTool::new();
    let ctx = test_context();
    let result = tool
        .execute(json!({"path": "/tmp/test.txt", "old_string": "x"}), &ctx)
        .await;
    assert!(result.is_err(), "Expected validation error for missing new_string");
}

#[tokio::test]
async fn file_edit_file_not_found_fails() {
    let tool = FileEditTool::new();
    let ctx = test_context();
    let result = tool
        .execute(
            json!({
                "path": "/tmp/syscity-nonexistent-edit.txt",
                "old_string": "x",
                "new_string": "y"
            }),
            &ctx,
        )
        .await;
    assert!(result.is_ok());
    let output = result.unwrap();
    assert!(!output.success);
    assert!(output.error.unwrap().contains("does not exist"));
}

#[tokio::test]
async fn glob_no_matches_returns_empty() {
    let temp_dir = tempfile::tempdir().unwrap();
    let tool = GlobTool::new();
    let ctx = test_context().with_workspace_root(temp_dir.path());
    let result = tool
        .execute(json!({"pattern": "*.nonexistent"}), &ctx)
        .await;
    assert!(result.is_ok());
    let output = result.unwrap();
    assert!(output.success);
    let count = output
        .data
        .as_ref()
        .and_then(|d| d.get("count"))
        .and_then(|v| v.as_i64())
        .unwrap_or(-1);
    assert_eq!(count, 0, "Expected 0 matches for nonexistent pattern");
}

#[tokio::test]
async fn glob_invalid_pattern_fails() {
    let tool = GlobTool::new();
    let ctx = test_context();
    let result = tool.execute(json!({"pattern": "["}), &ctx).await;
    assert!(result.is_ok());
    let output = result.unwrap();
    assert!(!output.success, "Expected failure for invalid glob pattern");
}

#[tokio::test]
async fn grep_invalid_regex_fails() {
    let tool = GrepTool::new();
    let ctx = test_context();
    let result = tool.execute(json!({"pattern": "[invalid"}), &ctx).await;
    let is_failed = result.as_ref().map(|o| !o.success).unwrap_or(true);
    assert!(is_failed, "Expected failure for invalid regex");
}

#[tokio::test]
async fn grep_no_matches_returns_empty() {
    let tool = GrepTool::new();
    let ctx = test_context();
    let result = tool
        .execute(json!({"pattern": "xyz_nonexistent_pattern_12345"}), &ctx)
        .await;
    assert!(result.is_ok());
    let output = result.unwrap();
    assert!(output.success);
    let count = output
        .data
        .as_ref()
        .and_then(|d| d.get("count"))
        .and_then(|v| v.as_i64())
        .unwrap_or(-1);
    assert_eq!(count, 0, "Expected 0 matches");
}

#[tokio::test]
async fn grep_json_format_returns_structured() {
    let temp_dir = tempfile::tempdir().unwrap();
    let file_path = temp_dir.path().join("search.rs");
    tokio::fs::write(&file_path, "fn main() {}\n")
        .await
        .unwrap();

    let tool = GrepTool::new();
    let ctx = test_context();
    let result = tool
        .execute(
            json!({
                "pattern": "fn main",
                "format": "json",
                "path": file_path.to_str().unwrap()
            }),
            &ctx,
        )
        .await;
    assert!(result.is_ok());
    let output = result.unwrap();
    assert!(output.success);
    let matches = output
        .data
        .as_ref()
        .and_then(|d| d.get("matches"))
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    assert!(!matches.is_empty(), "Expected structured matches array");
}
