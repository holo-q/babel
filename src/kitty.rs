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
use std::time::Duration;
use serde::{Deserialize, Serialize};
use anyhow::{Result, Context, bail};
use tracing::instrument;

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
// Kitty Config Parsing
// ═══════════════════════════════════════════════════════════════════════════════
//
// Babel requires kitty to be configured with remote control enabled.
// We parse kitty.conf to validate this and determine the socket base path.
//
// Kitty's socket naming: config says `listen_on unix:$XDG_RUNTIME_DIR/kitty.sock`
// but kitty auto-appends `-$PID` when loaded from config (see kitty/main.py:402).
// This means multiple instances = multiple sockets, always.
//
// Our strategy:
// - Parse config to get the BASE socket path (without PID)
// - "Main" socket = lowest PID (oldest instance, likely the intentional one)
// - Orphan sockets = higher PIDs (accidental spawns without -1 flag)

/// Kitty configuration relevant to babel
#[derive(Debug, Clone)]
pub struct KittyConfig {
    /// Whether remote control is enabled (required for babel)
    pub allow_remote_control: bool,
    /// Base socket path from config (without -$PID suffix)
    /// e.g., "unix:/run/user/1000/kitty.sock"
    pub listen_on_base: Option<String>,
    /// Path to the config file that was parsed
    pub config_path: PathBuf,
}

/// Errors when parsing/validating kitty config
#[derive(Debug, Clone)]
pub enum KittyConfigError {
    /// Config file not found
    NotFound(PathBuf),
    /// Remote control not enabled
    RemoteControlDisabled,
    /// No listen_on configured
    NoListenOn,
    /// listen_on doesn't use unix socket
    NotUnixSocket(String),
}

impl std::fmt::Display for KittyConfigError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotFound(path) => write!(f, "kitty.conf not found at {}", path.display()),
            Self::RemoteControlDisabled => write!(f,
                "kitty remote control not enabled. Add to kitty.conf:\n  allow_remote_control yes"),
            Self::NoListenOn => write!(f,
                "kitty listen_on not configured. Add to kitty.conf:\n  listen_on unix:$XDG_RUNTIME_DIR/kitty.sock"),
            Self::NotUnixSocket(val) => write!(f,
                "kitty listen_on must be unix socket, got: {}", val),
        }
    }
}

impl std::error::Error for KittyConfigError {}

/// Parse kitty.conf to extract remote control settings
///
/// Looks for:
/// - `allow_remote_control yes|true|password|socket|socket-only` (any truthy value)
/// - `listen_on unix:...` (the socket path, without -$PID suffix)
pub fn parse_kitty_config() -> std::result::Result<KittyConfig, KittyConfigError> {
    let config_path = kitty_config_path();

    if !config_path.exists() {
        return Err(KittyConfigError::NotFound(config_path));
    }

    let content = std::fs::read_to_string(&config_path)
        .map_err(|_| KittyConfigError::NotFound(config_path.clone()))?;

    let mut allow_remote_control = false;
    let mut listen_on_base = None;

    for line in content.lines() {
        let line = line.trim();

        // Skip comments and empty lines
        if line.is_empty() || line.starts_with('#') {
            continue;
        }

        // Parse key-value pairs (kitty uses space/tab separator)
        let parts: Vec<&str> = line.splitn(2, |c: char| c.is_whitespace()).collect();
        if parts.len() != 2 {
            continue;
        }

        let key = parts[0].trim();
        let value = parts[1].trim();

        match key {
            "allow_remote_control" => {
                // Truthy values: yes, true, password, socket, socket-only
                // Falsy: no, false
                allow_remote_control = !matches!(value.to_lowercase().as_str(), "no" | "false" | "");
            }
            "listen_on" => {
                // Expand $XDG_RUNTIME_DIR and ~ but NOT {kitty_pid}
                let expanded = expand_kitty_path(value);
                listen_on_base = Some(expanded);
            }
            _ => {}
        }
    }

    Ok(KittyConfig {
        allow_remote_control,
        listen_on_base,
        config_path,
    })
}

/// Validate kitty config for babel compatibility
///
/// Returns Ok(KittyConfig) if valid, Err with specific problem if not.
pub fn validate_kitty_config() -> std::result::Result<KittyConfig, KittyConfigError> {
    let config = parse_kitty_config()?;

    if !config.allow_remote_control {
        return Err(KittyConfigError::RemoteControlDisabled);
    }

    match &config.listen_on_base {
        None => return Err(KittyConfigError::NoListenOn),
        Some(path) if !path.starts_with("unix:") => {
            return Err(KittyConfigError::NotUnixSocket(path.clone()));
        }
        _ => {}
    }

    Ok(config)
}

