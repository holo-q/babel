//! Kitty Remote Control Wrapper
//!
//! This module provides a Rust interface to kitty's remote control protocol via `kitten @` commands.
//!
//! ## Socket Standard
//!
//! Kitty creates sockets at `$XDG_RUNTIME_DIR/kitty.sock-$PID` (NOT `kitty.sock` despite config).
//! See `Docs/15-kitty-single-instance-protocol.md` for details.
//!
//! **CRITICAL**: There must be exactly ONE kitty process. Multiple processes break kitty-attach.
//! If multiple sockets are found, this module logs a warning.
//!
//! ## Data Model
//!
//! Follows kitty's JSON output structure: OS Window -> Tabs -> Windows (panes)
//!
//! Each window can have:
//!   - foreground_processes: Running commands (we use this to find claude sessions)
//!   - user_vars: Custom key-value metadata (for babel state)
//!   - cwd: Current working directory

use std::path::PathBuf;
use std::collections::HashMap;
use std::process::Command;
use std::env;
use std::os::unix::fs::FileTypeExt;
use serde::{Deserialize, Serialize};
use anyhow::{Result, Context, bail};

/// Get the kitty socket path for remote control.
///
/// Socket detection priority:
/// 1. `KITTY_LISTEN_ON` env var (when running inside kitty)
/// 2. Find `$XDG_RUNTIME_DIR/kitty.sock-*` (the actual socket kitty creates)
/// 3. Fallback to `kitty.sock` (usually doesn't exist)
///
/// **WARNING**: If multiple `kitty.sock-*` files exist, single-instance is violated.
/// This breaks kitty-attach. The function logs a warning and picks the first socket.
///
/// Note: kitten auto-detects from TTY, but systemd services have no TTY,
/// so we must explicitly pass `--to unix:$socket_path`.
pub fn kitty_socket_path() -> String {
    // Priority 1: Use KITTY_LISTEN_ON if set (current terminal's kitty instance)
    if let Ok(listen_on) = env::var("KITTY_LISTEN_ON") {
        if !listen_on.is_empty() {
            return listen_on;
        }
    }

    let runtime_dir = env::var("XDG_RUNTIME_DIR").unwrap_or_else(|_| {
        let uid = users::get_current_uid();
        format!("/run/user/{}", uid)
    });

    // Priority 2: Find kitty.sock-* sockets (the ACTUAL socket kitty creates)
    // Despite listen_on config saying "kitty.sock", kitty creates "kitty.sock-$PID"
    if let Ok(entries) = std::fs::read_dir(&runtime_dir) {
        let sockets: Vec<_> = entries
            .filter_map(|e| e.ok())
            .filter(|e| {
                let name = e.file_name();
                let name_str = name.to_string_lossy();
                if !name_str.starts_with("kitty.sock-") {
                    return false;
                }
                // Check if it's actually a socket
                e.file_type().map(|ft| ft.is_socket()).unwrap_or(false)
            })
            .collect();

        // Warn if multiple sockets exist (single-instance violated)
        if sockets.len() > 1 {
            tracing::warn!(
                count = sockets.len(),
                "Multiple kitty sockets found - single instance violated! kitty-attach will break."
            );
        }

        if let Some(entry) = sockets.first() {
            return format!("unix:{}", entry.path().display());
        }
    }

    // Fallback: try kitty.sock (usually doesn't exist, but worth trying)
    let canonical = format!("{}/kitty.sock", runtime_dir);
    format!("unix:{}", canonical)
}

