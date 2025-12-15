//! Babel - CLI for managing Claude Code sessions across kitty windows
//!
//! Two modes of operation:
//! - With daemon: Instant responses via IPC (recommended)
//! - Without daemon: Direct kitty/file queries (fallback)
//!
//! Start daemon: `babel daemon`
//! All other commands auto-detect and use daemon if available.

use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};

use claude_babel::claude_storage::{get_recent_sessions, SessionInfo};
use claude_babel::daemon::{run_daemon, run_daemon_traced};
use claude_babel::discovery::{discover_claude_windows, ClaudeWindow};
use claude_babel::fingerprint::migrate_project;
use claude_babel::ipc::{send_request, Request, Response};
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

    /// Logging args (--debug) - enforced by spaceship-std
    #[command(flatten)]
    logging: spaceship_std::logging::LoggingArgs,
}

#[derive(Subcommand)]
enum Commands {
    // ─── Information Retrieval ──────────────────────────────────────────────────

    /// List all discovered Claude sessions
    #[command(alias = "list")]
    Ls {
        /// Show detailed multiline info for each session
        #[arg(short, long)]
        details: bool,
    },

    /// List all kitty terminals (not just Claude)
    LsTerminals,

    /// List all kitty panes with their IDs
    ///
    /// Shows all kitty window panes grouped by OS window. Panes are the
    /// individual terminal views within a kitty window.
    #[command(alias = "lsp")]
    LsPanes,

    /// Check status of a kitty window
    ///
    /// Shows detailed information about a Claude window including session info,
    /// fingerprint data, and activity state. If no window ID is provided, shows
    /// the currently focused Claude window.
    #[command(alias = "cw")]
    CheckWindow {
        /// Kitty window ID to query (omit for focused window)
        window_id: Option<u64>,
    },

    /// Check status of a panel pane
    ///
    /// Shows information about a richspace-babel panel pane.
    #[command(alias = "cp")]
    CheckPane {
        /// Pane name to query
        pane_name: Option<String>,
    },

    /// Get scrollback text from a window
    ///
    /// Retrieves the full scrollback buffer from a kitty window. Useful for
    /// debugging or piping to other tools.
    #[command(alias = "gsb")]
    GetScrollback {
        /// Kitty window ID
        window_id: u64,
        /// Maximum number of lines to retrieve
        #[arg(short, long)]
        lines: Option<usize>,
    },

    /// Show conversation history from ~/.claude
    ///
    /// Without arguments, shows recent conversations. Pass session IDs as
    /// positional arguments to show specific sessions.
    #[command(alias = "h")]
    History {
        /// Session IDs to show (if none, shows recent conversations)
        #[arg(value_name = "SESSION")]
        sessions: Vec<String>,

        /// Limit number of results (when no session IDs specified)
        #[arg(short, long, default_value = "20")]
        limit: usize,

        /// Show all sessions (overrides limit)
        #[arg(long)]
        all: bool,
    },

    /// Debug fingerprint linkage between terminals, sessions, and directories
    ///
    /// Traces the connection between a terminal window, its scrollback fingerprint,
    /// and matching session JSONL files. Use this to debug why `babel mv` doesn't
    /// detect a session or why matching fails.
    ///
    /// Input is auto-detected:
    ///   - Pure number (42) → window ID
    ///   - Path-like (., ./foo, /path) → directory
    ///   - Otherwise → session ID
    ///
    /// Examples:
    ///   babel fingerprint 42        # Trace window ID 42
    ///   babel fingerprint .         # Trace current directory
    ///   babel fingerprint abc123    # Trace session abc123
    #[command(alias = "fp")]
    Fingerprint {
        /// Window ID, directory path, or session ID (auto-detected)
        #[arg(value_name = "INPUT")]
        input: Option<String>,

        /// Force interpretation as window ID
        #[arg(long, short = 'w')]
        window: bool,

        /// Force interpretation as directory
        #[arg(long, short = 'd')]
        dir: bool,

        /// Force interpretation as session ID
        #[arg(long, short = 's')]
        session: bool,
    },

    // ─── Actions ────────────────────────────────────────────────────────────────

    /// Focus a Claude window (rofi picker if no ID given)
    Focus {
        /// Kitty window ID to focus (omit for interactive rofi picker)
        window_id: Option<u64>,
    },

    /// Send text to a Claude window
    Send {
        /// Kitty window ID
        window_id: u64,

        /// Text to send
        text: String,
    },

    /// Set a custom icon for a window
    ///
    /// Associates a custom emoji or icon with a Claude session. The icon
    /// appears in `babel ls` output and can be used to visually mark
    /// important sessions.
    #[command(alias = "si")]
    SetIcon {
        /// Kitty window ID
        window_id: u64,

        /// Icon/emoji to display (e.g., "🔥", "⭐", "🚧")
        icon: String,
    },

    /// Mark a window's conversation as read
    MarkRead {
        /// Kitty window ID
        window_id: u64,
    },

    // ─── Management ─────────────────────────────────────────────────────────────

    /// Update or display workspace titles
    ///
    /// Workspace titles are derived from the Claude sessions visible on each
    /// workspace. Use --refresh to force an update from the current window state.
    #[command(alias = "ut")]
    UpdateTitles {
        /// Force refresh titles (otherwise returns cached)
        #[arg(short, long)]
        refresh: bool,

        /// Refresh only this workspace
        #[arg(short, long)]
        workspace: Option<i32>,
    },

    /// Move a directory while preserving Claude conversation history
    ///
    /// When you move a project directory, Claude's conversation history becomes
    /// orphaned because it stores paths like ~/.claude/projects/-home-user-OldProject/.
    /// This command updates those paths so your conversations follow the directory.
    ///
    /// If Claude terminals are open in the source path:
    /// - Idle terminals are automatically migrated (cd + claude -r)
    /// - Active terminals block the move (use --force to override)
    ///
    /// Usage:
    ///   babel mv ~/OldProject ~/NewProject           # Move + update history
    ///   babel mv --dry ~/OldProject ~/NewProject      # Preview changes only
    ///   babel mv --history-only ~/Old ~/New          # Update history without moving
    ///   babel mv --anxious ~/Old ~/New               # Step-by-step confirmation
    ///   babel mv --force ~/Old ~/New                 # Force move even with active terminals
    Mv {
        /// Source directory path
        source: PathBuf,

        /// Destination directory path
        dest: PathBuf,

        /// Preview changes without executing (shows what would be modified)
        #[arg(long = "dry", id = "dry_run")]
        dry_run: bool,

        /// Only update Claude history, don't move the actual directory
        /// Use this if you already moved the directory manually
        #[arg(long)]
        history_only: bool,

        /// Step-by-step confirmation mode - confirm each action before executing
        /// Useful for debugging or when you want to be extra careful
        #[arg(long)]
        anxious: bool,

        /// Force move even if active Claude terminals exist (they will break)
        #[arg(long)]
        force: bool,
    },