/// Get the "main" kitty socket - the oldest instance (lowest PID)
///
/// In a healthy system, there's one kitty started intentionally (main).
/// Orphan sockets are accidental spawns that didn't use -1/--single-instance.
/// We identify main by lowest PID (oldest = likely the intentional one).
pub fn main_socket() -> Option<String> {
    let sockets = find_all_sockets();

    // Sort by PID (lowest first)
    let mut with_pids: Vec<_> = sockets.iter()
        .filter_map(|s| socket_pid(s).map(|pid| (pid, s.clone())))
        .collect();

    with_pids.sort_by_key(|(pid, _)| *pid);

    with_pids.first().map(|(_, s)| s.clone())
}

/// Check if a socket is the "main" one (lowest PID)
pub fn is_main_socket(socket: &str) -> bool {
    main_socket().as_deref() == Some(socket)
}

/// List orphan sockets (not the main one)
pub fn orphan_sockets() -> Vec<String> {
    let main = main_socket();
    find_all_sockets().into_iter()
        .filter(|s| main.as_ref() != Some(s))
        .collect()
}

/// Get default kitty config path
fn kitty_config_path() -> PathBuf {
    if let Ok(config_home) = env::var("XDG_CONFIG_HOME") {
        PathBuf::from(config_home).join("kitty/kitty.conf")
    } else if let Ok(home) = env::var("HOME") {
        PathBuf::from(home).join(".config/kitty/kitty.conf")
    } else {
        PathBuf::from("/etc/xdg/kitty/kitty.conf")
    }
}

/// Expand environment variables in kitty config paths
/// Handles $VAR syntax, plus ~
fn expand_kitty_path(path: &str) -> String {
    let mut result = path.to_string();

    // Expand ~ to HOME
    if result.starts_with('~') {
        if let Ok(home) = env::var("HOME") {
            result = result.replacen('~', &home, 1);
        }
    }

    // Expand common env vars used in kitty configs
    // Simple approach: just handle the known ones kitty uses
    for var in ["XDG_RUNTIME_DIR", "HOME", "TMPDIR", "XDG_CONFIG_HOME"] {
        if let Ok(val) = env::var(var) {
            result = result.replace(&format!("${}", var), &val);
            result = result.replace(&format!("${{{}}}", var), &val);
        }
    }

    result
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

/// Screen geometry for a pane (absolute coordinates)
///
/// This is the pane's position on the screen, including the OS window position.
/// Kitty provides this in the `screen` field of `kitten @ ls` output (newer versions).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScreenGeometry {
    /// Absolute X position on screen
    pub x: i32,
    /// Absolute Y position on screen
    pub y: i32,
    /// Width in pixels
    pub width: u32,
    /// Height in pixels
    pub height: u32,
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
    /// Screen geometry (absolute coordinates) - newer kitty versions only
    ///
    /// When available, this provides precise per-pane positioning for multi-pane windows.
    /// Falls back to OS window geometry if not provided by kitty.
    #[serde(default)]
    pub screen: Option<ScreenGeometry>,
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
// XFWM4 Focus Stealing Prevention Bypass
// ═══════════════════════════════════════════════════════════════════════════════
//
// XFWM4 has focus stealing prevention enabled by default. This blocks programmatic
// focus requests from `kitten @ focus-window`. To work around this, we temporarily
// disable prevention, focus the window, wait for the WM to process, then restore.
//
// This matches the behavior of `focus-steal wrap` from Lib/focus-steal but
// implemented in pure Rust for daemon compatibility (no nushell dependency).

const XFWM4_CHANNEL: &str = "xfwm4";
const PREVENT_STEALING_PROP: &str = "/general/prevent_focus_stealing";
const ACTIVATE_ACTION_PROP: &str = "/general/activate_action";
/// Delay after focus to let WM process before restoring prevention
const FOCUS_SETTLE_MS: u64 = 100;

/// Get an xfconf property value
fn xfconf_get(property: &str) -> Option<String> {
    let output = Command::new("xfconf-query")
        .args(["-c", XFWM4_CHANNEL, "-p", property])
        .output()
        .ok()?;

    if output.status.success() {
        Some(String::from_utf8_lossy(&output.stdout).trim().to_string())
    } else {
        None
    }
}

/// Set an xfconf property value
fn xfconf_set(property: &str, value: &str) -> Result<()> {
    let output = Command::new("xfconf-query")
        .args(["-c", XFWM4_CHANNEL, "-p", property, "-s", value])
        .output()
        .context("Failed to execute xfconf-query")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("xfconf-query set failed: {}", stderr);
    }

    Ok(())
}

