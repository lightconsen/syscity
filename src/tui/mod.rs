//! Interactive terminal UI client for Syscity.
//!
//! The client runs *inline*, like a shell session rather than a full-screen
//! application: finished output is written into the terminal's own scrollback
//! (so native scrolling, native selection and post-exit history all work) and
//! only a small live region at the bottom — composer, status row, blocking
//! prompts — is redrawn. It talks to a running gateway over WebSocket and
//! offers real-time chat, session resume, slash commands and configuration.
// INVARIANTS-NONE: presentation layer; owns no shared persistent state.

mod actions;
mod app;
mod auth;
mod commands;
mod error;
mod event_loop;
pub mod gateway_calls;
mod input;
mod osc11;
mod resume;
mod retry;
pub mod scrollback;
pub mod state;
#[cfg(test)]
mod test_gateway;
pub mod transcript;
pub mod ui;
mod ws_client;

pub use app::{run, SessionChoice};
pub use error::TuiError;
