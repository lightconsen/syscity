//! Fetch, redirect and SSRF-guard tests for the web fetch tool.

// Tests assert against fallible bind/send/read results; unwrapping keeps
// the failure-path assertions readable (same allowance as
// `sandbox_interceptor`).
#![allow(clippy::unwrap_used)]

use super::*;
use crate::browser::NavigationPolicy;

/// Canned per-path raw HTTP responses served by [`spawn_http_server`].
type Routes = std::collections::HashMap<String, String>;

/// Build a raw HTTP/1.1 response with correct Content-Length.
fn http_response(status_line: &str, headers: &[(&str, &str)], body: &str) -> String {
    let mut response = format!("{status_line}\r\n");
    for (name, value) in headers {
        response.push_str(&format!("{name}: {value}\r\n"));
    }
    response.push_str(&format!("Content-Length: {}\r\n", body.len()));
    response.push_str("Connection: close\r\n\r\n");
    response.push_str(body);
    response
}

/// Serve canned raw HTTP responses on an ephemeral loopback port.
///
/// Each accepted connection is handled on its own task: read the request
/// head, look up the path, write the canned response, then close. A
/// `{port}` placeholder inside a canned response is replaced with the
/// server's own port at serve time (redirects need absolute targets).
async fn spawn_http_server(routes: Routes) -> u16 {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let routes = std::sync::Arc::new(routes);
    tokio::spawn(async move {
        loop {
            match listener.accept().await {
                Ok((socket, _)) => {
                    let routes = std::sync::Arc::clone(&routes);
                    tokio::spawn(serve_connection(socket, routes, port));
                }
                Err(err) => {
                    debug!("test server accept failed, stopping: {}", err);
                    break;
                }
            }
        }
    });
    port
}

/// Handle one connection of the canned-response test server.
async fn serve_connection(
    mut socket: tokio::net::TcpStream,
    routes: std::sync::Arc<Routes>,
    port: u16,
) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let mut buffer = Vec::new();
    let mut chunk = [0u8; 1024];
    loop {
        match socket.read(&mut chunk).await {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                buffer.extend_from_slice(&chunk[..n]);
                if buffer.windows(4).any(|window| window == b"\r\n\r\n") {
                    break;
                }
            }
        }
    }
    let request = String::from_utf8_lossy(&buffer);
    let path = request.split_whitespace().nth(1).unwrap_or("/").to_string();
    let path = path.split('?').next().unwrap_or("/").to_string();
    let response = routes.get(&path).cloned().unwrap_or_else(|| {
        "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_string()
    });
    let response = response.replace("{port}", &port.to_string());
    if let Err(err) = socket.write_all(response.as_bytes()).await {
        debug!("test server write failed: {}", err);
    }
    if let Err(err) = socket.shutdown().await {
        debug!("test server shutdown failed: {}", err);
    }
}

#[test]
fn test_web_fetch_tool_creation() {
    let tool = WebFetchTool::new();
    assert_eq!(tool.name(), "web_fetch");
    assert!(!tool.description().is_empty());
}

#[test]
fn test_html_to_markdown() {
    let html = r#"<h1>Title</h1><p>This is <strong>bold</strong> and <em>italic</em>.</p>"#;
    let markdown = WebFetchTool::html_to_markdown(html);
    assert!(markdown.contains("# Title"));
    assert!(markdown.contains("**bold**"));
    assert!(markdown.contains("_italic_"));
}

#[test]
fn test_is_html() {
    assert!(WebFetchTool::is_html(Some("text/html")));
    assert!(WebFetchTool::is_html(Some("text/html; charset=utf-8")));
    assert!(WebFetchTool::is_html(Some("application/xhtml+xml")));
    assert!(!WebFetchTool::is_html(Some("text/plain")));
    assert!(!WebFetchTool::is_html(Some("application/json")));
    assert!(!WebFetchTool::is_html(None));
}

#[test]
fn test_truncate_content() {
    let long_content = "a".repeat(MAX_CONTENT_SIZE + 100);
    let truncated = WebFetchTool::truncate_content(long_content);
    assert!(truncated.contains("truncated"));
    assert!(truncated.len() <= MAX_CONTENT_SIZE + 100);
}

// ── Redirect helpers ────────────────────────────────────────────────────

#[test]
fn test_is_followable_redirect() {
    use reqwest::StatusCode;
    assert!(WebFetchTool::is_followable_redirect(StatusCode::MOVED_PERMANENTLY));
    assert!(WebFetchTool::is_followable_redirect(StatusCode::FOUND));
    assert!(WebFetchTool::is_followable_redirect(StatusCode::SEE_OTHER));
    assert!(WebFetchTool::is_followable_redirect(StatusCode::TEMPORARY_REDIRECT));
    assert!(WebFetchTool::is_followable_redirect(StatusCode::PERMANENT_REDIRECT));
    // 300 and 304 are 3xx but carry no followable relocation target here.
    assert!(!WebFetchTool::is_followable_redirect(StatusCode::MULTIPLE_CHOICES));
    assert!(!WebFetchTool::is_followable_redirect(StatusCode::NOT_MODIFIED));
    assert!(!WebFetchTool::is_followable_redirect(StatusCode::OK));
}