    /// Manage saved workspace sets (WSet)
    ///
    /// Workspace sets capture all Claude windows and their positions across workspaces.
    /// Use `babel wset save` and `babel wset load` to manage layouts.
    #[command(alias = "ws")]
    Wset {
        #[command(subcommand)]
        command: WSetCommands,
    },

    // ─── System ─────────────────────────────────────────────────────────────────

    /// Start the babel daemon (babeld)
    Daemon {
        /// Enable verbose trace logging
        #[arg(long)]
        trace: bool,
    },
}

/// WSet management subcommands
#[derive(Subcommand)]
enum WSetCommands {
    /// Save current workspace layout
    ///
    /// Captures all Claude windows and their positions across workspaces.
    /// WSet files are stored in ~/.config/claude-babel/wsets/
    #[command(alias = "s")]
    Save {
        /// Name for the WSet (defaults to "default")
        name: Option<String>,

        /// Overwrite existing WSet without confirmation
        #[arg(long)]
        force: bool,
    },

    /// Load a workspace layout
    ///
    /// Spawns Claude windows from the saved state.
    /// Sessions that no longer exist in ~/.claude are skipped.
    #[command(alias = "l")]
    Load {
        /// Name of WSet to load (defaults to "default")
        name: Option<String>,

        /// Show what would happen without executing
        #[arg(long = "dry")]
        dry_run: bool,

        /// Step-by-step confirmation mode
        #[arg(long)]
        anxious: bool,
    },

    /// List all saved WSet files
    #[command(alias = "ls")]
    List,

    /// Rename a WSet
    Rename {
        /// Current name
        old: String,
        /// New name
        new: String,
    },

    /// Delete a WSet
    #[command(alias = "rm")]
    Delete {
        /// Name of WSet to delete
        name: String,
    },

