//! Document report tool for Syscity
//!
//! `write_report` — saves markdown or HTML content to `~/.syscity/artifacts/`
//! and returns metadata so the frontend can render a preview card.

use async_trait::async_trait;
use serde_json::Value;
use tracing::info;

use super::{create_schema, Tool, ToolContext, ToolExecutionResult};
use crate::tools::sdk::ToolCapabilities;

/// Tool that writes a user-viewable report (markdown or HTML) to the
/// artifacts directory and returns metadata for frontend preview.
#[derive(Debug)]
pub struct WriteReportTool {
    /// Layout root the artifact directories resolve against.
    paths: std::sync::Arc<crate::dirs::SyscityPaths>,
}

impl WriteReportTool {
    pub fn new(paths: std::sync::Arc<crate::dirs::SyscityPaths>) -> Self {
        Self { paths }
    }
}

/// Map an agent workspace path to its serving-URL owner segment, when the
/// path follows the standard layout. Returns `None` for custom workspace
/// layouts, whose reports fall back to the legacy global artifacts dir.
///
/// - `~/.syscity/workspace` → `default`
/// - `~/.syscity/agents/<id>/workspace` → `<id>`
fn artifact_url_owner(
    paths: &crate::dirs::SyscityPaths,
    agent_ws: &std::path::Path,
) -> Option<String> {
    let base = paths.root().to_path_buf();
    if agent_ws == base.join("workspace") {
        return Some("default".to_string());
    }
    // `agents/<id>/workspace` — the id is the parent-of-parent's file name.
    let parent = agent_ws.parent()?;
    if agent_ws.file_name().and_then(|f| f.to_str()) != Some("workspace") {
        return None;
    }
    let id = parent.file_name()?.to_str()?;
    if parent.parent()? != base.join("agents") {
        return None;
    }
    // Keep the id URL-safe (same charset the gateway allows for agent ids).
    if id.is_empty()
        || !id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        return None;
    }
    Some(id.to_string())
}

/// Resolve the artifacts directory + serving URL for a previewable file
/// produced by the current agent. Shared by `write_report` and `svg_to_png` so
/// all document/image artifacts land in the same owner-addressed workspace
/// convention.
pub fn resolve_artifact_target(
    paths: &crate::dirs::SyscityPaths,
    context: &ToolContext,
    filename: &str,
) -> (std::path::PathBuf, String) {
    let delegation_scope = context.delegation.as_ref();
    let agent_ws = context
        .sandbox
        .agent_workspace
        .clone()
        .unwrap_or_else(|| context.workspace_root().clone());
    let owner = artifact_url_owner(paths, &agent_ws);
    let tree_path = match delegation_scope {
        Some(scope) => format!("{}/{}", scope.root_id, scope.task_id),
        None => String::new(),
    };
    let artifacts_dir = match &owner {
        Some(_) => {
            let mut d = agent_ws.join("artifacts");
            if !tree_path.is_empty() {
                d = d.join(&tree_path);
            }
            d
        }
        None => {
            let mut d = paths.artifacts_dir();
            if !tree_path.is_empty() {
                d = d.join(&tree_path);
            }
            d
        }
    };
    let mut url = String::from("/api/v1/artifacts");
    if let Some(owner) = &owner {
        url.push_str("/@");
        url.push_str(owner);
    }
    if !tree_path.is_empty() {
        url.push('/');
        url.push_str(&tree_path);
    }
    url.push('/');
    url.push_str(filename);
    (artifacts_dir, url)
}

#[async_trait]
impl Tool for WriteReportTool {
    fn name(&self) -> &str {
        "write_report"
    }

