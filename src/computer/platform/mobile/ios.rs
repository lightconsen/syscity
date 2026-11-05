//! iOS device control via libimobiledevice.
//!
//! Provides basic device listing, screenshot, and app management via
//! `idevice*` command-line tools.  Full UI automation requires
//! WebDriverAgent (WDA) which is more complex; this module provides
//! the foundational tools.

use async_trait::async_trait;
use serde_json::Value;

use super::{has_idevice, run_cmd};
use crate::computer::platform::screen;
use crate::computer::platform::{OsControlScope, PlatformConstraints, PlatformToolSet};
use crate::tools::{create_schema, Tool, ToolContext, ToolExecutionResult};

/// iOS device platform tool set (requires `libimobiledevice` on PATH).
pub struct IosToolset;

impl Default for IosToolset {
    fn default() -> Self {
        Self::new()
    }
}

impl IosToolset {
    pub fn new() -> Self {
        Self
    }
}

impl PlatformToolSet for IosToolset {
    fn id(&self) -> &str {
        "ios"
    }

    fn name(&self) -> &str {
        "iOS Device Bridge"
    }

    fn description(&self) -> &str {
        "Control iOS devices via libimobiledevice: list, screenshot, install, launch apps"
    }

    fn constraints(&self) -> &PlatformConstraints {
        static CONSTRAINTS: std::sync::OnceLock<PlatformConstraints> = std::sync::OnceLock::new();
        CONSTRAINTS.get_or_init(|| PlatformConstraints {
            target_os: Vec::<String>::new(), // any OS
            requires_gui: false,
            requires_services: Vec::<String>::new(),
        })
    }

    fn scope(&self) -> OsControlScope {
        OsControlScope::UserSpace
    }

    fn tools(&self) -> Vec<Box<dyn Tool>> {
        vec![
            Box::new(IdeviceIdTool::new()),
            Box::new(IdeviceScreenshotTool::new()),
            Box::new(IdeviceInstallerTool::new()),
        ]
    }

    fn is_available(&self) -> bool {
        has_idevice()
    }

    /// None of the three shared judgments yet — and saying so is the point.
    ///
    /// This set can list devices, screenshot, and install or launch an app, but
    /// it has no UI automation: no tree to re-read after an action (so no
    /// effect verdict), no field to read back, and no event stream. A caller
    /// that asks for one of those gets "this device set cannot answer" rather
    /// than a bare `unknown` that looks like a read failure. When WebDriverAgent
    /// lands here, this becomes
    /// [`ScreenSemantics::ALL`](crate::computer::platform::ScreenSemantics::ALL)
    /// and the tools inherit the same words Android uses.
    fn screen_semantics(&self) -> screen::ScreenSemantics {
        screen::ScreenSemantics::NONE
    }
}

// ── idevice_id — list connected devices ────────────────────────────────────

#[derive(Debug)]
pub struct IdeviceIdTool;

impl Default for IdeviceIdTool {
    fn default() -> Self {
        Self::new()
    }
}

impl IdeviceIdTool {
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl Tool for IdeviceIdTool {
    fn name(&self) -> &str {
        "ios_list_devices"
    }

    fn description(&self) -> &str {
        "List connected iOS devices with their UDIDs."
    }

    fn parameters_schema(&self) -> Value {
        create_schema("List iOS devices", serde_json::json!({}), Vec::<String>::new())
    }

    async fn execute(
        &self,
        _args: Value,
        _context: &ToolContext,
    ) -> crate::Result<ToolExecutionResult> {
        let (status, stdout, stderr) = run_cmd("idevice_id", &["--list"]).await.map_err(|e| {
            crate::error::SyscityError::ExternalService {
                source: "idevice_id failed".to_string(),
                cause: Some(Box::new(e)),
            }
        })?;

        if !status.success() {
            return Ok(ToolExecutionResult::error(format!("idevice_id failed: {}", stderr)));
        }

        let devices: Vec<String> = stdout
            .lines()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect();

        Ok(ToolExecutionResult::success(format!("Found {} device(s)", devices.len()))
            .with_data(serde_json::json!({ "devices": devices })))
    }
}

// ── idevicescreenshot ──────────────────────────────────────────────────────

#[derive(Debug)]
pub struct IdeviceScreenshotTool {
    udid: Option<String>,
}

impl Default for IdeviceScreenshotTool {
    fn default() -> Self {
        Self::new()
    }
}

impl IdeviceScreenshotTool {
    pub fn new() -> Self {
        Self { udid: None }
    }

    pub fn with_udid(mut self, udid: String) -> Self {
        self.udid = Some(udid);
        self
    }
}

#[async_trait]
impl Tool for IdeviceScreenshotTool {
    fn name(&self) -> &str {
        "ios_screenshot"
    }

    fn description(&self) -> &str {
        "Capture a screenshot of a connected iOS device."
    }

    fn parameters_schema(&self) -> Value {
        create_schema(
            "Capture iOS screenshot",
            serde_json::json!({
                "udid": { "type": "string", "description": "Optional device UDID" }
            }),
            Vec::<String>::new(),
        )
    }

