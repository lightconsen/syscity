//! The higher-level Android tools: one call where the model would otherwise
//! spend several turns reading, scrolling and tapping.
//!
//! Three of them exist to remove round trips rather than to add capability:
//! `android_collect` reads a long list screen by screen, `android_find` scrolls
//! to a label the model already knows it wants, and `android_dialog` reads the
//! dialog that is in the way instead of making the model describe it.
//! `android_system_action` goes through Android's own intents for alarms,
//! timers, calendar events, shares and settings — one stable call instead of a
//! sequence of taps whose layout changes between OEM builds. `android_ocr`
//! reads the screenshot for the case where the accessibility tree describes
//! nothing.
//!
//! The rest are the tools that replace a loop of the core ones: `android_run`
//! for several known steps, `android_extract` for the same screen as data,
//! `android_event` for what the phone says has happened, `android_wait` for a
//! screen that has not settled yet, `android_set_wheel` for a control that is
//! otherwise one tap at a time, `android_query` for one control without the
//! whole tree, `android_diagnose` for why a screen looks the way it does,
//! `android_display`/`android_hand_to_user` for the second screen, and
//! `android_find` and `android_collect` above.
//!
//! Each file holds one tool and the helpers it alone needs. What more than one
//! tool needs — the scroll geometry, the tree/OCR merge, and the element
//! builders their tests share — lives in `scroll`, `screen` and `testkit`.

use async_trait::async_trait;
use serde_json::Value;
use std::time::Duration;

// Everything the set exposes, and the private helpers it uses, exactly as the
// tools named them when they lived in `tools.rs` next to the set.
use super::*;
use crate::tools::{create_schema, Tool, ToolContext, ToolExecutionResult};

mod collect;
mod diagnose_tool;
mod dialog;
mod display_tool;
mod events;
mod extract;
mod find;
mod hand_to_user;
mod ocr_tool;
mod query_tool;
mod run;
mod screen;
mod scroll;
mod system_action;
mod wait;
mod wheel_tool;

#[cfg(test)]
mod testkit;

// The shared geometry and merge helpers: private to the tools, but named
// without a path from each one, as they were when this was a single file.
pub(crate) use screen::*;
pub(crate) use scroll::*;

// Shared across the tool files: a run watches events while it steps, a wait
// reads the ring the event tool owns, and the display tool names keycodes the
// run tool decodes. Named without a path, as when they shared a file.
pub(crate) use events::{read_a11y_status, read_events, wait_for_event};
pub(crate) use run::described_keycode;
pub(crate) use wait::WAIT_POLL_MS;

pub use collect::AdbCollectTool;
pub use diagnose_tool::AdbDiagnoseTool;
pub use dialog::AdbDialogTool;
pub use display_tool::AdbDisplayTool;
pub use events::AdbEventTool;
pub use extract::AdbExtractTool;
pub use find::AdbFindTool;
pub use hand_to_user::AdbHandToUserTool;
pub use ocr_tool::AdbOcrTool;
pub use query_tool::AdbQueryTool;
pub use run::AdbRunTool;
pub use system_action::AdbSystemActionTool;
pub use wait::AdbWaitTool;
pub use wheel_tool::AdbSetWheelTool;

// `knowledge` reads it to tell a dialog's title from a page's heading.
pub(crate) use dialog::is_dialog_class;
