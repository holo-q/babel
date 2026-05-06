//! Tmux terminal backend
//!
//! Implements [`TerminalBackend`] for tmux via CLI commands.
//!
//! ## Connection Model
//!
//! Tmux servers listen on sockets at `/tmp/tmux-$UID/<name>` (default: `default`).
//! Connection strings use the `tmux:` prefix: `tmux:/tmp/tmux-1000/default`.
//! The `$TMUX` env var carries `<socket>,<pid>,<session>` when inside tmux.
//!
//! ## Pane Identity
//!
//! Tmux pane IDs are globally unique per server (`%0`, `%1`, ...).
//! We strip the `%` and store as u64 in `Pane::id`.
//!
//! ## Text Injection Safety
//!
//! Uses `send-keys -l` (literal mode) for text, then `send-keys Enter`
//! separately. Bare `send-keys` interprets key names (e.g., "Enter" as a
//! literal key press) which corrupts arbitrary text.
//!
//! ## Foreground Process Discovery
//!
//! Tmux provides `pane_pid` — the shell PID. We walk `/proc/<pid>/children`
//! with depth limit 3 and breadth limit 32 to find the actual agent process.
//! Uses the same `ForegroundProcess` struct and `AgentKind::from_cmdline`
//! vocabulary as the kitty backend.

use anyhow::{bail, Context, Result};
use async_trait::async_trait;
use std::collections::HashMap;
use std::path::PathBuf;
use std::time::Duration;
use tokio::time::timeout;

use super::{BackendInstance, ForegroundProcess, Pane, PaneExtras, TerminalBackend};
use vtr::{boundary, effect, trace_error};

const TMUX_TIMEOUT_SHORT: Duration = Duration::from_secs(5);
const TMUX_TIMEOUT_LONG: Duration = Duration::from_secs(10);

const CONNECTION_PREFIX: &str = "tmux:";

// Max depth when walking /proc for child processes
const PROC_WALK_MAX_DEPTH: usize = 3;
// Max total children to collect across all depths
const PROC_WALK_MAX_BREADTH: usize = 32;

/// Tmux terminal backend.
pub struct TmuxBackend;

// =============================================================================
// Subprocess Helpers
// =============================================================================

/// Run a tmux command targeting a specific server socket, with timeout.
async fn run_tmux(
    socket: &str,
    args: &[&str],
    duration: Duration,
) -> Result<std::process::Output> {
    let op = args.first().unwrap_or(&"unknown");
    boundary!("tmux", op, socket = socket);

    let output = timeout(duration, async {
        tokio::process::Command::new("tmux")
            .args(["-S", socket])
            .args(args)
            .output()
            .await
            .context("tmux subprocess failed")
    })
    .await
    .context("tmux subprocess timed out")??;

    Ok(output)
}

/// Run a tmux command with stdin piped, for load-buffer.
async fn run_tmux_with_stdin(
    socket: &str,
    args: &[&str],
    stdin_data: &[u8],
    duration: Duration,
) -> Result<std::process::Output> {
    let op = args.first().unwrap_or(&"unknown");
    boundary!("tmux", op, socket = socket);

    use tokio::io::AsyncWriteExt;

    let output = timeout(duration, async {
        let mut child = tokio::process::Command::new("tmux")
            .args(["-S", socket])
            .args(args)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .context("tmux subprocess spawn failed")?;

        if let Some(mut stdin) = child.stdin.take() {
            stdin.write_all(stdin_data).await.ok();
            drop(stdin);
        }

        child.wait_with_output().await.context("tmux wait failed")
    })
    .await
    .context("tmux subprocess timed out")??;

    Ok(output)
}

// =============================================================================
// Connection String Helpers
// =============================================================================

/// Extract the raw socket path from a connection string.
/// `"tmux:/tmp/tmux-1000/default"` → `"/tmp/tmux-1000/default"`
fn socket_from_conn(conn: &str) -> &str {
    conn.strip_prefix(CONNECTION_PREFIX).unwrap_or(conn)
}

/// Build a connection string from a raw socket path.
fn conn_from_socket(socket: &str) -> String {
    format!("{CONNECTION_PREFIX}{socket}")
}

