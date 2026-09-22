//! Gateway lifecycle functions — start, stop, build_router.
//!
//! Extracted from `gateway/mod.rs` to reduce the main module size. Each
//! function takes the pieces of [`Gateway`](super::Gateway) it needs
//! explicitly (state, config, shutdown_token, task-trackers) instead of
//! `&self`.

use std::net::SocketAddr;
use std::sync::Arc;

use axum::{
    middleware::{from_fn, from_fn_with_state},
    routing::{get, post},
    Router,
};
use tokio::net::TcpListener;
use tokio::task::JoinHandle;
use tokio::time::{timeout, Duration};
use tokio_util::sync::CancellationToken;
use tower_http::cors::CorsLayer;
use tracing::{debug, error, info, warn};

use super::agent_spawn::spawn_agent_inner;
use super::*;
use super::{GatewayConfig, GatewayState};
use crate::agent::AgentConfig;
use crate::config::hot_reload::ConfigFileType;
use crate::mcp::McpToolWrapper;

mod helpers;
mod router;
mod shutdown;
mod start;

#[cfg(test)]
mod tests;

pub(crate) use helpers::register_mcp_tools;
pub(crate) use router::build_router;
pub(crate) use shutdown::stop_gateway;
pub(crate) use start::start_gateway;

/// How long shutdown waits for the engine's fire-and-forget writes.
///
/// They are ordinary SQLite inserts on the shared pool, so seconds is generous;
/// the bound exists so a stuck writer cannot hold the process open.
const PENDING_WRITES_TIMEOUT: Duration = Duration::from_secs(5);

/// How often stale approvals are denied.
///
/// The approval queue's own `default_timeout` decides what "stale" means; this
/// is only how promptly the sweep notices.
const APPROVAL_SWEEP_INTERVAL: Duration = Duration::from_secs(60);

/// How long [`stop_gateway`] waits for the tasks left in the registry to finish
/// what they were writing before aborting them.
#[cfg(not(test))]
const BACKGROUND_DRAIN_TIMEOUT: Duration = Duration::from_secs(2);
#[cfg(test)]
const BACKGROUND_DRAIN_TIMEOUT: Duration = Duration::from_millis(1000);

/// How long [`stop_gateway`] waits for the storage backend to release its
/// connection pool.
const STORAGE_CLOSE_TIMEOUT: Duration = Duration::from_secs(5);
