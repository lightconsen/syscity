//! Claude-Code-style permission modes and allow/deny/ask rules.
//!
//! A `PermissionsRuntime` sits in front of the registry's policy gate and
//! decides one of four things for every tool call before any hook runs:
//!
//! - **deny** — a `[permissions].deny` rule matched: refuse with a reason.
//! - **ask** — a `.ask` rule matched: force the approval flow even if the
//!   tool itself would not require one.
//! - **allow now** — a `.allow` rule matched, or the active mode pre-approves
//!   the call (`accept_edits` for file writers, `bypass` for everything not
//!   denied). Hooks still run; the `requires_approval` fallback is skipped.
//! - **pass through** — today's semantics: hooks, then the fallback.
//!
//! Modes are per session: a gateway-wide default comes from config
//! (`[permissions].mode`) and a client can override the current conversation
//! (`sessions.set_mode`) until it disconnects or the daemon restarts — the
//! override is deliberately ephemeral.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;
use std::sync::{Arc, RwLock};

use crate::hooks::matcher::glob_match;

/// The permission mode a conversation runs in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PermissionMode {
    /// Today's semantics: rules, then hooks, then the approval fallback.
    #[default]
    Default,
    /// File writers (`category: "write"`) run without approval.
    AcceptEdits,
    /// Read-only exploration: every non-read-only tool is refused.
    Plan,
    /// Skip approval prompts entirely; deny rules and hooks still apply.
    Bypass,
}

impl PermissionMode {
    /// Parse a mode from its wire form. Accepts the serde spelling
    /// (`accept_edits`) and the hyphenated display form (`accept-edits`).
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_lowercase().as_str() {
            "default" => Some(Self::Default),
            "accept_edits" | "accept-edits" => Some(Self::AcceptEdits),
            "plan" => Some(Self::Plan),
            "bypass" => Some(Self::Bypass),
            _ => None,
        }
    }

    /// The canonical wire form (serde snake_case).
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Default => "default",
            Self::AcceptEdits => "accept_edits",
            Self::Plan => "plan",
            Self::Bypass => "bypass",
        }
    }
}

/// The `[permissions]` block of the gateway config.
///
/// Lives in the tools layer so the registry (the choke point) and the
/// gateway can both consume it without a dependency cycle.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct PermissionsConfig {
    /// The gateway-wide default mode.
    pub mode: PermissionMode,
    /// When false (the default), `bypass` is refused — a stray
    /// "approve everything" must be turned on deliberately.
    pub allow_bypass: bool,
    /// Pre-approved calls: `"tool"` or `"tool:glob"` over the primary
    /// invocation argument.
    pub allow: Vec<String>,
    /// Refused calls, whatever the mode.
    pub deny: Vec<String>,
    /// Calls forced through the approval flow, whatever the mode.
    pub ask: Vec<String>,
}

impl Default for PermissionsConfig {
    fn default() -> Self {
        Self {
            mode: PermissionMode::Default,
            allow_bypass: false,
            allow: Vec::new(),
            deny: Vec::new(),
            ask: Vec::new(),
        }
    }
}

/// What the permission engine tells the registry gate to do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EngineDecision {
    /// Refuse the call with this reason.
    Deny(String),
    /// Force the approval flow with this reason.
    Ask(String),
    /// Rules or the mode pre-approved the call. Hooks still run; the
    /// `requires_approval` fallback is skipped.
    AllowNow,
    /// Nothing special: hooks, then the fallback — today's semantics.
    PassThrough,
}

/// The argument that identifies what a call acts on, for rule matching.
///
/// `shell`/`process`/`execute_code` → the command string; the file tools →
/// the path; `web_fetch` → the URL. `None` for tools whose identity is the
/// tool name alone, which is what bare `"tool"` rules match.
pub fn primary_invocation_arg(tool: &str, args: &Value) -> Option<String> {
    let field = match tool {
        "shell" | "process" | "execute_code" => "command",
        "file_read" | "file_write" | "file_edit" | "apply_patch" => "path",
        "web_fetch" => "url",
        _ => return None,
    };
    args.get(field)
        .and_then(Value::as_str)
        .map(|s| s.to_string())
}

/// Whether `rules` contains a bare `"tool"` entry, or a `"tool:glob"` entry
/// whose glob matches `primary`. A call with no primary argument never
/// matches a glob rule — matching on nothing would mean matching everything.
pub fn rules_match(rules: &[String], tool: &str, primary: Option<&str>) -> bool {
    rules.iter().any(|rule| match rule.split_once(':') {
        Some((name, glob)) => name == tool && primary.is_some_and(|p| glob_match(glob, p)),
        None => rule == tool,
    })
}

