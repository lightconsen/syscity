//! Alarms, timers, calendar events, shares and settings, through intents.

use super::*;

/// Alarm, timer, calendar, share and settings, through Android's own intents.
#[derive(Debug)]
pub struct AdbSystemActionTool {
    device: Option<String>,
}

impl Default for AdbSystemActionTool {
    fn default() -> Self {
        Self::new()
    }
}

impl AdbSystemActionTool {
    pub fn new() -> Self {
        Self { device: None }
    }

    pub fn with_device(mut self, device: String) -> Self {
        self.device = Some(device);
        self
    }
}

/// The settings screens this tool will open, mapped to their intent actions.
///
/// An explicit list rather than whatever the model names: an arbitrary
/// `android.settings.*` string is a way to open screens the task has no business
/// opening, and a typo silently does nothing.
const SETTINGS_SCREENS: &[(&str, &str)] = &[
    ("wifi", "android.settings.WIFI_SETTINGS"),
    ("bluetooth", "android.settings.BLUETOOTH_SETTINGS"),
    ("airplane", "android.settings.AIRPLANE_MODE_SETTINGS"),
    ("data_usage", "android.settings.DATA_USAGE_SETTINGS"),
    ("battery", "android.settings.BATTERY_SAVER_SETTINGS"),
    ("display", "android.settings.DISPLAY_SETTINGS"),
    ("sound", "android.settings.SOUND_SETTINGS"),
    ("apps", "android.settings.APPLICATION_SETTINGS"),
    ("notifications", "android.settings.NOTIFICATION_SETTINGS"),
    ("storage", "android.settings.INTERNAL_STORAGE_SETTINGS"),
    ("location", "android.settings.LOCATION_SOURCE_SETTINGS"),
    ("developer", "android.settings.APPLICATION_DEVELOPMENT_SETTINGS"),
    ("accessibility", "android.settings.ACCESSIBILITY_SETTINGS"),
    ("date", "android.settings.DATE_SETTINGS"),
    ("language", "android.settings.LOCALE_SETTINGS"),
];

/// Build the `am start` argument list for one system action.
///
/// Split out from the tool so the intent construction is testable without a
/// device: the extras' names are Android's own constants and a typo in one is a
/// silent no-op on the phone.
/// Append `parts` to an argument list.
fn extend(out: &mut Vec<String>, parts: &[&str]) {
    out.extend(parts.iter().map(|s| s.to_string()));
}