/// Information about a kitty socket and its windows
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct KittyInstance {
    /// Socket path (e.g., "unix:/run/user/1000/kitty.sock-12345")
    pub socket: String,
    /// PID extracted from socket name (if available)
    pub pid: Option<u32>,
    /// Whether this is the "current" socket (from KITTY_LISTEN_ON or first found)
    pub is_current: bool,
    /// Whether we can successfully communicate with this socket
    pub is_responsive: bool,
    /// Windows accessible through this socket
    pub windows: Vec<KittyWindow>,
    /// Error message if not responsive
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// Find all kitty sockets on the system
///
/// Returns paths to all `kitty.sock-*` files in XDG_RUNTIME_DIR.
/// Does NOT verify if they're responsive - use `discover_all_instances()` for that.
pub fn find_all_sockets() -> Vec<String> {
    let runtime_dir = env::var("XDG_RUNTIME_DIR").unwrap_or_else(|_| {
        let uid = users::get_current_uid();
        format!("/run/user/{}", uid)
    });

    let mut sockets = Vec::new();

    if let Ok(entries) = std::fs::read_dir(&runtime_dir) {
        for entry in entries.filter_map(|e| e.ok()) {
            let name = entry.file_name();
            let name_str = name.to_string_lossy();

            if name_str.starts_with("kitty.sock-") && entry.file_type().map(|ft| ft.is_socket()).unwrap_or(false) {
                sockets.push(format!("unix:{}", entry.path().display()));
            }
        }
    }

    sockets
}

/// List windows from a specific socket
///
/// Like `list_windows()` but targets a specific socket instead of the default.
pub fn list_windows_from_socket(socket: &str) -> Result<Vec<KittyWindow>> {
    let output = Command::new("kitten")
        .args(["@", "--to", socket, "ls"])
        .output()
        .context("Failed to execute 'kitten @ ls'")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("kitten @ ls failed: {}", stderr);
    }

    let raw_os_windows: Vec<RawOsWindow> = serde_json::from_slice(&output.stdout)
        .context("Failed to parse 'kitten @ ls' output")?;

    // Flatten: OS Windows -> Tabs -> Windows
    let mut windows = Vec::new();
    for os_win in raw_os_windows {
        let os_window_id = os_win.id;
        let platform_window_id = os_win.platform_window_id;

        for tab in os_win.tabs {
            for raw in tab.windows {
                windows.push(KittyWindow {
                    id: raw.id,
                    title: raw.title,
                    cwd: raw.cwd,
                    is_focused: raw.is_focused,
                    is_active: raw.is_active,
                    foreground_processes: raw.foreground_processes
                        .into_iter()
                        .map(|proc| ForegroundProcess {
                            pid: proc.pid,
                            cmdline: proc.cmdline,
                            cwd: proc.cwd,
                        })
                        .collect(),
                    user_vars: raw.user_vars,
                    os_window_id,
                    platform_window_id,
                });
            }
        }
    }

    Ok(windows)
}

/// Discover all kitty instances on the system
///
/// Finds all kitty sockets, queries each one, and returns information about
/// which are responsive and what windows they contain. This helps identify
/// "orphaned" terminals on different sockets.
pub fn discover_all_instances() -> Vec<KittyInstance> {
    let current_socket = kitty_socket_path();
    let all_sockets = find_all_sockets();

    let mut instances = Vec::new();

    for socket in all_sockets {
        // Extract PID from socket name (kitty.sock-12345 -> 12345)
        let pid = socket
            .rsplit("kitty.sock-")
            .next()
            .and_then(|s| s.parse::<u32>().ok());

        let is_current = socket == current_socket;

        // Try to query windows from this socket
        match list_windows_from_socket(&socket) {
            Ok(windows) => {
                instances.push(KittyInstance {
                    socket,
                    pid,
                    is_current,
                    is_responsive: true,
                    windows,
                    error: None,
                });
            }
            Err(e) => {
                instances.push(KittyInstance {
                    socket,
                    pid,
                    is_current,
                    is_responsive: false,
                    windows: Vec::new(),
                    error: Some(e.to_string()),
                });
            }
        }
    }

    // Sort: current first, then by PID
    instances.sort_by(|a, b| {
        b.is_current.cmp(&a.is_current)
            .then(a.pid.cmp(&b.pid))
    });

    instances
}

