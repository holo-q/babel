//! Babel - CLI for managing Claude Code sessions across kitty windows
//!
//! Two modes of operation:
//! - With daemon: Instant responses via IPC (recommended)
//! - Without daemon: Direct kitty/file queries (fallback)
//!
//! Start daemon: `babel daemon`
//! All other commands auto-detect and use daemon if available.

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use tracing_subscriber::{prelude::*, EnvFilter};

use claude_babel::claude_storage::{get_recent_sessions, SessionInfo};
use claude_babel::daemon::{run_daemon, run_daemon_traced};
use claude_babel::discovery::{discover_claude_windows, ClaudeWindow};
use claude_babel::ipc::{send_request, socket_path, Request, Response};
use claude_babel::kitty::{focus_window, get_scrollback, send_text};
use claude_babel::overlay::{get_metadata, init_db, mark_read, set_icon};

#[derive(Parser)]
#[command(name = "babel")]
#[command(about = "Manage Claude Code sessions across kitty windows", long_about = None)]
struct Cli {
    #[command(subcommand)]
    command: Commands,

    /// Output as JSON
    #[arg(long, global = true)]
    json: bool,
}

#[derive(Subcommand)]
enum Commands {
    /// Start the babel daemon (babeld)
    Daemon {
        /// Enable verbose trace logging
        #[arg(long)]
        trace: bool,
    },

    /// Stop the running daemon
    Stop,

    /// Check if daemon is running
    Ping,

    /// List all discovered Claude sessions
    #[command(alias = "list")]
    Ls {
        /// Show detailed multiline info for each session
        #[arg(short, long)]
        details: bool,
    },

    /// List all kitty terminals (not just Claude)
    LsTerminals,

    /// List all kitty panels/windows with their IDs
    LsPanels,

    /// Show status of a specific window
    Status {
        /// Kitty window ID to query
        window_id: Option<u64>,
    },

    /// Focus a Claude window (rofi picker if no ID given)
    Focus {
        /// Kitty window ID to focus (omit for interactive rofi picker)
        window_id: Option<u64>,
    },

    /// Get scrollback from a window
    Scroll {
        /// Kitty window ID
        window_id: u64,
    },

    /// Send text to a Claude window
    Send {
        /// Kitty window ID
        window_id: u64,

        /// Text to send
        text: String,
    },

    /// Tag a window with a custom icon
    Tag {
        /// Kitty window ID
        window_id: u64,

        /// Icon/tag to apply
        icon: String,
    },

    /// Mark a window's conversation as read
    MarkRead {
        /// Kitty window ID
        window_id: u64,
    },

    /// Show conversation history from ~/.claude
    History {
        /// Limit number of results
        #[arg(short, long, default_value = "20")]
        limit: usize,
    },

    /// Force refresh daemon state
    Refresh,

    /// Subscribe to daemon events (streaming)
    Subscribe {
        /// Event types to subscribe to (empty = all)
        #[arg(short, long)]
        events: Vec<String>,

        /// Output format: json (default) or compact
        #[arg(long, default_value = "json")]
        format: String,
    },

    /// Get or refresh workspace titles
    Titles {
        /// Force refresh titles (otherwise returns cached)
        #[arg(short, long)]
        refresh: bool,

        /// Refresh only this workspace
        #[arg(short, long)]
        workspace: Option<i32>,
    },
}

/// Initialize structured logging to journald
///
/// Logs are sent to systemd journald with identifier "babel".
/// View logs with: journalctl -t babel -f
/// Set level via RUST_LOG env var (default: info)
fn init_logging() {
    let env_filter = EnvFilter::try_from_env("RUST_LOG")
        .unwrap_or_else(|_| EnvFilter::new("claude_babel=info"));

    let journald_layer = tracing_journald::layer()
        .expect("Failed to connect to journald")
        .with_syslog_identifier("babel".to_string());

    let subscriber = tracing_subscriber::registry()
        .with(env_filter)
        .with(journald_layer);

    let _ = tracing::subscriber::set_global_default(subscriber);
}

