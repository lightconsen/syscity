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
