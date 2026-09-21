//! Device capability tools exposed to the agent (mobile-migration §4.1/§4.2).
//!
//! Each tool wraps the optional [`DeviceBridge`] and forwards its command to
//! the platform plugin. On desktop the bridge is `None`, so `is_available()`
//! returns false and the LLM never sees the tool.
//!
//! The bridge responses are treated as opaque JSON — the payload contracts
//! are defined by the Kotlin `DevicePlugin` and validated at the tool boundary
//! only as far as needed to produce a useful `ToolExecutionResult`.

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{json, Value};

use super::{
    DeviceBridge, CMD_CAPTURE_CAMERA, CMD_GET_LOCATION, CMD_HAPTIC, CMD_NOTIFY, CMD_PICK_FILE,
    CMD_RUN_SHORTCUT, CMD_SHORTCUT_INBOX, CMD_SHORTCUT_RESULTS,
};
use crate::tools::{
    approval::RiskLevel, create_schema, sdk::ToolCapabilities, Tool, ToolContext,
    ToolExecutionResult,
};

/// Error text used when the bridge is absent. Kept in one place so the WS
/// handlers and the tools agree.
pub(crate) const NO_BRIDGE_MSG: &str = "Native device bridge is not available on this platform";

/// Helper shared by every device tool: extract the bridge or return the
/// standard "unsupported platform" error.
fn bridge(b: &Option<Arc<dyn DeviceBridge>>) -> crate::Result<Arc<dyn DeviceBridge>> {
    b.clone()
        .ok_or_else(|| crate::error::SyscityError::Unsupported(NO_BRIDGE_MSG.to_string()))
}

/// Run a bridge command, returning the raw response for the caller to shape.
async fn run(b: &Arc<dyn DeviceBridge>, command: &str, payload: Value) -> crate::Result<Value> {
    b.call(command, payload).await
}

/// Device camera tool — `device_camera`.
///
/// Captures a photo with the device's rear/front camera. The captured image
/// is written under the syscity data directory and the returned `path` can be
/// read with `file_read`.
pub struct DeviceCameraTool {
    bridge: Option<Arc<dyn DeviceBridge>>,
}

impl DeviceCameraTool {
    pub fn new(bridge: Option<Arc<dyn DeviceBridge>>) -> Self {
        Self { bridge }
    }
}

#[async_trait]
impl Tool for DeviceCameraTool {
    fn name(&self) -> &str {
        "device_camera"
    }

    fn description(&self) -> &str {
        "Capture a photo using the device's camera. Returns the saved image path (readable with file_read), width, and height. Only available on mobile devices."
    }

    fn parameters_schema(&self) -> Value {
        create_schema("No parameters", json!({}), Vec::<String>::new())
    }

    fn capabilities(&self) -> ToolCapabilities {
        ToolCapabilities {
            requires_approval: true,
            risk_level: RiskLevel::Medium,
            categories: vec!["device".to_string(), "multimedia".to_string()],
            ..Default::default()
        }
    }

    fn is_available(&self, _context: &ToolContext) -> bool {
        self.bridge.is_some()
    }

    async fn execute(
        &self,
        _args: Value,
        _context: &ToolContext,
    ) -> crate::Result<ToolExecutionResult> {
        let b = bridge(&self.bridge)?;
        let data = run(&b, CMD_CAPTURE_CAMERA, json!({})).await?;
        let path = data
            .get("path")
            .and_then(|v| v.as_str())
            .unwrap_or("unknown");
        Ok(ToolExecutionResult::success(format!("Captured photo saved to {path}")).with_data(data))
    }
}

/// Device geolocation tool — `device_geolocate`.
///
/// Returns the current GPS/network location fix. May take several seconds
/// while the location services converge.
pub struct DeviceGeolocateTool {
    bridge: Option<Arc<dyn DeviceBridge>>,
}

impl DeviceGeolocateTool {
    pub fn new(bridge: Option<Arc<dyn DeviceBridge>>) -> Self {
        Self { bridge }
    }
}

#[async_trait]
impl Tool for DeviceGeolocateTool {
    fn name(&self) -> &str {
        "device_geolocate"
    }

    fn description(&self) -> &str {
        "Return the device's current location as {latitude, longitude, accuracy_meters, timestamp_ms}. Only available on mobile devices."
    }

