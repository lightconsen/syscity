//! What is installed, what is running, and how an app is started cleanly.
use async_trait::async_trait;
use serde_json::Value;

// The set, the device targeting, and the siblings' items: this file used
// to be part of `android`, and names them exactly as it did there.
use super::*;

/// How long `am start -W` may wait for the activity to report idle.
///
/// `-W` is what turns "the command was dispatched" into "the app is up", and
/// the cost of that is paid by the device lock this call holds. An app that
/// never reports idle would otherwise hold the lock forever: `ProcessRequest`
/// defaults to `timeout: None`, which means exactly that, so the timeout is
/// not optional here.
pub(super) const LAUNCH_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(15);

/// How long a `resolve-activity` / `monkey` probe may take.
///
/// Neither waits on an app, so a tight bound is right: this is a lookup, and a
/// lookup that has not answered in ten seconds has failed.
pub(super) const PROBE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

/// Parse the `-brief` output of `cmd package resolve-activity`.
///
/// The command prints the component as the last non-empty line
/// (`com.example/.MainActivity`) after a `priority=…` line and, on some
/// releases, a `packageName=…` line. Returning the input verbatim when no
/// component-shaped line is found is deliberate: the caller then falls back to
/// `monkey` instead of launching whatever word happened to be first.
pub(super) fn parse_resolve_activity(stdout: &str) -> Option<String> {
    // `-brief` output is one line on API 26+, but older platform builds print
    // `priority=N`/`packageName=…` preamble as well, so scan for the shape
    // rather than trusting the last line.
    for line in stdout.lines().rev() {
        let line = line.trim();
        if line.is_empty() || line.starts_with("priority=") || line.starts_with("packageName=") {
            continue;
        }
        // A component is `package/class` (or `package/package.class`).
        let (pkg, class) = match line.split_once('/') {
            Some(parts) => parts,
            None => continue,
        };
        if pkg.is_empty() || class.is_empty() || pkg.contains(' ') {
            continue;
        }
        return Some(line.to_string());
    }
    None
}

/// What `am start -W` said about the launch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum LaunchOutcome {
    /// `Status: ok` — the activity is up.
    Started,
    /// The task was already in front; Android reports this on stderr and does
    /// not start the activity again. The app *is* on screen, so this counts as
    /// a success even though no `Status:` line appears.
    AlreadyInFront,
    /// `Error: …`, or `Status:` with something other than `ok`.
    Failed,
    /// No recognisable status: the command exited zero but said nothing about
    /// the activity. Reported as unknown rather than assumed to be fine.
    Unknown,
}

/// The phrase Android prints instead of `Status:` when the task is already top.
pub(super) const BROUGHT_TO_FRONT: &str =
    "Warning: Activity not started, its current task has been brought to the front";

