//! BabelCore - Unified API for agent session management
//!
//! The brain of Babel, and the foundation for the Captain's throne. Today it serves
//! CLI commands as thin puppets; tomorrow it will be the Captain's interface to the
//! workers below. Every method here—`panes()`, `focus()`, `send()`—is a lever the
//! Captain will pull. The anima of orchestration flows through this core: queries
//! descend, states ascend, and soon, directives will flow both ways.
//!
//! The core transparently handles:
//!
//! - **Connected mode**: Fast path via IPC to babeld (persistent state, events)
//! - **Local mode**: Initialize state same as daemon, use directly
//!
//! # Architecture
//!
//! ```text
//! BabelState (same structure daemon uses)
//! ├── panes: HashMap<PaneAddr, AgentPane>
//! ├── fingerprint_index, summary_index, etc.
//! └── refresh_panes(), rebuild_*_index(), etc.
//!
//! CLI command:
//!   core = BabelCore::connect()  // init state if no daemon
//!   core.panes()                 // use it
//!   // exit, state drops
//!
//! Daemon:
//!   state = BabelState::new()   // same state
//!   loop { watch → refresh → serve IPC }
//! ```
//!
//! The daemon is literally just "BabelState + event loop + IPC server."
//! Local mode initializes the same state, uses it, and exits.

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{bail, Context, Result};
use tracing::{debug, info, instrument, warn};
use vtr::trace_error;

use crate::babel_storage;
use crate::ipc::{Request, Response};
use crate::kitty::{self, PaneAddr, PaneSelector};
use crate::service::state::{BabelState, TerminalInfo};
use crate::utility::agent_discovery::AgentPane;
use crate::utility::claude_storage::{MigrateResult, SessionInfo};
use crate::utility::ipc::{is_daemon_running, send_request};
use scrollparse::claude::{detect_activity_state, ActivityState};

/// Core API for agent session management
///
/// Automatically selects connected or local mode based on daemon availability.
/// Both modes provide identical API - local mode just initializes state locally.
pub struct BabelCore {
    mode: CoreMode,
}

enum CoreMode {
    /// Connected to babeld - proxy requests via IPC
    Connected,
    /// No daemon - we own the state directly (same structure as daemon)
    Local(BabelState),
}

impl BabelCore {
    /// Connect to babel - tries daemon first, falls back to local state
    ///
    /// In local mode, initializes state the same way daemon does:
    /// - refresh_panes() to discover kitty windows
    /// - rebuild_summary_index() for title matching
    /// - rebuild_fingerprint_index() for scrollback matching
    #[instrument(level = "debug")]
    pub async fn connect() -> Self {
        Self::connect_with_local_refresh(false, true, true, true, None).await
    }

    /// Connect without expensive local scrollback parsing.
    ///
    /// Command-level doctors need a clean evidence report, not a local daemon
    /// imitation that probes every pane and emits scrollparse diagnostics before
    /// the report starts. When babeld is running this still uses its cached state;
    /// when it is not, local mode only gathers pane/cwd structure.
    #[instrument(level = "debug")]
    pub async fn connect_lightweight() -> Self {
        Self::connect_with_local_refresh(true, true, false, true, None).await
    }

    /// Build a local lightweight core even when babeld is running.
    ///
    /// Harness migrations are filesystem transactions, not daemon actions. They
    /// may inspect live pane cwd structure for risk reporting, but must not block
    /// on, mutate through, or inherit stale state from the daemon.
    #[instrument(level = "debug")]
    pub async fn local_lightweight() -> Self {
        Self::connect_with_local_refresh(true, false, false, false, Some(Duration::from_secs(2)))
            .await
    }

    async fn connect_with_local_refresh(
        skip_activity_fetch: bool,
        rebuild_summary_index: bool,
        rebuild_fingerprint_index: bool,
        prefer_daemon: bool,
        refresh_timeout: Option<Duration>,
    ) -> Self {
        if prefer_daemon && is_daemon_running().await {
            debug!("connected to babeld");
            Self {
                mode: CoreMode::Connected,
            }
        } else {
            debug!("daemon not available, initializing local state");
            let mut state = BabelState::new();

            let refresh_result = if let Some(duration) = refresh_timeout {
                match tokio::time::timeout(duration, state.refresh_panes(skip_activity_fetch)).await
                {
                    Ok(result) => result,
                    Err(_) => {
                        warn!("local pane refresh timed out after {:?}", duration);
                        Ok(Vec::new())
                    }
                }
            } else {
                state.refresh_panes(skip_activity_fetch).await
            };
            if let Err(e) = refresh_result {
                trace_error!("pane refresh failed", error = %e);
                warn!("failed to refresh panes: {}", e);
            }
            if rebuild_summary_index {
                if let Err(e) = state.rebuild_summary_index() {
                    trace_error!("summary index rebuild failed", error = %e);
                    warn!("failed to build summary index: {}", e);
                }
            }
            if rebuild_fingerprint_index {
                if let Err(e) = state.rebuild_fingerprint_index() {
                    trace_error!("fingerprint index rebuild failed", error = %e);
                    warn!("failed to build fingerprint index: {}", e);
                }
            }

            Self {
                mode: CoreMode::Local(state),
            }
        }
    }

    /// Check if connected to daemon (vs local/ephemeral mode)
    pub fn is_connected(&self) -> bool {
        matches!(self.mode, CoreMode::Connected)
    }


    // ═══════════════════════════════════════════════════════════════════════════
    // Query Operations (read-only)
    // ═══════════════════════════════════════════════════════════════════════════

    /// Get all agent panes with enriched session data.
    ///
    /// This is Babel's ground-truth agent list: agents live in panes. Use
    /// `window` only for actual platform/X clients or legacy wire names.
    #[tracing::instrument(level = "debug", skip(self))]
    pub async fn panes(&self) -> Result<Vec<AgentPane>> {
        match &self.mode {
            CoreMode::Connected => match send_request(&Request::List).await {
                Ok(Response::Windows { windows }) => Ok(windows),
                Ok(other) => bail!("unexpected response: {:?}", other),
                Err(e) => {
                    trace_error!("IPC request failed", request = "List", error = %e);
                    warn!("daemon request failed: {}", e);
                    bail!("daemon connection failed: {}", e)
                }
            },
            CoreMode::Local(state) => {
                // Direct access to same state structure daemon uses
                Ok(state.panes.values().cloned().collect())
            }
        }
    }