/// State of XFWM4 focus stealing prevention
struct FocusStealingState {
    prevent: Option<String>,
    action: Option<String>,
}

impl FocusStealingState {
    /// Save current state
    fn save() -> Self {
        Self {
            prevent: xfconf_get(PREVENT_STEALING_PROP),
            action: xfconf_get(ACTIVATE_ACTION_PROP),
        }
    }

    /// Restore saved state
    fn restore(self) {
        if let Some(prevent) = self.prevent {
            let _ = xfconf_set(PREVENT_STEALING_PROP, &prevent);
        }
        if let Some(action) = self.action {
            let _ = xfconf_set(ACTIVATE_ACTION_PROP, &action);
        }
    }
}

/// Temporarily disable focus stealing prevention
///
/// Returns a guard that restores the original state on drop.
fn disable_focus_prevention() -> Option<FocusStealingState> {
    let state = FocusStealingState::save();

    // Only proceed if we successfully saved state (xfconf available)
    if state.prevent.is_none() && state.action.is_none() {
        tracing::debug!("xfconf not available, skipping focus prevention bypass");
        return None;
    }

    // Disable prevention and set action to "bring"
    if let Err(e) = xfconf_set(PREVENT_STEALING_PROP, "false") {
        tracing::debug!("Failed to disable focus prevention: {}", e);
        return None;
    }
    if let Err(e) = xfconf_set(ACTIVATE_ACTION_PROP, "bring") {
        tracing::debug!("Failed to set activate action: {}", e);
    }

    Some(state)
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
                    screen: raw.screen,
                });
            }
        }
    }

    Ok(panes)
}

pub(crate) fn focus_pane_on_socket(socket: &str, id: u64) -> Result<()> {
    // Temporarily disable XFWM4 focus stealing prevention
    // This is necessary because XFWM4 blocks programmatic focus requests by default
    let saved_state = disable_focus_prevention();

    let output = Command::new("kitten")
        .args(["@", "--to", socket, "focus-window", "--match", &format!("id:{}", id)])
        .output()
        .context("Failed to execute 'kitten @ focus-window'")?;

    // Wait for WM to process the focus request before restoring prevention
    if saved_state.is_some() {
        std::thread::sleep(Duration::from_millis(FOCUS_SETTLE_MS));
    }

    // Restore original focus prevention state
    if let Some(state) = saved_state {
        state.restore();
    }

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
#[instrument(level = "debug")]
pub fn list_panes() -> Result<Vec<KittyPane>> {
    list_panes_on_socket(&default_socket())
}

/// List panes from ALL kitty instances
///
/// Queries every socket in XDG_RUNTIME_DIR. Unresponsive instances are skipped.
#[instrument(level = "debug")]
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

/// Get all panes in a kitty OS window by its X11/Wayland platform window ID
///
/// When the user clicks a kitty window via slop, this maps the X11 window ID
/// to the kitty panes within that window. A single kitty OS window can contain
/// multiple panes (splits) across multiple tabs.
///
/// Returns empty Vec if the platform_window_id doesn't match any kitty instance.
pub fn get_panes_by_platform_id(platform_window_id: u64) -> Result<Vec<KittyPane>> {
    let panes = list_all_panes()?;
    Ok(panes.into_iter().filter(|p| p.platform_window_id == platform_window_id).collect())
}

// ═══════════════════════════════════════════════════════════════════════════════
// Public API: Instance Discovery
// ═══════════════════════════════════════════════════════════════════════════════

/// Discover all kitty instances on the system
///
/// Returns information about each instance including whether it's responsive.
#[instrument(level = "debug")]
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
#[instrument(level = "debug")]
pub fn focus_window(id: u64) -> Result<()> {
    focus_pane_on_socket(&default_socket(), id)
}

/// Send text to a pane by ID (default instance)
#[instrument(level = "debug", skip(text))]
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
#[instrument(level = "debug")]
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
#[instrument(level = "debug")]
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
#[instrument(level = "debug", skip(text))]
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
#[instrument(level = "debug")]
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
// Window Geometry (for multi-monitor precise restoration)
// ═══════════════════════════════════════════════════════════════════════════════

use crate::wset::PaneGeometry;

/// Get window geometry using xdotool
///
/// Returns (x, y, width, height) for precise multi-monitor restoration.
/// Uses xdotool which handles frame/decoration offsets consistently.
pub fn get_window_geometry(platform_window_id: u64) -> Result<PaneGeometry> {
    let hex_id = format!("0x{:x}", platform_window_id);

    // Get geometry with xdotool
    let output = Command::new("xdotool")
        .args(["getwindowgeometry", "--shell", &hex_id])
        .output()
        .context("Failed to execute 'xdotool'")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("xdotool getwindowgeometry failed: {}", stderr);
    }

    let stdout = String::from_utf8_lossy(&output.stdout);

    // Parse shell format output:
    // WINDOW=123456
    // X=100
    // Y=200
    // WIDTH=800
    // HEIGHT=600
    // SCREEN=0
    let mut x = 0i32;
    let mut y = 0i32;
    let mut width = 0u32;
    let mut height = 0u32;

    for line in stdout.lines() {
        if let Some((key, value)) = line.split_once('=') {
            match key {
                "X" => x = value.parse().unwrap_or(0),
                "Y" => y = value.parse().unwrap_or(0),
                "WIDTH" => width = value.parse().unwrap_or(0),
                "HEIGHT" => height = value.parse().unwrap_or(0),
                _ => {}
            }
        }
    }

    // Get monitor name for this position
    let monitor = get_monitor_at_position(x, y);

    Ok(PaneGeometry {
        x,
        y,
        width,
        height,
        monitor,
    })
}

