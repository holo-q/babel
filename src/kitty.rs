//! Kitty Remote Control Wrapper
//!
//! This module provides a Rust interface to kitty's remote control protocol via `kitten @` commands.
//! It communicates with kitty through the socket at $XDG_RUNTIME_DIR/kitty.sock.
//!
//! The data model follows kitty's JSON output structure:
//!   OS Window -> Tabs -> Windows (panes)
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
/// Kitty can create sockets in multiple locations:
/// 1. `$XDG_RUNTIME_DIR/kitty.sock` - single-instance canonical path
/// 2. `$XDG_RUNTIME_DIR/kitty.sock-$PID` - per-process sockets
///
/// This function tries the canonical path first, then falls back to finding
/// any available PID-suffixed socket.
///
/// Note: kitten auto-detects from TTY, but systemd services have no TTY,
/// so we must explicitly pass `--to unix:$socket_path`.
pub fn kitty_socket_path() -> String {
    // Priority 1: Use KITTY_LISTEN_ON if set (current terminal's kitty instance)
    // This is the most reliable way to get the right socket
    if let Ok(listen_on) = env::var("KITTY_LISTEN_ON") {
        if !listen_on.is_empty() {
            return listen_on;
        }
    }

    let runtime_dir = env::var("XDG_RUNTIME_DIR").unwrap_or_else(|_| "/run/user/1000".to_string());

    // Priority 2: Find PID-suffixed sockets (more reliable than symlink)
    // List entries matching kitty.sock-* that are actual sockets
    if let Ok(entries) = std::fs::read_dir(&runtime_dir) {
        let mut sockets: Vec<_> = entries
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

        // Sort by name (which includes PID, so higher PIDs = more recent)
        sockets.sort_by(|a, b| b.file_name().cmp(&a.file_name()));

        if let Some(entry) = sockets.first() {
            return format!("unix:{}", entry.path().display());
        }
    }

    // Priority 3: Try canonical symlink path (often stale, but worth trying)
    let canonical = format!("{}/kitty.sock", runtime_dir);
    if std::path::Path::new(&canonical).exists() {
        if let Ok(meta) = std::fs::metadata(&canonical) {
            if meta.file_type().is_socket() {
                return format!("unix:{}", canonical);
            }
        }
    }

    // Ultimate fallback - return canonical even if broken
    format!("unix:{}", canonical)
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

/// Find all windows running claude (checks if "claude" appears in any foreground process cmdline)
///
/// Used by the overlay to discover active claude sessions.
pub fn find_claude_windows() -> Result<Vec<KittyWindow>> {
    let all_windows = list_windows()?;

    let claude_windows = all_windows
        .into_iter()
        .filter(|win| {
            win.foreground_processes.iter().any(|proc| {
                proc.cmdline.iter().any(|arg| arg.contains("claude"))
            })
        })
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