    /// Compatibility alias for older callers. New code should use `panes()`.
    #[tracing::instrument(level = "debug", skip(self))]
    pub async fn windows(&self) -> Result<Vec<AgentPane>> {
        self.panes().await
    }

    /// Get all terminals (not just agent sessions)
    ///
    /// Returns all kitty terminals for visibility into the full terminal flow.
    /// Useful for watching terminals transition to agent sessions.
    #[instrument(level = "debug", skip(self))]
    pub async fn terminals(&self) -> Result<Vec<TerminalInfo>> {
        match &self.mode {
            CoreMode::Connected => match send_request(&Request::ListTerminals).await {
                Ok(Response::Terminals { terminals }) => Ok(terminals),
                Ok(other) => bail!("unexpected response: {:?}", other),
                Err(e) => {
                    trace_error!("IPC request failed", request = "ListTerminals", error = %e);
                    warn!("daemon request failed: {}", e);
                    bail!("daemon connection failed: {}", e)
                }
            },
            CoreMode::Local(state) => {
                // In local mode, we don't track terminals (only daemon does)
                // Return empty list - users should use daemon for full terminal visibility
                Ok(state.terminals.values().cloned().collect())
            }
        }
    }

    /// Get raw kitty panes from all sockets
    ///
    /// Returns raw KittyPane data without babel enrichment. Queries all
    /// responsive kitty instances directly. Useful for low-level kitty inspection.
    #[instrument(level = "debug", skip(self))]
    pub async fn kitty_panes(&self) -> Result<Vec<kitty::KittyPane>> {
        match &self.mode {
            CoreMode::Connected => match send_request(&Request::ListPanes).await {
                Ok(Response::Panes { panes }) => Ok(panes),
                Ok(other) => bail!("unexpected response: {:?}", other),
                Err(e) => {
                    trace_error!("IPC request failed", request = "ListPanes", error = %e);
                    warn!("daemon request failed: {}", e);
                    bail!("daemon connection failed: {}", e)
                }
            },
            CoreMode::Local(_) => {
                // Query kitty directly - same as daemon does
                kitty::list_all_panes().await
            }
        }
    }

    /// Get kitty socket status (multi-instance awareness)
    ///
    /// Returns status for each known kitty socket including responsiveness,
    /// pane count, and whether it's the current socket.
    #[instrument(level = "debug", skip(self))]
    pub async fn sockets(
        &self,
    ) -> Result<std::collections::HashMap<String, crate::service::state::SocketStatus>> {
        match &self.mode {
            CoreMode::Connected => match send_request(&Request::ListSockets).await {
                Ok(Response::Sockets { sockets }) => Ok(sockets),
                Ok(other) => bail!("unexpected response: {:?}", other),
                Err(e) => {
                    trace_error!("IPC request failed", request = "ListSockets", error = %e);
                    warn!("daemon request failed: {}", e);
                    bail!("daemon connection failed: {}", e)
                }
            },
            CoreMode::Local(state) => {
                // Return socket status from local state
                Ok(state.socket_status.clone())
            }
        }
    }

    /// Get agent panes with fingerprints extracted from scrollback.
    #[instrument(level = "debug", skip(self))]
    pub async fn panes_with_fingerprints(&self) -> Result<Vec<AgentPane>> {
        match &self.mode {
            CoreMode::Connected => match send_request(&Request::ListWithFingerprints).await {
                Ok(Response::Windows { windows }) => Ok(windows),
                Ok(other) => bail!("unexpected response: {:?}", other),
                Err(e) => {
                    trace_error!("IPC request failed", request = "ListWithFingerprints", error = %e);
                    warn!("daemon request failed: {}", e);
                    bail!("daemon connection failed: {}", e)
                }
            },
            CoreMode::Local(state) => {
                // Panes already have fingerprints from state initialization
                Ok(state.panes.values().cloned().collect())
            }
        }
    }

    /// Compatibility alias for older callers. New code should use `panes_with_fingerprints()`.
    #[instrument(level = "debug", skip(self))]
    pub async fn windows_with_fingerprints(&self) -> Result<Vec<AgentPane>> {
        self.panes_with_fingerprints().await
    }

    /// Get a specific agent pane by ID, or focused pane if None.
    #[instrument(level = "debug", skip(self))]
    pub async fn pane(&self, pane_id: Option<u64>) -> Result<Option<AgentPane>> {
        let target = pane_id.map(PaneSelector::from);
        match &self.mode {
            CoreMode::Connected => match send_request(&Request::Status { target }).await {
                Ok(Response::Window { window }) => Ok(*window),
                Ok(other) => bail!("unexpected response: {:?}", other),
                Err(e) => {
                    trace_error!("IPC request failed", request = "Status", error = %e);
                    warn!("daemon request failed: {}", e);
                    bail!("daemon connection failed: {}", e)
                }
            },
            CoreMode::Local(state) => match pane_id {
                Some(id) => Ok(state.find_pane_by_id(id).cloned()),
                None => Ok(state.panes.values().find(|w| w.is_focused).cloned()),
            },
        }
    }

    /// Compatibility alias for older callers. New code should use `pane()`.
    #[instrument(level = "debug", skip(self))]
    pub async fn window(&self, pane_id: Option<u64>) -> Result<Option<AgentPane>> {
        self.pane(pane_id).await
    }

    /// Address-aware variant of `pane`. Resolves to the canonical PaneAddr.
    #[instrument(level = "debug", skip(self))]
    pub async fn pane_addr(&self, addr: &PaneAddr) -> Result<Option<AgentPane>> {
        match &self.mode {
            CoreMode::Connected => {
                let target = Some(PaneSelector::from(addr));
                match send_request(&Request::Status { target }).await {
                    Ok(Response::Window { window }) => Ok(*window),
                    Ok(other) => bail!("unexpected response: {:?}", other),
                    Err(e) => {
                        trace_error!("IPC request failed", request = "Status", addr = %addr, error = %e);
                        bail!("daemon connection failed: {}", e)
                    }
                }
            }
            CoreMode::Local(state) => Ok(state.panes.get(addr).cloned()),
        }
    }