    fn parameters_schema(&self) -> Value {
        create_schema("No parameters", json!({}), Vec::<String>::new())
    }

    fn capabilities(&self) -> ToolCapabilities {
        ToolCapabilities {
            requires_approval: true,
            risk_level: RiskLevel::Medium,
            categories: vec!["device".to_string(), "location".to_string()],
            ..Default::default()
        }
    }

    fn is_available(&self, _context: &ToolContext) -> bool {
        self.bridge.is_some()
    }

    async fn execute(
        &self,
        _args: Value,
        _context: &ToolContext,
    ) -> crate::Result<ToolExecutionResult> {
        let b = bridge(&self.bridge)?;
        let data = run(&b, CMD_GET_LOCATION, json!({})).await?;
        let lat = data.get("latitude").and_then(|v| v.as_f64()).unwrap_or(0.0);
        let lon = data
            .get("longitude")
            .and_then(|v| v.as_f64())
            .unwrap_or(0.0);
        Ok(ToolExecutionResult::success(format!("Current location: {lat:.5}, {lon:.5}"))
            .with_data(data))
    }
}

/// Device notification tool — `device_notify`.
///
/// Posts a heads-up notification on the device. Useful to surface an
/// important event to the user even when they are not looking at the app.
pub struct DeviceNotifyTool {
    bridge: Option<Arc<dyn DeviceBridge>>,
}

impl DeviceNotifyTool {
    pub fn new(bridge: Option<Arc<dyn DeviceBridge>>) -> Self {
        Self { bridge }
    }
}

#[async_trait]
impl Tool for DeviceNotifyTool {
    fn name(&self) -> &str {
        "device_notify"
    }

    fn description(&self) -> &str {
        "Post a heads-up notification on the device with the given title and body. Only available on mobile devices."
    }

    fn parameters_schema(&self) -> Value {
        create_schema(
            "Post a device notification",
            json!({
                "title": {
                    "type": "string",
                    "description": "Notification title (short)"
                },
                "body": {
                    "type": "string",
                    "description": "Notification body text"
                }
            }),
            vec!["title", "body"],
        )
    }

    fn capabilities(&self) -> ToolCapabilities {
        ToolCapabilities {
            requires_approval: true,
            risk_level: RiskLevel::Low,
            categories: vec!["device".to_string(), "communication".to_string()],
            ..Default::default()
        }
    }

    fn is_available(&self, _context: &ToolContext) -> bool {
        self.bridge.is_some()
    }

    async fn execute(
        &self,
        args: Value,
        _context: &ToolContext,
    ) -> crate::Result<ToolExecutionResult> {
        let b = bridge(&self.bridge)?;
        let title = args
            .get("title")
            .and_then(|v| v.as_str())
            .unwrap_or("Syscity")
            .to_string();
        let body = args
            .get("body")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        run(&b, CMD_NOTIFY, json!({ "title": title, "body": body })).await?;
        Ok(ToolExecutionResult::success("Notification posted"))
    }
}

/// Device haptic tool — `device_haptic`.
///
/// Triggers a short vibration on the device.
pub struct DeviceHapticTool {
    bridge: Option<Arc<dyn DeviceBridge>>,
}

impl DeviceHapticTool {
    pub fn new(bridge: Option<Arc<dyn DeviceBridge>>) -> Self {
        Self { bridge }
    }
}

#[async_trait]
impl Tool for DeviceHapticTool {
    fn name(&self) -> &str {
        "device_haptic"
    }

    fn description(&self) -> &str {
        "Trigger a short vibration on the device (haptic feedback). Only available on mobile devices."
    }

    fn parameters_schema(&self) -> Value {
        create_schema(
            "Trigger a device vibration",
            json!({
                "duration_ms": {
                    "type": "integer",
                    "description": "Vibration duration in milliseconds (default 200)"
                }
            }),
            Vec::<String>::new(),
        )
    }

    fn capabilities(&self) -> ToolCapabilities {
        ToolCapabilities {
            requires_approval: true,
            risk_level: RiskLevel::Low,
            categories: vec!["device".to_string(), "feedback".to_string()],
            ..Default::default()
        }
    }

    fn is_available(&self, _context: &ToolContext) -> bool {
        self.bridge.is_some()
    }

