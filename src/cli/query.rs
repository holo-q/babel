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