    /// Get session history
    #[instrument(level = "debug", skip(self))]
    pub async fn history(&self, limit: usize) -> Result<Vec<SessionInfo>> {
        match &self.mode {
            CoreMode::Connected => match send_request(&Request::History { limit }).await {
                Ok(Response::History { sessions }) => Ok(sessions),
                Ok(other) => bail!("unexpected response: {:?}", other),
                Err(e) => {
                    trace_error!("IPC request failed", request = "History", error = %e);
                    warn!("daemon request failed: {}", e);
                    bail!("daemon connection failed: {}", e)
                }
            },
            CoreMode::Local(_state) => {
                // Direct file access - same as daemon does internally
                crate::utility::claude_storage::get_recent_sessions(limit)
            }
        }
    }

    /// Get scrollback text from a pane (legacy bare-id edge).
    #[instrument(level = "debug", skip(self))]
    pub async fn scrollback(&self, pane_id: u64, lines: Option<usize>) -> Result<String> {
        self.scrollback_with(PaneSelector::from(pane_id), lines)
            .await
    }

    /// Address-aware scrollback fetch for daemon-resolved targets.
    #[instrument(level = "debug", skip(self))]
    pub async fn scrollback_addr(&self, addr: &PaneAddr, lines: Option<usize>) -> Result<String> {
        self.scrollback_with(PaneSelector::from(addr), lines).await
    }

    async fn scrollback_with(&self, target: PaneSelector, lines: Option<usize>) -> Result<String> {
        let pane_id = target.id();
        let text = match &self.mode {
            CoreMode::Connected => {
                match send_request(&Request::Scroll {
                    target: target.clone(),
                })
                .await
                {
                    Ok(Response::Scrollback { text }) => text,
                    Ok(other) => bail!("unexpected response: {:?}", other),
                    Err(e) => {
                        trace_error!("IPC request failed", request = "Scroll", target = %target, error = %e);
                        warn!("daemon request failed: {}", e);
                        bail!("daemon connection failed: {}", e)
                    }
                }
            }
            CoreMode::Local(_) => match target.addr() {
                Some(addr) => kitty::get_scrollback_on_socket(&addr.socket, addr.id).await?,
                None => kitty::get_scrollback(pane_id).await?,
            },
        };

        if let Some(n) = lines {
            let limited: Vec<&str> = text.lines().rev().take(n).collect();
            Ok(limited.into_iter().rev().collect::<Vec<_>>().join("\n"))
        } else {
            Ok(text)
        }
    }

    // ═══════════════════════════════════════════════════════════════════════════
    // Mutation Operations (state-changing)
    // ═══════════════════════════════════════════════════════════════════════════

    /// Focus a pane (legacy bare-id edge).
    #[instrument(level = "debug", skip(self))]
    pub async fn focus(&self, pane_id: u64) -> Result<()> {
        self.focus_target(PaneSelector::from(pane_id)).await
    }

    /// Focus a pane by canonical address.
    #[instrument(level = "debug", skip(self))]
    pub async fn focus_addr(&self, addr: &PaneAddr) -> Result<()> {
        self.focus_target(PaneSelector::from(addr)).await
    }

    async fn focus_target(&self, target: PaneSelector) -> Result<()> {
        match &self.mode {
            CoreMode::Connected => {
                match send_request(&Request::Focus {
                    target: target.clone(),
                })
                .await
                {
                    Ok(Response::Ok { .. }) => Ok(()),
                    Ok(Response::Error { message }) => {
                        trace_error!("focus failed", target = %target, error = %message);
                        bail!("{}", message)
                    }
                    Ok(other) => bail!("unexpected response: {:?}", other),
                    Err(e) => {
                        trace_error!("IPC request failed", request = "Focus", target = %target, error = %e);
                        bail!("daemon connection failed: {}", e)
                    }
                }
            }
            CoreMode::Local(_) => match target.addr() {
                Some(addr) => kitty::focus_pane_on_socket(&addr.socket, addr.id).await,
                None => kitty::focus_pane(target.id()).await,
            },
        }
    }

    /// Send text to a pane (with Enter/CR at end). Legacy bare-id edge.
    ///
    /// The text is appended with a carriage return to submit it to the agent.
    #[instrument(level = "debug", skip(self, text))]
    pub async fn send(&self, pane_id: u64, text: &str) -> Result<()> {
        self.send_target(PaneSelector::from(pane_id), text).await
    }

    /// Send text to a pane addressed canonically.
    #[instrument(level = "debug", skip(self, text))]
    pub async fn send_addr(&self, addr: &PaneAddr, text: &str) -> Result<()> {
        self.send_target(PaneSelector::from(addr), text).await
    }

    async fn send_target(&self, target: PaneSelector, text: &str) -> Result<()> {
        match &self.mode {
            CoreMode::Connected => {
                match send_request(&Request::Send {
                    target,
                    text: text.to_string(),
                })
                .await
                {
                    Ok(Response::Ok { .. }) => Ok(()),
                    Ok(Response::Error { message }) => bail!("{}", message),
                    Ok(other) => bail!("unexpected response: {:?}", other),
                    Err(e) => bail!("daemon connection failed: {}", e),
                }
            }
            CoreMode::Local(_) => {
                let text_with_cr = format!("{}\r", text);
                match target.addr() {
                    Some(addr) => {
                        kitty::send_text_on_socket(&addr.socket, addr.id, &text_with_cr).await
                    }
                    None => kitty::send_text(target.id(), &text_with_cr).await,
                }
            }
        }
    }

    /// Type text to a pane (without Enter/CR at end). Legacy bare-id edge.
    ///
    /// Types the text into the input area without submitting. Useful for
    /// composing prompts incrementally or staging input.
    #[instrument(level = "debug", skip(self, text))]
    pub async fn type_text(&self, pane_id: u64, text: &str) -> Result<()> {
        self.type_text_target(PaneSelector::from(pane_id), text)
            .await
    }

    /// Type text without submitting, addressed canonically.
    #[instrument(level = "debug", skip(self, text))]
    pub async fn type_text_addr(&self, addr: &PaneAddr, text: &str) -> Result<()> {
        self.type_text_target(PaneSelector::from(addr), text).await
    }

    async fn type_text_target(&self, target: PaneSelector, text: &str) -> Result<()> {
        match &self.mode {
            CoreMode::Connected => {
                match send_request(&Request::Type {
                    target,
                    text: text.to_string(),
                })
                .await
                {
                    Ok(Response::Ok { .. }) => Ok(()),
                    Ok(Response::Error { message }) => bail!("{}", message),
                    Ok(other) => bail!("unexpected response: {:?}", other),
                    Err(e) => bail!("daemon connection failed: {}", e),
                }
            }
            CoreMode::Local(_) => match target.addr() {
                Some(addr) => kitty::send_text_on_socket(&addr.socket, addr.id, text).await,
                None => kitty::send_text(target.id(), text).await,
            },
        }
    }

