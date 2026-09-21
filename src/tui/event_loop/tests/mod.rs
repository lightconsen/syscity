//! Tests for the event loop, grouped by the path each one drives.
//!
//! `support` holds the fixtures they share; the rest are named for the code
//! under test — the loop and its dispatch rules, sending and the queue, the
//! reconnect path, gateway events, the two prompts, and line mode.

mod support;

mod events_tests;
mod loop_tests;
mod plain_tests;
mod prompts_tests;
mod reconnect_tests;
mod send_tests;