// ═══════════════════════════════════════════════════════════════════════════════
// Data Structures - Mirror kitty's JSON output
// ═══════════════════════════════════════════════════════════════════════════════

/// A kitty window (pane) with all relevant metadata
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct KittyWindow {
    pub id: u64,
    pub title: String,
    pub cwd: PathBuf,
    pub is_focused: bool,
    pub is_active: bool,  // Active within its tab
    pub foreground_processes: Vec<ForegroundProcess>,
    #[serde(default)]
    pub user_vars: HashMap<String, String>,
    /// The kitty OS window ID (internal to kitty)
    pub os_window_id: u64,
    /// X11/Wayland window ID for workspace lookup
    pub platform_window_id: u64,
}

/// Detection signals for identifying Claude sessions
///
/// Multiple signals can be present simultaneously. A window is considered
/// a Claude session if ANY signal is positive.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ClaudeSignals {
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

impl ClaudeSignals {
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
pub fn detect_claude_signals(window: &KittyWindow) -> ClaudeSignals {
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

    ClaudeSignals {
        process_running,
        title_indicator,
        babel_tagged,
        session_id,
    }
}

/// A foreground process running in a window
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ForegroundProcess {
    pub pid: u32,
    pub cmdline: Vec<String>,
    pub cwd: PathBuf,
}

// Raw JSON structures from `kitten @ ls` (internal, for parsing)

#[derive(Debug, Deserialize)]
struct RawOsWindow {
    id: u64,
    platform_window_id: u64,
    tabs: Vec<RawTab>,
}

#[derive(Debug, Deserialize)]
struct RawTab {
    windows: Vec<RawWindow>,
}

#[derive(Debug, Deserialize)]
struct RawWindow {
    id: u64,
    title: String,
    cwd: PathBuf,
    is_focused: bool,
    is_active: bool,
    foreground_processes: Vec<RawForegroundProcess>,
    #[serde(default)]
    user_vars: HashMap<String, String>,
}

#[derive(Debug, Deserialize)]
struct RawForegroundProcess {
    pid: u32,
    cmdline: Vec<String>,
    cwd: PathBuf,
}

// ═══════════════════════════════════════════════════════════════════════════════
// Public API
// ═══════════════════════════════════════════════════════════════════════════════

/// List all kitty windows across all OS windows and tabs
///
/// Calls `kitten @ ls` and flattens the hierarchy into a simple window list.
pub fn list_windows() -> Result<Vec<KittyWindow>> {
    let socket = kitty_socket_path();
    let output = Command::new("kitten")
        .args(["@", "--to", &socket, "ls"])
        .output()
        .context("Failed to execute 'kitten @ ls'")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("kitten @ ls failed: {}", stderr);
    }

    let raw_os_windows: Vec<RawOsWindow> = serde_json::from_slice(&output.stdout)
        .context("Failed to parse 'kitten @ ls' output")?;

    // Flatten: OS Windows -> Tabs -> Windows, preserving OS window context
    let mut windows = Vec::new();
    for os_win in raw_os_windows {
        let os_window_id = os_win.id;
        let platform_window_id = os_win.platform_window_id;

        for tab in os_win.tabs {
            for raw in tab.windows {
                windows.push(KittyWindow {
                    id: raw.id,
                    title: raw.title,
                    cwd: raw.cwd,
                    is_focused: raw.is_focused,
                    is_active: raw.is_active,
                    foreground_processes: raw.foreground_processes
                        .into_iter()
                        .map(|proc| ForegroundProcess {
                            pid: proc.pid,
                            cmdline: proc.cmdline,
                            cwd: proc.cwd,
                        })
                        .collect(),
                    user_vars: raw.user_vars,
                    os_window_id,
                    platform_window_id,
                });
            }
        }
    }

    Ok(windows)
}

