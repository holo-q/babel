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

// ═══════════════════════════════════════════════════════════════════════════════
// ls-sessions — Direct-Read Cross-Harness Session Listing
// ═══════════════════════════════════════════════════════════════════════════════

/// A session discovered from native harness storage.
///
/// This is a read-through view — no caching, no DB. Each harness scanner reads
/// its native storage and returns these. The overlay DB (session_metadata) is
/// only consulted for babel's own runtime state (hook_state, icon, is_read).
pub(super) struct NativeSession {
    pub(super) agent_kind: babel::AgentKind,
    pub(super) native_id: String,
    pub(super) project_path: Option<String>,
    pub(super) display_name: Option<String>,
    pub(super) last_prompt: Option<String>,
    pub(super) turn_count: u32,
    pub(super) last_seen_at: i64,
    pub(super) interactive: bool,
}

/// Scan all harnesses and return interactive sessions sorted by recency.
///
/// This is the shared pipeline for `ls-sessions` and `resume <index>`.
pub(super) fn scan_all_sessions(
    kind: Option<&str>,
    show_all: bool,
) -> Vec<NativeSession> {
    let mut sessions = Vec::new();
    let kind_filter = kind.and_then(babel::AgentKind::from_slug);

    if kind_filter.is_none() || kind_filter == Some(babel::AgentKind::Claude) {
        sessions.extend(scan_claude().unwrap_or_default());
    }
    if kind_filter.is_none() || kind_filter == Some(babel::AgentKind::Codex) {
        sessions.extend(scan_codex().unwrap_or_default());
    }
    if kind_filter.is_none() || kind_filter == Some(babel::AgentKind::Gemini) {
        sessions.extend(scan_gemini().unwrap_or_default());
    }
    if kind_filter.is_none() || kind_filter == Some(babel::AgentKind::Kimi) {
        sessions.extend(scan_kimi().unwrap_or_default());
    }

    sessions.sort_by(|a, b| b.last_seen_at.cmp(&a.last_seen_at));
    if !show_all {
        sessions.retain(|s| s.interactive);
    }
    sessions
}

