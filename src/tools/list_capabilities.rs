//! List capabilities tool — report what OS-specific capabilities are available,
//! and open the ones a turn needs.

use async_trait::async_trait;
use serde::Serialize;
use serde_json::Value;

use crate::tools::sdk::ToolCapabilities;
use crate::tools::{create_schema, Tool, ToolContext, ToolExecutionResult};

/// Information about a single capability set.
#[derive(Debug, Clone, Serialize)]
pub struct CapabilityInfo {
    pub id: String,
    pub name: String,
    pub description: String,
    pub available: bool,
    pub reason: Option<String>,
    /// The optional tool groups this set offers, if it offers any.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub groups: Vec<GroupInfo>,
    /// The shared action judgments this set makes: `effect`, `readback`,
    /// `events`.
    ///
    /// Named here because the words are shared across device sets and the
    /// absences are not: a set that omits `readback` is saying a successful
    /// write was never verified, which is a different fact from a failed one.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub judges: Vec<String>,
}

/// One optional group of tools: what it gives you, and whether it is open.
#[derive(Debug, Clone, Serialize)]
pub struct GroupInfo {
    pub id: String,
    pub about: String,
    pub tools: Vec<String>,
    pub open: bool,
}

/// Tool that lists available capability sets on the host.
#[derive(Debug)]
pub struct ListCapabilitiesTool;

impl Default for ListCapabilitiesTool {
    fn default() -> Self {
        Self::new()
    }
}

impl ListCapabilitiesTool {
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl Tool for ListCapabilitiesTool {
    fn name(&self) -> &str {
        "list_capabilities"
    }

    fn description(&self) -> &str {
        "List the platform capability sets available on this host, and the tool groups each \
         one holds back until asked. Most platform tools are not advertised until their group \
         is opened — pass `open` to make one visible. Use when the user asks 'what can you \
         do', when a platform tool seems to be missing, or before a task whose tools you \
         cannot see yet."
    }

    fn parameters_schema(&self) -> Value {
        create_schema(
            "List available capability sets, and optionally open one of their tool groups",
            serde_json::json!({
                "open": {
                    "type": "string",
                    "description": "Optional: the id of a tool group to open, as reported in \
                                    `groups[].id` (e.g. \"android.locate\"). Its tools become \
                                    available for the rest of this session. Opening is \
                                    idempotent, and an unknown id is refused with the list of \
                                    real ones."
                }
            }),
            Vec::<String>::new(),
        )
    }

    fn capabilities(&self) -> ToolCapabilities {
        ToolCapabilities {
            read_only: true,
            requires_approval: false,
            risk_level: crate::tools::approval::RiskLevel::Low,
            categories: vec!["system".to_string(), "info".to_string()],
            idempotent: true,
            ..Default::default()
        }
    }

    async fn execute(
        &self,
        args: Value,
        context: &ToolContext,
    ) -> crate::Result<ToolExecutionResult> {
        let sets = crate::computer::platform::all_known_toolsets();

        // `open` is resolved against the registry the turn is actually running
        // with: a group the caller can see in this report can be opened, and
        // one it cannot see has no id to name. Without a registry there is
        // nothing tiered, so an `open` has nothing to do — reported, not
        // silently dropped.
        let requested = args
            .get("open")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|id| !id.is_empty());
        let mut opened: Option<String> = None;

        let infos = {
            let mut infos = Vec::new();
            for set in sets {
                let available = set.is_available();
                let reason = if available {
                    None
                } else {
                    let constraints = set.constraints();
                    let mut reasons = Vec::new();
                    if !constraints.target_os.is_empty()
                        && !constraints
                            .target_os
                            .iter()
                            .any(|os| os == std::env::consts::OS)
                    {
                        reasons.push(format!("requires OS: {:?}", constraints.target_os));
                    }
                    if constraints.requires_gui && !has_display() {
                        reasons.push("requires GUI/display".to_string());
                    }
                    for svc in &constraints.requires_services {
                        reasons.push(format!("requires service: {}", svc));
                    }
                    if reasons.is_empty() {
                        Some("unknown constraint failure".to_string())
                    } else {
                        Some(reasons.join(", "))
                    }
                };

                let groups = context
                    .tiering()
                    .map(|tiering| super::tiering::groups_for(&tiering.groups_of(set.id())))
                    .unwrap_or_default();

                infos.push(CapabilityInfo {
                    id: set.id().to_string(),
                    name: set.name().to_string(),
                    description: set.description().to_string(),
                    available,
                    reason,
                    groups,
                    judges: judged(set.screen_semantics()),
                });
            }
            infos
        };

        if let Some(id) = requested {
            let Some(tiering) = context.tiering() else {
                return Ok(ToolExecutionResult::error(
                    "this context has no tool registry attached, so there are no optional tool \
                     groups to open",
                ));
            };
            match tiering.open(id) {
                Some(_) => opened = Some(id.to_string()),
                None => {
                    let known: Vec<String> =
                        tiering.groups().into_iter().map(|group| group.id).collect();
                    return Ok(ToolExecutionResult::error(format!(
                        "no tool group with id {id:?}; available groups: {}",
                        if known.is_empty() {
                            "none — this host offers no optional tool groups".to_string()
                        } else {
                            known.join(", ")
                        }
                    ))
                    .with_data(serde_json::json!({ "groups": known })));
                }
            }
        }

        let json = serde_json::to_string_pretty(&infos)
            .map_err(crate::error::SyscityError::Serialization)?;

        let result = ToolExecutionResult::success(json).with_data(serde_json::to_value(&infos)?);
        if let Some(id) = opened {
            // The tools appear on the next request; saying so is the difference
            // between the model waiting for them and assuming nothing happened.
            // The report itself was read before the open, so `open: false` on
            // the group in this answer is the state the caller just changed.
            return Ok(ToolExecutionResult::success(format!(
                "Opened {id}. Its tools are available from the next turn.\n\n{}",
                result.output
            ))
            .with_data(result.data.unwrap_or(Value::Null)));
        }
        Ok(result)
    }
}

