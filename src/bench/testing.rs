//! A fake OSWorld VM, shared by the bench tests.
//!
//! It speaks the four endpoints an adapter uses and counts what reached it, so
//! a test can assert not only that the agent acted but that the action went
//! where the shape under test says it should — the replay log in `run`, the
//! harness link in `serve`.

use std::sync::Arc;

pub(crate) struct FakeDesktop {
    /// How many times `/screenshot` was asked for something.
    pub(crate) screens: std::sync::Mutex<usize>,
    /// Every snippet that reached `/execute`, in order.
    pub(crate) commands: std::sync::Mutex<Vec<String>>,
}

pub(crate) fn png_bytes(width: u32, height: u32) -> Vec<u8> {
    let mut bytes = vec![0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a];
    bytes.extend_from_slice(&13u32.to_be_bytes());
    bytes.extend_from_slice(b"IHDR");
    bytes.extend_from_slice(&width.to_be_bytes());
    bytes.extend_from_slice(&height.to_be_bytes());
    bytes.extend_from_slice(&[8, 6, 0, 0, 0]);
    bytes
}

/// Start the fake VM on an ephemeral port; returns its URL and its counters.
pub(crate) async fn start_fake_desktop() -> (String, Arc<FakeDesktop>) {
    use axum::routing::{get, post};
    use base64::Engine;

    let state = Arc::new(FakeDesktop {
        screens: std::sync::Mutex::new(0),
        commands: std::sync::Mutex::new(Vec::new()),
    });

    let screenshot = move || async move {
        axum::Json(serde_json::json!({
            "screenshot": base64::engine::general_purpose::STANDARD
                .encode(png_bytes(1280, 800)),
            "error": null,
        }))
    };
    let tree = || async {
        r#"<desktop-frame role="desktop frame" name="Desktop" x="0" y="0"
             width="1280" height="800">
             <push-button role="push button" name="Save" states="enabled"
               x="100" y="50" width="20" height="20"/>
           </desktop-frame>"#
    };
    let counter = state.clone();
    let app = axum::Router::new()
        .route(
            "/screenshot",
            get(move || {
                let counter = counter.clone();
                async move {
                    *counter.screens.lock().unwrap() += 1;
                    screenshot().await
                }
            }),
        )
        .route("/accessibility", get(tree))
        .route(
            "/screen_size",
            post(|| async {
                axum::Json(serde_json::json!({
                    "output": { "width": 1280, "height": 800 },
                    "error": null,
                }))
            }),
        )
        .route(
            "/execute",
            post(
                |axum::extract::State(state): axum::extract::State<Arc<FakeDesktop>>,
                 axum::Json(body): axum::Json<serde_json::Value>| async move {
                    let command = body
                        .get("command")
                        .and_then(|c| c.as_str())
                        .unwrap_or_default()
                        .to_string();
                    state.commands.lock().unwrap().push(command);
                    axum::Json(serde_json::json!({ "output": "", "error": null }))
                },
            ),
        )
        .with_state(state.clone());

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    (format!("http://{address}"), state)
}
