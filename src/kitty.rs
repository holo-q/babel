//! Kitty Remote Control Wrapper
//!
//! This module provides a Rust interface to kitty's remote control protocol via `kitten @` commands.
//!
//! ## Socket Standard
//!
//! Kitty creates sockets at `$XDG_RUNTIME_DIR/kitty.sock-$PID` (NOT `kitty.sock` despite config).
//! See `Docs/15-kitty-single-instance-protocol.md` for details.
//!
//! ## Multi-Instance Support
//!
//! While single-instance is preferred, this module supports querying multiple kitty instances.
//! Each `KittyPane` carries its `socket` field, enabling operations to target the correct instance.
//!
//! - `list_panes()` - panes from the current/default socket
//! - `list_all_panes()` - panes from ALL kitty instances
//!
//! ## Data Model
//!
//! Follows kitty's JSON output structure: OS Window -> Tabs -> Windows (panes)
//!
//! Each pane can have:
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

// ═══════════════════════════════════════════════════════════════════════════════
// Socket Discovery
// ═══════════════════════════════════════════════════════════════════════════════

/// Get the default kitty socket path for remote control.
///
/// Priority:
/// 1. KITTY_LISTEN_ON environment variable (set by kitty in its shells)
/// 2. First `kitty.sock-*` found in XDG_RUNTIME_DIR
/// 3. Fallback to `kitty.sock` (usually doesn't exist)
///
/// Note: kitten auto-detects from TTY, but systemd services have no TTY,
/// so we must explicitly pass `--to unix:$socket_path`.
pub fn default_socket() -> String {
    // Priority 1: KITTY_LISTEN_ON env var (most authoritative - set by running kitty)
    if let Ok(socket) = std::env::var("KITTY_LISTEN_ON") {
        if !socket.is_empty() {
            return socket;
        }
    }

    let runtime_dir = runtime_dir_path();

    // Priority 2: Find kitty.sock-* sockets (kitty creates "kitty.sock-$PID")
    if let Ok(entries) = std::fs::read_dir(&runtime_dir) {
        let sockets: Vec<_> = entries
            .filter_map(|e| e.ok())
            .filter(|e| {
                let name = e.file_name();
                let name_str = name.to_string_lossy();
                if !name_str.starts_with("kitty.sock-") {
                    return false;
                }
                e.file_type().map(|ft| ft.is_socket()).unwrap_or(false)
            })
            .collect();

        if let Some(entry) = sockets.first() {
            return format!("unix:{}", entry.path().display());
        }
    }

    // Fallback: try kitty.sock (usually doesn't exist, but worth trying)
    let canonical = format!("{}/kitty.sock", runtime_dir);
    format!("unix:{}", canonical)
}

/// Find all kitty sockets on the system
///
/// Returns paths to all `kitty.sock-*` files in XDG_RUNTIME_DIR.
pub fn find_all_sockets() -> Vec<String> {
    let runtime_dir = runtime_dir_path();
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

/// Extract PID from a socket path (kitty.sock-12345 -> Some(12345))
pub fn socket_pid(socket: &str) -> Option<u32> {
    socket
        .rsplit("kitty.sock-")
        .next()
        .and_then(|s| s.parse::<u32>().ok())
}

fn runtime_dir_path() -> String {
    env::var("XDG_RUNTIME_DIR").unwrap_or_else(|_| {
        let uid = users::get_current_uid();
        format!("/run/user/{}", uid)
    })
}

// ═══════════════════════════════════════════════════════════════════════════════
// Data Structures
// ═══════════════════════════════════════════════════════════════════════════════

/// Address of a pane across kitty instances
///
/// Like a network address but for terminal panes.
/// Uniquely identifies a pane even when multiple kitty instances exist.
///
/// Window IDs are only unique within a single kitty instance. When multiple
/// kitty instances are running (e.g., after a crash or intentional multi-instance),
/// the same window ID can exist in different sockets. PaneAddr solves this by
/// combining socket + id into a composite key.
#[derive(Debug, Clone, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub struct PaneAddr {
    /// Socket path (e.g., "unix:/run/user/1000/kitty.sock-12345")
    pub socket: String,
    /// Pane ID within that socket's kitty instance
    pub id: u64,
}

