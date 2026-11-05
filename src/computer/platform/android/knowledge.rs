//! What this phone has already taught us about each app.
//!
//! A model that has opened an app once knows three things the second time: the
//! component that launches it, roughly what its first screen offers, and which
//! question it asks first. Relearning them costs a `resolve-activity` probe, a
//! `monkey` probe, and a turn spent reading a screen that was read before —
//! every single launch, on every device.
//!
//! So the tools write down what they learned, keyed by package, and read it
//! back only when that package comes up again. Nothing is injected into the
//! prompt ahead of time: a fact is recalled because the model is about to act
//! on that app, which is the only moment it can be wrong in a way the model
//! would notice.
//!
//! # Why not a directory of markdown
//!
//! Mika ships hand-written app/site notes. That works when the notes are
//! authored once by a person. The facts here are written by the tool as it
//! runs, per device, and there is no author to keep a file honest — so what
//! keeps them honest has to be the feedback: a fact that was recalled and
//! contradicted by the screen loses weight, and one that keeps being confirmed
//! keeps being offered. That is exactly the hit-rate loop the memory system
//! already runs ([`crate::memory::effectiveness`]), so it is the one this uses
//! rather than a second, parallel notion of "how sure are we".
//!
//! # What it is not
//!
//! It never overrides the screen. A remembered launch entry is tried first
//! because it saves two probes, but a failed start falls back to resolving, and
//! then the remembered fact is corrected. The current read always wins; the
//! knowledge only shortens the path to it.

use std::collections::HashMap;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use tokio::sync::RwLock;
use tracing::{debug, warn};

use super::{element_label, UiElement};
use crate::memory::effectiveness::{EffectivenessConfig, EffectivenessStats};

/// The file under the memory directory, when one can be resolved.
const FILE_NAME: &str = "app_knowledge.json";

/// How many facts one recall is allowed to carry.
///
/// A launch note competes with the screen for the model's attention, and the
/// screen is the authority; a handful of facts is a prior, a page of them is a
/// second opinion nobody asked for.
const MAX_RECALLED: usize = 6;

/// How many labels of a first screen are kept as its description.
const HOME_LABELS: usize = 8;

/// How long a fact stays quiet after being shown alongside a screen.
///
/// Guessed knowledge is a prior, and a prior repeated on every read is noise:
/// a model that observes six times in a row does not need the same sentence
/// six times. Fifteen minutes is longer than any one task spends on a screen
/// and shorter than the gap between two conversations.
const SCREEN_QUIET_SECS: u64 = 15 * 60;

/// Upper bound on stored facts, so a long-lived phone's file stays a cache and
/// not a database. The worst-performing facts are dropped first.
const MAX_FACTS: usize = 400;

/// The three things worth remembering about an app.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FactKind {
    /// The component the launcher opens, e.g. `com.x/.MainActivity`.
    LaunchEntry,
    /// The names on the app's first screen, in tree order.
    Home,
    /// A question the app asked right after it opened.
    Dialog,
}

impl FactKind {
    /// The word the model reads.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::LaunchEntry => "launch entry",
            Self::Home => "first screen",
            Self::Dialog => "dialog after launch",
        }
    }

    /// Order in a recalled note: the launch entry first, since it is the one
    /// the next launch can act on.
    fn order(self) -> u8 {
        match self {
            Self::LaunchEntry => 0,
            Self::Home => 1,
            Self::Dialog => 2,
        }
    }
}

/// One remembered fact.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Fact {
    /// The package the fact is about.
    pub package: String,
    /// What kind of fact it is.
    pub kind: FactKind,
    /// The fact itself, as the model reads it.
    pub text: String,
    /// For a first screen, the names it offered — what a later read is
    /// compared against. Empty for the other kinds.
    #[serde(default)]
    pub labels: Vec<String>,
    /// Recall hit rate, from the same counters the memory system uses.
    pub stats: EffectivenessStats,
    /// How many times the fact has been seen to be true when written.
    #[serde(default)]
    pub seen: u32,
    /// When it was last written or recalled, for eviction order.
    #[serde(default)]
    pub last_seen: u64,
    /// When this fact was last put in front of the model alongside a screen.
    ///
    /// Separate from [`last_seen`](Self::last_seen): that one is about how
    /// fresh the fact is, this one is about how recently it was *said*. A
    /// screen note is said once and then kept quiet for a while
    /// ([`SCREEN_QUIET_SECS`]) so repeating the same observation does not
    /// repeat the same sentence.
    #[serde(default)]
    pub last_shown: u64,
}

