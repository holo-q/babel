//! Zellij terminal backend
//!
//! Implements [`TerminalBackend`] for zellij via CLI commands.
//!
//! ## Connection Model
//!
//! Zellij sessions are identified by name. Connection strings use the `zellij:`
//! prefix: `zellij:my-session`. The `$ZELLIJ_SESSION_NAME` env var carries the
//! session name when running inside zellij.
//!
//! ## Pane Identity
//!
//! Zellij pane IDs come in forms like `terminal_7`, `plugin_3`, or bare `7`.
//! We strip the `terminal_` prefix and store the numeric part as u64 in `Pane::id`.
//! Plugin panes are skipped (not terminal panes).
//!
//! ## Text Injection
//!
//! Uses `zellij action write-chars` for text body, then `zellij action send-keys Enter`
//! separately when the text ends with CR/LF.
//!
//! ## Foreground Process Discovery
//!
//! Zellij JSON may include `pid` for terminal panes. We walk `/proc/<pid>/children`
//! with depth limit 3 and breadth limit 32 to find the actual agent process —
//! same strategy as the tmux backend.

use anyhow::{bail, Context, Result};
use async_trait::async_trait;
use std::collections::HashMap;
use std::path::PathBuf;
use std::time::Duration;
use tokio::time::timeout;

use super::{BackendInstance, ForegroundProcess, Pane, PaneExtras, TerminalBackend};
#[allow(unused_imports)]
use vtr::{boundary, effect, trace_error};

const ZELLIJ_TIMEOUT_SHORT: Duration = Duration::from_secs(5);
const ZELLIJ_TIMEOUT_LONG: Duration = Duration::from_secs(10);

const CONNECTION_PREFIX: &str = "zellij:";

// Max depth when walking /proc for child processes
const PROC_WALK_MAX_DEPTH: usize = 3;
// Max total children to collect across all depths
const PROC_WALK_MAX_BREADTH: usize = 32;

/// Zellij terminal backend.
pub struct ZellijBackend;

// =============================================================================
// Subprocess Helpers
// =============================================================================

/// Run a `zellij action <subcommand>` command with timeout.
///
/// Zellij actions operate within the current session context (via
/// `$ZELLIJ_SESSION_NAME` or by specifying a session). We pass the session
/// name explicitly so the daemon can address any discovered session.
async fn run_zellij(
    session: &str,
    args: &[&str],
    duration: Duration,
) -> Result<std::process::Output> {
    let op = args.first().unwrap_or(&"unknown");
    boundary!("zellij", op, session = session);

    let output = timeout(duration, async {
        tokio::process::Command::new("zellij")
            .args(["--session", session, "action"])
            .args(args)
            .output()
            .await
            .context("zellij subprocess failed")
    })
    .await
    .context("zellij subprocess timed out")??;

    Ok(output)
}

// =============================================================================
// Connection String Helpers
// =============================================================================

/// Extract the session name from a connection string.
/// `"zellij:my-session"` -> `"my-session"`
fn session_from_conn(conn: &str) -> &str {
    conn.strip_prefix(CONNECTION_PREFIX).unwrap_or(conn)
}

/// Build a connection string from a session name.
fn conn_from_session(session: &str) -> String {
    format!("{CONNECTION_PREFIX}{session}")
}

/// Format a zellij pane target from a numeric id.
/// Zellij accepts bare numeric IDs as shorthand.
fn pane_target(id: u64) -> String {
    id.to_string()
}

// =============================================================================
// Discovery Helpers
// =============================================================================

/// Parse zellij pane ID string to numeric u64.
///
/// Zellij pane IDs come as `"terminal_7"`, `"plugin_3"`, or bare `"7"`.
/// We only care about terminal panes. Returns None for plugin panes.
fn parse_pane_id(raw: &str) -> Option<u64> {
    if raw.starts_with("plugin_") {
        return None;
    }
    let numeric = raw.strip_prefix("terminal_").unwrap_or(raw);
    numeric.parse().ok()
}

/// Parse the `$ZELLIJ_SESSION_NAME` environment variable.
fn parse_zellij_env() -> Option<String> {
    std::env::var("ZELLIJ_SESSION_NAME").ok()
}