    /// Set or show description for a WSet
    Describe {
        /// WSet name
        name: String,
        /// Description (omit to show current)
        description: Option<String>,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    // Parse CLI first to get --debug flag before logging init
    let cli = Cli::parse();

    // Initialize logging via spaceship-std (centralized config + SIGHUP hot-reload)
    // Config: ~/Workspace/logging.toml | Logs: journalctl -t babel -f
    // "babel" = config key and journald identifier, "claude_babel" = Rust crate for filtering
    // --debug flag forces debug level regardless of config
    spaceship_std::logging::init("babel", "claude_babel", &cli.logging);

    if cli.logging.debug {
        tracing::debug!("debug logging enabled via --debug flag");
    }

    match cli.command {
        // Daemon management commands - always direct
        Commands::Daemon { trace } => {
            if trace {
                run_daemon_traced().await
            } else {
                run_daemon().await
            }
        }
        Commands::UpdateTitles { refresh, workspace } => cmd_update_titles(cli.json, refresh, workspace).await,

        // Data commands - use daemon if available
        Commands::Ls { details } => cmd_list(cli.json, details).await,
        Commands::LsTerminals => cmd_ls_terminals(cli.json).await,
        Commands::LsPanes => cmd_ls_panes(cli.json).await,
        Commands::CheckWindow { window_id } => cmd_check_window(window_id, cli.json).await,
        Commands::CheckPane { pane_name } => cmd_check_pane(pane_name, cli.json).await,
        Commands::History { sessions, limit, all } => cmd_history(sessions, limit, all, cli.json).await,

        // Action commands - use daemon if available
        Commands::Focus { window_id } => cmd_focus(window_id).await,
        Commands::GetScrollback { window_id, lines } => cmd_get_scrollback(window_id, lines).await,
        Commands::Send { window_id, text } => cmd_send(window_id, &text).await,
        Commands::SetIcon { window_id, icon } => cmd_set_icon(window_id, &icon).await,
        Commands::MarkRead { window_id } => cmd_mark_read(window_id).await,

        // Migration commands - direct only (no daemon needed)
        Commands::Mv { source, dest, dry_run, history_only, anxious, force } => {
            cmd_mv(source, dest, dry_run, history_only, anxious, force, cli.json).await
        }

        // Diagnostic commands
        Commands::Fingerprint { input, window, dir, session } => {
            cmd_fingerprint(input, window, dir, session, cli.json).await
        }

        // WSet commands
        Commands::Wset { command } => cmd_wset(command, cli.json).await,
    }
}

// ═══════════════════════════════════════════════════════════════════════════════
// Daemon Commands
// ═══════════════════════════════════════════════════════════════════════════════

async fn cmd_update_titles(json: bool, refresh: bool, workspace: Option<i32>) -> Result<()> {
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

/// List all kitty panes grouped by OS window
async fn cmd_ls_panes(json: bool) -> Result<()> {
    use claude_babel::kitty::list_windows;
    use std::collections::HashMap;

    let windows = list_windows().context("Failed to list kitty windows")?;

    if json {
        println!("{}", serde_json::to_string_pretty(&windows)?);
        return Ok(());
    }

    if windows.is_empty() {
        println!("No kitty panes found");
        return Ok(());
    }

    // Group by OS window
    let mut by_os_window: HashMap<u64, Vec<_>> = HashMap::new();
    for win in windows {
        by_os_window.entry(win.os_window_id).or_default().push(win);
    }

    let total_panes: usize = by_os_window.values().map(|v| v.len()).sum();
    println!("Kitty panes ({} panes in {} OS windows):", total_panes, by_os_window.len());
    println!();

    for (os_id, panes) in by_os_window.iter() {
        println!("  OS Window {} ({} panes):", os_id, panes.len());
        for win in panes {
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

async fn cmd_check_window(window_id: Option<u64>, json: bool) -> Result<()> {
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
                println!();
                show_available_windows().await?;
            } else {
                println!("No focused Claude window found");
            }
        }
    }

    Ok(())
}

async fn cmd_check_pane(pane_name: Option<String>, _json: bool) -> Result<()> {
    // TODO: Implement once richspace-babel pane querying is available
    match pane_name {
        Some(name) => {
            println!("Pane '{}' not found (richspace-babel integration pending)", name);
        }
        None => {
            println!("No pane name specified");
            println!();
            println!("Usage: babel check-pane <PANE_NAME>");
        }
    }
    Ok(())
}

async fn cmd_history(sessions: Vec<String>, limit: usize, all: bool, json: bool) -> Result<()> {
    // Determine effective limit
    let effective_limit = if all { usize::MAX } else { limit };

    // If specific sessions requested, filter to those
    let results = if !sessions.is_empty() {
        // TODO: Implement session ID filtering via daemon or direct lookup
        // For now, filter from full history
        let all_sessions = get_history(usize::MAX).await?;
        all_sessions
            .into_iter()
            .filter(|s| sessions.iter().any(|id| s.session_id.contains(id)))
            .collect()
    } else {
        get_history(effective_limit).await?
    };

    if json {
        println!("{}", serde_json::to_string_pretty(&results)?);
        return Ok(());
    }

    if results.is_empty() {
        if !sessions.is_empty() {
            println!("No sessions found matching: {}", sessions.join(", "));
        } else {
            println!("No conversation history found");
        }
        return Ok(());
    }

    if sessions.is_empty() {
        println!("Recent conversations:");
    } else {
        println!("Matching sessions:");
    }
    println!();

    for session in &results {
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

async fn cmd_get_scrollback(window_id: u64, lines: Option<usize>) -> Result<()> {
    // Try daemon first
    if let Ok(Response::Scrollback { text }) = send_request(&Request::Scroll { window_id }).await {
        let output = if let Some(n) = lines {
            text.lines().take(n).collect::<Vec<_>>().join("\n")
        } else {
            text
        };
        print!("{}", output);
        return Ok(());
    }

    // Direct fallback
    let scrollback = get_scrollback(window_id).context("Failed to get scrollback")?;
    let output = if let Some(n) = lines {
        scrollback.lines().take(n).collect::<Vec<_>>().join("\n")
    } else {
        scrollback
    };
    print!("{}", output);
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

async fn cmd_set_icon(window_id: u64, icon: &str) -> Result<()> {
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

    println!("Set icon for window {}: {}", window_id, icon);
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
// Migration Commands
// ═══════════════════════════════════════════════════════════════════════════════

use claude_babel::state::{SessionState, detect_state};

/// A Claude terminal that would be affected by a directory move
#[derive(Debug)]
struct ConflictingWindow {
    window: ClaudeWindow,
    state: SessionState,
    /// Path relative to source directory (for calculating new cwd)
    relative_path: PathBuf,
}

/// Find all Claude windows whose cwd is inside the source directory
///
/// These windows will break after the move unless migrated. Returns windows
/// partitioned by state - idle ones can be migrated, active ones block the move.
fn find_conflicting_windows(source: &Path) -> Result<Vec<ConflictingWindow>> {
    // Canonicalize to absolute path - kitty's cwd is always absolute, but user may
    // pass relative path like "." or "foo". Without this, starts_with() always fails.
    let source = source.canonicalize()
        .unwrap_or_else(|_| std::env::current_dir().unwrap().join(source));

    let windows = discover_claude_windows()?;
    let mut conflicts = Vec::new();

    for win in windows {
        // Check if window's cwd is inside (or equal to) source path
        if win.cwd.starts_with(&source) {
            // Get window state from scrollback
            let state = match get_scrollback(win.kitty_id) {
                Ok(scrollback) => detect_state(&scrollback),
                Err(_) => SessionState::Unknown,
            };

            // Calculate relative path for new cwd computation
            let relative_path = win.cwd
                .strip_prefix(&source)
                .unwrap_or(Path::new(""))
                .to_path_buf();

            conflicts.push(ConflictingWindow {
                window: win,
                state,
                relative_path,
            });
        }
    }

    Ok(conflicts)
}

/// Migrate an idle terminal to the new directory
///
/// Sends: Ctrl-C (ensure clean prompt) → cd <new_path> → claude -r <session_id>
/// This allows the terminal to continue working seamlessly after the directory move.
fn migrate_terminal(
    window: &ClaudeWindow,
    new_cwd: &Path,
    anxious: bool,
) -> Result<()> {
    use std::thread::sleep;
    use std::time::Duration;

    if anxious {
        println!("  → Sending Ctrl-C...");
    }
    send_text(window.kitty_id, "\x03")?; // Ctrl-C
    sleep(Duration::from_millis(100));

    if anxious {
        println!("  → cd {}", new_cwd.display());
    }
    send_text(window.kitty_id, &format!("cd {}\n", shell_escape(new_cwd)))?;
    sleep(Duration::from_millis(50));

    // Resume session if we know the session ID
    if let Some(ref session_id) = window.session_id {
        if anxious {
            println!("  → claude -r {}...", &session_id[..session_id.len().min(8)]);
        }
        send_text(window.kitty_id, &format!("claude -r {}\n", session_id))?;
    }

    Ok(())
}

/// Escape a path for safe shell usage
fn shell_escape(path: &Path) -> String {
    let s = path.to_string_lossy();
    // If path contains special chars, quote it
    if s.contains(' ') || s.contains('\'') || s.contains('"') || s.contains('$') {
        format!("'{}'", s.replace('\'', "'\\''"))
    } else {
        s.to_string()
    }
}

/// Move a directory while preserving Claude conversation history
///
/// This command handles two cases:
/// 1. Full move: Move the directory AND update Claude's history
/// 2. History-only: Just update history (if directory was already moved manually)
///
/// With --anxious, each step requires explicit y/n confirmation before proceeding.
/// With --force, move proceeds even if active Claude terminals would break.
async fn cmd_mv(
    source: PathBuf,
    dest: PathBuf,
    dry_run: bool,
    history_only: bool,
    anxious: bool,
    force: bool,
    json: bool,
) -> Result<()> {
    use std::fs;
    use claude_babel::fingerprint::path_to_encoded;

    // Expand ~ in paths
    let source = expand_tilde(&source);
    let dest = expand_tilde(&dest);

    // Validate source exists (unless history-only mode allows missing source)
    let source_exists = source.exists();
    if !source_exists && !history_only {
        bail!("Source directory does not exist: {}\n\
               If you already moved the directory, use --history-only",
            source.display());
    }

    // ─────────────────────────────────────────────────────────────────────────────
    // Step 0: Check for conflicting terminals
    // ─────────────────────────────────────────────────────────────────────────────
    let conflicts = find_conflicting_windows(&source)?;

    if !conflicts.is_empty() {
        // Partition by state: idle/awaiting can be migrated, thinking/tool_use are active
        let (migratable, active): (Vec<_>, Vec<_>) = conflicts.iter().partition(|c| {
            matches!(c.state, SessionState::Idle | SessionState::AwaitingInput)
        });

        // Display conflicts
        println!("Found {} Claude window(s) in {}:\n", conflicts.len(), source.display());

        for c in &conflicts {
            let state_str = match c.state {
                SessionState::Idle => "[IDLE]  ",
                SessionState::AwaitingInput => "[AWAIT] ",
                SessionState::Thinking => "[ACTIVE: Thinking]",
                SessionState::ToolUse => "[ACTIVE: Tool Use]",
                SessionState::Unknown => "[UNKNOWN]",
            };
            let title = c.window.title.strip_prefix("✳ ").unwrap_or(&c.window.title);
            let title_short: String = title.chars().take(40).collect();
            println!("  {} id:{:<4} \"{}\"", state_str, c.window.kitty_id, title_short);
        }
        println!();

        // Handle active terminals
        if !active.is_empty() {
            if force {
                println!("⚠ Warning: {} active terminal(s) will break after move.", active.len());
                println!("  Proceeding anyway due to --force flag.\n");
            } else {
                println!("{} active window(s) detected - cannot safely migrate.", active.len());
                println!();
                println!("Options:");
                println!("  • Wait for active sessions to finish and retry");
                println!("  • Use --force to move anyway (active sessions will break)");
                bail!("Active Claude terminals in source path");
            }
        }

        // Migrate idle terminals (before the actual move)
        if !migratable.is_empty() {
            if dry_run {
                println!("[DRY RUN] Would migrate {} idle terminal(s):\n", migratable.len());
                for c in &migratable {
                    let new_cwd = dest.join(&c.relative_path);
                    println!("  id:{} → cd {}", c.window.kitty_id, new_cwd.display());
                    if let Some(ref sid) = c.window.session_id {
                        println!("         → claude -r {}", &sid[..sid.len().min(8)]);
                    }
                }
                println!();
            } else {
                println!("{} idle terminal(s) will be migrated.\n", migratable.len());

                for c in &migratable {
                    let new_cwd = dest.join(&c.relative_path);

                    if anxious {
                        println!("Migrate terminal id:{}?", c.window.kitty_id);
                        println!("  cwd: {} → {}", c.window.cwd.display(), new_cwd.display());
                        if !confirm("Proceed?")? {
                            println!("  ⊘ Skipped\n");
                            continue;
                        }
                    }

                    migrate_terminal(&c.window, &new_cwd, anxious)?;

                    if anxious {
                        println!("  ✓ Terminal migrated\n");
                    } else {
                        println!("  Migrated id:{} → {}", c.window.kitty_id, new_cwd.display());
                    }
                }
                println!();
            }
        }
    }

    // Pre-compute paths for anxious mode preview
    let old_encoded = path_to_encoded(&source);
    let new_encoded = path_to_encoded(&dest);
    let claude_base = dirs::home_dir().unwrap().join(".claude");
    let old_project_dir = claude_base.join("projects").join(&old_encoded);

    // Anxious mode: show full plan upfront
    if anxious {
        println!("=== Step-by-step confirmation ===\n");
        println!("Plan:");
        let mut step = 1;
        if !history_only && source_exists {
            println!("  {}. Move directory: {} → {}", step, source.display(), dest.display());
            step += 1;
        }
        if old_project_dir.exists() {
            let session_count = fs::read_dir(&old_project_dir)
                .map(|e| e.filter_map(|e| e.ok())
                    .filter(|e| e.path().extension().and_then(|s| s.to_str()) == Some("jsonl"))
                    .count())
                .unwrap_or(0);
            println!("  {}. Rename Claude project folder:", step);
            println!("     {} → {}", old_encoded, new_encoded);
            println!("     ({} session files)", session_count);
            step += 1;
        } else {
            println!("  {}. (skip) No Claude project folder at: {}", step, old_encoded);
            step += 1;
        }
        println!("  {}. Update path references in ~/.claude/history.jsonl", step);
        println!();
    }

    // Step 1: Move the actual directory (unless --history-only or --dry-run)
    if !history_only && source_exists {
        if dry_run {
            println!("[DRY RUN] Would move directory:");
            println!("  {} → {}", source.display(), dest.display());
        } else {
            if anxious {
                println!("Step 1: Move directory");
                println!("  FROM: {}", source.display());
                println!("  TO:   {}", dest.display());
                if !confirm("Proceed with directory move?")? {
                    bail!("Aborted by user at step 1 (directory move)");
                }
            }

            // Try rename first (same filesystem)
            if let Err(_) = fs::rename(&source, &dest) {
                if anxious {
                    println!("  (rename failed, will copy+delete for cross-filesystem move)");
                    if !confirm("Proceed with copy+delete?")? {
                        bail!("Aborted by user at step 1 (cross-filesystem copy)");
                    }
                }
                // Fall back to copy + delete for cross-filesystem moves
                copy_dir_recursive(&source, &dest)
                    .with_context(|| format!(
                        "Failed to copy {} → {}",
                        source.display(), dest.display()
                    ))?;
                fs::remove_dir_all(&source)
                    .with_context(|| format!("Failed to remove source: {}", source.display()))?;
            }
            if anxious {
                println!("  ✓ Directory moved\n");
            } else {
                println!("Moved directory:");
                println!("  {} → {}", source.display(), dest.display());
            }
        }
        println!();
    }

    // Step 2: Migrate Claude's conversation history
    // Use the canonical paths for the project folders (what Claude sees)
    let old_project_path = if source_exists { &source } else { &source };
    let new_project_path = if dest.exists() { &dest } else { &dest };

    if anxious && old_project_dir.exists() {
        let session_count = fs::read_dir(&old_project_dir)
            .map(|e| e.filter_map(|e| e.ok())
                .filter(|e| e.path().extension().and_then(|s| s.to_str()) == Some("jsonl"))
                .count())
            .unwrap_or(0);
        println!("Step 2: Rename Claude project folder");
        println!("  FROM: ~/.claude/projects/{}", old_encoded);
        println!("  TO:   ~/.claude/projects/{}", new_encoded);
        println!("  Sessions: {} JSONL files", session_count);
        if !confirm("Proceed with project folder rename?")? {
            bail!("Aborted by user at step 2 (project folder rename)");
        }
    }

    if anxious {
        println!("Step 3: Update history.jsonl");
        println!("  File: ~/.claude/history.jsonl");
        println!("  Replace all occurrences of:");
        println!("    {} → {}", source.display(), dest.display());
        if !confirm("Proceed with history update?")? {
            bail!("Aborted by user at step 3 (history update)");
        }
    }

    println!("{}Updating Claude history...",
        if dry_run { "[DRY RUN] " } else { "" });

    let result = migrate_project(old_project_path, new_project_path, dry_run)?;

    if json {
        println!("{}", serde_json::to_string_pretty(&result)?);
        return Ok(());
    }

    // Display results
    if result.project_folder_renamed {
        if anxious {
            println!("  ✓ Renamed project folder ({} sessions)", result.sessions_preserved);
        } else {
            println!("  Renamed project folder:");
            println!("    {} → {}", result.old_folder, result.new_folder);
        }
    } else {
        println!("  No project folder found at: {}", result.old_folder);
    }

    if result.history_entries_updated > 0 {
        if anxious {
            println!("  ✓ Updated {} history entries", result.history_entries_updated);
        } else {
            println!("  Updated {} history entries", result.history_entries_updated);
        }
    } else {
        println!("  No history entries to update");
    }

    if result.sessions_preserved > 0 && !anxious {
        println!("  Preserved {} conversation sessions", result.sessions_preserved);
    }

    println!();
    if dry_run {
        println!("This was a dry run. No changes were made.");
    } else {
        println!("Done! Conversation history maintained.");
    }

    Ok(())
}

/// Ask user for y/n confirmation (for --anxious mode)
fn confirm(prompt: &str) -> Result<bool> {
    print!("{} [y/N] ", prompt);
    std::io::stdout().flush()?;

    let mut input = String::new();
    std::io::stdin().read_line(&mut input)?;

    let response = input.trim().to_lowercase();
    Ok(response == "y" || response == "yes")
}

/// Expand ~ to home directory
fn expand_tilde(path: &Path) -> PathBuf {
    if let Ok(stripped) = path.strip_prefix("~") {
        if let Some(home) = dirs::home_dir() {
            return home.join(stripped);
        }
    }
    path.to_path_buf()
}

/// Recursively copy a directory (for cross-filesystem moves)
fn copy_dir_recursive(src: &Path, dst: &Path) -> Result<()> {
    use std::fs;

    fs::create_dir_all(dst)?;

    for entry in fs::read_dir(src)? {
        let entry = entry?;
        let src_path = entry.path();
        let dst_path = dst.join(entry.file_name());

        if src_path.is_dir() {
            copy_dir_recursive(&src_path, &dst_path)?;
        } else {
            fs::copy(&src_path, &dst_path)?;
        }
    }

    Ok(())
}

// ═══════════════════════════════════════════════════════════════════════════════
// Diagnostic Commands
// ═══════════════════════════════════════════════════════════════════════════════

use claude_babel::fingerprint::{extract_from_scrollback, extract_from_jsonl, match_fingerprints, MatchConfidence};
use claude_babel::claude_storage::list_sessions;

/// Debug fingerprint linkage between terminals, sessions, and directories
///
/// Input is auto-detected or forced via flags. Shows the full trace of fingerprint data
/// and cross-matching between terminals and session files.
async fn cmd_fingerprint(
    input: Option<String>,
    force_window: bool,
    force_dir: bool,
    force_session: bool,
    json: bool,
) -> Result<()> {
    use claude_babel::fingerprint::path_to_encoded;
    use console::style;

    // Require input
    let input = match input {
        Some(i) => i,
        None => {
            bail!("Input required: window ID, directory path, or session ID.\n\
                   Examples:\n\
                     babel fingerprint 42        # Window ID\n\
                     babel fingerprint .         # Current directory\n\
                     babel fingerprint abc123    # Session ID");
        }
    };

    // Determine input type via flags or auto-detection
    enum InputType {
        Window(u64),
        Directory(PathBuf),
        Session(String),
    }

    let input_type = if force_window {
        let id = input.parse::<u64>()
            .context("--window flag set but input is not a valid window ID")?;
        InputType::Window(id)
    } else if force_dir {
        InputType::Directory(PathBuf::from(&input))
    } else if force_session {
        InputType::Session(input.clone())
    } else {
        // Auto-detect based on content
        if input.chars().all(|c| c.is_ascii_digit()) {
            // Pure digits = window ID
            InputType::Window(input.parse().unwrap())
        } else if input.starts_with('/') || input.starts_with('.') || input.contains('/') {
            // Path-like = directory
            InputType::Directory(PathBuf::from(&input))
        } else {
            // Otherwise = session ID
            InputType::Session(input.clone())
        }
    };

    // Convert to the old parameter style for compatibility with existing logic
    let (window, dir, session): (Option<u64>, Option<PathBuf>, Option<String>) = match input_type {
        InputType::Window(id) => (Some(id), None, None),
        InputType::Directory(path) => (None, Some(path), None),
        InputType::Session(id) => (None, None, Some(id)),
    };

    // Resolve directory to absolute path
    let scope_dir = dir.map(|d| {
        d.canonicalize().unwrap_or_else(|_| std::env::current_dir().unwrap().join(&d))
    });

    println!();
    println!("{}", style("═══════════════════════════════════════════════════════════════════════════════").cyan());
    println!("{}", style("FINGERPRINT TRACE").cyan().bold());
    println!("{}", style("═══════════════════════════════════════════════════════════════════════════════").cyan());
    println!();

    // Show input
    if let Some(ref d) = scope_dir {
        println!("Input: directory {}", d.display());
    }
    if let Some(w) = window {
        println!("Input: window id:{}", w);
    }
    if let Some(ref s) = session {
        println!("Input: session {}", s);
    }
    println!();

    // ─── Terminals ─────────────────────────────────────────────────────────────
    let terminals = discover_claude_windows()?;
    let filtered_terminals: Vec<_> = terminals.iter().filter(|w| {
        // Filter by window ID if specified
        if let Some(wid) = window {
            if w.kitty_id != wid {
                return false;
            }
        }
        // Filter by scope directory if specified
        if let Some(ref d) = scope_dir {
            if !w.cwd.starts_with(d) {
                return false;
            }
        }
        true
    }).collect();

    println!("{}", style("─── Terminals ────────────────────────────────────────────────────────────────").dim());

    if filtered_terminals.is_empty() {
        println!("  (no matching terminals found)");
    }

    // Collect terminal fingerprints for cross-matching
    let mut terminal_fps: Vec<(u64, claude_babel::fingerprint::SessionFingerprint)> = Vec::new();

    for win in &filtered_terminals {
        let in_scope = scope_dir.as_ref().map(|d| win.cwd.starts_with(d)).unwrap_or(true);
        let scope_marker = if in_scope { style("✓ IN SCOPE").green() } else { style("✗ not in scope").dim() };

        println!("  id:{:<4}  cwd: {}  {}", win.kitty_id, win.cwd.display(), scope_marker);

        let title = win.title.strip_prefix("✳ ").unwrap_or(&win.title);
        println!("         title: \"{}\"", title);

        if let Some(ref sid) = win.session_id {
            println!("         tagged session: {}", style(sid).cyan());
        } else {
            println!("         tagged session: {}", style("(none)").dim());
        }

        // Extract fingerprint from scrollback
        match get_scrollback(win.kitty_id) {
            Ok(scrollback) => {
                let fp = extract_from_scrollback(&scrollback);
                terminal_fps.push((win.kitty_id, fp.clone()));

                println!();
                println!("         {}", style("Scrollback Fingerprint:").yellow());
                if let Some(ref first) = fp.first_prompt {
                    let display = if first.len() > 50 { format!("{}...", &first[..50]) } else { first.clone() };
                    println!("           first_prompt: \"{}\"", display);
                } else {
                    println!("           first_prompt: {}", style("(none)").dim());
                }
                println!("           recent_prompts: {:?}", fp.recent_prompts);
                println!("           tool_sequence: {:?}", fp.tool_sequence);
            }
            Err(e) => {
                println!("         {}: {}", style("scrollback error").red(), e);
            }
        }
        println!();
    }

    // ─── Sessions ──────────────────────────────────────────────────────────────
    println!("{}", style("─── Sessions ─────────────────────────────────────────────────────────────────").dim());

    // Determine which project directories to scan
    let claude_base = dirs::home_dir().unwrap().join(".claude").join("projects");
    let project_dirs: Vec<PathBuf> = if let Some(ref d) = scope_dir {
        // Specific directory - find its project folder
        let encoded = path_to_encoded(d);
        let project_dir = claude_base.join(&encoded);
        if project_dir.exists() {
            vec![project_dir]
        } else {
            println!("  Project: {}", encoded);
            println!("    {}", style("(no project folder found)").dim());
            vec![]
        }
    } else if let Some(ref s) = session {
        // Session ID specified - search all projects
        let mut found = Vec::new();
        if let Ok(entries) = std::fs::read_dir(&claude_base) {
            for entry in entries.filter_map(|e| e.ok()) {
                let sessions_path = entry.path();
                if sessions_path.is_dir() {
                    let session_file = sessions_path.join(format!("{}.jsonl", s));
                    if session_file.exists() {
                        found.push(sessions_path);
                        break;
                    }
                }
            }
        }
        found
    } else {
        vec![]
    };

    // Collect session fingerprints for cross-matching
    let mut session_fps: Vec<(String, PathBuf, claude_babel::fingerprint::SessionFingerprint)> = Vec::new();

    for project_dir in &project_dirs {
        let project_name = project_dir.file_name().unwrap_or_default().to_string_lossy();
        println!("  Project: {}", project_name);
        println!();

        let sessions = list_sessions(project_dir)?;

        // Filter sessions if specific one requested
        let sessions: Vec<_> = sessions.into_iter().filter(|path| {
            if let Some(ref s) = session {
                path.file_stem().map(|fs| fs.to_string_lossy().contains(s)).unwrap_or(false)
            } else {
                true
            }
        }).collect();

        if sessions.is_empty() {
            println!("    {}", style("(no matching sessions)").dim());
            continue;
        }

        for session_path in sessions {
            let session_name = session_path.file_name().unwrap_or_default().to_string_lossy();
            let session_id = session_path.file_stem().unwrap_or_default().to_string_lossy().to_string();

            // Get file metadata
            let metadata = std::fs::metadata(&session_path).ok();
            let modified = metadata.and_then(|m| m.modified().ok())
                .map(|t| {
                    let elapsed = t.elapsed().unwrap_or_default();
                    if elapsed.as_secs() < 60 { format!("{}s ago", elapsed.as_secs()) }
                    else if elapsed.as_secs() < 3600 { format!("{}m ago", elapsed.as_secs() / 60) }
                    else if elapsed.as_secs() < 86400 { format!("{}h ago", elapsed.as_secs() / 3600) }
                    else { format!("{}d ago", elapsed.as_secs() / 86400) }
                })
                .unwrap_or_else(|| "?".to_string());

            println!("  {}  ({})", style(&session_name).cyan(), modified);

            // Extract fingerprint from JSONL
            match extract_from_jsonl(&session_path) {
                Ok(fp) => {
                    session_fps.push((session_id.clone(), session_path.clone(), fp.clone()));

                    if let Some(ref first) = fp.first_prompt {
                        let display = if first.len() > 50 { format!("{}...", &first[..50]) } else { first.clone() };
                        println!("    first_prompt: \"{}\"", display);
                    }
                    println!("    tool_sequence: {:?}", fp.tool_sequence);

                    // Cross-match with terminals
                    for (term_id, term_fp) in &terminal_fps {
                        let confidence = match_fingerprints(term_fp, &fp);
                        if confidence != MatchConfidence::None {
                            println!();
                            println!("    {} id:{}: {} (confidence: {:?})",
                                style("Match vs").yellow(), term_id, format_confidence(confidence), confidence);

                            // Show match details
                            if term_fp.first_prompt == fp.first_prompt && term_fp.first_prompt.is_some() {
                                println!("      {} first_prompt match (+2)", style("✓").green());
                            }
                            let overlap: Vec<_> = term_fp.recent_prompts.iter()
                                .filter(|p| fp.recent_prompts.contains(p))
                                .collect();
                            if !overlap.is_empty() {
                                println!("      {} recent_prompts overlap (+1)", style("✓").green());
                            }
                            // Tool overlap
                            let term_tools: std::collections::HashSet<_> = term_fp.tool_sequence.iter().collect();
                            let session_tools: std::collections::HashSet<_> = fp.tool_sequence.iter().collect();
                            let intersection = term_tools.intersection(&session_tools).count();
                            let union = term_tools.union(&session_tools).count();
                            if union > 0 && (intersection as f64 / union as f64) > 0.5 {
                                println!("      {} tool_sequence similarity (+1)", style("✓").green());
                            }
                        }
                    }
                }
                Err(e) => {
                    println!("    {}: {}", style("parse error").red(), e);
                }
            }
            println!();
        }
    }

    // ─── Linkage Summary ───────────────────────────────────────────────────────
    println!("{}", style("─── Linkage ──────────────────────────────────────────────────────────────────").dim());

    let mut found_links = false;
    for (term_id, term_fp) in &terminal_fps {
        let mut best_match: Option<(&str, MatchConfidence)> = None;

        for (session_id, _, session_fp) in &session_fps {
            let confidence = match_fingerprints(term_fp, session_fp);
            if confidence >= MatchConfidence::Medium {
                if best_match.map(|(_, c)| confidence > c).unwrap_or(true) {
                    best_match = Some((session_id.as_str(), confidence));
                }
            }
        }

        if let Some((sid, conf)) = best_match {
            found_links = true;
            println!("  Terminal id:{} ←─[{:?}]─→ Session {}", term_id, conf, sid);
        }
    }

    if !found_links && !terminal_fps.is_empty() && !session_fps.is_empty() {
        println!("  {}", style("No strong matches found").yellow());
        println!();
        println!("  Recommendation: Check that the terminal has exchanged at least one message");
        println!("                  with Claude (fingerprinting needs prompt content).");
    } else if terminal_fps.is_empty() && session_fps.is_empty() {
        println!("  {}", style("No data to link").dim());
    }

    println!();
    println!("{}", style("═══════════════════════════════════════════════════════════════════════════════").cyan());
    println!();

    // JSON output
    if json {
        let output = serde_json::json!({
            "terminals": terminal_fps.iter().map(|(id, fp)| {
                serde_json::json!({
                    "kitty_id": id,
                    "first_prompt": fp.first_prompt,
                    "recent_prompts": fp.recent_prompts,
                    "tool_sequence": fp.tool_sequence,
                })
            }).collect::<Vec<_>>(),
            "sessions": session_fps.iter().map(|(id, path, fp)| {
                serde_json::json!({
                    "session_id": id,
                    "path": path,
                    "first_prompt": fp.first_prompt,
                    "tool_sequence": fp.tool_sequence,
                })
            }).collect::<Vec<_>>(),
        });
        println!("{}", serde_json::to_string_pretty(&output)?);
    }

    Ok(())
}

/// Format confidence level for display
fn format_confidence(conf: MatchConfidence) -> &'static str {
    match conf {
        MatchConfidence::None => "NONE",
        MatchConfidence::Low => "LOW",
        MatchConfidence::Medium => "MEDIUM",
        MatchConfidence::High => "HIGH",
        MatchConfidence::Exact => "EXACT",
    }
}

// ═══════════════════════════════════════════════════════════════════════════════
// UX Helpers
// ═══════════════════════════════════════════════════════════════════════════════

/// Show available Claude windows when user provides invalid input
async fn show_available_windows() -> Result<()> {
    let windows = get_windows().await?;

    if windows.is_empty() {
        println!("No Claude windows found");
        return Ok(());
    }

    println!("Available Claude windows:");
    for w in &windows {
        let title = w.title.strip_prefix("✳ ").unwrap_or(&w.title);
        let title: String = title.chars().take(30).collect();
        println!("  {:>5}  {:30}  {}", w.kitty_id, title, w.cwd.display());
    }
    Ok(())
}

// ═══════════════════════════════════════════════════════════════════════════════
// WSet Commands
// ═══════════════════════════════════════════════════════════════════════════════

async fn cmd_wset_save(name: Option<String>, _force: bool, json: bool) -> Result<()> {
    let request = Request::WSetSave { name };

    match send_request(&request).await {
        Ok(Response::WSetSaved { name, wspaces, windows }) => {
            if json {
                println!("{}", serde_json::to_string_pretty(&serde_json::json!({
                    "status": "saved",
                    "name": name,
                    "wspaces": wspaces,
                    "windows": windows,
                }))?);
            } else {
                println!("Saved WSet '{}': {} wspaces, {} windows", name, wspaces, windows);
            }
            Ok(())
        }
        Ok(Response::Error { message }) => {
            eprintln!("error: {}", message);
            std::process::exit(1);
        }
        Ok(other) => {
            eprintln!("error: unexpected response from daemon: {:?}", other);
            std::process::exit(1);
        }
        Err(e) => {
            eprintln!("error: daemon not running or connection failed: {}", e);
            std::process::exit(1);
        }
    }
}

async fn cmd_wset_load(name: Option<String>, dry_run: bool, _anxious: bool, json: bool) -> Result<()> {
    // First, get a dry-run preview
    let preview_request = Request::WSetLoad { name: name.clone(), dry_run: true };

    let (wset_name, wspaces, windows) = match send_request(&preview_request).await {
        Ok(Response::WSetLoaded { name, wspaces, windows, .. }) => (name, wspaces, windows),
        Ok(Response::Error { message }) => {
            eprintln!("error: {}", message);
            std::process::exit(1);
        }
        Ok(other) => {
            eprintln!("error: unexpected response from daemon: {:?}", other);
            std::process::exit(1);
        }
        Err(e) => {
            eprintln!("error: daemon not running or connection failed: {}", e);
            std::process::exit(1);
        }
    };

    // Get current state for comparison
    let current_windows = match send_request(&Request::List).await {
        Ok(Response::Windows { windows }) => windows.len(),
        Ok(Response::Error { message }) => {
            eprintln!("error: failed to get window list: {}", message);
            std::process::exit(1);
        }
        Ok(other) => {
            eprintln!("error: unexpected response when getting window list: {:?}", other);
            std::process::exit(1);
        }
        Err(e) => {
            eprintln!("error: daemon not running or connection failed: {}", e);
            std::process::exit(1);
        }
    };

    let current_name = match send_request(&Request::WSetCurrent).await {
        Ok(Response::WSetCurrent { name }) => name.unwrap_or_else(|| "(none)".to_string()),
        Ok(Response::Error { message }) => {
            eprintln!("error: failed to get current wset: {}", message);
            std::process::exit(1);
        }
        Ok(other) => {
            eprintln!("error: unexpected response when getting current wset: {:?}", other);
            std::process::exit(1);
        }
        Err(e) => {
            eprintln!("error: daemon not running or connection failed: {}", e);
            std::process::exit(1);
        }
    };

    if dry_run {
        // Just show preview
        if json {
            println!("{}", serde_json::to_string_pretty(&serde_json::json!({
                "status": "preview",
                "name": wset_name,
                "wspaces": wspaces,
                "windows": windows,
                "current_name": current_name,
                "current_windows": current_windows,
            }))?);
        } else {
            println!("Would load WSet '{}':", wset_name);
            println!("  {} wspaces, {} windows", wspaces, windows);
            println!("\nCurrent: {} ({} windows)", current_name, current_windows);
            if current_windows > 0 {
                println!("  {} windows would be closed", current_windows);
            }
        }
        return Ok(());
    }

    // Show confirmation box
    use console::style;

    println!();
    println!("{}", style("╭─ Load WSet ─────────────────────────────────────────╮").cyan());
    println!("{} {} wspaces {} {} sessions {}",
        style("│").cyan(),
        wspaces,
        style("•").dim(),
        windows,
        style("│").cyan()
    );
    println!("{}{}", style("│").cyan(), style("                                                     │").cyan());
    println!("{} Current: {} ({} windows) {}",
        style("│").cyan(),
        style(&current_name).yellow(),
        current_windows,
        style("│").cyan()
    );
    if current_windows > 0 {
        println!("{} {} {} windows will be closed {}",
            style("│").cyan(),
            style("⚠").yellow(),
            current_windows,
            style("│").cyan()
        );
    }
    println!("{}{}", style("│").cyan(), style("                                                     │").cyan());
    println!("{} {} confirm   {} abort {}",
        style("│").cyan(),
        style("[Enter]").green(),
        style("[Ctrl-C]").red(),
        style("│").cyan()
    );
    println!("{}", style("╰─────────────────────────────────────────────────────╯").cyan());
    println!();

    // Wait for Enter
    use std::io::{self, BufRead};
    print!("  ");
    io::stdout().flush().ok();
    let mut input = String::new();
    io::stdin().lock().read_line(&mut input)?;

    // Execute the load
    let request = Request::WSetLoad { name, dry_run: false };

    match send_request(&request).await {
        Ok(Response::WSetLoaded { name, wspaces, windows, skipped, dry_run: _ }) => {
            if json {
                println!("{}", serde_json::to_string_pretty(&serde_json::json!({
                    "status": "loaded",
                    "name": name,
                    "wspaces": wspaces,
                    "windows": windows,
                    "skipped": skipped,
                }))?);
            } else {
                let skipped_msg = if skipped.is_empty() {
                    String::new()
                } else {
                    format!(" ({} skipped)", skipped.len())
                };
                println!("Loaded WSet '{}': {} wspaces, {} windows{}",
                    name, wspaces, windows, skipped_msg);

                if !skipped.is_empty() {
                    println!("\nSkipped sessions (file not found):");
                    for s in &skipped {
                        println!("  - {}", s);
                    }
                }
            }
            Ok(())
        }
        Ok(Response::Error { message }) => {
            eprintln!("error: {}", message);
            std::process::exit(1);
        }
        Ok(other) => {
            eprintln!("error: unexpected response from daemon: {:?}", other);
            std::process::exit(1);
        }
        Err(e) => {
            eprintln!("error: daemon not running or connection failed: {}", e);
            std::process::exit(1);
        }
    }
}

async fn cmd_wset(command: WSetCommands, json: bool) -> Result<()> {
    match command {
        WSetCommands::Save { name, force } => {
            cmd_wset_save(name, force, json).await
        }

        WSetCommands::Load { name, dry_run, anxious } => {
            cmd_wset_load(name, dry_run, anxious, json).await
        }

        WSetCommands::List => {
            match send_request(&Request::WSetList).await {
                Ok(Response::WSetList { wsets, current }) => {
                    if json {
                        println!("{}", serde_json::to_string_pretty(&serde_json::json!({
                            "wsets": wsets,
                            "current": current,
                        }))?);
                    } else if wsets.is_empty() {
                        println!("No saved WSet files found");
                        println!("Run 'babel save [name]' to create one");
                    } else {
                        println!("Saved WSet files:");
                        println!();
                        for wset in &wsets {
                            let marker = if current.as_ref() == Some(&wset.name) { "* " } else { "  " };
                            let desc = wset.description.as_ref()
                                .map(|d| format!(" - {}", d))
                                .unwrap_or_default();
                            println!("{}{:<16} {} wspaces, {} windows{}",
                                marker, wset.name, wset.wspaces, wset.windows, desc);
                        }
                        if current.is_some() {
                            println!();
                            println!("  * = current");
                        }
                    }
                    Ok(())
                }
                Ok(Response::Error { message }) => {
                    eprintln!("error: {}", message);
                    std::process::exit(1);
                }
                Err(e) => {
                    eprintln!("error: daemon not running: {}", e);
                    std::process::exit(1);
                }
                Ok(other) => {
                    eprintln!("error: unexpected response from daemon: {:?}", other);
                    std::process::exit(1);
                }
            }
        }

        WSetCommands::Rename { old, new } => {
            match send_request(&Request::WSetRename { old: old.clone(), new: new.clone() }).await {
                Ok(Response::Ok { message }) => {
                    if json {
                        println!("{}", serde_json::to_string_pretty(&serde_json::json!({
                            "status": "renamed",
                            "old": old,
                            "new": new,
                        }))?);
                    } else {
                        println!("{}", message);
                    }
                    Ok(())
                }
                Ok(Response::Error { message }) => {
                    eprintln!("error: {}", message);
                    std::process::exit(1);
                }
                Err(e) => {
                    eprintln!("error: daemon not running: {}", e);
                    std::process::exit(1);
                }
                Ok(other) => {
                    eprintln!("error: unexpected response from daemon: {:?}", other);
                    std::process::exit(1);
                }
            }
        }

        WSetCommands::Delete { name } => {
            match send_request(&Request::WSetDelete { name: name.clone() }).await {
                Ok(Response::Ok { message }) => {
                    if json {
                        println!("{}", serde_json::to_string_pretty(&serde_json::json!({
                            "status": "deleted",
                            "name": name,
                        }))?);
                    } else {
                        println!("{}", message);
                    }
                    Ok(())
                }
                Ok(Response::Error { message }) => {
                    eprintln!("error: {}", message);
                    std::process::exit(1);
                }
                Err(e) => {
                    eprintln!("error: daemon not running: {}", e);
                    std::process::exit(1);
                }
                Ok(other) => {
                    eprintln!("error: unexpected response from daemon: {:?}", other);
                    std::process::exit(1);
                }
            }
        }

        WSetCommands::Describe { name, description } => {
            match send_request(&Request::WSetDescribe { name: name.clone(), description: description.clone() }).await {
                Ok(Response::Ok { message }) => {
                    if json {
                        println!("{}", serde_json::to_string_pretty(&serde_json::json!({
                            "status": "described",
                            "name": name,
                            "description": description,
                        }))?);
                    } else {
                        println!("{}", message);
                    }
                    Ok(())
                }
                Ok(Response::Error { message }) => {
                    eprintln!("error: {}", message);
                    std::process::exit(1);
                }
                Err(e) => {
                    eprintln!("error: daemon not running: {}", e);
                    std::process::exit(1);
                }
                Ok(other) => {
                    eprintln!("error: unexpected response from daemon: {:?}", other);
                    std::process::exit(1);
                }
            }
        }
    }
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