    async fn execute(
        &self,
        args: Value,
        _context: &ToolContext,
    ) -> crate::Result<ToolExecutionResult> {
        let b = bridge(&self.bridge)?;
        let duration_ms = args
            .get("duration_ms")
            .and_then(|v| v.as_u64())
            .unwrap_or(200);
        run(&b, CMD_HAPTIC, json!({ "duration_ms": duration_ms })).await?;
        Ok(ToolExecutionResult::success(format!("Vibrated for {duration_ms} ms")))
    }
}

/// Device file-picker tool — `device_pick_file` (SAF, §4.2).
///
/// Opens the system document picker (Storage Access Framework). The chosen
/// file is copied under the syscity data directory so the standard file tools
/// (`file_read`, `file_write`, `file_edit`) work on it unchanged.
pub struct DevicePickFileTool {
    bridge: Option<Arc<dyn DeviceBridge>>,
}

impl DevicePickFileTool {
    pub fn new(bridge: Option<Arc<dyn DeviceBridge>>) -> Self {
        Self { bridge }
    }
}

#[async_trait]
impl Tool for DevicePickFileTool {
    fn name(&self) -> &str {
        "device_pick_file"
    }

    fn description(&self) -> &str {
        "Open the device's document picker and let the user choose a file. The chosen file is copied into the syscity data directory; returns its path (readable with file_read), original name, and size in bytes. Only available on mobile devices."
    }

    fn parameters_schema(&self) -> Value {
        create_schema("No parameters", json!({}), Vec::<String>::new())
    }

    fn capabilities(&self) -> ToolCapabilities {
        ToolCapabilities {
            requires_approval: true,
            risk_level: RiskLevel::Medium,
            categories: vec!["device".to_string(), "file".to_string()],
            ..Default::default()
        }
    }

    fn is_available(&self, _context: &ToolContext) -> bool {
        self.bridge.is_some()
    }

    async fn execute(
        &self,
        _args: Value,
        _context: &ToolContext,
    ) -> crate::Result<ToolExecutionResult> {
        let b = bridge(&self.bridge)?;
        let data = run(&b, CMD_PICK_FILE, json!({})).await?;
        let path = data
            .get("path")
            .and_then(|v| v.as_str())
            .unwrap_or("unknown");
        let name = data.get("name").and_then(|v| v.as_str()).unwrap_or(path);
        let size = data.get("size_bytes").and_then(|v| v.as_u64()).unwrap_or(0);
        Ok(ToolExecutionResult::success(format!(
            "Picked file '{name}' ({size} bytes) saved to {path}"
        ))
        .with_data(data))
    }
}

/// Device shortcut hand-off tool — `ios_shortcut_run` (§4.6).
///
/// Hands off to the Shortcuts app via the public `shortcuts://run-shortcut`
/// URL scheme. The shortcut runs visibly in the Shortcuts app (foreground
/// hand-off); its final step invokes Syscity's `SyscityOutputIntent`, which
/// writes the output into the app sandbox for `ios_shortcut_results` to pick
/// up. iOS only.
pub struct DeviceShortcutRunTool {
    bridge: Option<Arc<dyn DeviceBridge>>,
}

impl DeviceShortcutRunTool {
    pub fn new(bridge: Option<Arc<dyn DeviceBridge>>) -> Self {
        Self { bridge }
    }
}

#[async_trait]
impl Tool for DeviceShortcutRunTool {
    fn name(&self) -> &str {
        "ios_shortcut_run"
    }

    fn description(&self) -> &str {
        "Run an iOS Shortcut by name, optionally passing text input. The shortcut runs in the Shortcuts app; its final step can return output to Syscity via the SyscityOutput AppIntent. Only available on iOS."
    }

    fn parameters_schema(&self) -> Value {
        create_schema(
            "Run an iOS Shortcut",
            json!({
                "name": {
                    "type": "string",
                    "description": "Name of the shortcut to run (as shown in the Shortcuts app)"
                },
                "input": {
                    "type": "string",
                    "description": "Optional text input passed to the shortcut"
                }
            }),
            vec!["name"],
        )
    }

    fn capabilities(&self) -> ToolCapabilities {
        ToolCapabilities {
            requires_approval: true,
            risk_level: RiskLevel::Medium,
            categories: vec!["device".to_string(), "automation".to_string()],
            ..Default::default()
        }
    }

