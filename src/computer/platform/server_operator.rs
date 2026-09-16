//! ServerOperator — high-level server management abstraction.
//!
//! Orchestrates system inspection, snapshot generation, and LLM-based
//! diagnosis. Use `LinuxSystemInspector` on Linux or provide your own
//! `SystemInspector` impl.
//!
//! **Not wired up yet.** Nothing constructs a `ServerOperator`, nothing
//! registers an inspector as a platform tool set, and there are no tests. The
//! building blocks below an inspector — `SystemInspectTool` — are live and
//! registered; this layer above them is a design in progress. Say so here so a
//! reader does not mistake it for a running feature.

use std::sync::Arc;

/// Trait for platform-specific system inspection.
#[async_trait::async_trait]
pub trait SystemInspector: Send + Sync {
    /// Collect a full system snapshot.
    async fn inspect_full(&self) -> crate::Result<SystemSnapshot>;
}

/// A structured system snapshot.
///
/// Re-exported from the platform-specific inspect module so consumers
/// can use a single type regardless of OS.
pub use crate::computer::platform::linux::system_inspect::SystemSnapshot;

/// Diagnosis input containing a formatted LLM prompt and the snapshot.
#[derive(Debug, Clone)]
pub struct Diagnosis {
    /// Pre-formatted prompt ready to send to an LLM.
    pub prompt: String,
    /// The snapshot that the prompt is based on.
    pub snapshot: SystemSnapshot,
}

/// High-level server operator.
///
/// Wraps a [`SystemInspector`] to provide snapshot collection and
/// LLM-ready diagnosis prompts.
pub struct ServerOperator {
    inspector: Arc<dyn SystemInspector>,
}

impl ServerOperator {
    /// Create a new server operator.
    pub fn new(inspector: Arc<dyn SystemInspector>) -> Self {
        Self { inspector }
    }

    /// Collect a full system snapshot.
    pub async fn inspect(&self) -> crate::Result<SystemSnapshot> {
        self.inspector.inspect_full().await
    }

    /// Generate a diagnosis prompt from a snapshot.
    ///
    /// The returned string can be sent directly to an LLM for analysis.
    pub fn diagnose_prompt(snapshot: &SystemSnapshot) -> String {
        format!(
            "You are a senior Linux SRE. Analyze the following system snapshot and identify \
             anomalies, performance bottlenecks, security concerns, and potential issues. Provide \
             actionable recommendations.\n\n{}",
            serde_json::to_string_pretty(snapshot).unwrap_or_default()
        )
    }

    /// Produce a [`Diagnosis`] from a snapshot.
    pub fn diagnose(snapshot: &SystemSnapshot) -> Diagnosis {
        Diagnosis {
            prompt: Self::diagnose_prompt(snapshot),
            snapshot: snapshot.clone(),
        }
    }
}
