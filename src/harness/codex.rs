//! Codex-owned protocol surface.
//!
//! Keep Codex storage, roster, and transcript formats behind this namespace so
//! feature code never has to know which Codex file carries which native detail.

pub mod spec;
pub mod transcript;