/// Format a tmux pane target from a numeric id: `42` → `%42`
fn pane_target(id: u64) -> String {
    format!("%{id}")
}

// =============================================================================
// Discovery Helpers
// =============================================================================

/// Parse the `$TMUX` environment variable.
/// Format: `<socket_path>,<server_pid>,<session_index>`
fn parse_tmux_env() -> Option<(String, u32)> {
    let val = std::env::var("TMUX").ok()?;
    let mut parts = val.splitn(3, ',');
    let socket = parts.next()?;
    let pid: u32 = parts.next()?.parse().ok()?;
    Some((socket.to_string(), pid))
}

/// Find all tmux server sockets for the current user.
///
/// Scans `/tmp/tmux-$UID/` for socket files. Each socket is a separate
/// tmux server instance.
pub fn find_all_sockets() -> Vec<String> {
    let uid = users::get_current_uid();
    let tmux_dir = format!("/tmp/tmux-{uid}");

    let Ok(entries) = std::fs::read_dir(&tmux_dir) else {
        return Vec::new();
    };

    entries
        .filter_map(|e| e.ok())
        .filter(|e| {
            e.file_type()
                .map(|ft| {
                    use std::os::unix::fs::FileTypeExt;
                    ft.is_socket()
                })
                .unwrap_or(false)
        })
        .map(|e| e.path().to_string_lossy().to_string())
        .collect()
}