    /// Check if a pane has pending (unsent) input in the textbox.
    ///
    /// Returns (has_pending, pending_text) where pending_text may be None
    /// even if has_pending is true (due to detection limitations).
    ///
    /// TODO: As scrollparse improves, this will become more reliable and
    /// will support extracting the actual pending text for save/restore
    /// operations during broadcast.
    #[instrument(level = "debug", skip(self))]
    pub async fn has_pending_input(&self, pane_id: u64) -> Result<(bool, Option<String>)> {
        self.has_pending_input_target(PaneSelector::from(pane_id))
            .await
    }

    /// Address-aware variant of `has_pending_input`.
    #[instrument(level = "debug", skip(self))]
    pub async fn has_pending_input_addr(&self, addr: &PaneAddr) -> Result<(bool, Option<String>)> {
        self.has_pending_input_target(PaneSelector::from(addr))
            .await
    }

    async fn has_pending_input_target(
        &self,
        target: PaneSelector,
    ) -> Result<(bool, Option<String>)> {
        match &self.mode {
            CoreMode::Connected => {
                match send_request(&Request::HasPendingInput {
                    target: target.clone(),
                })
                .await
                {
                    Ok(Response::PendingInput {
                        has_pending,
                        pending_text,
                        ..
                    }) => Ok((has_pending, pending_text)),
                    Ok(Response::Error { message }) => bail!("{}", message),
                    Ok(other) => bail!("unexpected response: {:?}", other),
                    Err(e) => bail!("daemon connection failed: {}", e),
                }
            }
            CoreMode::Local(_) => {
                let scrollback = match target.addr() {
                    Some(addr) => kitty::get_scrollback_on_socket(&addr.socket, addr.id).await?,
                    None => kitty::get_scrollback(target.id()).await?,
                };
                let (has_pending, pending_text) = detect_pending_input(&scrollback);
                Ok((has_pending, pending_text))
            }
        }
    }

    /// Set icon/tag for a pane (legacy bare-id edge).
    #[instrument(level = "debug", skip(self))]
    pub async fn set_icon(&self, pane_id: u64, icon: &str) -> Result<()> {
        self.set_icon_target(PaneSelector::from(pane_id), icon)
            .await
    }

    /// Set icon/tag for a pane addressed canonically.
    #[instrument(level = "debug", skip(self))]
    pub async fn set_icon_addr(&self, addr: &PaneAddr, icon: &str) -> Result<()> {
        self.set_icon_target(PaneSelector::from(addr), icon).await
    }

    async fn set_icon_target(&self, target: PaneSelector, icon: &str) -> Result<()> {
        match &self.mode {
            CoreMode::Connected => {
                match send_request(&Request::Tag {
                    target,
                    icon: icon.to_string(),
                })
                .await
                {
                    Ok(Response::Ok { .. }) => Ok(()),
                    Ok(Response::Error { message }) => bail!("{}", message),
                    Ok(other) => bail!("unexpected response: {:?}", other),
                    Err(e) => bail!("daemon connection failed: {}", e),
                }
            }
            CoreMode::Local(state) => {
                let pane = match target.addr() {
                    Some(addr) => state.panes.get(addr).cloned(),
                    None => state.find_pane_by_id(target.id()).cloned(),
                };
                if let Some(pane) = &pane {
                    if let Some(session_id) = &pane.session_id {
                        let db = babel_storage::init_db()?;
                        babel_storage::set_icon(&db, session_id, icon)?;
                    }
                }
                match target.addr() {
                    Some(addr) => {
                        kitty::set_user_var_on_socket(&addr.socket, addr.id, "babel_icon", icon)
                            .await?
                    }
                    None => kitty::set_user_var(target.id(), "babel_icon", icon).await?,
                }
                Ok(())
            }
        }
    }

    /// Mark session as read (legacy bare-id edge).
    #[instrument(level = "debug", skip(self))]
    pub async fn mark_read(&self, pane_id: u64) -> Result<()> {
        self.mark_read_target(PaneSelector::from(pane_id)).await
    }

    /// Mark session as read for a canonical address.
    #[instrument(level = "debug", skip(self))]
    pub async fn mark_read_addr(&self, addr: &PaneAddr) -> Result<()> {
        self.mark_read_target(PaneSelector::from(addr)).await
    }

    async fn mark_read_target(&self, target: PaneSelector) -> Result<()> {
        match &self.mode {
            CoreMode::Connected => match send_request(&Request::MarkRead { target }).await {
                Ok(Response::Ok { .. }) => Ok(()),
                Ok(Response::Error { message }) => bail!("{}", message),
                Ok(other) => bail!("unexpected response: {:?}", other),
                Err(e) => bail!("daemon connection failed: {}", e),
            },
            CoreMode::Local(state) => {
                let pane = match target.addr() {
                    Some(addr) => state.panes.get(addr).cloned(),
                    None => state.find_pane_by_id(target.id()).cloned(),
                };
                if let Some(pane) = pane {
                    if let Some(session_id) = &pane.session_id {
                        let db = babel_storage::init_db()?;
                        babel_storage::mark_read(&db, session_id)?;
                    }
                }
                Ok(())
            }
        }
    }

    /// Set a pane's kitty title.
    #[instrument(level = "debug", skip(self))]
    pub async fn set_title(&self, pane_id: u64, title: &str) -> Result<()> {
        kitty::set_pane_title(pane_id, title).await
    }

    /// Set or clear solo mode (isolate a single pane for debugging).
    ///
    /// When pane_id is Some, only that pane is shown by `ls` and similar.
    /// When pane_id is None, solo mode is disabled and all panes are shown.
    #[instrument(level = "debug", skip(self))]
    pub async fn solo(&self, pane_id: Option<u64>) -> Result<()> {
        let target = pane_id.map(PaneSelector::from);
        match &self.mode {
            CoreMode::Connected => match send_request(&Request::Solo { target }).await {
                Ok(Response::Ok { message }) => {
                    info!("{}", message);
                    Ok(())
                }
                Ok(Response::Error { message }) => bail!("{}", message),
                Ok(other) => bail!("unexpected response: {:?}", other),
                Err(e) => bail!("daemon connection failed: {}", e),
            },
            CoreMode::Local(_state) => {
                // Local mode doesn't support solo - would need mutable state
                bail!("Solo mode requires daemon to be running")
            }
        }
    }

