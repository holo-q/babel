//! Kitty terminal backend
//!
//! Implements [`TerminalBackend`] for kitty via `kitten @` remote control commands.
//! Each operation wraps a subprocess call to kitten with timeout protection.
//!
//! ## Socket Standard
//!
//! Kitty creates sockets at `$XDG_RUNTIME_DIR/kitty.sock-$PID` (NOT `kitty.sock` despite config).
//! Each socket is a gateway to a tower of vessels, each capable of holding a working mind.
//! See `docs/15-kitty-single-instance-protocol.md` for details.
//!
//! ## Multi-Instance Support
//!
//! While single-instance is preferred, this module supports querying multiple kitty instances.
//! Each [`Pane`] carries its `connection` field, enabling operations to target the correct vessel
//! across towers.
//!
//! ## Data Model
//!
//! Follows kitty's JSON output structure: OS Window -> Tabs -> Windows (panes)
//!
//! Each pane can have:
//!   - foreground_processes: Running commands (we use this to find agent sessions)
//!   - user_vars: Custom key-value metadata (for babel state)
//!   - cwd: Current working directory

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::env;
use std::os::unix::fs::FileTypeExt;
use std::path::PathBuf;
use std::time::Duration;
use tokio::time::timeout;
use tracing::instrument;
use async_trait::async_trait;

use crate::model::PaneAddr;
use crate::desktop::{disable_focus_prevention, FOCUS_SETTLE_MS};
use super::{Pane, PaneExtras, ScreenGeometry, ForegroundProcess, BackendInstance, TerminalBackend};

// VTR semantic tracing - boundary markers for kitty IPC crossings
// - boundary!: External domain crossings (IPC, subprocess calls)
// - effect!: State-changing operations (mutations, side effects)
// - trace_error!: Error conditions with semantic context
use vtr::{boundary, effect, trace_error};

// =============================================================================
// Async Subprocess Helpers
// =============================================================================
//
// Problem: kitten @ subprocess calls can block forever if kitty is unresponsive.
// Solution: Wrap all async subprocess calls with timeouts.
//
// Default timeouts:
// - 5s for quick operations (ls, focus, send-text, set-colors)
// - 10s for data-heavy operations (get-text scrollback)

const KITTEN_TIMEOUT_SHORT: Duration = Duration::from_secs(5);
const KITTEN_TIMEOUT_LONG: Duration = Duration::from_secs(10);

/// Run a kitten @ command with timeout protection
///
/// This is the foundation for all kitty remote control operations.
/// If kitty becomes unresponsive (socket exists but process hung), we don't
/// want babel to block forever waiting for a response.
async fn run_kitten_with_timeout(
    socket: &str,
    args: &[&str],
    duration: Duration,
) -> Result<std::process::Output> {
    // VTR boundary: crossing into kitty's domain via IPC
    let op = args.first().unwrap_or(&"unknown");
    boundary!("kitty", op, socket = socket);

    let output = timeout(duration, async {
        tokio::process::Command::new("kitten")
            .args(["@", "--to", socket])
            .args(args)
            .output()
            .await
            .context("kitten subprocess failed")
    })
    .await
    .context("kitten subprocess timed out")??;

    Ok(output)
}

