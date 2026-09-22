//! Tool SDK Extension Layer
//!
//! The `ToolSdk` is the primary external-facing API for tool discovery,
//! registration, and capability querying. It provides:
//!
//! - **Pack management**: Group related tools into versioned packs
//! - **Discovery**: List, find, and query tools by name, capability, or
//!   category
//! - **Metadata**: Access tool parameter schemas, descriptions, and risk levels
//! - **Integration**: Bidirectional sync with the core `ToolRegistry`

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::tools::approval::RiskLevel;
use crate::tools::ToolRegistry;

/// Errors from ToolSdk operations.
#[derive(Debug, Clone)]
pub enum ToolSdkError {
    /// Tool not found in any registered pack or registry.
    ToolNotFound(String),
    /// Pack not found.
    PackNotFound(String),
    /// Tool registry not available (not connected).
    RegistryNotAvailable,
    /// Validation error.
    ValidationError(String),
}

impl std::fmt::Display for ToolSdkError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ToolSdkError::ToolNotFound(name) => write!(f, "tool '{}' not found", name),
            ToolSdkError::PackNotFound(name) => write!(f, "pack '{}' not found", name),
            ToolSdkError::RegistryNotAvailable => write!(f, "tool registry not connected"),
            ToolSdkError::ValidationError(msg) => write!(f, "validation error: {}", msg),
        }
    }
}

impl std::error::Error for ToolSdkError {}

/// Capabilities advertised by a tool.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCapabilities {
    /// Whether this tool requires human approval.
    pub requires_approval: bool,
    /// Whether this tool can run in a sandbox.
    pub sandboxed: bool,
    /// Whether this tool supports streaming output.
    pub streaming: bool,
    /// Risk level classification.
    pub risk_level: RiskLevel,
    /// Categories (e.g. ["file", "system", "network"]).
    pub categories: Vec<String>,
    /// Whether running this call twice has the same effect as running it once.
    ///
    /// The default is `false` — *assume a retry can duplicate the effect* —
    /// because the alternative direction is the dangerous one: a tool that
    /// unsends nothing, spends money twice or posts twice is worse than a
    /// caller being needlessly careful. Tools that are genuinely safe to repeat
    /// (reads, whole-snapshot writes) say so here, and the timeout path quotes
    /// this field back when a call's outcome is unknown.
    pub idempotent: bool,
    /// How this tool's effect can be undone, if it can.
    ///
    /// `None` means "nothing to compensate with", which is the honest answer
    /// for sending a message or running a shell command. Where a compensating
    /// action exists it is named for the caller: the agent has no rollback
    /// machinery for side effects, and this field exists so nothing pretends
    /// it does.
    pub compensation: Option<&'static str>,
    /// True when this call only observes state and never mutates anything.
    ///
    /// The default is the careful `false`: an unclassified tool is never
    /// treated as read-only, so plan mode (which admits only read-only
    /// tools) hides and refuses it rather than exposing a possible write.
    pub read_only: bool,
}

impl ToolCapabilities {
    /// Declare this tool read-only. Chain on a `Default` or an existing
    /// `ToolCapabilities`: `.read_only` can also be set directly.
    pub fn mark_read_only(mut self) -> Self {
        self.read_only = true;
        self
    }
}

impl Default for ToolCapabilities {
    fn default() -> Self {
        Self {
            requires_approval: false,
            sandboxed: false,
            streaming: false,
            risk_level: RiskLevel::Low,
            categories: vec![],
            idempotent: false,
            compensation: None,
            read_only: false,
        }
    }
}

/// Metadata for a registered tool.
#[derive(Debug, Clone)]
pub struct ToolMetadata {
    pub name: String,
    pub description: String,
    pub capabilities: ToolCapabilities,
    pub parameter_schema: serde_json::Value,
}

/// A "tool pack" — a bundle of related tools shipped as a unit.
pub struct ToolPack {
    pub name: String,
    pub version: String,
    pub description: String,
    pub tools: Vec<String>,
}

