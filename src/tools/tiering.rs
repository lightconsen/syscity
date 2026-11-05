//! Two-tier tool dispatch: the tools always offered, and the ones that wait
//! to be asked for.
//!
//! A toolset that offers everything it can do spends prompt space on answers
//! nobody asked for. The Android set has twenty-four tools; a fresh turn needs
//! six of them, and the other eighteen describe capabilities the model has not
//! decided to use yet. So the set declares a core, and groups the rest — and a
//! group becomes visible when someone opens it (see
//! [`crate::tools::list_capabilities`]).
//!
//! Tiering is a visibility measure, not a fence. A tool in a closed group is
//! still registered and still runs if it is called by name: hiding a tool from
//! the advertisement must never be the thing standing between an approved
//! action and its execution, and a model that remembers a name from an earlier
//! turn should not have to re-discover it. What a closed group saves is the
//! schema in every request.
//!
//! The state is shared, not per-conversation: the phone is one device, and a
//! group opened in one turn is a fact about this host, not about that
//! conversation.

use std::collections::HashSet;

use serde::Serialize;

/// One optional group of tools, as the registry knows it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ToolGroup {
    /// The set the tools come from, e.g. `"android"`.
    pub set_id: String,
    /// The key a caller names to open it, e.g. `"android.read"`.
    pub id: String,
    /// One line, for the catalog.
    pub about: String,
    /// The tool names in the group.
    pub tools: Vec<String>,
    /// Whether the group has been opened.
    pub open: bool,
}

#[derive(Debug, Default)]
struct TierState {
    groups: Vec<ToolGroup>,
    /// Members of the groups that are still closed.
    dormant: HashSet<String>,
}

impl TierState {
    /// Recompute which tools are hidden. Called only when the group table or an
    /// open flag changes, which is rare, so the hot read path stays a set
    /// lookup.
    fn rebuild(&mut self) {
        self.dormant = self
            .groups
            .iter()
            .filter(|group| !group.open)
            .flat_map(|group| group.tools.iter().cloned())
            .collect();
        // A tool that is also offered always is not hidden, even if a group
        // names it: the core list is the set's own statement about what must
        // be available, and it wins.
        self.dormant.retain(|name| {
            !self
                .groups
                .iter()
                .any(|group| group.open && group.tools.contains(name))
        });
    }
}

/// The registry's view of which tools wait behind a closed group.
#[derive(Debug, Default)]
pub struct ToolTiering {
    state: std::sync::RwLock<TierState>,
}

impl ToolTiering {
    /// A tiering that hides nothing — every tool is offered, which is what a
    /// registry with no platform sets (or a test) should do.
    pub fn new() -> Self {
        Self::default()
    }

    /// Replace the group table. Every group starts closed.
    ///
    /// Called once, after the platform sets have been exported, because the
    /// groups are a property of the sets.
    pub fn declare(&self, mut groups: Vec<ToolGroup>) {
        for group in &mut groups {
            group.open = false;
        }
        if let Ok(mut state) = self.state.write() {
            state.groups = groups;
            state.rebuild();
        }
    }

    /// Whether a tool is waiting in a closed group.
    pub fn is_dormant(&self, name: &str) -> bool {
        self.state
            .read()
            .map(|state| state.dormant.contains(name))
            .unwrap_or(false)
    }

    /// Open a group, returning the names it made visible.
    ///
    /// `None` when no group has that id — which the caller reports rather than
    /// treating as success, since "I opened it" and "there is no such group"
    /// are the two outcomes a model can act on.
    pub fn open(&self, id: &str) -> Option<Vec<String>> {
        let mut state = self.state.write().ok()?;
        let group = state.groups.iter_mut().find(|group| group.id == id)?;
        group.open = true;
        let tools = group.tools.clone();
        state.rebuild();
        Some(tools)
    }

    /// The group table, in declaration order.
    pub fn groups(&self) -> Vec<ToolGroup> {
        self.state
            .read()
            .map(|state| state.groups.clone())
            .unwrap_or_default()
    }

