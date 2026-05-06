//! Babel - The tower where many voices speak as one
//!
//! A unified interface for discovering, tracking, and orchestrating agent sessions
//! across kitty terminal panes. Each pane hosts an agent worker in the tower, speaking
//! its own conversation yet part of the greater chorus. Babel listens to all voices,
//! translates their states, and awaits the Captain who will conduct them.
//!
//! The name evokes both the multiplicity of concurrent agent sessions and the aspiration
//! to bridge them into coherent collaboration. Where the mythic tower fell to confusion,
//! this Babel succeeds through structured communication: events, fingerprints, and the
//! coming orchestration layer that will let a Captain coordinate the workers below.
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
//! │  - Unified API: panes(), focus(), history(), etc.           │
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
//! use babel::core::BabelCore;
//!
//! let core = BabelCore::connect().await;
//!
//! // These work identically whether daemon is running or not
//! let panes = core.panes().await?;
//! core.focus(42).await?;
//! let history = core.history(10).await?;
//! ```
//!
//! ## Daemon Mode
//!
//! When `babeld` is running, BabelCore connects via unix socket for instant responses.
//! The daemon maintains:
//! - Live pane → session mappings
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

pub mod backend;
pub mod agent_kind;
pub mod babel_storage;
pub mod config;
pub mod core;
pub mod daemon;
pub mod desktop;
pub mod events;
pub mod file_index;
pub mod fingerprint;
pub mod fire;
pub mod harness_ops;
pub mod indicator;
pub mod ipc;
pub mod kitty;
pub mod layout;
pub mod logging;
pub mod model;
pub mod native_sessions;
pub mod pager;
pub mod paint;
#[cfg(feature = "gtk-render")]
pub mod render;
pub mod service;
pub mod session_row;
pub mod summarizer;
pub mod title_policy;
pub mod tui;
pub mod utility;
pub mod wset;

// Re-export activity state and visual types from spaceship-std
// These were extracted to spaceship-std to be shared across components
pub use spaceship_std::agents::ActivityState;
pub use spaceship_std::agents_style::{
    colors, AgentStyle, AgentTextures, OutlinePattern, OutlineStyle, Rgb,
};

// Type aliases for backwards compatibility (names changed in refactor)
pub type DotStyle = AgentStyle;
pub type DotTexture = AgentTextures;

// Re-export indicator types for panel widgets
pub use indicator::{IndicatorBatch, IndicatorEvent};

// Paint stream — babel is authoritative over UX, clients are dumb forwarders.
pub use paint::{PaintEvent, WorkspacePaintEvent};

// Which harness owns a pane (claude, codex, ...). Threaded through events
// and indicators so panel widgets can pick the right color family per agent.
pub use agent_kind::{
    AgentKind, HarnessSpec, HarnessSupport, HookEventSpec, HookStateEffect, InstallStrategy,
    PulseEffect, ReadEffect,
};
pub use model::{AgentSessionKey, PaneAddr, PaneSelector, SessionId};
