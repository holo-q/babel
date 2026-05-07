//! Claude-owned protocol surface.
//!
//! Keep Claude storage and title mutation formats behind this namespace so
//! feature code never hardcodes Claude JSONL records outside the harness.

pub mod spec;
pub mod title;
pub mod transcript;
