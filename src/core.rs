//! BabelCore - Unified API for Claude session management
//!
//! This is the "brain" of babel. All CLI commands should be thin puppets
//! that call BabelCore methods. The core transparently handles:
//!
//! - **Daemon mode**: Fast path via IPC to babeld (persistent state, events)
//! - **Ephemeral mode**: Populate state on-demand when daemon unavailable
//!
//! # Architecture
//!
//! ```text
//! ┌─────────────────────────────────────────────────────────────┐
//! │  CLI Layer (dumb puppets)                                   │
//! │  babel ls, babel focus, etc.                                │
//! └─────────────────────────────────────────────────────────────┘
//!                             │
//!                             ▼
//! ┌─────────────────────────────────────────────────────────────┐
//! │  BabelCore                                                  │
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
//! # Usage
//!
//! ```rust,ignore
//! let core = BabelCore::connect().await;
//!
//! // These work identically whether daemon is running or not
//! let windows = core.windows().await?;
//! core.focus(42).await?;
//! let history = core.history(10).await?;
//! ```

use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::RwLock;

use anyhow::{Result, bail};
use tracing::{debug, warn};

use crate::claude_storage::{get_recent_sessions, SessionInfo};
use crate::discovery::{discover_claude_windows, ClaudeWindow};
use crate::fingerprint::{extract_from_scrollback, SessionFingerprint};
use crate::ipc::{send_request, is_daemon_running, Request, Response};
use crate::kitty;
use crate::overlay;

/// Core API for Claude session management
///
/// Automatically selects daemon or ephemeral mode based on availability.
/// All operations go through this unified interface.
pub struct BabelCore {
    mode: CoreMode,
}

enum CoreMode {
    /// Connected to babeld - all operations via IPC
    Daemon,
    /// No daemon - ephemeral state populated on-demand
    Ephemeral(Arc<RwLock<EphemeralState>>),
}

/// Ephemeral state for when daemon is unavailable
///
/// Lazily populated on first access. Unlike DaemonState which maintains
/// everything continuously, this only loads what's requested.
#[derive(Default)]
struct EphemeralState {
    /// Cached windows (populated on first windows() call)
    windows: Option<Vec<ClaudeWindow>>,
    /// Cached history (populated on first history() call)
    history: Option<Vec<SessionInfo>>,
    /// Extracted fingerprints for windows (kitty_id → fingerprint)
    window_fingerprints: HashMap<u64, SessionFingerprint>,
}

impl BabelCore {
    /// Connect to babel - tries daemon first, falls back to ephemeral mode
    ///
    /// This is the main entry point. CLI commands should call this once
    /// at startup and reuse the core for all operations.
    pub async fn connect() -> Self {
        if is_daemon_running().await {
            debug!("connected to babeld");
            Self { mode: CoreMode::Daemon }
        } else {
            debug!("daemon not available, using ephemeral mode");
            Self {
                mode: CoreMode::Ephemeral(Arc::new(RwLock::new(EphemeralState::default()))),
            }
        }
    }

    /// Check if connected to daemon (vs ephemeral mode)
    pub fn is_daemon_mode(&self) -> bool {
        matches!(self.mode, CoreMode::Daemon)
    }

    // ═══════════════════════════════════════════════════════════════════════════
    // Query Operations (read-only)
    // ═══════════════════════════════════════════════════════════════════════════

    /// Get all Claude windows with enriched session data
    pub async fn windows(&self) -> Result<Vec<ClaudeWindow>> {
        match &self.mode {
            CoreMode::Daemon => {
                match send_request(&Request::List).await {
                    Ok(Response::Windows { windows }) => Ok(windows),
                    Ok(other) => bail!("unexpected response: {:?}", other),
                    Err(e) => {
                        warn!("daemon request failed, falling back to direct: {}", e);
                        self.windows_direct().await
                    }
                }
            }
            CoreMode::Ephemeral(state) => {
                // Check cache first
                {
                    let s = state.read().await;
                    if let Some(ref windows) = s.windows {
                        return Ok(windows.clone());
                    }
                }
                // Populate cache
                let windows = self.windows_direct().await?;
                {
                    let mut s = state.write().await;
                    s.windows = Some(windows.clone());
                }
                Ok(windows)
            }
        }
    }

    /// Get windows with fingerprints extracted from scrollback
    pub async fn windows_with_fingerprints(&self) -> Result<Vec<ClaudeWindow>> {
        match &self.mode {
            CoreMode::Daemon => {
                match send_request(&Request::ListWithFingerprints).await {
                    Ok(Response::Windows { windows }) => Ok(windows),
                    Ok(other) => bail!("unexpected response: {:?}", other),
                    Err(e) => {
                        warn!("daemon request failed: {}", e);
                        self.windows_with_fingerprints_direct().await
                    }
                }
            }
            CoreMode::Ephemeral(_) => {
                self.windows_with_fingerprints_direct().await
            }
        }
    }