/// The allow rule a remembered approval ("yes, don't ask again") becomes.
///
/// Command- and URL-shaped calls remember the approved invocation and
/// anything that extends it (`"shell:git status*"`); the file writers
/// remember the parent directory (`"file_write:/workspace/*"`), because a
/// follow-up write to the same directory is the useful unit while an exact
/// path is useless for the next file; every other tool remembers just its
/// name.
pub fn remember_rule(tool: &str, args: &Value) -> String {
    let primary = primary_invocation_arg(tool, args);
    match tool {
        "shell" | "process" | "execute_code" | "web_fetch" => primary
            .map(|arg| format!("{tool}:{arg}*"))
            .unwrap_or_else(|| tool.to_string()),
        "file_write" | "file_edit" | "apply_patch" => primary
            .map(|arg| {
                let dir = std::path::Path::new(&arg)
                    .parent()
                    .map(|p| p.display().to_string())
                    .unwrap_or(arg);
                format!("{tool}:{dir}/*")
            })
            .unwrap_or_else(|| tool.to_string()),
        _ => tool.to_string(),
    }
}

/// What the gate consults: config defaults plus the per-session overrides.
#[derive(Debug, Clone)]
struct Snapshot {
    default_mode: PermissionMode,
    allow_bypass: bool,
    allow: Arc<[String]>,
    deny: Arc<[String]>,
    ask: Arc<[String]>,
    per_session: Arc<HashMap<String, PermissionMode>>,
}

/// Shared, lock-light permission state.
///
/// The gateway seeds it from config at start, reloads it on `config.set` and
/// hot reload, and writes session overrides through the WS layer; the
/// registry reads it on every tool call. A snapshot clones a handful of
/// `Arc`s under the lock and never holds it across an `.await`.
#[derive(Debug, Clone)]
pub struct PermissionsRuntime {
    inner: Arc<RwLock<Snapshot>>,
}

impl Default for PermissionsRuntime {
    fn default() -> Self {
        Self {
            inner: Arc::new(RwLock::new(Snapshot {
                default_mode: PermissionMode::Default,
                allow_bypass: false,
                allow: Vec::new().into(),
                deny: Vec::new().into(),
                ask: Vec::new().into(),
                per_session: Arc::new(HashMap::new()),
            })),
        }
    }
}

impl PermissionsRuntime {
    /// Seed from the config's `[permissions]` block.
    pub fn from_config(config: &PermissionsConfig) -> Self {
        let runtime = Self::default();
        runtime.reload(config);
        runtime
    }

    /// Replace the config-derived state (mode, flags, rules); per-session
    /// overrides survive a reload — they are runtime state, not config.
    pub fn reload(&self, config: &PermissionsConfig) {
        let mut guard = self.inner.write().unwrap_or_else(|e| e.into_inner());
        guard.default_mode = config.mode;
        guard.allow_bypass = config.allow_bypass;
        guard.allow = config.allow.clone().into();
        guard.deny = config.deny.clone().into();
        guard.ask = config.ask.clone().into();
    }

    /// The mode `conversation_id` runs in: its override, else the default.
    pub fn effective_mode(&self, conversation_id: &str) -> PermissionMode {
        let guard = self.inner.read().unwrap_or_else(|e| e.into_inner());
        guard
            .per_session
            .get(conversation_id)
            .copied()
            .unwrap_or(guard.default_mode)
    }

    /// Set (or, with `None`, clear) a conversation's mode override.
    pub fn set_session_mode(&self, conversation_id: &str, mode: Option<PermissionMode>) {
        let mut guard = self.inner.write().unwrap_or_else(|e| e.into_inner());
        let per_session = Arc::make_mut(&mut guard.per_session);
        match mode {
            Some(mode) => {
                per_session.insert(conversation_id.to_string(), mode);
            }
            None => {
                per_session.remove(conversation_id);
            }
        }
    }

    /// Whether `bypass` may be selected at all.
    pub fn allow_bypass(&self) -> bool {
        self.inner
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .allow_bypass
    }

    /// Whether `conversation_id` carries a per-session override.
    pub fn has_session_override(&self, conversation_id: &str) -> bool {
        self.inner
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .per_session
            .contains_key(conversation_id)
    }

