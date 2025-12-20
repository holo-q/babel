//! Claude Babel - Unified interface for Claude Code sessions across kitty windows
//!
//! This library provides the core functionality for discovering, tracking, and interacting
//! with Claude Code sessions running in kitty terminal windows. It integrates with both
//! kitty's remote control protocol and Claude's conversation storage.
//!
//! ## Architecture
//!
//! ```text
//! ┌─────────────────────────────────────────────────────────────┐
//! │  CLI Layer (thin puppets)                                   │
//! │  babel ls, babel focus, babel send, etc.                    │
//! └─────────────────────────────────────────────────────────────┘
//!                             │
//!                             ▼
//! ┌─────────────────────────────────────────────────────────────┐
//! │  BabelCore (the brain)                                      │
//! │  - Unified API: windows(), focus(), history(), etc.         │
//! │  - Transparently handles daemon OR ephemeral mode           │
//! └─────────────────────────────────────────────────────────────┘
//!                             │
//!               ┌─────────────┴─────────────┐
//!               ▼                           ▼
//! ┌─────────────────────────┐   ┌─────────────────────────┐
//! │  Daemon Mode            │   │  Ephemeral Mode         │
//! │  - IPC to babeld        │   │  - Direct kitty/file    │
//! │  - Cached, instant      │   │  - On-demand loading    │
//! └─────────────────────────┘   └─────────────────────────┘
//! ```
//!
//! ## Usage
//!
//! CLI commands should use `BabelCore` as their single entry point:
//!
//! ```rust,ignore
//! use claude_babel::core::BabelCore;
//!
//! let core = BabelCore::connect().await;
//!
//! // These work identically whether daemon is running or not
//! let windows = core.windows().await?;
//! core.focus(42).await?;
//! let history = core.history(10).await?;
//! ```
//!
//! ## Daemon Mode
//!
//! When `babeld` is running, BabelCore connects via unix socket for instant responses.
//! The daemon maintains:
//! - Live window → session mappings
//! - Summary index for fast title matching
//! - Fingerprint index for reliable session matching (scrollback → JSONL)
//! - Event pub/sub for GUI frontends (no polling required)
//! - File and kitty change watching
//!
//! ## Ephemeral Mode
//!
//! When daemon is unavailable, BabelCore populates state on-demand. Each operation
//! queries kitty and ~/.claude directly. Results are cached for the lifetime of the
//! BabelCore instance to avoid redundant queries within a single command execution

pub mod utility;
pub mod kitty;
pub mod babel_storage;
pub mod fire;
pub mod daemon;
pub mod events;
pub mod fingerprint;
pub mod summarizer;
pub mod wset;
pub mod core;
pub mod tui;
pub mod file_index;
pub mod config;
pub mod title_policy;

// Re-export activity state from scrollparse for convenience
pub use scrollparse::claude::ActivityState;