    // ═══════════════════════════════════════════════════════════════════════════
    // WSet Operations
    // ═══════════════════════════════════════════════════════════════════════════

    /// Save current workspace layout
    #[instrument(level = "debug", skip(self))]
    pub async fn wset_save(&self, name: Option<String>) -> Result<crate::wset::WSet> {
        match &self.mode {
            CoreMode::Connected => {
                match send_request(&Request::WSetSave { name: name.clone() }).await {
                    Ok(Response::WSet { wset }) => Ok(wset),
                    Ok(Response::Error { message }) => bail!("{}", message),
                    Ok(other) => bail!("unexpected response: {:?}", other),
                    Err(e) => bail!("daemon connection failed: {}", e),
                }
            }
            CoreMode::Local(state) => {
                let windows: Vec<_> = state.panes.values().cloned().collect();
                let wset_name = name.unwrap_or_else(|| "unnamed".to_string());
                let mut wset =
                    crate::wset::WSet::from_windows(&wset_name, &windows, &state.workspace_titles);
                wset.save()?;
                Ok(wset)
            }
        }
    }

    /// List saved workspace sets
    #[instrument(level = "debug", skip(self))]
    pub async fn wset_list(&self) -> Result<Vec<crate::wset::WSetSummary>> {
        match &self.mode {
            CoreMode::Connected => match send_request(&Request::WSetList).await {
                Ok(Response::WSetList { wsets, .. }) => Ok(wsets),
                Ok(other) => bail!("unexpected response: {:?}", other),
                Err(e) => bail!("daemon connection failed: {}", e),
            },
            CoreMode::Local(_) => Ok(crate::wset::list_wsets()?),
        }
    }

    /// Get current wset name
    #[instrument(level = "debug", skip(self))]
    pub async fn wset_current(&self) -> Result<Option<String>> {
        match &self.mode {
            CoreMode::Connected => match send_request(&Request::WSetCurrent).await {
                Ok(Response::WSetCurrent { name }) => Ok(name),
                Ok(other) => bail!("unexpected response: {:?}", other),
                Err(e) => bail!("daemon connection failed: {}", e),
            },
            CoreMode::Local(_) => Ok(crate::wset::get_current_wset_name()?),
        }
    }

    /// Load a workspace set, spawning windows for each session
    ///
    /// This closes all existing agent panes and spawns new ones from the WSet.
    /// Returns information about what was loaded and any sessions that couldn't be restored.
    #[instrument(level = "debug", skip(self))]
    pub async fn wset_load(
        &mut self,
        name: Option<String>,
        dry_run: bool,
    ) -> Result<WSetLoadResult> {
        match &mut self.mode {
            CoreMode::Connected => {
                match send_request(&Request::WSetLoad {
                    name: name.clone(),
                    dry_run,
                })
                .await
                {
                    Ok(Response::WSetLoaded {
                        name,
                        wspaces,
                        windows,
                        skipped,
                        dry_run,
                    }) => Ok(WSetLoadResult {
                        name,
                        wspaces,
                        windows,
                        skipped,
                        dry_run,
                    }),
                    Ok(Response::Error { message }) => bail!("{}", message),
                    Ok(other) => bail!("unexpected response: {:?}", other),
                    Err(e) => bail!("daemon connection failed: {}", e),
                }
            }
            CoreMode::Local(state) => {
                // Load the wset file
                let wset_name = match name {
                    Some(n) => n,
                    None => crate::wset::get_current_wset_name()?
                        .ok_or_else(|| anyhow::anyhow!("No current WSet set"))?,
                };
                let wset = crate::wset::WSet::load(&wset_name)?;

                if dry_run {
                    return Ok(WSetLoadResult {
                        name: wset_name,
                        wspaces: wset.wspaces.len(),
                        windows: wset.window_count(),
                        skipped: vec![],
                        dry_run: true,
                    });
                }

                // Use the impl function from agent_discovery
                let skipped = crate::utility::agent_discovery::load_wset(&wset).await?;

                // Refresh state after loading (quick, activity via periodic poll)
                if let Err(e) = state.refresh_panes(true).await {
                    trace_error!("pane refresh after wset load failed", wset = %wset_name, error = %e);
                    warn!("failed to refresh panes after wset load: {}", e);
                }

                Ok(WSetLoadResult {
                    name: wset_name,
                    wspaces: wset.wspaces.len(),
                    windows: wset.window_count(),
                    skipped,
                    dry_run: false,
                })
            }
        }
    }

    // ═══════════════════════════════════════════════════════════════════════════
    // Working Directory Resolution
    // ═══════════════════════════════════════════════════════════════════════════

    /// Resolve working directory using smart detection
    ///
    /// Uses the detection waterfall:
    /// 1. Explicit path (if provided and exists)
    /// 2. Kitty focused window's cwd
    /// 3. X11 focused window's process cwd
    /// 4. Project path from process cmdline (.venv, --project)
    /// 5. Path from window title (IDE patterns)
    /// 6. Fallback: ~/Workspace
    pub fn resolve_workdir(explicit: Option<&str>) -> PathBuf {
        crate::utility::workdir::resolve(explicit)
    }

    // ═══════════════════════════════════════════════════════════════════════════
    // Detached Agent Spawning
    // ═══════════════════════════════════════════════════════════════════════════

    /// Spawn the current provider in a detached background process.
    ///
    /// Returns the child PID (best-effort detection).
    /// The process runs fully detached via setsid.
    ///
    /// # Arguments
    /// * `cwd` - Working directory for the session
    /// * `args` - Arguments to pass to claude (e.g., `["-p", "my prompt"]`)
    pub async fn spawn_detached_claude(cwd: &Path, args: &[&str]) -> Result<u32> {
        use std::process::Command;

        // Build claude command with args
        // SHELL=/usr/bin/bash: Claude Code doesn't support zsh
        let args_str = args
            .iter()
            .map(|a| format!("'{}'", a.replace('\'', "'\\''")))
            .collect::<Vec<_>>()
            .join(" ");

        let claude_script = format!(
            "cd '{}' && SHELL=/usr/bin/bash claude {} </dev/null >/dev/null 2>&1",
            cwd.display(),
            args_str
        );

        let child = Command::new("setsid")
            .arg("bash")
            .arg("-c")
            .arg(&claude_script)
            .spawn()
            .context("failed to spawn detached claude")?;

        let spawn_pid = child.id();

        // Small delay to let process start
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;

        // Try to find actual claude PID (best effort)
        // Fall back to spawn PID if detection fails
        let pid = if !args.is_empty() {
            let search = args[0].chars().take(20).collect::<String>();
            find_claude_pid(&search).unwrap_or(spawn_pid)
        } else {
            spawn_pid
        };

        Ok(pid)
    }

