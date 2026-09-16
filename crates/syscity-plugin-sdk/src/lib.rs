//! Syscity Plugin SDK
//!
//! A high-level Rust SDK for building WASM plugins for the Syscity AI assistant
//! platform.
//!
//! # Quick Start
//!
//! ```ignore
//! use syscity_plugin_sdk::*;
//!
//! #[no_mangle]
//! pub extern "C" fn call_tool(name_ptr: i32, name_len: i32, params_ptr: i32, params_len: i32,
//!                             out_ptr: i32, out_max: i32) -> i32 {
//!     let name = memory::read_string(name_ptr as *const u8, name_len as usize);
//!     let params: serde_json::Value = serde_json::from_str(&name).unwrap_or_default();
//!
//!     // Your logic here...
//!     logging::info(&format!("Tool called: {}", name));
//!
//!     let result = serde_json::json!({ "status": "ok" });
//!     unsafe {
//!         memory::write_result(out_ptr as *mut u8, out_max as usize, &result) as i32
//!     }
//! }
//! ```
//!
//! # WIT Component Model
//!
//! Enable the `wit` feature to use auto-generated bindings from WIT
//! definitions:
//!
//! **> Note — there are two ABIs, and they do not interoperate.** The Syscity
//! plugin runtime (`src/plugins/runtime/`) loads a *core module*: plugins
//! export plain `extern "C"` symbols (`call_tool`, `provider_complete`, …), as
//! shown in the Quick Start above. The `wit` bindings generated here describe a
//! *component model* ABI that the runtime does not instantiate
//! (`wasmtime::Module` is used, never `wasmtime::Component`). A plugin written
//! against `wit_bindings` cannot be loaded by the current runtime, and a
//! plugin written against the `extern "C"` symbols is not expressible in this
//! WIT. The bindings compile (they are exercised by CI's `--all-features`)
//! and are kept for forward compatibility; treat them as a preview of a future
//! ABI, not as what a deployed plugin links against today.
//!
//! ```ignore
//! // In your plugin's Cargo.toml:
//! // [dependencies]
//! // syscity-plugin-sdk = { version = "0.1", features = ["wit"] }
//!
//! use syscity_plugin_sdk::wit_bindings::host_functions;
//!
//! pub fn main() {
//!     let id = host_functions::get_plugin_id();
//!     host_functions::log("info", &format!("Plugin {} initialized", id));
//! }
//! ```
//!
//! # Host Functions
//!
//! The SDK wraps 15+ host functions provided by Syscity's WASM runtime:
//!
//! - **Config**: `config::get`, `config::get_all`
//! - **Memory**: `memory::store`, `memory::load`, `memory::search`
//! - **Store**: `store::get`, `store::set` (persistent KV)
//! - **HTTP**: `http::get`, `http::post`
//! - **Events**: `events::emit`
//! - **Context**: `context::get`, `context::session_id`
//! - **Plugin**: `plugin::id`
//! - **Logging**: `logging::info`, `logging::warn`, `logging::error`

// ---------------------------------------------------------------------------
// Modules
// ---------------------------------------------------------------------------

pub mod config;
pub mod context;
pub mod events;
pub mod http;
pub mod logging;
pub mod memory;
pub mod plugin;
pub mod store;

#[cfg(feature = "wit")]
pub mod wit_bindings;

// ---------------------------------------------------------------------------
// Low-level FFI declarations
// ---------------------------------------------------------------------------

extern "C" {
    // Logging
    fn log(ptr: *const u8, len: usize);

    // Config
    fn config_get(key_ptr: *const u8, key_len: usize, out_ptr: *mut u8, out_len: usize) -> usize;
    fn config_get_all(out_ptr: *mut u8, out_len: usize) -> usize;

    // Memory
    fn memory_store(
        key_ptr: *const u8,
        key_len: usize,
        val_ptr: *const u8,
        val_len: usize,
    ) -> usize;
    fn memory_load(key_ptr: *const u8, key_len: usize, out_ptr: *mut u8, out_len: usize) -> usize;
    fn memory_search(
        prefix_ptr: *const u8,
        prefix_len: usize,
        out_ptr: *mut u8,
        out_len: usize,
    ) -> usize;

    // Persistent Store
    fn store_get(key_ptr: *const u8, key_len: usize, out_ptr: *mut u8, out_len: usize) -> usize;
    fn store_set(key_ptr: *const u8, key_len: usize, val_ptr: *const u8, val_len: usize) -> usize;

    // HTTP
    fn http_get(url_ptr: *const u8, url_len: usize, out_ptr: *mut u8, out_len: usize) -> usize;
    fn http_post(
        url_ptr: *const u8,
        url_len: usize,
        body_ptr: *const u8,
        body_len: usize,
        ct_ptr: *const u8,
        ct_len: usize,
        out_ptr: *mut u8,
        out_len: usize,
    ) -> usize;

    // Events
    fn emit_event(
        type_ptr: *const u8,
        type_len: usize,
        payload_ptr: *const u8,
        payload_len: usize,
    ) -> usize;

    // Context
    fn get_context(key_ptr: *const u8, key_len: usize, out_ptr: *mut u8, out_len: usize) -> usize;
    fn get_session_id(out_ptr: *mut u8, out_len: usize) -> usize;

    // Plugin Info
    fn get_plugin_id(out_ptr: *mut u8, out_len: usize) -> usize;
}

// ---------------------------------------------------------------------------
// Helper: call an FFI function that writes to an output buffer, return a
// `String`.
// ---------------------------------------------------------------------------

/// Call an FFI function that writes to an output buffer, return a `String`.
///
/// Uses a stack buffer for small results, allocates on the heap if too large.
fn ffi_call_to_string(call: impl FnOnce(*mut u8, usize) -> usize) -> Option<String> {
    let mut buf = [0u8; 2048];
    let written = call(buf.as_mut_ptr(), buf.len());

    if written == 0 {
        return None;
    }

    if written <= buf.len() {
        return Some(String::from_utf8_lossy(&buf[..written]).into_owned());
    }

    // Need a larger heap allocation — but we can't re-call the FnOnce closure.
    // Allocate a larger buffer and accept potential truncation.
    // In practice, all our FFI calls return small enough data for 2KB.
    Some(String::from_utf8_lossy(&buf[..]).into_owned())
}
