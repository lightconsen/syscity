//! One UI dump, shared by every tool that reacts to the current screen.
use async_trait::async_trait;
use serde_json::Value;

// The set, the device targeting, and the siblings' items: this file used
// to be part of `android`, and names them exactly as it did there.
use super::*;

/// Dump and parse the device's current UI tree.
///
/// Shared by `android_ui_tree` and the safety net in `android_input` so both see
/// the same enumeration.
/// A parsed `uiautomator dump`: the actionable elements plus the screen
/// rectangle the dump describes.
pub struct UiDump {
    pub elements: Vec<UiElement>,
    /// Full-display size as the dump reports it (`None` if it had no root bounds).
    pub screen: Option<(i32, i32)>,
    /// Where the tree came from: `accessibility` (in-process) or `uiautomator`
    /// (ADB). Reported so a surprising read can be traced to its source.
    pub source: &'static str,
    /// The app that owns the focused window, when the platform said.
    ///
    /// It is what the outward pacing is bucketed by: two sends to the same app
    /// belong to the same history whether or not the screen changed.
    pub package: Option<String>,
    /// The name of *the screen this read saw*, quoted back by a later call.
    ///
    /// It is the tree's fingerprint in hex, so two reads of an unchanged screen
    /// carry the same name — which is what makes it usable as the `since`
    /// baseline of an incremental read ([`super::diff`]). It is not a version
    /// of the app, and it is not what an *index* refers to: indices belong to
    /// one enumeration, so a caller holding them is working from this read
    /// whether or not the screen has changed since. An element *key* is what
    /// survives the screen changing.
    pub snapshot: String,
    /// The source capped the node list, so this tree is a prefix of the screen.
    ///
    /// Only the in-process accessibility walk caps (`A11yBridge.MAX_NODES`);
    /// `uiautomator dump` writes whatever it saw. A caller looking for an
    /// element in a truncated tree has not seen the whole screen, and a caller
    /// matching more than the cap cannot count the matches.
    pub truncated: bool,
}

/// Name a reading of the tree.
///
/// The name *is* the fingerprint, in hex: two reads with the same elements get
/// the same name, which is what makes a name usable as the baseline for
/// [`super::diff`] — a later read can ask "what changed since this screen".
/// A read counter was the obvious alternative and is wrong: it would name the
/// same screen differently every time, so a caller diffing against it would
/// always be comparing with a baseline that cannot be found.
///
/// It is still not a version of the app, and still not what an *index* refers
/// to: indices belong to one enumeration. `key` is what survives a change.
pub(super) fn snapshot_name(fingerprint: u64) -> String {
    format!("{fingerprint:x}")
}

pub(super) async fn dump_ui_elements(
    device: &Option<String>,
    target: Target,
) -> Result<Vec<UiElement>, DumpFailure> {
    Ok(dump_ui(device, target).await?.elements)
}