    fn description(&self) -> &str {
        "Write a markdown, HTML, slides, docx, or xlsx document that enables a \
         rich split-panel preview in the chat UI. \
         \
         PREFER THIS OVER file_write when the user asks you to create a \
         document, report, article, essay, paper, summary, analysis, presentation, \
         slide deck, spreadsheet, or any \
         formatted content they would want to READ rather than edit. This tool \
         saves the report to a special directory and renders it with a \
         clickable preview card in the chat — the user can then open it in a \
         side-by-side viewer. \
         \
         Formats beyond plain markdown/html author constrained HTML that the \
         preview renders exactly and that the server converts to a real Office \
         file on download: \
         - \"slides\": canvas HTML — one `<div class=\"slide\">` per slide, each \
           a 1280x720 px canvas with absolutely-positioned children (inline \
           styles left/top/width/height in px, background, color, font-weight, \
           font-size in px — honored exactly in the downloaded deck) → .pptx. \
         - \"docx\": flowing HTML (h1-h6, p, ul/ol/li, table, strong/em/a/code, \
           pre, blockquote) → .docx. \
         - \"xlsx\": table HTML — one `<table>` per worksheet; use `data-sheet` \
           or `<caption>` for the sheet name, `<thead>` for a bold header row, \
           and `data-type=\"percent\"` to turn \"12.5%\" into a number → .xlsx. \
         \
         Use cases: research reports, industry analysis, weekly summaries, \
         technical documentation, essays, HTML newsletters, formatted articles, \
         meeting notes, whitepapers, tutorials, guides, slide decks, data \
         tables, and any long-form content that benefits from a dedicated \
         reading view. \
         \
         Do NOT use this for code files, configuration files, or data files — \
         those should use file_write."
    }

    fn parameters_schema(&self) -> Value {
        create_schema(
            "Write a report for user preview",
            serde_json::json!({
                "content": {
                    "type": "string",
                    "description": "The full content of the report (markdown or HTML)"
                },
                "filename": {
                    "type": "string",
                    "description": "Filename for the report, e.g. \"industry-report.md\" or \"report.html\""
                },
                "title": {
                    "type": "string",
                    "description": "Display title shown in the report card (defaults to filename)"
                },
                "format": {
                    "type": "string",
                    "enum": ["markdown", "html", "slides", "docx", "xlsx"],
                    "description": "Report format (default: markdown). \"slides\"/\"docx\"/\"xlsx\" author constrained HTML (see the tool description) that previews as-is and converts to .pptx/.docx/.xlsx on download.",
                    "default": "markdown"
                }
            }),
            vec!["content", "filename"],
        )
    }

    fn capabilities(&self) -> ToolCapabilities {
        ToolCapabilities {
            requires_approval: false,
            risk_level: crate::tools::approval::RiskLevel::Low,
            categories: vec!["document".to_string(), "content".to_string()],
            ..Default::default()
        }
    }

