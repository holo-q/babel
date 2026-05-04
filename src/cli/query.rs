//! Query commands - read-only operations that fetch data about agent sessions
//!
//! These commands never modify state and are safe to run anytime.
//! All commands support both daemon and direct mode, falling back automatically.

use std::collections::HashMap;

use anyhow::{Context, Result};
use console::{style, Style};
use tracing::instrument;

use super::Target;
use crate::cli::legend::Legend;
use babel::babel_storage::{get_generated_title, get_metadata, init_db};
use babel::core::BabelCore;
use babel::kitty::discover_all_instances;
use babel::service::state::TerminalInfo;
use babel::utility::agent_discovery::{detect_agent_signals, resolve_pane_title, AgentPane};
use babel::utility::claude_storage::{get_session_display_name, get_session_path, SessionInfo};
use babel::ActivityState;

// ═══════════════════════════════════════════════════════════════════════════════
// Core Query Commands
// ═══════════════════════════════════════════════════════════════════════════════

/// List all recognized agent sessions.
///
/// Always scans all kitty sockets on the system. Panes from non-current
/// sockets are displayed but fenced from operations that require the current socket.
#[tracing::instrument(level = "debug", skip(core))]
pub async fn cmd_ls(core: &BabelCore, json: bool, details: bool, all: bool) -> Result<()> {
    let mut windows = if details {
        // Use core method which properly handles daemon/local mode and multi-socket
        core.panes_with_fingerprints().await?
    } else {
        core.panes().await?
    };
    let mut terminals = core.terminals().await.unwrap_or_default();
    terminals.sort_by(|a, b| {
        let ws_a = a.workspace.unwrap_or(999);
        let ws_b = b.workspace.unwrap_or(999);
        ws_a.cmp(&ws_b)
            .then(a.platform_window_id.cmp(&b.platform_window_id))
            .then(a.id().cmp(&b.id()))
    });
    let unknown_terminal_count = terminals.iter().filter(|t| !t.is_agent).count();

    if json {
        if all {
            println!(
                "{}",
                serde_json::to_string_pretty(&serde_json::json!({
                    "agents": windows,
                    "terminals": terminals,
                    "unrecognized_terminal_count": unknown_terminal_count,
                }))?
            );
        } else {
            println!("{}", serde_json::to_string_pretty(&windows)?);
        }
        return Ok(());
    }

    if windows.is_empty() {
        println!("No agent sessions found");
        print_unknown_terminal_hint(unknown_terminal_count);
        if !all {
            return Ok(());
        }
    }

    // Sort by workspace, then by OS window ID for consistent grouping
    windows.sort_by(|a, b| {
        let ws_a = a.workspace.unwrap_or(999);
        let ws_b = b.workspace.unwrap_or(999);
        ws_a.cmp(&ws_b)
            .then(a.os_window_id.cmp(&b.os_window_id))
            .then(a.id().cmp(&b.id()))
    });

    // Group and display by workspace
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

    if all {
        print_unknown_terminals(&terminals, !windows.is_empty())?;
    } else {
        print_unknown_terminal_hint(unknown_terminal_count);
    }

    // Print legend showing what the symbols mean
    Legend::for_ls().print();

    Ok(())
}

fn print_unknown_terminal_hint(count: usize) {
    if count == 0 {
        return;
    }

    let dim = Style::new().dim();
    println!();
    println!(
        "{}",
        dim.apply_to(format!(
            "{} terminal{} could not be identified as agent sessions. These could be ordinary terminals or agents; pass --all to learn more.",
            count,
            if count == 1 { "" } else { "s" }
        ))
    );
}

fn print_unknown_terminals(terminals: &[TerminalInfo], separate: bool) -> Result<()> {
    let mut current_workspace: Option<i32> = None;
    let dim = Style::new().dim();
    let unknown: Vec<_> = terminals.iter().filter(|t| !t.is_agent).collect();

    if unknown.is_empty() {
        return Ok(());
    }

    if separate {
        println!();
    }
    println!("{}", dim.apply_to("─── Other Terminals ───"));

    for term in unknown {
        let ws = term.workspace.unwrap_or(-2);
        if current_workspace != Some(ws) {
            let ws_name = match ws {
                -1 => "Sticky".to_string(),
                -2 => "?".to_string(),
                n => format!("{}", n + 1),
            };
            println!("{}", dim.apply_to(format!("Workspace {}", ws_name)));
            current_workspace = Some(ws);
        }
        print_terminal(term);
    }

    Ok(())
}

fn print_terminal(term: &TerminalInfo) {
    let dim = Style::new().dim();
    let focus_indicator = if term.is_focused { "▸" } else { " " };
    let id_str = format!("{:>3}", term.id());
    let cwd_display = term
        .cwd
        .strip_prefix(dirs::home_dir().unwrap_or_default())
        .map(|p| format!("~/{}", p.display()))
        .unwrap_or_else(|_| term.cwd.display().to_string());
    let cwd_short = if cwd_display.len() > 30 {
        format!("…{}", &cwd_display[cwd_display.len() - 28..])
    } else {
        cwd_display
    };
    let command = term.foreground_command.as_deref().unwrap_or("?");
    let title = term.title.strip_prefix("✳ ").unwrap_or(&term.title);

    println!(
        " {} {} {}  {}  {}",
        focus_indicator,
        dim.apply_to(id_str),
        dim.apply_to(format!("{:<8}", command)),
        dim.apply_to(cwd_short),
        title
    );
}

