//! BabelCore - Unified API for Claude session management
//!
//! This is the "brain" of babel. All CLI commands should be thin puppets
//! that call BabelCore methods. The core transparently handles:
//!
//! - **Connected mode**: Fast path via IPC to babeld (persistent state, events)
//! - **Local mode**: Initialize state same as daemon, use directly
//!
//! # Architecture
//!
//! ```text
//! BabelState (same structure daemon uses)
//! ├── windows: HashMap<u64, ClaudeWindow>
//! ├── fingerprint_index, summary_index, etc.
//! └── refresh_windows(), rebuild_*_index(), etc.
//!
//! CLI command:
//!   core = BabelCore::connect()  // init state if no daemon
//!   core.windows()               // use it
//!   // exit, state drops
//!
//! Daemon:
//!   state = DaemonState::new()   // same state
//!   loop { watch → refresh → serve IPC }
//! ```
//!
//! The daemon is literally just "BabelState + event loop + IPC server."
//! Local mode initializes the same state, uses it, and exits.

use anyhow::{Result, bail};
use tracing::{debug, warn};

use crate::daemon::DaemonState;
use crate::discovery::ClaudeWindow;
use crate::claude_storage::SessionInfo;
use crate::ipc::{send_request, is_daemon_running, Request, Response};
use crate::kitty;
use crate::overlay;

/// Core API for Claude session management
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
    Local(DaemonState),
}

impl BabelCore {
    /// Connect to babel - tries daemon first, falls back to local state
    ///
    /// In local mode, initializes state the same way daemon does:
    /// - refresh_windows() to discover kitty windows
    /// - rebuild_summary_index() for title matching
    /// - rebuild_fingerprint_index() for scrollback matching
    pub async fn connect() -> Self {
        if is_daemon_running().await {
            debug!("connected to babeld");
            Self { mode: CoreMode::Connected }
        } else {
            debug!("daemon not available, initializing local state");
            let mut state = DaemonState::new();

            // Initialize same way daemon does
            if let Err(e) = state.refresh_windows() {
                warn!("failed to refresh windows: {}", e);
            }
            if let Err(e) = state.rebuild_summary_index() {
                warn!("failed to build summary index: {}", e);
            }
            if let Err(e) = state.rebuild_fingerprint_index() {
                warn!("failed to build fingerprint index: {}", e);
            }

            Self { mode: CoreMode::Local(state) }
        }
    }

    /// Check if connected to daemon (vs local mode)
    pub fn is_connected(&self) -> bool {
        matches!(self.mode, CoreMode::Connected)
    }

    // ═══════════════════════════════════════════════════════════════════════════
    // Query Operations (read-only)
    // ═══════════════════════════════════════════════════════════════════════════

    /// Get all Claude windows with enriched session data
    pub async fn windows(&self) -> Result<Vec<ClaudeWindow>> {
        match &self.mode {
            CoreMode::Connected => {
                match send_request(&Request::List).await {
                    Ok(Response::Windows { windows }) => Ok(windows),
                    Ok(other) => bail!("unexpected response: {:?}", other),
                    Err(e) => {
                        warn!("daemon request failed: {}", e);
                        bail!("daemon connection failed: {}", e)
                    }
                }
            }
            CoreMode::Local(state) => {
                // Direct access to same state structure daemon uses
                Ok(state.windows.values().cloned().collect())
            }
        }
    }

    /// Get windows with fingerprints extracted from scrollback
    pub async fn windows_with_fingerprints(&self) -> Result<Vec<ClaudeWindow>> {
        match &self.mode {
            CoreMode::Connected => {
                match send_request(&Request::ListWithFingerprints).await {
                    Ok(Response::Windows { windows }) => Ok(windows),
                    Ok(other) => bail!("unexpected response: {:?}", other),
                    Err(e) => {
                        warn!("daemon request failed: {}", e);
                        bail!("daemon connection failed: {}", e)
                    }
                }
            }
            CoreMode::Local(state) => {
                // Windows already have fingerprints from state initialization
                Ok(state.windows.values().cloned().collect())
            }
        }
    }

