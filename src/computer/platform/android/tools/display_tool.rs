//! Drive a second display so a task does not take the phone.

use super::*;

/// How long a launch is watched for the app to reach the top of the display.
///
/// `am start -W` returns when the activity reports idle, which for a cold start
/// of a heavy app is how long the *process* took, not how long the first frame
/// took to become the top task. Five seconds is Mika's number, and it is a
/// ceiling rather than a target: a display that already shows the app or a
/// chooser ends the wait on its first read.
const LAUNCH_SETTLE: Duration = Duration::from_secs(5);

/// Create and drive a second display, so the task does not take the phone.
#[derive(Debug)]
pub struct AdbDisplayTool {
    device: Option<String>,
}

impl Default for AdbDisplayTool {
    fn default() -> Self {
        Self::new()
    }
}

impl AdbDisplayTool {
    pub fn new() -> Self {
        Self { device: None }
    }

    /// Kill whatever helper is running on the device.
    ///
    /// Best effort: a device without `pkill` is not a reason to refuse.
    async fn kill_helper(&self, device: &Option<String>, class: &str) -> bool {
        let args = adb_args(device, &["shell", "pkill", "-f", class]);
        super::run_cmd("adb", &args.iter().map(|s| s.as_str()).collect::<Vec<_>>())
            .await
            .is_ok_and(|(status, _, _)| status.success())
    }
}

#[async_trait]
impl Tool for AdbDisplayTool {
    fn name(&self) -> &str {
        "android_display"
    }

    fn description(&self) -> &str {
        "Run a task on a second screen so the user keeps using the phone. `start` creates the \
         display and returns its id; every later call passes that id. Apps opened on it do not \
         appear on the user's screen, and on Android 14+ it holds its own focus, so typing there \
         does not steal the user's keyboard. `launch` ends the app first when nothing else is \
         holding it, starts it warm when another display still has a task of it, and refuses \
         when the person is using it on their own screen; the result's `cold_start` says which \
         happened and `opened` says what came to the top (the app, a chooser, or something else). \
         `frame` returns a PNG of that display. This needs the Syscity app running on the phone; \
         a desktop controlling a remote device has no second screen to offer. Coordinate \
         arguments also take a 0–999 form (`x_pct` / `y_pct` / `x2_pct` / `y2_pct`) measured \
         against this display; pixels still win when both are given."
    }

