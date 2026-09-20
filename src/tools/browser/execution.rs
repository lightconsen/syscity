//! Pool-based and legacy per-call browser action execution.

use super::{BrowserAction, BrowserTool, ToolContext, ToolExecutionResult};
use serde_json::{json, Value};
use tracing::{debug, warn};

/// Build response from action results
#[cfg(feature = "browser")]
fn build_result(
    results: Vec<Result<serde_json::Value, String>>,
    screenshot_data: Option<super::BrowserScreenshot>,
) -> ToolExecutionResult {
    let success = results.iter().all(|r| r.is_ok());
    let mut output = serde_json::to_string_pretty(&results)
        .unwrap_or_else(|_| "Failed to serialize results".to_string());

    let data = match screenshot_data {
        Some(super::BrowserScreenshot::Ref(aref)) => {
            // The marker line on its own output line is what the
            // request-time materializer scans for.
            output.push_str(&format!(
                "\nScreenshot captured ({} bytes, {}). The image is attached to this tool result.\n",
                aref.size, aref.mime
            ));
            output.push_str(&crate::attachments::render_ref_line(&aref));
            output.push('\n');
            json!({
                "screenshot_ref": aref.to_json(),
                "results": results
            })
        }
        Some(super::BrowserScreenshot::Inline(screenshot)) => {
            json!({
                "screenshot_base64": screenshot,
                "results": results
            })
        }
        None => json!({ "results": results }),
    };

    if success {
        ToolExecutionResult::success(output).with_data(data)
    } else {
        let errors: Vec<String> = results
            .iter()
            .filter_map(|r| r.as_ref().err().cloned())
            .collect();
        let error_msg = if errors.is_empty() {
            "One or more browser actions failed".to_string()
        } else {
            format!("Browser action errors: {}", errors.join("; "))
        };
        ToolExecutionResult::error(error_msg).with_data(data)
    }
}

impl BrowserTool {
    /// Execute browser actions via pool (persistent session)
    #[cfg(feature = "browser")]
    async fn execute_actions_pool(
        &self,
        actions: Vec<BrowserAction>,
        pool: &std::sync::Arc<crate::browser::BrowserPool>,
    ) -> crate::Result<ToolExecutionResult> {
        let instance = pool.get_or_create(&self.profile).await?;
        // Reuse the page a previous call left behind. Every call used to open
        // a fresh `about:blank`, so a Navigate in one call and a Type in the
        // next ran on two different tabs — the second found no element, and
        // every read returned an empty page. A one-line probe detects a page
        // that died since (closed externally, crashed) and falls back to a
        // fresh one.
        let mut current_handle = match instance.most_recent_page().await {
            Some(handle) if handle.page.evaluate("1").await.is_ok() => handle,
            Some(handle) => {
                warn!(
                    target_id = %handle.target_id,
                    "Tracked browser page is gone; opening a fresh one"
                );
                instance.new_page("about:blank").await?
            }
            None => instance.new_page("about:blank").await?,
        };
        instance.touch_page(&current_handle.target_id).await;
        if let Err(e) = crate::browser::instrument::ensure_instrumented(&current_handle.page).await
        {
            warn!("Failed to instrument pooled page: {}", e);
        }
        if let Err(e) = crate::browser::network_log::start_capture(&current_handle.page).await {
            warn!("Failed to start network capture on pooled page: {}", e);
        }

        let mut results = Vec::new();
        let mut screenshot_data = None;

        for action in actions {
            debug!("Executing browser action (pool): {:?}", action);
            let result = match action {
                BrowserAction::ListTabs => {
                    let tabs = instance.list_pages().await;
                    let tabs_json: Vec<Value> = tabs
                        .into_iter()
                        .map(
                            |(id, title, url)| json!({"target_id": id, "title": title, "url": url}),
                        )
                        .collect();
                    Ok(json!({
                        "success": true,
                        "tabs": tabs_json,
                        "count": tabs_json.len()
                    }))
                }
                BrowserAction::SwitchTab { index, title } => {
                    let tabs = instance.list_pages().await;
                    let target_id = if let Some(idx) = index {
                        tabs.get(idx).map(|(id, _, _)| id.clone())
                    } else if let Some(ref t) = title {
                        instance.find_page_by_title(t).await
                    } else {
                        None
                    };

                    match target_id {
                        Some(id) => match instance.switch_page(&id).await {
                            Ok(true) => {
                                if let Some(handle) = instance.get_page(&id).await {
                                    current_handle = handle;
                                }
                                Ok(json!({"success": true, "target_id": id}))
                            }
                            Ok(false) => Err("Failed to switch tab: page not found".to_string()),
                            Err(e) => Err(format!("Failed to switch tab: {}", e)),
                        },
                        None => Err("Tab not found".to_string()),
                    }
                }
                BrowserAction::CloseTab { index, title } => {
                    let tabs = instance.list_pages().await;
                    let target_id = if let Some(idx) = index {
                        tabs.get(idx).map(|(id, _, _)| id.clone())
                    } else if let Some(ref t) = title {
                        instance.find_page_by_title(t).await
                    } else {
                        None
                    };

                    match target_id {
                        Some(id) => match instance.close_page(&id).await {
                            Ok(true) => Ok(json!({"success": true, "target_id": id})),
                            Ok(false) => Err("Failed to close tab: page not found".to_string()),
                            Err(e) => Err(format!("Failed to close tab: {}", e)),
                        },
                        None => Err("Tab not found".to_string()),
                    }
                }
                other => {
                    Self::execute_single_action(
                        other,
                        &current_handle.page,
                        Some(instance.browser.as_ref()),
                        &mut screenshot_data,
                    )
                    .await
                }
            };
            results.push(result);
        }

        Ok(build_result(results, screenshot_data))
    }

