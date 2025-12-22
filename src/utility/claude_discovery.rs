//! Discovery - Match kitty windows to Claude Code sessions
//!
//! This module implements the core discovery logic for identifying which kitty windows
//! are running Claude Code, and matching them to their corresponding conversation sessions
//! in ~/.claude/projects/.
//!
//! ## Matching Strategy
//!
//! 1. **Pre-tagged windows**: Check user_vars for "babel_session_id" - already matched
//! 2. **Title extraction**: Strip "✳ " prefix from active Claude pane titles
//! 3. **Conversation search**: Search ~/.claude/projects/ for matching summaries
//! 4. **Tagging**: Once matched, tag the window with session ID for fast future lookups
//! 5. **CWD tiebreaker**: If multiple matches, use working directory proximity
//!
//! ## Window Title Formats
//!
//! - Active sessions: "✳ Summary Text Here"
//! - Idle terminals: "~/path: command"
//! - Some variations include project info
//!
//! The summary text is set by Claude Code itself and appears in the window title.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

use crate::utility::claude_storage::{find_session_by_summary, SessionInfo};
use crate::fingerprint::{SessionFingerprint, MatchConfidence};
use crate::kitty::{list_panes, set_user_var, get_recent_scrollback, close_window, get_pane, move_window_to_workspace, set_window_geometry, KittyPane, PaneAddr};
use crate::wset::WSet;

// ═══════════════════════════════════════════════════════════════════════════════
// Claude Detection
// ═══════════════════════════════════════════════════════════════════════════════

/// Detection signals for identifying Claude sessions
///
/// Multiple signals can be present simultaneously. A window is considered
/// a Claude session if ANY signal is positive.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ClaudeMarkers {
    /// "claude" found in foreground process cmdline (definitive)
    pub process_running: bool,
    /// "✳" prefix in title (Claude's active session indicator)
    pub title_indicator: bool,
    /// Has `babel_session_id` user_var (previously tagged by babel)
    pub babel_tagged: bool,
    /// Session ID if tagged
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
}

impl ClaudeMarkers {
    /// Returns true if any signal suggests this is a Claude session
    pub fn is_claude(&self) -> bool {
        self.process_running || self.title_indicator || self.babel_tagged
    }

    /// Returns a short status string for display
    pub fn status(&self) -> &'static str {
        if self.process_running {
            "running"      // Actively running claude process
        } else if self.title_indicator {
            "titled"       // Has ✳ title but process exited (at shell prompt)
        } else if self.babel_tagged {
            "tagged"       // Previously tagged but no other signals
        } else {
            "none"         // Not a Claude session
        }
    }

    /// Returns emoji indicator for the detection status
    pub fn indicator(&self) -> &'static str {
        if self.process_running {
            "●"  // Solid - definitely running
        } else if self.title_indicator {
            "◐"  // Half - session exists but at shell
        } else if self.babel_tagged {
            "○"  // Empty - was tagged but no active signals
        } else {
            " "  // Nothing
        }
    }
}

/// Analyze a window for Claude session signals
pub fn detect_claude_signals(window: &KittyPane) -> ClaudeMarkers {
    let process_running = window.foreground_processes.iter().any(|proc| {
        proc.cmdline.iter().any(|arg| arg.contains("claude"))
    });

    let title_indicator = window.title.starts_with("✳");

    let babel_tagged = window.user_vars
        .get("babel_session_id")
        .map(|s| !s.is_empty())
        .unwrap_or(false);

    let session_id = window.user_vars
        .get("babel_session_id")
        .filter(|s| !s.is_empty())
        .cloned();

    ClaudeMarkers {
        process_running,
        title_indicator,
        babel_tagged,
        session_id,
    }
}

/// Get the current activity state of a Claude session by window ID
///
/// Convenience function that fetches scrollback and detects activity state.
/// Returns Unknown on any error (window not found, kitten failure, etc.)
pub fn get_window_activity_state(id: u64) -> scrollparse::claude::ActivityState {
    match get_recent_scrollback(id, 20) {
        Ok(scrollback) => scrollparse::claude::detect_activity_state(&scrollback),
        Err(_) => scrollparse::claude::ActivityState::Unknown,
    }
}

/// Get both activity state and scrollback content for a Claude session
///
/// Activity detection result with all derived state
#[derive(Debug, Clone)]
pub struct ActivityResult {
    /// Current activity state (Idle, Thinking, ToolUse, etc.)
    pub state: scrollparse::claude::ActivityState,
    /// True if Claude's last message ended with a question
    pub asking_question: bool,
    /// Raw scrollback content (for pulse tracking)
    pub scrollback: String,
}