    // ═══════════════════════════════════════════════════════════════════════════
    // Fire-and-Forget Sessions
    // ═══════════════════════════════════════════════════════════════════════════

    /// Fire a prompt to an agent in a detached background session
    ///
    /// Combines workdir resolution, detached spawning, and task tracking.
    /// This is the high-level API for fire-and-forget prompts.
    ///
    /// # Arguments
    /// * `prompt` - The prompt to send to the agent
    /// * `workdir` - Optional working directory (resolved automatically if None)
    /// * `ambient_sound` - Optional ambient sound name to associate with task
    #[instrument(level = "debug", skip(self, prompt))]
    pub async fn fire(
        &mut self,
        prompt: &str,
        workdir: Option<&Path>,
        ambient_sound: Option<String>,
    ) -> Result<crate::fire::FiredTask> {
        use crate::fire::{track_task, FiredTask};

        // Resolve working directory
        let cwd = match workdir {
            Some(p) => p.to_path_buf(),
            None => Self::resolve_workdir(None),
        };

        info!(?cwd, prompt_len = prompt.len(), "firing agent session");

        // Spawn detached claude with prompt
        let pid = Self::spawn_detached_claude(&cwd, &["-p", prompt]).await?;

        // Create and track task
        let task = FiredTask {
            task_id: FiredTask::new_id(),
            pid,
            prompt_preview: prompt.chars().take(60).collect(),
            workdir: cwd,
            ambient_sound,
        };

        track_task(&task)?;

        info!(task_id = %task.task_id, pid = task.pid, "claude fire tracked");

        Ok(task)
    }

    /// List all currently running fired tasks
    pub fn fired_tasks() -> Result<Vec<crate::fire::FiredTask>> {
        crate::fire::list_running_tasks()
    }

    /// Clean up finished fired tasks
    pub fn cleanup_fired() -> Result<usize> {
        crate::fire::cleanup_finished_tasks()
    }

    // ═══════════════════════════════════════════════════════════════════════════
    // Session Spawning
    // ═══════════════════════════════════════════════════════════════════════════

    /// Spawn an agent session in a new kitty window
    ///
    /// Uses `kitty-claude` script for consistent window setup. Returns the new
    /// window ID if found (may be None if window spawns but can't be located).
    ///
    /// In connected mode, delegates to daemon. In local mode, spawns directly
    /// and refreshes state.
    #[instrument(level = "debug", skip(self))]
    pub async fn spawn_session(
        &mut self,
        session_id: &str,
        cwd: &Path,
    ) -> Result<Option<AgentPane>> {
        match &mut self.mode {
            CoreMode::Connected => {
                // Daemon handles spawning - use WSetLoad with single session as workaround
                // TODO: Add dedicated SpawnSession IPC request
                warn!("spawn_session in connected mode not yet implemented via IPC");
                bail!("spawn_session requires local mode or daemon support (coming soon)")
            }
            CoreMode::Local(state) => {
                // Direct spawn using the impl function
                let pane_id =
                    crate::utility::agent_discovery::spawn_agent_session(session_id, cwd).await?;

                // Refresh state to pick up the new pane (quick, activity via periodic poll)
                if let Err(e) = state.refresh_panes(true).await {
                    trace_error!("pane refresh after spawn failed", session_id = session_id, error = %e);
                    warn!("failed to refresh panes after spawn: {}", e);
                }

                // Return the pane if found
                match pane_id {
                    Some(id) => Ok(state.find_pane_by_id(id).cloned()),
                    None => Ok(None),
                }
            }
        }
    }

    // ═══════════════════════════════════════════════════════════════════════════
    // State Detection
    // ═══════════════════════════════════════════════════════════════════════════

    /// Get the current state of an agent session (idle, thinking, tool use, etc.)
    ///
    /// Analyzes the pane's scrollback to determine what the agent is currently doing.
    #[instrument(level = "debug", skip(self))]
    pub async fn get_pane_state(&self, pane_id: u64) -> Result<ActivityState> {
        let scrollback = self.scrollback(pane_id, Some(50)).await?;
        Ok(detect_activity_state(&scrollback))
    }

    /// Compatibility alias for older callers. New code should use `get_pane_state()`.
    #[instrument(level = "debug", skip(self))]
    pub async fn get_window_state(&self, pane_id: u64) -> Result<ActivityState> {
        self.get_pane_state(pane_id).await
    }

    /// Find all agent panes whose cwd is inside the given path.
    ///
    /// Returns panes along with their current state and relative path from source.
    /// Used by migration to detect affected terminals.
    #[instrument(level = "debug", skip(self))]
    pub async fn find_panes_in_path(&self, source: &Path) -> Result<Vec<ConflictingPane>> {
        // Canonicalize source path for accurate comparison
        let source = source
            .canonicalize()
            .unwrap_or_else(|_| std::env::current_dir().unwrap().join(source));

        let panes = self.panes().await?;
        let mut conflicts = Vec::new();

        for pane in panes {
            if pane.cwd.starts_with(&source) {
                let state = self
                    .get_pane_state(pane.id())
                    .await
                    .unwrap_or(ActivityState::Unknown);

                let relative_path = pane
                    .cwd
                    .strip_prefix(&source)
                    .unwrap_or(Path::new(""))
                    .to_path_buf();

                conflicts.push(ConflictingPane {
                    pane,
                    state,
                    relative_path,
                });
            }
        }

        Ok(conflicts)
    }

    /// Compatibility alias for older callers. New code should use `find_panes_in_path()`.
    #[instrument(level = "debug", skip(self))]
    pub async fn find_windows_in_path(&self, source: &Path) -> Result<Vec<ConflictingPane>> {
        self.find_panes_in_path(source).await
    }

    // ═══════════════════════════════════════════════════════════════════════════
    // Terminal Migration
    // ═══════════════════════════════════════════════════════════════════════════