/// A fact handed back for a turn to use, and to later confirm or deny.
#[derive(Debug, Clone, PartialEq)]
pub struct FactRecall {
    /// The package it is about.
    pub package: String,
    /// What kind of fact it is.
    pub kind: FactKind,
    /// The fact as the model reads it.
    pub text: String,
    /// The names a first screen offered (see [`Fact::labels`]).
    pub labels: Vec<String>,
}

impl FactRecall {
    /// Whether this recall predicted a launch component for `package`.
    pub fn launch_entry(&self) -> Option<&str> {
        (self.kind == FactKind::LaunchEntry).then_some(self.text.as_str())
    }
}

/// What one launch should write down, and whether it should have known it.
#[derive(Debug, Default)]
pub struct Learned {
    /// The component that actually started the app.
    pub launch_entry: Option<String>,
    /// The names on the first screen, when one was read.
    pub home: Option<Vec<String>>,
    /// A question the app asked right after opening.
    pub dialog: Option<String>,
}

/// The facts one package has accumulated.
#[derive(Debug, Default, Serialize, Deserialize)]
struct State {
    /// Keyed by package + kind + normalised text.
    facts: HashMap<String, Fact>,
    /// Whether the file has been read. Not serialised.
    #[serde(skip)]
    loaded: bool,
}

impl State {
    fn key(package: &str, kind: FactKind, text: &str) -> String {
        format!("{package}\u{1f}{}\u{1f}{}", kind.as_str(), normalise(text))
    }
}

/// A fact's text with the differences that are not differences removed:
/// case, runs of whitespace, and the decorative quotes a screen may add.
pub fn normalise(text: &str) -> String {
    let lowered = text.to_lowercase();
    let mut out = String::with_capacity(lowered.len());
    let mut last_space = true;
    for ch in lowered.chars() {
        if ch.is_whitespace() {
            if !last_space {
                out.push(' ');
            }
            last_space = true;
            continue;
        }
        out.push(ch);
        last_space = false;
    }
    out.trim().to_string()
}

/// Remembered facts about the apps on this phone.
///
/// One instance per process ([`global`]), because the phone is one device:
/// what one turn learned about an app is true for the next turn too, exactly
/// like the guard's counts and the read history.
pub struct AppKnowledge {
    path: Option<std::path::PathBuf>,
    state: RwLock<State>,
}

impl std::fmt::Debug for AppKnowledge {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AppKnowledge")
            .field("path", &self.path)
            .finish_non_exhaustive()
    }
}

/// The process-wide store, backed by the memory directory when one is
/// installed and held in memory otherwise.
///
/// A process with no path root (a unit test, a tool used outside a gateway)
/// does not get a panic: the knowledge is still usable for the life of the
/// process, it just does not survive a restart.
pub fn global() -> &'static AppKnowledge {
    static STORE: std::sync::OnceLock<AppKnowledge> = std::sync::OnceLock::new();
    STORE.get_or_init(|| match crate::dirs::installed_paths() {
        Some(paths) => AppKnowledge::at(paths.memory_dir().join(FILE_NAME)),
        None => {
            debug!("no installed path root; app knowledge will not persist");
            AppKnowledge::in_memory()
        }
    })
}

impl AppKnowledge {
    /// A store persisted at `path`.
    pub fn at(path: impl Into<std::path::PathBuf>) -> Self {
        Self {
            path: Some(path.into()),
            state: RwLock::new(State::default()),
        }
    }

    /// A store that lives only as long as this value.
    pub fn in_memory() -> Self {
        Self {
            path: None,
            state: RwLock::new(State {
                loaded: true,
                ..State::default()
            }),
        }
    }