/// Filter for querying tools by capability.
#[derive(Debug, Default)]
pub struct CapabilityFilter {
    pub requires_approval: Option<bool>,
    pub sandboxed: Option<bool>,
    pub streaming: Option<bool>,
    pub min_risk_level: Option<RiskLevel>,
    pub max_risk_level: Option<RiskLevel>,
    pub categories: Option<Vec<String>>,
}

/// Result of a sync operation.
#[derive(Debug, Default)]
pub struct SyncResult {
    pub tools_added: Vec<String>,
    pub tools_removed: Vec<String>,
    pub packs_updated: Vec<String>,
}

/// Tool SDK — entry point for dynamic tool registration and discovery.
pub struct ToolSdk {
    packs: HashMap<String, ToolPack>,
    /// Optional reference to the global ToolRegistry for metadata queries.
    registry: Option<Arc<ToolRegistry>>,
}

impl ToolSdk {
    pub fn new() -> Self {
        Self {
            packs: HashMap::new(),
            registry: None,
        }
    }

    /// Connect this SDK to a ToolRegistry for metadata queries and sync.
    pub fn with_registry(mut self, registry: Arc<ToolRegistry>) -> Self {
        self.registry = Some(registry);
        self
    }

    pub fn register_pack(&mut self, pack: ToolPack) {
        self.packs.insert(pack.name.clone(), pack);
    }

    pub fn list_packs(&self) -> Vec<&ToolPack> {
        self.packs.values().collect()
    }

    pub fn get_pack(&self, name: &str) -> Option<&ToolPack> {
        self.packs.get(name)
    }

    /// List all tools from packs and registry (if connected).
    pub fn list_tools(&self) -> Vec<String> {
        let mut tools: Vec<String> = self.packs.values().flat_map(|p| p.tools.clone()).collect();
        if let Some(ref reg) = self.registry {
            for name in reg.list() {
                if !tools.contains(&name) {
                    tools.push(name);
                }
            }
        }
        tools.sort();
        tools
    }

    /// Get metadata for a named tool.
    pub fn get_tool_metadata(&self, name: &str) -> Result<ToolMetadata, ToolSdkError> {
        // Query capabilities from registry if available (covers both
        // registry-registered tools and pack tools that happen to be
        // registered in the registry).
        if let Some((desc, schema, caps)) = self.registry.as_ref().and_then(|reg| {
            let tool = reg.get(name)?;
            Some((tool.description().to_string(), tool.parameters_schema(), tool.capabilities()))
        }) {
            return Ok(ToolMetadata {
                name: name.to_string(),
                description: desc,
                capabilities: caps,
                parameter_schema: schema,
            });
        }

        // Fallback: check packs for metadata
        for pack in self.packs.values() {
            if pack.tools.contains(&name.to_string()) {
                return Ok(ToolMetadata {
                    name: name.to_string(),
                    description: pack.description.clone(),
                    capabilities: ToolCapabilities::default(),
                    parameter_schema: serde_json::json!({"type": "object"}),
                });
            }
        }

        Err(ToolSdkError::ToolNotFound(name.to_string()))
    }

    /// Get the JSON schema for a specific tool's parameters.
    pub fn get_tool_parameter_schema(&self, name: &str) -> Result<serde_json::Value, ToolSdkError> {
        if let Some(ref reg) = self.registry {
            if let Some(tool) = reg.get(name) {
                return Ok(tool.parameters_schema());
            }
        }
        // Fallback to pack metadata
        for pack in self.packs.values() {
            if pack.tools.contains(&name.to_string()) {
                return Ok(serde_json::json!({"type": "object"}));
            }
        }
        Err(ToolSdkError::ToolNotFound(name.to_string()))
    }