impl PaneAddr {
    pub fn new(socket: impl Into<String>, id: u64) -> Self {
        Self { socket: socket.into(), id }
    }

    /// Create from KittyPane
    pub fn from_pane(pane: &KittyPane) -> Self {
        Self::new(&pane.socket, pane.id)
    }

    /// Short display form for warnings/logs
    /// e.g., "42@12345" (window 42 on kitty.sock-12345)
    pub fn short(&self) -> String {
        let sock_short = self.socket
            .rsplit("kitty.sock-")
            .next()
            .unwrap_or(&self.socket);
        format!("{}@{}", self.id, sock_short)
    }

    /// Check if this pane is on the current/default socket
    pub fn is_current_socket(&self) -> bool {
        self.socket == default_socket()
    }
}

impl std::fmt::Display for PaneAddr {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}:{}", self.socket, self.id)
    }
}

/// A kitty pane with all relevant metadata
///
/// Each pane carries its `socket` so operations can target the correct kitty instance.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct KittyPane {
    /// Socket for the kitty instance this pane belongs to
    pub socket: String,
    /// Unique ID within the kitty instance
    pub id: u64,
    pub title: String,
    pub cwd: PathBuf,
    pub is_focused: bool,
    /// Active within its tab
    pub is_active: bool,
    pub foreground_processes: Vec<ForegroundProcess>,
    #[serde(default)]
    pub user_vars: HashMap<String, String>,
    /// The kitty OS window ID (internal to kitty)
    pub os_window_id: u64,
    /// X11/Wayland window ID for workspace lookup
    pub platform_window_id: u64,
}

impl KittyPane {
    /// Get the address of this pane
    pub fn addr(&self) -> PaneAddr {
        PaneAddr::from_pane(self)
    }

    /// Focus this pane
    pub fn focus(&self) -> Result<()> {
        focus_pane_on_socket(&self.socket, self.id)
    }

    /// Send text to this pane's input
    pub fn send_text(&self, text: &str) -> Result<()> {
        send_text_on_socket(&self.socket, self.id, text)
    }

    /// Set a user variable on this pane
    pub fn set_user_var(&self, key: &str, value: &str) -> Result<()> {
        set_user_var_on_socket(&self.socket, self.id, key, value)
    }

    /// Set the title of this pane
    pub fn set_title(&self, title: &str) -> Result<()> {
        set_title_on_socket(&self.socket, self.id, title)
    }

    /// Get the full scrollback buffer
    pub fn scrollback(&self) -> Result<String> {
        get_scrollback_on_socket(&self.socket, self.id)
    }

    /// Get the last N lines of scrollback
    pub fn recent_scrollback(&self, lines: usize) -> Result<String> {
        get_recent_scrollback_on_socket(&self.socket, self.id, lines)
    }

    /// Close this pane
    pub fn close(&self) -> Result<()> {
        close_pane_on_socket(&self.socket, self.id)
    }

    /// Get workspace number for this pane's platform window
    pub fn workspace(&self) -> Option<i32> {
        get_workspace(self.platform_window_id)
    }

    /// Move this pane's window to a workspace
    pub fn move_to_workspace(&self, workspace: i32) -> Result<()> {
        move_window_to_workspace(self.platform_window_id, workspace)
    }
}

/// A foreground process running in a pane
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ForegroundProcess {
    pub pid: u32,
    pub cmdline: Vec<String>,
    pub cwd: PathBuf,
}

/// Information about a kitty instance (socket + its panes)
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
    /// Panes accessible through this socket
    pub panes: Vec<KittyPane>,
    /// Error message if not responsive
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

// ═══════════════════════════════════════════════════════════════════════════════
// Internal: Socket-targeted primitives
// ═══════════════════════════════════════════════════════════════════════════════