/// The judgment words a set makes, for the capability report.
///
/// Empty for a set that makes none — the field is skipped rather than shown as
/// an empty list, so "says nothing" and "says nothing is judged" stay different
/// readings.
fn judged(semantics: crate::computer::platform::ScreenSemantics) -> Vec<String> {
    use crate::computer::platform::Judgment;
    Judgment::ALL
        .into_iter()
        .filter(|judgment| semantics.makes(*judgment))
        .map(|judgment| {
            match judgment {
                Judgment::Effect => "effect",
                Judgment::Readback => "readback",
                Judgment::Events => "events",
            }
            .to_string()
        })
        .collect()
}

fn has_display() -> bool {
    std::env::var("DISPLAY").is_ok()
        || std::env::var("WAYLAND_DISPLAY").is_ok()
        || cfg!(target_os = "macos")
        || cfg!(target_os = "windows")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    #[test]
    fn test_list_capabilities_tool_creation() {
        let tool = ListCapabilitiesTool::new();
        assert_eq!(tool.name(), "list_capabilities");
        assert!(!tool.description().is_empty());
    }

    #[test]
    fn the_report_says_which_judgments_a_set_makes() {
        use crate::computer::platform::{Judgment, ScreenSemantics};
        assert_eq!(judged(ScreenSemantics::ALL), vec!["effect", "readback", "events"]);
        // A set that makes none says nothing rather than listing three
        // negatives — the field is about what is answered.
        assert!(judged(ScreenSemantics::NONE).is_empty());
        let effect_only = ScreenSemantics {
            judges_effect: true,
            ..ScreenSemantics::NONE
        };
        assert_eq!(judged(effect_only), vec!["effect"]);
        for judgment in Judgment::ALL {
            assert_eq!(
                ScreenSemantics::ALL.makes(judgment),
                judged(ScreenSemantics::ALL).len() == 3
            );
        }
    }

    #[test]
    fn the_android_set_answers_all_three_and_ios_answers_none() {
        // The shared vocabulary is only shared if a set is held to saying which
        // part of it it can apply.
        let sets = crate::computer::platform::all_known_toolsets();
        let android = sets
            .iter()
            .find(|set| set.id() == "android")
            .expect("android");
        assert_eq!(android.screen_semantics(), crate::computer::platform::ScreenSemantics::ALL);
        let ios = sets.iter().find(|set| set.id() == "ios").expect("ios");
        assert_eq!(ios.screen_semantics(), crate::computer::platform::ScreenSemantics::NONE);
    }

    fn tiering() -> Arc<crate::tools::tiering::ToolTiering> {
        let tiers = Arc::new(crate::tools::tiering::ToolTiering::new());
        tiers.declare(vec![crate::tools::tiering::ToolGroup {
            set_id: "android".to_string(),
            id: "android.locate".to_string(),
            about: "Find one control by name.".to_string(),
            tools: vec!["android_find".to_string(), "android_query".to_string()],
            open: false,
        }]);
        tiers
    }

    fn context(tiers: Arc<crate::tools::tiering::ToolTiering>) -> ToolContext {
        ToolContext::default().with_tiering(tiers)
    }

    /// The report carries the groups, so a caller reads an id it can open.
    #[tokio::test]
    async fn the_report_names_the_groups_and_their_state() {
        let tiers = tiering();
        let result = ListCapabilitiesTool::new()
            .execute(serde_json::json!({}), &context(tiers))
            .await
            .expect("runs");
        assert!(result.success, "{}", result.output);

        let data = result.data.expect("data");
        let android = data
            .as_array()
            .expect("list")
            .iter()
            .find(|set| set["id"] == "android")
            .expect("android set is known on every host");
        let group = &android["groups"][0];
        assert_eq!(group["id"], "android.locate");
        assert_eq!(group["open"], false);
        assert_eq!(group["tools"][0], "android_find");
    }

    #[tokio::test]
    async fn opening_a_group_makes_its_tools_visible() {
        let tiers = tiering();
        assert!(tiers.is_dormant("android_find"));

        let result = ListCapabilitiesTool::new()
            .execute(serde_json::json!({ "open": "android.locate" }), &context(tiers.clone()))
            .await
            .expect("runs");

        assert!(result.success, "{:?}", result.error);
        assert!(!tiers.is_dormant("android_find"), "the group is open now");
        assert!(!tiers.is_dormant("android_query"));
    }

    /// The tools appear on the next request, so this answer has to say the
    /// open happened — otherwise a model that sees no change assumes nothing
    /// did.
    #[tokio::test]
    async fn opening_a_group_says_so_in_the_answer() {
        let tiers = tiering();
        let result = ListCapabilitiesTool::new()
            .execute(serde_json::json!({ "open": "android.locate" }), &context(tiers))
            .await
            .expect("runs");

        assert!(result.success, "{:?}", result.error);
        assert!(result.output.starts_with("Opened android.locate"), "{}", result.output);
        assert!(result.output.contains("next turn"), "{}", result.output);
    }

    /// An id that is not a group is refused with the real ones, rather than
    /// reported as opened — a model that mistyped must be able to correct it.
    #[tokio::test]
    async fn an_unknown_group_is_refused_with_the_real_ones() {
        let result = ListCapabilitiesTool::new()
            .execute(serde_json::json!({ "open": "android.nope" }), &context(tiering()))
            .await
            .expect("runs");

        assert!(!result.success);
        let message = result.error.unwrap_or_default();
        assert!(message.contains("android.locate"), "{message}");
    }

    /// A context with no registry has nothing tiered; saying so beats
    /// pretending an open happened.
    #[tokio::test]
    async fn opening_without_a_registry_says_there_is_nothing_to_open() {
        let result = ListCapabilitiesTool::new()
            .execute(serde_json::json!({ "open": "android.locate" }), &ToolContext::default())
            .await
            .expect("runs");

        assert!(!result.success);
        let message = result.error.unwrap_or_default();
        assert!(message.contains("no tool registry"), "{message}");
    }
}