/// Find all windows that are Claude sessions (uses multiple detection signals)
///
/// Detection signals (any match counts):
/// - "claude" in foreground process cmdline (process actively running)
/// - "✳" prefix in title (Claude's active session indicator)
/// - `babel_session_id` user_var set (previously tagged by babel)
///
/// This catches sessions that have exited to shell prompt but still have
/// the ✳ title, or windows that were previously identified and tagged.
pub fn find_claude_windows() -> Result<Vec<KittyWindow>> {
    let all_windows = list_windows()?;

    let claude_windows = all_windows
        .into_iter()
        .filter(|win| detect_claude_signals(win).is_claude())
        .collect();

    Ok(claude_windows)
}

/// Get a specific window by ID
///
/// Returns None if the window doesn't exist (may have been closed).
pub fn get_window(id: u64) -> Result<Option<KittyWindow>> {
    let windows = list_windows()?;
    Ok(windows.into_iter().find(|w| w.id == id))
}

/// Focus a window by ID
///
/// Uses `kitten @ focus-window --match id:N` to bring the window into focus.
/// May require disabling XFWM4's focus stealing prevention (see focus-steal wrapper in Bin/).
pub fn focus_window(id: u64) -> Result<()> {
    let socket = kitty_socket_path();
    let output = Command::new("kitten")
        .args(["@", "--to", &socket, "focus-window", "--match", &format!("id:{}", id)])
        .output()
        .context("Failed to execute 'kitten @ focus-window'")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("kitten @ focus-window failed: {}", stderr);
    }

    Ok(())
}

/// Set a user variable on a window
///
/// User variables are persisted in kitty and can be queried later.
/// Babel uses these to store session metadata (conversation_id, last_updated, etc.)
pub fn set_user_var(id: u64, key: &str, value: &str) -> Result<()> {
    let socket = kitty_socket_path();
    let output = Command::new("kitten")
        .args([
            "@", "--to", &socket, "set-user-vars",
            "--match", &format!("id:{}", id),
            &format!("{}={}", key, value),
        ])
        .output()
        .context("Failed to execute 'kitten @ set-user-vars'")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("kitten @ set-user-vars failed: {}", stderr);
    }

    Ok(())
}

/// Get the scrollback buffer from a window
///
/// Returns the entire scrollback as a string. Can be large - use with caution.
/// Uses --extent=all to get full scrollback history, not just visible screen.
pub fn get_scrollback(id: u64) -> Result<String> {
    let socket = kitty_socket_path();
    let output = Command::new("kitten")
        .args(["@", "--to", &socket, "get-text", "--match", &format!("id:{}", id), "--extent=all"])
        .output()
        .context("Failed to execute 'kitten @ get-text'")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("kitten @ get-text failed: {}", stderr);
    }

    Ok(String::from_utf8_lossy(&output.stdout).to_string())
}

/// Get the last N lines of scrollback from a window
///
/// Uses kitty's remote control to extract recent scrollback.
/// Useful for state detection where only recent lines matter.
pub fn get_recent_scrollback(id: u64, lines: usize) -> Result<String> {
    // Get screen content (visible area) which is faster than full scrollback
    let socket = kitty_socket_path();
    let output = Command::new("kitten")
        .args([
            "@", "--to", &socket, "get-text",
            "--match", &format!("id:{}", id),
            "--extent", "screen",
        ])
        .output()
        .context("Failed to execute 'kitten @ get-text'")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("kitten @ get-text failed: {}", stderr);
    }

    let text = String::from_utf8_lossy(&output.stdout);

    // Return the last N lines
    let result: Vec<&str> = text
        .lines()
        .rev()
        .take(lines)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();

    Ok(result.join("\n"))
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

/// Send text to a window's input
///
/// Text is sent as if typed by the user. Does not include a newline by default.
/// Used by fire.rs to inject prompts into existing claude sessions.
pub fn send_text(id: u64, text: &str) -> Result<()> {
    let socket = kitty_socket_path();
    let output = Command::new("kitten")
        .args([
            "@", "--to", &socket, "send-text",
            "--match", &format!("id:{}", id),
            text,
        ])
        .output()
        .context("Failed to execute 'kitten @ send-text'")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("kitten @ send-text failed: {}", stderr);
    }

    Ok(())
}

