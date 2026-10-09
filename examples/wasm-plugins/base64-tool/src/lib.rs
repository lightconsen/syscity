//! `base64-tool` — a Syscity WASM plugin exposing one tool, `base64`.
//!
//! # The host ABI this implements
//!
//! Defined by `src/plugins/runtime/mod.rs::invoke_wasm_tool`. The host:
//!
//! 1. resolves the guest's `memory` export and a `alloc(i32) -> i32` export;
//! 2. calls `alloc` three times, in this order, and writes into each buffer:
//!    the tool name, the JSON params, then reserves the output buffer;
//! 3. calls `call_tool(name_ptr, name_len, params_ptr, params_len, out_ptr,
//!    out_max) -> i32`, where the return value is the number of bytes written
//!    to `out_ptr`, or a negative number for an error;
//! 4. reads that many bytes as UTF-8 and parses them as JSON.
//!
//! There is no `free` export and no signal marking where one call ends and the
//! next begins, which is what shapes the allocator below.
//!
//! No host imports are used: base64 is pure computation, so this plugin needs
//! no permissions at all. That is deliberate for a first plugin — nothing here
//! can reach the filesystem, the network, or the agent's memory.

use std::cell::UnsafeCell;
use std::sync::atomic::{AtomicUsize, Ordering};

use base64::engine::general_purpose::STANDARD;
use base64::Engine as _;

/// Buffers handed out round-robin by [`alloc`], in the order the host asks.
///
/// The ABI has no `free`, so a real allocator would leak once per tool call for
/// the life of the instance. Instead each of the host's three allocations gets
/// a reserved buffer back, cycled: `alloc` call *n* mod 3 → buffer *n* mod 3.
/// The host always asks for the same three things in the same order, so the
/// cycle lines up every time; if it ever does not, the worst case is one
/// buffer being reused as another and the call fails visibly rather than
/// corrupting anything.
const SLOT_COUNT: usize = 3;

/// Sizes per slot, sized to what the host actually asks for: the tool name is
/// short, `OUT_MAX` is 64 KiB, and params get room to spare.
const SLOT_SIZES: [usize; SLOT_COUNT] = [4 * 1024, 512 * 1024, 64 * 1024];

/// Single-threaded by construction (`wasm32-unknown-unknown` has no threads and
/// the host instantiates one instance per plugin), so `UnsafeCell` in a static
/// is sound here. `Sync` is asserted for the same reason.
struct Slots {
    bufs: [UnsafeCell<Vec<u8>>; SLOT_COUNT],
    call: AtomicUsize,
}

unsafe impl Sync for Slots {}

static SLOTS: Slots = Slots {
    bufs: [
        UnsafeCell::new(Vec::new()),
        UnsafeCell::new(Vec::new()),
        UnsafeCell::new(Vec::new()),
    ],
    call: AtomicUsize::new(0),
};

/// Reserve a buffer for the host to write into, as `alloc(size) -> ptr`.
///
/// Returns 0 when the request exceeds the buffer the cycle offers, which the
/// host reads as "no allocation" and skips writing to.
#[no_mangle]
pub extern "C" fn alloc(_size: i32) -> i32 {
    let n = SLOTS.call.fetch_add(1, Ordering::Relaxed) % SLOT_COUNT;
    let cell = &SLOTS.bufs[n];
    // SAFETY: single-threaded (see `Slots`); the returned pointer is not used
    // by this side until the host calls `call_tool`, at which point no `alloc`
    // for this slot can be in flight.
    let buf = unsafe { &mut *cell.get() };
    if buf.len() < SLOT_SIZES[n] {
        buf.resize(SLOT_SIZES[n], 0);
    }
    buf.as_mut_ptr() as i32
}

/// The tool dispatcher: `call_tool(name_ptr, name_len, params_ptr, params_len,
/// out_ptr, out_max) -> bytes_written`.
#[no_mangle]
pub extern "C" fn call_tool(
    name_ptr: i32,
    name_len: i32,
    params_ptr: i32,
    params_len: i32,
    out_ptr: i32,
    out_max: i32,
) -> i32 {
    let name = match read_str(name_ptr, name_len) {
        Some(s) => s,
        None => return write_result(out_ptr, out_max, &error("could not read the tool name")),
    };
    let params = match read_str(params_ptr, params_len) {
        Some(s) => s,
        None => return write_result(out_ptr, out_max, &error("could not read the params")),
    };

    let result = match name {
        "base64" => base64_tool(params),
        other => error(&format!("unknown tool '{other}'")),
    };
    write_result(out_ptr, out_max, &result)
}

/// The one tool. `{ "mode": "encode" | "decode", "text": "..." }`.
fn base64_tool(params: &str) -> serde_json::Value {
    let parsed: serde_json::Value = match serde_json::from_str(params) {
        Ok(v) => v,
        Err(e) => return error(&format!("params must be JSON: {e}")),
    };

    let mode = match parsed.get("mode").and_then(|m| m.as_str()) {
        Some(m) => m,
        None => return error("missing 'mode' (\"encode\" or \"decode\")"),
    };
    let text = match parsed.get("text").and_then(|t| t.as_str()) {
        Some(t) => t,
        None => return error("missing 'text'"),
    };

    match mode {
        "encode" => serde_json::json!({ "result": STANDARD.encode(text) }),
        "decode" => match STANDARD.decode(text) {
            Ok(bytes) => match String::from_utf8(bytes) {
                Ok(decoded) => serde_json::json!({ "result": decoded }),
                // Decoding succeeded but the bytes are not text: say so rather
                // than lossily replacing them, which would look like a
                // successful round trip of something else.
                Err(_) => error("decoded bytes are not valid UTF-8"),
            },
            Err(e) => error(&format!("not valid base64: {e}")),
        },
        other => error(&format!("unknown mode '{other}' (want \"encode\" or \"decode\")")),
    }
}

fn error(message: &str) -> serde_json::Value {
    serde_json::json!({ "error": message })
}

/// Read a UTF-8 string out of guest memory.
fn read_str(ptr: i32, len: i32) -> Option<&'static str> {
    if ptr <= 0 || len < 0 {
        return None;
    }
    // SAFETY: the host wrote `len` bytes at `ptr` (it allocated them through
    // `alloc` immediately before), and this is the only reader for the duration
    // of the call.
    let bytes = unsafe { std::slice::from_raw_parts(ptr as *const u8, len as usize) };
    std::str::from_utf8(bytes).ok()
}

/// Serialize `value` into the host's output buffer and return the length.
fn write_result(out_ptr: i32, out_max: i32, value: &serde_json::Value) -> i32 {
    let text = match serde_json::to_string(value) {
        Ok(t) => t,
        Err(e) => {
            // Nothing better to do than report failure through the return code.
            let _ = e;
            return -1;
        }
    };
    let bytes = text.as_bytes();
    if out_ptr <= 0 || bytes.len() > out_max.max(0) as usize {
        return -1;
    }
    // SAFETY: as in `read_str` — the host allocated `out_max` bytes at
    // `out_ptr` and reads exactly what we return.
    unsafe {
        std::ptr::copy_nonoverlapping(bytes.as_ptr(), out_ptr as *mut u8, bytes.len());
    }
    bytes.len() as i32
}

/// Optional lifecycle hook; the host calls it once after instantiation.
#[no_mangle]
pub extern "C" fn init() {}
