//! Claude Babel - Unified interface for Claude Code sessions across kitty windows
//!
//! This library provides the core functionality for discovering, tracking, and interacting
//! with Claude Code sessions running in kitty terminal windows. It integrates with both
//! kitty's remote control protocol and Claude's conversation storage.
//!
//! ## Architecture
//!
//! The system has two modes:
//! - **Direct mode**: CLI directly queries kitty and ~/.claude (slower but no daemon)
//! - **Daemon mode**: CLI queries babeld over unix socket (instant, pre-cached)
//!
//! The daemon (`babeld`) maintains:
//! - Live window → session mappings
//! - Summary index for fast title matching
//! - Fingerprint index for reliable session matching (scrollback → JSONL)
//! - Event pub/sub for GUI frontends (no polling required)
//! - Watches for kitty and file changes

pub mod claude_storage;
pub mod kitty;
pub mod discovery;
pub mod overlay;
pub mod state;
pub mod fire;
pub mod ipc;
pub mod daemon;
pub mod events;
pub mod fingerprint;
pub mod summarizer;
pub mod wset;