// =============================================================================
// Socket Discovery
// =============================================================================

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

            if name_str.starts_with("kitty.sock-")
                && entry.file_type().map(|ft| ft.is_socket()).unwrap_or(false)
            {
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

/// Get the "main" kitty socket - the oldest instance (lowest PID)
///
/// In a healthy system, there's one kitty started intentionally (main).
/// Orphan sockets are accidental spawns that didn't use -1/--single-instance.
/// We identify main by lowest PID (oldest = likely the intentional one).
pub fn main_socket() -> Option<String> {
    let sockets = find_all_sockets();

    // Sort by PID (lowest first)
    let mut with_pids: Vec<_> = sockets
        .iter()
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
    find_all_sockets()
        .into_iter()
        .filter(|s| main.as_ref() != Some(s))
        .collect()
}

// =============================================================================
// Kitty Config Parsing
// =============================================================================
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
                allow_remote_control =
                    !matches!(value.to_lowercase().as_str(), "no" | "false" | "");
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

// =============================================================================
// Data Structures
// =============================================================================

// Kitty-specific operations on the shared pane identity live here at the
// runtime edge. The canonical identity type itself is `model::PaneAddr`.
impl PaneAddr {
    /// Create from KittyPane
    pub fn from_pane(pane: &KittyPane) -> Self {
        Self::new(&pane.socket, pane.id)
    }

    /// Check if this pane is on the current/default socket
    pub fn is_current_socket(&self) -> bool {
        self.socket == default_socket()
    }
}

/// A kitty pane with all relevant metadata
///
/// The vessel -- a terminal container awaiting a soul. Each pane is a physical form
/// that can hold an agent worker's conversation. The pane is the body; elsewhere,
/// AgentPane adds the soul.
///
/// Each vessel carries its `socket` so operations can reach into the correct tower.
///
/// NOTE: This struct is kept temporarily for backward compatibility during the
/// migration to the multi-backend architecture. New code should use [`Pane`] from
/// `crate::backend`. Consumer migration will remove all KittyPane usage.
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

    /// Focus this pane - bring the vessel into awareness
    pub async fn focus(&self) -> Result<()> {
        focus_pane_on_socket(&self.socket, self.id).await
    }

    /// Send text to this pane's input - speak into the vessel
    pub async fn send_text(&self, text: &str) -> Result<()> {
        send_text_on_socket(&self.socket, self.id, text).await
    }

    /// Set a user variable on this pane
    pub async fn set_user_var(&self, key: &str, value: &str) -> Result<()> {
        set_user_var_on_socket(&self.socket, self.id, key, value).await
    }

    /// Set the title of this pane
    pub async fn set_title(&self, title: &str) -> Result<()> {
        set_title_on_socket(&self.socket, self.id, title).await
    }

    /// Get the full scrollback buffer - read the vessel's memory
    pub async fn scrollback(&self) -> Result<String> {
        get_scrollback_on_socket(&self.socket, self.id).await
    }

    /// Get the last N lines of scrollback - glimpse recent vessel activity
    pub async fn recent_scrollback(&self, lines: usize) -> Result<String> {
        get_recent_scrollback_on_socket(&self.socket, self.id, lines).await
    }

    /// Close this pane - dismiss the vessel
    pub async fn close(&self) -> Result<()> {
        close_pane_on_socket(&self.socket, self.id).await
    }

    /// Get workspace number for this pane's platform window
    pub fn workspace(&self) -> Option<i32> {
        crate::desktop::get_workspace(self.platform_window_id)
    }

    /// Move this pane's window to a workspace
    pub fn move_to_workspace(&self, workspace: i32) -> Result<()> {
        crate::desktop::move_window_to_workspace(self.platform_window_id, workspace)
    }
}

/// Information about a kitty instance (socket + its panes)
///
/// A tower in the realm -- a collection of vessels connected through one socket gateway.
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
    /// Vessels accessible through this gateway
    pub panes: Vec<KittyPane>,
    /// Error message if not responsive
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

impl KittyInstance {
    /// Convert to the backend-agnostic [`BackendInstance`] type.
    pub fn to_backend_instance(&self) -> BackendInstance {
        BackendInstance {
            connection: self.socket.clone(),
            pid: self.pid,
            is_current: self.is_current,
            is_responsive: self.is_responsive,
            panes: self
                .panes
                .iter()
                .map(|kp| Pane {
                    connection: kp.socket.clone(),
                    id: kp.id,
                    title: kp.title.clone(),
                    cwd: kp.cwd.clone(),
                    is_focused: kp.is_focused,
                    is_active: kp.is_active,
                    foreground_processes: kp.foreground_processes.clone(),
                    user_vars: kp.user_vars.clone(),
                    platform_window_id: Some(kp.platform_window_id),
                    extras: PaneExtras::Kitty {
                        os_window_id: kp.os_window_id,
                        screen: kp.screen.clone(),
                    },
                })
                .collect(),
            error: self.error.clone(),
        }
    }
}

