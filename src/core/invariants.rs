//! Runtime invariant registry.
//!
//! Modules own the data invariants they are responsible for upholding and
//! register them here; a single CLI entry point (`syscity invariants`) runs
//! every registered check against live local state and reports pass/fail.
//! The companion mechanical convention — enforced by
//! `scripts/static-analysis.sh` — is that every top-level module either
//! registers its checks here or carries an explicit `INVARIANTS-NONE:` marker
//! explaining why it has none. Nothing is silently unchecked.
//!
//! # Example
//!
//! ```
//! use std::future::Future;
//! use syscity::core::invariants::{Invariant, register, run_all};
//!
//! fn always_ok() -> std::pin::Pin<Box<dyn Future<Output = Result<(), String>> + Send>> {
//!     Box::pin(async { Ok(()) })
//! }
//!
//! register(Invariant {
//!     id: "example/always_ok",
//!     module: "example",
//!     description: "trivially passes",
//!     check: always_ok,
//! });
//! # tokio::runtime::Runtime::new().unwrap().block_on(async {
//! let report = run_all().await;
//! assert!(report.iter().any(|o| o.id == "example/always_ok" && o.passed));
//! # });
//! ```

use std::collections::BTreeMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::{LazyLock, Mutex};

/// Outcome of one invariant check.
#[derive(Debug, Clone, serde::Serialize)]
pub struct InvariantOutcome {
    /// Registry-wide unique id (`<module>/<name>`).
    pub id: String,
    /// Owning top-level module.
    pub module: String,
    pub passed: bool,
    /// Failure detail, or a note for passes that skipped on absent state.
    pub detail: Option<String>,
}

/// A boxed async check future: `Ok(())` holds, `Err(detail)` violates (see
/// [`SKIP_PREFIX`]).
pub type CheckFuture = Pin<Box<dyn Future<Output = Result<(), String>> + Send>>;

/// A named, module-owned runtime check over local persistent state.
pub struct Invariant {
    /// Registry-wide unique id (`<module>/<name>`).
    pub id: &'static str,
    /// Owning top-level module (used for grouping in reports).
    pub module: &'static str,
    /// What the invariant guarantees, in one sentence.
    pub description: &'static str,
    /// Run the check. `Err(detail)` = violated; `Ok(())` = holds; passing
    /// with a detail string means "not applicable here" (e.g. store absent).
    pub check: fn() -> CheckFuture,
}

static REGISTRY: LazyLock<Mutex<BTreeMap<&'static str, Invariant>>> =
    LazyLock::new(|| Mutex::new(BTreeMap::new()));

/// Acquire the registry lock. A poisoned lock (a panic while holding it)
/// still yields a usable map — all operations are plain inserts/snapshots —
/// so the guard is recovered instead of propagating the panic.
fn lock_registry() -> std::sync::MutexGuard<'static, BTreeMap<&'static str, Invariant>> {
    match REGISTRY.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    }
}

/// Register an invariant. Later registrations with the same id replace the
/// earlier one (keeps re-registration idempotent during tests).
pub fn register(invariant: Invariant) {
    let mut registry = lock_registry();
    registry.insert(invariant.id, invariant);
}

/// Register the built-in checks contributed by their owning modules. Called
/// once before running the CLI report; each source module keeps ownership of
/// its own checks next to the code that upholds them.
pub fn register_builtins() {
    for inv in crate::agent::session_store_invariant_checks() {
        register(inv);
    }
    for inv in crate::agent::todo_invariant_checks() {
        register(inv);
    }
    for inv in crate::cron::cron_invariant_checks() {
        register(inv);
    }
    // Cloud ships behind a feature: its checks register only when the module
    // exists in this build.
    #[cfg(feature = "cloud")]
    for inv in crate::cloud::cloud_invariant_checks() {
        register(inv);
    }
}

/// Snapshot of the currently registered invariant ids/modules.
pub fn registered() -> Vec<(&'static str, &'static str)> {
    let registry = lock_registry();
    registry.values().map(|i| (i.id, i.module)).collect()
}

/// Run every registered check and return the outcomes ordered by id.
pub async fn run_all() -> Vec<InvariantOutcome> {
    // Snapshot the check fns first so no lock is held across awaits.
    let checks: Vec<(String, String, CheckFuture)> = {
        let registry = lock_registry();
        registry
            .values()
            .map(|i| (i.id.to_string(), i.module.to_string(), (i.check)()))
            .collect()
    };
    let mut outcomes = Vec::with_capacity(checks.len());
    for (id, module, fut) in checks {
        match fut.await {
            Ok(()) => outcomes.push(InvariantOutcome {
                id,
                module,
                passed: true,
                detail: None,
            }),
            Err(e) if e.starts_with(SKIP_PREFIX) => outcomes.push(InvariantOutcome {
                id,
                module,
                passed: true,
                detail: Some(e),
            }),
            Err(e) => outcomes.push(InvariantOutcome {
                id,
                module,
                passed: false,
                detail: Some(e),
            }),
        }
    }
    outcomes.sort_by(|a, b| a.id.cmp(&b.id));
    outcomes
}

/// Prefix a check's `Err` detail with this to report "not applicable / nothing
/// to verify" instead of a violation (e.g. the daemon has never run, so there
/// is no store to inspect). The outcome still counts as passed.
pub const SKIP_PREFIX: &str = "skip: ";

#[cfg(test)]
mod tests {
    use super::*;

    fn ok_check() -> Pin<Box<dyn Future<Output = Result<(), String>> + Send>> {
        Box::pin(async { Ok(()) })
    }

    fn fail_check() -> Pin<Box<dyn Future<Output = Result<(), String>> + Send>> {
        Box::pin(async { Err("violated".to_string()) })
    }

    fn skip_check() -> Pin<Box<dyn Future<Output = Result<(), String>> + Send>> {
        Box::pin(async { Err(format!("{SKIP_PREFIX}nothing to check")) })
    }

    #[tokio::test]
    async fn registers_runs_and_dedups_by_id() {
        register(Invariant {
            id: "test/dup",
            module: "test",
            description: "first",
            check: ok_check,
        });
        // Re-registration with the same id replaces, not duplicates.
        register(Invariant {
            id: "test/dup",
            module: "test",
            description: "replacement",
            check: ok_check,
        });
        let report = run_all().await;
        let dupes = report.iter().filter(|o| o.id == "test/dup").count();
        assert_eq!(dupes, 1, "same-id registration must replace");
    }

    #[tokio::test]
    async fn failing_and_skipping_checks_report_distinctly() {
        register(Invariant {
            id: "test/fail",
            module: "test",
            description: "fails",
            check: fail_check,
        });
        register(Invariant {
            id: "test/skip",
            module: "test",
            description: "skips",
            check: skip_check,
        });
        let report = run_all().await;
        let failed = report
            .iter()
            .find(|o| o.id == "test/fail")
            .expect("fail present");
        assert!(!failed.passed && failed.detail.as_deref() == Some("violated"));
        let skipped = report
            .iter()
            .find(|o| o.id == "test/skip")
            .expect("skip present");
        assert!(skipped.passed, "skip-prefixed errors count as passed-with-note");
    }
}