    fn is_available(&self, _context: &ToolContext) -> bool {
        self.bridge.is_some()
    }

    async fn execute(
        &self,
        args: Value,
        _context: &ToolContext,
    ) -> crate::Result<ToolExecutionResult> {
        let b = bridge(&self.bridge)?;
        let name = args
            .get("name")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string();
        let input = args
            .get("input")
            .and_then(|v| v.as_str())
            .map(str::to_string);
        let data = run(&b, CMD_RUN_SHORTCUT, json!({ "name": name, "input": input })).await?;
        let launched = data
            .get("launched")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        Ok(ToolExecutionResult::success(if launched {
            format!("Launched Shortcut '{name}' — it runs in the Shortcuts app")
        } else {
            format!("Could not launch Shortcut '{name}' (not found or hand-off denied)")
        })
        .with_data(data))
    }
}

/// Device shortcut results tool — `ios_shortcut_results` (§4.6).
///
/// Lists and consumes outputs produced by the `SyscityOutputIntent`. Results
/// are delete-read: once returned they are removed so the agent never sees the
/// same output twice.
pub struct DeviceShortcutResultsTool {
    bridge: Option<Arc<dyn DeviceBridge>>,
}

impl DeviceShortcutResultsTool {
    pub fn new(bridge: Option<Arc<dyn DeviceBridge>>) -> Self {
        Self { bridge }
    }
}

#[async_trait]
impl Tool for DeviceShortcutResultsTool {
    fn name(&self) -> &str {
        "ios_shortcut_results"
    }

    fn description(&self) -> &str {
        "List outputs returned to Syscity by the SyscityOutput AppIntent (the final step of an iOS Shortcut). Each entry is {output, at_ms}. Reading a result consumes it. Only available on iOS."
    }

    fn parameters_schema(&self) -> Value {
        create_schema("No parameters", json!({}), Vec::<String>::new())
    }

    fn capabilities(&self) -> ToolCapabilities {
        ToolCapabilities {
            read_only: true,
            requires_approval: false,
            risk_level: RiskLevel::Low,
            categories: vec!["device".to_string(), "automation".to_string()],
            ..Default::default()
        }
    }

    fn is_available(&self, _context: &ToolContext) -> bool {
        self.bridge.is_some()
    }

    async fn execute(
        &self,
        _args: Value,
        _context: &ToolContext,
    ) -> crate::Result<ToolExecutionResult> {
        let b = bridge(&self.bridge)?;
        let data = run(&b, CMD_SHORTCUT_RESULTS, json!({})).await?;
        let items = data
            .get("items")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();
        let n = items.len();
        Ok(
            ToolExecutionResult::success(format!("Returned {n} shortcut output(s): {items:?}"))
                .with_data(data),
        )
    }
}

/// Device shortcut inbox tool — `ios_shortcut_inbox` (§4.6).
///
/// Lists and consumes prompts sent in via the `AskSyscityIntent` (Siri /
/// Shortcuts automation). Prompts are delete-read like results.
pub struct DeviceShortcutInboxTool {
    bridge: Option<Arc<dyn DeviceBridge>>,
}

impl DeviceShortcutInboxTool {
    pub fn new(bridge: Option<Arc<dyn DeviceBridge>>) -> Self {
        Self { bridge }
    }
}

#[async_trait]
impl Tool for DeviceShortcutInboxTool {
    fn name(&self) -> &str {
        "ios_shortcut_inbox"
    }

    fn description(&self) -> &str {
        "List prompts sent to Syscity via the AskSyscity AppIntent (Siri or a Shortcuts automation). Each entry is {prompt, at_ms}. Reading a prompt consumes it. Only available on iOS."
    }

    fn parameters_schema(&self) -> Value {
        create_schema("No parameters", json!({}), Vec::<String>::new())
    }

    fn capabilities(&self) -> ToolCapabilities {
        ToolCapabilities {
            read_only: true,
            requires_approval: false,
            risk_level: RiskLevel::Low,
            categories: vec!["device".to_string(), "automation".to_string()],
            ..Default::default()
        }
    }

    fn is_available(&self, _context: &ToolContext) -> bool {
        self.bridge.is_some()
    }