/// Set window geometry using wmctrl
///
/// Moves and resizes window to exact position. The gravity parameter (first value)
/// is 0 to use default positioning.
pub fn set_window_geometry(platform_window_id: u64, geom: &PaneGeometry) -> Result<()> {
    let hex_id = format!("0x{:08x}", platform_window_id);

    // wmctrl -i -r <id> -e <gravity>,<x>,<y>,<width>,<height>
    // gravity 0 = use default, -1 = don't change position, etc.
    let geom_str = format!("0,{},{},{},{}", geom.x, geom.y, geom.width, geom.height);

    let output = Command::new("wmctrl")
        .args(["-i", "-r", &hex_id, "-e", &geom_str])
        .output()
        .context("Failed to execute 'wmctrl'")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("wmctrl geometry set failed: {}", stderr);
    }

    tracing::debug!(
        window = hex_id,
        x = geom.x, y = geom.y,
        w = geom.width, h = geom.height,
        "Set window geometry"
    );

    Ok(())
}

/// Get monitor name at a given position using xrandr
///
/// Parses xrandr output to find which monitor contains the given coordinates.
/// Returns None if position is outside all monitors or xrandr fails.
fn get_monitor_at_position(x: i32, y: i32) -> Option<String> {
    let output = Command::new("xrandr")
        .args(["--query"])
        .output()
        .ok()?;

    if !output.status.success() {
        return None;
    }

    let stdout = String::from_utf8_lossy(&output.stdout);

    // Parse xrandr output for connected monitors with geometry
    // Format: "HDMI-1 connected 1920x1080+0+0 ..."
    for line in stdout.lines() {
        if !line.contains(" connected") {
            continue;
        }

        let parts: Vec<&str> = line.split_whitespace().collect();
        if parts.len() < 3 {
            continue;
        }

        let monitor_name = parts[0];

        // Find geometry pattern: WxH+X+Y
        for part in &parts[2..] {
            if let Some((dims, pos)) = part.split_once('+') {
                if let Some((w, h)) = dims.split_once('x') {
                    let mon_w: i32 = w.parse().ok()?;
                    let mon_h: i32 = h.parse().ok()?;

                    // Parse +X+Y or +X-Y etc.
                    let coords: Vec<&str> = pos.split(|c| c == '+' || c == '-').collect();
                    if coords.len() >= 2 {
                        let mon_x: i32 = coords[0].parse().ok()?;
                        let mon_y: i32 = coords[1].parse().ok()?;

                        // Check if point is within this monitor
                        if x >= mon_x && x < mon_x + mon_w &&
                           y >= mon_y && y < mon_y + mon_h {
                            return Some(monitor_name.to_string());
                        }
                    }
                }
            }
        }
    }

    None
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
    windows: Vec<RawPane>,
}

#[derive(Debug, Deserialize)]
struct RawPane {
    id: u64,
    title: String,
    cwd: PathBuf,
    is_focused: bool,
    is_active: bool,
    foreground_processes: Vec<RawForegroundProcess>,
    #[serde(default)]
    user_vars: HashMap<String, String>,
    /// Screen geometry from newer kitty versions (absolute coordinates)
    #[serde(default)]
    screen: Option<ScreenGeometry>,
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
