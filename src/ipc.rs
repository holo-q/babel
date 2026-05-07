//! Stable IPC wire DTOs.
//!
//! These types are the serialized contract between Babel clients and the
//! daemon. Transport code lives in `utility::ipc`; this module owns the request
//! and response shapes so command, core, daemon, and tests can all point at the
//! same boundary.

use serde::{Deserialize, Serialize};

use crate::events::EventMessage;
use crate::kitty::{KittyPane, PaneAddr, PaneSelector};
use crate::paint::PaintEvent;
use crate::service::state::{DaemonReadiness, SocketStatus, TerminalInfo};
use crate::utility::agent_discovery::AgentPane;
use crate::utility::claude_storage::SessionInfo;
use crate::wset::WSetSummary;
use crate::{AgentKind, PulseEffect};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TitleTarget {
    PlatformWindow { platform_window_id: u64 },
    Pane { os_window_id: u64, pane_id: u64 },
}

/// Request - A query descending into the tower.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "cmd", rename_all = "snake_case")]
pub enum Request {
    /// List all agent panes (fast - from cache)
    List,

    /// List all kitty terminals (not just agents)
    /// Useful for seeing the full terminal flow and watching transitions
    ListTerminals,

    /// List raw kitty panes from all sockets
    /// Unlike ListTerminals, this returns raw KittyPane data without enrichment
    ListPanes,

    /// List kitty socket status (multi-instance awareness)
    ListSockets,

    /// List all agent panes with fingerprint data (slow - extracts scrollback)
    ListWithFingerprints,

    /// Get status of specific pane (or focused if `target` is None)
    ///
    /// `target` accepts the canonical `PaneSelector::Addr` form or the legacy
    /// bare-id form for CLI input edges. New callers should use `Addr`.
    Status {
        #[serde(default)]
        target: Option<PaneSelector>,
    },

    /// Get full session info for a pane (triggers enrichment if needed)
    Enrich { target: PaneSelector },

    /// Focus a pane
    Focus { target: PaneSelector },

    /// Get scrollback from pane
    Scroll { target: PaneSelector },

    /// Send text to pane (with Enter/CR at end)
    Send { target: PaneSelector, text: String },

    /// Type text to pane (without Enter/CR at end)
    /// Useful for composing prompts incrementally
    Type { target: PaneSelector, text: String },

    /// Check if a pane has pending (unsent) input in the textbox
    /// Returns true if there's text typed but not yet submitted
    HasPendingInput { target: PaneSelector },

    /// Tag pane with icon
    Tag { target: PaneSelector, icon: String },

    /// Mark pane as read
    MarkRead { target: PaneSelector },

    /// Get recent history from ~/.claude
    History { limit: usize },

    /// Ping - check if daemon is alive
    Ping,

    /// Read daemon warmup state without being gated by warmup.
    Readiness,

    /// Shutdown daemon
    Shutdown,

    /// Force refresh - re-scan kitty windows
    Refresh,

    /// Subscribe to events (connection stays open for streaming)
    /// Opens one's ears to the workers below, listening for their continuous reports
    Subscribe {
        /// Event types to receive (empty = all events)
        events: Vec<String>,
    },

    /// Subscribe to the paint stream (connection stays open for streaming).
    ///
    /// Parallel to Subscribe, but the stream carries `PaintEvent`s: UX-level
    /// commands ready to render. Babel is authoritative over color, ring,
    /// scale, outline, workspace CSS class, urgent flag. Clients
    /// (richmon-babel, richspace-babel) forward verbatim to their renderers.
    ///
    /// On connect: full state replay so panel restarts converge instantly.
    /// No filter: the paint stream is the contract; clients ignore strands
    /// they don't care about (richmon ignores Workspace, richspace ignores
    /// Window).
    SubscribePaint,

    /// Get current workspace titles (from authoritative cache)
    Titles,

    /// Resolve the best available title for a window or pane.
    GetTitle { target: TitleTarget },

    /// Force refresh titles for workspace(s)
    /// If workspace is None, refreshes all workspaces with agent panes
    TitleRefresh { workspace: Option<i32> },

    // WSet operations.
    /// Save current state to a WSet
    /// If name is None, saves to current WSet (from _current file)
    WSetSave { name: Option<String> },

    /// Load a WSet, spawning windows for each session
    /// If name is None, loads current WSet (from _current file)
    /// dry_run=true returns what would happen without executing
    WSetLoad { name: Option<String>, dry_run: bool },

    /// List all saved WSet files
    WSetList,

    /// Get current WSet name (from _current file)
    WSetCurrent,

    /// Delete a WSet by name
    WSetDelete { name: String },

    /// Rename a WSet
    WSetRename { old: String, new: String },

    /// Set description for a WSet
    WSetDescribe {
        name: String,
        description: Option<String>,
    },