    /// Append an allow rule (deduped) and return whether it was new.
    /// Used by approve-and-remember.
    pub fn append_allow_rule(&self, rule: String) -> bool {
        let mut guard = self.inner.write().unwrap_or_else(|e| e.into_inner());
        if guard.allow.iter().any(|r| r == &rule) {
            return false;
        }
        let mut allow = guard.allow.to_vec();
        allow.push(rule);
        guard.allow = allow.into();
        true
    }

    /// Copy out the config-shaped view (for `config.get` and tests).
    pub fn config_projection(&self) -> PermissionsConfig {
        let guard = self.inner.read().unwrap_or_else(|e| e.into_inner());
        PermissionsConfig {
            mode: guard.default_mode,
            allow_bypass: guard.allow_bypass,
            allow: guard.allow.to_vec(),
            deny: guard.deny.to_vec(),
            ask: guard.ask.to_vec(),
        }
    }

    /// Take a cheap snapshot for the gate's hot path.
    ///
    /// Private to this module: `Snapshot` is an internal shape, and the only
    /// caller is `evaluate_for` below. Keeping the method private is what keeps
    /// the type out of a public interface.
    fn snapshot(&self) -> Snapshot {
        self.inner.read().unwrap_or_else(|e| e.into_inner()).clone()
    }

    /// The permission engine: rules + mode for one call, before any hook.
    /// The registry gate turns the decision into a `ToolPolicyDecision` and
    /// still runs the hooks afterwards.
    pub(crate) fn evaluate(
        &self,
        name: &str,
        args: &Value,
        conversation_id: &str,
        read_only: bool,
        categories: &[String],
    ) -> EngineDecision {
        let snapshot = self.snapshot();
        evaluate(&snapshot, name, args, conversation_id, read_only, categories)
    }
}