/// List all kitty terminals (not just agent sessions)
///
/// Scans ALL kitty sockets on the system, showing terminals from all instances.
#[instrument(level = "debug", skip(_core))]
pub async fn cmd_ls_terminals(_core: &BabelCore, json: bool) -> Result<()> {
    let instances = discover_all_instances().await;

    if json {
        println!("{}", serde_json::to_string_pretty(&instances)?);
        return Ok(());
    }

    if instances.is_empty() {
        println!("No kitty sockets found");
        return Ok(());
    }

    let total_windows: usize = instances.iter().map(|i| i.panes.len()).sum();
    let responsive: usize = instances.iter().filter(|i| i.is_responsive).count();

    println!(
        "Kitty instances ({} sockets, {} responsive, {} total windows):",
        instances.len(),
        responsive,
        total_windows
    );
    println!();

    for instance in &instances {
        let status = if instance.is_current {
            "● current"
        } else if instance.is_responsive {
            "○ other"
        } else {
            "✗ dead"
        };

        let pid_str = instance
            .pid
            .map(|p| p.to_string())
            .unwrap_or_else(|| "?".to_string());

        println!(
            "  {} [PID {}] {} windows",
            status,
            pid_str,
            instance.panes.len()
        );

        if let Some(ref err) = instance.error {
            println!("    Error: {}", err);
            continue;
        }

        // Show windows for this instance
        for win in &instance.panes {
            let signals = detect_agent_signals(win);
            let cmdline = win
                .foreground_processes
                .first()
                .and_then(|p| p.cmdline.first())
                .map(|s| s.rsplit('/').next().unwrap_or(s))
                .unwrap_or("?");

            let title: String = win.title.chars().take(40).collect();
            let title = if win.title.len() > 40 {
                format!("{}…", title)
            } else {
                title
            };

            println!(
                "    {:>5} {} {:8}  {}",
                win.id,
                signals.indicator(),
                cmdline,
                title
            );
        }
        println!();
    }

    if instances.len() > 1 {
        println!("⚠ Multiple kitty instances detected - terminals may be unreachable!");
        println!("  Consider: pkill kitty && kitty (to consolidate)");
    }

    // Print legend explaining socket status symbols (only in human-readable mode)
    if !json {
        Legend::for_ls_terminals().print();
    }

    Ok(())
}

/// List all kitty panes grouped by socket and OS window
///
/// Now queries all responsive kitty sockets via the core (daemon or local mode).
#[instrument(level = "debug", skip(core))]
pub async fn cmd_ls_panes(core: &BabelCore, json: bool) -> Result<()> {
    let panes = core
        .kitty_panes()
        .await
        .context("Failed to list kitty panes")?;

    if json {
        println!("{}", serde_json::to_string_pretty(&panes)?);
        return Ok(());
    }

    if panes.is_empty() {
        println!("No kitty panes found");
        return Ok(());
    }

    // Group by socket, then by OS window
    let mut by_socket: HashMap<String, HashMap<u64, Vec<_>>> = HashMap::new();
    for pane in panes {
        by_socket
            .entry(pane.socket.clone())
            .or_default()
            .entry(pane.os_window_id)
            .or_default()
            .push(pane);
    }

    let total_panes: usize = by_socket
        .values()
        .flat_map(|s| s.values())
        .map(|v| v.len())
        .sum();
    let total_sockets = by_socket.len();
    let total_os_windows: usize = by_socket.values().map(|s| s.len()).sum();

    println!(
        "Kitty panes ({} panes in {} OS windows across {} socket{}):",
        total_panes,
        total_os_windows,
        total_sockets,
        if total_sockets == 1 { "" } else { "s" }
    );
    println!();

    let current_socket = babel::kitty::default_socket();
    for (socket, os_windows) in by_socket.iter() {
        // Show socket indicator: ● current, ○ other
        let is_current = socket == &current_socket;
        let marker = if is_current { "●" } else { "○" };
        let socket_short = socket.rsplit("kitty.sock-").next().unwrap_or(socket);
        println!(
            "  {} Socket {} ({} OS windows)",
            marker,
            socket_short,
            os_windows.len()
        );

        for (os_id, panes) in os_windows.iter() {
            println!("    OS Window {} ({} panes):", os_id, panes.len());
            for pane in panes {
                let title: String = pane.title.chars().take(45).collect();
                let title = if pane.title.len() > 45 {
                    format!("{}…", title)
                } else {
                    title
                };
                let focus = if pane.is_focused { "*" } else { " " };
                println!("      {:>5}{} {}", pane.id, focus, title);
            }
        }
    }

    Legend::for_ls_panes().print();

    Ok(())
}

/// List kitty sockets with status and windows
///
/// Socket-first view showing each kitty instance with its status,
/// and the agent panes running in that instance.
#[instrument(level = "debug", skip(core))]
pub async fn cmd_ls_sockets(core: &BabelCore, json: bool) -> Result<()> {
    let sockets = core.sockets().await.context("Failed to list sockets")?;
    let windows = core.panes().await.unwrap_or_default();

    if json {
        // Combine socket status with windows for JSON output
        let output: Vec<_> = sockets
            .iter()
            .map(|(socket, status)| {
                let socket_windows: Vec<_> =
                    windows.iter().filter(|w| w.socket() == socket).collect();
                serde_json::json!({
                    "socket": socket,
                    "status": status,
                    "windows": socket_windows,
                })
            })
            .collect();
        println!("{}", serde_json::to_string_pretty(&output)?);
        return Ok(());
    }

    if sockets.is_empty() {
        println!("No kitty sockets found");
        return Ok(());
    }

    let total_sockets = sockets.len();
    let responsive = sockets.values().filter(|s| s.is_responsive).count();
    let total_panes: usize = sockets.values().map(|s| s.pane_count).sum();
    let total_claude: usize = windows.len();

    println!(
        "Kitty sockets ({} socket{}, {} responsive, {} panes, {} agent panes):",
        total_sockets,
        if total_sockets == 1 { "" } else { "s" },
        responsive,
        total_panes,
        total_claude
    );
    println!();

    // Sort sockets: current first, then by PID
    let mut sorted: Vec<_> = sockets.iter().collect();
    sorted.sort_by(|a, b| {
        b.1.is_current
            .cmp(&a.1.is_current)
            .then_with(|| a.0.cmp(b.0))
    });

    let dim = Style::new().dim();
    let bold = Style::new().bold();
    let yellow = Style::new().yellow();
    let red = Style::new().red();

    for (socket, status) in sorted {
        // Socket header with status
        let marker = if status.is_current {
            style("●").green().to_string()
        } else if status.is_responsive {
            "○".to_string()
        } else {
            style("✗").red().to_string()
        };

        let socket_name = socket.rsplit('/').next().unwrap_or(socket);
        let status_label = if status.is_current {
            bold.apply_to("current").to_string()
        } else if status.is_responsive {
            "responsive".to_string()
        } else {
            red.apply_to("dead").to_string()
        };

        println!("{} {} ({})", marker, socket_name, status_label);
        println!("    {} panes total", status.pane_count);

        // Show error if any
        if let Some(err) = &status.last_error {
            println!("    {} {}", red.apply_to("error:"), err);
        }

        // List agent panes in this socket
        let socket_windows: Vec<_> = windows.iter().filter(|w| w.socket() == socket).collect();

        if socket_windows.is_empty() {
            println!("    {}", dim.apply_to("no agent sessions"));
        } else {
            println!(
                "    {} agent session{}:",
                socket_windows.len(),
                if socket_windows.len() == 1 { "" } else { "s" }
            );

            for wnd in socket_windows {
                let title = wnd
                    .session_info
                    .as_ref()
                    .and_then(|s| s.summaries.first())
                    .map(|s| s.summary.as_str())
                    .unwrap_or(&wnd.title);
                let title = title.strip_prefix("✳ ").unwrap_or(title);
                let title_short: String = title.chars().take(40).collect();
                let title_display = if title.len() > 40 {
                    format!("{}…", title_short)
                } else {
                    title_short
                };

                let focus = if wnd.is_focused { "▸" } else { " " };
                let id_str = format!("{:>3}", wnd.id());

                print!(
                    "      {}{} ",
                    focus,
                    if wnd.is_focused {
                        yellow.apply_to(&id_str)
                    } else {
                        dim.apply_to(&id_str)
                    }
                );
                println!("{}", title_display);
            }
        }
        println!();
    }

    // Print legend showing socket status symbols and focused indicator
    Legend::for_ls_sockets().print();

    Ok(())
}