fn system_action_args(action: &str, args: &Value) -> Result<Vec<String>, String> {
    let text = |key: &str| args.get(key).and_then(|v| v.as_str()).map(str::to_string);
    let number = |key: &str| args.get(key).and_then(|v| v.as_i64());
    let flag =
        |key: &str, default: bool| args.get(key).and_then(|v| v.as_bool()).unwrap_or(default);

    let mut out: Vec<String> = Vec::new();

    match action {
        "set_alarm" => {
            let hour = number("hour").ok_or("set_alarm needs 'hour' (0-23)")?;
            let minute = number("minute").ok_or("set_alarm needs 'minute' (0-59)")?;
            if !(0..=23).contains(&hour) || !(0..=59).contains(&minute) {
                return Err(format!("hour {hour} / minute {minute} is not a time of day"));
            }
            extend(
                &mut out,
                &[
                    "-a",
                    "android.intent.action.SET_ALARM",
                    "--ei",
                    "android.intent.extra.alarm.HOUR",
                    &hour.to_string(),
                    "--ei",
                    "android.intent.extra.alarm.MINUTES",
                    &minute.to_string(),
                ],
            );
            if let Some(label) = text("label") {
                out.push("--es".into());
                out.push("android.intent.extra.alarm.MESSAGE".into());
                out.push(sh_quote(&label));
            }
            if let Some(days) = args.get("days").and_then(|v| v.as_array()) {
                let joined = days
                    .iter()
                    .filter_map(|d| d.as_i64())
                    .filter(|d| (1..=7).contains(d))
                    .map(|d| d.to_string())
                    .collect::<Vec<_>>()
                    .join(",");
                if !joined.is_empty() {
                    out.push("--esa".into());
                    out.push("android.intent.extra.alarm.DAYS".into());
                    out.push(sh_quote(&joined));
                }
            }
            out.push("--ez".into());
            out.push("android.intent.extra.alarm.SKIP_UI".into());
            out.push(flag("skip_ui", true).to_string());
        }
        "set_timer" => {
            let seconds = number("seconds").ok_or("set_timer needs 'seconds'")?;
            if seconds <= 0 {
                return Err("'seconds' must be positive".to_string());
            }
            extend(
                &mut out,
                &[
                    "-a",
                    "android.intent.action.SET_TIMER",
                    "--ei",
                    "android.intent.extra.alarm.LENGTH",
                    &seconds.to_string(),
                ],
            );
            if let Some(label) = text("label") {
                out.push("--es".into());
                out.push("android.intent.extra.alarm.MESSAGE".into());
                out.push(sh_quote(&label));
            }
            out.push("--ez".into());
            out.push("android.intent.extra.alarm.SKIP_UI".into());
            out.push(flag("skip_ui", true).to_string());
        }
        "show_alarms" => extend(&mut out, &["-a", "android.intent.action.SHOW_ALARMS"]),
        "insert_calendar" => {
            let title = text("title").ok_or("insert_calendar needs 'title'")?;
            let begin = number("begin_ms").ok_or("insert_calendar needs 'begin_ms' (epoch ms)")?;
            let end = number("end_ms").ok_or("insert_calendar needs 'end_ms' (epoch ms)")?;
            if end <= begin {
                return Err("'end_ms' must be after 'begin_ms'".to_string());
            }
            extend(
                &mut out,
                &[
                    "-a",
                    "android.intent.action.INSERT",
                    "-t",
                    "vnd.android.cursor.dir/event",
                    "--es",
                    "title",
                    &sh_quote(&title),
                    "--el",
                    "beginTime",
                    &begin.to_string(),
                    "--el",
                    "endTime",
                    &end.to_string(),
                ],
            );
            out.push("--ez".into());
            out.push("allDay".into());
            out.push(flag("all_day", false).to_string());
            for (key, extra) in [
                ("location", "eventLocation"),
                ("description", "description"),
            ] {
                if let Some(value) = text(key) {
                    out.push("--es".into());
                    out.push(extra.into());
                    out.push(sh_quote(&value));
                }
            }
        }
        "share_text" => {
            let body = text("text").ok_or("share_text needs 'text'")?;
            extend(
                &mut out,
                &[
                    "-a",
                    "android.intent.action.SEND",
                    "-t",
                    "text/plain",
                    "--es",
                    "android.intent.extra.TEXT",
                    &sh_quote(&body),
                ],
            );
            if let Some(package) = text("package") {
                out.push("-p".into());
                out.push(sh_quote(&package));
            }
            // Opening the share sheet is the point; the send itself stays a
            // model decision on the screen that appears.
            out.push("--ez".into());
            out.push("android.intent.extra.NO_PREVIEW".into());
            out.push("false".into());
        }
        "open_settings" => {
            let which = text("which").ok_or("open_settings needs 'which'")?;
            let action = SETTINGS_SCREENS
                .iter()
                .find(|(name, _)| *name == which.to_ascii_lowercase())
                .map(|(_, action)| *action)
                .ok_or_else(|| {
                    let known: Vec<&str> = SETTINGS_SCREENS.iter().map(|(name, _)| *name).collect();
                    format!("unknown settings screen {which:?}; known: {}", known.join(", "))
                })?;
            extend(&mut out, &["-a", action]);
            if let Some(package) = text("package") {
                out.push("--es".into());
                out.push("android.provider.extra.APP_PACKAGE".into());
                out.push(sh_quote(&package));
            }
        }
        "open_url" => {
            let url = text("url").ok_or("open_url needs 'url'")?;
            if !(url.starts_with("http://") || url.starts_with("https://")) {
                return Err(format!(
                    "open_url takes an http(s) URL; {url:?} would be a deep link into another app"
                ));
            }
            extend(
                &mut out,
                &[
                    "-a",
                    "android.intent.action.VIEW",
                    "-d",
                    &sh_quote(&url),
                    "-f",
                    "0x10000000",
                ],
            );
        }
        other => {
            return Err(format!(
                "unknown action {other:?}; use set_alarm, set_timer, show_alarms, \
                 insert_calendar, share_text, open_settings or open_url"
            ))
        }
    }
    Ok(out)
}

#[async_trait]
impl Tool for AdbSystemActionTool {
    fn name(&self) -> &str {
        "android_system_action"
    }

    fn description(&self) -> &str {
        "Do a phone-wide action through Android's own intents instead of tapping through its UI: \
         set an alarm, set a timer, add a calendar event, share text into an app, open a settings \
         screen, or open a web link. These are one stable call where the UI differs between \
         vendors and versions; preferring them over tapping is the difference between working on \
         one phone and working on any phone."
    }