/// List all known sessions across all harnesses.
///
/// Reads native storage directly per harness, merges, sorts by recency,
/// and joins with session_metadata for overlay enrichment (hook_state, icon, is_read).
#[instrument(level = "debug", skip(_core))]
pub async fn cmd_ls_sessions(
    _core: &BabelCore,
    count: usize,
    kind: Option<&str>,
    show_sub: bool,
    show_all: bool,
    json: bool,
) -> Result<()> {
    let include_non_interactive = show_sub || show_all;
    let mut sessions = scan_all_sessions(kind, include_non_interactive);

    // Filter hidden sessions unless --all
    if !show_all {
        let conn = init_db().ok();
        sessions.retain(|s| {
            let key = s.agent_kind.session_key(&s.native_id);
            let hidden = conn
                .as_ref()
                .and_then(|c| babel::babel_storage::get_metadata(c, &key).ok().flatten())
                .map(|m| m.hidden)
                .unwrap_or(false);
            !hidden
        });
    }

    let total = sessions.len();
    sessions.truncate(count);

    if json {
        let json_out: Vec<_> = sessions
            .iter()
            .map(|s| {
                serde_json::json!({
                    "agent_kind": s.agent_kind.slug(),
                    "native_id": s.native_id,
                    "project_path": s.project_path,
                    "display_name": s.display_name,
                    "last_prompt": s.last_prompt,
                    "turn_count": s.turn_count,
                    "last_seen_at": s.last_seen_at,
                })
            })
            .collect();
        println!("{}", serde_json::to_string_pretty(&json_out)?);
        return Ok(());
    }

    if sessions.is_empty() {
        if kind.is_some() {
            println!("No sessions found for harness {:?}", kind.unwrap());
        } else {
            println!("No sessions found");
        }
        return Ok(());
    }

    let showing = sessions.len();
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

    // Pass 1: compute display cells
    let rows: Vec<SessionRow> = sessions
        .iter()
        .map(|s| session_row(s, &conn, now))
        .collect();

    // Pass 2: measure column widths
    let w_idx = format!("{}", rows.len()).len();
    let w_harness = rows.iter().map(|r| r.harness.len()).max().unwrap_or(0);
    let w_ws = rows.iter().map(|r| r.workspace.len()).max().unwrap_or(0);
    let w_cwd = rows.iter().map(|r| r.cwd.len()).max().unwrap_or(0);
    let w_time = rows.iter().map(|r| r.time.len()).max().unwrap_or(0);
    let w_turns = rows.iter().map(|r| r.turns.len()).max().unwrap_or(0);
    let w_title = rows.iter().map(|r| r.title.chars().count()).max().unwrap_or(0);
    let w_prompt = rows.iter().map(|r| r.last_prompt.chars().count()).max().unwrap_or(0);

    // Pass 3: print
    for (i, row) in rows.iter().enumerate() {
        let idx = i + 1; // 1-based
        let bright = row.interactive && !row.hidden;
        let accent_c = closest_ansi256_from_hex(row.accent);
        let harness_style = if bright { Style::new().color256(accent_c) } else { Style::new().dim() };
        let text_style = if bright { Style::new().color256(accent_c) } else { Style::new().dim() };
        let state_style = if bright { row.state_style() } else { Style::new().dim() };

        print!(" {}{}", row.marker, state_style.apply_to(row.state_icon));
        print!(" {:<w_harness$}", harness_style.apply_to(&row.harness));
        if w_ws > 0 {
            print!("  {:>w_ws$}", dim.apply_to(&row.workspace));
        }
        print!("  {:<w_cwd$}", dim.apply_to(&row.cwd));
        print!("  {:>w_time$}", dim.apply_to(&row.time));
        print!("  {:>w_turns$}", dim.apply_to(&row.turns));
        print!("  {:>w_idx$}", dim.apply_to(idx));
        let tpad = w_title - row.title.chars().count();
        print!("  {}{}", text_style.apply_to(&row.title), " ".repeat(tpad));
        if w_prompt > 0 {
            let ppad = w_prompt - row.last_prompt.chars().count();
            print!("  {}{}", text_style.apply_to(&row.last_prompt), " ".repeat(ppad));
        }
        println!();
    }

    println!();
    let mut kinds = std::collections::HashSet::new();
    for s in &sessions {
        kinds.insert(s.agent_kind);
    }
    println!(
        "{}",
        dim.apply_to(format!(
            "  {} total across {} harness{}",
            total,
            kinds.len(),
            if kinds.len() == 1 { "" } else { "es" }
        ))
    );

    Ok(())
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

pub(super) fn sanitize_display(s: &str, max_chars: usize) -> String {
    let clean = s.replace('\n', "↵").replace('\r', "");
    if clean.chars().count() > max_chars {
        let short: String = clean.chars().take(max_chars).collect();
        format!("{}…", short)
    } else {
        clean
    }
}

fn relative_time(seconds_ago: i64) -> String {
    if seconds_ago < 0 {
        return "now".to_string();
    }
    if seconds_ago < 60 {
        return format!("{}s", seconds_ago);
    }
    let minutes = seconds_ago / 60;
    if minutes < 60 {
        return format!("{}m", minutes);
    }
    let hours = minutes / 60;
    if hours < 24 {
        return format!("{}h", hours);
    }
    let days = hours / 24;
    if days < 30 {
        return format!("{}d", days);
    }
    let months = days / 30;
    format!("{}mo", months)
}

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

fn closest_ansi256_from_hex(hex: &str) -> u8 {
    let (r, g, b) = hex_to_rgb(hex).unwrap_or((102, 102, 102));
    let ri = ((r as u16) * 5 / 255) as u8;
    let gi = ((g as u16) * 5 / 255) as u8;
    let bi = ((b as u16) * 5 / 255) as u8;
    16 + 36 * ri + 6 * gi + bi
}

/// Precomputed display cells for one session row.
struct SessionRow {
    marker: String,
    state_icon: &'static str,
    state_kind: StateKind,
    harness: String,
    workspace: String,
    cwd: String,
    time: String,
    turns: String,
    title: String,
    last_prompt: String,
    accent: &'static str,
    interactive: bool,
    hidden: bool,
}

#[derive(Clone, Copy)]
enum StateKind { Idle, Working, ToolRunning, Unknown }

impl SessionRow {
    fn state_style(&self) -> Style {
        match self.state_kind {
            StateKind::Idle => Style::new().dim(),
            StateKind::Working => Style::new().yellow(),
            StateKind::ToolRunning => Style::new().cyan().bold(),
            StateKind::Unknown => Style::new().dim(),
        }
    }
}

fn session_row(s: &NativeSession, conn: &rusqlite::Connection, now: i64) -> SessionRow {
    use babel::babel_storage::{get_metadata, HookState};

    let accent = s.agent_kind.accent_color();
    let session_key = s.agent_kind.session_key(&s.native_id);

    let meta = get_metadata(conn, &session_key).ok().flatten();
    let unread = !meta.as_ref().map(|m| m.is_read).unwrap_or(true);
    let custom_icon = meta.as_ref().and_then(|m| m.icon.as_ref());
    let hook_state = meta.as_ref().and_then(|m| m.hook_state);

    let (state_icon, state_kind) = match hook_state {
        Some(HookState::Idle) => ("○", StateKind::Idle),
        Some(HookState::Working) => ("●", StateKind::Working),
        Some(HookState::ToolRunning) => ("⚙", StateKind::ToolRunning),
        None => (" ", StateKind::Unknown),
    };

    let marker = if let Some(icon) = custom_icon {
        format!("{} ", icon)
    } else if unread {
        format!("{} ", style("●").yellow())
    } else {
        "  ".to_string()
    };

    let cwd = s
        .project_path
        .as_deref()
        .and_then(|p| {
            let home = dirs::home_dir()?;
            std::path::Path::new(p)
                .strip_prefix(&home)
                .ok()
                .map(|rel| format!("~/{}", rel.display()))
                .or_else(|| Some(p.to_string()))
        })
        .unwrap_or_default();

    let title = s
        .display_name
        .as_deref()
        .map(|t| sanitize_display(t, 40))
        .unwrap_or_default();

    let last_prompt = s
        .last_prompt
        .as_deref()
        .map(|t| sanitize_display(t, 40))
        .unwrap_or_default();

    let turns = if s.turn_count > 0 {
        format!("{}t", s.turn_count)
    } else {
        String::new()
    };

    let workspace = meta
        .as_ref()
        .and_then(|m| m.last_workspace)
        .map(|ws| format!("{}", ws + 1)) // 0-indexed → 1-indexed
        .unwrap_or_default();

    let elapsed = now - s.last_seen_at;
    let time = relative_time(elapsed);

    SessionRow {
        marker,
        state_icon,
        state_kind,
        harness: s.agent_kind.slug().to_string(),
        workspace,
        cwd,
        time,
        turns,
        title,
        last_prompt,
        accent,
        interactive: s.interactive,
        hidden: meta.as_ref().map(|m| m.hidden).unwrap_or(false),
    }
}

// ═══════════════════════════════════════════════════════════════════════════════
// Per-Harness Native Storage Scanners
// ═══════════════════════════════════════════════════════════════════════════════

/// Scan Claude Code's ~/.claude/history.jsonl
///
/// Each line is a user prompt: {display, sessionId, project, timestamp}.
/// We accumulate per session: first display as title, last display as last_prompt,
/// min/max timestamps, and turn count.
fn scan_claude() -> Result<Vec<NativeSession>> {
    use babel::utility::claude_storage::claude_base;
    use std::io::{BufRead, BufReader};

    let history_path = claude_base().join("history.jsonl");
    if !history_path.exists() {
        return Ok(Vec::new());
    }

    #[derive(serde::Deserialize)]
    struct Entry {
        display: String,
        #[serde(rename = "sessionId", default)]
        session_id: Option<String>,
        project: std::path::PathBuf,
        timestamp: i64,
    }

    struct Acc {
        project: String,
        first_display: String,
        last_display: String,
        first_ts: i64,
        last_ts: i64,
        turns: u32,
    }

    let file = std::fs::File::open(&history_path)?;
    let mut sessions: std::collections::HashMap<String, Acc> = std::collections::HashMap::new();

    for line in BufReader::new(file).lines().flatten() {
        if line.trim().is_empty() {
            continue;
        }
        let e: Entry = match serde_json::from_str(&line) {
            Ok(e) => e,
            Err(_) => continue,
        };
        let sid = match e.session_id {
            Some(id) if !id.is_empty() => id,
            _ => continue,
        };
        let ts = e.timestamp / 1000; // millis → secs
        let project = e.project.to_string_lossy().into_owned();

        sessions
            .entry(sid)
            .and_modify(|acc| {
                if ts < acc.first_ts {
                    acc.first_ts = ts;
                    acc.first_display = e.display.clone();
                }
                if ts >= acc.last_ts {
                    acc.last_ts = ts;
                    acc.last_display = e.display.clone();
                    acc.project = project.clone();
                }
                acc.turns += 1;
            })
            .or_insert(Acc {
                project,
                first_display: e.display.clone(),
                last_display: e.display,
                first_ts: ts,
                last_ts: ts,
                turns: 1,
            });
    }

    Ok(sessions
        .into_iter()
        .map(|(sid, acc)| NativeSession {
            agent_kind: babel::AgentKind::Claude,
            native_id: sid,
            project_path: Some(acc.project),
            display_name: Some(acc.first_display),
            last_prompt: if acc.turns > 1 {
                Some(acc.last_display)
            } else {
                None
            },
            turn_count: acc.turns,
            last_seen_at: acc.last_ts,
            interactive: true,
        })
        .collect())
}

/// Scan Codex CLI sessions.
///
/// Primary: state_*.sqlite `threads` table (has cwd, title, timestamps).
/// Enrichment: history.jsonl for turn counts and last_prompt text.
/// Fallback: history.jsonl alone if no state DB found.
fn scan_codex() -> Result<Vec<NativeSession>> {
    use std::io::{BufRead, BufReader};

    let codex_home = dirs::home_dir().context("no home dir")?.join(".codex");
    if !codex_home.exists() {
        return Ok(Vec::new());
    }

    // Accumulator for history.jsonl prompt data (turn count + last prompt)
    struct PromptAcc {
        first_text: String,
        last_text: String,
        turns: u32,
    }
    let mut prompt_data: std::collections::HashMap<String, PromptAcc> =
        std::collections::HashMap::new();

    #[derive(serde::Deserialize)]
    struct HistEntry { session_id: String, ts: i64, text: String }

    let hist_path = codex_home.join("history.jsonl");
    if hist_path.exists() {
        if let Ok(file) = std::fs::File::open(&hist_path) {
            for line in BufReader::new(file).lines().flatten() {
                if let Ok(e) = serde_json::from_str::<HistEntry>(&line) {
                    if e.session_id.is_empty() {
                        continue;
                    }
                    prompt_data
                        .entry(e.session_id)
                        .and_modify(|acc| {
                            acc.last_text = truncate_str(&e.text, 100);
                            acc.turns += 1;
                        })
                        .or_insert(PromptAcc {
                            first_text: truncate_str(&e.text, 100),
                            last_text: truncate_str(&e.text, 100),
                            turns: 1,
                        });
                }
            }
        }
    }

    // Try state DB (has cwd, title, timestamps for all threads)
    let state_db = find_codex_state_db(&codex_home);
    if let Some(db_path) = state_db {
        if let Ok(conn) = rusqlite::Connection::open_with_flags(
            &db_path,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
        ) {
            let mut stmt = conn.prepare(
                "SELECT id, cwd, title, first_user_message, updated_at, has_user_event
                 FROM threads
                 WHERE archived = 0
                 ORDER BY updated_at DESC",
            )?;

            let rows = stmt.query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, Option<String>>(1)?,
                    row.get::<_, Option<String>>(2)?,
                    row.get::<_, Option<String>>(3)?,
                    row.get::<_, i64>(4)?,
                    row.get::<_, bool>(5)?,
                ))
            })?;

            let mut out = Vec::new();
            for row in rows.flatten() {
                let (id, cwd, title, first_msg, updated_at, has_user_event) = row;
                let prompts = prompt_data.remove(&id);
                let turn_count = prompts.as_ref().map(|p| p.turns).unwrap_or(0);

                let display_name = title
                    .filter(|t| !t.is_empty())
                    .or_else(|| first_msg.filter(|m| !m.is_empty()))
                    .or_else(|| prompts.as_ref().map(|p| p.first_text.clone()));

                let last_prompt = prompts.and_then(|p| {
                    if p.turns > 1 { Some(p.last_text) } else { None }
                });

                out.push(NativeSession {
                    agent_kind: babel::AgentKind::Codex,
                    native_id: id,
                    project_path: cwd,
                    display_name,
                    last_prompt,
                    turn_count,
                    last_seen_at: updated_at,
                    interactive: has_user_event || turn_count > 0,
                });
            }
            return Ok(out);
        }
    }

    // Fallback: history.jsonl only (no cwd, but at least we show something)
    Ok(prompt_data
        .into_iter()
        .map(|(sid, acc)| NativeSession {
            agent_kind: babel::AgentKind::Codex,
            native_id: sid,
            project_path: None,
            display_name: Some(acc.first_text),
            last_prompt: if acc.turns > 1 { Some(acc.last_text) } else { None },
            turn_count: acc.turns,
            last_seen_at: 0,
            interactive: true,
        })
        .collect())
}

