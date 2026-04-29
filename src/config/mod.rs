//! Babel configuration management
//!
//! Loads configuration from ~/.config/babel/babel.toml with sensible defaults.
//! Supports hot-reload via file watching.

mod loader;
mod schema;

pub use loader::*;
pub use schema::*;