/// Set the title of a kitty window
///
/// Updates the window title displayed in kitty. The title persists until
/// changed by the shell or application running in the window.
pub fn set_window_title(id: u64, title: &str) -> Result<()> {
    let socket = kitty_socket_path();
    let output = Command::new("kitten")
        .args([
            "@", "--to", &socket, "set-window-title",
            "--match", &format!("id:{}", id),
            title,
        ])
        .output()
        .context("Failed to execute 'kitten @ set-window-title'")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("kitten @ set-window-title failed: {}", stderr);
    }

    Ok(())
}

/// Get the current working directory of the focused window
///
/// Used by claude-fire to determine the context for new prompts.
/// Returns None if no window is focused (shouldn't happen in practice).
pub fn get_focused_cwd() -> Result<Option<PathBuf>> {
    let windows = list_windows()?;
    Ok(windows
        .into_iter()
        .find(|w| w.is_focused)
        .map(|w| w.cwd))
}

/// Get workspace number for a platform window ID
///
/// Uses `wmctrl -l` to query window manager for workspace assignments.
/// Returns None if wmctrl fails or window not found.
pub fn get_workspace(platform_window_id: u64) -> Option<i32> {
    let output = Command::new("wmctrl")
        .args(["-l"])
        .output()
        .ok()?;

    if !output.status.success() {
        return None;
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let hex_id = format!("0x{:08x}", platform_window_id);

    // wmctrl -l format: "0x00e00003 -1 hostname title..."
    // Column 0: window ID (hex), Column 1: workspace (-1 = all)
    for line in stdout.lines() {
        let parts: Vec<&str> = line.split_whitespace().collect();
        if parts.len() >= 2 && parts[0].to_lowercase() == hex_id.to_lowercase() {
            return parts[1].parse().ok();
        }
    }

    None
}

/// Get all workspace mappings in one call (more efficient for multiple lookups)
///
/// Returns a map of platform_window_id -> workspace_number
pub fn get_all_workspaces() -> HashMap<u64, i32> {
    let mut result = HashMap::new();

    let output = match Command::new("wmctrl").args(["-l"]).output() {
        Ok(o) if o.status.success() => o,
        _ => return result,
    };

    let stdout = String::from_utf8_lossy(&output.stdout);

    for line in stdout.lines() {
        let parts: Vec<&str> = line.split_whitespace().collect();
        if parts.len() >= 2 {
            // Parse hex window ID (strip 0x prefix)
            if let Ok(window_id) = u64::from_str_radix(parts[0].trim_start_matches("0x"), 16) {
                if let Ok(workspace) = parts[1].parse::<i32>() {
                    result.insert(window_id, workspace);
                }
            }
        }
    }

    result
}

// ═══════════════════════════════════════════════════════════════════════════════
// WSet Loading - Spawn/Close/Move Windows
// ═══════════════════════════════════════════════════════════════════════════════

use crate::wset::WSet;

/// Close a kitty window by ID
///
/// Uses `kitten @ close-window` to terminate the window.
pub fn close_window(id: u64) -> Result<()> {
    let socket = kitty_socket_path();
    let output = Command::new("kitten")
        .args(["@", "--to", &socket, "close-window", "--match", &format!("id:{}", id)])
        .output()
        .context("Failed to execute 'kitten @ close-window'")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        // Don't fail if window already closed
        if !stderr.contains("No matching") {
            bail!("kitten @ close-window failed: {}", stderr);
        }
    }

    Ok(())
}

/// Close all claude windows
///
/// Used before loading a WSet to clear the slate.
pub fn close_all_claude_windows() -> Result<()> {
    let claude_windows = find_claude_windows()?;

    for win in claude_windows {
        if let Err(e) = close_window(win.id) {
            tracing::warn!(kitty_id = win.id, error = %e, "Failed to close window");
        }
    }

    Ok(())
}

