//! Babel configuration management
//!
//! Loads configuration from ~/.config/babel/babel.toml with sensible defaults.
//! Supports hot-reload via file watching.

mod schema;
mod loader;

pub use schema::*;
pub use loader::*;