/// Find all active zellij sessions.
///
/// Runs `zellij list-sessions --short` which outputs one session name per line.
pub fn find_all_sessions() -> Vec<String> {
    let output = std::process::Command::new("zellij")
        .args(["list-sessions", "--short"])
        .output()
        .ok();
    match output {
        Some(o) if o.status.success() => String::from_utf8_lossy(&o.stdout)
            .lines()
            .filter(|l| !l.is_empty())
            .map(|l| l.to_string())
            .collect(),
        _ => Vec::new(),
    }
}

// =============================================================================
// JSON Deserialization — zellij list-panes output
// =============================================================================

/// Serde struct for a single pane from `zellij action dump-layout` or similar.
///
/// The exact JSON schema varies across zellij versions. We use `#[serde(default)]`
/// liberally to be resilient against missing fields, since we can't test against
/// a live session during development.
#[derive(Debug, serde::Deserialize)]
struct ZellijPaneInfo {
    /// Pane ID — may be numeric or prefixed (e.g. "terminal_7")
    #[serde(default, alias = "id")]
    pane_id: Option<serde_json::Value>,

    /// Whether this pane is currently focused
    #[serde(default)]
    is_focused: bool,

    /// Shell PID running inside the pane
    #[serde(default)]
    pid: Option<u32>,

    /// Pane title (user-set or shell-reported)
    #[serde(default)]
    title: Option<String>,

    /// Current working directory
    #[serde(default)]
    cwd: Option<String>,

    /// Whether this pane is a plugin pane (we skip these)
    #[serde(default)]
    is_plugin: bool,

    /// Tab name this pane belongs to
    #[serde(default)]
    tab_name: Option<String>,

    /// Whether this is the active pane in its tab
    #[serde(default)]
    is_active: bool,
}

impl ZellijPaneInfo {
    /// Extract numeric pane ID, skipping plugin panes.
    fn numeric_id(&self) -> Option<u64> {
        if self.is_plugin {
            return None;
        }
        match &self.pane_id {
            Some(serde_json::Value::Number(n)) => n.as_u64(),
            Some(serde_json::Value::String(s)) => parse_pane_id(s),
            _ => None,
        }
    }
}

/// Top-level structure for `zellij action list-panes --json` output.
/// Zellij may return an array of tabs, each containing panes, or a flat
/// array of panes. We try both shapes.
#[derive(Debug, serde::Deserialize)]
#[serde(untagged)]
enum ZellijListPanesOutput {
    /// Flat array of pane info objects
    Flat(Vec<ZellijPaneInfo>),
    /// Array of tab objects each containing panes
    Tabs(Vec<ZellijTabInfo>),
}

#[derive(Debug, serde::Deserialize)]
struct ZellijTabInfo {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    panes: Vec<ZellijPaneInfo>,
}

/// Parse `zellij action list-panes` JSON output into Pane structs.
fn parse_list_panes_json(json_str: &str, conn: &str, session_name: &str) -> Vec<Pane> {
    let parsed: Result<ZellijListPanesOutput, _> = serde_json::from_str(json_str);

    let pane_infos: Vec<(ZellijPaneInfo, Option<String>)> = match parsed {
        Ok(ZellijListPanesOutput::Flat(panes)) => panes.into_iter().map(|p| (p, None)).collect(),
        Ok(ZellijListPanesOutput::Tabs(tabs)) => tabs
            .into_iter()
            .flat_map(|tab| {
                let tab_name = tab.name.clone();
                tab.panes.into_iter().map(move |p| (p, tab_name.clone()))
            })
            .collect(),
        Err(e) => {
            tracing::warn!(error = %e, "Failed to parse zellij list-panes JSON");
            return Vec::new();
        }
    };

    let mut panes = Vec::new();

    for (info, tab_name_from_parent) in pane_infos {
        let Some(id) = info.numeric_id() else {
            continue;
        };

        let shell_pid = info.pid.unwrap_or(0);
        let foreground_processes = discover_foreground_processes(shell_pid);

        // CWD: prefer JSON field, fall back to /proc/<pid>/cwd
        let cwd = info
            .cwd
            .map(PathBuf::from)
            .or_else(|| {
                if shell_pid > 0 {
                    std::fs::read_link(format!("/proc/{shell_pid}/cwd")).ok()
                } else {
                    None
                }
            })
            .unwrap_or_default();

        let tab_name = info.tab_name.or(tab_name_from_parent);

        panes.push(Pane {
            connection: conn.to_string(),
            id,
            title: info.title.unwrap_or_default(),
            cwd,
            is_focused: info.is_focused,
            is_active: info.is_active || info.is_focused,
            foreground_processes,
            user_vars: HashMap::new(),
            platform_window_id: None,
            extras: PaneExtras::Zellij {
                session: session_name.to_string(),
                tab_name,
            },
        });
    }

    panes
}