// =============================================================================
// KittyBackend — TerminalBackend implementation
// =============================================================================

/// Kitty terminal backend using `kitten @` remote control.
pub struct KittyBackend;

#[async_trait]
impl TerminalBackend for KittyBackend {
    fn backend_name(&self) -> &'static str {
        "kitty"
    }

    fn default_connection(&self) -> Option<String> {
        // Check if any kitty socket actually exists
        if find_all_sockets().is_empty() {
            None
        } else {
            Some(default_socket())
        }
    }

    fn find_all_connections(&self) -> Vec<String> {
        find_all_sockets()
    }

    async fn list_panes(&self, conn: &str) -> Result<Vec<Pane>> {
        let kitty_panes = list_panes_on_socket(conn).await?;
        Ok(kitty_panes.into_iter().map(|kp| kitty_pane_to_pane(kp)).collect())
    }

    async fn discover_instances(&self) -> Vec<BackendInstance> {
        discover_all_instances()
            .await
            .into_iter()
            .map(|ki| ki.to_backend_instance())
            .collect()
    }

    async fn focus_pane(&self, conn: &str, id: u64) -> Result<()> {
        focus_pane_on_socket(conn, id).await
    }

    async fn send_text(&self, conn: &str, id: u64, text: &str) -> Result<()> {
        send_text_on_socket(conn, id, text).await
    }

    async fn get_scrollback(&self, conn: &str, id: u64) -> Result<String> {
        get_scrollback_on_socket(conn, id).await
    }

    async fn get_recent_scrollback(&self, conn: &str, id: u64, lines: usize) -> Result<String> {
        get_recent_scrollback_on_socket(conn, id, lines).await
    }

    async fn close_pane(&self, conn: &str, id: u64) -> Result<()> {
        close_pane_on_socket(conn, id).await
    }

    async fn set_meta(&self, conn: &str, id: u64, key: &str, val: &str) -> Result<()> {
        set_user_var_on_socket(conn, id, key, val).await
    }

    async fn set_title(&self, conn: &str, id: u64, title: &str) -> Result<()> {
        set_title_on_socket(conn, id, title).await
    }

    async fn set_border_color(
        &self,
        conn: &str,
        id: u64,
        active: &str,
        inactive: &str,
    ) -> Result<()> {
        set_border_color_on_socket(conn, id, active, inactive).await
    }

    async fn reset_border_color(&self, conn: &str, id: u64) -> Result<()> {
        reset_border_color_on_socket(conn, id).await
    }

    async fn list_panes_raw(&self, conn: &str) -> Result<String> {
        list_panes_raw_on_socket(conn).await
    }

    fn has_desktop_windows(&self) -> bool {
        true
    }

    fn has_border_coloring(&self) -> bool {
        true
    }

    fn has_raw_layout(&self) -> bool {
        true
    }
}

// =============================================================================
// Conversion helpers
// =============================================================================

/// Convert a [`KittyPane`] into the backend-agnostic [`Pane`].
pub fn kitty_pane_to_pane(kp: KittyPane) -> Pane {
    Pane {
        connection: kp.socket,
        id: kp.id,
        title: kp.title,
        cwd: kp.cwd,
        is_focused: kp.is_focused,
        is_active: kp.is_active,
        foreground_processes: kp.foreground_processes,
        user_vars: kp.user_vars,
        platform_window_id: Some(kp.platform_window_id),
        extras: PaneExtras::Kitty {
            os_window_id: kp.os_window_id,
            screen: kp.screen,
        },
    }
}

// =============================================================================
// Internal: Socket-targeted primitives
// =============================================================================