    fn parameters_schema(&self) -> Value {
        create_schema(
            "Create and drive a second Android display",
            serde_json::json!({
                "action": {
                    "type": "string",
                    "enum": [
                        "start", "restart", "stop", "status", "limits", "frame", "launch",
                        "tap", "swipe", "text", "paste", "key", "mute", "sound"
                    ],
                    "description": "start creates the display; limits says how many may be open; restart replaces a stale helper; the rest act on it. 'paste' carries non-ASCII text, which `text` cannot. 'mute' silences the app on this display and 'sound' gives it back; launching an app mutes it already. 'launch' may end the app first, start it warm, or refuse — the result says which."
                },
                "display": {
                    "type": "integer",
                    "description": "Display id returned by `start`. Required except for start/status."
                },
                "package": {
                    "type": "string",
                    "description": "start / launch: the app to open on the display; mute / sound: the app whose sound to change."
                },
                "width": { "type": "integer", "description": "start: display width (default 1080)." },
                "height": { "type": "integer", "description": "start: display height (default 2400)." },
                "x": { "type": "integer", "description": "tap / swipe start X, in pixels. A pixel wins over `x_pct`." },
                "y": { "type": "integer", "description": "tap / swipe start Y, in pixels." },
                "x2": { "type": "integer", "description": "swipe end X, in pixels." },
                "y2": { "type": "integer", "description": "swipe end Y, in pixels." },
                "x_pct": {
                    "type": "integer",
                    "minimum": 0,
                    "maximum": 999,
                    "description": "tap / swipe start X as 0–999 across this display (0 left, 999 right), when its size is not known to the caller. The size is read from the display itself."
                },
                "y_pct": {
                    "type": "integer",
                    "minimum": 0,
                    "maximum": 999,
                    "description": "tap / swipe start Y as 0–999 down this display."
                },
                "x2_pct": {
                    "type": "integer",
                    "minimum": 0,
                    "maximum": 999,
                    "description": "swipe end X as 0–999."
                },
                "y2_pct": {
                    "type": "integer",
                    "minimum": 0,
                    "maximum": 999,
                    "description": "swipe end Y as 0–999."
                },
                "duration": { "type": "integer", "description": "swipe duration in ms (default 300)." },
                "text": { "type": "string", "description": "text to type on the display." },
                "keycode": { "type": "string", "description": "key to press, e.g. BACK or ENTER." },
                "device": { "type": "string", "description": DEVICE_PARAM }
            }),
            vec!["action"],
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
        let action = args.get("action").and_then(|v| v.as_str()).unwrap_or("");
        // Not the device lock: a display is its own focus, so two displays on
        // one phone are two places to write and do not need to wait for each
        // other. Which display this call is about is often only known after
        // the action is parsed (`start` names none, `stop` names one), so the
        // lock for a named display is taken below, once the id is in hand.
        // Actions that are about the phone as a whole (`start`, `restart`,
        // `status`, `limits`) take the device.
        let named_display = args
            .get("display")
            .and_then(|v| v.as_i64())
            .and_then(|id| i32::try_from(id).ok())
            .filter(|_| action != "start" && action != "restart");
        let _target = match named_display {
            Some(id) => super::lock_target(&device, Target::Display(id)).await,
            None => super::lock_device(&device).await,
        };
        let bridge = match display_bridge() {
            Ok(bridge) => bridge,
            Err(message) => return Ok(ToolExecutionResult::error(message)),
        };

        if action == "status" {
            // The helper's own description, and the state that decides whether
            // a write still means anything: locked screen, which app each
            // display holds. Reported together because "it is running" is not
            // the same answer as "it is running somewhere useful".
            return Ok(match super::display::VirtualDisplay::status(&bridge).await {
                Ok(answer) => {
                    let mut text = format!("Display helper is running: {answer}");
                    if let Ok(state) = super::display::VirtualDisplay::readiness(&bridge).await {
                        text.push('\n');
                        text.push_str(&describe_readiness(&state));
                    }
                    ToolExecutionResult::success(text).with_data(answer)
                }
                Err(failed) => ToolExecutionResult::error(failed.to_string()),
            });
        }

        if action == "limits" {
            // Whether another display may be opened at all, and why not. A
            // caller that starts three on a phone that allows one gets two
            // failures and a half-driven screen; this answers first.
            return Ok(match super::display::VirtualDisplay::parallel_limit(&bridge).await {
                Ok(limits) => {
                    let mut text = format!(
                        "{} display(s) may be open at once; {} already are.",
                        limits.limit, limits.open
                    );
                    if let Some(reason) = &limits.reason {
                        text.push(' ');
                        text.push_str(reason);
                    }
                    ToolExecutionResult::success(text).with_data(serde_json::json!({
                        "limit": limits.limit,
                        "open": limits.open,
                        "remaining": limits.remaining(),
                        "reason": limits.reason,
                    }))
                }
                Err(failed) => ToolExecutionResult::error(failed.to_string()),
            });
        }

        if action == "restart" {
            // Kill whatever helper is running before starting a fresh one.
            //
            // A helper can outlive the app that started it (`app_process` is
            // detached on purpose) and then hold the socket name with a
            // process from an older build — reachable only if its identity
            // still matches. This clears that without needing shell on the
            // host.
            let killed = self
                .kill_helper(&device, super::display::HELPER_CLASS)
                .await;
            return Ok(
                match super::display::ensure_helper(
                    &device,
                    super::display::ANDROID_PACKAGE,
                    &bridge,
                )
                .await
                {
                    Ok(()) => ToolExecutionResult::success(format!(
                        "Display helper restarted{} and is answering.",
                        if killed {
                            ""
                        } else {
                            " (nothing was running to kill)"
                        }
                    )),
                    Err(failed) => ToolExecutionResult::error(failed.to_string()),
                },
            );
        }

        if action == "start" {
            // The helper is started on demand: nothing creates a shell process
            // on the phone until a task actually needs a display.
            if let Err(failed) =
                super::display::ensure_helper(&device, super::display::ANDROID_PACKAGE, &bridge)
                    .await
            {
                return Ok(ToolExecutionResult::error(failed.to_string()));
            }
            let size = match (
                args.get("width").and_then(|v| v.as_u64()),
                args.get("height").and_then(|v| v.as_u64()),
            ) {
                (Some(width), Some(height)) => {
                    Some((u32::try_from(width).unwrap_or(0), u32::try_from(height).unwrap_or(0)))
                }
                _ => None,
            };
            let package = args.get("package").and_then(|v| v.as_str());
            return Ok(match super::display::VirtualDisplay::open(bridge, size, package).await {
                Ok(display) => {
                    let text = format!(
                        "Second display ready: id {}, {}x{}. Open an app with action 'launch', \
                         read it with 'frame'. The user's screen is untouched.",
                        display.id, display.width, display.height
                    );
                    ToolExecutionResult::success(text).with_data(serde_json::json!({
                        "display": display.id,
                        "width": display.width,
                        "height": display.height,
                    }))
                }
                Err(failed) => ToolExecutionResult::error(failed.to_string()),
            });
        }

        let Some(id) = args.get("display").and_then(|v| v.as_i64()) else {
            return Ok(ToolExecutionResult::error(
                "this action needs the `display` id that `start` returned",
            ));
        };
        let id = i32::try_from(id).unwrap_or(-1);
        // The display is addressed by id alone for every later call: the helper
        // owns the record, and reconstructing a handle here would need its size.
        let display = super::display::VirtualDisplay::attach(bridge.clone(), id);

        // A tap on this display can still end somewhere irreversible: the
        // display runs a copy of the app, and the app's send button is the
        // app's send button. The tree for the display is read here because
        // the coordinates alone say nothing.
        let mut before_package: Option<String> = None;
        let mut before_fingerprint: Option<u64> = None;
        // A 0–999 point is a fraction of *this* display, whose size the phone's
        // `wm size` does not describe — it describes the user's screen. The
        // size comes from the display's own tree, read once here and kept: a
        // tap needs that read anyway for the risk classification below.
        let needs_size = super::coords::uses_percent(&args);
        let early = if needs_size && matches!(action, "tap" | "swipe") {
            match super::dump_ui(&device, Target::Display(id)).await {
                Ok(dump) => Some(dump),
                Err(failed) => {
                    return Ok(ToolExecutionResult::error(format!(
                        "Refused: this write names a point in the 0–999 space, which needs \
                         display {id}'s size, and the display could not be read ({}). Nothing \
                         was done. Pass `x` / `y` in pixels instead.",
                        failed.message
                    )))
                }
            }
        } else {
            None
        };
        let display_size = early.as_ref().and_then(|dump| dump.screen);
        // Only `tap` and `swipe` take a point; every other action's `x` is not
        // a coordinate and must not be read as one.
        let (tap_point, swipe_end) = if matches!(action, "tap" | "swipe") {
            let tap = match super::coords::resolve(&args, super::coords::TAP, display_size) {
                Ok(point) => point,
                Err(refused) => return Ok(ToolExecutionResult::error(refused)),
            };
            let end = match super::coords::resolve(&args, super::coords::END, display_size) {
                Ok(point) => point,
                Err(refused) => return Ok(ToolExecutionResult::error(refused)),
            };
            (tap, end)
        } else {
            (super::coords::Point::default(), super::coords::Point::default())
        };
        let tap_pixels = tap_point.pixels();
        let clearance = match action {
            "tap" => {
                let (x, y) = tap_pixels;
                let read = match early {
                    Some(dump) => Ok(dump),
                    None => super::dump_ui(&device, Target::Display(id)).await,
                };
                let offer = match read {
                    Ok(dump) => {
                        before_package = dump.package;
                        before_fingerprint = Some(super::tree_fingerprint(&dump.elements));
                        super::risk::classify_point(&dump.elements, x, y)
                    }
                    // A read that failed is not a reason to refuse a tap the
                    // agent has already located. It does mean the judgement
                    // could not be made, which the reply carries no `risk` for.
                    Err(_) => None,
                };
                let subject = match &offer {
                    Some(offer) => format!(
                        "tap \"{}\" on display {id}",
                        offer.element.as_deref().unwrap_or("that")
                    ),
                    None => format!("tap on display {id}"),
                };
                super::gate::check(offer, context, &subject, before_package.as_deref()).await
            }
            // A key press on a display is the same act as on the phone: one
            // that activates the focused control presses it, and in a chat
            // field that is the send. The tree has to be read to judge it —
            // what has the focus is only on the screen — so this costs one
            // read, and only for the key action.
            "key" => {
                let keycode = super::normalize_keycode(described_keycode(&args));
                let dump = super::dump_ui(&device, Target::Display(id)).await;
                let (offer, subject) = match dump {
                    Ok(dump) => {
                        before_package = dump.package.clone();
                        before_fingerprint = Some(super::tree_fingerprint(&dump.elements));
                        let offer = super::risk::classify_key(&dump.elements, &keycode);
                        let subject = match &offer {
                            Some(offer) => match offer.element.as_deref() {
                                Some(control) => format!(
                                    "press {keycode} on \"{control}\" on display {id} — {}",
                                    offer.describe()
                                ),
                                None => format!(
                                    "press {keycode} on display {id} — {}",
                                    offer.describe()
                                ),
                            },
                            None => format!("press {keycode} on display {id}"),
                        };
                        (offer, subject)
                    }
                    // The read failed, so nothing here can be judged. Like a
                    // tap on an unreadable screen, the action proceeds and the
                    // reply carries no `risk` for it.
                    Err(_) => (None, format!("press {keycode} on display {id}")),
                };
                super::gate::check(offer, context, &subject, before_package.as_deref()).await
            }
            // Text on a display is typed, not pressed: `input text` cannot
            // carry a line break, so a newline in `text` is dropped rather than
            // sent, and a paste inserts. Neither commits anything — the enter
            // that does is its own `key` call — so there is no offer to make.
            "text" | "paste" => {
                let dump = super::dump_ui(&device, Target::Display(id)).await;
                if let Ok(dump) = dump {
                    before_package = dump.package.clone();
                    before_fingerprint = Some(super::tree_fingerprint(&dump.elements));
                }
                Ok(super::gate::Clearance {
                    offer: None,
                    asked: false,
                    granted: None,
                })
            }
            // `launch` starts an app, so the app it is about to open is the app
            // this write will be in — named by the argument, since there is no
            // screen to read it from yet.
            "launch" => {
                before_package = args
                    .get("package")
                    .and_then(|v| v.as_str())
                    .map(str::trim)
                    .filter(|p| !p.is_empty())
                    .map(str::to_string);
                Ok(super::gate::Clearance {
                    offer: None,
                    asked: false,
                    granted: None,
                })
            }
            _ => Ok(super::gate::Clearance {
                offer: None,
                asked: false,
                granted: None,
            }),
        };
        let clearance = match clearance {
            Ok(clearance) => clearance,
            Err(refusal) => return Ok(ToolExecutionResult::error(refusal.reason)),
        };

        // What was approved was a description, and the screen has had the whole
        // question's lifetime to move. One re-read, only when somebody was
        // actually asked: a clock has closed and a message has arrived behind
        // it, and "tap 发送" means nothing unless the 发送 that is there now is
        // the one that was described.
        if clearance.asked {
            let fresh = match super::dump_ui(&device, Target::Display(id)).await {
                Ok(fresh) => fresh,
                Err(failed) => {
                    return Ok(ToolExecutionResult::error(format!(
                        "Refused: the tap on display {id} was approved, but the display could \
                         not be read again to confirm it is still there ({}). Nothing was done.",
                        failed.message
                    )))
                }
            };
            // The same question the original classification asked, asked
            // again against the screen as it is now: a tap is judged by what
            // is under it, a key by the control it activates, and nothing else
            // reaches past the phone on a display.
            let now = match action {
                "tap" => super::risk::classify_point(&fresh.elements, tap_pixels.0, tap_pixels.1),
                "key" => super::risk::classify_key(
                    &fresh.elements,
                    &super::normalize_keycode(described_keycode(&args)),
                ),
                _ => None,
            };
            if let Err(withheld) =
                super::gate::verify(&clearance, now.as_ref(), fresh.package.as_deref())
            {
                return Ok(ToolExecutionResult::error(withheld.reason));
            }
            // The press is computed against the screen as it is now, not as it
            // was when the question was asked.
            before_package = fresh.package;
            before_fingerprint = Some(super::tree_fingerprint(&fresh.elements));
        }

        // Two displays on one phone are two places to write, but the app on
        // them is one app: one process, one account, one set of unsaved edits.
        // `launch` names its app in the argument; every other action finds out
        // from the screen it just read. `stop` touches no app at all.
        let lease_package = if action == "stop" {
            None
        } else {
            before_package.clone()
        };
        let seen = super::app_completed(&device, lease_package.as_deref());
        let _lease = super::lease_app(&device, lease_package.as_deref(), seen).await;
        if _lease.stale {
            return Ok(ToolExecutionResult::error(format!(
                "Refused: another writer changed {} while this call waited for it — the screen \
                 read a moment ago no longer describes what is there. Nothing was done. Read the \
                 display again and retry.",
                lease_package.as_deref().unwrap_or("this app")
            )));
        }

        // The app's own screens are the controls that govern the agent; no
        // approval makes operating them right. Only `launch` names a package
        // without reading one, and the guard covers it too.
        if let Some(package) = before_package.as_deref() {
            if super::takeover::is_own_app(package) {
                return Ok(ToolExecutionResult::error(format!(
                    "Refused: this would write to Syscity's own screen ({package}). The app's \
                     settings, pairing and display pages are the controls that govern the \
                     agent, so the agent does not operate them — a person does. Nothing was \
                     done."
                )));
            }
        }

        // `launch` on a display is the one write that has to decide about the
        // app's *process* before it starts anything: a clean start means ending
        // it first, and ending it is not local. The decision needs the whole
        // task list, which only the helper can read, so it is taken here — after
        // the gate and the lease, before anything is dispatched.
        let mut cold_start: Option<super::coldstart::ColdStart> = None;
        if action == "launch" {
            let package = args.get("package").and_then(|v| v.as_str()).unwrap_or("");
            let state = super::display::VirtualDisplay::readiness(&bridge)
                .await
                .ok();
            let main = state.as_ref().and_then(|state| state.top_of(0));
            // "In use" is a task the person is *looking at*, and the launcher is
            // where the phone sits rather than something they opened — so it
            // never holds a launch back.
            let visible_on_main = main
                .and_then(|top| top.package.as_deref())
                .is_some_and(|shown| shown == package && !super::coldstart::is_home(shown));
            let decision = match state.as_ref().and_then(|s| s.displays_holding(package)) {
                Some(displays) => {
                    super::coldstart::ColdStart::decide(&displays, id, visible_on_main)
                }
                // The task list could not be read. Ending a process on a guess
                // is the one move that cannot be undone, so this is a warm
                // start — the person's screen is still visible in readiness and
                // still refuses.
                None => super::coldstart::ColdStart::unaware(visible_on_main),
            };
            if !decision.allowed() {
                return Ok(ToolExecutionResult::error(decision.describe(package)).with_data(
                    serde_json::json!({
                        "display": id,
                        "action": action,
                        "cold_start": decision.as_str(),
                        "launched": false,
                    }),
                ));
            }
            if decision.ends_process() {
                // `am force-stop` reaches the whole app, on every display and in
                // the background — which is exactly why the decision above had
                // to establish that nothing else was holding it.
                let argv = adb_args(&device, &["shell", "am", "force-stop", package]);
                let stopped =
                    run_cmd("adb", &argv.iter().map(|s| s.as_str()).collect::<Vec<_>>()).await;
                match stopped {
                    Ok((status, _, _)) if status.success() => {}
                    Ok((_, _, stderr)) => {
                        return Ok(ToolExecutionResult::error(format!(
                            "Refused: `{package}` could not be ended before starting it \
                             ({stderr}). Nothing was started — starting it now would leave \
                             whatever it had before on screen and call that a fresh start."
                        )))
                    }
                    Err(e) => {
                        return Ok(ToolExecutionResult::error(format!(
                            "Refused: `adb` could not end `{package}` before starting it \
                             ({e}). Nothing was started."
                        )))
                    }
                }
            }
            cold_start = Some(decision);
        }

        // Whether the phone is still in the state this write was computed for:
        // the helper still alive, the person's screen unlocked, this display
        // still holding the app it held. `launch` is exempt — it is the
        // ordinary way back into a display whose app has gone, so "this display
        // no longer shows what it did" is the starting condition, not a fault.
        if action != "launch" {
            if let Ok(readiness) = super::display::VirtualDisplay::readiness(&bridge).await {
                if let Err(blocked) =
                    super::ready::check(Some(&readiness), Target::Display(id), None)
                {
                    return Ok(ToolExecutionResult::error(super::ready::describe(&blocked)));
                }
            }
        }

        // What the model is about to do, named so that "the same action again"
        // is a question with an answer. A tap is worth more than its
        // coordinates: the label of what is under it, from the read above.
        let reads_only =
            matches!(action, "frame" | "status" | "limits" | "stop" | "mute" | "sound");
        let signature = match action {
            "tap" => format!("tap@{}:{}", tap_pixels.0, tap_pixels.1),
            "swipe" => {
                // The *resolved* pixels, not the arguments: two swipes written
                // one in each space are the same gesture, and a percent swipe
                // must not read as `swipe@0:0>0:0` to the ladder.
                let (x, y) = tap_pixels;
                let (x2, y2) = swipe_end.pixels();
                format!("swipe@{x}:{y}>{x2}:{y2}")
            }
            "text" => {
                let text = args.get("text").and_then(|v| v.as_str()).unwrap_or("");
                format!("text:{}", text.chars().take(40).collect::<String>())
            }
            "paste" => {
                let text = args.get("text").and_then(|v| v.as_str()).unwrap_or("");
                format!("paste:{}", text.chars().take(40).collect::<String>())
            }
            "key" => format!("key:{}", args.get("keycode").and_then(|v| v.as_str()).unwrap_or("")),
            _ => format!("{action}:{id}"),
        };
        let guard_key = super::guard::Guard::bucket_key(
            &context.conversation_id,
            &format!("{}:{}", device.as_deref().unwrap_or("-"), id),
        );
        let ladder = if reads_only {
            super::guard::Verdict::Clear
        } else {
            super::guard::Guard::global().check(&guard_key, &signature)
        };
        if let Some(reason) = ladder.refusal() {
            // A refusal to take an action that was already approved has to say
            // so: the person is not being asked again, and nothing was done.
            return Ok(ToolExecutionResult::error(if clearance.asked {
                format!("{reason} (An approval was given but unused; nothing was sent.)")
            } else {
                reason.to_string()
            }));
        }

        // What the launch turned into, once the display has been asked. Only a
        // launch has one: every other action is judged by the screen it acted
        // on, and this is the screen a launch *moved to*.
        let mut opened: Option<super::coldstart::Opened> = None;
        // Which copy the launch is asking about, when the system raised a
        // chooser because the app is installed twice. `None` is "not a
        // question", which is not the same as "could not tell".
        let mut chooser: Option<Vec<super::profiles::Choice>> = None;
        let result = match action {
            "stop" => display.close().await.map(|_| "Display closed.".to_string()),
            "launch" => {
                let package = args.get("package").and_then(|v| v.as_str()).unwrap_or("");
                match display.launch(package).await {
                    Ok(()) => {
                        // `am start -W` returns when the activity reports idle
                        // — for a chooser, without the app ever arriving. So the
                        // display is asked what actually came to the top, and
                        // the answer is the result rather than the command's
                        // exit.
                        opened =
                            super::coldstart::await_opened(&bridge, id, package, LAUNCH_SETTLE)
                                .await;
                        // A chooser is the system asking which copy of a
                        // twice-installed app to open. The display cannot
                        // answer it — a tap would be picking without knowing
                        // which copy — so it is named, its options are read,
                        // and the caller chooses.
                        if matches!(opened, Some(super::coldstart::Opened::Chooser { .. })) {
                            chooser = match (
                                super::profiles::users(&device).await,
                                super::dump_ui(&device, Target::Display(id)).await,
                            ) {
                                (Some(users), Ok(screen)) => {
                                    super::profiles::chooser_options(&screen.elements, &users)
                                }
                                // Not being able to read who else is on the
                                // phone, or the chooser itself, leaves the
                                // question open rather than answered: the
                                // caller is told it is a chooser either way.
                                _ => None,
                            };
                        }
                        let mut text = format!("Opened {package} on display {id} (muted).");
                        if let Some(rest) = opened
                            .as_ref()
                            .and_then(|opened| opened.describe(package, id))
                        {
                            text.push(' ');
                            text.push_str(&rest);
                        }
                        if let Some(choices) = &chooser {
                            text.push(' ');
                            text.push_str(&super::profiles::describe_choices(choices));
                        }
                        Ok(text)
                    }
                    Err(failed) => Err(failed),
                }
            }
            "frame" => match display.screenshot().await {
                Ok(frame) => {
                    let encoded = base64::Engine::encode(
                        &base64::engine::general_purpose::STANDARD,
                        &frame.png,
                    );
                    return Ok(ToolExecutionResult::success("Frame captured").with_data(
                        serde_json::json!({
                            "base64": encoded,
                            "format": "png",
                            "width": frame.width,
                            "height": frame.height,
                            "display": id,
                        }),
                    ));
                }
                Err(failed) => Err(failed),
            },
            "tap" => {
                let (x, y) = tap_pixels;
                display
                    .tap(x, y)
                    .await
                    .map(|_| format!("Tapped ({x}, {y}) on display {id}."))
            }
            "swipe" => {
                let value = |key: &str, default: i64| {
                    args.get(key).and_then(|v| v.as_i64()).unwrap_or(default)
                };
                let duration = u32::try_from(value("duration", 300).max(1)).unwrap_or(300);
                display
                    .swipe(tap_pixels, swipe_end.pixels(), duration)
                    .await
                    .map(|_| format!("Swiped on display {id}."))
            }
            "text" => {
                let text = args.get("text").and_then(|v| v.as_str()).unwrap_or("");
                display.text(text).await.map(|_| {
                    format!("Typed {} character(s) on display {id}.", text.chars().count())
                })
            }
            "paste" => {
                // Bypasses the key character map, so CJK and emoji arrive as
                // they are. `text` above remains the ASCII path.
                let text = args.get("text").and_then(|v| v.as_str()).unwrap_or("");
                display.paste_text(text).await.map(|_| {
                    format!(
                        "Pasted {} character(s) into the focused field on display {id}.",
                        text.chars().count()
                    )
                })
            }
            "key" => {
                let keycode = args
                    .get("keycode")
                    .and_then(|v| v.as_str())
                    .unwrap_or("BACK");
                display
                    .key(keycode)
                    .await
                    .map(|_| format!("Pressed {keycode} on display {id}."))
            }
            "mute" | "sound" => {
                let package = args.get("package").and_then(|v| v.as_str()).unwrap_or("");
                if package.is_empty() {
                    return Ok(ToolExecutionResult::error(
                        "`package` is required: say which app's sound to change",
                    ));
                }
                let muted = action == "mute";
                match display.set_audio_muted(package, muted).await {
                    Ok(true) => Ok(if muted {
                        format!(
                            "{package} is silent on display {id}; its sound returns when it leaves."
                        )
                    } else {
                        // True also for an app the agent never muted: nothing is
                        // being held down, and saying what it was set back to
                        // would imply it had been changed.
                        format!("{package}'s sound is no longer held down by the agent.")
                    }),
                    Ok(false) => Err(super::display::DisplayError::new(
                        super::display::DISPLAY_HELPER_ERROR,
                        format!(
                            "the sound of {package} was not changed: the helper records what \
                             the mode was before silencing an app, and will not touch one it \
                             cannot read first. The app is running; it is merely audible."
                        ),
                    )),
                    Err(failed) => Err(failed),
                }
            }
            other => {
                return Ok(ToolExecutionResult::error(format!(
                    "unknown action {other:?}; use start, stop, status, limits, frame, launch, \
                     tap, swipe, text, paste, key, mute or sound"
                )))
            }
        };

        // The ladder learns what this did, from the screen's own answer rather
        // than the action's: a tap the helper accepted is not a tap that
        // landed. Only a tap took a read before it, so only a tap has a
        // fingerprint to compare against; the rest are recorded as unseen,
        // which the ladder does not count as "nothing happened".
        if !reads_only {
            let (outcome, frame) = match before_fingerprint {
                Some(was) => match super::dump_ui(&device, Target::Display(id)).await {
                    Ok(now) => {
                        let now = super::tree_fingerprint(&now.elements);
                        let outcome = if now == was {
                            super::guard::Outcome::Unchanged
                        } else {
                            super::guard::Outcome::Changed
                        };
                        (outcome, Some(now))
                    }
                    Err(_) => (super::guard::Outcome::Unknown, None),
                },
                None => (super::guard::Outcome::Unknown, None),
            };
            super::guard::Guard::global().record_with(&guard_key, &signature, outcome, frame);
            if result.is_ok() {
                super::note_success(&guard_key);
            }
        }

        // Same pace as `android_input`, and likewise after the action: the
        // display is another screen running the same app, so a second send
        // there is a second send.
        let pace_note = if clearance.reaches_outward() && context.mobile_control().pace_outward {
            Some(super::note_outward(before_package.as_deref()).await)
        } else {
            None
        };

        Ok(match result {
            Ok(message) => {
                let mut text = message;
                if let Some(note) = ladder.note() {
                    text.push(' ');
                    text.push_str(note);
                }
                ToolExecutionResult::success(text).with_data(serde_json::json!({
                    "display": id,
                    "action": action,
                    // A launch carries three extra facts, and they are the
                    // ones a caller has to branch on: whether the process was
                    // ended first, and what actually came to the top — the app,
                    // a chooser, or something else.
                    "cold_start": cold_start.map(super::coldstart::ColdStart::as_str),
                    "opened": opened.as_ref().map(|opened| serde_json::json!({
                        "state": opened.as_str(),
                        "task": match opened {
                            super::coldstart::Opened::Here { task } => Some(*task),
                            _ => None,
                        },
                        "package": match opened {
                            super::coldstart::Opened::Chooser { package }
                            | super::coldstart::Opened::Elsewhere { package: Some(package) } => {
                                Some(package.clone())
                            }
                            _ => None,
                        },
                    })),
                    // The options the chooser is offering, when the launch
                    // raised one and they could be read. Absent means "not a
                    // chooser, or a question that could not be read" — the
                    // `opened` state says which.
                    "chooser": chooser.as_ref().map(|choices| {
                        serde_json::json!({
                            "options": choices.iter().map(|choice| serde_json::json!({
                                "label": choice.label,
                                "copy": choice.copy.as_str(),
                                "note": choice.copy.note(),
                            })).collect::<Vec<_>>(),
                        })
                    }),
                    "risk": clearance.note(),
                    "risk_approved": clearance.asked,
                    // Where the write landed, in both spaces, for the actions
                    // that have a point. Absent for the rest rather than
                    // reporting an origin nothing touched.
                    "coordinates": match action {
                        "tap" => tap_point.json(),
                        "swipe" => serde_json::json!({
                            "from": tap_point.json(),
                            "to": swipe_end.json(),
                        }),
                        _ => serde_json::Value::Null,
                    },
                    "paced": pace_note,
                    "guard": ladder.note(),
                }))
            }
            Err(failed) => ToolExecutionResult::error(failed.to_string()),
        })
    }
}

/// One line per fact the model needs before it writes to a display.
///
/// The three states are separate sentences because they call for three
/// different moves: unlock the phone, start the helper, or re-read the screen.
fn describe_readiness(state: &super::display::Readiness) -> String {
    let mut lines = Vec::new();
    lines.push(if state.locked {
        "The phone is locked: a tap on the user's screen lands on the lock screen, and an app \
         started behind it will not have changed."
            .to_string()
    } else {
        "The phone is unlocked.".to_string()
    });
    if let Some(top) = &state.main {
        lines.push(format!(
            "User's screen: {} (task {}).",
            top.package.as_deref().unwrap_or("an unnamed app"),
            top.task
        ));
    }
    if state.displays.is_empty() {
        lines.push("No displays are open.".to_string());
    } else {
        let shown: Vec<String> = state
            .displays
            .iter()
            .map(|top| match top.package.as_deref() {
                Some(package) => format!("display {} holds {package}", top.display),
                None => format!("display {} is empty", top.display),
            })
            .collect();
        lines.push(format!("{}.", shown.join("; ")));
    }
    lines.join(" ")
}

#[cfg(test)]
mod tests {
    use super::super::display::{Readiness, ScreenTop};
    use super::*;

