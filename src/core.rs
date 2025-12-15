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
//!   state = BabelState::new()   // same state
//!   loop { watch → refresh → serve IPC }
//! ```
//!
//! The daemon is literally just "BabelState + event loop + IPC server."
//! Local mode initializes the same state, uses it, and exits.

use std::path::{Path, PathBuf};

use anyhow::{Result, bail, Context};
use tracing::{debug, warn, info};

use crate::daemon::BabelState;
use crate::discovery::ClaudeWindow;
use crate::claude_storage::{SessionInfo, MigrateResult};
use crate::ipc::{send_request, is_daemon_running, Request, Response};
use crate::kitty;
use crate::babel_storage;
use scrollparse::claude::{detect_activity_state, ActivityState};

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
    Local(BabelState),
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
            let mut state = BabelState::new();

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
                        let db = babel_storage::init_db()?;
                        babel_storage::set_icon(&db, session_id, icon)?;
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
                        let db = babel_storage::init_db()?;
                        babel_storage::mark_read(&db, session_id)?;
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

    // ═══════════════════════════════════════════════════════════════════════════
    // State Detection
    // ═══════════════════════════════════════════════════════════════════════════

    /// Get the current state of a Claude session (idle, thinking, tool use, etc.)
    ///
    /// Analyzes the window's scrollback to determine what Claude is currently doing.
    pub async fn get_window_state(&self, window_id: u64) -> Result<ActivityState> {
        let scrollback = self.scrollback(window_id, Some(50)).await?;
        Ok(detect_activity_state(&scrollback))
    }

    /// Find all windows whose cwd is inside the given path
    ///
    /// Returns windows along with their current state and relative path from source.
    /// Used by migration to detect affected terminals.
    pub async fn find_windows_in_path(&self, source: &Path) -> Result<Vec<ConflictingWindow>> {
        // Canonicalize source path for accurate comparison
        let source = source.canonicalize()
            .unwrap_or_else(|_| std::env::current_dir().unwrap().join(source));

        let windows = self.windows().await?;
        let mut conflicts = Vec::new();

        for win in windows {
            if win.cwd.starts_with(&source) {
                let state = self.get_window_state(win.kitty_id).await
                    .unwrap_or(ActivityState::Unknown);

                let relative_path = win.cwd
                    .strip_prefix(&source)
                    .unwrap_or(Path::new(""))
                    .to_path_buf();

                conflicts.push(ConflictingWindow {
                    window: win,
                    state,
                    relative_path,
                });
            }
        }

        Ok(conflicts)
    }

    // ═══════════════════════════════════════════════════════════════════════════
    // Terminal Migration
    // ═══════════════════════════════════════════════════════════════════════════

    /// Migrate an idle terminal to a new working directory
    ///
    /// Sends: Ctrl-C (ensure clean prompt) → cd <new_path> → claude -r <session_id>
    /// This allows the terminal to continue working after a directory move.
    pub async fn migrate_terminal(&self, window_id: u64, new_cwd: &Path, session_id: Option<&str>) -> Result<()> {
        use std::time::Duration;
        use tokio::time::sleep;

        // Ctrl-C to ensure clean prompt
        self.send(window_id, "\x03").await?;
        sleep(Duration::from_millis(100)).await;

        // cd to new directory
        let cd_cmd = format!("cd {}\n", shell_escape(new_cwd));
        self.send(window_id, &cd_cmd).await?;
        sleep(Duration::from_millis(50)).await;

        // Resume session if we have the ID
        if let Some(sid) = session_id {
            let resume_cmd = format!("claude -r {}\n", sid);
            self.send(window_id, &resume_cmd).await?;
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
    pub async fn migrate_project(
        &mut self,
        old_path: &Path,
        new_path: &Path,
        options: MigrateOptions,
    ) -> Result<MigrateOutcome> {
        info!(?old_path, ?new_path, dry_run = options.dry_run, "starting project migration");

        // Canonicalize paths
        let old_canonical = old_path.canonicalize()
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

        // Find conflicting windows
        let conflicts = self.find_windows_in_path(&old_canonical).await?;

        // Partition by migratable state
        let (migratable, active): (Vec<_>, Vec<_>) = conflicts.iter().partition(|c| {
            matches!(c.state, ActivityState::Idle | ActivityState::AwaitingInput)
        });

        // Check for blocking active windows
        if !active.is_empty() && !options.force {
            bail!(
                "{} active Claude session(s) in source path would break. \
                Use force=true to proceed anyway.",
                active.len()
            );
        }

        let mut migrated_terminals = Vec::new();

        // Migrate idle terminals (before moving the directory)
        if options.migrate_terminals && !options.dry_run {
            for conflict in &migratable {
                let new_cwd = new_canonical.join(&conflict.relative_path);
                let session_id = conflict.window.session_id.as_deref();

                if let Err(e) = self.migrate_terminal(
                    conflict.window.kitty_id,
                    &new_cwd,
                    session_id,
                ).await {
                    warn!(
                        window_id = conflict.window.kitty_id,
                        error = %e,
                        "failed to migrate terminal"
                    );
                } else {
                    migrated_terminals.push(conflict.window.kitty_id);
                }
            }
        }

        // Move the physical directory (if requested and source exists)
        let mut directory_moved = false;
        if options.move_directory && old_canonical.exists() && !options.dry_run {
            // Try rename first (same filesystem)
            if std::fs::rename(&old_canonical, &new_canonical).is_err() {
                debug!("rename failed, falling back to copy+delete");
                copy_dir_recursive(&old_canonical, &new_canonical)
                    .with_context(|| format!(
                        "Failed to copy {} → {}",
                        old_canonical.display(), new_canonical.display()
                    ))?;
                std::fs::remove_dir_all(&old_canonical)
                    .with_context(|| format!("Failed to remove source: {}", old_canonical.display()))?;
            }
            directory_moved = true;
        }

        // Update Claude storage (project folder + history.jsonl)
        let storage_result = crate::claude_storage::migrate_project(
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
            active_terminals: active.iter().map(|c| c.window.kitty_id).collect(),
            dry_run: options.dry_run,
        })
    }

    // ═══════════════════════════════════════════════════════════════════════════
    // State Management
    // ═══════════════════════════════════════════════════════════════════════════

    /// Force refresh of internal state
    ///
    /// In connected mode, tells daemon to refresh. In local mode, re-initializes state.
    pub async fn refresh(&mut self) -> Result<()> {
        match &mut self.mode {
            CoreMode::Connected => {
                match send_request(&Request::Refresh).await {
                    Ok(Response::Ok { .. }) => Ok(()),
                    Ok(Response::Error { message }) => bail!("{}", message),
                    Ok(other) => bail!("unexpected response: {:?}", other),
                    Err(e) => bail!("daemon connection failed: {}", e),
                }
            }
            CoreMode::Local(state) => {
                // Re-initialize state same as connect()
                if let Err(e) = state.refresh_windows() {
                    warn!("failed to refresh windows: {}", e);
                }
                if let Err(e) = state.rebuild_summary_index() {
                    warn!("failed to rebuild summary index: {}", e);
                }
                if let Err(e) = state.rebuild_fingerprint_index() {
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

/// A window whose cwd conflicts with a migration source path
pub struct ConflictingWindow {
    pub window: ClaudeWindow,
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
    /// Window IDs of terminals that were migrated
    pub terminals_migrated: Vec<u64>,
    /// Window IDs of active terminals (not migrated)
    pub active_terminals: Vec<u64>,
    /// Whether this was a dry run
    pub dry_run: bool,
}

// ═══════════════════════════════════════════════════════════════════════════════
// Helper Functions
// ═══════════════════════════════════════════════════════════════════════════════

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
