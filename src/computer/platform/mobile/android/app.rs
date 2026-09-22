//! Android app manager tool — install, launch, force-stop and list packages.

use async_trait::async_trait;
use serde_json::Value;

use super::{adb_args, resolve_device, resolved_serial, DEVICE_PARAM};
use crate::computer::platform::mobile::run_cmd;
use crate::tools::{create_schema, Tool, ToolContext, ToolExecutionResult};

// ── ADB App Manager Tool ───────────────────────────────────────────────────

/// Install, launch, force-stop, and list Android apps.
#[derive(Debug)]
pub struct AdbAppManagerTool {
    device: Option<String>,
}

impl Default for AdbAppManagerTool {
    fn default() -> Self {
        Self::new()
    }
}

impl AdbAppManagerTool {
    pub fn new() -> Self {
        Self { device: None }
    }

    pub fn with_device(mut self, device: String) -> Self {
        self.device = Some(device);
        self
    }
}

#[async_trait]
impl Tool for AdbAppManagerTool {
    fn name(&self) -> &str {
        "android_app_manager"
    }

    fn description(&self) -> &str {
        "Install, launch, force-stop, or list apps on an Android device. 'completed' means the \
         adb command was dispatched, not that the app started — observe afterwards to see."
    }

    fn parameters_schema(&self) -> Value {
        create_schema(
            "Manage Android apps",
            serde_json::json!({
                "action": {
                    "type": "string",
                    "description": "install | launch | force_stop | list_packages",
                    "enum": ["install", "launch", "force_stop", "list_packages"]
                },
                "package": { "type": "string", "description": "Package name (e.g. com.example.app)" },
                "activity": { "type": "string", "description": "Activity class (launch)" },
                "apk_path": { "type": "string", "description": "Local path to APK (install)" },
                "device": { "type": "string", "description": DEVICE_PARAM }
            }),
            vec!["action"],
        )
    }

    async fn execute(
        &self,
        args: Value,
        _context: &ToolContext,
    ) -> crate::Result<ToolExecutionResult> {
        // One device: two conversations must not type into it at once. See
        // `tools::target_lock`.
        let _target = crate::tools::target_lock::TargetLocks::global()
            .lock(crate::tools::target_lock::ANDROID)
            .await;
        let action = args.get("action").and_then(|v| v.as_str()).unwrap_or("");
        let device = match resolve_device(&args, self.device.as_deref()) {
            Ok(device) => device,
            Err(refusal) => return Ok(ToolExecutionResult::error(refusal)),
        };

        let argv = match action {
            "install" => {
                let path = args.get("apk_path").and_then(|v| v.as_str()).unwrap_or("");
                adb_args(&device, &["install", "-r", path])
            }
            "launch" => {
                let pkg = args.get("package").and_then(|v| v.as_str()).unwrap_or("");
                let activity = args.get("activity").and_then(|v| v.as_str());
                let component = match activity {
                    Some(a) => format!("{}/{}", pkg, a),
                    None => format!("{}/.MainActivity", pkg),
                };
                adb_args(&device, &["shell", "am", "start", "-n", &component])
            }
            "force_stop" => {
                let pkg = args.get("package").and_then(|v| v.as_str()).unwrap_or("");
                adb_args(&device, &["shell", "am", "force-stop", pkg])
            }
            "list_packages" => adb_args(&device, &["shell", "pm", "list", "packages"]),
            _ => return Ok(ToolExecutionResult::error(format!("Unknown action: {}", action))),
        };

        let (status, stdout, stderr) =
            run_cmd("adb", &argv.iter().map(|s| s.as_str()).collect::<Vec<_>>())
                .await
                .map_err(|e| crate::error::SyscityError::ExternalService {
                    source: "adb app manager failed".to_string(),
                    cause: Some(Box::new(e)),
                })?;

        if !status.success() {
            return Ok(ToolExecutionResult::error(format!("adb app manager failed: {}", stderr)));
        }

        Ok(
            ToolExecutionResult::success(format!("Action '{}' completed\n{}", action, stdout))
                .with_data(serde_json::json!({ "device": resolved_serial(&device).await })),
        )
    }
}
