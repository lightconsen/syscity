//! Windows desktop control tool using PowerShell + .NET SendKeys /
//! UIAutomation.

use async_trait::async_trait;
use serde_json::Value;
use tokio::process::Command;
use tokio::time::{timeout, Duration};
use tracing::info;

use crate::tools::{create_schema, Tool, ToolContext, ToolExecutionResult};

/// Desktop control tool for Windows via PowerShell.
///
/// Uses .NET SendKeys for input and Get-Process / UIAutomation for window
/// inspection and activation.
#[derive(Debug)]
pub struct DesktopControlTool;

impl Default for DesktopControlTool {
    fn default() -> Self {
        Self::new()
    }
}

impl DesktopControlTool {
    pub fn new() -> Self {
        Self
    }

    async fn run_ps(script: &str) -> crate::Result<(bool, String, String)> {
        let output = timeout(
            Duration::from_secs(15),
            Command::new("powershell")
                .args([
                    "-NoProfile",
                    "-ExecutionPolicy",
                    "Bypass",
                    "-Command",
                    script,
                ])
                .output(),
        )
        .await;
        match output {
            Ok(Ok(out)) => {
                let stdout = String::from_utf8_lossy(&out.stdout).trim().to_string();
                let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();
                Ok((out.status.success(), stdout, stderr))
            }
            Ok(Err(e)) => Ok((false, String::new(), format!("PowerShell spawn error: {}", e))),
            Err(_) => Ok((false, String::new(), "PowerShell timed out".to_string())),
        }
    }
}

#[async_trait]
impl Tool for DesktopControlTool {
    fn name(&self) -> &str {
        "windows_desktop_control"
    }

    fn description(&self) -> &str {
        "Control the Windows desktop using PowerShell. Supports click, type, key presses, window \
         inspection, and window activation. Input goes to the live session: it moves the real \
         pointer and types into whatever holds focus, so on a desktop someone is using, say what \
         you intend and ask before acting."
    }