    /// Read the file once, if there is one.
    ///
    /// A file that cannot be read or parsed is not an error the caller has to
    /// handle: this is a cache of facts the tools can relearn, and failing a
    /// launch because the cache is corrupt would be the tail wagging the dog.
    async fn ensure_loaded(&self) {
        let mut state = self.state.write().await;
        if state.loaded {
            return;
        }
        state.loaded = true;
        let Some(path) = self.path.as_ref() else {
            return;
        };
        let text = match tokio::fs::read_to_string(path).await {
            Ok(text) => text,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return,
            Err(e) => {
                warn!("could not read app knowledge at {}: {e}", path.display());
                return;
            }
        };
        match serde_json::from_str::<Vec<Fact>>(&text) {
            Ok(facts) => {
                for fact in facts {
                    state
                        .facts
                        .insert(State::key(&fact.package, fact.kind, &fact.text), fact);
                }
                debug!("loaded {} app fact(s)", state.facts.len());
            }
            Err(e) => warn!("ignoring unreadable app knowledge at {}: {e}", path.display()),
        }
    }

    /// Write down something the tools just learned about `package`.
    ///
    /// A fact already known is not duplicated: it is marked seen again, which
    /// is what raises its standing in a later recall without pretending the
    /// hit rate moved.
    pub async fn record(&self, package: &str, kind: FactKind, text: &str, labels: Vec<String>) {
        let package = package.trim();
        let text = text.trim();
        if package.is_empty() || text.is_empty() {
            return;
        }
        self.ensure_loaded().await;
        let snapshot = {
            let mut state = self.state.write().await;
            let key = State::key(package, kind, text);
            let now = now_secs();
            match state.facts.get_mut(&key) {
                Some(fact) => {
                    fact.seen += 1;
                    fact.last_seen = now;
                    if !labels.is_empty() {
                        fact.labels = labels;
                    }
                }
                None => {
                    // One app has one first screen. A read that found different
                    // names is a newer description of the same thing, not a
                    // second one — keeping both would offer two answers to one
                    // question, and the stale one would never lose on its own.
                    if kind == FactKind::Home {
                        state
                            .facts
                            .retain(|_, fact| !(fact.package == package && fact.kind == kind));
                    }
                    state.facts.insert(
                        key,
                        Fact {
                            package: package.to_string(),
                            kind,
                            text: text.to_string(),
                            labels,
                            stats: EffectivenessStats::default(),
                            seen: 1,
                            last_seen: now,
                            last_shown: 0,
                        },
                    );
                }
            }
            evict(&mut state.facts);
            state.facts.values().cloned().collect::<Vec<_>>()
        };
        self.persist(&snapshot).await;
    }