    /// Get a specific window by ID, or focused window if None
    pub async fn window(&self, window_id: Option<u64>) -> Result<Option<ClaudeWindow>> {
        match &self.mode {
            CoreMode::Connected => {
                match send_request(&Request::Status { window_id }).await {
                    Ok(Response::Window { window }) => Ok(*window),
                    Ok(other) => bail!("unexpected response: {:?}", other),
                    Err(e) => {
                        warn!("daemon request failed: {}", e);
                        bail!("daemon connection failed: {}", e)
                    }
                }
            }
            CoreMode::Local(state) => {
                match window_id {
                    Some(id) => Ok(state.windows.get(&id).cloned()),
                    None => Ok(state.windows.values().find(|w| w.is_focused).cloned()),
                }
            }
        }
    }

    /// Get session history
    pub async fn history(&self, limit: usize) -> Result<Vec<SessionInfo>> {
        match &self.mode {
            CoreMode::Connected => {
                match send_request(&Request::History { limit }).await {
                    Ok(Response::History { sessions }) => Ok(sessions),
                    Ok(other) => bail!("unexpected response: {:?}", other),
                    Err(e) => {
                        warn!("daemon request failed: {}", e);
                        bail!("daemon connection failed: {}", e)
                    }
                }
            }
            CoreMode::Local(_state) => {
                // Direct file access - same as daemon does internally
                crate::claude_storage::get_recent_sessions(limit)
            }
        }
    }