/// Returns activity state, asking_question, and scrollback for use in both
/// state detection and activity pulse tracking. Avoids double-fetching scrollback.
///
/// Returns (Unknown, false, empty string) on any error.
///
/// NOTE: This uses the default socket. For windows on other sockets, use
/// `get_activity_with_scrollback_on_socket` instead.
pub fn get_window_activity_with_scrollback(id: u64) -> ActivityResult {
    match get_recent_scrollback(id, 50) {
        Ok(scrollback) => {
            let state = scrollparse::claude::detect_activity_state(&scrollback);
            let asking_question = scrollparse::claude::detect_asking_question(&scrollback);
            ActivityResult { state, asking_question, scrollback }
        }
        Err(_) => ActivityResult {
            state: scrollparse::claude::ActivityState::Unknown,
            asking_question: false,
            scrollback: String::new(),
        },
    }
}

/// Get activity state and scrollback for a pane on a specific socket
///
/// Socket-aware variant that works with windows on non-default kitty instances.
/// Takes a PaneAddr to correctly route the query to the right socket.
///
/// Returns ActivityResult with Unknown state on any error.
pub fn get_activity_with_scrollback_on_socket(addr: &crate::kitty::PaneAddr) -> ActivityResult {
    use crate::kitty::get_recent_scrollback_on_socket;

    match get_recent_scrollback_on_socket(&addr.socket, addr.id, 50) {
        Ok(scrollback) => {
            let state = scrollparse::claude::detect_activity_state(&scrollback);
            let asking_question = scrollparse::claude::detect_asking_question(&scrollback);
            ActivityResult { state, asking_question, scrollback }
        }
        Err(_) => ActivityResult {
            state: scrollparse::claude::ActivityState::Unknown,
            asking_question: false,
            scrollback: String::new(),
        },
    }
}

/// Find all kitty windows that appear to be running Claude Code
///
/// A window is considered a Claude session if any of these are true:
/// - Has "claude" in a foreground process name
/// - Has title starting with "✳ " (active Claude session indicator)
/// - Has `babel_session_id` user_var set (previously tagged by babel)
///
/// This catches sessions that have exited to shell prompt but still have
/// the ✳ title, or windows that were previously identified and tagged.
pub fn find_claude_windows() -> Result<Vec<KittyPane>> {
    let all_windows = list_panes()?;

    let claude_windows = all_windows
        .into_iter()
        .filter(|win| detect_claude_signals(win).is_claude())
        .collect();

    Ok(claude_windows)
}

/// A single Claude worker in the tower—one voice among many
///
/// Each ClaudePane represents an individual Claude Code session running in a kitty
/// terminal pane. In the Babel architecture, these are the workers: autonomous agents
/// each engaged in their own conversation, yet observable and (soon) directable by
/// a Captain orchestrator. The pane carries both its terminal identity (`addr`) and
/// its conversational soul (`session_id`, `fingerprint`).
///
/// Uniquely identified by `addr` (PaneAddr = socket + kitty window ID).
/// This supports multiple kitty instances where window IDs may collide.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClaudePane {
    /// Unique address of this pane across all kitty instances
    pub addr: PaneAddr,
    pub title: String,
    pub session_id: Option<String>,
    pub session_info: Option<SessionInfo>,
    pub cwd: PathBuf,
    pub is_focused: bool,
    /// Kitty OS window ID (internal)
    pub os_window_id: u64,
    /// X11/Wayland window ID
    pub platform_window_id: u64,
    /// Workspace number (-1 = sticky/all, 0+ = workspace index)
    pub workspace: Option<i32>,
    /// Current activity state (Idle, Thinking, ToolUse, etc.)
    /// Populated from scrollback analysis when available
    #[serde(skip_serializing_if = "Option::is_none")]
    pub activity_state: Option<scrollparse::claude::ActivityState>,
    /// Hook state—ground truth from Claude Code lifecycle events
    /// This is deterministic: Working (after prompt submit) or Idle (after stop)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hook_state: Option<crate::babel_storage::HookState>,
    /// Extracted fingerprint from scrollback (for debugging/verification)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fingerprint: Option<SessionFingerprint>,
    /// Match confidence if matched via fingerprinting
    #[serde(skip_serializing_if = "Option::is_none")]
    pub match_confidence: Option<MatchConfidence>,
}

impl ClaudePane {
    // ─── Convenience Accessors ──────────────────────────────────────────────────

    /// Get the kitty window ID
    pub fn id(&self) -> u64 {
        self.addr.id
    }

    /// Get the socket path
    pub fn socket(&self) -> &str {
        &self.addr.socket
    }