    /// Execute browser actions via legacy per-call launch
    #[cfg(feature = "browser")]
    async fn execute_actions_legacy(
        &self,
        actions: Vec<BrowserAction>,
    ) -> crate::Result<ToolExecutionResult> {
        use std::sync::Arc;

        use chromiumoxide::browser::{Browser, BrowserConfig};
        use futures::StreamExt;

        let mut builder = BrowserConfig::builder()
            .viewport(chromiumoxide::handler::viewport::Viewport {
                width: self.viewport_width,
                height: self.viewport_height,
                device_scale_factor: Some(1.0),
                emulating_mobile: false,
                is_landscape: true,
                has_touch: false,
            })
            .request_timeout(self.default_timeout);

        if self.headless {
            builder = builder.arg("--headless=new");
        }

        if let Some(ref path) = self.chrome_path {
            builder = builder.chrome_executable(std::path::PathBuf::from(path));
        }

        let config = builder
            .build()
            .map_err(|e| crate::error::SyscityError::ExternalService {
                source: format!("Browser configuration failed: {}", e),
                cause: None,
            })?;

        let (browser, mut handler) = Browser::launch(config).await.map_err(|e| {
            crate::error::SyscityError::ExternalService {
                source: "Failed to launch Chrome/Chromium. Is it installed?".to_string(),
                cause: Some(Box::new(e)),
            }
        })?;

        let browser = Arc::new(browser);
        let _browser_clone = browser.clone();
        tokio::spawn(async move {
            while let Some(h) = handler.next().await {
                if h.is_err() {
                    break;
                }
            }
        });

        let page = browser.new_page("about:blank").await.map_err(|e| {
            crate::error::SyscityError::ExternalService {
                source: "Failed to create browser page".to_string(),
                cause: Some(Box::new(e)),
            }
        })?;

        if let Err(e) = crate::browser::instrument::ensure_instrumented(&page).await {
            warn!("Failed to instrument page: {}", e);
        }
        if let Err(e) = crate::browser::network_log::start_capture(&page).await {
            warn!("Failed to start network capture: {}", e);
        }

        let mut results = Vec::new();
        let mut screenshot_data = None;

        for action in actions {
            debug!("Executing browser action (legacy): {:?}", action);
            let result = Self::execute_single_action(
                action,
                &page,
                Some(browser.as_ref()),
                &mut screenshot_data,
            )
            .await;
            results.push(result);
        }

        Ok(build_result(results, screenshot_data))
    }

    /// Execute browser actions
    #[cfg(feature = "browser")]
    pub(super) async fn execute_actions(
        &self,
        actions: Vec<BrowserAction>,
        _context: &ToolContext,
    ) -> crate::Result<ToolExecutionResult> {
        // Use pool if available
        if let Some(ref pool) = self.pool {
            return self.execute_actions_pool(actions, pool).await;
        }

        // Fall back to legacy per-call launch
        self.execute_actions_legacy(actions).await
    }
}
