//! Platform tool set registry — manages platform-specific tool collections.

use std::collections::{HashMap, HashSet};

use super::{OsControlScope, PlatformToolSet, ToolConflictStrategy};
use crate::tools::ToolRegistry;

/// Registry of all platform tool sets, with runtime environment detection.
///
/// Tool lists are allocated by `set.tools()` on each export call. The
/// `Tool` trait does not currently implement `Clone`, so caching tool
/// lists would require a larger refactor. Since export typically happens
/// once at startup, the allocation cost is acceptable.
pub struct PlatformCapabilityRegistry {
    sets: Vec<Box<dyn PlatformToolSet>>,
    disabled: HashSet<String>,
    /// Environment availability cache. Uses `std::sync::RwLock` — the
    /// critical section is a brief HashMap lookup/insert and is never
    /// held across an `.await` point, so it is safe in async contexts.
    availability_cache: std::sync::RwLock<HashMap<String, bool>>,
}

impl Default for PlatformCapabilityRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl PlatformCapabilityRegistry {
    /// Create an empty registry.
    pub fn new() -> Self {
        Self {
            sets: Vec::new(),
            disabled: HashSet::new(),
            availability_cache: std::sync::RwLock::new(HashMap::new()),
        }
    }

    /// Register a platform tool set.
    pub fn register(&mut self, set: Box<dyn PlatformToolSet>) {
        self.sets.push(set);
        self.refresh_cache();
    }

    /// Disable a set by ID.
    pub fn disable(&mut self, set_id: &str) {
        self.disabled.insert(set_id.to_string());
    }

    /// Re-enable a previously disabled set.
    pub fn enable(&mut self, set_id: &str) {
        self.disabled.remove(set_id);
    }

    /// Whether a set is explicitly disabled.
    pub fn is_disabled(&self, set_id: &str) -> bool {
        self.disabled.contains(set_id)
    }

    /// Disable every set that needs a GUI on *this* machine, returning the ids
    /// it disabled.
    ///
    /// Used when the desktop the agent should act on is somewhere else: these
    /// sets are bound to the local display — AppleScript runs on this host,
    /// `*_desktop_control` takes over the local pointer — so leaving them
    /// offered does not give the model a second way to reach the task, it gives
    /// it a wrong one that looks right. Sets that need no GUI (a server's
    /// processes, a phone over adb) are unaffected: they do not act on the host
    /// display, so a remote desktop is no reason to withhold them.
    pub fn disable_gui_sets(&mut self) -> Vec<String> {
        let ids: Vec<String> = self
            .sets
            .iter()
            .filter(|s| s.constraints().requires_gui)
            .map(|s| s.id().to_string())
            .collect();
        for id in &ids {
            self.disabled.insert(id.clone());
        }
        ids
    }

    /// All registered sets (regardless of availability).
    pub fn all_sets(&self) -> Vec<&dyn PlatformToolSet> {
        self.sets.iter().map(|s| s.as_ref()).collect()
    }

    /// Sets that pass environment checks and are not disabled.
    pub fn available_sets(&self) -> Vec<&dyn PlatformToolSet> {
        self.sets
            .iter()
            .filter(|s| !self.disabled.contains(s.id()))
            .filter(|s| self.check_availability(s.id(), s.as_ref()))
            .map(|s| s.as_ref())
            .collect()
    }

    /// Export tools from all available sets into a `ToolRegistry`, and declare
    /// the groups each set holds back.
    ///
    /// The two go together because the groups name tools that only exist once
    /// the set is exported: declaring with nothing exported would hide nothing,
    /// and exporting without declaring would offer everything, which is what
    /// this replaces. A tool in a closed group is still callable by name; what
    /// is withheld is its schema in the advertisement.
    pub fn export_to_tool_registry(
        &self,
        registry: &mut ToolRegistry,
        strategy: ToolConflictStrategy,
    ) {
        let sets = self.available_sets();
        Self::export_sets(&sets, registry, strategy);
        registry.declare_tiers(&sets);
    }

    fn export_sets(
        sets: &[&dyn PlatformToolSet],
        registry: &mut ToolRegistry,
        strategy: ToolConflictStrategy,
    ) {
        let mut seen = HashSet::new();
        for set in sets {
            for tool in set.tools() {
                let name = tool.name().to_string();
                if seen.contains(&name) {
                    match strategy {
                        ToolConflictStrategy::Reject => {
                            panic!("Tool '{}' conflicts between platform tool sets", name);
                        }
                        ToolConflictStrategy::Override => {
                            registry.register(tool);
                        }
                    }
                } else {
                    seen.insert(name.clone());
                    registry.register(tool);
                }
            }
        }
    }

    /// Export only sets within a max permission scope.
    pub fn export_with_scope(
        &self,
        registry: &mut ToolRegistry,
        max_scope: OsControlScope,
        strategy: ToolConflictStrategy,
    ) {
        let sets: Vec<&dyn PlatformToolSet> = self
            .available_sets()
            .into_iter()
            .filter(|set| set.scope() <= max_scope)
            .collect();
        Self::export_sets(&sets, registry, strategy);
        registry.declare_tiers(&sets);
    }

    /// Clear availability cache.
    pub fn refresh_cache(&self) {
        if let Ok(mut cache) = self.availability_cache.write() {
            cache.clear();
        }
    }

    fn check_availability(&self, id: &str, set: &dyn PlatformToolSet) -> bool {
        if let Ok(cache) = self.availability_cache.read() {
            if let Some(cached) = cache.get(id) {
                return *cached;
            }
        }
        let available = set.is_available();
        if let Ok(mut cache) = self.availability_cache.write() {
            cache.insert(id.to_string(), available);
        }
        available
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::computer::platform::{PlatformConstraints, PlatformToolSet};

    /// A set whose only interesting property is whether it needs a GUI.
    struct StubSet {
        id: &'static str,
        gui: bool,
    }

    impl PlatformToolSet for StubSet {
        fn id(&self) -> &str {
            self.id
        }

        fn name(&self) -> &str {
            self.id
        }

        fn description(&self) -> &str {
            "test stub"
        }

        fn constraints(&self) -> &PlatformConstraints {
            // Leak a tiny boxed value per stub: tests only, and `constraints`
            // has to hand back a reference that outlives the call.
            Box::leak(Box::new(PlatformConstraints {
                requires_gui: self.gui,
                ..Default::default()
            }))
        }

        fn tools(&self) -> Vec<Box<dyn crate::tools::Tool>> {
            Vec::new()
        }

        fn is_available(&self) -> bool {
            true
        }
    }

    #[test]
    fn disable_gui_sets_withholds_only_gui_sets() {
        let mut reg = PlatformCapabilityRegistry::new();
        reg.register(Box::new(StubSet { id: "x11", gui: true }));
        reg.register(Box::new(StubSet { id: "macos", gui: true }));
        reg.register(Box::new(StubSet { id: "linux", gui: false }));

        let mut disabled = reg.disable_gui_sets();
        disabled.sort();

        assert_eq!(disabled, vec!["macos".to_string(), "x11".to_string()]);
        assert!(reg.is_disabled("x11"));
        assert!(reg.is_disabled("macos"));
        // A server set acts on processes, not the display; a remote desktop is
        // no reason to take it away.
        assert!(!reg.is_disabled("linux"));

        let names: Vec<&str> = reg.available_sets().iter().map(|s| s.id()).collect();
        assert_eq!(names, vec!["linux"]);
    }
}