/// Parse tmux `list-panes -a -F` output into Pane structs.
///
/// Format string fields (tab-separated):
/// pane_id, pane_pid, pane_title, pane_current_path, window_active,
/// pane_active, session_name, window_index
fn parse_list_panes(output: &str, conn: &str) -> Vec<Pane> {
    let mut panes = Vec::new();

    for line in output.lines() {
        let fields: Vec<&str> = line.split('\t').collect();
        if fields.len() < 8 {
            continue;
        }

        let pane_id_str = fields[0]; // e.g., "%5"
        let pane_pid: u32 = fields[1].parse().unwrap_or(0);
        let title = fields[2];
        let cwd = fields[3];
        let window_active = fields[4] == "1";
        let pane_active = fields[5] == "1";
        let session_name = fields[6];
        let window_idx: u32 = fields[7].parse().unwrap_or(0);

        // Strip '%' prefix and parse as u64
        let id: u64 = match pane_id_str.strip_prefix('%').and_then(|s| s.parse().ok()) {
            Some(id) => id,
            None => continue,
        };

        // Discover foreground processes from pane_pid
        let foreground_processes = discover_foreground_processes(pane_pid);

        // Read babel user vars from pane options
        // Deferred to avoid N subprocess calls per discovery tick —
        // user_vars are populated lazily or via hook binding
        let user_vars = HashMap::new();

        panes.push(Pane {
            connection: conn.to_string(),
            id,
            title: title.to_string(),
            cwd: PathBuf::from(cwd),
            is_focused: window_active && pane_active,
            is_active: pane_active,
            foreground_processes,
            user_vars,
            platform_window_id: None,
            extras: PaneExtras::Tmux {
                session: session_name.to_string(),
                window_idx,
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
/// tmux only gives us `pane_pid` (the shell). We walk its children
/// to find the actual agent process (claude, codex, etc.).
///
/// Depth limit: 3 levels (shell → agent → agent child)
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

        // Read this process's cmdline
        if let Some(proc_info) = read_proc_info(pid) {
            result.push(proc_info);
        }

        // Find children
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

    let cwd = std::fs::read_link(format!("/proc/{pid}/cwd"))
        .unwrap_or_default();

    Some(ForegroundProcess { pid, cmdline, cwd })
}

// =============================================================================
// User Vars (pane options with @babel_ prefix)
// =============================================================================

/// Read all @babel_* pane options for a tmux pane.
///
/// Calls `tmux show-options -p -t %N` and filters for `@babel_` prefix.
async fn read_user_vars(socket: &str, id: u64) -> HashMap<String, String> {
    let target = pane_target(id);
    let output = run_tmux(
        socket,
        &["show-options", "-p", "-t", &target],
        TMUX_TIMEOUT_SHORT,
    )
    .await;

    let mut vars = HashMap::new();
    if let Ok(output) = output {
        if output.status.success() {
            let stdout = String::from_utf8_lossy(&output.stdout);
            for line in stdout.lines() {
                // Format: "@babel_key value" or "@babel_key "quoted value""
                if let Some(rest) = line.strip_prefix("@babel_") {
                    if let Some((key, val)) = rest.split_once(' ') {
                        let val = val.trim_matches('"');
                        vars.insert(key.to_string(), val.to_string());
                    }
                }
            }
        }
    }

    vars
}

// =============================================================================
// TmuxBackend — TerminalBackend implementation
// =============================================================================

#[async_trait]
impl TerminalBackend for TmuxBackend {
    fn backend_name(&self) -> &'static str {
        "tmux"
    }

    fn default_connection(&self) -> Option<String> {
        // Priority 1: $TMUX env var
        if let Some((socket, _pid)) = parse_tmux_env() {
            return Some(conn_from_socket(&socket));
        }

        // Priority 2: scan for sockets
        let sockets = find_all_sockets();
        sockets.first().map(|s| conn_from_socket(s))
    }

    fn find_all_connections(&self) -> Vec<String> {
        find_all_sockets()
            .into_iter()
            .map(|s| conn_from_socket(&s))
            .collect()
    }

    async fn list_panes(&self, conn: &str) -> Result<Vec<Pane>> {
        let socket = socket_from_conn(conn);

        let format_str = "#{pane_id}\t#{pane_pid}\t#{pane_title}\t#{pane_current_path}\t#{window_active}\t#{pane_active}\t#{session_name}\t#{window_index}";
        let output = run_tmux(
            socket,
            &["list-panes", "-a", "-F", format_str],
            TMUX_TIMEOUT_SHORT,
        )
        .await?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            bail!("tmux list-panes failed: {stderr}");
        }

        let stdout = String::from_utf8_lossy(&output.stdout);
        Ok(parse_list_panes(&stdout, conn))
    }

    async fn discover_instances(&self) -> Vec<BackendInstance> {
        let sockets = find_all_sockets();
        let current_socket = parse_tmux_env().map(|(s, _)| s);

        let mut instances = Vec::new();

        for socket in sockets {
            let conn = conn_from_socket(&socket);
            let is_current = current_socket.as_deref() == Some(&socket);

            match self.list_panes(&conn).await {
                Ok(panes) => {
                    // Extract server PID from socket name or from $TMUX
                    let pid = if is_current {
                        parse_tmux_env().map(|(_, pid)| pid)
                    } else {
                        None
                    };

                    instances.push(BackendInstance {
                        connection: conn,
                        pid,
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
        let socket = socket_from_conn(conn);
        let target = pane_target(id);

        let output = run_tmux(
            socket,
            &["select-pane", "-t", &target],
            TMUX_TIMEOUT_SHORT,
        )
        .await?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            bail!("tmux select-pane failed: {stderr}");
        }

        // Also select the window containing this pane
        let _ = run_tmux(
            socket,
            &["select-window", "-t", &target],
            TMUX_TIMEOUT_SHORT,
        )
        .await;

        effect!("tmux", "pane focused", id = id);
        Ok(())
    }

    async fn send_text(&self, conn: &str, id: u64, text: &str) -> Result<()> {
        let socket = socket_from_conn(conn);
        let target = pane_target(id);

        // Safe text injection: send-keys -l sends literal text without
        // interpreting key names. Enter is sent separately.
        //
        // For multiline text, use load-buffer + paste-buffer which handles
        // arbitrary content without escaping issues.
        if text.contains('\n') || text.contains('\r') {
            // Strip trailing CR/LF — we send Enter separately
            let body = text.trim_end_matches(['\r', '\n']);

            // Pipe text into tmux buffer, then paste
            let output = run_tmux_with_stdin(
                socket,
                &["load-buffer", "-"],
                body.as_bytes(),
                TMUX_TIMEOUT_SHORT,
            )
            .await?;

            if !output.status.success() {
                let stderr = String::from_utf8_lossy(&output.stderr);
                bail!("tmux load-buffer failed: {stderr}");
            }

            let output = run_tmux(
                socket,
                &["paste-buffer", "-d", "-t", &target],
                TMUX_TIMEOUT_SHORT,
            )
            .await?;

            if !output.status.success() {
                let stderr = String::from_utf8_lossy(&output.stderr);
                bail!("tmux paste-buffer failed: {stderr}");
            }

            // Send Enter if the original text ended with CR/LF
            if text.ends_with('\r') || text.ends_with('\n') {
                let _ = run_tmux(
                    socket,
                    &["send-keys", "-t", &target, "Enter"],
                    TMUX_TIMEOUT_SHORT,
                )
                .await;
            }
        } else {
            // Simple single-line: send-keys -l for literal text
            let has_cr = text.ends_with('\r') || text.ends_with('\n');
            let body = text.trim_end_matches(['\r', '\n']);

            if !body.is_empty() {
                let output = run_tmux(
                    socket,
                    &["send-keys", "-l", "-t", &target, body],
                    TMUX_TIMEOUT_SHORT,
                )
                .await?;

                if !output.status.success() {
                    let stderr = String::from_utf8_lossy(&output.stderr);
                    bail!("tmux send-keys failed: {stderr}");
                }
            }

            if has_cr {
                let _ = run_tmux(
                    socket,
                    &["send-keys", "-t", &target, "Enter"],
                    TMUX_TIMEOUT_SHORT,
                )
                .await;
            }
        }

        effect!("tmux", "text sent", id = id);
        Ok(())
    }

    async fn get_scrollback(&self, conn: &str, id: u64) -> Result<String> {
        let socket = socket_from_conn(conn);
        let target = pane_target(id);

        // -p: print to stdout, -S -: start from beginning of history
        let output = run_tmux(
            socket,
            &["capture-pane", "-p", "-S", "-", "-t", &target],
            TMUX_TIMEOUT_LONG,
        )
        .await?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            bail!("tmux capture-pane failed: {stderr}");
        }

        Ok(String::from_utf8_lossy(&output.stdout).to_string())
    }

    async fn get_recent_scrollback(&self, conn: &str, id: u64, lines: usize) -> Result<String> {
        let socket = socket_from_conn(conn);
        let target = pane_target(id);

        // -S -N: start N lines from the end
        let start = format!("-{lines}");
        let output = run_tmux(
            socket,
            &["capture-pane", "-p", "-S", &start, "-t", &target],
            TMUX_TIMEOUT_SHORT,
        )
        .await?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            bail!("tmux capture-pane failed: {stderr}");
        }

        Ok(String::from_utf8_lossy(&output.stdout).to_string())
    }

    async fn close_pane(&self, conn: &str, id: u64) -> Result<()> {
        let socket = socket_from_conn(conn);
        let target = pane_target(id);

        let output = run_tmux(
            socket,
            &["kill-pane", "-t", &target],
            TMUX_TIMEOUT_SHORT,
        )
        .await?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            bail!("tmux kill-pane failed: {stderr}");
        }

        effect!("tmux", "pane closed", id = id);
        Ok(())
    }

    async fn set_meta(&self, conn: &str, id: u64, key: &str, val: &str) -> Result<()> {
        let socket = socket_from_conn(conn);
        let target = pane_target(id);
        let option_name = format!("@babel_{key}");

        let output = run_tmux(
            socket,
            &["set-option", "-p", "-t", &target, &option_name, val],
            TMUX_TIMEOUT_SHORT,
        )
        .await?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            bail!("tmux set-option failed: {stderr}");
        }

        effect!("tmux", "meta set", id = id, key = key);
        Ok(())
    }

    async fn set_title(&self, conn: &str, id: u64, title: &str) -> Result<()> {
        let socket = socket_from_conn(conn);
        let target = pane_target(id);

        let output = run_tmux(
            socket,
            &["select-pane", "-t", &target, "-T", title],
            TMUX_TIMEOUT_SHORT,
        )
        .await?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            bail!("tmux set title failed: {stderr}");
        }

        effect!("tmux", "title set", id = id);
        Ok(())
    }

    // Tmux panes share the host terminal's X11 window
    fn has_desktop_windows(&self) -> bool {
        false
    }
}