#[tokio::main]
async fn main() -> Result<()> {
    init_logging();

    let cli = Cli::parse();

    match cli.command {
        // Daemon management commands - always direct
        Commands::Daemon { trace } => {
            if trace {
                run_daemon_traced().await
            } else {
                run_daemon().await
            }
        }
        Commands::Stop => cmd_stop().await,
        Commands::Ping => cmd_ping().await,
        Commands::Refresh => cmd_refresh().await,
        Commands::Subscribe { events, format } => cmd_subscribe(events, format).await,
        Commands::Titles { refresh, workspace } => cmd_titles(cli.json, refresh, workspace).await,

        // Data commands - use daemon if available
        Commands::Ls { details } => cmd_list(cli.json, details).await,
        Commands::LsTerminals => cmd_ls_terminals(cli.json).await,
        Commands::LsPanels => cmd_ls_panels(cli.json).await,
        Commands::Status { window_id } => cmd_status(window_id, cli.json).await,
        Commands::History { limit } => cmd_history(limit, cli.json).await,

        // Action commands - use daemon if available
        Commands::Focus { window_id } => cmd_focus(window_id).await,
        Commands::Scroll { window_id } => cmd_scroll(window_id).await,
        Commands::Send { window_id, text } => cmd_send(window_id, &text).await,
        Commands::Tag { window_id, icon } => cmd_tag(window_id, &icon).await,
        Commands::MarkRead { window_id } => cmd_mark_read(window_id).await,
    }
}

// ═══════════════════════════════════════════════════════════════════════════════
// Daemon Management Commands
// ═══════════════════════════════════════════════════════════════════════════════

async fn cmd_stop() -> Result<()> {
    match send_request(&Request::Shutdown).await {
        Ok(Response::Ok { message }) => {
            println!("{}", message);
            Ok(())
        }
        Ok(Response::Error { message }) => {
            tracing::error!("Shutdown failed: {}", message);
            Ok(())
        }
        Err(_) => {
            println!("Daemon not running");
            Ok(())
        }
        _ => Ok(()),
    }
}

async fn cmd_ping() -> Result<()> {
    match send_request(&Request::Ping).await {
        Ok(Response::Pong { uptime_secs }) => {
            println!("Daemon running (uptime: {}s)", uptime_secs);
            Ok(())
        }
        Err(_) => {
            println!("Daemon not running");
            std::process::exit(1);
        }
        _ => Ok(()),
    }
}

async fn cmd_refresh() -> Result<()> {
    match send_request(&Request::Refresh).await {
        Ok(Response::Ok { message }) => {
            println!("{}", message);
            Ok(())
        }
        Ok(Response::Error { message }) => {
            tracing::error!("Refresh failed: {}", message);
            Ok(())
        }
        Err(_) => {
            println!("Daemon not running");
            Ok(())
        }
        _ => Ok(()),
    }
}

async fn cmd_titles(json: bool, refresh: bool, workspace: Option<i32>) -> Result<()> {
    // If refresh requested, trigger it and show results
    if refresh {
        match send_request(&Request::TitleRefresh { workspace }).await {
            Ok(Response::Ok { message }) => {
                if json {
                    // For JSON output, fetch the actual titles structure
                    if let Ok(Response::Titles { titles }) = send_request(&Request::Titles).await {
                        println!("{}", serde_json::to_string_pretty(&titles)?);
                    }
                } else {
                    println!("{}", message);
                }
                return Ok(());
            }
            Ok(Response::Error { message }) => {
                tracing::error!("Title refresh failed: {}", message);
                return Ok(());
            }
            Err(_) => {
                tracing::error!("Daemon not running");
                return Ok(());
            }
            _ => return Ok(()),
        }
    }

    // Just fetch current titles (no refresh)
    match send_request(&Request::Titles).await {
        Ok(Response::Titles { titles }) => {
            if json {
                println!("{}", serde_json::to_string_pretty(&titles)?);
            } else if titles.is_empty() {
                println!("No workspace titles cached");
            } else {
                // Sort by workspace number (parse string keys to int for proper ordering)
                let mut entries: Vec<_> = titles.into_iter().collect();
                entries.sort_by_key(|(ws, _)| ws.parse::<i32>().unwrap_or(0));
                for (ws, title) in entries {
                    println!("  Workspace {}: {}", ws, title);
                }
            }
            Ok(())
        }
        Ok(Response::Error { message }) => {
            tracing::error!("Failed to get titles: {}", message);
            Ok(())
        }
        Err(_) => {
            tracing::error!("Daemon not running");
            Ok(())
        }
        _ => Ok(()),
    }
}