    /// Get a specific window by ID, or focused window if None
    pub async fn window(&self, window_id: Option<u64>) -> Result<Option<ClaudeWindow>> {
        match &self.mode {
            CoreMode::Daemon => {
                match send_request(&Request::Status { window_id }).await {
                    Ok(Response::Window { window }) => Ok(*window),
                    Ok(other) => bail!("unexpected response: {:?}", other),
                    Err(e) => {
                        warn!("daemon request failed: {}", e);
                        self.window_direct(window_id).await
                    }
                }
            }
            CoreMode::Ephemeral(_) => {
                self.window_direct(window_id).await
            }
        }
    }

    /// Get session history
    pub async fn history(&self, limit: usize) -> Result<Vec<SessionInfo>> {
        match &self.mode {
            CoreMode::Daemon => {
                match send_request(&Request::History { limit }).await {
                    Ok(Response::History { sessions }) => Ok(sessions),
                    Ok(other) => bail!("unexpected response: {:?}", other),
                    Err(e) => {
                        warn!("daemon request failed: {}", e);
                        self.history_direct(limit)
                    }
                }
            }
            CoreMode::Ephemeral(state) => {
                // Check cache (note: cache ignores limit, returns all)
                {
                    let s = state.read().await;
                    if let Some(ref history) = s.history {
                        return Ok(history.iter().take(limit).cloned().collect());
                    }
                }
                // Populate cache with generous limit
                let history = self.history_direct(100)?;
                {
                    let mut s = state.write().await;
                    s.history = Some(history.clone());
                }
                Ok(history.into_iter().take(limit).collect())
            }
        }
    }

