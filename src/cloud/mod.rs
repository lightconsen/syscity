//! Syscity Cloud integration (§2.7 / docs/cloud-integration.md).
//!
//! Compiled only with the `cloud` feature (default OFF — default builds have
//! zero cloud coupling). Runtime is additionally gated by `cloud.enabled` and
//! a logged-in session token.

#![cfg(feature = "cloud")]

pub mod client;
pub mod config;
pub mod device;
pub mod multipliers;
pub mod provider;
pub mod session;

/// Invariant checks owned by the cloud module, registered with
/// `core::invariants` (surfaced through `syscity invariants`).
pub fn cloud_invariant_checks() -> Vec<crate::core::invariants::Invariant> {
    use crate::core::invariants::Invariant;

    vec![Invariant {
        id: "cloud/model-catalog-timestamps",
        module: "cloud",
        description: "the cached cloud model catalog never carries a timestamp from the future",
        check: || {
            Box::pin(async {
                use crate::cloud::multipliers::catalog_cache_snapshot;
                match catalog_cache_snapshot().await {
                    Some((fetched_at, _)) => {
                        if fetched_at > std::time::Instant::now() {
                            Err("cached catalog timestamp is in the future".to_string())
                        } else {
                            Ok(())
                        }
                    }
                    None => Ok(()),
                }
            })
        },
    }]
}