async fn cmd_subscribe(events: Vec<String>, format: String) -> Result<()> {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

    let request = Request::Subscribe { events };
    let sock_path = socket_path();

    let stream = tokio::net::UnixStream::connect(&sock_path)
        .await
        .context("Failed to connect to daemon")?;

    // Send subscribe request
    let (reader, mut writer) = stream.into_split();
    let mut request_json = serde_json::to_string(&request)?;
    request_json.push('\n');
    writer.write_all(request_json.as_bytes()).await?;

    // Read events forever
    let mut reader = BufReader::new(reader);
    let mut line = String::new();

    loop {
        line.clear();
        let n = reader.read_line(&mut line).await?;
        if n == 0 {
            tracing::info!("Event stream connection closed");
            break;
        }

        if format == "compact" {
            // Parse and format compactly
            if let Ok(resp) = serde_json::from_str::<Response>(&line) {
                match resp {
                    Response::Event { event } => {
                        println!("[{}] {:?}", event.timestamp.format("%H:%M:%S"), event.event);
                    }
                    Response::Subscribed { subscriber_id } => {
                        tracing::info!(subscriber_id, "Subscribed to events");
                    }
                    _ => {}
                }
            }
        } else {
            // Raw JSON output
            print!("{}", line);
        }
    }

    Ok(())
}

// ═══════════════════════════════════════════════════════════════════════════════
// Data Commands - Daemon or Direct
// ═══════════════════════════════════════════════════════════════════════════════

async fn cmd_list(json: bool, details: bool) -> Result<()> {
    let mut windows = if details {
        get_windows_with_fingerprints().await?
    } else {
        get_windows().await?
    };

    if json {
        println!("{}", serde_json::to_string_pretty(&windows)?);
        return Ok(());
    }

    if windows.is_empty() {
        println!("No Claude sessions found");
        return Ok(());
    }

    // Sort by workspace, then by OS window ID for consistent grouping
    windows.sort_by(|a, b| {
        let ws_a = a.workspace.unwrap_or(999);
        let ws_b = b.workspace.unwrap_or(999);
        ws_a.cmp(&ws_b)
            .then(a.os_window_id.cmp(&b.os_window_id))
            .then(a.kitty_id.cmp(&b.kitty_id))
    });

    // Group and display by workspace
    use console::Style;

    let mut current_workspace: Option<i32> = None;
    let dim = Style::new().dim();

    for win in &windows {
        let ws = win.workspace.unwrap_or(-2); // -2 = unknown

        // Print workspace header when workspace changes
        if current_workspace != Some(ws) {
            if current_workspace.is_some() {
                println!(); // Blank line between workspaces
            }
            let ws_name = match ws {
                -1 => "Sticky".to_string(),
                -2 => "?".to_string(),
                n => format!("{}", n + 1), // 0-indexed to 1-indexed
            };
            println!("{}", dim.apply_to(format!("─── Workspace {} ───", ws_name)));
            current_workspace = Some(ws);
        }

        if details {
            print_window_detailed(win)?;
        } else {
            print_window(win)?;
        }
    }

    Ok(())
}

/// List all kitty terminals (not just Claude sessions)
async fn cmd_ls_terminals(json: bool) -> Result<()> {
    use claude_babel::kitty::list_windows;

    let windows = list_windows().context("Failed to list kitty windows")?;

    if json {
        println!("{}", serde_json::to_string_pretty(&windows)?);
        return Ok(());
    }

    if windows.is_empty() {
        println!("No kitty windows found");
        return Ok(());
    }

    println!("Kitty terminals ({} total):", windows.len());
    println!();

    for win in &windows {
        let cmdline = win.foreground_processes
            .first()
            .and_then(|p| p.cmdline.first())
            .map(|s| s.as_str())
            .unwrap_or("?");

        // Truncate title
        let title: String = win.title.chars().take(40).collect();
        let title = if win.title.len() > 40 {
            format!("{}…", title)
        } else {
            title
        };

        println!("  {:>5}  {}  {}", win.id, cmdline, title);
    }

    Ok(())
}