    /// Migrate an idle terminal to a new working directory
    ///
    /// Sends: Ctrl-C (ensure clean prompt) → cd <new_path> → claude -r <session_id>
    /// This allows the terminal to continue working after a directory move.
    #[instrument(level = "debug", skip(self))]
    pub async fn migrate_terminal(
        &self,
        pane_id: u64,
        new_cwd: &Path,
        session_id: Option<&str>,
    ) -> Result<()> {
        use std::time::Duration;
        use tokio::time::sleep;

        // Ctrl-C to ensure clean prompt
        self.send(pane_id, "\x03").await?;
        sleep(Duration::from_millis(100)).await;

        // cd to new directory
        let cd_cmd = format!("cd {}\n", shell_escape(new_cwd));
        self.send(pane_id, &cd_cmd).await?;
        sleep(Duration::from_millis(50)).await;

        // Resume session if we have the ID
        if let Some(sid) = session_id {
            let resume_cmd = format!("claude -r {}\n", sid);
            self.send(pane_id, &resume_cmd).await?;
        }

        Ok(())
    }

    // ═══════════════════════════════════════════════════════════════════════════
    // Project Migration
    // ═══════════════════════════════════════════════════════════════════════════

    /// Migrate a project directory, updating Claude's storage and active terminals
    ///
    /// This is the unified migration operation that:
    /// 1. Migrates idle terminals to the new path (if migrate_terminals is true)
    /// 2. Renames the project folder in ~/.claude/projects/
    /// 3. Updates path references in ~/.claude/history.jsonl
    /// 4. Refreshes internal state to reflect changes
    ///
    /// Returns the migration result and list of windows that were migrated.
    #[instrument(level = "debug", skip(self))]
    pub async fn migrate_project(
        &mut self,
        old_path: &Path,
        new_path: &Path,
        options: MigrateOptions,
    ) -> Result<MigrateOutcome> {
        info!(
            ?old_path,
            ?new_path,
            dry_run = options.dry_run,
            "starting project migration"
        );

        // Canonicalize paths
        let old_canonical = old_path
            .canonicalize()
            .unwrap_or_else(|_| old_path.to_path_buf());
        let new_canonical = new_path.canonicalize().unwrap_or_else(|_| {
            if new_path.is_absolute() {
                new_path.to_path_buf()
            } else {
                std::env::current_dir()
                    .map(|cwd| cwd.join(new_path))
                    .unwrap_or_else(|_| new_path.to_path_buf())
            }
        });

        // Find conflicting panes
        let conflicts = self.find_panes_in_path(&old_canonical).await?;

        // Partition by migratable state
        // PlanApproval is considered migratable (waiting for user decision, not actively processing)
        let (migratable, active): (Vec<_>, Vec<_>) = conflicts.iter().partition(|c| {
            matches!(
                c.state,
                ActivityState::Idle | ActivityState::AwaitingInput | ActivityState::PlanApproval
            )
        });

        // Check for blocking active panes
        if !active.is_empty() && !options.force {
            bail!(
                "{} active agent session(s) in source path would break. \
                Use force=true to proceed anyway.",
                active.len()
            );
        }

        let mut migrated_terminals = Vec::new();

        // Migrate idle terminals (before moving the directory)
        if options.migrate_terminals && !options.dry_run {
            for conflict in &migratable {
                let new_cwd = new_canonical.join(&conflict.relative_path);
                let session_id = conflict.pane.session_id.as_deref();

                if let Err(e) = self
                    .migrate_terminal(conflict.pane.id(), &new_cwd, session_id)
                    .await
                {
                    trace_error!("terminal migration failed", pane_id = conflict.pane.id(), new_cwd = %new_cwd.display(), error = %e);
                    warn!(
                        pane_id = conflict.pane.id(),
                        error = %e,
                        "failed to migrate terminal"
                    );
                } else {
                    migrated_terminals.push(conflict.pane.id());
                }
            }
        }

        // Move the physical directory (if requested and source exists)
        let mut directory_moved = false;
        if options.move_directory && old_canonical.exists() && !options.dry_run {
            // Try rename first (same filesystem)
            if std::fs::rename(&old_canonical, &new_canonical).is_err() {
                debug!("rename failed, falling back to copy+delete");
                copy_dir_recursive(&old_canonical, &new_canonical).with_context(|| {
                    format!(
                        "Failed to copy {} → {}",
                        old_canonical.display(),
                        new_canonical.display()
                    )
                })?;
                std::fs::remove_dir_all(&old_canonical).with_context(|| {
                    format!("Failed to remove source: {}", old_canonical.display())
                })?;
            }
            directory_moved = true;
        }

        // Update Claude storage (project folder + history.jsonl)
        let storage_result = crate::utility::claude_storage::migrate_project(
            &old_canonical,
            &new_canonical,
            options.dry_run,
        )?;

        // Refresh state to reflect the changes
        if !options.dry_run {
            self.refresh().await?;
        }

        Ok(MigrateOutcome {
            storage: storage_result,
            directory_moved,
            terminals_migrated: migrated_terminals,
            active_terminals: active.iter().map(|c| c.pane.id()).collect(),
            dry_run: options.dry_run,
        })
    }

    // ═══════════════════════════════════════════════════════════════════════════
    // State Management
    // ═══════════════════════════════════════════════════════════════════════════

    /// Force refresh of internal state
    ///
    /// In connected mode, tells daemon to refresh. In local mode, re-initializes state.
    #[instrument(level = "debug", skip(self))]
    pub async fn refresh(&mut self) -> Result<()> {
        match &mut self.mode {
            CoreMode::Connected => match send_request(&Request::Refresh).await {
                Ok(Response::Ok { .. }) => Ok(()),
                Ok(Response::Error { message }) => {
                    trace_error!("refresh failed", error = %message);
                    bail!("{}", message)
                }
                Ok(other) => bail!("unexpected response: {:?}", other),
                Err(e) => {
                    trace_error!("IPC request failed", request = "Refresh", error = %e);
                    bail!("daemon connection failed: {}", e)
                }
            },
            CoreMode::Local(state) => {
                // Re-initialize state same as connect() (full refresh)
                if let Err(e) = state.refresh_panes(false).await {
                    trace_error!("pane refresh failed", error = %e);
                    warn!("failed to refresh panes: {}", e);
                }
                if let Err(e) = state.rebuild_summary_index() {
                    trace_error!("summary index rebuild failed", error = %e);
                    warn!("failed to rebuild summary index: {}", e);
                }
                if let Err(e) = state.rebuild_fingerprint_index() {
                    trace_error!("fingerprint index rebuild failed", error = %e);
                    warn!("failed to rebuild fingerprint index: {}", e);
                }
                Ok(())
            }
        }
    }
}