/// Check status of a specific window or the focused window
#[instrument(level = "debug", skip(core))]
pub async fn cmd_check_agent_pane(
    core: &BabelCore,
    pane_id: Option<u64>,
    json: bool,
) -> Result<()> {
    let window = core.pane(pane_id).await?;

    match window {
        Some(win) => {
            if json {
                println!("{}", serde_json::to_string_pretty(&win)?);
            } else {
                print_window(&win)?;
            }
        }
        None => {
            if pane_id.is_some() {
                println!("Window not found or not an agent session");
                println!();
                show_available_windows(core).await?;
            } else {
                println!("No focused agent pane found");
            }
        }
    }

    Ok(())
}

/// Check status of a specific richspace pane
#[instrument(level = "debug", skip(_core))]
pub async fn cmd_check_pane(
    _core: &BabelCore,
    pane_name: Option<String>,
    _json: bool,
) -> Result<()> {
    // TODO: Implement once richspace-babel pane querying is available
    match pane_name {
        Some(name) => {
            println!(
                "Pane '{}' not found (richspace-babel integration pending)",
                name
            );
        }
        None => {
            println!("No pane name specified");
            println!();
            println!("Usage: babel check-pane <PANE_NAME>");
        }
    }
    Ok(())
}

/// Show conversation history
#[instrument(level = "debug", skip(core, sessions))]
pub async fn cmd_history(
    core: &BabelCore,
    sessions: Vec<String>,
    limit: usize,
    all: bool,
    json: bool,
) -> Result<()> {
    // Determine effective limit
    let effective_limit = if all { usize::MAX } else { limit };

    // If specific sessions requested, filter to those
    let results = if !sessions.is_empty() {
        // TODO: Implement session ID filtering via daemon or direct lookup
        // For now, filter from full history
        let all_sessions = core.history(usize::MAX).await?;
        all_sessions
            .into_iter()
            .filter(|s| sessions.iter().any(|id| s.session_id.contains(id)))
            .collect()
    } else {
        core.history(effective_limit).await?
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
// Helper Functions - Data Fetching
// ═══════════════════════════════════════════════════════════════════════════════

/// Show available agent panes for user selection
#[instrument(level = "debug", skip(core))]
async fn show_available_windows(core: &BabelCore) -> Result<()> {
    let windows = core.panes().await?;

    if windows.is_empty() {
        println!("No agent panes found");
        return Ok(());
    }

    println!("Available agent panes:");
    for wnd in &windows {
        let title = wnd.title.strip_prefix("✳ ").unwrap_or(&wnd.title);
        let title: String = title.chars().take(30).collect();
        println!("  {:>5}  {:30}  {}", wnd.id(), title, wnd.cwd.display());
    }
    Ok(())
}

// ═══════════════════════════════════════════════════════════════════════════════
// Display Helpers - Formatting
// ═══════════════════════════════════════════════════════════════════════════════

/// Print a single window in compact format
pub fn print_window(wnd: &AgentPane) -> Result<()> {
    let conn = init_db()?;

    // Get overlay metadata if we have a session ID
    let meta = wnd
        .session_id
        .as_ref()
        .and_then(|id| get_metadata(&conn, id).ok().flatten());

    // Indicators
    let focus_indicator = if wnd.is_focused { "▸" } else { " " };
    let unread = !meta.as_ref().map(|m| m.is_read).unwrap_or(true);
    let custom_icon = meta.as_ref().and_then(|m| m.icon.as_ref());

    // Socket indicator - show warning for windows on non-current kitty instance
    let current_socket = babel::kitty::default_socket();
    let is_current_socket = wnd.socket() == current_socket;

    // The worker's breath — reading their current state
    // Hook state is ground truth from Claude Code lifecycle (Working/Idle)
    // Activity state gives granularity when working (Thinking, ToolUse, etc.)
    //
    // Icons: ⚡ Thinking, ⚙ ToolUse, 📋 PlanApproval, ◆ AwaitingInput,
    //        ◐ BackgroundTask, ○ Idle, ● Working (generic), ◌ Unknown
    use babel::babel_storage::HookState;

    let (state_icon, state_style) = match (wnd.hook_state, wnd.activity_state) {
        // Hook says Idle → trust it absolutely (worker finished, awaiting the Captain's voice)
        (Some(HookState::Idle), _) => ("○", Style::new().dim()),

        // Hook says ToolRunning → most precise state from PreToolUse hook
        (Some(HookState::ToolRunning), _) => ("⚙", Style::new().cyan().bold()),

        // Hook says Working → use activity_state for granularity
        (Some(HookState::Working), Some(ActivityState::Thinking)) => ("⚡", Style::new().yellow()),
        (Some(HookState::Working), Some(ActivityState::ToolUse)) => ("⚙", Style::new().cyan()),
        (Some(HookState::Working), Some(ActivityState::PlanApproval)) => {
            ("📋", Style::new().magenta())
        }
        (Some(HookState::Working), Some(ActivityState::BackgroundTask)) => {
            ("◐", Style::new().magenta())
        }
        (Some(HookState::Working), _) => ("●", Style::new().yellow()), // working but no granular state

        // No hook state → fall back to activity_state
        (None, Some(ActivityState::Thinking)) => ("⚡", Style::new().yellow()),
        (None, Some(ActivityState::ToolUse)) => ("⚙", Style::new().cyan()),
        (None, Some(ActivityState::PlanApproval)) => ("📋", Style::new().magenta()),
        (None, Some(ActivityState::AwaitingInput)) => ("◆", Style::new().green()),
        (None, Some(ActivityState::BackgroundTask)) => ("◐", Style::new().magenta()),
        (None, Some(ActivityState::Idle)) => ("○", Style::new().dim()),
        (None, Some(ActivityState::Unknown)) | (None, None) => ("◌", Style::new().blue()), // no data
    };

    // Title - strip ✳ prefix if present, use summary from session if available
    let raw_title = wnd
        .session_info
        .as_ref()
        .and_then(|s| s.summaries.first())
        .map(|s| s.summary.as_str())
        .unwrap_or(&wnd.title);
    let title = raw_title.strip_prefix("✳ ").unwrap_or(raw_title);

    // Determine title source for styling:
    // - Haiku-generated (babel's own) → non-italic, normal color
    // - User /rename (from history.jsonl) → italic, normal color
    // - Procedural (JSONL summary, first prompt) → italic, dim
    enum TitleSource {
        BabelHaiku,
        UserRename,
        Procedural,
    }

    let title_source = if let Some(ref id) = wnd.session_id {
        if get_generated_title(&conn, id).ok().flatten().is_some() {
            TitleSource::BabelHaiku
        } else if get_session_display_name(id).is_some() {
            TitleSource::UserRename
        } else {
            TitleSource::Procedural
        }
    } else {
        TitleSource::Procedural
    };

    let title_style = match title_source {
        TitleSource::BabelHaiku => Style::new(), // non-italic, normal
        TitleSource::UserRename => Style::new().italic(), // italic, normal
        TitleSource::Procedural => Style::new().dim().italic(), // italic, dim
    };

    // Styles
    let dim = Style::new().dim();
    let bold = Style::new().bold();

    // Build the line
    let id_str = format!("{:>3}", wnd.id());

    // Unread dot or custom icon
    let marker = if let Some(icon) = custom_icon {
        format!("{} ", icon)
    } else if unread {
        format!("{} ", style("●").yellow())
    } else {
        "  ".to_string()
    };

    // Compact cwd - just the last component or ~ prefix
    let cwd_display = wnd
        .cwd
        .strip_prefix(dirs::home_dir().unwrap_or_default())
        .map(|p| format!("~/{}", p.display()))
        .unwrap_or_else(|_| wnd.cwd.display().to_string());

    // Truncate cwd if too long
    let cwd_short = if cwd_display.len() > 30 {
        format!("…{}", &cwd_display[cwd_display.len() - 28..])
    } else {
        cwd_display
    };

    // Format: " ▸●⚡123 ~/path  Title"
    // Components: focus | unread/icon | state | id | cwd | title [socket]
    // Socket indicator: nothing for current, ⚠sock-XXXXX for non-current
    print!(
        " {}{}{}",
        focus_indicator,
        marker,
        state_style.apply_to(state_icon)
    );
    print!(
        "{} ",
        if wnd.is_focused {
            bold.apply_to(&id_str)
        } else {
            dim.apply_to(&id_str)
        }
    );
    print!("{}  ", dim.apply_to(&cwd_short));
    // Title styled by source: haiku=normal, user-rename=italic, procedural=dim+italic
    // Focus no longer affects title color (▸ marker is sufficient)
    print!("{}", title_style.apply_to(title));

    // Show socket warning for non-current socket
    if !is_current_socket {
        // Extract filename: "unix:/run/user/1000/kitty.sock-74830" → "kitty.sock-74830"
        let sock_name = wnd.socket().rsplit('/').next().unwrap_or("other");
        print!(
            " {}",
            Style::new().red().apply_to(format!("⚠{}", sock_name))
        );
    }
    println!();

    Ok(())
}

/// Print a single window in detailed format with all metadata
pub fn print_window_detailed(wnd: &AgentPane) -> Result<()> {
    let conn = init_db()?;

    // Get overlay metadata if we have a session ID
    let meta = wnd
        .session_id
        .as_ref()
        .and_then(|id| get_metadata(&conn, id).ok().flatten());

    // Styles
    let dim = Style::new().dim();
    let bold = Style::new().bold();
    let cyan = Style::new().cyan();

    // Title - strip ✳ prefix
    let raw_title = wnd
        .session_info
        .as_ref()
        .and_then(|s| s.summaries.first())
        .map(|s| s.summary.as_str())
        .unwrap_or(&wnd.title);
    let title = raw_title.strip_prefix("✳ ").unwrap_or(raw_title);

    // Determine title source for styling (same as print_window)
    enum TitleSource {
        BabelHaiku,
        UserRename,
        Procedural,
    }

    let title_source = if let Some(ref id) = wnd.session_id {
        if get_generated_title(&conn, id).ok().flatten().is_some() {
            TitleSource::BabelHaiku
        } else if get_session_display_name(id).is_some() {
            TitleSource::UserRename
        } else {
            TitleSource::Procedural
        }
    } else {
        TitleSource::Procedural
    };

    let title_style = match title_source {
        TitleSource::BabelHaiku => Style::new(), // non-italic, normal
        TitleSource::UserRename => Style::new().italic(), // italic, normal
        TitleSource::Procedural => Style::new().dim().italic(), // italic, dim
    };

    // Focus/unread indicators
    let focus_marker = if wnd.is_focused { "▸ " } else { "  " };
    let unread = !meta.as_ref().map(|m| m.is_read).unwrap_or(true);

    // Header line: focus + ID + title (title styled by source, not focus)
    print!("{}", focus_marker);
    print!(
        "{} ",
        if wnd.is_focused {
            bold.apply_to(format!("[{}]", wnd.id()))
        } else {
            dim.apply_to(format!("[{}]", wnd.id()))
        }
    );
    println!("{}", title_style.apply_to(title));

    // Details
    let indent = "      ";

    // CWD - full path
    let cwd_display = wnd
        .cwd
        .strip_prefix(dirs::home_dir().unwrap_or_default())
        .map(|p| format!("~/{}", p.display()))
        .unwrap_or_else(|_| wnd.cwd.display().to_string());
    println!("{}{} {}", indent, dim.apply_to("cwd"), cwd_display);

    // Session ID with confidence if fingerprint matched
    if let Some(ref session_id) = wnd.session_id {
        if let Some(confidence) = wnd.match_confidence {
            println!(
                "{}{} {} ({:?} confidence)",
                indent,
                dim.apply_to("session"),
                dim.apply_to(session_id),
                confidence
            );
        } else {
            println!(
                "{}{} {}",
                indent,
                dim.apply_to("session"),
                dim.apply_to(session_id)
            );
        }
    }

    // Fingerprint data if available
    if let Some(ref fp) = wnd.fingerprint {
        println!("{}{}", indent, dim.apply_to("fingerprint:"));

        if let Some(ref first) = fp.first_prompt {
            let truncated = if first.len() > 50 {
                format!("{}...", &first[..50])
            } else {
                first.clone()
            };
            println!(
                "{}  {} \"{}\"",
                indent,
                dim.apply_to("first_prompt:"),
                truncated
            );
        }

        if !fp.tool_sequence.is_empty() {
            let tools_str = if fp.tool_sequence.len() > 4 {
                format!(
                    "[{}, ... {} total]",
                    fp.tool_sequence[..4].join(", "),
                    fp.tool_sequence.len()
                )
            } else {
                format!("[{}]", fp.tool_sequence.join(", "))
            };
            println!("{}  {} {}", indent, dim.apply_to("tools:"), tools_str);
        }

        if !fp.recent_prompts.is_empty() {
            println!(
                "{}  {} {}",
                indent,
                dim.apply_to("prompts:"),
                fp.recent_prompts.len()
            );
        }
    }

    // Session info (when available)
    if let Some(ref info) = wnd.session_info {
        // Project path
        let project_display = info
            .project
            .strip_prefix(dirs::home_dir().unwrap_or_default())
            .map(|p| format!("~/{}", p.display()))
            .unwrap_or_else(|_| info.project.display().to_string());
        println!("{}{} {}", indent, dim.apply_to("project"), project_display);

        // JSONL path - direct link to conversation file
        let jsonl_path = get_session_path(&info.project, &info.session_id);
        let jsonl_display = jsonl_path
            .strip_prefix(dirs::home_dir().unwrap_or_default())
            .map(|p| format!("~/{}", p.display()))
            .unwrap_or_else(|_| jsonl_path.display().to_string());
        println!(
            "{}{} {}",
            indent,
            dim.apply_to("jsonl"),
            cyan.apply_to(&jsonl_display)
        );

        // Slug (codename)
        if let Some(ref slug) = info.slug {
            println!("{}{} {}", indent, dim.apply_to("slug"), cyan.apply_to(slug));
        }

        // Message count
        if info.message_count > 0 {
            println!(
                "{}{} {}",
                indent,
                dim.apply_to("messages"),
                info.message_count
            );
        }

        // First prompt (from JSONL)
        if let Some(ref prompt) = info.first_prompt {
            println!(
                "{}{} \"{}\"",
                indent,
                dim.apply_to("first"),
                dim.apply_to(prompt)
            );
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
        println!("{}{}", indent, style("● unread").yellow());
    }

    println!(); // Blank line between entries
    Ok(())
}

/// Print a session in compact format for history listings
pub fn print_session(session: &SessionInfo) -> Result<()> {
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
    let project_display = session
        .project
        .strip_prefix(dirs::home_dir().unwrap_or_default())
        .map(|p| format!("~/{}", p.display()))
        .unwrap_or_else(|_| session.project.display().to_string());

    println!(
        "  {}{}  {}",
        summary,
        dim.apply_to(&slug),
        dim.apply_to(&project_display)
    );

    Ok(())
}

// ═══════════════════════════════════════════════════════════════════════════════
// Title Query
// ═══════════════════════════════════════════════════════════════════════════════

/// Get the title/name of a tracked session
///
/// Returns the conversation name from Claude's history (set via /rename),
/// falling back to the session summary, window title, or first prompt.
///
/// Title resolution order:
/// 1. Display name from history.jsonl (set via /rename in Claude Code)
/// 2. Session summary (from session JSONL)
/// 3. Window title (strip ✳ prefix if present)
/// 4. First prompt from conversation
#[instrument(level = "debug", skip(core))]
pub async fn cmd_get_title(core: &BabelCore, target: &Target, json: bool) -> Result<()> {
    // Resolve target to window ID
    let pane_id = match target {
        Target::Window(id) => *id,
        Target::Current => {
            let (id, _socket) = super::current_pane_info()?;
            id
        }
        Target::All => {
            anyhow::bail!(
                "Cannot get title for all windows. Use a specific window ID or '.' for current."
            );
        }
    };

    // Get window info
    let windows = core.panes().await?;
    let window = windows.iter().find(|w| w.id() == pane_id).ok_or_else(|| {
        anyhow::anyhow!("Window {} not found or is not an agent session", pane_id)
    })?;

    let title = resolve_pane_title(window);

    if json {
        let output = serde_json::json!({
            "pane_id": pane_id,
            "session_id": window.session_id,
            "title": title,
        });
        println!("{}", serde_json::to_string_pretty(&output)?);
    } else {
        println!("{}", title);
    }

    Ok(())
}

// ═══════════════════════════════════════════════════════════════════════════════
// Plan Command
// ═══════════════════════════════════════════════════════════════════════════════

/// Show the todo/plan list for an agent session
///
/// Resolves target to session ID, then extracts todos from the transcript JSONL.
#[tracing::instrument(level = "debug", skip(core))]
pub async fn cmd_plan(core: &BabelCore, target: &str, json: bool) -> Result<()> {
    use babel::utility::claude_storage::get_todos_by_session_id;

    // Resolve target to session ID
    let session_id = resolve_plan_target(core, target).await?;

    // Get todos
    let todos = get_todos_by_session_id(&session_id)?;

    if json {
        let output = serde_json::json!({
            "session_id": session_id,
            "todos": todos,
        });
        println!("{}", serde_json::to_string_pretty(&output)?);
        return Ok(());
    }

    if todos.is_empty() {
        println!("{}", style("No plan/todos found for this session").dim());
        return Ok(());
    }

    // Pretty print
    print_plan(&todos, &session_id);

    Ok(())
}

/// Resolve plan target to a session ID
///
/// Handles:
/// - "." → current pane's session
/// - numeric → window ID lookup
/// - otherwise → assume session ID directly
async fn resolve_plan_target(core: &BabelCore, target: &str) -> Result<String> {
    use super::current_pane_info;

    match target {
        "." => {
            // Current pane - get from KITTY_WINDOW_ID
            let (pane_id, _socket) = current_pane_info()?;
            let windows = core.panes().await?;
            let pane = windows
                .iter()
                .find(|w| w.id() == pane_id)
                .context("Current pane is not an agent session")?;
            pane.session_id
                .clone()
                .context("Current pane has no session ID")
        }
        s if s.chars().all(|c| c.is_ascii_digit()) => {
            // Numeric - window ID
            let pane_id: u64 = s.parse()?;
            let windows = core.panes().await?;
            let pane = windows
                .iter()
                .find(|w| w.id() == pane_id)
                .with_context(|| format!("Window {} not found", pane_id))?;
            pane.session_id
                .clone()
                .with_context(|| format!("Window {} has no session ID", pane_id))
        }
        s => {
            // Assume session ID directly
            Ok(s.to_string())
        }
    }
}

// ═══════════════════════════════════════════════════════════════════════════════
// Session Index — Cross-Harness Session Listing
// ═══════════════════════════════════════════════════════════════════════════════

/// List all known sessions across all harnesses.
///
/// Queries the session_index table, triggering a lazy backfill from native
/// harness storage on first invocation (when the index is empty).
#[instrument(level = "debug", skip(_core))]
pub async fn cmd_ls_sessions(
    _core: &BabelCore,
    count: usize,
    kind: Option<&str>,
    json: bool,
) -> Result<()> {
    use babel::babel_storage::BabelStorage;

    let db = BabelStorage::open().context("Failed to open babel storage")?;

    if db.session_index_count(None)? == 0 {
        backfill_session_index(&db)?;
    }

    let entries = db.query_session_index(count, kind)?;

    if json {
        println!("{}", serde_json::to_string_pretty(&entries)?);
        return Ok(());
    }

    if entries.is_empty() {
        if kind.is_some() {
            println!("No sessions found for harness {:?}", kind.unwrap());
        } else {
            println!("No sessions found");
        }
        println!();
        println!(
            "{}",
            Style::new().dim().apply_to(
                "Sessions appear here via hooks (live) or backfill (historical).\n\
                 Run `babel hook install` to set up hooks for your harnesses."
            )
        );
        return Ok(());
    }

    let total = db.session_index_count(kind)?;
    let showing = entries.len();
    let dim = Style::new().dim();

    if let Some(k) = kind {
        println!(
            "Sessions ({} of {} for {}):",
            showing,
            total,
            style(k).bold()
        );
    } else {
        println!("Sessions ({} of {}):", showing, total);
    }
    println!();

    let conn = init_db()?;
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64;

    for entry in &entries {
        print_session_index_entry(entry, &conn, now)?;
    }

    println!();
    println!(
        "{}",
        dim.apply_to(format!(
            "  {} total across {} harness{}",
            db.session_index_count(None)?,
            count_distinct_kinds(&entries),
            if count_distinct_kinds(&entries) == 1 {
                ""
            } else {
                "es"
            }
        ))
    );

    Ok(())
}

fn count_distinct_kinds(entries: &[babel::babel_storage::SessionIndexEntry]) -> usize {
    let mut kinds = std::collections::HashSet::new();
    for e in entries {
        kinds.insert(&e.agent_kind);
    }
    kinds.len()
}

/// Truncate a string at a char boundary.
fn truncate_str(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    let mut end = max.min(s.len());
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &s[..end])
}

/// Format relative time from seconds ago.
fn relative_time(seconds_ago: i64) -> String {
    if seconds_ago < 0 {
        return "just now".to_string();
    }
    if seconds_ago < 60 {
        return format!("{}s ago", seconds_ago);
    }
    let minutes = seconds_ago / 60;
    if minutes < 60 {
        return format!("{}m ago", minutes);
    }
    let hours = minutes / 60;
    if hours < 24 {
        return format!("{}h ago", hours);
    }
    let days = hours / 24;
    if days < 30 {
        return format!("{}d ago", days);
    }
    let months = days / 30;
    format!("{}mo ago", months)
}

/// Parse a hex accent color to RGB.
fn hex_to_rgb(hex: &str) -> Option<(u8, u8, u8)> {
    let hex = hex.strip_prefix('#')?;
    if hex.len() != 6 {
        return None;
    }
    let r = u8::from_str_radix(&hex[0..2], 16).ok()?;
    let g = u8::from_str_radix(&hex[2..4], 16).ok()?;
    let b = u8::from_str_radix(&hex[4..6], 16).ok()?;
    Some((r, g, b))
}

/// Print one session index row with harness-colored accent.
fn print_session_index_entry(
    entry: &babel::babel_storage::SessionIndexEntry,
    conn: &rusqlite::Connection,
    now: i64,
) -> Result<()> {
    use babel::babel_storage::{get_metadata, HookState};
    use babel::AgentKind;

    let dim = Style::new().dim();

    let agent_kind = AgentKind::from_slug(&entry.agent_kind);
    let accent = agent_kind.map(|k| k.accent_color()).unwrap_or("#666666");
    let (r, g, b) = hex_to_rgb(accent).unwrap_or((102, 102, 102));

    let harness_style = Style::new().color256(
        closest_ansi256(r, g, b),
    );

    let meta = get_metadata(conn, &entry.session_key).ok().flatten();
    let unread = !meta.as_ref().map(|m| m.is_read).unwrap_or(true);
    let custom_icon = meta.as_ref().and_then(|m| m.icon.as_ref());
    let hook_state = meta.as_ref().map(|m| m.hook_state);

    let (state_icon, state_style) = match hook_state {
        Some(HookState::Idle) => ("○", Style::new().dim()),
        Some(HookState::Working) => ("●", Style::new().yellow()),
        Some(HookState::ToolRunning) => ("⚙", Style::new().cyan().bold()),
        None => (" ", Style::new().dim()),
    };

    let marker = if let Some(icon) = custom_icon {
        format!("{} ", icon)
    } else if unread {
        format!("{} ", style("●").yellow())
    } else {
        "  ".to_string()
    };

    let harness_name = format!("{:<8}", entry.agent_kind);

    let cwd_display = entry
        .project_path
        .as_deref()
        .and_then(|p| {
            let home = dirs::home_dir()?;
            let path = std::path::Path::new(p);
            path.strip_prefix(&home)
                .ok()
                .map(|rel| format!("~/{}", rel.display()))
                .or_else(|| Some(p.to_string()))
        })
        .unwrap_or_default();
    let cwd_short = if cwd_display.len() > 30 {
        format!("…{}", &cwd_display[cwd_display.len() - 28..])
    } else {
        cwd_display
    };

    let raw_title = entry.display_name.as_deref().unwrap_or("");
    let title = &raw_title.replace('\n', "↵").replace('\r', "");
    let title_short: String = title.chars().take(40).collect();
    let title_display = if title.chars().count() > 40 {
        format!("{}…", title_short)
    } else {
        title_short
    };

    let elapsed = now - entry.last_seen_at;
    let time_str = format!("{:>7}", relative_time(elapsed));

    print!(
        " {}{}",
        marker,
        state_style.apply_to(state_icon),
    );
    print!(" {}", harness_style.apply_to(&harness_name));
    print!(" {}  ", dim.apply_to(format!("{:<30}", cwd_short)));
    print!("{} ", dim.apply_to(&time_str));
    print!("{}", harness_style.apply_to(&title_display));

    let resume_cmd = resume_command(&entry.agent_kind, &entry.native_id);
    println!("  {}", dim.apply_to(&resume_cmd));

    Ok(())
}

/// Generate a copy-pasteable resume command for a session.
fn resume_command(agent_kind: &str, native_id: &str) -> String {
    match agent_kind {
        "claude" => format!("claude --resume {}", native_id),
        "codex" => format!("codex resume {}", native_id),
        "gemini" => format!("gemini --session {}", native_id),
        "kimi" => format!("kimi resume {}", native_id),
        "factory-droid" => format!("factory --resume {}", native_id),
        "qwen-code" => format!("qwen-code --resume {}", native_id),
        _ => format!("{} (no resume)", agent_kind),
    }
}

/// Map RGB to closest ANSI 256-color index (216-color cube).
fn closest_ansi256(r: u8, g: u8, b: u8) -> u8 {
    let ri = ((r as u16) * 5 / 255) as u8;
    let gi = ((g as u16) * 5 / 255) as u8;
    let bi = ((b as u16) * 5 / 255) as u8;
    16 + 36 * ri + 6 * gi + bi
}

// ═══════════════════════════════════════════════════════════════════════════════
// Session Index Backfill — Populate from Native Harness Storage
// ═══════════════════════════════════════════════════════════════════════════════

/// Backfill session index from all harnesses with discoverable native storage.
///
/// Called lazily on first `ls-sessions` when the index is empty. Each harness
/// adapter scans its native storage layout and upserts entries. Harnesses
/// without central storage are hook-only (no backfill).
fn backfill_session_index(db: &babel::babel_storage::BabelStorage) -> Result<()> {
    let mut total = 0usize;

    total += backfill_claude(db).unwrap_or(0);
    total += backfill_codex(db).unwrap_or(0);
    total += backfill_gemini(db).unwrap_or(0);
    total += backfill_kimi(db).unwrap_or(0);
    // Factory Droid and Qwen use Claude-fork storage with different base dirs.
    // Their sessions appear via hooks; no known stable storage path for backfill.

    if total > 0 {
        eprintln!(
            "{}",
            Style::new()
                .dim()
                .apply_to(format!("Backfilled {} sessions from native storage", total))
        );
    }

    Ok(())
}

/// Backfill from Claude Code's ~/.claude/history.jsonl
///
/// Reads history.jsonl directly for display names and timestamps rather than
/// parsing each session JSONL file (which would be O(n * file_size)).
fn backfill_claude(db: &babel::babel_storage::BabelStorage) -> Result<usize> {
    use babel::utility::claude_storage::claude_base;
    use std::io::{BufRead, BufReader};

    let history_path = claude_base().join("history.jsonl");
    if !history_path.exists() {
        return Ok(0);
    }

    #[derive(serde::Deserialize)]
    struct HistEntry {
        display: String,
        #[serde(rename = "sessionId", default)]
        session_id: Option<String>,
        project: std::path::PathBuf,
        timestamp: i64,
    }

    let file = std::fs::File::open(&history_path)?;
    let reader = BufReader::new(file);

    let mut seen = std::collections::HashSet::new();
    let mut count = 0;

    for line in reader.lines() {
        let line = match line {
            Ok(l) => l,
            Err(_) => continue,
        };
        if line.trim().is_empty() {
            continue;
        }
        let entry: HistEntry = match serde_json::from_str(&line) {
            Ok(e) => e,
            Err(_) => continue,
        };
        let session_id = match entry.session_id {
            Some(id) if !id.is_empty() => id,
            _ => continue,
        };
        if !seen.insert(session_id.clone()) {
            continue;
        }

        let key = babel::AgentKind::Claude.session_key(&session_id);
        let project_str = entry.project.to_string_lossy().into_owned();
        let display = if entry.display.is_empty() {
            None
        } else {
            Some(entry.display)
        };

        // Claude history.jsonl timestamps are milliseconds
        let ts_secs = entry.timestamp / 1000;

        let idx = babel::babel_storage::SessionIndexEntry {
            session_key: key,
            agent_kind: "claude".to_string(),
            native_id: session_id,
            project_path: Some(project_str),
            display_name: display,
            first_seen_at: ts_secs,
            last_seen_at: ts_secs,
        };
        if db.upsert_session_index(&idx).is_ok() {
            count += 1;
        }
    }

    Ok(count)
}

/// Backfill from Codex CLI's ~/.codex/history.jsonl + session_index.jsonl
///
/// history.jsonl has per-prompt entries: {session_id, ts, text}
/// session_index.jsonl has thread names: {id, thread_name, updated_at}
/// We merge both to get session_id → (display_name, first/last timestamps).
fn backfill_codex(db: &babel::babel_storage::BabelStorage) -> Result<usize> {
    use std::io::{BufRead, BufReader};

    let codex_home = dirs::home_dir().context("no home dir")?.join(".codex");
    if !codex_home.exists() {
        return Ok(0);
    }

    // Phase 1: Read thread names from session_index.jsonl
    #[derive(serde::Deserialize)]
    struct ThreadEntry {
        id: String,
        thread_name: String,
    }

    let mut thread_names: std::collections::HashMap<String, String> =
        std::collections::HashMap::new();
    let idx_path = codex_home.join("session_index.jsonl");
    if idx_path.exists() {
        if let Ok(file) = std::fs::File::open(&idx_path) {
            for line in BufReader::new(file).lines().flatten() {
                if let Ok(e) = serde_json::from_str::<ThreadEntry>(&line) {
                    if !e.thread_name.is_empty() {
                        thread_names.insert(e.id, e.thread_name);
                    }
                }
            }
        }
    }

    // Phase 2: Read history.jsonl to discover sessions with timestamps
    #[derive(serde::Deserialize)]
    struct HistEntry {
        session_id: String,
        ts: i64,
        text: String,
    }

    struct SessionAcc {
        first_ts: i64,
        last_ts: i64,
        first_text: String,
    }

    let hist_path = codex_home.join("history.jsonl");
    if !hist_path.exists() {
        return Ok(0);
    }

    let mut sessions: std::collections::HashMap<String, SessionAcc> =
        std::collections::HashMap::new();

    if let Ok(file) = std::fs::File::open(&hist_path) {
        for line in BufReader::new(file).lines().flatten() {
            if let Ok(e) = serde_json::from_str::<HistEntry>(&line) {
                if e.session_id.is_empty() {
                    continue;
                }
                sessions
                    .entry(e.session_id.clone())
                    .and_modify(|acc| {
                        acc.first_ts = acc.first_ts.min(e.ts);
                        acc.last_ts = acc.last_ts.max(e.ts);
                    })
                    .or_insert(SessionAcc {
                        first_ts: e.ts,
                        last_ts: e.ts,
                        first_text: truncate_str(&e.text, 100),
                    });
            }
        }
    }

    let mut count = 0;
    for (session_id, acc) in &sessions {
        let key = babel::AgentKind::Codex.session_key(session_id);
        let display = thread_names
            .get(session_id)
            .cloned()
            .or_else(|| {
                if acc.first_text.is_empty() {
                    None
                } else {
                    Some(acc.first_text.clone())
                }
            });

        let entry = babel::babel_storage::SessionIndexEntry {
            session_key: key,
            agent_kind: "codex".to_string(),
            native_id: session_id.clone(),
            project_path: None,
            display_name: display,
            first_seen_at: acc.first_ts,
            last_seen_at: acc.last_ts,
        };
        if db.upsert_session_index(&entry).is_ok() {
            count += 1;
        }
    }

    Ok(count)
}

/// Backfill from Gemini CLI's ~/.gemini/
fn backfill_gemini(db: &babel::babel_storage::BabelStorage) -> Result<usize> {
    let gemini_dir = dirs::home_dir()
        .context("no home dir")?
        .join(".gemini");

    if !gemini_dir.exists() {
        return Ok(0);
    }

    let mut count = 0;

    // Gemini stores sessions in tmp/<project-id>/chats/ (JSON/JSONL files)
    let tmp_dir = gemini_dir.join("tmp");
    if tmp_dir.exists() {
        for project_entry in std::fs::read_dir(&tmp_dir).into_iter().flatten() {
            let project_entry = match project_entry {
                Ok(e) => e,
                Err(_) => continue,
            };
            let project_path = project_entry.path();
            if !project_path.is_dir() {
                continue;
            }
            let chats_dir = project_path.join("chats");
            if !chats_dir.exists() {
                continue;
            }
            for chat_entry in std::fs::read_dir(&chats_dir).into_iter().flatten() {
                let chat_entry = match chat_entry {
                    Ok(e) => e,
                    Err(_) => continue,
                };
                let chat_path = chat_entry.path();
                let ext = chat_path.extension().and_then(|e| e.to_str());
                if !matches!(ext, Some("json" | "jsonl")) {
                    continue;
                }

                let session_id = chat_path
                    .file_stem()
                    .and_then(|n| n.to_str())
                    .unwrap_or_default()
                    .to_string();
                if session_id.is_empty() {
                    continue;
                }

                let mtime = chat_entry
                    .metadata()
                    .ok()
                    .and_then(|m| m.modified().ok())
                    .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                    .map(|d| d.as_secs() as i64)
                    .unwrap_or(0);

                let key = babel::AgentKind::Gemini.session_key(&session_id);
                let entry = babel::babel_storage::SessionIndexEntry {
                    session_key: key,
                    agent_kind: "gemini".to_string(),
                    native_id: session_id,
                    project_path: None,
                    display_name: None,
                    first_seen_at: mtime,
                    last_seen_at: mtime,
                };
                if db.upsert_session_index(&entry).is_ok() {
                    count += 1;
                }
            }
        }
    }

    Ok(count)
}

/// Backfill from Kimi CLI's ~/.local/share/kimi-cli/sessions/
fn backfill_kimi(db: &babel::babel_storage::BabelStorage) -> Result<usize> {
    let kimi_dir = dirs::data_dir()
        .context("no data dir")?
        .join("kimi-cli")
        .join("sessions");

    if !kimi_dir.exists() {
        return Ok(0);
    }

    let mut count = 0;

    // Kimi stores sessions in sessions/<workdir-hash>/<session-id>/
    for workdir_entry in std::fs::read_dir(&kimi_dir).into_iter().flatten() {
        let workdir_entry = match workdir_entry {
            Ok(e) => e,
            Err(_) => continue,
        };
        let workdir_path = workdir_entry.path();
        if !workdir_path.is_dir() {
            continue;
        }
        for session_entry in std::fs::read_dir(&workdir_path).into_iter().flatten() {
            let session_entry = match session_entry {
                Ok(e) => e,
                Err(_) => continue,
            };
            let session_path = session_entry.path();
            if !session_path.is_dir() {
                continue;
            }

            let session_id = session_path
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or_default()
                .to_string();
            if session_id.is_empty() {
                continue;
            }

            let mtime = session_entry
                .metadata()
                .ok()
                .and_then(|m| m.modified().ok())
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_secs() as i64)
                .unwrap_or(0);

            let key = babel::AgentKind::Kimi.session_key(&session_id);
            let entry = babel::babel_storage::SessionIndexEntry {
                session_key: key,
                agent_kind: "kimi".to_string(),
                native_id: session_id,
                project_path: None,
                display_name: None,
                first_seen_at: mtime,
                last_seen_at: mtime,
            };
            if db.upsert_session_index(&entry).is_ok() {
                count += 1;
            }
        }
    }

    Ok(count)
}

/// Pretty print a todo list
fn print_plan(todos: &[babel::utility::claude_storage::TodoItem], session_id: &str) {
    use console::style;

    // Header with session (truncated)
    let short_id = if session_id.len() > 8 {
        &session_id[..8]
    } else {
        session_id
    };
    println!(
        "{} {}",
        style("Plan").bold().cyan(),
        style(format!("({})", short_id)).dim()
    );
    println!();

    // Status indicators
    let completed = style("✓").green();
    let in_progress = style("●").yellow();
    let pending = style("○").dim();

    for todo in todos {
        let status_icon = match todo.status.as_str() {
            "completed" => &completed,
            "in_progress" => &in_progress,
            _ => &pending,
        };

        // Use activeForm if available and in_progress, otherwise content
        let text = if todo.status == "in_progress" {
            todo.active_form.as_deref().unwrap_or(&todo.content)
        } else {
            &todo.content
        };

        // Style based on status
        let styled_text = match todo.status.as_str() {
            "completed" => style(text).dim().strikethrough(),
            "in_progress" => style(text).bold(),
            _ => style(text).white(),
        };

        println!("  {} {}", status_icon, styled_text);
    }
    println!();

    // Summary
    let completed_count = todos.iter().filter(|t| t.status == "completed").count();
    let in_progress_count = todos.iter().filter(|t| t.status == "in_progress").count();
    let pending_count = todos.iter().filter(|t| t.status == "pending").count();

    println!(
        "{}",
        style(format!(
            "  {} completed • {} in progress • {} pending",
            completed_count, in_progress_count, pending_count
        ))
        .dim()
    );
}