/// Move a window to a specific workspace using wmctrl
///
/// Uses `wmctrl -i -r <hex_id> -t <workspace>` to move the window.
pub fn move_window_to_workspace(platform_window_id: u64, workspace: i32) -> Result<()> {
    let hex_id = format!("0x{:08x}", platform_window_id);

    let output = Command::new("wmctrl")
        .args(["-i", "-r", &hex_id, "-t", &workspace.to_string()])
        .output()
        .context("Failed to execute 'wmctrl'")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("wmctrl move failed: {}", stderr);
    }

    Ok(())
}

/// Spawn a claude session in a new kitty window
///
/// Uses `kitty-claude` script for consistent window setup with random backgrounds.
/// Returns the new kitty window ID after a brief delay for the window to appear.
pub async fn spawn_claude_session(session_id: &str, cwd: &std::path::Path) -> Result<Option<u64>> {
    use std::process::Stdio;
    use tokio::time::{sleep, Duration};

    // Verify the session exists in ~/.claude before spawning
    let claude_base = crate::claude_storage::claude_base();
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

    // Spawn kitty-claude with the session
    // kitty-claude handles random background selection and consistent styling
    let _child = Command::new("kitty-claude")
        .args(["-d", &cwd.to_string_lossy()])
        .args(["-e", "claude", "-r", session_id])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
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

/// Load a WSet by closing all existing claude windows and spawning new ones
///
/// Returns a list of session IDs that couldn't be restored (file missing, etc.)
pub async fn load_wset(wset: &WSet) -> Result<Vec<String>> {
    use tokio::time::{sleep, Duration};

    let mut skipped: Vec<String> = Vec::new();

    // Step 1: Close all existing claude windows
    tracing::info!(wset = %wset.meta.name, "Closing existing claude windows");
    close_all_claude_windows()?;

    // Brief pause to let windows close
    sleep(Duration::from_millis(300)).await;

    // Step 2: Spawn windows for each wspace
    for wspace in &wset.wspaces {
        tracing::info!(workspace = wspace.index, windows = wspace.windows.len(), "Spawning wspace");

        for window_config in &wspace.windows {
            match spawn_claude_session(&window_config.session_id, &window_config.cwd).await {
                Ok(Some(kitty_id)) => {
                    // Move to correct workspace
                    // Need to get platform_window_id first
                    if let Ok(Some(win)) = get_window(kitty_id) {
                        if let Err(e) = move_window_to_workspace(win.platform_window_id, wspace.index) {
                            tracing::warn!(
                                kitty_id,
                                workspace = wspace.index,
                                error = %e,
                                "Failed to move window to workspace"
                            );
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

// ═══════════════════════════════════════════════════════════════════════════════
// Tests
// ═══════════════════════════════════════════════════════════════════════════════

#[cfg(test)]
mod tests {
    use super::*;

    // Integration tests - require running kitty instance
    // Run with: cargo test -- --ignored

    #[test]
    #[ignore]
    fn test_list_windows() {
        let windows = list_windows().unwrap();
        assert!(!windows.is_empty(), "Should find at least one window");

        for win in &windows {
            tracing::info!("Window {}: {} (cwd: {})", win.id, win.title, win.cwd.display());
            for proc in &win.foreground_processes {
                tracing::debug!("  Process {}: {:?}", proc.pid, proc.cmdline);
            }
        }
    }

    #[test]
    #[ignore]
    fn test_find_claude_windows() {
        let claude_windows = find_claude_windows().unwrap();
        tracing::info!("Found {} claude windows", claude_windows.len());

        for win in &claude_windows {
            tracing::info!("Claude window {}: {}", win.id, win.title);
        }
    }

    #[test]
    #[ignore]
    fn test_get_focused_cwd() {
        let cwd = get_focused_cwd().unwrap();
        tracing::info!("Focused window cwd: {:?}", cwd);
        assert!(cwd.is_some(), "Should have a focused window");
    }
}
