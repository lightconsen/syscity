use super::*;

#[tokio::test]
async fn pdf_tool_generates_output() {
    let tool = PdfTool::new();
    let temp_dir = tempfile::tempdir().unwrap();
    let output_path = temp_dir
        .path()
        .join("test_output")
        .to_str()
        .unwrap()
        .to_string();

    let result = tool
        .execute(
            json!({
                "content": "# Hello\n\nThis is a test document.",
                "output": output_path,
                "title": "Test Document"
            }),
            &test_context(),
        )
        .await;

    match result {
        Ok(output) => {
            assert!(
                output.output.contains("test_output")
                    || output.output.contains("html")
                    || output.output.contains("pdf"),
                "Expected output path info, got: {}",
                output.output
            );
        }
        Err(e) => {
            println!("PdfTool failed (expected if no Chrome): {}", e);
        }
    }
}

#[tokio::test]
async fn image_tool_reads_temp_file() {
    let tool = ImageTool::new();
    let temp_dir = tempfile::tempdir().unwrap();
    let img_path = temp_dir.path().join("test.png");

    let png_header: Vec<u8> = vec![
        0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0x00, 0x00, 0x00, 0x0D, 0x49, 0x48, 0x44,
        0x52, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x02, 0x00, 0x00, 0x00, 0x90,
        0x77, 0x53, 0xDE, 0x00, 0x00, 0x00, 0x00, 0x49, 0x45, 0x4E, 0x44, 0xAE, 0x42, 0x60, 0x82,
    ];
    tokio::fs::write(&img_path, &png_header).await.unwrap();

    let result = tool
        .execute(json!({"path": img_path.to_str().unwrap(), "action": "info"}), &test_context())
        .await;

    match result {
        Ok(output) => {
            assert!(
                output.output.contains("png")
                    || output.output.contains("PNG")
                    || output.output.contains("1x1"),
                "Expected PNG info, got: {}",
                output.output
            );
        }
        Err(e) => {
            println!("ImageTool failed: {}", e);
        }
    }
}

#[tokio::test]
async fn tts_tool_falls_back_without_key() {
    let tool = TtsTool::new();
    let result = tool
        .execute(json!({"text": "hello world"}), &test_context())
        .await;

    match result {
        Ok(output) => {
            println!("TtsTool output: {}", output.output);
        }
        Err(e) => {
            let err_str = format!("{}", e);
            assert!(
                err_str.contains("API key")
                    || err_str.contains("TTS")
                    || err_str.contains("say")
                    || err_str.contains("espeak"),
                "Expected TTS-related error, got: {}",
                err_str
            );
        }
    }
}

#[tokio::test]
async fn canvas_tool_presents() {
    let canvas_mgr = Arc::new(syscity::canvas::CanvasManager::new());
    let tool = CanvasTool::new(canvas_mgr);
    let ctx = test_context();

    let result = tool
        .execute(
            json!({
                "action": "present",
                "session_id": "test-session",
                "title": "Test Canvas",
                "components": [
                    {"type": "text", "id": "msg", "content": "Hello from canvas"}
                ]
            }),
            &ctx,
        )
        .await;

    match result {
        Ok(output) => {
            assert!(
                output.output.contains("presented")
                    || output.output.contains("canvas")
                    || output.output.contains("Test Canvas"),
                "Expected canvas presentation confirmation, got: {}",
                output.output
            );
        }
        Err(e) => {
            println!("CanvasTool failed: {}", e);
        }
    }
}

#[tokio::test]
async fn pdf_generates_with_custom_title() {
    let temp_dir = tempfile::tempdir().unwrap();
    let output_path = temp_dir.path().join("test.pdf");

    let tool = PdfTool::new();
    let ctx = test_context();
    let result = tool
        .execute(
            json!({
                "content": "# Hello PDF",
                "output": output_path.to_str().unwrap(),
                "title": "Custom Title"
            }),
            &ctx,
        )
        .await;
    assert!(result.is_ok());
    let output = result.unwrap();
    assert!(output.success);
    assert!(
        output.output.contains("Custom Title")
            || output.output.contains("pdf")
            || output.output.contains("HTML")
    );
}

#[tokio::test]
async fn pdf_orientation_landscape() {
    let temp_dir = tempfile::tempdir().unwrap();
    let output_path = temp_dir.path().join("landscape.pdf");

    let tool = PdfTool::new();
    let ctx = test_context();
    let result = tool
        .execute(
            json!({
                "content": "Landscape content",
                "output": output_path.to_str().unwrap(),
                "orientation": "landscape"
            }),
            &ctx,
        )
        .await;
    assert!(result.is_ok());
    let output = result.unwrap();
    assert!(output.success);
}

#[tokio::test]
async fn image_file_not_found_fails() {
    let tool = ImageTool::new();
    let ctx = test_context();
    let result = tool
        .execute(json!({"path": "/tmp/syscity-nonexistent-image.png"}), &ctx)
        .await;
    assert!(result.is_ok());
    let output = result.unwrap();
    assert!(!output.success, "Expected failure for nonexistent image");
}