    /// Check if this window is on the current/default kitty socket
    pub fn is_current_socket(&self) -> bool {
        self.addr.is_current_socket()
    }

    // ─── Operations ─────────────────────────────────────────────────────────────

    /// Focus this window
    pub fn focus(&self) -> Result<()> {
        crate::kitty::focus_pane_on_socket(&self.addr.socket, self.addr.id)
    }

    /// Send text to this window's input
    pub fn send_text(&self, text: &str) -> Result<()> {
        crate::kitty::send_text_on_socket(&self.addr.socket, self.addr.id, text)
    }

    /// Set a user variable on this window
    pub fn set_user_var(&self, key: &str, value: &str) -> Result<()> {
        crate::kitty::set_user_var_on_socket(&self.addr.socket, self.addr.id, key, value)
    }

    /// Set the title of this window
    pub fn set_title(&self, title: &str) -> Result<()> {
        crate::kitty::set_title_on_socket(&self.addr.socket, self.addr.id, title)
    }

    /// Get the full scrollback buffer
    pub fn scrollback(&self) -> Result<String> {
        crate::kitty::get_scrollback_on_socket(&self.addr.socket, self.addr.id)
    }

    /// Get the last N lines of scrollback
    pub fn recent_scrollback(&self, lines: usize) -> Result<String> {
        crate::kitty::get_recent_scrollback_on_socket(&self.addr.socket, self.addr.id, lines)
    }

    /// Close this window
    pub fn close(&self) -> Result<()> {
        crate::kitty::close_pane_on_socket(&self.addr.socket, self.addr.id)
    }
}

/// Fast discovery - just finds Claude panes without expensive session matching
///
/// This is O(1) kitty call + O(1) wmctrl call - no filesystem scanning.
/// For session info, call `enrich_window` on specific windows.
pub fn discover_claude_windows() -> Result<Vec<ClaudePane>> {
    use crate::kitty::get_all_workspaces;

    let claude_windows = find_claude_windows().context("Failed to find claude panes")?;

    // Get workspace mappings in one call
    let workspaces = get_all_workspaces();

    let discovered = claude_windows
        .into_iter()
        .map(|window| {
            // Check if already tagged (fast - just reads user_vars from kitty output)
            let session_id = window
                .user_vars
                .get("babel_session_id")
                .filter(|s| !s.is_empty())
                .cloned();

            // Look up workspace for this OS window
            let workspace = workspaces.get(&window.platform_window_id).copied();

            ClaudePane {
                addr: window.addr(),
                title: window.title.clone(),
                session_id,
                session_info: None, // Lazy - only load on demand
                cwd: window.cwd.clone(),
                is_focused: window.is_focused,
                os_window_id: window.os_window_id,
                platform_window_id: window.platform_window_id,
                workspace,
                activity_state: None, // Populated by daemon from cached scrollback analysis
                hook_state: None, // Populated by daemon from babel_storage
                fingerprint: None, // Only populated in daemon with --details
                match_confidence: None,
            }
        })
        .collect();

    Ok(discovered)
}

/// Enrich a window with full session info (expensive - searches ~/.claude)
///
/// Call this only when you need the full session metadata for a specific window.
/// This does the filesystem scan to match window title → conversation file.
pub fn enrich_window(window: &mut ClaudePane) -> Result<()> {
    // If already has session_info, skip
    if window.session_info.is_some() {
        return Ok(());
    }

    // Use shared resolution logic
    if let Some(info) = resolve_session(
        window.session_id.as_deref(),
        &window.title,
        window.id(),
    )? {
        window.session_id = Some(info.session_id.clone());
        window.session_info = Some(info);
    }

    Ok(())
}

/// Match a single window to its session
///
/// Matching algorithm:
/// 1. Check user_vars for "babel_session_id" - already tagged
/// 2. If not tagged:
///    - Extract summary from title (strip "✳ " prefix)
///    - Search ~/.claude/projects/ for matching summaries
///    - If found, tag window with session_id for future lookups
/// 3. Return the session ID (as SessionInfo) if matched, None otherwise
pub fn match_window_to_session(window: &KittyPane) -> Result<Option<SessionInfo>> {
    let existing_session_id = window
        .user_vars
        .get("babel_session_id")
        .filter(|s| !s.is_empty())
        .map(|s| s.as_str());

    resolve_session(existing_session_id, &window.title, window.id)
}