    async fn execute(
        &self,
        _args: Value,
        _context: &ToolContext,
    ) -> crate::Result<ToolExecutionResult> {
        let b = bridge(&self.bridge)?;
        let data = run(&b, CMD_SHORTCUT_INBOX, json!({})).await?;
        let items = data
            .get("items")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();
        let n = items.len();
        Ok(ToolExecutionResult::success(format!(
            "Returned {n} prompt(s) from the inbox: {items:?}"
        ))
        .with_data(data))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::device::tests::MockDeviceBridge;

    fn with_bridge() -> Arc<dyn DeviceBridge> {
        Arc::new(MockDeviceBridge::new(
            json!({ "path": "/data/cam.jpg", "width": 100, "height": 200 }),
        ))
    }

    fn context() -> ToolContext {
        crate::tools::ToolContext::default()
    }

    #[test]
    fn test_tools_unavailable_without_bridge() {
        let camera = DeviceCameraTool::new(None);
        let geolocate = DeviceGeolocateTool::new(None);
        let notify = DeviceNotifyTool::new(None);
        let haptic = DeviceHapticTool::new(None);
        let pick = DevicePickFileTool::new(None);
        let shortcut = DeviceShortcutRunTool::new(None);
        let results = DeviceShortcutResultsTool::new(None);
        let inbox = DeviceShortcutInboxTool::new(None);
        assert!(!camera.is_available(&context()));
        assert!(!geolocate.is_available(&context()));
        assert!(!notify.is_available(&context()));
        assert!(!haptic.is_available(&context()));
        assert!(!pick.is_available(&context()));
        assert!(!shortcut.is_available(&context()));
        assert!(!results.is_available(&context()));
        assert!(!inbox.is_available(&context()));
    }

    #[test]
    fn test_tools_available_with_bridge() {
        let bridge = Some(with_bridge());
        assert!(DeviceCameraTool::new(bridge.clone()).is_available(&context()));
        assert!(DeviceGeolocateTool::new(bridge.clone()).is_available(&context()));
        assert!(DeviceNotifyTool::new(bridge.clone()).is_available(&context()));
        assert!(DeviceHapticTool::new(bridge.clone()).is_available(&context()));
        assert!(DevicePickFileTool::new(bridge.clone()).is_available(&context()));
        assert!(DeviceShortcutRunTool::new(bridge.clone()).is_available(&context()));
        assert!(DeviceShortcutResultsTool::new(bridge.clone()).is_available(&context()));
        assert!(DeviceShortcutInboxTool::new(bridge).is_available(&context()));
    }

    #[tokio::test]
    async fn test_camera_forwards_and_wraps() {
        let b: Arc<MockDeviceBridge> = Arc::new(MockDeviceBridge::new(json!({
            "path": "camera/IMG_1.jpg", "width": 4032, "height": 3024
        })));
        let tool = DeviceCameraTool::new(Some(b.clone()));
        let result = tool.execute(json!({}), &context()).await.unwrap();
        assert!(result.output.contains("camera/IMG_1.jpg"));
        assert_eq!(result.data.as_ref().unwrap()["width"], 4032);
        let calls = b.calls();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].0, CMD_CAPTURE_CAMERA);
    }

    #[tokio::test]
    async fn test_geolocate_forwards() {
        let b: Arc<MockDeviceBridge> = Arc::new(MockDeviceBridge::new(json!({
            "latitude": 31.2304, "longitude": 121.4737, "accuracy_meters": 10, "timestamp_ms": 1
        })));
        let tool = DeviceGeolocateTool::new(Some(b.clone()));
        let result = tool.execute(json!({}), &context()).await.unwrap();
        assert!(result.output.contains("31.2304"));
        assert_eq!(result.data.as_ref().unwrap()["longitude"], 121.4737);
        assert_eq!(b.calls()[0].0, CMD_GET_LOCATION);
    }

    #[tokio::test]
    async fn test_notify_forwards_payload() {
        let b: Arc<MockDeviceBridge> =
            Arc::new(MockDeviceBridge::new(json!({ "delivered": true })));
        let tool = DeviceNotifyTool::new(Some(b.clone()));
        let result = tool
            .execute(json!({ "title": "Hi", "body": "World" }), &context())
            .await
            .unwrap();
        assert!(result.success);
        let calls = b.calls();
        assert_eq!(calls[0].0, CMD_NOTIFY);
        assert_eq!(calls[0].1["title"], "Hi");
        assert_eq!(calls[0].1["body"], "World");
    }

    #[tokio::test]
    async fn test_haptic_forwards_duration() {
        let b: Arc<MockDeviceBridge> = Arc::new(MockDeviceBridge::new(json!({})));
        let tool = DeviceHapticTool::new(Some(b.clone()));
        let result = tool
            .execute(json!({ "duration_ms": 500 }), &context())
            .await
            .unwrap();
        assert!(result.output.contains("500"));
        assert_eq!(b.calls()[0].1["duration_ms"], 500);
    }

    #[tokio::test]
    async fn test_pick_file_forwards() {
        let b: Arc<MockDeviceBridge> = Arc::new(MockDeviceBridge::new(json!({
            "path": "user-files/report.pdf", "name": "report.pdf", "size_bytes": 1024
        })));
        let tool = DevicePickFileTool::new(Some(b.clone()));
        let result = tool.execute(json!({}), &context()).await.unwrap();
        assert!(result.output.contains("report.pdf"));
        assert_eq!(result.data.as_ref().unwrap()["size_bytes"], 1024);
        assert_eq!(b.calls()[0].0, CMD_PICK_FILE);
    }

    #[tokio::test]
    async fn test_shortcut_run_forwards_payload() {
        let b: Arc<MockDeviceBridge> = Arc::new(MockDeviceBridge::new(json!({ "launched": true })));
        let tool = DeviceShortcutRunTool::new(Some(b.clone()));
        let result = tool
            .execute(json!({ "name": "Order Coffee", "input": "large" }), &context())
            .await
            .unwrap();
        assert!(result.success);
        let calls = b.calls();
        assert_eq!(calls[0].0, CMD_RUN_SHORTCUT);
        assert_eq!(calls[0].1["name"], "Order Coffee");
        assert_eq!(calls[0].1["input"], "large");
    }

    #[tokio::test]
    async fn test_shortcut_run_optional_input() {
        let b: Arc<MockDeviceBridge> = Arc::new(MockDeviceBridge::new(json!({ "launched": true })));
        let tool = DeviceShortcutRunTool::new(Some(b.clone()));
        tool.execute(json!({ "name": "DoThing" }), &context())
            .await
            .unwrap();
        let calls = b.calls();
        assert_eq!(calls[0].0, CMD_RUN_SHORTCUT);
        assert_eq!(calls[0].1["name"], "DoThing");
        assert!(calls[0].1.get("input").is_some_and(|v| v.is_null()));
    }

    #[tokio::test]
    async fn test_shortcut_results_consume() {
        let b: Arc<MockDeviceBridge> = Arc::new(MockDeviceBridge::new(json!({
            "items": [ { "output": "answer", "at_ms": 123 } ]
        })));
        let tool = DeviceShortcutResultsTool::new(Some(b.clone()));
        let result = tool.execute(json!({}), &context()).await.unwrap();
        assert!(result.output.contains("answer"));
        assert_eq!(result.data.as_ref().unwrap()["items"][0]["output"], "answer");
        assert_eq!(b.calls()[0].0, CMD_SHORTCUT_RESULTS);
    }

    #[tokio::test]
    async fn test_shortcut_inbox_consume() {
        let b: Arc<MockDeviceBridge> = Arc::new(MockDeviceBridge::new(json!({
            "items": [ { "prompt": "what is the weather?", "at_ms": 123 } ]
        })));
        let tool = DeviceShortcutInboxTool::new(Some(b.clone()));
        let result = tool.execute(json!({}), &context()).await.unwrap();
        assert!(result.output.contains("what is the weather?"));
        assert_eq!(result.data.as_ref().unwrap()["items"][0]["prompt"], "what is the weather?");
        assert_eq!(b.calls()[0].0, CMD_SHORTCUT_INBOX);
    }

    #[tokio::test]
    async fn test_execute_without_bridge_errors() {
        let tool = DeviceCameraTool::new(None);
        let err = tool.execute(json!({}), &context()).await.unwrap_err();
        assert!(err.to_string().contains("not available"));
    }
}