    /// What is known about `package`, best first — or nothing.
    ///
    /// "Best" is the memory system's own ordering: a fact that keeps being
    /// confirmed outranks one that has never been tested, and a fresh fact
    /// outranks an old one at equal standing.
    pub async fn recall(&self, package: &str) -> Vec<FactRecall> {
        self.ensure_loaded().await;
        let state = self.state.read().await;
        let mut found: Vec<&Fact> = state
            .facts
            .values()
            .filter(|fact| fact.package == package)
            .collect();
        found.sort_by(|a, b| {
            b.stats
                .hit_rate
                .partial_cmp(&a.stats.hit_rate)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| a.kind.order().cmp(&b.kind.order()))
                .then_with(|| b.seen.cmp(&a.seen))
                .then_with(|| b.last_seen.cmp(&a.last_seen))
        });
        found
            .into_iter()
            .take(MAX_RECALLED)
            .map(|fact| FactRecall {
                package: fact.package.clone(),
                kind: fact.kind,
                text: fact.text.clone(),
                labels: fact.labels.clone(),
            })
            .collect()
    }

    /// What is worth saying about the screen in front of the model.
    ///
    /// The same knowledge as [`recall`](Self::recall), filtered by what is
    /// actually on this screen — a remembered first screen that half matches
    /// the current read, and any dialog the app is known to ask. A fact that
    /// has just been said is held back, so six observations in a row carry one
    /// note rather than six.
    ///
    /// Only facts this read supports are returned. Knowledge that does not
    /// match is not evidence of anything, and offering it would tell the model
    /// that a different screen is the one it is looking at.
    pub async fn for_screen(&self, package: &str, elements: &[UiElement]) -> Vec<FactRecall> {
        self.ensure_loaded().await;
        let now = now_secs();
        let state = self.state.read().await;
        let mut found: Vec<&Fact> = state
            .facts
            .values()
            .filter(|fact| fact.package == package)
            .filter(|fact| match fact.kind {
                FactKind::Home => home_matches(&fact.labels, elements),
                FactKind::Dialog => {
                    let seen = first_dialog(elements);
                    seen.is_some_and(|seen| normalise(&seen) == normalise(&fact.text))
                }
                // The launch entry is about the launch, not the screen: it is
                // said once when the app was opened, not on every read.
                FactKind::LaunchEntry => false,
            })
            .filter(|fact| now.saturating_sub(fact.last_shown) >= SCREEN_QUIET_SECS)
            .collect();
        found.sort_by_key(|a| a.kind.order());
        found
            .into_iter()
            .take(MAX_RECALLED)
            .map(|fact| FactRecall {
                package: fact.package.clone(),
                kind: fact.kind,
                text: fact.text.clone(),
                labels: fact.labels.clone(),
            })
            .collect()
    }

    /// Record that these facts were put in front of the model.
    ///
    /// Called by whoever prints them, so the quiet window starts from the
    /// moment the model actually saw the sentence.
    pub async fn mark_shown(&self, facts: &[FactRecall]) {
        if facts.is_empty() {
            return;
        }
        self.ensure_loaded().await;
        let snapshot = {
            let mut state = self.state.write().await;
            let now = now_secs();
            let mut touched = false;
            for recall in facts {
                let key = State::key(&recall.package, recall.kind, &recall.text);
                if let Some(fact) = state.facts.get_mut(&key) {
                    fact.last_shown = now;
                    touched = true;
                }
            }
            touched.then(|| state.facts.values().cloned().collect::<Vec<_>>())
        };
        if let Some(snapshot) = snapshot {
            self.persist(&snapshot).await;
        }
    }

    /// Say whether a recalled fact held up.
    ///
    /// This is the feedback half: the screen is the authority, and a fact it
    /// contradicts loses standing until it is dropped
    /// ([`should_drop`]). A fact that was recalled but that this turn could
    /// not test is simply left alone — an untested fact is not a wrong one.
    pub async fn mark(&self, recall: &FactRecall, held: bool) {
        self.ensure_loaded().await;
        let snapshot = {
            let mut state = self.state.write().await;
            let key = State::key(&recall.package, recall.kind, &recall.text);
            let Some(fact) = state.facts.get_mut(&key) else {
                return;
            };
            fact.stats.record(held, 0);
            fact.last_seen = now_secs();
            let drop_it = !held && should_drop(&fact.stats, &EffectivenessConfig::default());
            let kind = fact.kind;
            let recalls = fact.stats.total_recalls;
            if drop_it {
                state.facts.remove(&key);
                debug!(
                    "dropped a {} fact for {}: the screen contradicted it {} time(s)",
                    kind.as_str(),
                    recall.package,
                    recalls
                );
            }
            state.facts.values().cloned().collect::<Vec<_>>()
        };
        self.persist(&snapshot).await;
    }

    /// Forget everything about `package` (all kinds).
    pub async fn forget(&self, package: &str) -> usize {
        self.ensure_loaded().await;
        let (removed, snapshot) = {
            let mut state = self.state.write().await;
            let doomed: Vec<String> = state
                .facts
                .iter()
                .filter(|(_, fact)| fact.package == package)
                .map(|(key, _)| key.clone())
                .collect();
            for key in &doomed {
                state.facts.remove(key);
            }
            (doomed.len(), state.facts.values().cloned().collect::<Vec<_>>())
        };
        if removed > 0 {
            self.persist(&snapshot).await;
        }
        removed
    }

    /// Everything known, for a caller that wants to show or export it.
    pub async fn facts(&self) -> Vec<Fact> {
        self.ensure_loaded().await;
        let state = self.state.read().await;
        let mut facts: Vec<Fact> = state.facts.values().cloned().collect();
        facts.sort_by(|a, b| {
            a.package
                .cmp(&b.package)
                .then_with(|| a.kind.order().cmp(&b.kind.order()))
        });
        facts
    }

    /// Write the current facts to disk, when there is a disk to write to.
    ///
    /// Through a temporary file and a rename, so a crash mid-write leaves the
    /// previous file intact instead of a truncated one the next load would
    /// refuse.
    async fn persist(&self, facts: &[Fact]) {
        let Some(path) = self.path.as_ref() else {
            return;
        };
        if let Some(parent) = path.parent() {
            if let Err(e) = tokio::fs::create_dir_all(parent).await {
                warn!("could not create {} for app knowledge: {e}", parent.display());
                return;
            }
        }
        let text = match serde_json::to_string_pretty(facts) {
            Ok(text) => text,
            Err(e) => {
                warn!("could not serialise app knowledge: {e}");
                return;
            }
        };
        let tmp = path.with_extension("json.tmp");
        if let Err(e) = tokio::fs::write(&tmp, text).await {
            warn!("could not write app knowledge to {}: {e}", tmp.display());
            return;
        }
        if let Err(e) = tokio::fs::rename(&tmp, path).await {
            warn!("could not replace {}: {e}", path.display());
        }
    }
}