    /// Get scrollback text from a window
    pub async fn scrollback(&self, window_id: u64, lines: Option<usize>) -> Result<String> {
        let text = match &self.mode {
            CoreMode::Daemon => {
                match send_request(&Request::Scroll { window_id }).await {
                    Ok(Response::Scrollback { text }) => text,
                    Ok(other) => bail!("unexpected response: {:?}", other),
                    Err(e) => {
                        warn!("daemon request failed: {}", e);
                        kitty::get_scrollback(window_id)?
                    }
                }
            }
            CoreMode::Ephemeral(_) => {
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
            CoreMode::Daemon => {
                match send_request(&Request::Focus { window_id }).await {
                    Ok(Response::Ok { .. }) => Ok(()),
                    Ok(Response::Error { message }) => bail!("{}", message),
                    Ok(other) => bail!("unexpected response: {:?}", other),
                    Err(e) => {
                        warn!("daemon request failed: {}", e);
                        kitty::focus_window(window_id)
                    }
                }
            }
            CoreMode::Ephemeral(_) => {
                kitty::focus_window(window_id)
            }
        }
    }

    /// Send text to a window
    pub async fn send(&self, window_id: u64, text: &str) -> Result<()> {
        match &self.mode {
            CoreMode::Daemon => {
                match send_request(&Request::Send { window_id, text: text.to_string() }).await {
                    Ok(Response::Ok { .. }) => Ok(()),
                    Ok(Response::Error { message }) => bail!("{}", message),
                    Ok(other) => bail!("unexpected response: {:?}", other),
                    Err(e) => {
                        warn!("daemon request failed: {}", e);
                        kitty::send_text(window_id, text)
                    }
                }
            }
            CoreMode::Ephemeral(_) => {
                kitty::send_text(window_id, text)
            }
        }
    }

    /// Set icon/tag for a window
    pub async fn set_icon(&self, window_id: u64, icon: &str) -> Result<()> {
        match &self.mode {
            CoreMode::Daemon => {
                match send_request(&Request::Tag { window_id, icon: icon.to_string() }).await {
                    Ok(Response::Ok { .. }) => Ok(()),
                    Ok(Response::Error { message }) => bail!("{}", message),
                    Ok(other) => bail!("unexpected response: {:?}", other),
                    Err(e) => {
                        warn!("daemon request failed: {}", e);
                        self.set_icon_direct(window_id, icon).await
                    }
                }
            }
            CoreMode::Ephemeral(_) => {
                self.set_icon_direct(window_id, icon).await
            }
        }
    }

    /// Mark session as read
    pub async fn mark_read(&self, window_id: u64) -> Result<()> {
        match &self.mode {
            CoreMode::Daemon => {
                match send_request(&Request::MarkRead { window_id }).await {
                    Ok(Response::Ok { .. }) => Ok(()),
                    Ok(Response::Error { message }) => bail!("{}", message),
                    Ok(other) => bail!("unexpected response: {:?}", other),
                    Err(e) => {
                        warn!("daemon request failed: {}", e);
                        self.mark_read_direct(window_id).await
                    }
                }
            }
            CoreMode::Ephemeral(_) => {
                self.mark_read_direct(window_id).await
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
            CoreMode::Daemon => {
                match send_request(&Request::WSetSave { name: name.clone() }).await {
                    Ok(Response::WSet { wset }) => Ok(wset),
                    Ok(Response::Error { message }) => bail!("{}", message),
                    Ok(other) => bail!("unexpected response: {:?}", other),
                    Err(e) => {
                        warn!("daemon request failed: {}", e);
                        self.wset_save_direct(name).await
                    }
                }
            }
            CoreMode::Ephemeral(_) => {
                self.wset_save_direct(name).await
            }
        }
    }

    /// List saved workspace sets
    pub async fn wset_list(&self) -> Result<Vec<crate::wset::WSetSummary>> {
        match &self.mode {
            CoreMode::Daemon => {
                match send_request(&Request::WSetList).await {
                    Ok(Response::WSetList { wsets, .. }) => Ok(wsets),
                    Ok(other) => bail!("unexpected response: {:?}", other),
                    Err(e) => {
                        warn!("daemon request failed: {}", e);
                        Ok(crate::wset::list_wsets()?)
                    }
                }
            }
            CoreMode::Ephemeral(_) => {
                Ok(crate::wset::list_wsets()?)
            }
        }
    }

    /// Get current wset name
    pub async fn wset_current(&self) -> Result<Option<String>> {
        match &self.mode {
            CoreMode::Daemon => {
                match send_request(&Request::WSetCurrent).await {
                    Ok(Response::WSetCurrent { name }) => Ok(name),
                    Ok(other) => bail!("unexpected response: {:?}", other),
                    Err(e) => {
                        warn!("daemon request failed: {}", e);
                        Ok(crate::wset::get_current_wset_name()?)
                    }
                }
            }
            CoreMode::Ephemeral(_) => {
                Ok(crate::wset::get_current_wset_name()?)
            }
        }
    }

    // ═══════════════════════════════════════════════════════════════════════════
    // Direct implementations (no daemon)
    // ═══════════════════════════════════════════════════════════════════════════

    async fn windows_direct(&self) -> Result<Vec<ClaudeWindow>> {
        let windows = discover_claude_windows()?;
        Ok(windows)
    }

    async fn windows_with_fingerprints_direct(&self) -> Result<Vec<ClaudeWindow>> {
        let mut windows = discover_claude_windows()?;

        for window in &mut windows {
            if let Ok(scrollback) = kitty::get_scrollback(window.kitty_id) {
                // extract_from_scrollback returns SessionFingerprint directly
                let fp = extract_from_scrollback(&scrollback);
                // Only use fingerprint if it has meaningful data
                if fp.first_prompt.is_some() || !fp.recent_prompts.is_empty() {
                    window.fingerprint = Some(fp);
                }
            }
        }

        Ok(windows)
    }

    async fn window_direct(&self, window_id: Option<u64>) -> Result<Option<ClaudeWindow>> {
        let windows = discover_claude_windows()?;

        match window_id {
            Some(id) => Ok(windows.into_iter().find(|w| w.kitty_id == id)),
            None => Ok(windows.into_iter().find(|w| w.is_focused)),
        }
    }

    fn history_direct(&self, limit: usize) -> Result<Vec<SessionInfo>> {
        get_recent_sessions(limit)
    }

    async fn set_icon_direct(&self, window_id: u64, icon: &str) -> Result<()> {
        // Get session ID for this window
        let window = self.window(Some(window_id)).await?
            .ok_or_else(|| anyhow::anyhow!("window {} not found", window_id))?;

        if let Some(session_id) = &window.session_id {
            let db = overlay::init_db()?;
            overlay::set_icon(&db, session_id, icon)?;
        }

        // Also set kitty user var for visual feedback
        kitty::set_user_var(window_id, "babel_icon", icon)?;

        Ok(())
    }

    async fn mark_read_direct(&self, window_id: u64) -> Result<()> {
        let window = self.window(Some(window_id)).await?
            .ok_or_else(|| anyhow::anyhow!("window {} not found", window_id))?;

        if let Some(session_id) = &window.session_id {
            let db = overlay::init_db()?;
            overlay::mark_read(&db, session_id)?;
        }

        Ok(())
    }

    async fn wset_save_direct(&self, name: Option<String>) -> Result<crate::wset::WSet> {
        let windows = self.windows().await?;
        // from_windows needs (name, windows, workspace_titles)
        // For ephemeral mode, we don't have cached workspace titles, use empty map
        let workspace_titles = std::collections::HashMap::new();
        let wset_name = name.unwrap_or_else(|| "unnamed".to_string());
        let mut wset = crate::wset::WSet::from_windows(&wset_name, &windows, &workspace_titles);
        wset.save()?;
        Ok(wset)
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