fn list_panes_on_socket(socket: &str) -> Result<Vec<KittyPane>> {
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

    // Flatten: OS Windows -> Tabs -> Windows (panes)
    let mut panes = Vec::new();
    for os_win in raw_os_windows {
        let os_window_id = os_win.id;
        let platform_window_id = os_win.platform_window_id;

        for tab in os_win.tabs {
            for raw in tab.windows {
                panes.push(KittyPane {
                    socket: socket.to_string(),
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

    Ok(panes)
}

pub(crate) fn focus_pane_on_socket(socket: &str, id: u64) -> Result<()> {
    let output = Command::new("kitten")
        .args(["@", "--to", socket, "focus-window", "--match", &format!("id:{}", id)])
        .output()
        .context("Failed to execute 'kitten @ focus-window'")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("kitten @ focus-window failed: {}", stderr);
    }

    Ok(())
}

pub(crate) fn send_text_on_socket(socket: &str, id: u64, text: &str) -> Result<()> {
    let output = Command::new("kitten")
        .args([
            "@", "--to", socket, "send-text",
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

pub(crate) fn set_user_var_on_socket(socket: &str, id: u64, key: &str, value: &str) -> Result<()> {
    let output = Command::new("kitten")
        .args([
            "@", "--to", socket, "set-user-vars",
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

pub(crate) fn set_title_on_socket(socket: &str, id: u64, title: &str) -> Result<()> {
    let output = Command::new("kitten")
        .args([
            "@", "--to", socket, "set-window-title",
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

pub(crate) fn get_scrollback_on_socket(socket: &str, id: u64) -> Result<String> {
    let output = Command::new("kitten")
        .args(["@", "--to", socket, "get-text", "--match", &format!("id:{}", id), "--extent=all"])
        .output()
        .context("Failed to execute 'kitten @ get-text'")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("kitten @ get-text failed: {}", stderr);
    }

    Ok(String::from_utf8_lossy(&output.stdout).to_string())
}

pub(crate) fn get_recent_scrollback_on_socket(socket: &str, id: u64, lines: usize) -> Result<String> {
    let output = Command::new("kitten")
        .args([
            "@", "--to", socket, "get-text",
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

pub(crate) fn close_pane_on_socket(socket: &str, id: u64) -> Result<()> {
    let output = Command::new("kitten")
        .args(["@", "--to", socket, "close-window", "--match", &format!("id:{}", id)])
        .output()
        .context("Failed to execute 'kitten @ close-window'")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        // Don't fail if pane already closed
        if !stderr.contains("No matching") {
            bail!("kitten @ close-window failed: {}", stderr);
        }
    }

    Ok(())
}

// ═══════════════════════════════════════════════════════════════════════════════
// Public API: Pane Listing
// ═══════════════════════════════════════════════════════════════════════════════

/// List panes from the default kitty instance
pub fn list_panes() -> Result<Vec<KittyPane>> {
    list_panes_on_socket(&default_socket())
}

/// List panes from ALL kitty instances
///
/// Queries every socket in XDG_RUNTIME_DIR. Unresponsive instances are skipped.
pub fn list_all_panes() -> Result<Vec<KittyPane>> {
    let sockets = find_all_sockets();
    let mut all_panes = Vec::new();

    for socket in &sockets {
        match list_panes_on_socket(socket) {
            Ok(panes) => all_panes.extend(panes),
            Err(e) => {
                tracing::debug!(socket = %socket, error = %e, "Failed to query kitty instance");
            }
        }
    }

    Ok(all_panes)
}

/// List panes from a specific socket
pub fn list_panes_from_socket(socket: &str) -> Result<Vec<KittyPane>> {
    list_panes_on_socket(socket)
}

/// Get a specific pane by ID from the default instance
pub fn get_pane(id: u64) -> Result<Option<KittyPane>> {
    let panes = list_panes()?;
    Ok(panes.into_iter().find(|p| p.id == id))
}

/// Get a specific pane by ID, searching all instances
///
/// Note: IDs are only unique within an instance. If multiple instances have
/// the same ID, returns the first match (current instance preferred).
pub fn get_pane_all(id: u64) -> Result<Option<KittyPane>> {
    // Check default instance first
    if let Some(pane) = get_pane(id)? {
        return Ok(Some(pane));
    }

    // Search other instances
    let panes = list_all_panes()?;
    Ok(panes.into_iter().find(|p| p.id == id))
}

// ═══════════════════════════════════════════════════════════════════════════════
// Public API: Instance Discovery
// ═══════════════════════════════════════════════════════════════════════════════

/// Discover all kitty instances on the system
///
/// Returns information about each instance including whether it's responsive.
pub fn discover_all_instances() -> Vec<KittyInstance> {
    let current_socket = default_socket();
    let all_sockets = find_all_sockets();

    let mut instances = Vec::new();

    for socket in all_sockets {
        let pid = socket_pid(&socket);
        let is_current = socket == current_socket;

        match list_panes_on_socket(&socket) {
            Ok(panes) => {
                instances.push(KittyInstance {
                    socket,
                    pid,
                    is_current,
                    is_responsive: true,
                    panes,
                    error: None,
                });
            }
            Err(e) => {
                instances.push(KittyInstance {
                    socket,
                    pid,
                    is_current,
                    is_responsive: false,
                    panes: Vec::new(),
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
// Public API: Legacy / Convenience (default socket, by ID)
// ═══════════════════════════════════════════════════════════════════════════════

/// Focus a pane by ID (default instance)
pub fn focus_window(id: u64) -> Result<()> {
    focus_pane_on_socket(&default_socket(), id)
}

/// Send text to a pane by ID (default instance)
pub fn send_text(id: u64, text: &str) -> Result<()> {
    send_text_on_socket(&default_socket(), id, text)
}

/// Set a user variable on a pane by ID (default instance)
pub fn set_user_var(id: u64, key: &str, value: &str) -> Result<()> {
    set_user_var_on_socket(&default_socket(), id, key, value)
}

/// Set title on a pane by ID (default instance)
pub fn set_window_title(id: u64, title: &str) -> Result<()> {
    set_title_on_socket(&default_socket(), id, title)
}

/// Get full scrollback from a pane by ID (default instance)
pub fn get_scrollback(id: u64) -> Result<String> {
    get_scrollback_on_socket(&default_socket(), id)
}

/// Get recent scrollback from a pane by ID (default instance)
pub fn get_recent_scrollback(id: u64, lines: usize) -> Result<String> {
    get_recent_scrollback_on_socket(&default_socket(), id, lines)
}

/// Close a pane by ID (default instance)
pub fn close_window(id: u64) -> Result<()> {
    close_pane_on_socket(&default_socket(), id)
}

/// Get the cwd of the focused pane (default instance)
pub fn get_focused_cwd() -> Result<Option<PathBuf>> {
    let panes = list_panes()?;
    Ok(panes.into_iter().find(|p| p.is_focused).map(|p| p.cwd))
}

// Legacy aliases
#[doc(hidden)]
pub fn list_windows() -> Result<Vec<KittyPane>> { list_panes() }
#[doc(hidden)]
pub fn get_window(id: u64) -> Result<Option<KittyPane>> { get_pane(id) }
#[doc(hidden)]
pub fn list_windows_from_socket(socket: &str) -> Result<Vec<KittyPane>> { list_panes_from_socket(socket) }

// ═══════════════════════════════════════════════════════════════════════════════
// Public API: Multi-Socket Operations (search all sockets)
// ═══════════════════════════════════════════════════════════════════════════════

/// Result of a multi-socket operation
///
/// Contains the result and whether the pane was on a non-current socket
/// (which may warrant a warning to the user).
#[derive(Debug)]
pub struct MultiSocketResult<T> {
    pub result: T,
    /// The address of the pane that was operated on
    pub addr: PaneAddr,
    /// True if the pane was on a non-current socket
    pub is_non_current: bool,
}

impl<T> MultiSocketResult<T> {
    /// Format a warning message if on non-current socket
    pub fn warning(&self) -> Option<String> {
        if self.is_non_current {
            Some(format!("⚠ Operating on non-current socket: {}", self.addr.short()))
        } else {
            None
        }
    }
}

/// Focus a pane by ID, searching all sockets
///
/// Returns the pane address and whether it was on a non-current socket.
pub fn focus_window_any(id: u64) -> Result<MultiSocketResult<()>> {
    let current = default_socket();
    match get_pane_all(id)? {
        Some(pane) => {
            let addr = pane.addr();
            let is_non_current = pane.socket != current;
            pane.focus()?;
            Ok(MultiSocketResult { result: (), addr, is_non_current })
        }
        None => bail!("Window {} not found in any kitty instance", id),
    }
}

/// Send text to a pane by ID, searching all sockets
pub fn send_text_any(id: u64, text: &str) -> Result<MultiSocketResult<()>> {
    let current = default_socket();
    match get_pane_all(id)? {
        Some(pane) => {
            let addr = pane.addr();
            let is_non_current = pane.socket != current;
            pane.send_text(text)?;
            Ok(MultiSocketResult { result: (), addr, is_non_current })
        }
        None => bail!("Window {} not found in any kitty instance", id),
    }
}

/// Get scrollback from a pane by ID, searching all sockets
pub fn get_scrollback_any(id: u64) -> Result<MultiSocketResult<String>> {
    let current = default_socket();
    match get_pane_all(id)? {
        Some(pane) => {
            let addr = pane.addr();
            let is_non_current = pane.socket != current;
            let text = pane.scrollback()?;
            Ok(MultiSocketResult { result: text, addr, is_non_current })
        }
        None => bail!("Window {} not found in any kitty instance", id),
    }
}

/// Set title on a pane by ID, searching all sockets
pub fn set_title_any(id: u64, title: &str) -> Result<MultiSocketResult<()>> {
    let current = default_socket();
    match get_pane_all(id)? {
        Some(pane) => {
            let addr = pane.addr();
            let is_non_current = pane.socket != current;
            pane.set_title(title)?;
            Ok(MultiSocketResult { result: (), addr, is_non_current })
        }
        None => bail!("Window {} not found in any kitty instance", id),
    }
}

/// Set user var on a pane by ID, searching all sockets
pub fn set_user_var_any(id: u64, key: &str, value: &str) -> Result<MultiSocketResult<()>> {
    let current = default_socket();
    match get_pane_all(id)? {
        Some(pane) => {
            let addr = pane.addr();
            let is_non_current = pane.socket != current;
            pane.set_user_var(key, value)?;
            Ok(MultiSocketResult { result: (), addr, is_non_current })
        }
        None => bail!("Window {} not found in any kitty instance", id),
    }
}

/// Close a pane by ID, searching all sockets
pub fn close_window_any(id: u64) -> Result<MultiSocketResult<()>> {
    let current = default_socket();
    match get_pane_all(id)? {
        Some(pane) => {
            let addr = pane.addr();
            let is_non_current = pane.socket != current;
            pane.close()?;
            Ok(MultiSocketResult { result: (), addr, is_non_current })
        }
        None => bail!("Window {} not found in any kitty instance", id),
    }
}

// ═══════════════════════════════════════════════════════════════════════════════
// Workspace Operations (wmctrl)
// ═══════════════════════════════════════════════════════════════════════════════

/// Get workspace number for a platform window ID
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

    for line in stdout.lines() {
        let parts: Vec<&str> = line.split_whitespace().collect();
        if parts.len() >= 2 && parts[0].to_lowercase() == hex_id.to_lowercase() {
            return parts[1].parse().ok();
        }
    }

    None
}

/// Get all workspace mappings in one call
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
            if let Ok(window_id) = u64::from_str_radix(parts[0].trim_start_matches("0x"), 16) {
                if let Ok(workspace) = parts[1].parse::<i32>() {
                    result.insert(window_id, workspace);
                }
            }
        }
    }

    result
}

/// Move a window to a specific workspace
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

// ═══════════════════════════════════════════════════════════════════════════════
// Raw JSON Structures (for parsing kitten @ ls output)
// ═══════════════════════════════════════════════════════════════════════════════

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
// Tests
// ═══════════════════════════════════════════════════════════════════════════════

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[ignore]
    fn test_list_panes() {
        let panes = list_panes().unwrap();
        assert!(!panes.is_empty(), "Should find at least one pane");

        for pane in &panes {
            println!("Pane {} [{}]: {} (cwd: {})",
                pane.id, pane.socket, pane.title, pane.cwd.display());
        }
    }

    #[test]
    #[ignore]
    fn test_list_all_panes() {
        let panes = list_all_panes().unwrap();
        println!("Found {} panes across all instances", panes.len());

        for pane in &panes {
            println!("  {} @ {}: {}", pane.id, pane.socket, pane.title);
        }
    }

    #[test]
    #[ignore]
    fn test_discover_instances() {
        let instances = discover_all_instances();
        println!("Found {} kitty instances", instances.len());

        for inst in &instances {
            println!("  {} (pid: {:?}, current: {}, responsive: {})",
                inst.socket, inst.pid, inst.is_current, inst.is_responsive);
            println!("    {} panes", inst.panes.len());
        }
    }
}