/// Whether the feedback says a fact should be dropped.
///
/// The same rule the memory manager applies to a recalled memory: enough
/// recalls to mean something ([`min_recalls_for_adjustment`]) and a hit rate
/// at or below the demotion bar. Below the minimum, silence stays silence.
///
/// [`min_recalls_for_adjustment`]: EffectivenessConfig::min_recalls_for_adjustment
pub fn should_drop(stats: &EffectivenessStats, config: &EffectivenessConfig) -> bool {
    !config.auto_adjust
        || (stats.total_recalls >= config.min_recalls_for_adjustment
            && stats.hit_rate <= config.demotion_threshold)
}

/// Drop the least useful facts when the store has grown past its bound.
fn evict(facts: &mut HashMap<String, Fact>) {
    if facts.len() <= MAX_FACTS {
        return;
    }
    let mut ranked: Vec<(String, f32, u64)> = facts
        .iter()
        .map(|(key, fact)| (key.clone(), fact.stats.hit_rate, fact.last_seen))
        .collect();
    ranked.sort_by(|a, b| {
        a.1.partial_cmp(&b.1)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.2.cmp(&b.2))
    });
    for (key, _, _) in ranked.into_iter().take(facts.len() - MAX_FACTS) {
        facts.remove(&key);
    }
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// The names on an app's first screen, in the order the tree lists them.
///
/// Clickable and enabled only: those are the places a task can go, which is
/// what "what does this app's home offer" means. Duplicates are collapsed and
/// the list is capped — this describes a screen, it does not transcribe it.
pub fn home_facts(elements: &[UiElement]) -> Vec<String> {
    let mut labels: Vec<String> = Vec::new();
    for element in elements
        .iter()
        .filter(|element| element.clickable && element.enabled)
    {
        let Some(label) = element_label(element) else {
            continue;
        };
        let label = label.trim();
        if label.is_empty() || label.chars().count() > 40 {
            continue;
        }
        if labels.iter().any(|known| known.eq_ignore_ascii_case(label)) {
            continue;
        }
        labels.push(label.to_string());
        if labels.len() >= HOME_LABELS {
            break;
        }
    }
    labels
}

/// Whether a screen still offers most of what a first-screen fact recorded.
///
/// At least half of the recorded names, and at least one. Half rather than all
/// because a home screen legitimately changes — a badge, a new promotion — and
/// all-or-nothing would throw away a useful fact the first time the app
/// rearranged a tile.
pub fn home_matches(labels: &[String], elements: &[UiElement]) -> bool {
    if labels.is_empty() {
        return false;
    }
    let present: Vec<String> = home_facts(elements);
    let shared = labels
        .iter()
        .filter(|label| present.iter().any(|seen| seen.eq_ignore_ascii_case(label)))
        .count();
    shared >= 1 && shared * 2 >= labels.len()
}