    /// Find tools that match all specified capability criteria.
    pub fn find_by_capability(&self, filter: &CapabilityFilter) -> Vec<String> {
        let all = self.list_tools();
        let mut results = Vec::new();

        for name in all {
            let meta = match self.get_tool_metadata(&name) {
                Ok(m) => m,
                Err(_) => continue,
            };
            let caps = &meta.capabilities;

            if let Some(req) = filter.requires_approval {
                if caps.requires_approval != req {
                    continue;
                }
            }
            if let Some(s) = filter.sandboxed {
                if caps.sandboxed != s {
                    continue;
                }
            }
            if let Some(s) = filter.streaming {
                if caps.streaming != s {
                    continue;
                }
            }
            if let Some(min_risk) = filter.min_risk_level {
                if caps.risk_level < min_risk {
                    continue;
                }
            }
            if let Some(max_risk) = filter.max_risk_level {
                if caps.risk_level > max_risk {
                    continue;
                }
            }
            if let Some(ref cats) = filter.categories {
                let caps_cats: HashSet<&String> = caps.categories.iter().collect();
                let filter_cats: HashSet<&String> = cats.iter().collect();
                if !filter_cats.is_subset(&caps_cats) {
                    continue;
                }
            }

            results.push(name);
        }

        results
    }

    /// Synchronize tool packs from the ToolRegistry.
    ///
    /// Removes stale tools, adds new tools, and returns a sync report.
    pub fn sync_from_tool_registry(&mut self, tool_registry: &ToolRegistry) -> SyncResult {
        let registry_tools: HashSet<String> = tool_registry.list().into_iter().collect();
        let mut result = SyncResult::default();

        // Remove tools from packs that are no longer in the registry
        for pack in self.packs.values_mut() {
            let before = pack.tools.len();
            pack.tools.retain(|t| registry_tools.contains(t));
            if pack.tools.len() < before {
                result.tools_removed.push(pack.name.clone());
                result.packs_updated.push(pack.name.clone());
            }
        }

        // Add new tools not in any pack
        let all_pack_tools: HashSet<String> =
            self.packs.values().flat_map(|p| p.tools.clone()).collect();
        let new_tools: Vec<String> = registry_tools
            .difference(&all_pack_tools)
            .cloned()
            .collect();

        if !new_tools.is_empty() {
            if let Some(default_pack) = self.packs.get_mut("default_tools") {
                default_pack.tools.extend(new_tools.clone());
                result.tools_added = new_tools;
                result.packs_updated.push("default_tools".to_string());
            } else {
                self.register_pack(ToolPack {
                    name: "default_tools".to_string(),
                    version: "1.0".to_string(),
                    description: "Tools synchronized from ToolRegistry".to_string(),
                    tools: new_tools.clone(),
                });
                result.tools_added = new_tools;
                result.packs_updated.push("default_tools".to_string());
            }
        }

        result
    }
}

impl Default for ToolSdk {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::{BoxedTool, Tool};

    // ── Test stub tool ──────────────────────────────────────────────────
    struct StubTool {
        name: String,
    }

    impl StubTool {
        fn new(name: &str) -> Self {
            Self { name: name.to_string() }
        }
    }

    #[async_trait::async_trait]
    impl Tool for StubTool {
        fn name(&self) -> &str {
            &self.name
        }
        fn description(&self) -> &str {
            "stub"
        }
        fn parameters_schema(&self) -> serde_json::Value {
            serde_json::json!({"type": "object"})
        }
        async fn execute(
            &self,
            _args: serde_json::Value,
            _context: &crate::tools::ToolContext,
        ) -> crate::Result<crate::tools::ToolExecutionResult> {
            Ok(crate::tools::ToolExecutionResult::success("ok"))
        }
    }

    fn stub_tool(name: &str) -> BoxedTool {
        Box::new(StubTool::new(name))
    }

    #[test]
    fn test_tool_capabilities_default() {
        let caps = ToolCapabilities::default();
        assert!(!caps.requires_approval);
        assert!(!caps.sandboxed);
    }

    #[test]
    fn test_tool_pack() {
        let mut sdk = ToolSdk::new();
        sdk.register_pack(ToolPack {
            name: "filesystem".to_string(),
            version: "1.0".to_string(),
            description: "File and directory operations".to_string(),
            tools: vec!["read_file".to_string(), "write_file".to_string()],
        });
        assert_eq!(sdk.list_packs().len(), 1);
        assert!(sdk.get_pack("filesystem").is_some());
    }

    #[test]
    fn test_tool_sdk_default() {
        let sdk: ToolSdk = Default::default();
        assert!(sdk.list_packs().is_empty());
    }

