pub mod admin;
pub mod artifacts;
#[cfg(feature = "cloud")]
pub mod cloud;
pub mod config;
pub mod health;
pub mod openai;
pub mod update;
pub mod web_ui;
pub use admin::*;
pub use artifacts::*;
// No `pub use config::*` here: the module now holds only the crate-internal
// `persist_config_atomic` (the REST config endpoints were replaced by the WS
// `config.get`/`config.set`), and every caller names it by its full path.
pub use health::*;
pub use openai::*;
pub use web_ui::*;