/// Find the most recent Codex state DB (state_*.sqlite).
fn find_codex_state_db(codex_home: &std::path::Path) -> Option<std::path::PathBuf> {
    let mut candidates: Vec<std::path::PathBuf> = std::fs::read_dir(codex_home)
        .ok()?
        .flatten()
        .filter_map(|e| {
            let name = e.file_name();
            let name = name.to_str()?;
            if name.starts_with("state_") && name.ends_with(".sqlite") {
                Some(e.path())
            } else {
                None
            }
        })
        .collect();
    candidates.sort();
    candidates.pop()
}

/// Scan Gemini CLI's ~/.gemini/tmp/<project>/chats/
fn scan_gemini() -> Result<Vec<NativeSession>> {
    let tmp_dir = dirs::home_dir()
        .context("no home dir")?
        .join(".gemini")
        .join("tmp");
    if !tmp_dir.exists() {
        return Ok(Vec::new());
    }

    let mut out = Vec::new();
    for proj in std::fs::read_dir(&tmp_dir).into_iter().flatten().flatten() {
        let chats = proj.path().join("chats");
        if !chats.is_dir() {
            continue;
        }
        for chat in std::fs::read_dir(&chats).into_iter().flatten().flatten() {
            let p = chat.path();
            if !matches!(p.extension().and_then(|e| e.to_str()), Some("json" | "jsonl")) {
                continue;
            }
            let sid = p.file_stem().and_then(|n| n.to_str()).unwrap_or_default().to_string();
            if sid.is_empty() {
                continue;
            }
            let mtime = chat.metadata().ok()
                .and_then(|m| m.modified().ok())
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_secs() as i64)
                .unwrap_or(0);
            out.push(NativeSession {
                agent_kind: babel::AgentKind::Gemini,
                native_id: sid,
                project_path: None,
                display_name: None,
                last_prompt: None,
                turn_count: 0,
                last_seen_at: mtime,
                interactive: true,
            });
        }
    }
    Ok(out)
}