// =============================================================================
// Foreground Process Discovery
// =============================================================================

/// Walk /proc to find foreground processes from a shell PID.
///
/// Zellij provides the shell PID for terminal panes. We walk its children
/// to find the actual agent process (claude, codex, etc.).
///
/// Depth limit: 3 levels (shell -> agent -> agent child)
/// Breadth limit: 32 total processes collected
fn discover_foreground_processes(shell_pid: u32) -> Vec<ForegroundProcess> {
    if shell_pid == 0 {
        return Vec::new();
    }

    let mut result = Vec::new();
    let mut queue: Vec<(u32, usize)> = vec![(shell_pid, 0)];

    while let Some((pid, depth)) = queue.pop() {
        if result.len() >= PROC_WALK_MAX_BREADTH {
            break;
        }
        if depth > PROC_WALK_MAX_DEPTH {
            continue;
        }

        if let Some(proc_info) = read_proc_info(pid) {
            result.push(proc_info);
        }

        if depth < PROC_WALK_MAX_DEPTH {
            if let Ok(children_str) =
                std::fs::read_to_string(format!("/proc/{pid}/task/{pid}/children"))
            {
                for child_pid_str in children_str.split_whitespace() {
                    if let Ok(child_pid) = child_pid_str.parse::<u32>() {
                        queue.push((child_pid, depth + 1));
                    }
                }
            }
        }
    }

    result
}

/// Read /proc/<pid>/cmdline and /proc/<pid>/cwd for a single process.
fn read_proc_info(pid: u32) -> Option<ForegroundProcess> {
    let cmdline_raw = std::fs::read(format!("/proc/{pid}/cmdline")).ok()?;
    if cmdline_raw.is_empty() {
        return None;
    }

    let cmdline: Vec<String> = cmdline_raw
        .split(|&b| b == 0)
        .filter(|s| !s.is_empty())
        .map(|s| String::from_utf8_lossy(s).to_string())
        .collect();

    if cmdline.is_empty() {
        return None;
    }

    let cwd = std::fs::read_link(format!("/proc/{pid}/cwd")).unwrap_or_default();

    Some(ForegroundProcess { pid, cmdline, cwd })
}

// =============================================================================
// ZellijBackend — TerminalBackend implementation
// =============================================================================