    fn parameters_schema(&self) -> Value {
        create_schema(
            "Perform a desktop control action",
            serde_json::json!({
                "action": {
                    "type": "string",
                    "description": "Action: inspect, click, type, key, list_windows, activate_window",
                    "enum": ["inspect", "click", "double_click", "type", "key", "scroll", "drag", "list_windows", "activate_window", "close_window", "get_window_geometry", "move_window", "resize_window", "minimize_window", "maximize_window"]
                },
                "x": {
                    "type": "integer",
                    "description": "X coordinate for click"
                },
                "y": {
                    "type": "integer",
                    "description": "Y coordinate for click"
                },
                "text": {
                    "type": "string",
                    "description": "Text to type"
                },
                "keys": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "Keys to press (e.g. ['ctrl', 'c'] sends ^c)"
                },
                "direction": {
                    "type": "string",
                    "description": "Scroll direction",
                    "enum": ["up", "down", "left", "right"]
                },
                "amount": {
                    "type": "integer",
                    "description": "Scroll amount (wheel clicks)",
                    "default": 3
                },
                "from_x": {
                    "type": "integer",
                    "description": "Start X for drag"
                },
                "from_y": {
                    "type": "integer",
                    "description": "Start Y for drag"
                },
                "to_x": {
                    "type": "integer",
                    "description": "End X for drag"
                },
                "to_y": {
                    "type": "integer",
                    "description": "End Y for drag"
                },
                "name": {
                    "type": "string",
                    "description": "Window name for activate / close"
                },
                "width": {
                    "type": "integer",
                    "description": "Target width (resize_window)"
                },
                "height": {
                    "type": "integer",
                    "description": "Target height (resize_window)"
                }
            }),
            vec!["action"],
        )
    }

    async fn execute(
        &self,
        args: Value,
        _context: &ToolContext,
    ) -> crate::Result<ToolExecutionResult> {
        // One pointer and one focus: two conversations must not drive this
        // display at once. See `tools::target_lock`.
        let _target = crate::tools::target_lock::TargetLocks::global()
            .lock(crate::tools::target_lock::DESKTOP)
            .await;
        let action = args
            .get("action")
            .and_then(|v| v.as_str())
            .unwrap_or("inspect");
        info!("Windows desktop control action: {}", action);

        match action {
            "inspect" => {
                let script = r#"
Add-Type @"
using System;
using System.Runtime.InteropServices;
public class WinAPI {
    [DllImport("user32.dll")]
    public static extern IntPtr GetForegroundWindow();
    [DllImport("user32.dll", CharSet=CharSet.Auto)]
    public static extern int GetWindowText(IntPtr hWnd, System.Text.StringBuilder text, int count);
    [DllImport("user32.dll")]
    public static extern uint GetWindowThreadProcessId(IntPtr hWnd, out uint lpdwProcessId);
}
"@
$hwnd = [WinAPI]::GetForegroundWindow()
$title = New-Object System.Text.StringBuilder 256
[WinAPI]::GetWindowText($hwnd, $title, 256) | Out-Null
$pid = 0
[WinAPI]::GetWindowThreadProcessId($hwnd, [ref]$pid) | Out-Null
$proc = Get-Process -Id $pid -ErrorAction SilentlyContinue
"Window: $($title.ToString())`nPID: $pid`nProcess: $($proc.ProcessName)"
"#;
                let (ok, stdout, err) = Self::run_ps(script).await?;
                if ok {
                    let raw = stdout.clone();
                    Ok(ToolExecutionResult::success(stdout).with_data(serde_json::json!({
                        "raw": raw
                    })))
                } else {
                    Ok(ToolExecutionResult::error(format!("Inspect failed: {}", err)))
                }
            }
            "click" => {
                let x = args.get("x").and_then(|v| v.as_i64()).unwrap_or(0);
                let y = args.get("y").and_then(|v| v.as_i64()).unwrap_or(0);
                let script = format!(
                    r#"
Add-Type -AssemblyName System.Windows.Forms
[System.Windows.Forms.Cursor]::Position = New-Object System.Drawing.Point({}, {})
Add-Type @"
using System; using System.Runtime.InteropServices;
public class Click {{ [DllImport("user32.dll")] public static extern void mouse_event(uint dwFlags, uint dx, uint dy, uint dwData, int dwExtraInfo); }}
"@
[Click]::mouse_event(0x02, 0, 0, 0, 0)
[Click]::mouse_event(0x04, 0, 0, 0, 0)
"#,
                    x, y
                );
                let (ok, _, err) = Self::run_ps(&script).await?;
                if ok {
                    Ok(ToolExecutionResult::success(format!("Clicked at {}, {}", x, y)))
                } else {
                    Ok(ToolExecutionResult::error(format!("Click failed: {}", err)))
                }
            }
            "double_click" => {
                let x = args.get("x").and_then(|v| v.as_i64()).unwrap_or(0);
                let y = args.get("y").and_then(|v| v.as_i64()).unwrap_or(0);
                let script = format!(
                    r#"
Add-Type -AssemblyName System.Windows.Forms
[System.Windows.Forms.Cursor]::Position = New-Object System.Drawing.Point({}, {})
Add-Type @"
using System; using System.Runtime.InteropServices;
public class Click {{ [DllImport("user32.dll")] public static extern void mouse_event(uint dwFlags, uint dx, uint dy, uint dwData, int dwExtraInfo); }}
"@
[Click]::mouse_event(0x02, 0, 0, 0, 0)
[Click]::mouse_event(0x04, 0, 0, 0, 0)
Start-Sleep -Milliseconds 50
[Click]::mouse_event(0x02, 0, 0, 0, 0)
[Click]::mouse_event(0x04, 0, 0, 0, 0)
"#,
                    x, y
                );
                let (ok, _, err) = Self::run_ps(&script).await?;
                if ok {
                    Ok(ToolExecutionResult::success(format!("Double-clicked at {}, {}", x, y)))
                } else {
                    Ok(ToolExecutionResult::error(format!("Double-click failed: {}", err)))
                }
            }
            "scroll" => {
                let x = args.get("x").and_then(|v| v.as_i64()).unwrap_or(0);
                let y = args.get("y").and_then(|v| v.as_i64()).unwrap_or(0);
                let direction = args
                    .get("direction")
                    .and_then(|v| v.as_str())
                    .unwrap_or("down");
                let amount = args.get("amount").and_then(|v| v.as_u64()).unwrap_or(3);
                let delta = if direction == "up" {
                    120i64 * amount as i64
                } else if direction == "down" {
                    -120i64 * amount as i64
                } else {
                    0i64
                };
                let script = format!(
                    r#"
Add-Type -AssemblyName System.Windows.Forms
[System.Windows.Forms.Cursor]::Position = New-Object System.Drawing.Point({}, {})
Add-Type @"
using System; using System.Runtime.InteropServices;
public class Click {{ [DllImport("user32.dll")] public static extern void mouse_event(uint dwFlags, uint dx, uint dy, uint dwData, int dwExtraInfo); }}
"@
[Click]::mouse_event(0x0800, 0, 0, {}, 0)
"#,
                    x, y, delta
                );
                let (ok, _, err) = Self::run_ps(&script).await?;
                if ok {
                    Ok(ToolExecutionResult::success(format!(
                        "Scrolled {} at {}, {}",
                        direction, x, y
                    )))
                } else {
                    Ok(ToolExecutionResult::error(format!("Scroll failed: {}", err)))
                }
            }
            "drag" => {
                let from_x = args.get("from_x").and_then(|v| v.as_i64()).unwrap_or(0);
                let from_y = args.get("from_y").and_then(|v| v.as_i64()).unwrap_or(0);
                let to_x = args.get("to_x").and_then(|v| v.as_i64()).unwrap_or(0);
                let to_y = args.get("to_y").and_then(|v| v.as_i64()).unwrap_or(0);
                let script = format!(
                    r#"
Add-Type -AssemblyName System.Windows.Forms
[System.Windows.Forms.Cursor]::Position = New-Object System.Drawing.Point({}, {})
Add-Type @"
using System; using System.Runtime.InteropServices;
public class Click {{ [DllImport("user32.dll")] public static extern void mouse_event(uint dwFlags, uint dx, uint dy, uint dwData, int dwExtraInfo); }}
"@
[Click]::mouse_event(0x02, 0, 0, 0, 0)
Start-Sleep -Milliseconds 100
[System.Windows.Forms.Cursor]::Position = New-Object System.Drawing.Point({}, {})
[Click]::mouse_event(0x04, 0, 0, 0, 0)
"#,
                    from_x, from_y, to_x, to_y
                );
                let (ok, _, err) = Self::run_ps(&script).await?;
                if ok {
                    Ok(ToolExecutionResult::success(format!(
                        "Dragged from ({}, {}) to ({}, {})",
                        from_x, from_y, to_x, to_y
                    )))
                } else {
                    Ok(ToolExecutionResult::error(format!("Drag failed: {}", err)))
                }
            }
            "type" => {
                let text = args.get("text").and_then(|v| v.as_str()).unwrap_or("");
                let script = format!(
                    r#"
Add-Type -AssemblyName System.Windows.Forms
[System.Windows.Forms.SendKeys]::SendWait('{}')
"#,
                    text.replace("'", "''")
                );
                let (ok, _, err) = Self::run_ps(&script).await?;
                if ok {
                    Ok(ToolExecutionResult::success(format!("Typed: {}", text)))
                } else {
                    Ok(ToolExecutionResult::error(format!("Type failed: {}", err)))
                }
            }
            "key" => {
                let keys: Vec<String> = args
                    .get("keys")
                    .and_then(|v| v.as_array())
                    .map(|arr| {
                        arr.iter()
                            .filter_map(|v| v.as_str().map(String::from))
                            .collect()
                    })
                    .unwrap_or_default();

                // Convert to SendKeys format
                let sendkeys = keys
                    .iter()
                    .map(|k| match k.as_str() {
                        "ctrl" | "control" => "^".to_string(),
                        "alt" => "%".to_string(),
                        "shift" => "+".to_string(),
                        "enter" => "~".to_string(),
                        "tab" => "{TAB}".to_string(),
                        "escape" => "{ESC}".to_string(),
                        "up" => "{UP}".to_string(),
                        "down" => "{DOWN}".to_string(),
                        "left" => "{LEFT}".to_string(),
                        "right" => "{RIGHT}".to_string(),
                        "home" => "{HOME}".to_string(),
                        "end" => "{END}".to_string(),
                        "pageup" => "{PGUP}".to_string(),
                        "pagedown" => "{PGDN}".to_string(),
                        k => k.to_string(),
                    })
                    .collect::<String>();

                let script = format!(
                    r#"
Add-Type -AssemblyName System.Windows.Forms
[System.Windows.Forms.SendKeys]::SendWait('{}')
"#,
                    sendkeys.replace("'", "''")
                );
                let (ok, _, err) = Self::run_ps(&script).await?;
                if ok {
                    Ok(ToolExecutionResult::success(format!("Pressed: {:?}", keys)))
                } else {
                    Ok(ToolExecutionResult::error(format!("Key press failed: {}", err)))
                }
            }
            "activate_window" => {
                let name = args.get("name").and_then(|v| v.as_str());
                if let Some(n) = name {
                    let script = format!(
                        r#"
$proc = Get-Process | Where-Object {{ $_.MainWindowTitle -like '*{name}*' }} | Select-Object -First 1
if ($proc -ne $null) {{
    Add-Type @"
using System; using System.Runtime.InteropServices;
public class WinAPI {{
    [DllImport("user32.dll")] public static extern bool SetForegroundWindow(IntPtr hWnd);
    [DllImport("user32.dll")] public static extern bool ShowWindowAsync(IntPtr hWnd, int nCmdShow);
}}
"@
    [WinAPI]::ShowWindowAsync($proc.MainWindowHandle, 1) | Out-Null
    [WinAPI]::SetForegroundWindow($proc.MainWindowHandle) | Out-Null
    "Activated: $($proc.MainWindowTitle)"
}} else {{
    Write-Error "Window not found"
}}
"#,
                        name = n.replace("'", "''")
                    );
                    let (ok, stdout, err) = Self::run_ps(&script).await?;
                    if ok {
                        Ok(ToolExecutionResult::success(stdout))
                    } else {
                        Ok(ToolExecutionResult::error(format!("Activate failed: {}", err)))
                    }
                } else {
                    Ok(ToolExecutionResult::error(
                        "Provide 'name' to activate a window".to_string(),
                    ))
                }
            }
            "close_window" => {
                let name = args.get("name").and_then(|v| v.as_str());
                if let Some(n) = name {
                    let script = format!(
                        r#"
$proc = Get-Process | Where-Object {{ $_.MainWindowTitle -like '*{name}*' }} | Select-Object -First 1
if ($proc -ne $null) {{
    $proc.CloseMainWindow() | Out-Null
    Start-Sleep -Milliseconds 500
    if (!$proc.HasExited) {{ Stop-Process -Id $proc.Id -Force }}
    "Closed: $($proc.MainWindowTitle)"
}} else {{
    Write-Error "Window not found"
}}
"#,
                        name = n.replace("'", "''")
                    );
                    let (ok, stdout, err) = Self::run_ps(&script).await?;
                    if ok {
                        Ok(ToolExecutionResult::success(stdout))
                    } else {
                        Ok(ToolExecutionResult::error(format!("Close failed: {}", err)))
                    }
                } else {
                    Ok(ToolExecutionResult::error("Provide 'name' to close a window".to_string()))
                }
            }
            // ── Window management ──────────────────────────────────────────
            "list_windows" => {
                // Enhanced with bounds and window state via P/Invoke
                let script = r#"
Add-Type @"
using System; using System.Runtime.InteropServices;
using System.Text;
public class WinAPI {
    [DllImport("user32.dll")] public static extern bool EnumWindows(EnumWindowsProc lpEnumFunc, IntPtr lParam);
    [DllImport("user32.dll")] public static extern int GetWindowText(IntPtr hWnd, StringBuilder lpString, int nMaxCount);
    [DllImport("user32.dll")] public static extern uint GetWindowThreadProcessId(IntPtr hWnd, out uint lpdwProcessId);
    [DllImport("user32.dll")] public static extern bool GetWindowRect(IntPtr hWnd, out RECT lpRect);
    [DllImport("user32.dll")] public static extern bool IsIconic(IntPtr hWnd);
    [DllImport("user32.dll")] public static extern bool IsZoomed(IntPtr hWnd);
    [DllImport("user32.dll")] public static extern IntPtr GetForegroundWindow();
    public delegate bool EnumWindowsProc(IntPtr hWnd, IntPtr lParam);
    public struct RECT { public int Left; public int Top; public int Right; public int Bottom; }
    public static string EnumAll() {
        System.Text.StringBuilder sb = new System.Text.StringBuilder();
        IntPtr fg = GetForegroundWindow();
        EnumWindows((hWnd, _) => {
            System.Text.StringBuilder title = new System.Text.StringBuilder(256);
            GetWindowText(hWnd, title, 256);
            if (title.Length > 0) {
                GetWindowThreadProcessId(hWnd, out uint pid);
                RECT r; GetWindowRect(hWnd, out r);
                sb.Append(hWnd.ToInt64().ToString() + "|||");
                sb.Append(title.ToString().Replace("|","") + "|||");
                sb.Append(pid.ToString() + "|||");
                sb.Append(r.Left + "," + r.Top + "," + (r.Right - r.Left) + "," + (r.Bottom - r.Top) + "|||");
                sb.Append((hWnd == fg ? "1" : "0") + "|||");
                sb.Append(IsIconic(hWnd) ? "1" : "0" + "|||");
                sb.Append(IsZoomed(hWnd) ? "1" : "0");
                sb.AppendLine();
            }
            return true;
        }, IntPtr.Zero);
        return sb.ToString().Trim();
    }
}
"@
[WinAPI]::EnumAll()
"#;
                let (ok, stdout, err) = Self::run_ps(script).await?;
                if ok {
                    Ok(ToolExecutionResult::success(format!("Windows:\n{}", stdout)))
                } else {
                    Ok(ToolExecutionResult::error(format!("List windows failed: {}", err)))
                }
            }
            "get_window_geometry" => {
                let name = args.get("name").and_then(|v| v.as_str());
                if let Some(n) = name {
                    let script = format!(
                        r#"
$proc = Get-Process | Where-Object {{ $_.MainWindowTitle -like '*{name}*' }} | Select-Object -First 1
if ($proc -ne $null) {{
    Add-Type @"
using System; using System.Runtime.InteropServices;
public class WinAPI {{
    [DllImport("user32.dll")] public static extern bool GetWindowRect(IntPtr hWnd, out RECT lpRect);
    public struct RECT {{ public int Left; public int Top; public int Right; public int Bottom; }}
}}
"@
    $r = New-Object WinAPI+RECT
    [WinAPI]::GetWindowRect($proc.MainWindowHandle, [ref]$r) | Out-Null
    "$($r.Left),$($r.Top),$($r.Right - $r.Left),$($r.Bottom - $r.Top)"
}} else {{
    Write-Error "Window not found"
}}
"#,
                        name = n.replace("'", "''")
                    );
                    let (ok, stdout, err) = Self::run_ps(&script).await?;
                    if ok {
                        Ok(ToolExecutionResult::success(stdout))
                    } else {
                        Ok(ToolExecutionResult::error(format!("Get geometry failed: {}", err)))
                    }
                } else {
                    Ok(ToolExecutionResult::error(
                        "Provide 'name' for get_window_geometry".to_string(),
                    ))
                }
            }
            "move_window" => {
                let name = args.get("name").and_then(|v| v.as_str());
                let x = args.get("x").and_then(|v| v.as_i64());
                let y = args.get("y").and_then(|v| v.as_i64());
                if let (Some(n), Some(xv), Some(yv)) = (name, x, y) {
                    let script = format!(
                        r#"
Add-Type @"
using System; using System.Runtime.InteropServices;
public class WinAPI {{
    [DllImport("user32.dll")] public static extern bool SetWindowPos(IntPtr hWnd, IntPtr hWndInsertAfter, int X, int Y, int cx, int cy, uint uFlags);
}}
"@
$proc = Get-Process | Where-Object {{ $_.MainWindowTitle -like '*{name}*' }} | Select-Object -First 1
if ($proc -ne $null) {{
    [WinAPI]::SetWindowPos($proc.MainWindowHandle, [IntPtr]::Zero, {x}, {y}, 0, 0, 0x0001) | Out-Null
    "Moved: $($proc.MainWindowTitle)"
}} else {{ Write-Error "Window not found" }}
"#,
                        name = n.replace("'", "''"),
                        x = xv,
                        y = yv
                    );
                    let (ok, stdout, err) = Self::run_ps(&script).await?;
                    if ok {
                        Ok(ToolExecutionResult::success(stdout))
                    } else {
                        Ok(ToolExecutionResult::error(format!("Move window failed: {}", err)))
                    }
                } else {
                    Ok(ToolExecutionResult::error(
                        "Provide 'name', 'x', 'y' for move_window".to_string(),
                    ))
                }
            }
            "resize_window" => {
                let name = args.get("name").and_then(|v| v.as_str());
                let width = args.get("width").and_then(|v| v.as_u64());
                let height = args.get("height").and_then(|v| v.as_u64());
                if let (Some(n), Some(w), Some(h)) = (name, width, height) {
                    let script = format!(
                        r#"
Add-Type @"
using System; using System.Runtime.InteropServices;
public class WinAPI {{
    [DllImport("user32.dll")] public static extern bool SetWindowPos(IntPtr hWnd, IntPtr hWndInsertAfter, int X, int Y, int cx, int cy, uint uFlags);
}}
"@
$proc = Get-Process | Where-Object {{ $_.MainWindowTitle -like '*{name}*' }} | Select-Object -First 1
if ($proc -ne $null) {{
    [WinAPI]::SetWindowPos($proc.MainWindowHandle, [IntPtr]::Zero, 0, 0, {w}, {h}, 0x0002) | Out-Null
    "Resized: $($proc.MainWindowTitle)"
}} else {{ Write-Error "Window not found" }}
"#,
                        name = n.replace("'", "''"),
                        w = w,
                        h = h
                    );
                    let (ok, stdout, err) = Self::run_ps(&script).await?;
                    if ok {
                        Ok(ToolExecutionResult::success(stdout))
                    } else {
                        Ok(ToolExecutionResult::error(format!("Resize window failed: {}", err)))
                    }
                } else {
                    Ok(ToolExecutionResult::error(
                        "Provide 'name', 'width', 'height' for resize_window".to_string(),
                    ))
                }
            }
            "minimize_window" => {
                let name = args.get("name").and_then(|v| v.as_str());
                if let Some(n) = name {
                    let script = format!(
                        r#"
Add-Type @"
using System; using System.Runtime.InteropServices;
public class WinAPI {{
    [DllImport("user32.dll")] public static extern bool ShowWindowAsync(IntPtr hWnd, int nCmdShow);
}}
"@
$proc = Get-Process | Where-Object {{ $_.MainWindowTitle -like '*{name}*' }} | Select-Object -First 1
if ($proc -ne $null) {{
    [WinAPI]::ShowWindowAsync($proc.MainWindowHandle, 6) | Out-Null
    "Minimized: $($proc.MainWindowTitle)"
}} else {{ Write-Error "Window not found" }}
"#,
                        name = n.replace("'", "''")
                    );
                    let (ok, stdout, err) = Self::run_ps(&script).await?;
                    if ok {
                        Ok(ToolExecutionResult::success(stdout))
                    } else {
                        Ok(ToolExecutionResult::error(format!("Minimize failed: {}", err)))
                    }
                } else {
                    Ok(ToolExecutionResult::error("Provide 'name' for minimize_window".to_string()))
                }
            }
            "maximize_window" => {
                let name = args.get("name").and_then(|v| v.as_str());
                if let Some(n) = name {
                    let script = format!(
                        r#"
Add-Type @"
using System; using System.Runtime.InteropServices;
public class WinAPI {{
    [DllImport("user32.dll")] public static extern bool ShowWindowAsync(IntPtr hWnd, int nCmdShow);
}}
"@
$proc = Get-Process | Where-Object {{ $_.MainWindowTitle -like '*{name}*' }} | Select-Object -First 1
if ($proc -ne $null) {{
    [WinAPI]::ShowWindowAsync($proc.MainWindowHandle, 3) | Out-Null
    "Maximized: $($proc.MainWindowTitle)"
}} else {{ Write-Error "Window not found" }}
"#,
                        name = n.replace("'", "''")
                    );
                    let (ok, stdout, err) = Self::run_ps(&script).await?;
                    if ok {
                        Ok(ToolExecutionResult::success(stdout))
                    } else {
                        Ok(ToolExecutionResult::error(format!("Maximize failed: {}", err)))
                    }
                } else {
                    Ok(ToolExecutionResult::error("Provide 'name' for maximize_window".to_string()))
                }
            }
            _ => Ok(ToolExecutionResult::error(format!("Unknown action: {}", action))),
        }
    }

    fn is_available(&self, _context: &ToolContext) -> bool {
        cfg!(target_os = "windows")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_desktop_control_tool_creation() {
        let tool = DesktopControlTool::new();
        assert_eq!(tool.name(), "windows_desktop_control");
    }
}