/// Dump, pull and parse the tree in one go.
///
/// The in-process accessibility tree is preferred whenever it is available: one
/// bridge call instead of a subprocess plus an `/sdcard` round trip, and the
/// only source that can see a display other than the default one. It exists
/// only on the phone itself and only while Syscity's accessibility service is
/// on, so a desktop driving a remote device — and any phone with the service
/// off — falls through to ADB exactly as before.
pub(super) async fn dump_ui(
    device: &Option<String>,
    target: Target,
) -> Result<UiDump, DumpFailure> {
    if let Target::Display(id) = target {
        // A virtual display is reachable only through the accessibility
        // service, which is the only reader that can be pointed at it. There is
        // no `uiautomator` fallback: that dumps the default display, which
        // would silently describe the wrong screen.
        let bridge = display_bridge()
            .map_err(|message| DumpFailure::new(DISPLAY_TREE_UNAVAILABLE, message))?;
        return match super::a11y::fetch(&bridge, Some(id)).await {
            Ok(snapshot) => {
                let fingerprint = tree_fingerprint(&snapshot.elements);
                crate::computer::platform::android::diff::remember(
                    device.as_deref(),
                    target.display_id(),
                    fingerprint,
                    snapshot.package.as_deref(),
                    &snapshot.elements,
                );
                Ok(UiDump {
                    elements: snapshot.elements,
                    screen: snapshot.display,
                    source: "accessibility",
                    package: snapshot.package,
                    snapshot: snapshot_name(fingerprint),
                    truncated: snapshot.truncated,
                })
            }
            Err(unavailable) => Err(DumpFailure::new(
                DISPLAY_TREE_UNAVAILABLE,
                format!(
                    "Could not read display {id}: {}. Work from the frame instead —                      android_display action 'frame' returns one.",
                    unavailable.message
                ),
            )),
        };
    }

    if device.is_none() {
        if let Some(bridge) = crate::device::bridge() {
            match super::a11y::fetch(bridge, None).await {
                Ok(snapshot) if snapshot.is_addressable() => {
                    let fingerprint = tree_fingerprint(&snapshot.elements);
                    crate::computer::platform::android::diff::remember(
                        device.as_deref(),
                        target.display_id(),
                        fingerprint,
                        snapshot.package.as_deref(),
                        &snapshot.elements,
                    );
                    return Ok(UiDump {
                        elements: snapshot.elements,
                        screen: snapshot.display,
                        source: "accessibility",
                        package: snapshot.package,
                        snapshot: snapshot_name(fingerprint),
                        truncated: snapshot.truncated,
                    });
                }
                Ok(_) => {
                    tracing::debug!(
                        "accessibility tree had no addressable nodes; falling back to uiautomator"
                    );
                }
                Err(unavailable) => {
                    tracing::debug!(
                        code = unavailable.code,
                        "accessibility tree unavailable: {}",
                        unavailable.message
                    );
                }
            }
        }
    }

    let xml = pull_ui_xml(device).await?;
    let elements = parse_uiautomator_xml(&xml).map_err(|source| {
        DumpFailure::new(
            DUMP_FAILED,
            format!("Could not read the UI tree: the dump did not parse ({source})"),
        )
    })?;
    let fingerprint = tree_fingerprint(&elements);
    let package = parse_package(&xml);
    crate::computer::platform::android::diff::remember(
        device.as_deref(),
        target.display_id(),
        fingerprint,
        package.as_deref(),
        &elements,
    );
    Ok(UiDump {
        elements,
        screen: parse_screen_bounds(&xml),
        source: "uiautomator",
        package,
        snapshot: snapshot_name(fingerprint),
        // The dump writes what the window held; it has no cap of its own.
        truncated: false,
    })
}

/// Run `uiautomator dump` on the device and pull the file back.
pub(super) async fn pull_ui_xml(device: &Option<String>) -> Result<String, DumpFailure> {
    let mut prefix: Vec<String> = Vec::new();
    if let Some(serial) = device {
        prefix.push("-s".to_string());
        prefix.push(serial.clone());
    }
    let argv = |base: &[&str]| -> Vec<String> {
        let mut v = prefix.clone();
        v.extend(base.iter().map(|s| s.to_string()));
        v
    };
    let run = |args: Vec<String>| {
        let args: Vec<String> = args;
        async move { run_cmd("adb", &args.iter().map(|s| s.as_str()).collect::<Vec<_>>()).await }
    };

    // `uiautomator dump` writes to a device file, which then has to be pulled.
    let dump = argv(&["shell", "uiautomator", "dump", "/sdcard/window_dump.xml"]);
    let (status, _, stderr) = run(dump).await.map_err(|e| {
        DumpFailure::new(DUMP_FAILED, format!("Could not run adb to read the UI tree: {e}"))
    })?;
    if !status.success() {
        return Err(DumpFailure::classify(&stderr));
    }

    let pull = argv(&["pull", "/sdcard/window_dump.xml", "-"]);
    let (status, stdout, stderr) = run(pull).await.map_err(|e| {
        DumpFailure::new(DUMP_FAILED, format!("Could not read the UI tree: adb pull failed ({e})"))
    })?;
    if !status.success() {
        // A failed pull reports its own reason on stderr ("no such file", a
        // device that went away between the two commands), so it goes through
        // the same classifier rather than a second vocabulary.
        return Err(DumpFailure::classify(&stderr));
    }

    Ok(stdout)
}

#[async_trait]
impl Tool for AdbUiTreeTool {
    fn name(&self) -> &str {
        "android_ui_tree"
    }