/// Scan Kimi CLI's ~/.local/share/kimi-cli/sessions/
fn scan_kimi() -> Result<Vec<NativeSession>> {
    let kimi_dir = dirs::data_dir()
        .context("no data dir")?
        .join("kimi-cli")
        .join("sessions");
    if !kimi_dir.exists() {
        return Ok(Vec::new());
    }

    let mut out = Vec::new();
    for workdir in std::fs::read_dir(&kimi_dir).into_iter().flatten().flatten() {
        if !workdir.path().is_dir() {
            continue;
        }
        for sess in std::fs::read_dir(workdir.path()).into_iter().flatten().flatten() {
            if !sess.path().is_dir() {
                continue;
            }
            let sid = sess.path().file_name().and_then(|n| n.to_str()).unwrap_or_default().to_string();
            if sid.is_empty() {
                continue;
            }
            let mtime = sess.metadata().ok()
                .and_then(|m| m.modified().ok())
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_secs() as i64)
                .unwrap_or(0);
            out.push(NativeSession {
                agent_kind: babel::AgentKind::Kimi,
                native_id: sid,
                project_path: None,
                display_name: None,
                last_prompt: None,
                turn_count: 0,
                last_seen_at: mtime,
                interactive: true,
            });
        }
    }
    Ok(out)
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
