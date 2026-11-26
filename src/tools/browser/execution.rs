//! Pool-based and legacy per-call browser action execution.

use super::{BrowserAction, BrowserTool, ToolContext, ToolExecutionResult};
use serde_json::{json, Value};
use tracing::{debug, warn};

/// What the directory is called: unique per process and per launch, so two
/// one-shot browsers — including ones in parallel tests — never contend for
/// one profile.
#[cfg(feature = "browser")]
fn one_shot_profile_dir() -> std::path::PathBuf {
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(0);
    std::env::temp_dir().join(format!(
        "syscity-browser-one-shot-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ))
}

/// Removes a one-shot browser's profile directory when the call ends.
///
/// Declared before the `Browser`, so it drops after Chrome has been killed;
/// the removal is best-effort and a failure to unlink is not worth a warning.
#[cfg(feature = "browser")]
struct OneShotProfileDir(std::path::PathBuf);

#[cfg(feature = "browser")]
impl Drop for OneShotProfileDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

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
        owner: &str,
    ) -> crate::Result<ToolExecutionResult> {
        let instance = pool.get_or_create(&self.profile).await?;
        // Reuse the page a previous call left behind. Every call used to open
        // a fresh `about:blank`, so a Navigate in one call and a Type in the
        // next ran on two different tabs — the second found no element, and
        // every read returned an empty page. A one-line probe detects a page
        // that died since (closed externally, crashed) and falls back to a
        // fresh one.
        //
        // Only this conversation's pages qualify: the pool is shared, and
        // picking the globally most-recent page would hand this call a tab
        // another conversation is working in.
        let mut current_handle = match instance.most_recent_page_of(owner).await {
            Some(handle) if handle.page.evaluate("1").await.is_ok() => handle,
            Some(handle) => {
                warn!(
                    target_id = %handle.target_id,
                    "Tracked browser page is gone; opening a fresh one"
                );
                instance
                    .new_page_owned_by("about:blank", Some(owner.to_string()))
                    .await?
            }
            None => {
                instance
                    .new_page_owned_by("about:blank", Some(owner.to_string()))
                    .await?
            }
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
                    let tabs = instance.list_pages_of(Some(owner)).await;
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
                    let tabs = instance.list_pages_of(Some(owner)).await;
                    let target_id = if let Some(idx) = index {
                        tabs.get(idx).map(|(id, _, _)| id.clone())
                    } else if let Some(ref t) = title {
                        instance.find_page_by_title_of(t, Some(owner)).await
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
                    let tabs = instance.list_pages_of(Some(owner)).await;
                    let target_id = if let Some(idx) = index {
                        tabs.get(idx).map(|(id, _, _)| id.clone())
                    } else if let Some(ref t) = title {
                        instance.find_page_by_title_of(t, Some(owner)).await
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
                        &self.navigation_policy,
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

        // A throwaway browser still needs its own profile directory. Left
        // unset, chromiumoxide hands every launch the fixed
        // `temp_dir()/chromiumoxide-runner`, and Chrome refuses to start a
        // second instance on a profile another process holds — the refusal
        // surfaces as "Failed to launch Chrome/Chromium. Is it installed?".
        // One leftover Chrome is then enough to fail every call here and in
        // the integration suite, which goes through this same path.
        let profile_dir = one_shot_profile_dir();
        if let Err(e) = std::fs::create_dir_all(&profile_dir) {
            warn!("Failed to create one-shot browser profile dir: {}", e);
        }
        let _profile_dir_guard = OneShotProfileDir(profile_dir.clone());
        builder = builder.user_data_dir(profile_dir);

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

        let mut browser = Arc::new(browser);
        tokio::spawn(async move {
            while let Some(h) = handler.next().await {
                if h.is_err() {
                    break;
                }
            }
        });

        let outcome = async {
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
                    &self.navigation_policy,
                )
                .await;
                results.push(result);
            }

            Ok::<_, crate::error::SyscityError>(build_result(results, screenshot_data))
        }
        .await;

        // Shut the throwaway browser down and let it exit before the profile
        // guard unlinks the directory: Chrome holds files under it for as long
        // as it lives, so removing it any earlier fails and leaves ~6 MB
        // behind per call. `close` asks over CDP, `wait` collects the child.
        if let Some(browser) = Arc::get_mut(&mut browser) {
            if let Err(e) = browser.close().await {
                warn!("Failed to close one-shot browser: {}", e);
            }
            if let Err(e) = browser.wait().await {
                warn!("Failed to wait for one-shot browser exit: {}", e);
            }
        }
        drop(browser);

        outcome
    }

    /// Execute browser actions
    #[cfg(feature = "browser")]
    pub(super) async fn execute_actions(
        &self,
        actions: Vec<BrowserAction>,
        context: &ToolContext,
    ) -> crate::Result<ToolExecutionResult> {
        // Use pool if available
        if let Some(ref pool) = self.pool {
            // The conversation owns the tab. A gateway shares this tool — and
            // therefore its pool — with every conversation, so without this
            // the next caller would inherit the previous one's page.
            let owner = context.identity.conversation_id.clone();
            return self.execute_actions_pool(actions, pool, &owner).await;
        }

        // Fall back to legacy per-call launch
        self.execute_actions_legacy(actions).await
    }
}