/// Core session resolution logic (shared by enrich_window and match_window_to_session)
///
/// 1. If session_id provided, try to load directly (O(1) if cached, O(n×m) otherwise)
/// 2. Extract summary from title and search by summary
/// 3. Tag window with result for fast future lookups
fn resolve_session(
    session_id: Option<&str>,
    title: &str,
    window_id: u64,
) -> Result<Option<SessionInfo>> {
    // Try loading by session_id first (already tagged)
    if let Some(sid) = session_id {
        if let Some(info) = load_session_by_id(sid, None)? {
            return Ok(Some(info));
        }
        // Session file missing/moved - fall through to re-match by title
    }

    // Extract summary from title
    let summary = extract_summary_from_title(title);
    if summary.is_empty() {
        return Ok(None); // Not an active Claude session
    }

    // Find matching session by searching conversation files
    let session_info = find_session_by_summary(&summary)?;

    // Tag the window for fast future lookups
    if let Some(ref info) = session_info {
        let _ = tag_window(window_id, &info.session_id);
    }

    Ok(session_info)
}

/// Tag a window with its session ID for fast future lookups
///
/// Uses kitty's user_vars feature to persistently associate a session ID with a window.
/// The tag survives as long as the window exists.
pub fn tag_window(kitty_id: u64, session_id: &str) -> Result<()> {
    set_user_var(kitty_id, "babel_session_id", session_id)
        .context("Failed to tag window with session ID")
}

// ============================================================================
// Internal Implementation
// ============================================================================

/// Extract summary text from a window title
///
/// Strips the "✳ " prefix from active Claude sessions.
/// Returns empty string if not an active session.
fn extract_summary_from_title(title: &str) -> String {
    if let Some(summary) = title.strip_prefix("✳ ") {
        summary.trim().to_string()
    } else {
        String::new()
    }
}

/// Load session info by session ID
///
/// If `path_cache` is provided and contains the session_id, uses O(1) lookup.
/// Otherwise falls back to O(n×m) scan of all projects/sessions.
///
/// The cache is populated by BabelState::rebuild_summary_index() in daemon mode.
pub fn load_session_by_id(
    session_id: &str,
    path_cache: Option<&std::collections::HashMap<String, std::path::PathBuf>>,
) -> Result<Option<SessionInfo>> {
    use crate::utility::claude_storage::{get_session_info, list_projects, list_sessions};

    // Fast path: use cache if available
    if let Some(cache) = path_cache {
        if let Some(session_path) = cache.get(session_id) {
            if session_path.exists() {
                return Ok(Some(get_session_info(session_path)?));
            }
        }
        // Cache miss or stale - fall through to scan
    }

    // Slow path: search all projects for this session ID
    for project_dir in list_projects()? {
        for session_path in list_sessions(&project_dir)? {
            if let Some(file_stem) = session_path.file_stem().and_then(|s| s.to_str()) {
                if file_stem == session_id {
                    let info = get_session_info(&session_path)?;
                    return Ok(Some(info));
                }
            }
        }
    }

    Ok(None)
}

// ═══════════════════════════════════════════════════════════════════════════════
// Session Spawning
// ═══════════════════════════════════════════════════════════════════════════════

/// Spawn a claude session in a new kitty window
///
/// Uses `kitty-claude` script for consistent window setup with random backgrounds.
/// Always spawns on the MAIN kitty socket (lowest PID) to consolidate windows
/// after multi-instance accidents. Returns the new kitty window ID after a brief
/// delay for the window to appear.
pub async fn spawn_claude_session(session_id: &str, cwd: &std::path::Path) -> Result<Option<u64>> {
    use std::process::{Command, Stdio};
    use tokio::time::{sleep, Duration};

    // Verify the session exists in ~/.claude before spawning
    let claude_base = crate::utility::claude_storage::claude_base();
    let projects_dir = claude_base.join("projects");

    // Search for the session file in any project directory
    let mut session_exists = false;
    if projects_dir.exists() {
        for entry in std::fs::read_dir(&projects_dir)? {
            let entry = entry?;
            let session_path = entry.path().join(format!("{}.jsonl", session_id));
            if session_path.exists() {
                session_exists = true;
                break;
            }
        }
    }

    if !session_exists {
        tracing::warn!(session_id, "Session file not found, skipping spawn");
        return Ok(None);
    }

    // Get the main socket (lowest PID) - consolidates to primary kitty instance
    // This ensures wset load always restores to the "real" kitty, not orphan instances
    let main_socket = crate::kitty::main_socket();

    // Spawn kitty-claude with the session
    // kitty-claude handles random background selection and consistent styling
    // KITTY_LISTEN_ON forces targeting the main socket, not whatever socket
    // the daemon happens to be running from
    let mut cmd = Command::new("kitty-claude");
    cmd.args(["-d", &cwd.to_string_lossy()])
        .args(["-e", "claude", "-r", session_id])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());

    // Target main socket explicitly - critical for multi-instance consolidation
    if let Some(socket) = &main_socket {
        cmd.env("KITTY_LISTEN_ON", socket);
        tracing::debug!(socket, session_id, "Spawning session on main socket");
    }

    let _child = cmd.spawn()
        .context("Failed to spawn kitty-claude")?;

    // Wait for window to appear
    // This is a bit racy but necessary since kitty spawns async
    sleep(Duration::from_millis(500)).await;

    // Find the new window by looking for one with this session in scrollback
    // or title, or by user_var if we tagged it
    let windows = find_claude_windows()?;

    // Try to find by title containing session_id or by matching cwd
    // This is imperfect but usually works
    for win in &windows {
        // Check if this is a newly spawned window at the right cwd
        if win.cwd == cwd {
            // Tag it for future fast lookups
            let _ = set_user_var(win.id, "babel_session_id", session_id);
            return Ok(Some(win.id));
        }
    }

    // If we can't find it, return None but don't fail
    // The daemon will pick it up on next refresh
    tracing::info!(session_id, "Spawned session but couldn't find window immediately");
    Ok(None)
}

