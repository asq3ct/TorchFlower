#![allow(unknown_lints)]
#![recursion_limit = "256"]
#![allow(
    clippy::collapsible_if,
    clippy::collapsible_else_if,
    clippy::excessive_precision,
    clippy::field_reassign_with_default,
    clippy::manual_div_ceil,
    clippy::manual_is_multiple_of,
    clippy::needless_borrow,
    clippy::too_many_arguments,
    clippy::vec_init_then_push
)]
#[cfg(feature = "full-engine")]
pub mod api;
#[path = "bedrock/auth/mod.rs"]
pub mod auth;
pub mod bedrock;
#[cfg(feature = "full-engine")]
pub mod bot;
pub mod config;
pub mod core;
pub mod db;
pub mod diagnostics;
#[cfg(feature = "easy-auth")]
pub mod easy_auth;
pub mod error;
pub mod models;
pub mod native_client;
pub mod pool;
#[cfg(feature = "full-engine")]
pub mod validation;
pub use crate::core::{BotBuilder, BotEvent, BotSession, Event};