/// List all kitty panels/windows grouped by OS window
async fn cmd_ls_panels(json: bool) -> Result<()> {
    use claude_babel::kitty::list_windows;
    use std::collections::HashMap;

    let windows = list_windows().context("Failed to list kitty windows")?;

    if json {
        println!("{}", serde_json::to_string_pretty(&windows)?);
        return Ok(());
    }

    if windows.is_empty() {
        println!("No kitty panels found");
        return Ok(());
    }

    // Group by OS window
    let mut by_os_window: HashMap<u64, Vec<_>> = HashMap::new();
    for win in windows {
        by_os_window.entry(win.os_window_id).or_default().push(win);
    }

    let total_panels: usize = by_os_window.values().map(|v| v.len()).sum();
    println!("Kitty panels ({} panels in {} OS windows):", total_panels, by_os_window.len());
    println!();

    for (os_id, panels) in by_os_window.iter() {
        println!("  OS Window {} ({} panels):", os_id, panels.len());
        for win in panels {
            let title: String = win.title.chars().take(50).collect();
            let title = if win.title.len() > 50 {
                format!("{}…", title)
            } else {
                title
            };
            let focus = if win.is_focused { "*" } else { " " };
            println!("    {:>5}{} {}", win.id, focus, title);
        }
    }

    Ok(())
}

async fn cmd_status(window_id: Option<u64>, json: bool) -> Result<()> {
    let window = get_window(window_id).await?;

    match window {
        Some(win) => {
            if json {
                println!("{}", serde_json::to_string_pretty(&win)?);
            } else {
                print_window(&win)?;
            }
        }
        None => {
            if window_id.is_some() {
                println!("Window not found or not a Claude session");
            } else {
                println!("No focused Claude window found");
            }
        }
    }

    Ok(())
}

async fn cmd_history(limit: usize, json: bool) -> Result<()> {
    let sessions = get_history(limit).await?;

    if json {
        println!("{}", serde_json::to_string_pretty(&sessions)?);
        return Ok(());
    }

    if sessions.is_empty() {
        println!("No conversation history found");
        return Ok(());
    }

    println!("Recent conversations:");
    println!();

    for session in &sessions {
        print_session(session)?;
    }

    Ok(())
}

// ═══════════════════════════════════════════════════════════════════════════════
// Action Commands - Daemon or Direct
// ═══════════════════════════════════════════════════════════════════════════════

async fn cmd_focus(window_id: Option<u64>) -> Result<()> {
    // Direct focus if ID provided
    if let Some(id) = window_id {
        return focus_by_id(id).await;
    }

    // Interactive picker via rofi
    let windows = get_windows().await?;
    if windows.is_empty() {
        println!("No Claude sessions found");
        return Ok(());
    }

    // Sort by workspace for consistent ordering
    let mut windows = windows;
    windows.sort_by(|a, b| {
        let ws_a = a.workspace.unwrap_or(999);
        let ws_b = b.workspace.unwrap_or(999);
        ws_a.cmp(&ws_b)
            .then(a.os_window_id.cmp(&b.os_window_id))
            .then(a.kitty_id.cmp(&b.kitty_id))
    });

    // Format entries for rofi: "[ws] title │ ~/path"
    let entries: Vec<(u64, String)> = windows.iter().map(|win| {
        let ws = match win.workspace {
            Some(-1) => "S".to_string(),  // Sticky
            Some(n) => format!("{}", n + 1),
            None => "?".to_string(),
        };

        // Strip ✳ prefix from active sessions
        let title = win.title.strip_prefix("✳ ").unwrap_or(&win.title);
        let title_short: String = title.chars().take(40).collect();
        let title_display = if title.len() > 40 {
            format!("{}…", title_short)
        } else {
            title_short
        };

        // Compact cwd
        let cwd = win.cwd
            .strip_prefix(dirs::home_dir().unwrap_or_default())
            .map(|p| format!("~/{}", p.display()))
            .unwrap_or_else(|_| win.cwd.display().to_string());

        let label = format!("[{}] {} │ {}", ws, title_display, cwd);
        (win.kitty_id, label)
    }).collect();

    // Launch rofi
    let labels: Vec<&str> = entries.iter().map(|(_, l)| l.as_str()).collect();

    match rofi::Rofi::new(&labels).prompt("Claude").run() {
        Ok(choice) => {
            if let Some((id, _)) = entries.iter().find(|(_, l)| l == &choice) {
                focus_by_id(*id).await?;
            }
        }
        Err(rofi::Error::Interrupted) => {} // User cancelled (Esc)
        Err(e) => anyhow::bail!("Rofi error: {}", e),
    }

    Ok(())
}