#[tracing::instrument(level = "debug")]
async fn list_panes_on_socket(socket: &str) -> Result<Vec<KittyPane>> {
    let output = run_kitten_with_timeout(socket, &["ls"], KITTEN_TIMEOUT_SHORT).await?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("kitten @ ls failed: {}", stderr);
    }

    let raw_os_windows: Vec<RawOsWindow> =
        serde_json::from_slice(&output.stdout).context("Failed to parse 'kitten @ ls' output")?;

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
                    foreground_processes: raw
                        .foreground_processes
                        .into_iter()
                        .map(|proc| ForegroundProcess {
                            pid: proc.pid,
                            cmdline: proc.cmdline,
                            // Null cwd from kitty (unreadable /proc/<pid>/cwd)
                            // collapses to an empty PathBuf. Downstream callers
                            // already treat cwd as best-effort; an empty path
                            // is interpreted as "unknown" and skipped.
                            cwd: proc.cwd.unwrap_or_default(),
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

    effect!("kitty", "panes listed", count = panes.len());
    Ok(panes)
}

/// Get raw JSON from kitten @ ls (for layout capture)
///
/// Returns the unparsed JSON string containing full layout_state info
/// that isn't captured by our KittyPane struct. Used by layout.rs for
/// capturing split tree structure.
#[tracing::instrument(level = "debug")]
pub async fn list_panes_raw_on_socket(socket: &str) -> Result<String> {
    let output = run_kitten_with_timeout(socket, &["ls"], KITTEN_TIMEOUT_SHORT).await?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("kitten @ ls failed: {}", stderr);
    }

    String::from_utf8(output.stdout).context("kitten @ ls output is not valid UTF-8")
}

#[instrument(level = "debug", skip(socket), fields(kitty_id = id))]
pub(crate) async fn focus_pane_on_socket(socket: &str, id: u64) -> Result<()> {
    // Temporarily disable XFWM4 focus stealing prevention
    // Reaching into a vessel requires bypassing WM gatekeeping
    let saved_state = disable_focus_prevention();

    let match_arg = format!("id:{}", id);
    let output = run_kitten_with_timeout(
        socket,
        &["focus-window", "--match", &match_arg],
        KITTEN_TIMEOUT_SHORT,
    )
    .await?;

    // Wait for WM to process the focus request before restoring prevention
    if saved_state.is_some() {
        tokio::time::sleep(Duration::from_millis(FOCUS_SETTLE_MS)).await;
    }

    // Restore original focus prevention state
    if let Some(state) = saved_state {
        state.restore();
    }

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("kitten @ focus-window failed: {}", stderr);
    }

    effect!("pane", "focused", id = id);
    Ok(())
}

#[instrument(level = "debug", skip(socket, text), fields(kitty_id = id, text_len = text.len()))]
pub(crate) async fn send_text_on_socket(socket: &str, id: u64, text: &str) -> Result<()> {
    let match_arg = format!("id:{}", id);
    let output = run_kitten_with_timeout(
        socket,
        &["send-text", "--match", &match_arg, text],
        KITTEN_TIMEOUT_SHORT,
    )
    .await?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("kitten @ send-text failed: {}", stderr);
    }

    effect!("pane", "text sent", id = id, len = text.len());
    Ok(())
}

#[instrument(level = "debug", skip(socket, value), fields(kitty_id = id))]
pub(crate) async fn set_user_var_on_socket(
    socket: &str,
    id: u64,
    key: &str,
    value: &str,
) -> Result<()> {
    let match_arg = format!("id:{}", id);
    let var_arg = format!("{}={}", key, value);
    let output = run_kitten_with_timeout(
        socket,
        &["set-user-vars", "--match", &match_arg, &var_arg],
        KITTEN_TIMEOUT_SHORT,
    )
    .await?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("kitten @ set-user-vars failed: {}", stderr);
    }

    effect!("pane", "user var set", id = id, key = key);
    Ok(())
}