/// Read the verdict out of `am start -W`'s combined output.
///
/// Both streams are passed in because the "already in front" warning goes to
/// stderr on some releases and stdout on others, and an `Error:` line is what
/// decides failure — a `Status: ok` that follows an `Error:` in the same run
/// must not mask it.
pub(super) fn parse_am_start_output(stdout: &str, stderr: &str) -> LaunchOutcome {
    let combined = format!("{stdout}\n{stderr}");
    for line in combined.lines() {
        let line = line.trim();
        if line.starts_with("Error:") || line.starts_with("Exception:") {
            return LaunchOutcome::Failed;
        }
        if line.contains(BROUGHT_TO_FRONT) {
            return LaunchOutcome::AlreadyInFront;
        }
        if let Some(rest) = line.strip_prefix("Status:") {
            // `Status: ok` / `Status: timeout` / `Status: warning`
            return if rest.trim().eq_ignore_ascii_case("ok") {
                LaunchOutcome::Started
            } else {
                LaunchOutcome::Failed
            };
        }
        if let Some(rest) = line.strip_prefix("Activity:") {
            // `Activity: com.example/.Main` — the launch reached the activity
            // manager, but this is not itself a verdict; keep scanning.
            let _ = rest;
        }
    }
    LaunchOutcome::Unknown
}

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

    /// Launch `package`, resolving its entry point instead of guessing.
    ///
    /// The old code sent `am start -n pkg/.MainActivity` whenever the caller did
    /// not name an activity, which is true of almost no app: the launcher entry
    /// is whatever the app declares, often `…ui.LauncherActivity` or a
    /// Flutter/Compose shell that is not called `MainActivity` at all. So:
    ///
    /// 1. `cmd package resolve-activity --brief -c android.intent.category.LAUNCHER`
    ///    — what the system itself would open. The `-c` is load-bearing: without
    ///    the category this resolves the *default* activity, which may be an
    ///    activity with no launcher entry at all.
    /// 2. `monkey -p <pkg> 1`, which asks the launcher machinery to open the
    ///    app's main entry. It exists precisely for the case where the package
    ///    manager cannot name a component.
    /// 3. Only then report `launch_no_entry`.
    ///
    /// The start itself uses `-W` and parses the verdict, because "the command
    /// was dispatched" and "the app is on screen" are different facts and the
    /// caller is about to act on the second one. `-W` waits for the activity to
    /// report idle, which some apps take seconds to do — hence [`LAUNCH_TIMEOUT`]
    /// rather than the default (unbounded) process timeout.
    async fn launch(
        &self,
        device: &Option<String>,
        args: &Value,
    ) -> crate::Result<ToolExecutionResult> {
        let pkg = args.get("package").and_then(|v| v.as_str()).unwrap_or("");
        if pkg.trim().is_empty() {
            return Ok(ToolExecutionResult::error("launch needs `package` — the app to start"));
        }
        let pkg = pkg.trim();

        // What this phone already learned about the app: the entry that worked
        // last time, the first screen, the question it asked. It is a prior,
        // not an instruction — an entry the phone refuses falls through to
        // resolving, and the fact is marked wrong.
        let known = super::knowledge::global().recall(pkg).await;
        let known_entry = known
            .iter()
            .find_map(|fact| fact.launch_entry().map(str::to_string));

        // `activity` from the caller still wins: an explicit request is not a
        // memory to be second-guessed.
        let explicit = match args.get("activity").and_then(|v| v.as_str()) {
            Some(a) if !a.trim().is_empty() => Some(format!("{pkg}/{}", a.trim())),
            Some(_) => Some(format!("{pkg}/.MainActivity")),
            None => None,
        };

        // The entry to try first. A remembered entry is tried as-is — the
        // probe it saves is the whole point — and a refusal by the platform
        // sends the launch back through a fresh resolve rather than reporting
        // that the app has no entry.
        let mut component = explicit.clone().or_else(|| known_entry.clone());
        let mut from_memory = explicit.is_none() && known_entry.is_some();
        let mut retried = false;

        let (component, verdict, detail, timed_out) = loop {
            let entry = match component.take() {
                Some(entry) => entry,
                None => match self.resolve_component(device, pkg).await {
                    Some(entry) => {
                        super::knowledge::global()
                            .record(
                                pkg,
                                super::knowledge::FactKind::LaunchEntry,
                                &entry,
                                Vec::new(),
                            )
                            .await;
                        entry
                    }
                    None => {
                        return Ok(ToolExecutionResult::error(format!(
                            "launch_no_entry: no launcher activity found for `{pkg}` — it may \
                             have none (a service, a shortcut-only app), or it may not be \
                             installed. `android_app_manager action=list_packages` shows what is."
                        ))
                        .with_data(serde_json::json!({
                            "device": resolved_serial(device).await,
                            "reason": "launch_no_entry",
                        })))
                    }
                },
            };

            let argv = adb_args(device, &["shell", "am", "start", "-W", "-n", &entry]);
            let outcome = super::super::common::run_cmd_timeout(
                "adb",
                &argv.iter().map(|s| s.as_str()).collect::<Vec<_>>(),
                LAUNCH_TIMEOUT,
            )
            .await;

            let (verdict, detail, timed_out) = match outcome {
                super::super::common::RunOutcome::Finished { status, stdout, stderr } => {
                    let _ = status;
                    let verdict = parse_am_start_output(&stdout, &stderr);
                    let detail = if stderr.trim().is_empty() {
                        stdout.trim().to_string()
                    } else {
                        format!("{}\n{}", stdout.trim(), stderr.trim())
                    };
                    (verdict, detail, false)
                }
                super::super::common::RunOutcome::TimedOut { stdout, stderr } => (
                    LaunchOutcome::Unknown,
                    format!(
                        "no answer within {} ms (the launch may still complete)\n{}\n{}",
                        LAUNCH_TIMEOUT.as_millis(),
                        stdout.trim(),
                        stderr.trim()
                    ),
                    true,
                ),
                super::super::common::RunOutcome::Failed(why) => {
                    return Ok(ToolExecutionResult::error(format!("could not run `adb`: {why}")))
                }
            };

            // The platform refused a remembered entry: it is out of date, and
            // the app is worth one more try from a fresh resolve.
            if verdict == LaunchOutcome::Failed && from_memory && !retried {
                retried = true;
                if let Some(fact) = known
                    .iter()
                    .find(|fact| fact.kind == super::knowledge::FactKind::LaunchEntry)
                {
                    super::knowledge::global().mark(fact, false).await;
                }
                from_memory = false;
                continue;
            }

            // A remembered entry that started the app held up.
            if from_memory
                && matches!(verdict, LaunchOutcome::Started | LaunchOutcome::AlreadyInFront)
            {
                if let Some(fact) = known
                    .iter()
                    .find(|fact| fact.kind == super::knowledge::FactKind::LaunchEntry)
                {
                    super::knowledge::global().mark(fact, true).await;
                }
            }

            break (entry, verdict, detail, timed_out);
        };
        // "Already in front" is Android declining to restart an app that is
        // already open. The app is on screen, which is all the caller asked for.
        let started = matches!(verdict, LaunchOutcome::Started | LaunchOutcome::AlreadyInFront);

        // An install in two profiles is two copies of one package, and Android
        // answers a launch of it by asking which copy to open instead of
        // opening one. `am start` reports `Status: ok` for that question — the
        // activity it started is the chooser — so the launch is not finished
        // until the screen says what rose to the top. Read it here rather than
        // reporting "started" and leaving the caller to discover the question
        // on its next observation.
        //
        // The same read is what the app is learned from, so it is taken once:
        // a chooser is not the app, and a screen whose own tree names a
        // different package is not the app either — both leave nothing to
        // record, and the first screen of the app that was asked for is never
        // confused with the surface that opened instead.
        let landing = if started {
            dump_ui(device, Target::Screen).await.ok()
        } else {
            None
        };
        let chooser = match landing.as_ref() {
            Some(dump) => self.chooser_in(device, dump).await,
            None => None,
        };

        // What was learned on the way: the first screen the launch landed on.
        // The entry itself was written where it was resolved, so only the
        // screen is left to read. Best-effort: a launch that worked is not
        // failed by a note that could not be written, and a chooser is not the
        // app, so nothing is learned from a question.
        let learned = match landing.as_ref() {
            Some(dump) if chooser.is_none() => self.learn_first_screen(pkg, dump, &known).await,
            _ => None,
        };

        let sentence = match (&chooser, verdict) {
            (Some(choices), _) => format!(
                "`{pkg}` is up only as a question: {}. Pick one copy with \
                 `android_dialog` before going on.",
                super::profiles::describe_choices(choices)
            ),
            (None, LaunchOutcome::Started) => format!("`{pkg}` is up ({component})."),
            (None, LaunchOutcome::AlreadyInFront) => {
                format!("`{pkg}` was already in front; Android did not restart it ({component}).")
            }
            (None, LaunchOutcome::Failed) => format!("launch failed for `{pkg}` ({component})."),
            (None, LaunchOutcome::Unknown) if timed_out => format!(
                "`adb` did not confirm `{pkg}` within {} ms; it may still come up. Observe before acting.",
                LAUNCH_TIMEOUT.as_millis()
            ),
            (None, LaunchOutcome::Unknown) => {
                format!("`adb` exited without a status for `{pkg}`; observe before acting.")
            }
        };
        // The remembered first screen, when it is still the one on the phone.
        let prior = if learned.is_none() {
            super::knowledge::describe(&known)
        } else {
            learned.clone()
        };
        let (sentence, prior) = match prior {
            Some(note) => (format!("{sentence}\n{note}"), Some(note)),
            None => (sentence, None),
        };

        let status_word = match (&chooser, verdict) {
            (Some(_), _) => "chooser",
            (None, LaunchOutcome::Started) => "ok",
            (None, LaunchOutcome::AlreadyInFront) => "already_in_front",
            (None, LaunchOutcome::Failed) => "error",
            (None, LaunchOutcome::Unknown) => "unknown",
        };
        let data = serde_json::json!({
            "device": resolved_serial(device).await,
            "component": component,
            // A chooser is not the app: `launched` stays true because the
            // start was accepted, and `chooser` is the fact the caller has to
            // act on.
            "launched": started,
            "timed_out": timed_out,
            "status": status_word,
            "remembered": prior,
            "chooser": chooser.map(|choices| serde_json::json!({
                "package": pkg,
                "options": choices.iter().map(|choice| serde_json::json!({
                    "label": choice.label,
                    "copy": choice.copy.as_str(),
                    "note": choice.copy.note(),
                })).collect::<Vec<_>>(),
            })),
        });

        Ok(if started {
            ToolExecutionResult::success(format!("{sentence}\n{detail}")).with_data(data)
        } else {
            ToolExecutionResult::error(format!("{sentence}\n{detail}")).with_data(data)
        })
    }

    /// What this phone remembers about the apps, and a way to forget one.
    ///
    /// Knowledge is written by the tools that learn it, so the only thing a
    /// caller needs is to see it — and to drop it when it is wrong in a way no
    /// screen will correct (an app the person uninstalled, a fact recorded on a
    /// device they no longer use).
    async fn app_knowledge(&self, args: &Value) -> crate::Result<ToolExecutionResult> {
        let package = args
            .get("package")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|value| !value.is_empty());

        if args
            .get("forget")
            .and_then(|v| v.as_bool())
            .unwrap_or(false)
        {
            let Some(package) = package else {
                return Ok(ToolExecutionResult::error(
                    "forget needs `package` — say which app's knowledge to drop",
                ));
            };
            let removed = super::knowledge::global().forget(package).await;
            return Ok(ToolExecutionResult::success(if removed == 0 {
                format!("Nothing was remembered about `{package}`, so nothing was dropped.")
            } else {
                format!("Dropped {removed} fact(s) remembered about `{package}`.")
            })
            .with_data(serde_json::json!({ "package": package, "forgotten": removed })));
        }

        let facts = match package {
            Some(package) => super::knowledge::global().recall(package).await,
            None => super::knowledge::global()
                .facts()
                .await
                .into_iter()
                .map(|fact| super::knowledge::FactRecall {
                    package: fact.package,
                    kind: fact.kind,
                    text: fact.text,
                    labels: fact.labels,
                })
                .collect(),
        };

        if facts.is_empty() {
            let scope = package
                .map(|package| format!("about `{package}`"))
                .unwrap_or_else(|| "about any app".to_string());
            return Ok(ToolExecutionResult::success(format!(
                "Nothing is remembered {scope} yet. Knowledge is written when an app is launched: \
                 the entry that worked, the first screen it offered, and any question it asked."
            )));
        }

        let mut lines = vec![format!("{} remembered fact(s):", facts.len())];
        for fact in &facts {
            lines.push(format!("  · {} — {}: {}", fact.package, fact.kind.as_str(), fact.text));
        }
        Ok(ToolExecutionResult::success(lines.join("\n")).with_data(serde_json::json!({
            "count": facts.len(),
            "facts": facts.iter().map(|fact| serde_json::json!({
                "package": fact.package,
                "kind": format!("{:?}", fact.kind),
                "text": fact.text,
                "labels": fact.labels,
            })).collect::<Vec<_>>(),
        })))
    }

    /// Whether what came to the front after a launch is the system asking
    /// which copy of an app to open, and what the options are.
    ///
    /// Best-effort in both directions. The tree has already been read by the
    /// caller, which is why it is passed in rather than taken again; the user
    /// list may be unreadable (no shell). Neither is a launch failure, and
    /// neither is a reason to guess: the answer is `None` and the launch is
    /// reported as the `am start` verdict said.
    async fn chooser_in(
        &self,
        device: &Option<String>,
        dump: &UiDump,
    ) -> Option<Vec<super::profiles::Choice>> {
        let users = super::profiles::users(device).await?;
        let current = super::profiles::current_user(device)
            .await
            .unwrap_or(super::profiles::OWNER);
        let others: Vec<super::profiles::UserProfile> = users
            .into_iter()
            .filter(|user| user.id != current)
            .collect();
        super::profiles::chooser_options(&dump.elements, &others)
    }

    /// Read the screen a launch landed on, and remember what it offers.
    ///
    /// Two facts come out of one read, and only when the read is honest about
    /// which app it is looking at: the screen's own tree names its package, so
    /// a launch that landed somewhere else (a chooser, a redirect into a
    /// browser) is not attributed to the app that was asked for.
    ///
    /// What was already known is marked here rather than in a separate pass:
    /// this is the one moment a remembered first screen can be compared with
    /// the real one, so a fact that no longer describes the app loses standing
    /// exactly where it was tested.
    ///
    /// Returns the note to append to the launch's answer, when a remembered
    /// fact was confirmed or contradicted. `None` on a first launch, or when
    /// nothing could be read.
    async fn learn_first_screen(
        &self,
        pkg: &str,
        dump: &UiDump,
        known: &[super::knowledge::FactRecall],
    ) -> Option<String> {
        // The tree says which app it belongs to. A read that cannot say, or
        // says something else, is not this app's first screen — a redirect
        // into a browser or a launcher would otherwise teach us that the app's
        // first screen is somebody else's.
        if dump.package.as_deref() != Some(pkg) {
            return None;
        }

        let labels = super::knowledge::home_facts(&dump.elements);
        if labels.is_empty() {
            return None;
        }

        let mut note = None;
        if let Some(fact) = known
            .iter()
            .find(|fact| fact.kind == super::knowledge::FactKind::Home)
        {
            let held = super::knowledge::home_matches(&fact.labels, &dump.elements);
            super::knowledge::global().mark(fact, held).await;
            if !held && !fact.labels.is_empty() {
                note = Some(format!(
                    "The first screen does not match what was remembered about `{pkg}` (it used \
                     to offer {}), so that recollection no longer holds.",
                    fact.labels.join(", ")
                ));
            }
        }

        let text = labels.join(", ");
        super::knowledge::global()
            .record(pkg, super::knowledge::FactKind::Home, &format!("offers {text}"), labels)
            .await;

        // A dialog on the first screen is the question the app asks before it
        // is usable; remembering it lets the next launch expect it instead of
        // being surprised by it.
        if let Some(dialog) = super::knowledge::first_dialog(&dump.elements) {
            super::knowledge::global()
                .record(pkg, super::knowledge::FactKind::Dialog, &dialog, Vec::new())
                .await;
        }

        note
    }

    /// List the apps installed here, and which of them are also installed in
    /// another profile.
    ///
    /// The plain `pm list packages` answers for the user the shell is in, which
    /// on a phone with a work profile or a clone is one of several package
    /// lists. An app that is in two of them is *the same package name*, so a
    /// caller reading only its own list cannot tell the two copies apart — and
    /// a launch of it raises the system's "which one?" question instead of
    /// opening the app. `alsoIn` is that second answer.
    ///
    /// The extra reads are best-effort: on a phone with one user there is
    /// nothing to ask, and on a phone whose `pm` refuses `--user` the list is
    /// still the right answer to "what is installed here". Neither is a reason
    /// to fail the listing, so the result reports `profiles_known`.
    async fn list_packages(&self, device: &Option<String>) -> crate::Result<ToolExecutionResult> {
        let argv = adb_args(device, &["shell", "pm", "list", "packages"]);
        let (status, stdout, stderr) =
            run_cmd("adb", &argv.iter().map(|s| s.as_str()).collect::<Vec<_>>())
                .await
                .map_err(|e| crate::error::SyscityError::ExternalService {
                    source: "adb app manager failed".to_string(),
                    cause: Some(Box::new(e)),
                })?;
        if !status.success() {
            return Ok(ToolExecutionResult::error(format!(
                "listing the apps failed: {}",
                stderr.trim()
            )));
        }
        let own = super::profiles::parse_packages(&stdout);
        let current = super::profiles::current_user(device)
            .await
            .unwrap_or(super::profiles::OWNER);
        let shared = super::profiles::other_profiles(device, current)
            .await
            .map(|others| super::profiles::also_in(&others))
            .unwrap_or_default();
        let profiles_known = super::profiles::users(device).await.is_some();

        let apps: Vec<Value> = own
            .iter()
            .map(|package| {
                let mut entry = serde_json::json!({ "package": package });
                if let Some(profiles) = shared.get(package) {
                    entry["also_in"] = serde_json::json!(profiles);
                }
                entry
            })
            .collect();
        let text = if shared.is_empty() {
            format!(
                "{} app(s) installed{}.\n{}",
                own.len(),
                if profiles_known {
                    ""
                } else {
                    " (no other profile but the phone's own)"
                },
                stdout.trim()
            )
        } else {
            let mut lines = vec![format!(
                "{} app(s) installed; {} also have a copy in another profile:",
                own.len(),
                shared.len()
            )];
            for (package, profiles) in &shared {
                lines.push(format!("  {package} — also in {}", profiles.join(", ")));
            }
            lines.join("\n")
        };

        Ok(ToolExecutionResult::success(text).with_data(serde_json::json!({
            "device": resolved_serial(device).await,
            "apps": apps,
            "also_in": shared,
            "profiles_known": profiles_known,
        })))
    }

    /// Ask the phone which activity `package` launches through.
    ///
    /// Tries the package manager first, then `monkey`, which is the launcher's
    /// own entry path. Both are read-only probes on a bounded clock: a lookup
    /// that has not answered in ten seconds has failed.
    async fn resolve_component(&self, device: &Option<String>, pkg: &str) -> Option<String> {
        let argv = adb_args(
            device,
            &[
                "shell",
                "cmd",
                "package",
                "resolve-activity",
                "--brief",
                "-c",
                "android.intent.category.LAUNCHER",
                pkg,
            ],
        );
        if let super::super::common::RunOutcome::Finished { status, stdout, .. } =
            super::super::common::run_cmd_timeout(
                "adb",
                &argv.iter().map(|s| s.as_str()).collect::<Vec<_>>(),
                PROBE_TIMEOUT,
            )
            .await
        {
            if status.success() {
                if let Some(component) = parse_resolve_activity(&stdout) {
                    return Some(component);
                }
            }
            // `Error: …` on stdout is how the package manager says it could not
            // resolve; it is not worth logging, `monkey` is the next answer.
        }

        // `monkey -p pkg 1` prints the component it chose in its events dump.
        let argv = adb_args(
            device,
            &[
                "shell",
                "monkey",
                "-p",
                pkg,
                "-c",
                "android.intent.category.LAUNCHER",
                "1",
            ],
        );
        if let super::super::common::RunOutcome::Finished { stdout, .. } =
            super::super::common::run_cmd_timeout(
                "adb",
                &argv.iter().map(|s| s.as_str()).collect::<Vec<_>>(),
                PROBE_TIMEOUT,
            )
            .await
        {
            // `  // com.example/.MainActivity` — the chosen component, indented
            // under `cmp=`. Older releases print it as `cmp=com.example/.MainActivity`.
            for line in stdout.lines() {
                let line = line.trim();
                let candidate = line
                    .strip_prefix("// ")
                    .or_else(|| line.strip_prefix("cmp="))
                    .unwrap_or(line);
                if let Some((p, c)) = candidate.split_once('/') {
                    if p == pkg && !c.is_empty() && !c.contains(' ') {
                        return Some(candidate.to_string());
                    }
                }
            }
        }
        None
    }
}