/// Focus a window by its kitty ID (via daemon or direct)
async fn focus_by_id(window_id: u64) -> Result<()> {
    // Try daemon first
    if let Ok(Response::Ok { message }) = send_request(&Request::Focus { window_id }).await {
        println!("{}", message);
        return Ok(());
    }

    // Direct fallback
    focus_window(window_id).context("Failed to focus window")?;
    println!("Focused window {}", window_id);
    Ok(())
}

async fn cmd_scroll(window_id: u64) -> Result<()> {
    // Try daemon first
    if let Ok(Response::Scrollback { text }) = send_request(&Request::Scroll { window_id }).await {
        print!("{}", text);
        return Ok(());
    }

    // Direct fallback
    let scrollback = get_scrollback(window_id).context("Failed to get scrollback")?;
    print!("{}", scrollback);
    Ok(())
}

async fn cmd_send(window_id: u64, text: &str) -> Result<()> {
    // Try daemon first
    if let Ok(Response::Ok { message }) = send_request(&Request::Send {
        window_id,
        text: text.to_string(),
    }).await {
        println!("{}", message);
        return Ok(());
    }

    // Direct fallback
    send_text(window_id, text).context("Failed to send text")?;
    println!("Sent text to window {}", window_id);
    Ok(())
}

async fn cmd_tag(window_id: u64, icon: &str) -> Result<()> {
    // Try daemon first
    if let Ok(Response::Ok { message }) = send_request(&Request::Tag {
        window_id,
        icon: icon.to_string(),
    }).await {
        println!("{}", message);
        return Ok(());
    }

    // Direct fallback - need to find session first
    let windows = discover_claude_windows()?;
    let window = windows
        .into_iter()
        .find(|w| w.kitty_id == window_id)
        .context("Window not found")?;

    let session_id = window.session_id.context("Window has no session")?;

    let conn = init_db()?;
    set_icon(&conn, &session_id, icon)?;

    println!("Tagged window {} with: {}", window_id, icon);
    Ok(())
}

async fn cmd_mark_read(window_id: u64) -> Result<()> {
    // Try daemon first
    if let Ok(Response::Ok { message }) = send_request(&Request::MarkRead { window_id }).await {
        println!("{}", message);
        return Ok(());
    }

    // Direct fallback
    let windows = discover_claude_windows()?;
    let window = windows
        .into_iter()
        .find(|w| w.kitty_id == window_id)
        .context("Window not found")?;

    let session_id = window.session_id.context("Window has no session")?;

    let conn = init_db()?;
    mark_read(&conn, &session_id)?;

    println!("Marked window {} as read", window_id);
    Ok(())
}

// ═══════════════════════════════════════════════════════════════════════════════
// Data Fetching - Daemon or Direct
// ═══════════════════════════════════════════════════════════════════════════════

async fn get_windows() -> Result<Vec<ClaudeWindow>> {
    // Try daemon first
    if let Ok(Response::Windows { windows }) = send_request(&Request::List).await {
        return Ok(windows);
    }

    // Direct fallback
    discover_claude_windows()
}

async fn get_windows_with_fingerprints() -> Result<Vec<ClaudeWindow>> {
    // Try daemon first
    if let Ok(Response::Windows { windows }) = send_request(&Request::ListWithFingerprints).await {
        return Ok(windows);
    }

    // Direct fallback - extract fingerprints manually
    use claude_babel::fingerprint::extract_from_scrollback;
    use claude_babel::kitty::get_scrollback;

    let mut windows = discover_claude_windows()?;
    for win in &mut windows {
        if let Ok(scrollback) = get_scrollback(win.kitty_id) {
            let fp = extract_from_scrollback(&scrollback);
            win.fingerprint = Some(fp);
        }
    }
    Ok(windows)
}