    /// The groups belonging to one set.
    pub fn groups_of(&self, set_id: &str) -> Vec<ToolGroup> {
        self.groups()
            .into_iter()
            .filter(|group| group.set_id == set_id)
            .collect()
    }
}

/// Present a registry's groups as the catalog's rows.
///
/// The two shapes differ on purpose: [`ToolGroup`] is the registry's, and
/// [`crate::tools::list_capabilities::GroupInfo`] is what a caller reads, so a
/// change to one does not silently change the other's wire format.
pub fn groups_for(groups: &[ToolGroup]) -> Vec<crate::tools::list_capabilities::GroupInfo> {
    groups
        .iter()
        .map(|group| crate::tools::list_capabilities::GroupInfo {
            id: group.id.clone(),
            about: group.about.clone(),
            tools: group.tools.clone(),
            open: group.open,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn group(id: &str, tools: &[&str]) -> ToolGroup {
        ToolGroup {
            set_id: "android".to_string(),
            id: id.to_string(),
            about: format!("{id} about"),
            tools: tools.iter().map(|name| name.to_string()).collect(),
            open: false,
        }
    }

    #[test]
    fn a_declared_group_keeps_its_tools_out_of_the_advertisement() {
        let tiers = ToolTiering::new();
        tiers.declare(vec![group(
            "android.read",
            &["android_ui_tree", "android_verify"],
        )]);

        assert!(tiers.is_dormant("android_ui_tree"));
        assert!(tiers.is_dormant("android_verify"));
        assert!(!tiers.is_dormant("android_observe"));
    }

    #[test]
    fn opening_a_group_makes_exactly_its_tools_visible() {
        let tiers = ToolTiering::new();
        tiers.declare(vec![
            group("android.read", &["android_ui_tree"]),
            group("android.diagnose", &["android_ocr"]),
        ]);

        let opened = tiers.open("android.read").expect("group exists");
        assert_eq!(opened, vec!["android_ui_tree".to_string()]);
        assert!(!tiers.is_dormant("android_ui_tree"));
        assert!(tiers.is_dormant("android_ocr"), "the other group stays closed");
    }

    #[test]
    fn opening_an_unknown_group_is_reported_not_assumed() {
        let tiers = ToolTiering::new();
        tiers.declare(vec![group("android.read", &["android_ui_tree"])]);
        assert!(tiers.open("android.nope").is_none());
    }

    #[test]
    fn declaring_again_starts_every_group_closed() {
        let tiers = ToolTiering::new();
        tiers.declare(vec![group("android.read", &["android_ui_tree"])]);
        tiers.open("android.read");
        assert!(!tiers.is_dormant("android_ui_tree"));

        tiers.declare(vec![group("android.read", &["android_ui_tree"])]);
        assert!(tiers.is_dormant("android_ui_tree"), "a fresh declaration closes again");
    }

    #[test]
    fn a_tool_named_by_an_open_group_is_not_hidden_by_a_closed_one() {
        // Overlap is a declaration mistake, but the consequence of it must be
        // the safe one: a tool someone made visible stays visible.
        let tiers = ToolTiering::new();
        tiers.declare(vec![
            group("android.core", &["android_observe"]),
            group("android.extra", &["android_observe"]),
        ]);
        tiers.open("android.core");
        assert!(!tiers.is_dormant("android_observe"));
    }

    #[test]
    fn groups_are_reported_with_their_open_state() {
        let tiers = ToolTiering::new();
        tiers.declare(vec![
            group("android.read", &["android_ui_tree"]),
            group("android.web", &["android_web"]),
        ]);
        tiers.open("android.web");

        let groups = tiers.groups_of("android");
        assert_eq!(groups.len(), 2);
        assert!(!groups[0].open);
        assert!(groups[1].open);
        assert!(tiers.groups_of("ios").is_empty());
    }

    #[test]
    fn nothing_is_hidden_before_anything_is_declared() {
        let tiers = ToolTiering::new();
        assert!(!tiers.is_dormant("android_ocr"));
    }
}