    async fn execute(
        &self,
        _args: Value,
        _context: &ToolContext,
    ) -> crate::Result<ToolExecutionResult> {
        let tmp_file = format!("/tmp/syscity_ios_screenshot_{}.tiff", uuid::Uuid::new_v4());

        let mut cmd_args = vec!["--output", &tmp_file];
        if let Some(u) = &self.udid {
            cmd_args.push("--udid");
            cmd_args.push(u);
        }

        let (status, _, stderr) = run_cmd("idevicescreenshot", &cmd_args).await.map_err(|e| {
            crate::error::SyscityError::ExternalService {
                source: "idevicescreenshot failed".to_string(),
                cause: Some(Box::new(e)),
            }
        })?;

        if !status.success() {
            return Ok(ToolExecutionResult::error(format!("idevicescreenshot failed: {}", stderr)));
        }

        // Convert TIFF to PNG via sips (macOS) or ImageMagick convert
        let png_file = tmp_file.replace(".tiff", ".png");
        let convert_result = tokio::process::Command::new("sips")
            .args(["-s", "format", "png", &tmp_file, "--out", &png_file])
            .output()
            .await;

        let png_data = match convert_result {
            Ok(out) if out.status.success() => tokio::fs::read(&png_file).await.ok(),
            _ => {
                // Fallback: try ImageMagick
                let out = tokio::process::Command::new("convert")
                    .args([&tmp_file, &png_file])
                    .output()
                    .await;
                match out {
                    Ok(o) if o.status.success() => tokio::fs::read(&png_file).await.ok(),
                    _ => None,
                }
            }
        };

        // Clean up temp files
        if let Err(e) = tokio::fs::remove_file(&tmp_file).await {
            tracing::warn!("Failed to cleanup temp file '{}': {}", tmp_file, e);
        }
        if let Err(e) = tokio::fs::remove_file(&png_file).await {
            tracing::warn!("Failed to cleanup temp file '{}': {}", png_file, e);
        }

        let base64 = match png_data {
            Some(data) => base64::Engine::encode(&base64::engine::general_purpose::STANDARD, &data),
            None => {
                return Ok(ToolExecutionResult::error(
                    "Screenshot captured but TIFF→PNG conversion failed".to_string(),
                ))
            }
        };

        Ok(
            ToolExecutionResult::success("Screenshot captured").with_data(serde_json::json!({
                "base64": base64,
                "format": "png",
            })),
        )
    }
}

// ── ideviceinstaller ───────────────────────────────────────────────────────

#[derive(Debug)]
pub struct IdeviceInstallerTool {
    udid: Option<String>,
}

impl Default for IdeviceInstallerTool {
    fn default() -> Self {
        Self::new()
    }
}

impl IdeviceInstallerTool {
    pub fn new() -> Self {
        Self { udid: None }
    }

    pub fn with_udid(mut self, udid: String) -> Self {
        self.udid = Some(udid);
        self
    }
}

#[async_trait]
impl Tool for IdeviceInstallerTool {
    fn name(&self) -> &str {
        "ios_app_manager"
    }

    fn description(&self) -> &str {
        "Install, uninstall, or list apps on an iOS device via ideviceinstaller."
    }

    fn parameters_schema(&self) -> Value {
        create_schema(
            "Manage iOS apps",
            serde_json::json!({
                "action": {
                    "type": "string",
                    "description": "install | uninstall | list",
                    "enum": ["install", "uninstall", "list"]
                },
                "bundle_id": { "type": "string", "description": "App bundle identifier" },
                "ipa_path": { "type": "string", "description": "Path to .ipa file (install)" },
                "udid": { "type": "string", "description": "Optional device UDID" }
            }),
            vec!["action"],
        )
    }

    async fn execute(
        &self,
        args: Value,
        _context: &ToolContext,
    ) -> crate::Result<ToolExecutionResult> {
        let action = args.get("action").and_then(|v| v.as_str()).unwrap_or("");

        let mut cmd_args: Vec<String> = Vec::new();
        if let Some(u) = &self.udid {
            cmd_args.push("--udid".to_string());
            cmd_args.push(u.clone());
        }

        match action {
            "install" => {
                let path = args.get("ipa_path").and_then(|v| v.as_str()).unwrap_or("");
                cmd_args.push("--install".to_string());
                cmd_args.push(path.to_string());
            }
            "uninstall" => {
                let bundle = args.get("bundle_id").and_then(|v| v.as_str()).unwrap_or("");
                cmd_args.push("--uninstall".to_string());
                cmd_args.push(bundle.to_string());
            }
            "list" => {
                cmd_args.push("--list-apps".to_string());
            }
            _ => return Ok(ToolExecutionResult::error(format!("Unknown action: {}", action))),
        }

        let (status, stdout, stderr) =
            run_cmd("ideviceinstaller", &cmd_args.iter().map(|s| s.as_str()).collect::<Vec<_>>())
                .await
                .map_err(|e| crate::error::SyscityError::ExternalService {
                    source: "ideviceinstaller failed".to_string(),
                    cause: Some(Box::new(e)),
                })?;

        if !status.success() {
            return Ok(ToolExecutionResult::error(format!("ideviceinstaller failed: {}", stderr)));
        }

        Ok(ToolExecutionResult::success(format!("Action '{}' completed", action))
            .with_data(serde_json::json!({ "output": stdout })))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_ios_toolset_id() {
        let set = IosToolset::new();
        assert_eq!(set.id(), "ios");
    }

    #[test]
    fn test_idevice_id_tool_name() {
        let tool = IdeviceIdTool::new();
        assert_eq!(tool.name(), "ios_list_devices");
    }

    #[test]
    fn test_idevice_screenshot_tool_schema() {
        let tool = IdeviceScreenshotTool::new();
        let schema = tool.parameters_schema();
        assert!(schema.get("properties").is_some());
    }
}