#[test]
fn test_resolve_redirect_joins_relative_targets() {
    let base = reqwest::Url::parse("https://example.com/a/b").unwrap();
    assert_eq!(
        WebFetchTool::resolve_redirect(&base, "/c")
            .unwrap()
            .as_str(),
        "https://example.com/c"
    );
    assert_eq!(
        WebFetchTool::resolve_redirect(&base, "d").unwrap().as_str(),
        "https://example.com/a/d"
    );
    assert_eq!(
        WebFetchTool::resolve_redirect(&base, "https://other.org/x")
            .unwrap()
            .as_str(),
        "https://other.org/x"
    );
}

#[test]
fn test_resolve_redirect_rejects_disallowed_targets() {
    let base = reqwest::Url::parse("https://example.com/").unwrap();
    assert!(WebFetchTool::resolve_redirect(&base, "ftp://example.com/x").is_none());
    assert!(WebFetchTool::resolve_redirect(&base, "file:///etc/passwd").is_none());
    // Genuinely unparseable Location values are refused, not followed.
    assert!(WebFetchTool::resolve_redirect(&base, "http://[").is_none());
}

// ── Fetch: SSRF guard and redirect revalidation ─────────────────────────

fn permissive_fetcher() -> WebFetchTool {
    WebFetchTool::new().with_navigation_policy(NavigationPolicy::permissive())
}

async fn run_fetch(tool: &WebFetchTool, url: String) -> ToolExecutionResult {
    tool.execute(serde_json::json!({ "url": url }), &ToolContext::default())
        .await
        .unwrap()
}

#[tokio::test]
async fn fetch_follows_redirect_chain_to_final_body() {
    let port = spawn_http_server(
        [
            ("/a".to_string(), http_response("HTTP/1.1 302 Found", &[("Location", "/b")], "")),
            (
                "/b".to_string(),
                http_response("HTTP/1.1 200 OK", &[("Content-Type", "text/plain")], "final body"),
            ),
        ]
        .into_iter()
        .collect(),
    )
    .await;

    let result = run_fetch(&permissive_fetcher(), format!("http://127.0.0.1:{port}/a")).await;
    assert!(result.success, "expected success, got {:?}", result.error);
    assert!(result.output.contains("final body"));
    let data = result.data.unwrap();
    assert_eq!(data["status"], 200);
    assert_eq!(data["redirects"], 1);
    assert_eq!(data["final_url"], format!("http://127.0.0.1:{port}/b"));
}

#[tokio::test]
async fn fetch_returns_non_2xx_as_success_result() {
    let port = spawn_http_server(
        [(
            "/missing".to_string(),
            http_response(
                "HTTP/1.1 404 Not Found",
                &[("Content-Type", "text/plain")],
                "nothing here",
            ),
        )]
        .into_iter()
        .collect(),
    )
    .await;

    let result = run_fetch(&permissive_fetcher(), format!("http://127.0.0.1:{port}/missing")).await;
    assert!(result.success, "non-2xx must be a successful tool result");
    assert!(result.error.is_none());
    assert!(result.output.contains("HTTP 404 Not Found"));
    assert!(result.output.contains("nothing here"));
    let data = result.data.unwrap();
    assert_eq!(data["status"], 404);
    assert_eq!(data["http_error"], true);
}

#[tokio::test]
async fn fetch_redirect_loop_reports_unavailable() {
    let loop_response = http_response("HTTP/1.1 302 Found", &[("Location", "/y")], "");
    let port = spawn_http_server(
        [
            ("/x".to_string(), loop_response.clone()),
            ("/y".to_string(), loop_response),
        ]
        .into_iter()
        .collect(),
    )
    .await;

    let result = run_fetch(&permissive_fetcher(), format!("http://127.0.0.1:{port}/x")).await;
    assert!(!result.success, "redirect loops must fail");
    assert!(result
        .error
        .as_deref()
        .unwrap_or_default()
        .contains("redirect"));
    assert_eq!(result.data.unwrap()["code"], "UNAVAILABLE");
}

#[tokio::test]
async fn fetch_revalidates_every_redirect_hop_against_guard() {
    // The initial target (127.0.0.1) passes this policy; the redirect
    // target hostname "localhost" does not. If the guard only ran on the
    // first hop, the second request would go through.
    let policy = NavigationPolicy {
        allow_private: true,
        allowed_hostnames: Vec::new(),
        blocked_hostnames: vec!["localhost".to_string()],
    };
    let port = spawn_http_server(
        [(
            "/go".to_string(),
            http_response("HTTP/1.1 302 Found", &[("Location", "http://localhost:{port}/end")], ""),
        )]
        .into_iter()
        .collect(),
    )
    .await;

    let tool = WebFetchTool::new().with_navigation_policy(policy);
    let result = run_fetch(&tool, format!("http://127.0.0.1:{port}/go")).await;
    assert!(!result.success, "hop 2 must be blocked by the SSRF guard");
    let message = result.error.as_deref().unwrap_or_default();
    assert!(message.contains("SSRF guard"), "unexpected message: {}", message);
    assert!(message.contains("localhost"), "unexpected message: {}", message);
    assert_eq!(result.data.unwrap()["code"], "UNAVAILABLE");
}

#[tokio::test]
async fn fetch_blocks_private_target_on_first_hop() {
    let port = spawn_http_server(std::collections::HashMap::new()).await;
    // Default policy is restrictive: private targets are refused before
    // any request is issued.
    let result = run_fetch(&WebFetchTool::new(), format!("http://127.0.0.1:{port}/a")).await;
    assert!(!result.success);
    let message = result.error.as_deref().unwrap_or_default();
    assert!(message.contains("SSRF guard"), "unexpected message: {}", message);
    assert_eq!(result.data.unwrap()["code"], "UNAVAILABLE");
}