    /// Get scrollback text from a window
    pub async fn scrollback(&self, window_id: u64, lines: Option<usize>) -> Result<String> {
        let text = match &self.mode {
            CoreMode::Connected => {
                match send_request(&Request::Scroll { window_id }).await {
                    Ok(Response::Scrollback { text }) => text,
                    Ok(other) => bail!("unexpected response: {:?}", other),
                    Err(e) => {
                        warn!("daemon request failed: {}", e);
                        bail!("daemon connection failed: {}", e)
                    }
                }
            }
            CoreMode::Local(_) => {
                kitty::get_scrollback(window_id)?
            }
        };

        // Apply line limit if specified
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

    /// Focus a window
    pub async fn focus(&self, window_id: u64) -> Result<()> {
        match &self.mode {
            CoreMode::Connected => {
                match send_request(&Request::Focus { window_id }).await {
                    Ok(Response::Ok { .. }) => Ok(()),
                    Ok(Response::Error { message }) => bail!("{}", message),
                    Ok(other) => bail!("unexpected response: {:?}", other),
                    Err(e) => bail!("daemon connection failed: {}", e),
                }
            }
            CoreMode::Local(_) => {
                kitty::focus_window(window_id)
            }
        }
    }

    /// Send text to a window
    pub async fn send(&self, window_id: u64, text: &str) -> Result<()> {
        match &self.mode {
            CoreMode::Connected => {
                match send_request(&Request::Send { window_id, text: text.to_string() }).await {
                    Ok(Response::Ok { .. }) => Ok(()),
                    Ok(Response::Error { message }) => bail!("{}", message),
                    Ok(other) => bail!("unexpected response: {:?}", other),
                    Err(e) => bail!("daemon connection failed: {}", e),
                }
            }
            CoreMode::Local(_) => {
                kitty::send_text(window_id, text)
            }
        }
    }

    /// Set icon/tag for a window
    pub async fn set_icon(&self, window_id: u64, icon: &str) -> Result<()> {
        match &self.mode {
            CoreMode::Connected => {
                match send_request(&Request::Tag { window_id, icon: icon.to_string() }).await {
                    Ok(Response::Ok { .. }) => Ok(()),
                    Ok(Response::Error { message }) => bail!("{}", message),
                    Ok(other) => bail!("unexpected response: {:?}", other),
                    Err(e) => bail!("daemon connection failed: {}", e),
                }
            }
            CoreMode::Local(state) => {
                // Get session ID from our state
                if let Some(window) = state.windows.get(&window_id) {
                    if let Some(session_id) = &window.session_id {
                        let db = overlay::init_db()?;
                        overlay::set_icon(&db, session_id, icon)?;
                    }
                }
                // Also set kitty user var for visual feedback
                kitty::set_user_var(window_id, "babel_icon", icon)?;
                Ok(())
            }
        }
    }

    /// Mark session as read
    pub async fn mark_read(&self, window_id: u64) -> Result<()> {
        match &self.mode {
            CoreMode::Connected => {
                match send_request(&Request::MarkRead { window_id }).await {
                    Ok(Response::Ok { .. }) => Ok(()),
                    Ok(Response::Error { message }) => bail!("{}", message),
                    Ok(other) => bail!("unexpected response: {:?}", other),
                    Err(e) => bail!("daemon connection failed: {}", e),
                }
            }
            CoreMode::Local(state) => {
                if let Some(window) = state.windows.get(&window_id) {
                    if let Some(session_id) = &window.session_id {
                        let db = overlay::init_db()?;
                        overlay::mark_read(&db, session_id)?;
                    }
                }
                Ok(())
            }
        }
    }

    /// Set window title
    pub async fn set_title(&self, window_id: u64, title: &str) -> Result<()> {
        kitty::set_window_title(window_id, title)
    }

    // ═══════════════════════════════════════════════════════════════════════════
    // WSet Operations
    // ═══════════════════════════════════════════════════════════════════════════

    /// Save current workspace layout
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
                let windows: Vec<_> = state.windows.values().cloned().collect();
                let wset_name = name.unwrap_or_else(|| "unnamed".to_string());
                let mut wset = crate::wset::WSet::from_windows(&wset_name, &windows, &state.workspace_titles);
                wset.save()?;
                Ok(wset)
            }
        }
    }

    /// List saved workspace sets
    pub async fn wset_list(&self) -> Result<Vec<crate::wset::WSetSummary>> {
        match &self.mode {
            CoreMode::Connected => {
                match send_request(&Request::WSetList).await {
                    Ok(Response::WSetList { wsets, .. }) => Ok(wsets),
                    Ok(other) => bail!("unexpected response: {:?}", other),
                    Err(e) => bail!("daemon connection failed: {}", e),
                }
            }
            CoreMode::Local(_) => {
                Ok(crate::wset::list_wsets()?)
            }
        }
    }

    /// Get current wset name
    pub async fn wset_current(&self) -> Result<Option<String>> {
        match &self.mode {
            CoreMode::Connected => {
                match send_request(&Request::WSetCurrent).await {
                    Ok(Response::WSetCurrent { name }) => Ok(name),
                    Ok(other) => bail!("unexpected response: {:?}", other),
                    Err(e) => bail!("daemon connection failed: {}", e),
                }
            }
            CoreMode::Local(_) => {
                Ok(crate::wset::get_current_wset_name()?)
            }
        }
    }
}

// ═══════════════════════════════════════════════════════════════════════════════
// Convenience functions for CLI (thin wrappers)
// ═══════════════════════════════════════════════════════════════════════════════

/// Resolve a target specification to window IDs
///
/// This is CLI-specific logic that uses the core API.
pub async fn resolve_target(core: &BabelCore, target: &str) -> Result<Vec<u64>> {
    if target == "*" {
        let windows = core.windows().await?;
        Ok(windows.iter().map(|w| w.kitty_id).collect())
    } else {
        let id = target.parse::<u64>()
            .map_err(|_| anyhow::anyhow!("invalid target '{}': expected window ID or '*'", target))?;
        Ok(vec![id])
    }
}