    #[test]
    fn test_list_tools_empty() {
        let sdk = ToolSdk::new();
        assert!(sdk.list_tools().is_empty());
    }

    #[test]
    fn test_list_tools_with_packs() {
        let mut sdk = ToolSdk::new();
        sdk.register_pack(ToolPack {
            name: "web".to_string(),
            version: "1.0".to_string(),
            description: "Web tools".to_string(),
            tools: vec!["fetch".to_string(), "search".to_string()],
        });
        let tools = sdk.list_tools();
        assert_eq!(tools.len(), 2);
        assert!(tools.contains(&"fetch".to_string()));
    }

    #[test]
    fn test_get_tool_metadata_not_found() {
        let sdk = ToolSdk::new();
        let result = sdk.get_tool_metadata("nonexistent");
        assert!(result.is_err());
        assert!(matches!(result.unwrap_err(), ToolSdkError::ToolNotFound(_)));
    }

    #[test]
    fn test_get_tool_metadata_from_pack() {
        let mut sdk = ToolSdk::new();
        sdk.register_pack(ToolPack {
            name: "test".to_string(),
            version: "1.0".to_string(),
            description: "Test pack".to_string(),
            tools: vec!["my-tool".to_string()],
        });
        let meta = sdk.get_tool_metadata("my-tool").unwrap();
        assert_eq!(meta.name, "my-tool");
    }

    #[test]
    fn test_get_tool_parameter_schema_not_found() {
        let sdk = ToolSdk::new();
        let result = sdk.get_tool_parameter_schema("nonexistent");
        assert!(result.is_err());
    }

    #[test]
    fn test_sync_from_tool_registry_empty() {
        let mut sdk = ToolSdk::new();
        let registry = ToolRegistry::new();
        let result = sdk.sync_from_tool_registry(&registry);
        assert!(result.tools_added.is_empty());
        assert!(result.tools_removed.is_empty());
    }

    #[test]
    fn test_find_by_capability_empty() {
        let sdk = ToolSdk::new();
        let results = sdk.find_by_capability(&CapabilityFilter::default());
        assert!(results.is_empty());
    }

    #[test]
    fn test_tool_sdk_error_display() {
        let err = ToolSdkError::ToolNotFound("foo".into());
        assert!(err.to_string().contains("foo"));
        let err2 = ToolSdkError::PackNotFound("bar".into());
        assert!(err2.to_string().contains("bar"));
        let err3 = ToolSdkError::RegistryNotAvailable;
        assert!(err3.to_string().contains("not connected"));
    }

    #[test]
    fn test_tool_capabilities_serialization() {
        let caps = ToolCapabilities {
            requires_approval: true,
            sandboxed: true,
            streaming: false,
            risk_level: RiskLevel::High,
            categories: vec!["file".to_string(), "system".to_string()],
            idempotent: false,
            compensation: Some("delete the file it wrote"),
            read_only: false,
        };
        let json = serde_json::to_string(&caps).unwrap();
        assert!(json.contains("requires_approval"));
        assert!(
            json.contains("delete the file it wrote"),
            "the declaration travels with the capabilities: {json}"
        );
    }

    #[test]
    fn test_tool_metadata_creation() {
        let meta = ToolMetadata {
            name: "grep".to_string(),
            description: "Search files".to_string(),
            capabilities: ToolCapabilities::default(),
            parameter_schema: serde_json::json!({"type": "object"}),
        };
        assert_eq!(meta.name, "grep");
    }

    #[test]
    fn test_sync_result_default() {
        let result = SyncResult::default();
        assert!(result.tools_added.is_empty());
        assert!(result.tools_removed.is_empty());
        assert!(result.packs_updated.is_empty());
    }

    #[test]
    fn test_find_by_capability_filter_risk() {
        let mut sdk = ToolSdk::new();
        sdk.register_pack(ToolPack {
            name: "risky".to_string(),
            version: "1.0".to_string(),
            description: "Risky tools".to_string(),
            tools: vec!["danger".to_string()],
        });
        // With default ToolCapabilities (Low risk), the filter should find the tool
        let filter = CapabilityFilter {
            min_risk_level: Some(RiskLevel::Low),
            ..Default::default()
        };
        let results = sdk.find_by_capability(&filter);
        // The tool should appear since the default risk is Low
        assert_eq!(results.len(), 1);
    }