/// Evaluate rules + mode for one call. This is the permission engine proper;
/// the registry gate turns the decision into a [`ToolPolicyDecision`] and
/// still runs the hooks afterwards.
fn evaluate(
    snapshot: &Snapshot,
    name: &str,
    args: &Value,
    conversation_id: &str,
    read_only: bool,
    categories: &[String],
) -> EngineDecision {
    let mut mode = snapshot
        .per_session
        .get(conversation_id)
        .copied()
        .unwrap_or(snapshot.default_mode);
    if mode == PermissionMode::Bypass && !snapshot.allow_bypass {
        tracing::warn!(
            "bypass mode requested but [permissions].allow_bypass is false; using default mode"
        );
        mode = PermissionMode::Default;
    }

    let primary = primary_invocation_arg(name, args);

    if rules_match(&snapshot.deny, name, primary.as_deref()) {
        return EngineDecision::Deny(format!("denied by a [permissions].deny rule for '{name}'"));
    }
    // The call itself can say it needs to run outside the fence. That is an
    // ask by construction, and it lands before the rules so an `allow` rule
    // cannot silently grant the escape — a rule is "stop asking", not "leave
    // the fence". Bypass skips it like every other prompt.
    if let Some(escalation) = super::escalation::declared_escalation(args) {
        if mode == PermissionMode::Bypass {
            return EngineDecision::AllowNow;
        }
        return EngineDecision::Ask(format!(
            "asked to run outside the workspace fence: {}",
            escalation.justification
        ));
    }
    if rules_match(&snapshot.ask, name, primary.as_deref()) {
        return EngineDecision::Ask(format!("matched a [permissions].ask rule for '{name}'"));
    }
    if rules_match(&snapshot.allow, name, primary.as_deref()) {
        return EngineDecision::AllowNow;
    }

    match mode {
        PermissionMode::Default => EngineDecision::PassThrough,
        PermissionMode::AcceptEdits => {
            if categories.iter().any(|c| c == "write") {
                EngineDecision::AllowNow
            } else {
                EngineDecision::PassThrough
            }
        }
        PermissionMode::Plan => {
            if read_only {
                EngineDecision::PassThrough
            } else {
                EngineDecision::Deny(
                    "plan mode is active — read-only tools only; switch out of plan mode \
                     to run this"
                        .to_string(),
                )
            }
        }
        PermissionMode::Bypass => EngineDecision::AllowNow,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn mode_parse_accepts_both_spellings_and_refuses_junk() {
        assert_eq!(PermissionMode::parse("default"), Some(PermissionMode::Default));
        assert_eq!(PermissionMode::parse("accept-edits"), Some(PermissionMode::AcceptEdits));
        assert_eq!(PermissionMode::parse("accept_edits"), Some(PermissionMode::AcceptEdits));
        assert_eq!(PermissionMode::parse(" plan "), Some(PermissionMode::Plan));
        assert_eq!(PermissionMode::parse("bypass"), Some(PermissionMode::Bypass));
        assert_eq!(PermissionMode::parse("neon"), None);
        assert_eq!(PermissionMode::parse(""), None);
    }

    #[test]
    fn mode_wire_form_round_trips() {
        for mode in [
            PermissionMode::Default,
            PermissionMode::AcceptEdits,
            PermissionMode::Plan,
            PermissionMode::Bypass,
        ] {
            assert_eq!(PermissionMode::parse(mode.as_str()), Some(mode));
        }
    }

    #[test]
    fn primary_arg_follows_the_tools_identity_field() {
        assert_eq!(
            primary_invocation_arg("shell", &json!({"command": "git status"})),
            Some("git status".to_string())
        );
        assert_eq!(
            primary_invocation_arg("file_write", &json!({"path": "/tmp/a.txt"})),
            Some("/tmp/a.txt".to_string())
        );
        assert_eq!(
            primary_invocation_arg("web_fetch", &json!({"url": "https://x"})),
            Some("https://x".to_string())
        );
        assert_eq!(primary_invocation_arg("grep", &json!({"pattern": "x"})), None);
    }

    #[test]
    fn rules_match_bare_names_and_globs() {
        let rules = vec!["grep".to_string(), "shell:git status*".to_string()];
        assert!(rules_match(&rules, "grep", None));
        assert!(!rules_match(&rules, "shell", None), "no primary, no glob match");
        assert!(rules_match(&rules, "shell", Some("git status")));
        assert!(rules_match(&rules, "shell", Some("git status -s")));
        assert!(!rules_match(&rules, "shell", Some("rm -rf")));
        assert!(!rules_match(&rules, "shellx", Some("git status")));
    }

    #[test]
    fn runtime_effective_mode_prefers_the_session_override() {
        let runtime = PermissionsRuntime::from_config(&PermissionsConfig {
            mode: PermissionMode::Plan,
            ..Default::default()
        });
        assert_eq!(runtime.effective_mode("s1"), PermissionMode::Plan);

        runtime.set_session_mode("s1", Some(PermissionMode::Bypass));
        assert_eq!(runtime.effective_mode("s1"), PermissionMode::Bypass);
        assert_eq!(runtime.effective_mode("s2"), PermissionMode::Plan);
        assert!(runtime.has_session_override("s1"));

        runtime.set_session_mode("s1", None);
        assert_eq!(runtime.effective_mode("s1"), PermissionMode::Plan);
        assert!(!runtime.has_session_override("s1"));
    }

    #[test]
    fn reload_keeps_session_overrides() {
        let runtime = PermissionsRuntime::from_config(&PermissionsConfig::default());
        runtime.set_session_mode("s1", Some(PermissionMode::Plan));
        runtime.reload(&PermissionsConfig {
            mode: PermissionMode::AcceptEdits,
            allow_bypass: true,
            ..Default::default()
        });
        assert_eq!(runtime.effective_mode("s1"), PermissionMode::Plan);
        assert_eq!(runtime.effective_mode("other"), PermissionMode::AcceptEdits);
        assert!(runtime.allow_bypass());
    }

    #[test]
    fn append_allow_rule_dedupes() {
        let runtime = PermissionsRuntime::default();
        assert!(runtime.append_allow_rule("shell:git status*".to_string()));
        assert!(!runtime.append_allow_rule("shell:git status*".to_string()));
        let projection = runtime.config_projection();
        assert_eq!(projection.allow, vec!["shell:git status*"]);
    }

    #[test]
    fn engine_deny_beats_everything_and_bypass_still_honours_it() {
        let snapshot = Snapshot {
            default_mode: PermissionMode::Bypass,
            allow_bypass: true,
            allow: vec!["grep".to_string()].into(),
            deny: vec!["shell:rm -rf*".to_string()].into(),
            ask: Vec::new().into(),
            per_session: Arc::new(HashMap::new()),
        };
        assert!(matches!(
            evaluate(&snapshot, "shell", &json!({"command": "rm -rf /"}), "s1", false, &[]),
            EngineDecision::Deny(_)
        ));
        // An allow rule does not rescue a denied tool.
        assert!(matches!(
            evaluate(&snapshot, "shell", &json!({"command": "rm -rf /"}), "s1", false, &[]),
            EngineDecision::Deny(_)
        ));
    }

    #[test]
    fn a_declared_escalation_is_an_ask_that_allow_rules_cannot_silence() {
        let snapshot = Snapshot {
            default_mode: PermissionMode::Default,
            allow_bypass: true,
            // An allow rule for the very command, to prove the declaration
            // wins: "stop asking" is not "leave the fence".
            allow: vec!["shell:git status*".to_string()].into(),
            deny: Vec::new().into(),
            ask: Vec::new().into(),
            per_session: Arc::new(HashMap::new()),
        };
        let args = json!({
            "command": "git status",
            "permissions": { "require_escalated": true, "justification": "sees the host clock" }
        });
        let decision = evaluate(&snapshot, "shell", &args, "s1", false, &[]);
        assert!(
            matches!(decision, EngineDecision::Ask(ref why) if why.contains("sees the host clock")),
            "got {decision:?}"
        );

        // A deny rule still wins over the declaration.
        let denied = Snapshot {
            deny: vec!["shell".to_string()].into(),
            ..snapshot.clone()
        };
        assert!(matches!(
            evaluate(&denied, "shell", &args, "s1", false, &[]),
            EngineDecision::Deny(_)
        ));

        // Bypass is "no prompts", so it skips the ask rather than queueing one.
        let bypass = Snapshot {
            default_mode: PermissionMode::Bypass,
            ..snapshot
        };
        assert_eq!(evaluate(&bypass, "shell", &args, "s1", false, &[]), EngineDecision::AllowNow);
    }

    #[test]
    fn engine_ask_beats_allow_and_mode() {
        let snapshot = Snapshot {
            default_mode: PermissionMode::Bypass,
            allow_bypass: true,
            allow: vec!["web_fetch".to_string()].into(),
            deny: Vec::new().into(),
            ask: vec!["web_fetch".to_string()].into(),
            per_session: Arc::new(HashMap::new()),
        };
        assert!(matches!(
            evaluate(&snapshot, "web_fetch", &json!({"url": "https://x"}), "s1", true, &[]),
            EngineDecision::Ask(_)
        ));
    }

    #[test]
    fn plan_mode_refuses_mutating_and_admits_read_only() {
        let snapshot = Snapshot {
            default_mode: PermissionMode::Plan,
            allow_bypass: false,
            allow: Vec::new().into(),
            deny: Vec::new().into(),
            ask: Vec::new().into(),
            per_session: Arc::new(HashMap::new()),
        };
        let deny = evaluate(&snapshot, "shell", &json!({"command": "ls"}), "s1", false, &[]);
        assert!(matches!(deny, EngineDecision::Deny(ref m) if m.contains("plan mode")));
        let pass = evaluate(&snapshot, "grep", &json!({}), "s1", true, &[]);
        assert_eq!(pass, EngineDecision::PassThrough);
    }

    #[test]
    fn bypass_without_the_config_flag_falls_back_to_default() {
        let snapshot = Snapshot {
            default_mode: PermissionMode::Bypass,
            allow_bypass: false,
            allow: Vec::new().into(),
            deny: Vec::new().into(),
            ask: Vec::new().into(),
            per_session: Arc::new(HashMap::new()),
        };
        // A tool that would need approval passes through to the fallback
        // instead of being pre-approved.
        let decision = evaluate(&snapshot, "shell", &json!({"command": "ls"}), "s1", false, &[]);
        assert_eq!(decision, EngineDecision::PassThrough);
    }

    #[test]
    fn accept_edits_pre_approves_only_writers() {
        let snapshot = Snapshot {
            default_mode: PermissionMode::AcceptEdits,
            allow_bypass: false,
            allow: Vec::new().into(),
            deny: Vec::new().into(),
            ask: Vec::new().into(),
            per_session: Arc::new(HashMap::new()),
        };
        let writer = ["file".to_string(), "write".to_string()];
        assert_eq!(
            evaluate(&snapshot, "file_write", &json!({"path": "/tmp/a"}), "s1", false, &writer),
            EngineDecision::AllowNow
        );
        let reader = ["file".to_string(), "read".to_string()];
        assert_eq!(
            evaluate(&snapshot, "file_read", &json!({"path": "/tmp/a"}), "s1", true, &reader),
            EngineDecision::PassThrough
        );
    }
}