    fn description(&self) -> &str {
        "Read the Android device's UI: a numbered list of the elements you can act on, each with \
         its text or content description, whether it is clickable, and the screen coordinates of \
         its centre. Tap an element with android_input by passing that centre as x/y. Bare layout \
         containers are omitted. The result says how much of the screen the tree actually \
         describes (quality: usable / partial / unusable); when it is not usable this reads the \
         screenshot with OCR instead and returns those lines, so do not re-read hoping for more — \
         act on the OCR text or the coordinates it gives. Pass `since` with the snapshot of an \
         earlier read to get only what changed — the rows that appeared, left or moved — instead \
         of the whole screen."
    }

    fn parameters_schema(&self) -> Value {
        create_schema(
            "Get Android UI tree",
            serde_json::json!({
                "since": { "type": "string", "description": SINCE_PARAM },
                "display": { "type": "integer", "description": DISPLAY_PARAM },
                "device": { "type": "string", "description": DEVICE_PARAM }
            }),
            Vec::<String>::new(),
        )
    }

    async fn execute(
        &self,
        args: Value,
        context: &ToolContext,
    ) -> crate::Result<ToolExecutionResult> {
        let device = match resolve_device(&args, self.device.as_deref()) {
            Ok(device) => device,
            Err(refusal) => return Ok(ToolExecutionResult::error(refusal)),
        };
        // The raw dump is an order of magnitude larger than what the model can
        // use, and the model would have to parse `bounds` itself to act on it.
        // Hand back an indexed list instead; the XML stays out of the context.
        let target = match Target::from_args(&args) {
            Ok(target) => target,
            Err(message) => return Ok(ToolExecutionResult::error(message)),
        };
        // Reading the same unchanged screen over and over is the read-side
        // loop, and it is refused before the ADB round trip rather than after:
        // the answer would be the one the model already has.
        let guard_key = guard_key_for(context, device.as_deref(), target);
        if let Some(reason) = super::guard::Guard::global()
            .check_read(&guard_key, "android_ui_tree")
            .refusal()
        {
            return Ok(ToolExecutionResult::error(reason.to_string()));
        }
        let since = super::diff::since_arg(&args).map(str::to_string);
        let dump = match dump_ui(&device, target).await {
            Ok(dump) => dump,
            Err(failure) => {
                return Ok(ToolExecutionResult::error(failure.message)
                    .with_data(serde_json::json!({ "code": failure.code })))
            }
        };
        let note = read_note(&guard_key, "android_ui_tree", &dump.elements);
        if let Some(answer) = since_answer(device.as_deref(), target, since.as_deref(), &dump) {
            let (text, data) = match answer {
                SinceAnswer::Delta { text, data } => (text, data),
                SinceAnswer::Full(reason) => {
                    (format!("{} {}", reason.describe(), format_ui_summary(&dump.elements)), {
                        let mut data = since_fallback(reason);
                        data["elements"] = serde_json::json!(dump.elements);
                        data["snapshot"] = serde_json::json!(dump.snapshot);
                        data
                    })
                }
            };
            return Ok(ToolExecutionResult::success(with_note(text, note)).with_data(data));
        }
        let elements = dump.elements;
        let quality = assess_tree(&elements);
        let mut summary = format_ui_summary(&elements);
        let mut ocr_text = None;
        // When the tree describes little or nothing, reading the screenshot is
        // the only way to say what is actually on the screen — and saying it
        // here is what stops the model from re-reading the tree forever.
        if quality != TreeQuality::Usable {
            match ocr_screen(&device, target).await {
                Ok(lines) => {
                    summary.push_str("\n\n");
                    summary.push_str(&format_ocr_lines(&lines));
                    ocr_text = Some(lines);
                }
                Err(why) => {
                    if let Some(advice) = quality.advice() {
                        summary.push_str(&format!("\n\n{advice} (OCR unavailable: {why})"));
                    }
                }
            }
        }
        Ok(
            ToolExecutionResult::success(with_note(summary, note)).with_data(serde_json::json!({
                "count": elements.len(),
                "quality": quality.as_str(),
                "advice": quality.advice(),
                "ocr": ocr_text,
                "source": dump.source,
                "snapshot": dump.snapshot,
                "elements": elements,
                "device": resolved_serial(&device).await,
            })),
        )
    }
}