    #[test]
    fn test_sync_from_tool_registry_adds_new_tools() {
        let mut reg = ToolRegistry::new();
        reg.register(stub_tool("my-tool"));

        let mut sdk = ToolSdk::new();
        let result = sdk.sync_from_tool_registry(&reg);

        assert!(result.tools_added.contains(&"my-tool".to_string()));
        assert!(result.packs_updated.contains(&"default_tools".to_string()));
        let pack = sdk.get_pack("default_tools");
        assert!(pack.is_some());
        assert!(pack.unwrap().tools.contains(&"my-tool".to_string()));
    }

    #[test]
    fn test_sync_from_tool_registry_removes_stale_tools() {
        let _reg = ToolRegistry::new();

        let mut sdk = ToolSdk::new();
        sdk.register_pack(ToolPack {
            name: "stale".to_string(),
            version: "1.0".to_string(),
            description: "Has stale tools".to_string(),
            tools: vec!["gone".to_string(), "present".to_string()],
        });

        // Register "present" in the registry so it's not stale
        let mut reg2 = ToolRegistry::new();
        reg2.register(stub_tool("present"));
        let result = sdk.sync_from_tool_registry(&reg2);

        assert!(result.tools_removed.contains(&"stale".to_string()));
        // Verify the stale tool was removed from the pack
        let pack = sdk.get_pack("stale").unwrap();
        assert!(!pack.tools.contains(&"gone".to_string()));
        assert!(pack.tools.contains(&"present".to_string()));
    }

    #[test]
    fn test_find_by_capability_category_filter() {
        let mut sdk = ToolSdk::new();
        // This won't match without a registry, but we can test the filter
        // by adding to a pack with default (empty) capabilities
        sdk.register_pack(ToolPack {
            name: "tools".to_string(),
            version: "1.0".to_string(),
            description: "Tools".to_string(),
            tools: vec!["no-category".to_string()],
        });

        // Filter for "network" category — should not match any pack tool
        let filter = CapabilityFilter {
            categories: Some(vec!["network".to_string()]),
            ..Default::default()
        };
        // No tools have "network" category (pack tools use default capabilities)
        let results = sdk.find_by_capability(&filter);
        assert!(results.is_empty());
    }

    #[test]
    fn test_with_registry_metadata_lookup() {
        let mut reg = ToolRegistry::new();
        reg.register(stub_tool("alpha"));

        let sdk = ToolSdk::new().with_registry(std::sync::Arc::new(reg));

        // Tool should be found via registry
        let meta = sdk.get_tool_metadata("alpha").unwrap();
        assert_eq!(meta.name, "alpha");
        assert_eq!(meta.description, "stub");
    }

    #[test]
    fn test_get_tool_parameter_schema_from_registry() {
        let mut reg = ToolRegistry::new();
        reg.register(stub_tool("params-tool"));

        let sdk = ToolSdk::new().with_registry(std::sync::Arc::new(reg));

        let schema = sdk.get_tool_parameter_schema("params-tool").unwrap();
        assert_eq!(schema, serde_json::json!({"type": "object"}));
    }

    #[test]
    fn test_sync_default_pack_reused() {
        let mut reg = ToolRegistry::new();
        reg.register(stub_tool("tool-a"));
        reg.register(stub_tool("tool-b"));

        let mut sdk = ToolSdk::new();
        sdk.register_pack(ToolPack {
            name: "default_tools".to_string(),
            version: "1.0".to_string(),
            description: "Existing default pack".to_string(),
            tools: vec![],
        });

        let result = sdk.sync_from_tool_registry(&reg);
        assert_eq!(result.tools_added.len(), 2);
        assert!(result.tools_added.contains(&"tool-a".to_string()));

        let pack = sdk.get_pack("default_tools").unwrap();
        assert_eq!(pack.tools.len(), 2);
    }
}