#[instrument(level = "debug", skip(socket), fields(kitty_id = id))]
pub(crate) async fn set_title_on_socket(socket: &str, id: u64, title: &str) -> Result<()> {
    let match_arg = format!("id:{}", id);
    let output = run_kitten_with_timeout(
        socket,
        &["set-window-title", "--match", &match_arg, title],
        KITTEN_TIMEOUT_SHORT,
    )
    .await?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("kitten @ set-window-title failed: {}", stderr);
    }

    effect!("pane", "title set", id = id, title = title);
    Ok(())
}

#[instrument(level = "debug", skip(socket), fields(kitty_id = id))]
pub(crate) async fn get_scrollback_on_socket(socket: &str, id: u64) -> Result<String> {
    let match_arg = format!("id:{}", id);
    // Longer timeout for scrollback - can be large data transfer
    let output = run_kitten_with_timeout(
        socket,
        &["get-text", "--match", &match_arg, "--extent=all"],
        KITTEN_TIMEOUT_LONG,
    )
    .await?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("kitten @ get-text failed: {}", stderr);
    }

    Ok(String::from_utf8_lossy(&output.stdout).to_string())
}

#[instrument(level = "debug", skip(socket), fields(kitty_id = id))]
pub(crate) async fn get_recent_scrollback_on_socket(
    socket: &str,
    id: u64,
    lines: usize,
) -> Result<String> {
    let match_arg = format!("id:{}", id);
    let output = run_kitten_with_timeout(
        socket,
        &["get-text", "--match", &match_arg, "--extent", "screen"],
        KITTEN_TIMEOUT_SHORT,
    )
    .await?;

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

#[instrument(level = "debug", skip(socket), fields(kitty_id = id))]
pub async fn close_pane_on_socket(socket: &str, id: u64) -> Result<()> {
    let match_arg = format!("id:{}", id);
    let output = run_kitten_with_timeout(
        socket,
        &["close-window", "--match", &match_arg],
        KITTEN_TIMEOUT_SHORT,
    )
    .await?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        // Don't fail if pane already closed
        if !stderr.contains("No matching") {
            bail!("kitten @ close-window failed: {}", stderr);
        }
    }

    effect!("pane", "closed", id = id);
    Ok(())
}

/// Set border colors for a specific pane -- the ring speaks the worker's state
///
/// The border is the visual echo of attention: bright when the worker calls,
/// dim when their voice has been heard.
#[instrument(level = "debug", skip(socket), fields(kitty_id = id))]
pub async fn set_border_color_on_socket(
    socket: &str,
    id: u64,
    active_color: &str,
    inactive_color: &str,
) -> Result<()> {
    let match_arg = format!("id:{}", id);
    let active_arg = format!("active_border_color={}", active_color);
    let inactive_arg = format!("inactive_border_color={}", inactive_color);
    let output = run_kitten_with_timeout(
        socket,
        &[
            "set-colors",
            "--match",
            &match_arg,
            &active_arg,
            &inactive_arg,
        ],
        KITTEN_TIMEOUT_SHORT,
    )
    .await?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("kitten @ set-colors failed: {}", stderr);
    }

    effect!("pane", "border color set", id = id, active = active_color);
    Ok(())
}

/// Reset border colors to theme defaults for a specific pane
#[instrument(level = "debug", skip(socket), fields(kitty_id = id))]
pub async fn reset_border_color_on_socket(socket: &str, id: u64) -> Result<()> {
    // Read the active border color from the palette theme
    // Fallback to a sensible default if file isn't readable
    let palette_path = dirs::config_dir()
        .map(|p| p.join("kitty/palette-theme.conf"))
        .unwrap_or_default();

    let (active, inactive) = if let Ok(content) = std::fs::read_to_string(&palette_path) {
        let active = content
            .lines()
            .find(|l| l.starts_with("active_border_color"))
            .and_then(|l| l.split_whitespace().nth(1))
            .unwrap_or("#9ccfd8")
            .to_string();
        let inactive = content
            .lines()
            .find(|l| l.starts_with("inactive_border_color"))
            .and_then(|l| l.split_whitespace().nth(1))
            .unwrap_or("#494748")
            .to_string();
        (active, inactive)
    } else {
        ("#9ccfd8".to_string(), "#494748".to_string())
    };

    set_border_color_on_socket(socket, id, &active, &inactive).await
}

