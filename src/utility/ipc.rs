//! IPC Protocol - Unix socket communication between CLI and daemon
//!
//! Protocol: newline-delimited JSON over unix socket
//! Socket location: $XDG_RUNTIME_DIR/babel.sock (or /tmp/babel.sock fallback)
//!
//! Request/Response pattern - client sends Request, daemon sends Response.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};

use crate::utility::claude_storage::SessionInfo;
use crate::utility::claude_discovery::ClaudeWindow;
use crate::daemon::{TerminalInfo, SocketStatus};
use crate::kitty::KittyPane;
use crate::events::EventMessage;
use crate::wset::WSetSummary;

// ═══════════════════════════════════════════════════════════════════════════════
// Protocol Messages
// ═══════════════════════════════════════════════════════════════════════════════

/// Request from CLI to daemon
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "cmd", rename_all = "snake_case")]
pub enum Request {
    /// List all Claude windows (fast - from cache)
    List,

    /// List all kitty terminals (not just Claude)
    /// Useful for seeing the full terminal flow and watching transitions
    ListTerminals,

    /// List raw kitty panes from all sockets
    /// Unlike ListTerminals, this returns raw KittyPane data without enrichment
    ListPanes,

    /// List kitty socket status (multi-instance awareness)
    ListSockets,

    /// List all Claude windows with fingerprint data (slow - extracts scrollback)
    ListWithFingerprints,

    /// Get status of specific window (or focused if None)
    Status { window_id: Option<u64> },

    /// Get full session info for a window (triggers enrichment if needed)
    Enrich { window_id: u64 },

    /// Focus a window
    Focus { window_id: u64 },

    /// Get scrollback from window
    Scroll { window_id: u64 },

    /// Send text to window (with Enter/CR at end)
    Send { window_id: u64, text: String },

    /// Type text to window (without Enter/CR at end)
    /// Useful for composing prompts incrementally
    Type { window_id: u64, text: String },

    /// Check if a window has pending (unsent) input in the textbox
    /// Returns true if there's text typed but not yet submitted
    HasPendingInput { window_id: u64 },

    /// Tag window with icon
    Tag { window_id: u64, icon: String },

    /// Mark window as read
    MarkRead { window_id: u64 },

    /// Get recent history from ~/.claude
    History { limit: usize },

    /// Ping - check if daemon is alive
    Ping,

    /// Shutdown daemon
    Shutdown,

    /// Force refresh - re-scan kitty windows
    Refresh,

    /// Subscribe to events (connection stays open for streaming)
    Subscribe {
        /// Event types to receive (empty = all events)
        events: Vec<String>,
    },

    /// Get current workspace titles (from authoritative cache)
    Titles,

    /// Force refresh titles for workspace(s)
    /// If workspace is None, refreshes all workspaces with Claude windows
    TitleRefresh { workspace: Option<i32> },

    // ─── WSet Operations ────────────────────────────────────────────────────────

    /// Save current state to a WSet
    /// If name is None, saves to current WSet (from _current file)
    WSetSave { name: Option<String> },

    /// Load a WSet, spawning windows for each session
    /// If name is None, loads current WSet (from _current file)
    /// dry_run=true returns what would happen without executing
    WSetLoad { name: Option<String>, dry_run: bool },

    /// List all saved WSet files
    WSetList,

    /// Get current WSet name (from _current file)
    WSetCurrent,

    /// Delete a WSet by name
    WSetDelete { name: String },

    /// Rename a WSet
    WSetRename { old: String, new: String },

    /// Set description for a WSet
    WSetDescribe { name: String, description: Option<String> },
}

