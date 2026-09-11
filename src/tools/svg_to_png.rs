//! `svg_to_png` — rasterize SVG to PNG, with both written into the agent's
//! artifact store for embedding into documents.
//!
//! The agent authors SVG as text (charts, diagrams, infographics); this tool
//! validates it, rasterizes it to a PNG (via resvg), and registers both under
//! the owner-addressed artifact convention so the PNG can be `<img>`-referenced
//! from slides/docx/markdown and the SVG stays usable in HTML reports.

use async_trait::async_trait;
use serde_json::Value;

use super::{create_schema, Tool, ToolContext, ToolExecutionResult};
use crate::tools::report::resolve_artifact_target;
use crate::tools::sdk::ToolCapabilities;

/// Rasterize an SVG string to PNG bytes, returning `(png_bytes, width, height)`.
///
/// Loads the system font database so text (including CJK) renders; the generic
/// `sans-serif`/`serif`/`monospace` families fall back to common system fonts.
pub fn rasterize_svg(svg: &str) -> Result<(Vec<u8>, u32, u32), String> {
    use resvg::tiny_skia;
    use resvg::usvg;

    let mut db = usvg::fontdb::Database::new();
    db.load_system_fonts();
    db.set_sans_serif_family("PingFang SC");
    db.set_serif_family("Songti SC");
    db.set_monospace_family("Menlo");

    let options = usvg::Options {
        fontdb: std::sync::Arc::new(db),
        ..Default::default()
    };
    let tree = usvg::Tree::from_str(svg, &options).map_err(|e| format!("invalid SVG: {e}"))?;
    let size = tree.size();
    let width = size.width().round() as u32;
    let height = size.height().round() as u32;
    if width == 0 || height == 0 {
        return Err("SVG has zero width or height (missing viewBox?)".to_string());
    }
    let mut pixmap = tiny_skia::Pixmap::new(width, height).ok_or("failed to allocate pixmap")?;
    resvg::render(&tree, tiny_skia::Transform::default(), &mut pixmap.as_mut());
    let png = pixmap
        .encode_png()
        .map_err(|e| format!("PNG encode failed: {e}"))?;
    Ok((png, width, height))
}

/// `svg_to_png` tool.
#[derive(Debug)]
pub struct SvgToPngTool {
    /// Layout root the artifact directories resolve against.
    paths: std::sync::Arc<crate::dirs::SyscityPaths>,
}

impl SvgToPngTool {
    pub fn new(paths: std::sync::Arc<crate::dirs::SyscityPaths>) -> Self {
        Self { paths }
    }
}

#[async_trait]
impl Tool for SvgToPngTool {
    fn name(&self) -> &str {
        "svg_to_png"
    }

    fn description(&self) -> &str {
        "Rasterize an SVG image (charts, diagrams, infographics) to PNG so it \
         can be embedded into documents. \
         \
         Pass the SVG source as `svg` (or `path` to an existing .svg file). \
         Both the .svg and the .png are saved to the artifact store; use the \
         returned `filename` (e.g. `<img src=\"chart-123.png\">`) inside a \
         slides/docx document, or `png_url` in markdown/html. \
         \
         Use this when the user asks for a diagram, chart, infographic, or \
         schematic that you author as SVG and then want inside a report or \
         presentation."
    }

    fn parameters_schema(&self) -> Value {
        create_schema(
            "Rasterize an SVG to PNG for document embedding",
            serde_json::json!({
                "svg": {
                    "type": "string",
                    "description": "The full SVG source (XML). Required unless `path` is given."
                },
                "path": {
                    "type": "string",
                    "description": "Read the SVG from this file path instead of the `svg` argument."
                },
                "filename": {
                    "type": "string",
                    "description": "Base filename (no extension), e.g. \"sales-chart\". Defaults to a generated name."
                }
            }),
            Vec::<&str>::new(),
        )
    }

    fn capabilities(&self) -> ToolCapabilities {
        ToolCapabilities {
            requires_approval: false,
            risk_level: crate::tools::approval::RiskLevel::Low,
            categories: vec!["image".to_string(), "content".to_string()],
            ..Default::default()
        }
    }

    async fn execute(
        &self,
        args: Value,
        context: &ToolContext,
    ) -> crate::Result<ToolExecutionResult> {
        // Resolve the SVG source: inline `svg` wins, else read `path`.
        let svg = if let Some(svg) = args["svg"].as_str() {
            svg.to_string()
        } else if let Some(path) = args["path"].as_str() {
            tokio::fs::read_to_string(path).await.map_err(|e| {
                crate::error::SyscityError::IoContext {
                    context: format!("Failed to read SVG '{path}'"),
                    source: e,
                }
            })?
        } else {
            return Err(crate::error::SyscityError::Validation(
                "Either 'svg' or 'path' is required".to_string(),
            ));
        };

        let base = args["filename"]
            .as_str()
            .map(|s| {
                s.trim_end_matches(".svg")
                    .trim_end_matches(".png")
                    .to_string()
            })
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| format!("chart-{}", uuid::Uuid::new_v4()));

        let (png, width, height) =
            rasterize_svg(&svg).map_err(crate::error::SyscityError::Validation)?;

        let svg_filename = format!("{base}.svg");
        let png_filename = format!("{base}.png");

        let (artifacts_dir, _) = resolve_artifact_target(&self.paths, context, &png_filename);
        tokio::fs::create_dir_all(&artifacts_dir)
            .await
            .map_err(|e| crate::error::SyscityError::IoContext {
                context: "Failed to create artifacts directory".to_string(),
                source: e,
            })?;

        tokio::fs::write(artifacts_dir.join(&svg_filename), svg.as_bytes())
            .await
            .map_err(|e| crate::error::SyscityError::IoContext {
                context: format!("Failed to write {svg_filename}"),
                source: e,
            })?;
        tokio::fs::write(artifacts_dir.join(&png_filename), &png)
            .await
            .map_err(|e| crate::error::SyscityError::IoContext {
                context: format!("Failed to write {png_filename}"),
                source: e,
            })?;

        let (_, svg_url) = resolve_artifact_target(&self.paths, context, &svg_filename);
        let (_, png_url) = resolve_artifact_target(&self.paths, context, &png_filename);

        Ok(ToolExecutionResult::success(format!(
            "Rendered SVG to {png_filename} ({width}x{height})"
        ))
        .with_data(serde_json::json!({
            "svg_url": svg_url,
            "png_url": png_url,
            "filename": png_filename,
            "width": width,
            "height": height,
        })))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rasterize_svg_produces_png() {
        let svg = r##"<svg xmlns="http://www.w3.org/2000/svg" width="120" height="40" viewBox="0 0 120 40"><rect x="0" y="0" width="120" height="40" fill="#3366cc"/><text x="10" y="25" font-family="sans-serif" font-size="14" fill="#fff">Hello</text></svg>"##;
        let (png, w, h) = rasterize_svg(svg).expect("should rasterize");
        assert_eq!(w, 120);
        assert_eq!(h, 40);
        // PNG magic bytes.
        assert_eq!(&png[..8], &[0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A]);
    }

    #[test]
    fn rasterize_svg_rejects_invalid() {
        assert!(rasterize_svg("not svg at all").is_err());
        // A size-less root now defaults to 100x100 upstream; only an
        // explicitly zero-sized SVG trips our zero-size guard.
        assert!(rasterize_svg(r#"<svg width="0" height="0"/>"#).is_err());
    }
}