// =============================================================================
// Public API: Pane Listing
// =============================================================================

/// List panes from the default kitty instance
///
/// Enumerate vessels in the current tower.
#[instrument(level = "debug")]
pub async fn list_panes() -> Result<Vec<KittyPane>> {
    list_panes_on_socket(&default_socket()).await
}

/// List panes from ALL kitty instances
///
/// Enumerate vessels across all towers. Queries every socket in XDG_RUNTIME_DIR.
/// Unresponsive towers are skipped.
#[instrument(level = "debug")]
pub async fn list_all_panes() -> Result<Vec<KittyPane>> {
    let sockets = find_all_sockets();
    let mut all_panes = Vec::new();

    for socket in &sockets {
        match list_panes_on_socket(socket).await {
            Ok(panes) => all_panes.extend(panes),
            Err(e) => {
                trace_error!("kitty query failed", socket = %socket, error = %e);
            }
        }
    }

    Ok(all_panes)
}

/// List panes from a specific socket
pub async fn list_panes_from_socket(socket: &str) -> Result<Vec<KittyPane>> {
    list_panes_on_socket(socket).await
}

/// Get a specific pane by ID from the default instance
pub async fn get_pane(id: u64) -> Result<Option<KittyPane>> {
    let panes = list_panes().await?;
    Ok(panes.into_iter().find(|p| p.id == id))
}

/// Get a specific pane by ID, searching all instances
///
/// Note: IDs are only unique within an instance. If multiple instances have
/// the same ID, returns the first match (current instance preferred).
pub async fn get_pane_all(id: u64) -> Result<Option<KittyPane>> {
    // Check default instance first
    if let Some(pane) = get_pane(id).await? {
        return Ok(Some(pane));
    }

    // Search other instances
    let panes = list_all_panes().await?;
    Ok(panes.into_iter().find(|p| p.id == id))
}

/// Get all panes in a kitty OS window by its X11/Wayland platform window ID
///
/// When the user clicks a kitty window via slop, this maps the X11 window ID
/// to the vessels within that tower window. A single OS window can contain
/// multiple vessels (splits) across multiple tabs.
///
/// Returns empty Vec if the platform_window_id doesn't match any tower.
pub async fn get_panes_by_platform_id(platform_window_id: u64) -> Result<Vec<KittyPane>> {
    let panes = list_all_panes().await?;
    Ok(panes
        .into_iter()
        .filter(|p| p.platform_window_id == platform_window_id)
        .collect())
}

// =============================================================================
// Public API: Instance Discovery
// =============================================================================