/// Response from daemon to CLI
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum Response {
    /// Success with window list
    Windows { windows: Vec<ClaudeWindow> },

    /// Success with all terminal list (not just Claude)
    Terminals { terminals: Vec<TerminalInfo> },

    /// Success with raw kitty panes from all sockets
    Panes { panes: Vec<KittyPane> },

    /// Success with socket status information
    /// Keys are socket paths (e.g., "unix:/run/user/1000/kitty.sock-12345")
    Sockets { sockets: std::collections::HashMap<String, SocketStatus> },

    /// Success with single window
    /// Boxed to reduce enum size (ClaudeWindow is 432 bytes)
    Window { window: Box<Option<ClaudeWindow>> },

    /// Success with session history
    History { sessions: Vec<SessionInfo> },

    /// Success with scrollback text
    Scrollback { text: String },

    /// Simple success acknowledgment
    Ok { message: String },

    /// Pending input status for a window
    /// TODO: As scrollparse improves, this will include the actual pending text
    /// and support for save/restore operations during broadcast
    PendingInput {
        window_id: u64,
        has_pending: bool,
        /// The pending text if detected (may be empty even if has_pending is true
        /// due to detection limitations)
        pending_text: Option<String>,
    },

    /// Error response
    Error { message: String },

    /// Pong response to ping
    Pong { uptime_secs: u64 },

    /// Subscription acknowledged
    Subscribed { subscriber_id: u64 },

    /// Event notification (sent to subscribers)
    Event { event: EventMessage },

    /// Workspace titles response
    /// Keys are workspace numbers as strings (JSON doesn't support integer map keys)
    Titles { titles: std::collections::HashMap<String, String> },

    // ─── WSet Responses ─────────────────────────────────────────────────────────

    /// WSet saved successfully (full data)
    WSet { wset: crate::wset::WSet },

    /// WSet saved summary (legacy)
    WSetSaved {
        name: String,
        wspaces: usize,
        windows: usize,
    },

    /// WSet loaded successfully (or dry-run preview)
    WSetLoaded {
        name: String,
        wspaces: usize,
        windows: usize,
        /// Sessions that couldn't be restored (file missing, etc.)
        skipped: Vec<String>,
        /// True if this was a dry-run (no windows spawned)
        dry_run: bool,
    },

    /// List of all WSet summaries
    WSetList {
        wsets: Vec<WSetSummary>,
        current: Option<String>,
    },

    /// Current WSet name
    WSetCurrent {
        name: Option<String>,
    },
}

// ═══════════════════════════════════════════════════════════════════════════════
// Socket Path
// ═══════════════════════════════════════════════════════════════════════════════

/// Get the daemon socket path
pub fn socket_path() -> PathBuf {
    if let Ok(runtime_dir) = std::env::var("XDG_RUNTIME_DIR") {
        PathBuf::from(runtime_dir).join("babel.sock")
    } else {
        PathBuf::from("/tmp").join(format!("babel-{}.sock", users::get_current_uid()))
    }
}

// ═══════════════════════════════════════════════════════════════════════════════
// Client
// ═══════════════════════════════════════════════════════════════════════════════

/// Send a request to the daemon and get a response
#[tracing::instrument(level = "debug", skip(request), fields(cmd = ?request))]
pub async fn send_request(request: &Request) -> Result<Response> {
    let sock_path = socket_path();

    let mut stream = UnixStream::connect(&sock_path)
        .await
        .with_context(|| format!("Failed to connect to daemon at {}", sock_path.display()))?;

    // Send request as JSON line
    let mut request_json = serde_json::to_string(request)?;
    request_json.push('\n');
    stream.write_all(request_json.as_bytes()).await?;

    // Read response
    let mut reader = BufReader::new(stream);
    let mut response_line = String::new();
    reader.read_line(&mut response_line).await?;

    let response: Response = serde_json::from_str(&response_line)
        .context("Failed to parse daemon response")?;

    Ok(response)
}

/// Check if daemon is running
///
/// Fast check: just verifies socket exists and is connectable.
/// Doesn't send a message - saves one IPC round-trip (~5ms).
pub async fn is_daemon_running() -> bool {
    let sock_path = socket_path();
    // Quick check: can we connect to the socket?
    // This is faster than sending a ping and waiting for response
    UnixStream::connect(&sock_path).await.is_ok()
}

/// Synchronous wrapper for send_request (for non-async CLI)
pub fn send_request_sync(request: &Request) -> Result<Response> {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?
        .block_on(send_request(request))
}

// ═══════════════════════════════════════════════════════════════════════════════
// Server
// ═══════════════════════════════════════════════════════════════════════════════

/// Create and bind the daemon socket
pub async fn create_listener() -> Result<UnixListener> {
    let sock_path = socket_path();

    // Remove existing socket if present
    if sock_path.exists() {
        std::fs::remove_file(&sock_path)
            .with_context(|| format!("Failed to remove old socket at {}", sock_path.display()))?;
    }

    let listener = UnixListener::bind(&sock_path)
        .with_context(|| format!("Failed to bind socket at {}", sock_path.display()))?;

    // Make socket accessible
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&sock_path, std::fs::Permissions::from_mode(0o700))?;
    }

    Ok(listener)
}

/// Read a request from a client connection
pub async fn read_request(stream: &mut BufReader<UnixStream>) -> Result<Option<Request>> {
    let mut line = String::new();
    let bytes_read = stream.read_line(&mut line).await?;

    if bytes_read == 0 {
        return Ok(None); // Connection closed
    }

    let request: Request = serde_json::from_str(&line)
        .context("Failed to parse client request")?;

    Ok(Some(request))
}

/// Send a response to a client
pub async fn send_response(stream: &mut UnixStream, response: &Response) -> Result<()> {
    let mut response_json = serde_json::to_string(response)?;
    response_json.push('\n');
    stream.write_all(response_json.as_bytes()).await?;
    Ok(())
}