    /// Solo a single pane for debugging (isolate one pane, hide others)
    /// If `target` is None, disables solo mode (restore all panes).
    Solo {
        #[serde(default)]
        target: Option<PaneSelector>,
    },

    // Hook events: push path from harness hooks.
    /// Push hook state directly into daemon memory, bypassing poll lag.
    /// Sent by `babel hook` CLI handlers after writing to sqlite.
    ///
    /// On SessionStart: also binds kitty_id <-> session_id, eliminating
    /// the need for expensive fingerprint matching.
    HookEvent {
        /// Babel session key, namespaced as harness:native_id.
        session: String,
        /// Kitty pane ID (from $KITTY_WINDOW_ID env var, may be absent)
        kitty_id: Option<u64>,
        /// Full pane address (from $KITTY_LISTEN_ON + $KITTY_WINDOW_ID).
        /// Prefer this over kitty_id whenever present.
        #[serde(default)]
        pane_addr: Option<PaneAddr>,
        /// Tmux pane ID (from $TMUX_PANE env var, e.g. "%5"). When present,
        /// daemon binds this tmux pane to the session — same role as kitty_id
        /// but for the tmux backend.
        #[serde(default)]
        tmux_pane: Option<String>,
        /// Zellij pane ID (from $ZELLIJ_PANE_ID env var, e.g. "terminal_5").
        /// When present, daemon binds this zellij pane to the session.
        #[serde(default)]
        zellij_pane: Option<String>,
        /// Harness that emitted the hook.
        #[serde(default)]
        agent_kind: AgentKind,
        /// Hook state transition
        #[serde(default)]
        hook_state: Option<crate::babel_storage::HookState>,
        /// Visual pulse requested by the harness flow.
        #[serde(default)]
        pulse: PulseEffect,
        /// Read/unread effect requested by the harness flow.
        #[serde(default)]
        read: crate::ReadEffect,
        /// Which hook fired (for telemetry: "stop", "prompt", "pre_tool", etc.)
        hook_type: String,
    },
}

/// Response - Knowledge ascending from the tower.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum Response {
    /// Success with window list
    Windows { windows: Vec<AgentPane> },

    /// Success with all terminal list (not just agents)
    Terminals { terminals: Vec<TerminalInfo> },

    /// Success with raw kitty panes from all sockets
    Panes { panes: Vec<KittyPane> },

    /// Success with socket status information
    /// Keys are socket paths (e.g., "unix:/run/user/1000/kitty.sock-12345")
    Sockets {
        sockets: std::collections::HashMap<String, SocketStatus>,
    },

    /// Success with single window
    /// Boxed to reduce enum size (AgentPane is 432 bytes)
    Window { window: Box<Option<AgentPane>> },

    /// Success with session history
    History { sessions: Vec<SessionInfo> },

    /// Success with scrollback text
    Scrollback { text: String },

    /// Simple success acknowledgment
    Ok { message: String },

    /// Pending input status for a window
    /// TODO: As scrollparse improves, this will include the actual pending text
    /// and support for save/restore operations during broadcast
    PendingInput {
        window_id: u64,
        has_pending: bool,
        /// The pending text if detected (may be empty even if has_pending is true
        /// due to detection limitations)
        pending_text: Option<String>,
    },

    /// Error response
    Error { message: String },

    /// Pong response to ping
    Pong { uptime_secs: u64 },

    /// Daemon warmup state.
    Readiness { readiness: DaemonReadiness },

    /// Subscription acknowledged
    Subscribed { subscriber_id: u64 },

    /// Event notification (sent to subscribers)
    Event { event: EventMessage },

    /// Paint event notification (sent to SubscribePaint subscribers).
    ///
    /// Carries a fully-resolved `PaintEvent`: color hex, ring intensity,
    /// CSS class, etc. Subscribers forward verbatim to their renderers.
    PaintEvent { event: PaintEvent },

    /// Workspace titles response
    /// Keys are workspace numbers as strings (JSON doesn't support integer map keys)
    Titles {
        titles: std::collections::HashMap<String, String>,
    },

    /// Title lookup response for a specific window or pane
    Title { title: Option<String> },

    // WSet responses.
    /// WSet saved successfully (full data)
    WSet { wset: crate::wset::WSet },

    /// WSet saved summary (legacy)
    WSetSaved {
        name: String,
        wspaces: usize,
        windows: usize,
    },

    /// WSet loaded successfully (or dry-run preview)
    WSetLoaded {
        name: String,
        wspaces: usize,
        windows: usize,
        /// Sessions that couldn't be restored (file missing, etc.)
        skipped: Vec<String>,
        /// True if this was a dry-run (no windows spawned)
        dry_run: bool,
    },

    /// List of all WSet summaries
    WSetList {
        wsets: Vec<WSetSummary>,
        current: Option<String>,
    },

    /// Current WSet name
    WSetCurrent { name: Option<String> },
}