async fn get_window(window_id: Option<u64>) -> Result<Option<ClaudeWindow>> {
    // Try daemon first
    if let Ok(Response::Window { window }) = send_request(&Request::Status { window_id }).await {
        return Ok(window);
    }

    // Direct fallback
    let windows = discover_claude_windows()?;
    Ok(if let Some(id) = window_id {
        windows.into_iter().find(|w| w.kitty_id == id)
    } else {
        windows.into_iter().find(|w| w.is_focused)
    })
}

async fn get_history(limit: usize) -> Result<Vec<SessionInfo>> {
    // Try daemon first
    if let Ok(Response::History { sessions }) = send_request(&Request::History { limit }).await {
        return Ok(sessions);
    }

    // Direct fallback
    get_recent_sessions(limit)
}

// ═══════════════════════════════════════════════════════════════════════════════
// Display Helpers
// ═══════════════════════════════════════════════════════════════════════════════

use console::{style, Style};

fn print_window(win: &ClaudeWindow) -> Result<()> {
    let conn = init_db()?;

    // Get overlay metadata if we have a session ID
    let meta = win
        .session_id
        .as_ref()
        .and_then(|id| get_metadata(&conn, id).ok().flatten());

    // Indicators
    let focus_indicator = if win.is_focused { "▸" } else { " " };
    let unread = !meta.as_ref().map(|m| m.is_read).unwrap_or(true);
    let custom_icon = meta.as_ref().and_then(|m| m.icon.as_ref());

    // Title - strip ✳ prefix if present, use summary from session if available
    let raw_title = win
        .session_info
        .as_ref()
        .and_then(|s| s.summaries.first())
        .map(|s| s.summary.as_str())
        .unwrap_or(&win.title);
    let title = raw_title.strip_prefix("✳ ").unwrap_or(raw_title);

    // Styles
    let dim = Style::new().dim();
    let bold = Style::new().bold();
    let yellow = Style::new().yellow();

    // Build the line
    let id_str = format!("{:>3}", win.kitty_id);

    // Unread dot or custom icon
    let marker = if let Some(icon) = custom_icon {
        format!("{} ", icon)
    } else if unread {
        format!("{} ", style("●").yellow())
    } else {
        "  ".to_string()
    };

    // Compact cwd - just the last component or ~ prefix
    let cwd_display = win.cwd
        .strip_prefix(dirs::home_dir().unwrap_or_default())
        .map(|p| format!("~/{}", p.display()))
        .unwrap_or_else(|_| win.cwd.display().to_string());

    // Truncate cwd if too long
    let cwd_short = if cwd_display.len() > 30 {
        format!("…{}", &cwd_display[cwd_display.len()-28..])
    } else {
        cwd_display
    };

    // Format: " ▸● 123 Title                        ~/path"
    print!(" {}{}", focus_indicator, marker);
    print!("{} ", if win.is_focused { bold.apply_to(&id_str) } else { dim.apply_to(&id_str) });
    print!("{}", if win.is_focused { yellow.apply_to(title) } else { Style::new().apply_to(title) });
    println!("  {}", dim.apply_to(&cwd_short));

    Ok(())
}