#[async_trait]
impl TerminalBackend for ZellijBackend {
    fn backend_name(&self) -> &'static str {
        "zellij"
    }

    fn default_connection(&self) -> Option<String> {
        // Priority 1: $ZELLIJ_SESSION_NAME env var
        if let Some(session_name) = parse_zellij_env() {
            return Some(conn_from_session(&session_name));
        }

        // Priority 2: scan for active sessions
        let sessions = find_all_sessions();
        sessions.first().map(|s| conn_from_session(s))
    }

    fn find_all_connections(&self) -> Vec<String> {
        find_all_sessions()
            .into_iter()
            .map(|s| conn_from_session(&s))
            .collect()
    }

    async fn list_panes(&self, conn: &str) -> Result<Vec<Pane>> {
        let session = session_from_conn(conn);

        // Try JSON output first for structured data
        let output = run_zellij(session, &["list-panes"], ZELLIJ_TIMEOUT_SHORT).await?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            bail!("zellij list-panes failed: {stderr}");
        }

        let stdout = String::from_utf8_lossy(&output.stdout);

        // Try parsing as JSON; if that fails, the output format isn't structured
        // enough for us to extract pane info.
        let panes = parse_list_panes_json(&stdout, conn, session);
        if panes.is_empty() && !stdout.trim().is_empty() {
            tracing::debug!(
                output = %stdout,
                "zellij list-panes returned non-empty output but no panes parsed"
            );
        }

        Ok(panes)
    }

    async fn discover_instances(&self) -> Vec<BackendInstance> {
        let sessions = find_all_sessions();
        let current_session = parse_zellij_env();

        let mut instances = Vec::new();

        for session_name in sessions {
            let conn = conn_from_session(&session_name);
            let is_current = current_session.as_deref() == Some(&session_name);

            match self.list_panes(&conn).await {
                Ok(panes) => {
                    instances.push(BackendInstance {
                        connection: conn,
                        pid: None, // Zellij doesn't expose server PID via list-sessions
                        is_current,
                        is_responsive: true,
                        panes,
                        error: None,
                    });
                }
                Err(e) => {
                    instances.push(BackendInstance {
                        connection: conn,
                        pid: None,
                        is_current,
                        is_responsive: false,
                        panes: Vec::new(),
                        error: Some(e.to_string()),
                    });
                }
            }
        }

        instances
    }

    async fn focus_pane(&self, conn: &str, id: u64) -> Result<()> {
        let session = session_from_conn(conn);
        let target = pane_target(id);

        let output = run_zellij(session, &["focus-pane-id", &target], ZELLIJ_TIMEOUT_SHORT).await?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            bail!("zellij focus-pane-id failed: {stderr}");
        }

        effect!("zellij", "pane focused", id = id);
        Ok(())
    }

    async fn send_text(&self, conn: &str, id: u64, text: &str) -> Result<()> {
        let session = session_from_conn(conn);
        let target = pane_target(id);

        // Strip trailing CR/LF — we send Enter separately
        let has_cr = text.ends_with('\r') || text.ends_with('\n');
        let body = text.trim_end_matches(['\r', '\n']);

        if !body.is_empty() {
            // write-chars sends literal text to the specified pane
            let output = run_zellij(
                session,
                &["write-chars", "--pane-id", &target, body],
                ZELLIJ_TIMEOUT_SHORT,
            )
            .await?;

            if !output.status.success() {
                let stderr = String::from_utf8_lossy(&output.stderr);
                bail!("zellij write-chars failed: {stderr}");
            }
        }

        if has_cr {
            // Send Enter key separately
            let _ = run_zellij(
                session,
                &["write", "--pane-id", &target, "10"],
                ZELLIJ_TIMEOUT_SHORT,
            )
            .await;
        }

        effect!("zellij", "text sent", id = id);
        Ok(())
    }

    async fn get_scrollback(&self, conn: &str, id: u64) -> Result<String> {
        let session = session_from_conn(conn);
        let target = pane_target(id);

        // dump-screen outputs the full scrollback buffer to stdout
        let output = run_zellij(
            session,
            &["dump-screen", "--pane-id", &target, "--full"],
            ZELLIJ_TIMEOUT_LONG,
        )
        .await?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            bail!("zellij dump-screen failed: {stderr}");
        }

        Ok(String::from_utf8_lossy(&output.stdout).to_string())
    }

    async fn get_recent_scrollback(&self, conn: &str, id: u64, lines: usize) -> Result<String> {
        // Zellij doesn't have a native "last N lines" dump-screen mode,
        // so we get the full scrollback and take the tail.
        let full = self.get_scrollback(conn, id).await?;
        let recent: Vec<&str> = full.lines().rev().take(lines).collect();
        Ok(recent.into_iter().rev().collect::<Vec<_>>().join("\n"))
    }

    async fn close_pane(&self, conn: &str, id: u64) -> Result<()> {
        let session = session_from_conn(conn);
        let target = pane_target(id);

        let output = run_zellij(
            session,
            &["close-pane", "--pane-id", &target],
            ZELLIJ_TIMEOUT_SHORT,
        )
        .await?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            bail!("zellij close-pane failed: {stderr}");
        }

        effect!("zellij", "pane closed", id = id);
        Ok(())
    }

    async fn set_meta(&self, _conn: &str, _id: u64, _key: &str, _val: &str) -> Result<()> {
        // Zellij has no user vars / pane options mechanism.
        // No-op — metadata is tracked daemon-side via sqlite.
        Ok(())
    }

    async fn set_title(&self, conn: &str, id: u64, title: &str) -> Result<()> {
        let session = session_from_conn(conn);
        let target = pane_target(id);

        let output = run_zellij(
            session,
            &["rename-pane", "--pane-id", &target, title],
            ZELLIJ_TIMEOUT_SHORT,
        )
        .await?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            bail!("zellij rename-pane failed: {stderr}");
        }

        effect!("zellij", "title set", id = id);
        Ok(())
    }

    // Zellij panes share the host terminal's desktop window
    fn has_desktop_windows(&self) -> bool {
        false
    }

    fn has_border_coloring(&self) -> bool {
        false
    }

    fn has_raw_layout(&self) -> bool {
        false
    }
}
