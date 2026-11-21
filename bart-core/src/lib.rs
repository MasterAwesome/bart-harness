#[macro_use]
pub mod marker;

pub mod agent;
pub mod approval;
pub mod auth;
mod child;
pub mod choice;
pub mod confinement;
pub mod conversation;
pub mod detail_excerpt;
mod diagnostics;
pub mod history;
mod http_body;
pub mod http_client;
pub mod mcp;
pub mod persist;
pub mod preferences;
pub mod provider;
mod random;
pub mod record;
pub mod shell_quote;
pub mod signals;
pub mod supervise;
pub mod terminal_text;
pub mod tool;

pub const NOT_POISONED: &str = "a panic aborts, so no lock is poisoned";