fn print_window_detailed(win: &ClaudeWindow) -> Result<()> {
    let conn = init_db()?;

    // Get overlay metadata if we have a session ID
    let meta = win
        .session_id
        .as_ref()
        .and_then(|id| get_metadata(&conn, id).ok().flatten());

    // Styles
    let dim = Style::new().dim();
    let bold = Style::new().bold();
    let yellow = Style::new().yellow();
    let cyan = Style::new().cyan();

    // Title - strip ✳ prefix
    let raw_title = win
        .session_info
        .as_ref()
        .and_then(|s| s.summaries.first())
        .map(|s| s.summary.as_str())
        .unwrap_or(&win.title);
    let title = raw_title.strip_prefix("✳ ").unwrap_or(raw_title);

    // Focus/unread indicators
    let focus_marker = if win.is_focused { "▸ " } else { "  " };
    let unread = !meta.as_ref().map(|m| m.is_read).unwrap_or(true);

    // Header line: focus + ID + title
    print!("{}", focus_marker);
    print!("{} ", if win.is_focused { bold.apply_to(format!("[{}]", win.kitty_id)) } else { dim.apply_to(format!("[{}]", win.kitty_id)) });
    if win.is_focused {
        println!("{}", yellow.apply_to(title));
    } else {
        println!("{}", title);
    }

    // Details
    let indent = "      ";

    // CWD - full path
    let cwd_display = win.cwd
        .strip_prefix(dirs::home_dir().unwrap_or_default())
        .map(|p| format!("~/{}", p.display()))
        .unwrap_or_else(|_| win.cwd.display().to_string());
    println!("{}{} {}", indent, dim.apply_to("cwd"), cwd_display);

    // Session ID with confidence if fingerprint matched
    if let Some(ref session_id) = win.session_id {
        if let Some(confidence) = win.match_confidence {
            println!("{}{} {} ({:?} confidence)",
                indent,
                dim.apply_to("session"),
                dim.apply_to(session_id),
                confidence
            );
        } else {
            println!("{}{} {}", indent, dim.apply_to("session"), dim.apply_to(session_id));
        }
    }

    // Fingerprint data if available
    if let Some(ref fp) = win.fingerprint {
        println!("{}{}", indent, dim.apply_to("fingerprint:"));

        if let Some(ref first) = fp.first_prompt {
            let truncated = if first.len() > 50 {
                format!("{}...", &first[..50])
            } else {
                first.clone()
            };
            println!("{}  {} \"{}\"", indent, dim.apply_to("first_prompt:"), truncated);
        }

        if !fp.tool_sequence.is_empty() {
            let tools_str = if fp.tool_sequence.len() > 4 {
                format!("[{}, ... {} total]", fp.tool_sequence[..4].join(", "), fp.tool_sequence.len())
            } else {
                format!("[{}]", fp.tool_sequence.join(", "))
            };
            println!("{}  {} {}", indent, dim.apply_to("tools:"), tools_str);
        }

        if !fp.recent_prompts.is_empty() {
            println!("{}  {} {}", indent, dim.apply_to("prompts:"), fp.recent_prompts.len());
        }
    }

    // Session info (when available)
    if let Some(ref info) = win.session_info {
        // Project path
        let project_display = info.project
            .strip_prefix(dirs::home_dir().unwrap_or_default())
            .map(|p| format!("~/{}", p.display()))
            .unwrap_or_else(|_| info.project.display().to_string());
        println!("{}{} {}", indent, dim.apply_to("project"), project_display);

        // Slug (codename)
        if let Some(ref slug) = info.slug {
            println!("{}{} {}", indent, dim.apply_to("slug"), cyan.apply_to(slug));
        }

        // Message count
        if info.message_count > 0 {
            println!("{}{} {}", indent, dim.apply_to("messages"), info.message_count);
        }

        // First prompt (from JSONL)
        if let Some(ref prompt) = info.first_prompt {
            println!("{}{} \"{}\"", indent, dim.apply_to("first"), dim.apply_to(prompt));
        }

        // Last activity timestamp
        if let Some(ref ts) = info.last_timestamp {
            println!("{}{} {}", indent, dim.apply_to("last"), dim.apply_to(ts));
        }
    }

    // Custom icon
    if let Some(ref icon) = meta.as_ref().and_then(|m| m.icon.as_ref()) {
        println!("{}{} {}", indent, dim.apply_to("icon"), icon);
    }

    // Unread status
    if unread {
        println!("{}{}", indent, yellow.apply_to("● unread"));
    }

    println!(); // Blank line between entries
    Ok(())
}

fn print_session(session: &SessionInfo) -> Result<()> {
    let dim = Style::new().dim();

    let summary = session
        .summaries
        .first()
        .map(|s| s.summary.as_str())
        .unwrap_or("(no summary)");

    let slug = session
        .slug
        .as_ref()
        .map(|s| format!(" ({})", s))
        .unwrap_or_default();

    // Compact project path
    let project_display = session.project
        .strip_prefix(dirs::home_dir().unwrap_or_default())
        .map(|p| format!("~/{}", p.display()))
        .unwrap_or_else(|_| session.project.display().to_string());

    println!("  {}{}  {}", summary, dim.apply_to(&slug), dim.apply_to(&project_display));

    Ok(())
}