/// The note a launch carries when something is already known about the app.
///
/// `None` when nothing is known, so a first launch is not decorated with an
/// empty heading.
pub fn describe(facts: &[FactRecall]) -> Option<String> {
    if facts.is_empty() {
        return None;
    }
    let mut lines = vec![format!(
        "What this phone already knows about `{}` (from earlier runs — the screen is still the \
         authority):",
        facts[0].package
    )];
    for fact in facts {
        lines.push(format!("  · {}: {}", fact.kind.as_str(), fact.text));
    }
    Some(lines.join("\n"))
}

/// The text of a dialog on `elements`, when the screen holds one.
///
/// The dialog's own nodes are found by class, then the names inside their
/// rectangle are read as its message. A dialog with no text at all (a bare
/// confirmation, a spinner) is not worth remembering: there is nothing the
/// next launch could recognise it by.
pub fn first_dialog(elements: &[UiElement]) -> Option<String> {
    let roots: Vec<&UiElement> = elements
        .iter()
        .filter(|element| super::tools::is_dialog_class(&element.class))
        .collect();
    if roots.is_empty() {
        return None;
    }
    let (x1, y1, x2, y2) =
        roots
            .iter()
            .fold((i32::MAX, i32::MAX, i32::MIN, i32::MIN), |acc, element| {
                let (ex1, ey1, ex2, ey2) = element.bounds;
                (acc.0.min(ex1), acc.1.min(ey1), acc.2.max(ex2), acc.3.max(ey2))
            });
    let message = elements
        .iter()
        .filter(|element| {
            let (ex1, ey1, ex2, ey2) = element.bounds;
            ex1 >= x1 && ey1 >= y1 && ex2 <= x2 && ey2 <= y2
        })
        .filter(|element| !element.clickable)
        .filter_map(element_label)
        .map(str::trim)
        .filter(|label| !label.is_empty() && label.chars().count() <= 80)
        .collect::<Vec<_>>()
        .join(" ");
    (!message.trim().is_empty()).then_some(message)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn element(index: usize, label: &str, clickable: bool) -> UiElement {
        let mut element = UiElement::new(
            index,
            Some(label.to_string()),
            None,
            None,
            "Button".to_string(),
            clickable,
            true,
            (0, 0, 10, 10),
        );
        element.key = format!("button:{label}:{index}");
        element
    }

    fn dialog_root(index: usize) -> UiElement {
        let mut root = element(index, "", false);
        root.class = "android.app.Dialog".to_string();
        root
    }

    #[tokio::test]
    async fn knowledge_stays_with_the_app_it_was_learned_on() {
        let store = AppKnowledge::in_memory();
        store
            .record("com.one", FactKind::LaunchEntry, "com.one/.Main", Vec::new())
            .await;
        assert_eq!(store.recall("com.one").await.len(), 1);
        assert!(store.recall("com.two").await.is_empty());
    }

    #[tokio::test]
    async fn the_same_fact_is_learned_once_and_seen_again() {
        let store = AppKnowledge::in_memory();
        store
            .record("com.one", FactKind::LaunchEntry, "com.one/.Main", Vec::new())
            .await;
        store
            .record("com.one", FactKind::LaunchEntry, "  COM.one/.Main ", Vec::new())
            .await;

        let facts = store.facts().await;
        assert_eq!(facts.len(), 1, "case and spacing are not a second fact");
        assert_eq!(facts[0].seen, 2);
    }

    #[tokio::test]
    async fn a_contradicted_fact_is_dropped_after_enough_evidence() {
        let store = AppKnowledge::in_memory();
        store
            .record("com.one", FactKind::LaunchEntry, "com.one/.Old", Vec::new())
            .await;
        let fact = store.recall("com.one").await.pop().expect("recorded");

        // One contradiction is an accident; the config asks for three.
        store.mark(&fact, false).await;
        store.mark(&fact, false).await;
        assert_eq!(store.recall("com.one").await.len(), 1);

        store.mark(&fact, false).await;
        assert!(
            store.recall("com.one").await.is_empty(),
            "a fact the screen contradicted three times is not offered again"
        );
    }

    #[tokio::test]
    async fn one_app_has_one_first_screen() {
        let store = AppKnowledge::in_memory();
        store
            .record("com.one", FactKind::Home, "offers Inbox", vec!["Inbox".into()])
            .await;
        store
            .record("com.one", FactKind::Home, "offers Feed", vec!["Feed".into()])
            .await;

        let homes: Vec<_> = store
            .recall("com.one")
            .await
            .into_iter()
            .filter(|fact| fact.kind == FactKind::Home)
            .collect();
        assert_eq!(homes.len(), 1, "a new description replaces the old one: {homes:?}");
        assert_eq!(homes[0].text, "offers Feed");
    }

    #[tokio::test]
    async fn a_confirmed_fact_is_preferred_over_an_untested_one() {
        let store = AppKnowledge::in_memory();
        store
            .record("com.one", FactKind::Dialog, "asks for a PIN", Vec::new())
            .await;
        store
            .record("com.one", FactKind::Home, "home is a grid", Vec::new())
            .await;
        let home = store
            .recall("com.one")
            .await
            .into_iter()
            .find(|fact| fact.kind == FactKind::Home)
            .expect("recorded");
        store.mark(&home, true).await;

        let recalled = store.recall("com.one").await;
        assert_eq!(recalled[0].kind, FactKind::Home, "{recalled:?}");
    }

    #[tokio::test]
    async fn the_launch_entry_is_offered_before_the_rest() {
        let store = AppKnowledge::in_memory();
        store
            .record("com.one", FactKind::Dialog, "asks for a PIN", Vec::new())
            .await;
        store
            .record("com.one", FactKind::LaunchEntry, "com.one/.Main", Vec::new())
            .await;

        let recalled = store.recall("com.one").await;
        assert_eq!(recalled[0].launch_entry(), Some("com.one/.Main"));
    }

    #[tokio::test]
    async fn knowledge_survives_a_restart() {
        let dir = std::env::temp_dir().join(format!("syscity-kb-{}", uuid::Uuid::new_v4()));
        let path = dir.join(FILE_NAME);
        {
            let store = AppKnowledge::at(&path);
            store
                .record("com.one", FactKind::Home, "home is a grid", vec!["Inbox".into()])
                .await;
        }
        let store = AppKnowledge::at(&path);
        let recalled = store.recall("com.one").await;
        assert_eq!(recalled.len(), 1, "{recalled:?}");
        assert_eq!(recalled[0].labels, vec!["Inbox".to_string()]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn a_corrupt_file_is_a_cache_miss_not_a_failure() {
        let dir = std::env::temp_dir().join(format!("syscity-kb-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(FILE_NAME);
        std::fs::write(&path, "{ not json").unwrap();

        let store = AppKnowledge::at(&path);
        assert!(store.recall("com.one").await.is_empty());
        store
            .record("com.one", FactKind::LaunchEntry, "com.one/.Main", Vec::new())
            .await;
        assert_eq!(store.recall("com.one").await.len(), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn forgetting_an_app_forgets_all_of_it() {
        let store = AppKnowledge::in_memory();
        store
            .record("com.one", FactKind::LaunchEntry, "com.one/.Main", Vec::new())
            .await;
        store
            .record("com.one", FactKind::Dialog, "asks for a PIN", Vec::new())
            .await;
        store
            .record("com.two", FactKind::LaunchEntry, "com.two/.Main", Vec::new())
            .await;

        assert_eq!(store.forget("com.one").await, 2);
        assert!(store.recall("com.one").await.is_empty());
        assert_eq!(store.recall("com.two").await.len(), 1);
    }

    #[test]
    fn the_first_screen_is_described_by_its_places_to_go() {
        let elements = vec![
            element(1, "Search", true),
            element(2, "a label that is not clickable", false),
            element(3, "Home", true),
            element(4, "home", true),
            element(5, "Profile", true),
        ];
        assert_eq!(home_facts(&elements), vec!["Search", "Home", "Profile"]);
    }

    #[test]
    fn a_first_screen_is_capped() {
        let elements: Vec<UiElement> = (1..=20)
            .map(|i| element(i, &format!("Tab {i}"), true))
            .collect();
        assert_eq!(home_facts(&elements).len(), HOME_LABELS);
    }

    #[test]
    fn a_home_that_kept_most_of_its_places_still_matches() {
        let recorded = vec![
            "Inbox".to_string(),
            "Search".to_string(),
            "Profile".to_string(),
        ];
        let after = vec![element(1, "Inbox", true), element(2, "Search", true)];
        assert!(home_matches(&recorded, &after), "two of three is a match");

        let different = vec![element(1, "Settings", true), element(2, "About", true)];
        assert!(!home_matches(&recorded, &different));
    }

    #[test]
    fn nothing_recorded_matches_nothing() {
        assert!(!home_matches(&[], &[element(1, "Inbox", true)]));
    }

    #[tokio::test]
    async fn a_screen_only_carries_knowledge_that_screen_supports() {
        let store = AppKnowledge::in_memory();
        store
            .record(
                "com.one",
                FactKind::Home,
                "offers Inbox, Search, Profile",
                vec!["Inbox".into(), "Search".into(), "Profile".into()],
            )
            .await;
        store
            .record("com.one", FactKind::LaunchEntry, "com.one/.Main", Vec::new())
            .await;

        let screen = vec![element(1, "Inbox", true), element(2, "Search", true)];
        let recalled = store.for_screen("com.one", &screen).await;
        assert_eq!(recalled.len(), 1, "{recalled:?}");
        assert_eq!(recalled[0].kind, FactKind::Home, "the launch entry is about the launch");

        let elsewhere = vec![element(1, "Settings", true), element(2, "About", true)];
        assert!(
            store.for_screen("com.one", &elsewhere).await.is_empty(),
            "a screen that does not match says nothing"
        );
    }

    #[tokio::test]
    async fn a_dialog_is_named_only_while_it_is_on_screen() {
        let store = AppKnowledge::in_memory();
        store
            .record("com.one", FactKind::Dialog, "An update is available", Vec::new())
            .await;

        let with_dialog = vec![
            dialog_root(1),
            element(2, "An update is available", false),
            element(3, "Later", true),
        ];
        let recalled = store.for_screen("com.one", &with_dialog).await;
        assert_eq!(recalled.len(), 1, "{recalled:?}");

        assert!(
            store
                .for_screen("com.one", &[element(1, "Inbox", true)])
                .await
                .is_empty(),
            "no dialog on screen means no dialog fact"
        );
    }

    #[tokio::test]
    async fn a_note_that_was_just_said_is_not_said_again() {
        let store = AppKnowledge::in_memory();
        store
            .record("com.one", FactKind::Home, "offers Inbox", vec!["Inbox".to_string()])
            .await;
        let screen = vec![element(1, "Inbox", true)];

        let first = store.for_screen("com.one", &screen).await;
        assert_eq!(first.len(), 1);
        store.mark_shown(&first).await;
        assert!(
            store.for_screen("com.one", &screen).await.is_empty(),
            "the same sentence twice in a row is noise"
        );
    }

    #[test]
    fn a_dialog_is_described_by_its_message() {
        let elements = vec![
            dialog_root(1),
            element(2, "An update is available", false),
            element(3, "Later", true),
        ];
        assert_eq!(first_dialog(&elements).as_deref(), Some("An update is available"));
    }

    #[test]
    fn a_screen_with_no_dialog_has_none() {
        let elements = vec![element(1, "Inbox", true), element(2, "Search", true)];
        assert!(first_dialog(&elements).is_none());
    }

    #[test]
    fn the_note_names_the_facts_and_their_kinds() {
        let facts = vec![
            FactRecall {
                package: "com.chat".into(),
                kind: FactKind::LaunchEntry,
                text: "com.chat/.ui.Home".into(),
                labels: Vec::new(),
            },
            FactRecall {
                package: "com.chat".into(),
                kind: FactKind::Dialog,
                text: "asks to update".into(),
                labels: Vec::new(),
            },
        ];
        let note = describe(&facts).expect("two facts make a note");
        assert!(note.contains("com.chat"), "{note}");
        assert!(note.contains("launch entry: com.chat/.ui.Home"), "{note}");
        assert!(note.contains("dialog after launch: asks to update"), "{note}");
    }

    #[test]
    fn an_empty_recall_adds_no_note_at_all() {
        assert!(describe(&[]).is_none());
    }
}