    fn parameters_schema(&self) -> Value {
        create_schema(
            "Run an Android system action",
            serde_json::json!({
                "action": {
                    "type": "string",
                    "enum": [
                        "set_alarm", "set_timer", "show_alarms",
                        "insert_calendar", "share_text", "open_settings", "open_url"
                    ],
                    "description": "Which intent to send."
                },
                "hour": { "type": "integer", "description": "set_alarm: 0-23" },
                "minute": { "type": "integer", "description": "set_alarm: 0-59" },
                "days": {
                    "type": "array",
                    "items": { "type": "integer" },
                    "description": "set_alarm: repeat on these days, 1=Monday … 7=Sunday."
                },
                "label": { "type": "string", "description": "set_alarm / set_timer: the text shown with it." },
                "seconds": { "type": "integer", "description": "set_timer: duration." },
                "skip_ui": { "type": "boolean", "description": "set_alarm / set_timer: create it without opening the clock app (default true)." },
                "title": { "type": "string", "description": "insert_calendar: event title." },
                "begin_ms": { "type": "integer", "description": "insert_calendar: start, epoch milliseconds." },
                "end_ms": { "type": "integer", "description": "insert_calendar: end, epoch milliseconds." },
                "all_day": { "type": "boolean", "description": "insert_calendar: all-day event." },
                "location": { "type": "string", "description": "insert_calendar: location." },
                "description": { "type": "string", "description": "insert_calendar: notes." },
                "text": { "type": "string", "description": "share_text: the text to share." },
                "package": { "type": "string", "description": "share_text / open_settings: target app package." },
                "which": { "type": "string", "description": "open_settings: wifi, bluetooth, airplane, data_usage, battery, display, sound, apps, notifications, storage, location, developer, accessibility, date, language." },
                "url": { "type": "string", "description": "open_url: http(s) URL." },
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
        let device = match resolve_device(&args, self.device.as_deref()) {
            Ok(device) => device,
            Err(refusal) => return Ok(ToolExecutionResult::error(refusal)),
        };
        let _target = super::lock_device(&device).await;
        let action = args.get("action").and_then(|v| v.as_str()).unwrap_or("");
        let intent = match system_action_args(action, &args) {
            Ok(intent) => intent,
            Err(message) => return Ok(ToolExecutionResult::error(message)),
        };

        let mut argv: Vec<String> = Vec::new();
        if let Some(serial) = &device {
            argv.push("-s".into());
            argv.push(serial.clone());
        }
        argv.push("shell".into());
        argv.push("am".into());
        argv.push("start".into());
        argv.extend(intent);

        let rendered = std::iter::once("adb")
            .chain(argv.iter().map(String::as_str))
            .collect::<Vec<_>>()
            .join(" ");
        let (status, stdout, stderr) =
            run_cmd("adb", &argv.iter().map(|s| s.as_str()).collect::<Vec<_>>())
                .await
                .map_err(|e| crate::error::SyscityError::ExternalService {
                    source: "adb am start failed".to_string(),
                    cause: Some(Box::new(e)),
                })?;
        if !status.success() || stdout.contains("Error:") {
            return Ok(ToolExecutionResult::error(format!(
                "the system action was rejected by the phone: {}",
                if stderr.trim().is_empty() {
                    stdout.trim()
                } else {
                    stderr.trim()
                }
            ))
            .with_data(serde_json::json!({ "command": rendered })));
        }

        Ok(ToolExecutionResult::success(format!(
            "Sent {action}: the phone handled it through its own app (no UI was tapped).\n{rendered}"
        ))
        .with_data(serde_json::json!({
            "action": action,
            "command": rendered,
            "output": stdout.trim(),
            "device": resolved_serial(&device).await,
        })))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn alarm_intent_carries_the_time_and_skips_the_ui() {
        let args = serde_json::json!({ "action": "set_alarm", "hour": 7, "minute": 30,
                                       "label": "stand up" });
        let built = system_action_args("set_alarm", &args).expect("intent");
        let joined = built.join(" ");
        assert!(joined.contains("android.intent.action.SET_ALARM"), "{joined}");
        assert!(joined.contains("android.intent.extra.alarm.HOUR 7"), "{joined}");
        assert!(joined.contains("android.intent.extra.alarm.MINUTES 30"), "{joined}");
        assert!(joined.contains("'stand up'"), "{joined}");
        assert!(joined.contains("android.intent.extra.alarm.SKIP_UI true"), "{joined}");
    }
    #[test]
    fn alarm_rejects_an_impossible_time() {
        let args = serde_json::json!({ "hour": 25, "minute": 0 });
        assert!(system_action_args("set_alarm", &args).is_err());
    }
    #[test]
    fn calendar_rejects_an_end_before_the_start() {
        let args = serde_json::json!({ "title": "x", "begin_ms": 200, "end_ms": 100 });
        assert!(system_action_args("insert_calendar", &args).is_err());
    }
    #[test]
    fn share_text_quotes_the_body() {
        let args = serde_json::json!({ "text": "it's fine; rm -rf /", "package": "com.x" });
        let built = system_action_args("share_text", &args).expect("intent");
        let joined = built.join(" ");
        assert!(joined.contains("android.intent.action.SEND"), "{joined}");
        assert!(joined.contains("'it'\\''s fine; rm -rf /'"), "{joined}");
        assert!(joined.contains("-p 'com.x'"), "{joined}");
    }
    #[test]
    fn open_url_refuses_a_non_http_scheme() {
        let args = serde_json::json!({ "url": "content://secret" });
        assert!(system_action_args("open_url", &args).is_err());
    }
    #[test]
    fn open_settings_only_accepts_known_screens() {
        let ok = serde_json::json!({ "which": "wifi" });
        assert!(system_action_args("open_settings", &ok)
            .expect("wifi")
            .join(" ")
            .contains("android.settings.WIFI_SETTINGS"));
        let bad = serde_json::json!({ "which": "root_kitchen" });
        assert!(system_action_args("open_settings", &bad).is_err());
    }
    #[test]
    fn unknown_action_names_the_alternatives() {
        let err = system_action_args("reboot", &serde_json::json!({})).expect_err("rejected");
        assert!(err.contains("set_alarm"), "{err}");
    }
}