/// Load a WSet by closing all existing claude panes and spawning new ones
///
/// Returns a list of session IDs that couldn't be restored (file missing, etc.)
pub async fn load_wset(wset: &WSet) -> Result<Vec<String>> {
    use tokio::time::{sleep, Duration};

    let mut skipped: Vec<String> = Vec::new();

    // Step 1: Close all existing claude panes
    tracing::info!(wset = %wset.meta.name, "Closing existing claude panes");
    for win in find_claude_windows()? {
        if let Err(e) = close_window(win.id) {
            tracing::warn!(kitty_id = win.id, error = %e, "Failed to close window");
        }
    }

    // Brief pause to let windows close
    sleep(Duration::from_millis(300)).await;

    // Step 2: Spawn windows for each wspace
    for wspace in &wset.wspaces {
        tracing::info!(workspace = wspace.index, windows = wspace.windows.len(), "Spawning wspace");

        for window_config in &wspace.windows {
            match spawn_claude_session(&window_config.session_id, &window_config.cwd).await {
                Ok(Some(kitty_id)) => {
                    // Move to correct workspace and restore geometry
                    // Need to get platform_window_id first
                    if let Ok(Some(win)) = get_pane(kitty_id) {
                        // Step 1: Move to correct workspace
                        if let Err(e) = move_window_to_workspace(win.platform_window_id, wspace.index) {
                            tracing::warn!(
                                kitty_id,
                                workspace = wspace.index,
                                error = %e,
                                "Failed to move window to workspace"
                            );
                        }

                        // Step 2: Restore precise geometry if available
                        // This enables multi-monitor restoration
                        if let Some(ref geom) = window_config.geometry {
                            // Brief pause for workspace move to complete
                            sleep(Duration::from_millis(50)).await;

                            if let Err(e) = set_window_geometry(win.platform_window_id, geom) {
                                tracing::warn!(
                                    kitty_id,
                                    x = geom.x, y = geom.y,
                                    w = geom.width, h = geom.height,
                                    error = %e,
                                    "Failed to restore window geometry"
                                );
                            } else {
                                tracing::debug!(
                                    kitty_id,
                                    x = geom.x, y = geom.y,
                                    w = geom.width, h = geom.height,
                                    monitor = ?geom.monitor,
                                    "Restored window geometry"
                                );
                            }
                        }
                    }
                }
                Ok(None) => {
                    // Session file doesn't exist or window not found
                    skipped.push(window_config.session_id.clone());
                }
                Err(e) => {
                    tracing::warn!(
                        session_id = %window_config.session_id,
                        error = %e,
                        "Failed to spawn session"
                    );
                    skipped.push(window_config.session_id.clone());
                }
            }

            // Small delay between spawns to avoid overwhelming the system
            sleep(Duration::from_millis(200)).await;
        }
    }

    tracing::info!(
        wset = %wset.meta.name,
        spawned = wset.window_count() - skipped.len(),
        skipped = skipped.len(),
        "WSet load complete"
    );

    Ok(skipped)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_extract_summary_from_title() {
        assert_eq!(
            extract_summary_from_title("✳ Pipeline Rewrite Assessment"),
            "Pipeline Rewrite Assessment"
        );
        assert_eq!(extract_summary_from_title("~/path: command"), "");
        assert_eq!(extract_summary_from_title("✳ Summary"), "Summary");
    }
}