    async fn execute(
        &self,
        args: Value,
        context: &ToolContext,
    ) -> crate::Result<ToolExecutionResult> {
        let content = args["content"].as_str().ok_or_else(|| {
            crate::error::SyscityError::Validation("Missing 'content' argument".to_string())
        })?;

        let filename = args["filename"].as_str().ok_or_else(|| {
            crate::error::SyscityError::Validation("Missing 'filename' argument".to_string())
        })?;

        let title = args["title"]
            .as_str()
            .map(|s| s.to_string())
            .unwrap_or_else(|| filename.to_string());

        let format = args["format"].as_str().unwrap_or("markdown").to_string();

        // Authoring-contract validation: fail fast so the agent can fix the
        // markup instead of shipping a document that converts to nothing.
        match format.as_str() {
            "slides" => {
                if let Err(e) = crate::office::slides::parse_canvas(content) {
                    return Err(crate::error::SyscityError::Validation(format!(
                        "Invalid slides canvas: {e}"
                    )));
                }
            }
            "xlsx" => {
                // Runs the converter as a cheap correctness gate — surfaces
                // "no <table> elements" (and other build errors) at write time.
                if let Err(e) = crate::office::xlsx::tables_html_to_xlsx(content) {
                    return Err(crate::error::SyscityError::Validation(format!(
                        "Invalid xlsx content: {e}"
                    )));
                }
            }
            // "docx" and plain markdown/html: any flowing HTML converts, so no
            // structural gate is needed.
            _ => {}
        }

        // Resolve the artifacts directory: reports live in the producing
        // agent's own workspace (`<workspace>/artifacts/`) so users can
        // browse each agent's outputs in place. A delegated agent keeps its
        // tree binding (`artifacts/<root_id>/<task_id>`) on top of that so
        // parallel delegation trees never collide.
        //
        // The serving URL mirrors the physical placement: `@<owner>` names
        // the agent workspace (the default agent's shared
        // `~/.syscity/workspace` is addressed as `@default`). A workspace
        // outside the standard layout cannot be addressed safely, so those
        // reports keep the legacy global directory + flat URL.
        let (artifacts_dir, url) = resolve_artifact_target(&self.paths, context, filename);
        tokio::fs::create_dir_all(&artifacts_dir)
            .await
            .map_err(|e| crate::error::SyscityError::IoContext {
                context: "Failed to create artifacts directory".to_string(),
                source: e,
            })?;

        let path = artifacts_dir.join(filename);

        // Basic path traversal protection
        let canonical = path.canonicalize().unwrap_or_else(|_| path.clone());
        if !canonical.starts_with(&artifacts_dir) {
            return Err(crate::error::SyscityError::Validation(format!(
                "Invalid filename '{}': path escapes artifacts directory",
                filename
            )));
        }

        tokio::fs::write(&path, content).await.map_err(|e| {
            crate::error::SyscityError::IoContext {
                context: format!("Failed to write report '{}'", filename),
                source: e,
            }
        })?;

        let file_size = content.len();
        info!(
            "Wrote report '{filename}' ({format}, {size} bytes) to {path:?}",
            filename = filename,
            format = format,
            size = file_size,
            path = &path,
        );

        let mut data = serde_json::json!({
            "filename": filename,
            "title": title,
            "format": format,
            "url": url,
            "size": file_size,
        });
        // The preview panel renders the authored HTML directly; the export URL
        // converts it to the real Office file on demand.
        let export_target = match format.as_str() {
            "slides" => Some("pptx"),
            "docx" => Some("docx"),
            "xlsx" => Some("xlsx"),
            _ => None,
        };
        if let Some(target) = export_target {
            data["export_url"] = serde_json::Value::String(format!("{url}?to={target}"));
        }

        Ok(ToolExecutionResult::success(format!(
            "Report '{title}' written as {filename} ({format}, {size} bytes)",
            title = title,
            filename = filename,
            format = format,
            size = file_size,
        ))
        .with_data(data))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::delegation::DelegationScope;
    use crate::tools::ToolContext;

    #[tokio::test]
    async fn test_write_report_basic() {
        let tool = WriteReportTool::new(crate::dirs::paths());
        assert_eq!(tool.name(), "write_report");

        let args = serde_json::json!({
            "content": "# Test Report\n\nHello world.",
            "filename": "test-report.md",
            "title": "Test Report",
            "format": "markdown",
        });

        // Default context has no agent_workspace override, so the default
        // workspace is used and the URL is owner-addressed.
        let ctx = ToolContext::new("test", "test-conv");
        let result = tool.execute(args, &ctx).await.unwrap();
        assert!(result.success);
        assert!(result.data.is_some());

        let data = result.data.unwrap();
        assert_eq!(data["filename"], "test-report.md");
        assert_eq!(data["title"], "Test Report");
        assert_eq!(data["format"], "markdown");
        assert_eq!(data["url"], "/api/v1/artifacts/@default/test-report.md");

        // Clean up the file written into the real default workspace.
        let written = crate::dirs::workspace_data_dir()
            .join("artifacts")
            .join("test-report.md");
        assert!(written.exists());
        let _ = tokio::fs::remove_file(&written).await;
    }

    #[tokio::test]
    async fn test_write_report_slides_validates_canvas_and_exports() {
        let tool = WriteReportTool::new(crate::dirs::paths());
        let ctx = ToolContext::new("test", "test-conv");

        // A valid canvas gets an export_url for on-demand pptx conversion.
        let ok = tool
            .execute(
                serde_json::json!({
                    "content": "<div class=\"slide\"><div style=\"position:absolute;left:80px;top:60px;width:1120px\">标题</div></div>",
                    "filename": "deck-test.html",
                    "title": "Deck",
                    "format": "slides",
                }),
                &ctx,
            )
            .await
            .unwrap();
        let data = ok.data.unwrap();
        assert_eq!(data["format"], "slides");
        assert_eq!(
            data["export_url"].as_str().unwrap(),
            format!("{}?to=pptx", data["url"].as_str().unwrap())
        );

        // Missing slide divs is a validation error, not a silent bad deck.
        let bad = tool
            .execute(
                serde_json::json!({
                    "content": "<p>no slides here</p>",
                    "filename": "deck-bad.html",
                    "format": "slides",
                }),
                &ctx,
            )
            .await;
        assert!(bad.is_err());

        let written = crate::dirs::workspace_data_dir()
            .join("artifacts")
            .join("deck-test.html");
        let _ = tokio::fs::remove_file(&written).await;
    }

    #[tokio::test]
    async fn test_write_report_docx_and_xlsx_export_urls() {
        let tool = WriteReportTool::new(crate::dirs::paths());
        let ctx = ToolContext::new("test", "test-conv");

        // docx: any flowing HTML converts; no structural gate.
        let docx = tool
            .execute(
                serde_json::json!({
                    "content": "<h1>Title</h1><p>Body</p>",
                    "filename": "doc-test.html",
                    "format": "docx",
                }),
                &ctx,
            )
            .await
            .unwrap();
        let data = docx.data.unwrap();
        assert_eq!(data["format"], "docx");
        assert_eq!(
            data["export_url"].as_str().unwrap(),
            format!("{}?to=docx", data["url"].as_str().unwrap())
        );

        // xlsx: valid table HTML exports; empty HTML is rejected.
        let xlsx = tool
            .execute(
                serde_json::json!({
                    "content": "<table><thead><tr><th>Name</th></tr></thead><tbody><tr><td>1</td></tr></tbody></table>",
                    "filename": "sheet-test.html",
                    "format": "xlsx",
                }),
                &ctx,
            )
            .await
            .unwrap();
        let data = xlsx.data.unwrap();
        assert_eq!(
            data["export_url"].as_str().unwrap(),
            format!("{}?to=xlsx", data["url"].as_str().unwrap())
        );

        let bad_xlsx = tool
            .execute(
                serde_json::json!({
                    "content": "<p>no table</p>",
                    "filename": "sheet-bad.html",
                    "format": "xlsx",
                }),
                &ctx,
            )
            .await;
        assert!(bad_xlsx.is_err());

        let base = crate::dirs::workspace_data_dir().join("artifacts");
        let _ = tokio::fs::remove_file(base.join("doc-test.html")).await;
        let _ = tokio::fs::remove_file(base.join("sheet-test.html")).await;
    }

    #[tokio::test]
    async fn test_write_report_in_delegation_is_tree_bound() {
        let tool = WriteReportTool::new(crate::dirs::paths());

        let scope = DelegationScope::new("root-9", "task-9", 1, 3);
        let ctx = ToolContext::new("test", "test-conv").with_delegation(Some(scope));

        let args = serde_json::json!({
            "content": "# Tree Report\n\nShared.",
            "filename": "report.md",
            "title": "Tree Report",
            "format": "markdown",
        });

        let result = tool.execute(args, &ctx).await.unwrap();
        assert!(result.success);

        let data = result.data.unwrap();
        assert_eq!(data["url"], "/api/v1/artifacts/@default/root-9/task-9/report.md");

        // The file physically lands under the agent workspace's tree-scoped
        // artifacts subdir.
        let written = crate::dirs::workspace_data_dir()
            .join("artifacts")
            .join("root-9")
            .join("task-9")
            .join("report.md");
        assert!(written.exists(), "tree-bound artifact should be written: {:?}", written);
        let content = tokio::fs::read_to_string(&written).await.unwrap();
        assert!(content.contains("Tree Report"));

        // Clean up so the test does not leak files.
        let _ = tokio::fs::remove_dir_all(
            crate::dirs::workspace_data_dir()
                .join("artifacts")
                .join("root-9"),
        )
        .await;
    }

    #[test]
    fn test_artifact_url_owner_standard_layouts() {
        let paths = crate::dirs::paths();
        let base = paths.root();
        assert_eq!(artifact_url_owner(&paths, &base.join("workspace")).as_deref(), Some("default"));
        assert_eq!(
            artifact_url_owner(&paths, &base.join("agents/worker/workspace")).as_deref(),
            Some("worker")
        );
    }

    #[test]
    fn test_artifact_url_owner_rejects_nonstandard_or_unsafe() {
        let paths = crate::dirs::paths();
        let base = paths.root();
        // Custom workspace outside the standard layout.
        assert!(artifact_url_owner(&paths, std::path::Path::new("/tmp/custom-ws")).is_none());
        // Agent id with URL-hostile characters.
        assert!(artifact_url_owner(&paths, &base.join("agents/a.b/workspace")).is_none());
    }
}