/// Discover all kitty instances on the system
///
/// Survey all towers standing in the realm. Returns information about each
/// instance including whether it's responsive.
#[instrument(level = "debug")]
pub async fn discover_all_instances() -> Vec<KittyInstance> {
    let current_socket = default_socket();
    let all_sockets = find_all_sockets();

    let mut instances = Vec::new();

    for socket in all_sockets {
        let pid = socket_pid(&socket);
        let is_current = socket == current_socket;

        match list_panes_on_socket(&socket).await {
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
    instances.sort_by(|a, b| b.is_current.cmp(&a.is_current).then(a.pid.cmp(&b.pid)));

    let responsive = instances.iter().filter(|i| i.is_responsive).count();
    effect!(
        "kitty",
        "instances discovered",
        total = instances.len(),
        responsive = responsive
    );
    instances
}

// =============================================================================
// Public API: Convenience (default socket, by pane ID)
// =============================================================================

/// Focus a pane by ID (default instance)
///
/// Bring a vessel into awareness.
#[instrument(level = "debug")]
pub async fn focus_pane(id: u64) -> Result<()> {
    focus_pane_on_socket(&default_socket(), id).await
}

/// Send text to a pane by ID (default instance)
///
/// Speak into a vessel.
#[instrument(level = "debug", skip(text))]
pub async fn send_text(id: u64, text: &str) -> Result<()> {
    send_text_on_socket(&default_socket(), id, text).await
}

/// Set a user variable on a pane by ID (default instance)
pub async fn set_user_var(id: u64, key: &str, value: &str) -> Result<()> {
    set_user_var_on_socket(&default_socket(), id, key, value).await
}

/// Set title on a pane by ID (default instance)
pub async fn set_pane_title(id: u64, title: &str) -> Result<()> {
    set_title_on_socket(&default_socket(), id, title).await
}

/// Get full scrollback from a pane by ID (default instance)
#[instrument(level = "debug")]
pub async fn get_scrollback(id: u64) -> Result<String> {
    get_scrollback_on_socket(&default_socket(), id).await
}

/// Get recent scrollback from a pane by ID (default instance)
pub async fn get_recent_scrollback(id: u64, lines: usize) -> Result<String> {
    get_recent_scrollback_on_socket(&default_socket(), id, lines).await
}

/// Close a pane by ID (default instance)
pub async fn close_pane(id: u64) -> Result<()> {
    close_pane_on_socket(&default_socket(), id).await
}

/// Get the cwd of the focused pane (default instance)
pub async fn get_focused_cwd() -> Result<Option<PathBuf>> {
    let panes = list_panes().await?;
    Ok(panes.into_iter().find(|p| p.is_focused).map(|p| p.cwd))
}

// Legacy aliases for older callers and kitty's upstream "window" terminology.
#[doc(hidden)]
pub async fn focus_window(id: u64) -> Result<()> {
    focus_pane(id).await
}
#[doc(hidden)]
pub async fn set_window_title(id: u64, title: &str) -> Result<()> {
    set_pane_title(id, title).await
}
#[doc(hidden)]
pub async fn close_window(id: u64) -> Result<()> {
    close_pane(id).await
}
#[doc(hidden)]
pub async fn list_windows() -> Result<Vec<KittyPane>> {
    list_panes().await
}
#[doc(hidden)]
pub async fn get_window(id: u64) -> Result<Option<KittyPane>> {
    get_pane(id).await
}
#[doc(hidden)]
pub async fn list_windows_from_socket(socket: &str) -> Result<Vec<KittyPane>> {
    list_panes_from_socket(socket).await
}

// =============================================================================
// Public API: Multi-Socket Operations (search all sockets)
// =============================================================================

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
            Some(format!(
                "Warning: operating on non-current socket: {}",
                self.addr.short()
            ))
        } else {
            None
        }
    }
}

/// Focus a pane by ID, searching all sockets
///
/// Seek the vessel across all towers and bring it into awareness.
/// Returns the pane address and whether it was found in a distant tower.
#[instrument(level = "debug")]
pub async fn focus_pane_any(id: u64) -> Result<MultiSocketResult<()>> {
    let current = default_socket();
    match get_pane_all(id).await? {
        Some(pane) => {
            let addr = pane.addr();
            let is_non_current = pane.socket != current;
            pane.focus().await?;
            Ok(MultiSocketResult {
                result: (),
                addr,
                is_non_current,
            })
        }
        None => bail!("Pane {} not found in any kitty instance", id),
    }
}

/// Send text to a pane by ID, searching all sockets
#[instrument(level = "debug", skip(text))]
pub async fn send_text_any(id: u64, text: &str) -> Result<MultiSocketResult<()>> {
    let current = default_socket();
    match get_pane_all(id).await? {
        Some(pane) => {
            let addr = pane.addr();
            let is_non_current = pane.socket != current;
            pane.send_text(text).await?;
            Ok(MultiSocketResult {
                result: (),
                addr,
                is_non_current,
            })
        }
        None => bail!("Pane {} not found in any kitty instance", id),
    }
}