    fn state(locked: bool, displays: Vec<ScreenTop>) -> Readiness {
        Readiness {
            helper: true,
            locked,
            main: Some(ScreenTop {
                display: 0,
                task: 4,
                package: Some("com.example.chat".to_string()),
            }),
            displays,
            tasks: None,
        }
    }

    #[test]
    fn a_locked_phone_is_said_in_the_status_line() {
        let text = describe_readiness(&state(true, Vec::new()));
        assert!(text.contains("locked"), "{text}");
        assert!(text.contains("lock screen"), "{text}");
    }

    #[test]
    fn each_display_says_what_it_holds() {
        let text = describe_readiness(&state(
            false,
            vec![
                ScreenTop {
                    display: 3,
                    task: 9,
                    package: Some("com.example.mail".to_string()),
                },
                ScreenTop {
                    display: 4,
                    task: -1,
                    package: None,
                },
            ],
        ));
        assert!(text.contains("display 3 holds com.example.mail"), "{text}");
        assert!(text.contains("display 4 is empty"), "{text}");
        assert!(!text.contains("No displays"), "{text}");
    }

    #[test]
    fn no_displays_says_so_rather_than_nothing() {
        let text = describe_readiness(&state(false, Vec::new()));
        assert!(text.contains("No displays are open"), "{text}");
    }
}