// ═══════════════════════════════════════════════════════════════════════════════
// Migration Types
// ═══════════════════════════════════════════════════════════════════════════════

/// A pane whose cwd conflicts with a migration source path.
pub struct ConflictingPane {
    pub pane: AgentPane,
    pub state: ActivityState,
    /// Path relative to source directory
    pub relative_path: PathBuf,
}

/// Options for project migration
#[derive(Debug, Clone, Default)]
pub struct MigrateOptions {
    /// If true, only report what would happen without making changes
    pub dry_run: bool,
    /// If true, move the physical directory (not just update storage)
    pub move_directory: bool,
    /// If true, migrate idle terminals to the new path
    pub migrate_terminals: bool,
    /// If true, proceed even if active terminals would break
    pub force: bool,
}

/// Outcome of a project migration
#[derive(Debug)]
pub struct MigrateOutcome {
    /// Storage migration result (project folder + history.jsonl)
    pub storage: MigrateResult,
    /// Whether the physical directory was moved
    pub directory_moved: bool,
    /// Pane IDs of terminals that were migrated
    pub terminals_migrated: Vec<u64>,
    /// Pane IDs of active terminals (not migrated)
    pub active_terminals: Vec<u64>,
    /// Whether this was a dry run
    pub dry_run: bool,
}

/// Result of loading a WSet
#[derive(Debug)]
pub struct WSetLoadResult {
    /// Name of the WSet that was loaded
    pub name: String,
    /// Number of workspaces in the WSet
    pub wspaces: usize,
    /// Total number of windows spawned
    pub windows: usize,
    /// Session IDs that couldn't be restored (file missing, etc.)
    pub skipped: Vec<String>,
    /// Whether this was a dry run (no windows actually spawned)
    pub dry_run: bool,
}

// ═══════════════════════════════════════════════════════════════════════════════
// Helper Functions
// ═══════════════════════════════════════════════════════════════════════════════

/// Find a claude process by matching prompt fragment in cmdline
///
/// Best-effort PID detection for tracking fired sessions.
fn find_claude_pid(prompt_fragment: &str) -> Option<u32> {
    use std::process::Command;

    // Use pgrep to find claude processes
    let output = Command::new("pgrep")
        .arg("-f")
        .arg(format!("claude.*{}", prompt_fragment))
        .output()
        .ok()?;

    if !output.status.success() {
        return None;
    }

    // Get the first matching PID
    String::from_utf8(output.stdout)
        .ok()?
        .lines()
        .next()?
        .trim()
        .parse()
        .ok()
}

/// Escape a path for safe shell usage
fn shell_escape(path: &Path) -> String {
    let s = path.to_string_lossy();
    if s.contains(' ') || s.contains('\'') || s.contains('"') || s.contains('$') {
        format!("'{}'", s.replace('\'', "'\\''"))
    } else {
        s.to_string()
    }
}

/// Recursively copy a directory (for cross-filesystem moves)
fn copy_dir_recursive(src: &Path, dst: &Path) -> Result<()> {
    std::fs::create_dir_all(dst)?;

    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let src_path = entry.path();
        let dst_path = dst.join(entry.file_name());

        if src_path.is_dir() {
            copy_dir_recursive(&src_path, &dst_path)?;
        } else {
            std::fs::copy(&src_path, &dst_path)?;
        }
    }

    Ok(())
}

// ═══════════════════════════════════════════════════════════════════════════════
// Pending Input Detection
// ═══════════════════════════════════════════════════════════════════════════════

/// Detect if there's pending (unsent) input in a Claude Code terminal
///
/// Analyzes the last line of scrollback to detect text after the prompt.
/// Returns (has_pending, pending_text) where pending_text is the actual text
/// if it can be extracted.
///
/// # Detection Strategy
///
/// Claude Code's prompt is typically `> ` at the start of a line. If there's
/// text after `> ` on the last non-empty line, that's pending input.
///
/// # Current Limitations
///
/// - Only detects simple `> text` patterns
/// - May miss multiline input or complex prompt states
/// - Cannot distinguish between prompt and shell output in some edge cases
///
/// TODO: Integrate with scrollparse for more robust detection:
/// - Detect multiline input (continuation prompts)
/// - Handle plan mode input (`y/n`, selection UI)
/// - Support save/restore of pending input during broadcast
/// - Detect input in different prompt modes (shell, edit, etc.)
fn detect_pending_input(scrollback: &str) -> (bool, Option<String>) {
    // Get the last non-empty lines
    let lines: Vec<&str> = scrollback
        .lines()
        .rev()
        .filter(|l| !l.trim().is_empty())
        .take(3)
        .collect();

    if lines.is_empty() {
        return (false, None);
    }

    let last_line = lines[0].trim();

    // Check for Claude Code prompt with content: `> something`
    // The prompt is `> ` followed by user input
    if let Some(content) = last_line.strip_prefix("> ") {
        let trimmed = content.trim();
        if !trimmed.is_empty() {
            return (true, Some(trimmed.to_string()));
        }
        // Just `> ` with no content - no pending input
        return (false, None);
    }

    // Check for bare prompt endings that might have content
    // These are edge cases where the prompt indicator is at the end
    if last_line.ends_with('>') && !last_line.ends_with("->") {
        // Might be a prompt line without visible cursor content
        // Can't extract the pending text reliably here
        // TODO: scrollparse integration needed for accurate detection
        return (false, None);
    }

    // Check if we're in a state where input is being typed but the line
    // doesn't start with `> ` (possible continuation or shell state)
    //
    // TODO: scrollparse can help here by:
    // - Detecting SessionState (is it at a prompt? in plan mode?)
    // - Parsing the visible UI elements to understand context
    // - Handling continuation lines in multiline input

    (false, None)
}

// ═══════════════════════════════════════════════════════════════════════════════
// Convenience functions for CLI (thin wrappers)
// ═══════════════════════════════════════════════════════════════════════════════

/// Resolve a target specification to window IDs
///
/// This is CLI-specific logic that uses the core API.
pub async fn resolve_target(core: &BabelCore, target: &str) -> Result<Vec<u64>> {
    if target == "*" {
        let windows = core.panes().await?;
        Ok(windows.iter().map(|w| w.id()).collect())
    } else {
        let id = target.parse::<u64>().map_err(|_| {
            anyhow::anyhow!("invalid target '{}': expected window ID or '*'", target)
        })?;
        Ok(vec![id])
    }
}