#[async_trait]
impl Tool for AdbAppManagerTool {
    fn name(&self) -> &str {
        "android_app_manager"
    }

    fn description(&self) -> &str {
        "Install, launch, force-stop, or list apps on an Android device. 'completed' means the \
         adb command was dispatched, not that the app started — observe afterwards to see. \
         `list_packages` marks an app also installed in another profile (`also_in`); `launch` of \
         such an app can raise the system's \"which copy?\" question, reported as `chooser`. \
         `launch` remembers the entry that worked and the first screen it landed on, and later \
         launches start from that memory; `knowledge` shows what is remembered, per app."
    }

    fn parameters_schema(&self) -> Value {
        create_schema(
            "Manage Android apps",
            serde_json::json!({
                "action": {
                    "type": "string",
                    "description": "install | launch | force_stop | list_packages | knowledge",
                    "enum": ["install", "launch", "force_stop", "list_packages", "knowledge"]
                },
                "package": { "type": "string", "description": "Package name (e.g. com.example.app)" },
                "activity": { "type": "string", "description": "Activity class (launch)" },
                "apk_path": { "type": "string", "description": "Local path to APK (install)" },
                "forget": {
                    "type": "boolean",
                    "description": "With action=knowledge and a package: drop everything remembered about it."
                },
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
        // One device: two conversations must not type into it at once. The
        // lock is per serial. See `tools::target_lock`.
        let device = match resolve_device(&args, self.device.as_deref()) {
            Ok(device) => device,
            Err(refusal) => return Ok(ToolExecutionResult::error(refusal)),
        };
        let _target = lock_device(&device).await;
        let action = args.get("action").and_then(|v| v.as_str()).unwrap_or("");

        // `launch` is not a plain argv: it has to find the entry point, which is
        // a query of its own, and then wait for the activity. It reports what it
        // learned rather than a single line of stdout.
        if action == "launch" {
            return self.launch(&device, &args).await;
        }
        if action == "list_packages" {
            return self.list_packages(&device).await;
        }
        if action == "knowledge" {
            return self.app_knowledge(&args).await;
        }

        let argv = match action {
            "install" => {
                let path = args.get("apk_path").and_then(|v| v.as_str()).unwrap_or("");
                adb_args(&device, &["install", "-r", path])
            }
            "force_stop" => {
                let pkg = args.get("package").and_then(|v| v.as_str()).unwrap_or("");
                adb_args(&device, &["shell", "am", "force-stop", pkg])
            }
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
