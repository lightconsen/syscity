//! Android loopback self-pairing tools (§4.5).

use async_trait::async_trait;
use serde_json::Value;

use crate::tools::{create_schema, Tool, ToolContext, ToolExecutionResult};

// ── ADB Pairing Tools (§4.5) ───────────────────────────────────────────────

/// Pair the phone with its own wireless-debugging adbd over loopback (§4.5).
///
/// Requires the bundled adb client (scripts/fetch-android-adb.sh). The agent
/// first pairs with the "Pair device with pairing code" dialog's port + code,
/// then connects to the connect port shown on the wireless-debugging screen.
#[derive(Debug)]
pub struct AdbPairTool {
    device: Option<String>,
}

impl Default for AdbPairTool {
    fn default() -> Self {
        Self::new()
    }
}

impl AdbPairTool {
    pub fn new() -> Self {
        Self { device: None }
    }

    pub fn with_device(mut self, device: String) -> Self {
        self.device = Some(device);
        self
    }
}

#[async_trait]
impl Tool for AdbPairTool {
    fn name(&self) -> &str {
        "device_adb_pair"
    }

    fn description(&self) -> &str {
        "Pair this phone with its own wireless-debugging adb server over loopback. Pass the pairing port and code from the 'Pair device with pairing code' dialog, plus the connect port shown on the Wireless debugging screen. Returns whether pairing and connecting succeeded and the device list."
    }

    fn parameters_schema(&self) -> Value {
        create_schema(
            "Pair loopback adb",
            serde_json::json!({
                "port": {
                    "type": "integer",
                    "description": "Pairing port from the 'Pair device with pairing code' dialog"
                },
                "code": {
                    "type": "string",
                    "description": "Six-digit pairing code from the dialog"
                },
                "connect_port": {
                    "type": "integer",
                    "description": "Optional connect port from the Wireless debugging screen (defaults to port)"
                }
            }),
            vec!["port", "code"],
        )
    }

    async fn execute(
        &self,
        args: Value,
        _context: &ToolContext,
    ) -> crate::Result<ToolExecutionResult> {
        let port = args.get("port").and_then(|v| v.as_u64()).unwrap_or(0) as u16;
        let code = args
            .get("code")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let connect_port = args
            .get("connect_port")
            .and_then(|v| v.as_u64())
            .map(|p| p as u16);

        let data = crate::computer::platform::mobile::adb_pair(port, &code, connect_port).await?;
        Ok(
            ToolExecutionResult::success("ADB pairing attempted").with_data(serde_json::json!({
                "paired": data.get("paired").and_then(|v| v.as_bool()).unwrap_or(false),
                "connected": data.get("connected").and_then(|v| v.as_bool()).unwrap_or(false),
                "pair_output": data.get("pair_output"),
                "connect_output": data.get("connect_output"),
                "devices": data.get("devices"),
            })),
        )
    }
}

/// Report loopback adb pairing status (§4.5).
#[derive(Debug)]
pub struct AdbStatusTool {
    device: Option<String>,
}

impl Default for AdbStatusTool {
    fn default() -> Self {
        Self::new()
    }
}

impl AdbStatusTool {
    pub fn new() -> Self {
        Self { device: None }
    }

    pub fn with_device(mut self, device: String) -> Self {
        self.device = Some(device);
        self
    }
}

#[async_trait]
impl Tool for AdbStatusTool {
    fn name(&self) -> &str {
        "device_adb_status"
    }

    fn description(&self) -> &str {
        "Report whether this phone is paired with its own adb server and list the devices adb can see."
    }

    fn parameters_schema(&self) -> Value {
        create_schema("Loopback adb status", serde_json::json!({}), Vec::<String>::new())
    }

    async fn execute(
        &self,
        _args: Value,
        _context: &ToolContext,
    ) -> crate::Result<ToolExecutionResult> {
        let data = crate::computer::platform::mobile::adb_status().await?;
        Ok(ToolExecutionResult::success("ADB status").with_data(
            serde_json::json!({ "paired": data.get("paired"), "devices": data.get("devices") }),
        ))
    }
}
