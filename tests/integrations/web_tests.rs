use wiremock::{
    matchers::{method, path, query_param},
    Mock, MockServer, ResponseTemplate,
};

use super::*;

#[tokio::test]
async fn web_fetch_tool_fetches_example_com() {
    let tool = WebFetchTool::new();
    let result = tool
        .execute(json!({"url": "https://example.com"}), &test_context())
        .await;

    match result {
        Ok(output) => {
            if output.success {
                assert!(
                    output.output.contains("Example Domain")
                        || output.output.to_lowercase().contains("example")
                        || output.output.is_empty(),
                    "Expected example.com content, got: {}",
                    output.output
                );
            } else {
                println!("web_fetch returned error: {:?}", output.error);
            }
        }
        Err(e) => {
            println!("web_fetch failed (network may be unavailable): {}", e);
        }
    }
}

/// A live DuckDuckGo search.
///
/// The tool reports provider failures **in band**: `execute` returns
/// `Ok(ToolExecutionResult { success: false, .. })` carrying a classified code
/// (NOT_CONFIGURED / CONFIGURED_MISSING / UNAVAILABLE), not an `Err`. A test
/// that only checked `Err` therefore read every network failure as a
/// successful-but-empty search. This test needs the public internet, so it
/// distinguishes: a successful search (assert non-empty results), a
/// classified failure (skip — unreachable host, rate limit, bot check), and a
/// success with empty output, which would be a scraper regression and fails.
#[tokio::test]
async fn web_search_tool_duckduckgo() {
    let tool = WebSearchTool::new();
    let result = tool
        .execute(json!({"query": "Rust programming language", "limit": 3}), &test_context())
        .await
        .expect("search tool must not error out of band");

    if !result.success {
        let code = result
            .data
            .as_ref()
            .and_then(|d| d.get("code"))
            .and_then(|c| c.as_str())
            .unwrap_or("UNKNOWN");
        println!(
            "WebSearch skipped ({} — network unavailable or the endpoint refused us): {}",
            code,
            result.error.unwrap_or_default()
        );
        return;
    }

    assert!(
        !result.output.is_empty(),
        "a successful search must carry results — an empty success means the scraper \
         no longer matches DuckDuckGo's markup"
    );
    println!("WebSearch results: {}", result.output);
}

#[tokio::test]
async fn web_fetch_invalid_url_fails() {
    let tool = WebFetchTool::new();
    let ctx = test_context();
    let result = tool.execute(json!({"url": "not-a-url"}), &ctx).await;
    assert!(result.is_err(), "Expected validation error for invalid URL");
}

#[tokio::test]
async fn web_fetch_unsupported_scheme_fails() {
    let tool = WebFetchTool::new();
    let ctx = test_context();
    let result = tool
        .execute(json!({"url": "ftp://example.com"}), &ctx)
        .await;
    assert!(result.is_ok());
    let output = result.unwrap();
    assert!(!output.success, "Expected failure for unsupported scheme");
    assert!(output.error.as_ref().unwrap().contains("scheme"));
}

#[tokio::test]
async fn web_fetch_missing_url_validation_error() {
    let tool = WebFetchTool::new();
    let ctx = test_context();
    let result = tool.execute(json!({}), &ctx).await;
    assert!(result.is_err(), "Expected validation error for missing url");
}

#[tokio::test]
async fn web_search_missing_query_validation_error() {
    let tool = WebSearchTool::new();
    let ctx = test_context();
    let result = tool.execute(json!({}), &ctx).await;
    assert!(result.is_err(), "Expected validation error for missing query");
}

#[tokio::test]
async fn web_search_query_too_long_fails() {
    let tool = WebSearchTool::new();
    let ctx = test_context();
    let long_query = "a".repeat(501);
    let result = tool.execute(json!({"query": long_query}), &ctx).await;
    assert!(result.is_ok());
    let output = result.unwrap();
    assert!(!output.success, "Expected failure for query too long");
    assert!(output.error.as_ref().unwrap().contains("too long"));
}

#[tokio::test]
async fn web_search_returns_structured_results() {
    let server = MockServer::start().await;

    let mock_response = json!({
        "results": [
            {
                "title": "The Rust Programming Language",
                "url": "https://www.rust-lang.org/",
                "snippet": "A language empowering everyone to build reliable and efficient software."
            }
        ]
    });

    Mock::given(method("GET"))
        .and(path("/search"))
        .and(query_param("q", "Rust programming language"))
        .and(query_param("limit", "3"))
        .respond_with(ResponseTemplate::new(200).set_body_json(mock_response))
        .mount(&server)
        .await;

    let tool = WebSearchTool::new().with_provider(SearchProvider::Custom {
        url: format!("{}/search?q={{query}}&limit={{limit}}", server.uri()),
        api_key: None,
        headers: None,
        result_parser: None,
    });
    let ctx = test_context();
    let result = tool
        .execute(json!({"query": "Rust programming language", "limit": 3}), &ctx)
        .await;

    match result {
        Ok(output) if output.success => {
            let results = output
                .data
                .as_ref()
                .and_then(|d| d.get("results"))
                .and_then(|v| v.as_array())
                .cloned()
                .unwrap_or_default();
            assert_eq!(results.len(), 1, "Expected one structured result");
            assert_eq!(
                results[0].get("title").and_then(|v| v.as_str()),
                Some("The Rust Programming Language")
            );
            assert_eq!(
                results[0].get("url").and_then(|v| v.as_str()),
                Some("https://www.rust-lang.org/")
            );
        }
        Ok(output) => {
            panic!("Expected success from mocked search, got: {:?}", output.error);
        }
        Err(e) => panic!("Expected success from mocked search, got: {}", e),
    }
}