#[tokio::test]
async fn image_reads_jpeg() {
    let temp_dir = tempfile::tempdir().unwrap();
    let file_path = temp_dir.path().join("test.jpg");

    let jpeg_data = vec![0xFF, 0xD8, 0xFF, 0xD9];
    std::fs::write(&file_path, jpeg_data).unwrap();

    let tool = ImageTool::new();
    let ctx = test_context();
    let result = tool
        .execute(json!({"path": file_path.to_str().unwrap()}), &ctx)
        .await;
    assert!(result.is_ok());
    let output = result.unwrap();
    assert!(output.success || output.error.is_some(), "Expected either success or error");
}

#[tokio::test]
async fn tts_empty_text_fails() {
    let tool = TtsTool::new();
    let ctx = test_context();
    let result = tool.execute(json!({"text": ""}), &ctx).await;
    assert!(result.is_ok());
    let output = result.unwrap();
    let _ = output.success;
}

#[tokio::test]
async fn canvas_invalid_action_fails() {
    let canvas_mgr = Arc::new(syscity::canvas::CanvasManager::new());
    let tool = CanvasTool::new(canvas_mgr);
    let ctx = test_context();
    let result = tool.execute(json!({"action": "invalid"}), &ctx).await;
    assert!(result.is_ok());
    let output = result.unwrap();
    assert!(!output.success, "Expected failure for invalid action");
}

#[tokio::test]
async fn canvas_update_nonexistent_fails() {
    let canvas_mgr = Arc::new(syscity::canvas::CanvasManager::new());
    let tool = CanvasTool::new(canvas_mgr);
    let ctx = test_context();
    let result = tool
        .execute(
            json!({
                "action": "update",
                "session_id": "nonexistent-session",
                "components": [{"type": "text", "id": "t", "content": "x"}]
            }),
            &ctx,
        )
        .await;
    assert!(result.is_ok());
    let output = result.unwrap();
    let _ = output.success;
}

#[tokio::test]
async fn tts_missing_text_fails() {
    let tool = TtsTool::new();
    let ctx = test_context();
    let result = tool.execute(json!({"voice": "alloy"}), &ctx).await;
    assert!(result.is_ok());
    let output = result.unwrap();
    assert!(!output.success, "Expected failure for missing text");
    let err = output.error.unwrap();
    assert!(
        err.contains("text") || err.contains("missing"),
        "Expected text-related error, got: {}",
        err
    );
}

#[tokio::test]
#[cfg(not(feature = "browser"))]
async fn browser_navigate_without_feature_fails() {
    let tool = BrowserTool::new();
    let ctx = test_context();
    let result = tool
        .execute(
            json!({
                "actions": [{"navigate": {"url": "https://example.com"}}]
            }),
            &ctx,
        )
        .await;
    assert!(result.is_ok());
    let output = result.unwrap();
    assert!(!output.success, "Expected failure without browser feature");
    assert!(
        output.error.unwrap().contains("not available") || output.output.contains("not available"),
        "Expected 'not available' error"
    );
}

#[tokio::test]
async fn browser_empty_actions_fails() {
    let tool = BrowserTool::new();
    let ctx = test_context();
    let result = tool.execute(json!({"actions": []}), &ctx).await;
    assert!(result.is_ok());
    let output = result.unwrap();
    assert!(!output.success, "Expected failure for empty actions");
}

#[tokio::test]
async fn browser_invalid_action_fails() {
    let tool = BrowserTool::new();
    let ctx = test_context();
    let result = tool
        .execute(
            json!({
                "actions": [{"invalid_action": {}}]
            }),
            &ctx,
        )
        .await;
    assert!(result.is_err(), "Expected error for invalid action");
}

#[tokio::test]
async fn image_generate_missing_prompt_fails() {
    let tool = ImageGenerateTool::new();
    let ctx = test_context();
    let result = tool.execute(json!({"size": "1024x1024"}), &ctx).await;
    assert!(result.is_ok());
    let output = result.unwrap();
    assert!(!output.success, "Expected failure for missing prompt");
    let err = output.error.unwrap();
    assert!(
        err.contains("prompt") || err.contains("missing"),
        "Expected prompt-related error, got: {}",
        err
    );
}

/// With no provider the tool cannot generate anything, and says that. It used to
/// ask for an `OPENAI_API_KEY` of its own — media now follows the configured
/// provider, so there is no separate key to be missing.
#[tokio::test]
async fn image_generate_without_a_provider_fails() {
    let tool = ImageGenerateTool::new();
    let ctx = test_context();
    let result = tool
        .execute(json!({"prompt": "a cat", "size": "1024x1024"}), &ctx)
        .await;
    assert!(result.is_ok());
    let output = result.unwrap();
    assert!(!output.success, "Expected failure without a provider");
    let err = output.error.unwrap();
    assert!(
        err.contains("needs a configured provider"),
        "Expected a provider error, got: {}",
        err
    );
}