/// Get scrollback from a pane by ID, searching all sockets
#[instrument(level = "debug")]
pub async fn get_scrollback_any(id: u64) -> Result<MultiSocketResult<String>> {
    let current = default_socket();
    match get_pane_all(id).await? {
        Some(pane) => {
            let addr = pane.addr();
            let is_non_current = pane.socket != current;
            let text = pane.scrollback().await?;
            Ok(MultiSocketResult {
                result: text,
                addr,
                is_non_current,
            })
        }
        None => bail!("Pane {} not found in any kitty instance", id),
    }
}

/// Set title on a pane by ID, searching all sockets
pub async fn set_title_any(id: u64, title: &str) -> Result<MultiSocketResult<()>> {
    let current = default_socket();
    match get_pane_all(id).await? {
        Some(pane) => {
            let addr = pane.addr();
            let is_non_current = pane.socket != current;
            pane.set_title(title).await?;
            Ok(MultiSocketResult {
                result: (),
                addr,
                is_non_current,
            })
        }
        None => bail!("Pane {} not found in any kitty instance", id),
    }
}

/// Set user var on a pane by ID, searching all sockets
pub async fn set_user_var_any(id: u64, key: &str, value: &str) -> Result<MultiSocketResult<()>> {
    let current = default_socket();
    match get_pane_all(id).await? {
        Some(pane) => {
            let addr = pane.addr();
            let is_non_current = pane.socket != current;
            pane.set_user_var(key, value).await?;
            Ok(MultiSocketResult {
                result: (),
                addr,
                is_non_current,
            })
        }
        None => bail!("Pane {} not found in any kitty instance", id),
    }
}

/// Close a pane by ID, searching all sockets
pub async fn close_pane_any(id: u64) -> Result<MultiSocketResult<()>> {
    let current = default_socket();
    match get_pane_all(id).await? {
        Some(pane) => {
            let addr = pane.addr();
            let is_non_current = pane.socket != current;
            pane.close().await?;
            Ok(MultiSocketResult {
                result: (),
                addr,
                is_non_current,
            })
        }
        None => bail!("Pane {} not found in any kitty instance", id),
    }
}

#[doc(hidden)]
pub async fn focus_window_any(id: u64) -> Result<MultiSocketResult<()>> {
    focus_pane_any(id).await
}

#[doc(hidden)]
pub async fn close_window_any(id: u64) -> Result<MultiSocketResult<()>> {
    close_pane_any(id).await
}

// =============================================================================
// Raw JSON Structures (for parsing kitten @ ls output)
// =============================================================================

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
    /// kitty emits `null` for processes whose cwd cannot be read -- kernel
    /// threads, zombies, or short-lived processes that exited between
    /// enumeration and stat. Non-Option<PathBuf> rejects null at deserialize
    /// time and breaks the entire `kitten @ ls` parse, which is what produced
    /// the long-running "Failed to parse" warns and left babel blind to
    /// every window in the affected kitty instance.
    cwd: Option<PathBuf>,
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    #[ignore]
    async fn test_list_panes() {
        let panes = list_panes().await.unwrap();
        assert!(!panes.is_empty(), "Should find at least one pane");

        for pane in &panes {
            println!(
                "Pane {} [{}]: {} (cwd: {})",
                pane.id,
                pane.socket,
                pane.title,
                pane.cwd.display()
            );
        }
    }

    #[tokio::test]
    #[ignore]
    async fn test_list_all_panes() {
        let panes = list_all_panes().await.unwrap();
        println!("Found {} panes across all instances", panes.len());

        for pane in &panes {
            println!("  {} @ {}: {}", pane.id, pane.socket, pane.title);
        }
    }

    #[tokio::test]
    #[ignore]
    async fn test_discover_instances() {
        let instances = discover_all_instances().await;
        println!("Found {} kitty instances", instances.len());

        for inst in &instances {
            println!(
                "  {} (pid: {:?}, current: {}, responsive: {})",
                inst.socket, inst.pid, inst.is_current, inst.is_responsive
            );
            println!("    {} panes", inst.panes.len());
        }
    }
}
