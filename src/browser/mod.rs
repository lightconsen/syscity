//! Browser automation module for Syscity
//!
//! Provides:
//! - **BrowserPool**: Persistent browser instance caching with idle eviction
//! - **ARIA Snapshot**: LLM-friendly accessible tree with ref markers
//! - **Profile Management**: Multiple browser configs (headless/headed, Chrome
//!   MCP)
//! - **Bridge Server**: HTTP API decoupling (P1)
//! - **SSRF Guard**: Navigation security (P1)
//! - **Sandbox**: Docker-isolated browser (P3)
//!
//! All browser functionality is gated behind the `browser` feature.
// INVARIANTS-NONE: CDP captures live in in-memory ring buffers bounded by construction (MAX_RECORDS).

#[cfg(feature = "browser")]
pub mod aria_snapshot;
#[cfg(feature = "browser")]
pub mod coordinate_space;
#[cfg(feature = "browser")]
pub mod escalation;
#[cfg(feature = "browser")]
pub mod instrument;
#[cfg(feature = "browser")]
pub mod network_log;
#[cfg(feature = "browser")]
pub mod pointer;
#[cfg(feature = "browser")]
pub mod pool;
#[cfg(feature = "browser")]
pub mod profile;
#[cfg(feature = "browser")]
pub mod sourcemap;

#[cfg(feature = "browser")]
pub use aria_snapshot::{act_by_ref, aria_snapshot, ActKind, AriaNodeLine, AriaSnapshot};
#[cfg(feature = "browser")]
pub use pool::{BrowserInstance, BrowserPool, PageHandle};
#[cfg(feature = "browser")]
pub use profile::{BrowserDriver, BrowserPoolConfig, BrowserProfile};

// P1 modules (will be created in Phase 1)
#[cfg(feature = "browser")]
pub mod bridge;
#[cfg(feature = "browser")]
pub mod bridge_client;
// The SSRF guard is pure std/tokio logic — exported unconditionally so
// non-browser consumers (the web tool) can reuse it on every platform.
pub mod navigation_guard;

#[cfg(feature = "browser")]
pub use bridge::BrowserBridge;
#[cfg(feature = "browser")]
pub use bridge_client::BridgeClient;
pub use navigation_guard::{assert_navigation_allowed, NavigationPolicy};

// P3 module
#[cfg(feature = "browser")]
pub mod sandbox;

/// Check if the browser feature is enabled (compile-time)
pub const BROWSER_FEATURE_ENABLED: bool = cfg!(feature = "browser");
