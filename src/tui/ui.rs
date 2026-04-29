//! TUI Rendering - Layout construction and pane rendering
//!
//! Implements the 4-pane layout with 65/35 vertical split:
//! - Top 65%: Windows (25%) | Fired (25%) | Details (50%)
//! - Bottom 35%: IPC Log (full width)

use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, List, ListItem, Paragraph, Wrap},
    Frame,
};

use super::app::{DetailContent, Pane, TuiApp};
use super::ipc_client::IpcDirection;

/// Main draw function - renders the entire UI
pub fn draw(f: &mut Frame, app: &TuiApp) {
    let size = f.area();

    // Main vertical split: header, content, footer
    let main_chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1), // Header
            Constraint::Min(0),    // Content
            Constraint::Length(1), // Footer
        ])
        .split(size);

    draw_header(f, main_chunks[0], app);
    draw_content(f, main_chunks[1], app);
    draw_footer(f, main_chunks[2], app);

    // Help overlay
    if app.show_help {
        draw_help_overlay(f, size);
    }
}

/// Draw header bar with daemon status
fn draw_header(f: &mut Frame, area: Rect, app: &TuiApp) {
    let uptime_secs = app.daemon_uptime.as_secs();
    let hours = uptime_secs / 3600;
    let mins = (uptime_secs % 3600) / 60;

    let header_text = format!(
        " babel tui                                          [daemon: ●] uptime: {}h {}m",
        hours, mins
    );

    let header =
        Paragraph::new(header_text).style(Style::default().fg(Color::White).bg(Color::DarkGray));
    f.render_widget(header, area);
}

/// Draw footer with keybind hints
fn draw_footer(f: &mut Frame, area: Rect, app: &TuiApp) {
    let hints = if app.active_pane == Pane::IpcLog {
        "[q]uit [r]efresh [Tab]cycle [F1-F4]pane [a]uto-scroll [c]lear [?]help"
    } else {
        "[q]uit [r]efresh [Tab]cycle [F1-F4]pane [Enter]select [?]help"
    };

    let footer = Paragraph::new(hints).style(Style::default().fg(Color::DarkGray));
    f.render_widget(footer, area);
}

/// Draw main content area (65/35 split)
fn draw_content(f: &mut Frame, area: Rect, app: &TuiApp) {
    // 65/35 vertical split
    let content_chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Percentage(65), // Top panes
            Constraint::Percentage(35), // IPC Log
        ])
        .split(area);

    // Top section: 3 columns (25% | 25% | 50%)
    let top_chunks = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage(25), // Windows
            Constraint::Percentage(25), // Fired
            Constraint::Percentage(50), // Details
        ])
        .split(content_chunks[0]);

    draw_windows_pane(f, top_chunks[0], app);
    draw_fired_pane(f, top_chunks[1], app);
    draw_details_pane(f, top_chunks[2], app);
    draw_ipc_log_pane(f, content_chunks[1], app);
}

/// Draw the Windows pane (F1) - shows ALL terminals, not just agents.
///
/// Agent sessions show activity state indicators (⚡⚙◆○).
/// Non-agent terminals show dimmed with "-" indicator
fn draw_windows_pane(f: &mut Frame, area: Rect, app: &TuiApp) {
    let is_active = app.active_pane == Pane::Windows;
    let border_style = pane_border_style(is_active);

    // Count agent vs non-agent terminals for title
    let agent_count = app.terminals.iter().filter(|t| t.is_agent).count();
    let total_count = app.terminals.len();
    let title = format!(" Terminals [F1] ({}/{}) ", agent_count, total_count);

    let block = Block::default()
        .title(title)
        .borders(Borders::ALL)
        .border_style(border_style);

    if app.terminals.is_empty() {
        let para = Paragraph::new("No terminals")
            .style(Style::default().fg(Color::DarkGray))
            .block(block);
        f.render_widget(para, area);
        return;
    }

    let items: Vec<ListItem> = app
        .terminals
        .iter()
        .enumerate()
        .map(|(i, term)| {
            // For agent panes, look up activity state from windows list
            let (indicator, ind_style) = if term.is_agent {
                // Find matching agent pane to get activity state
                let activity = app
                    .windows
                    .iter()
                    .find(|w| w.id() == term.addr.id)
                    .and_then(|w| w.activity_state);

                use scrollparse::claude::ActivityState;
                // The worker's breath — what state holds their soul
                match activity {
                    Some(ActivityState::Thinking) => ("⚡", Style::default().fg(Color::Yellow)), // soul processing, inference running
                    Some(ActivityState::ToolUse) => ("⚙", Style::default().fg(Color::Cyan)), // hands moving in the world
                    Some(ActivityState::PlanApproval) => {
                        ("📋", Style::default().fg(Color::Magenta))
                    } // awaiting the user's blessing
                    Some(ActivityState::AwaitingInput) => ("◆", Style::default().fg(Color::Green)), // worker awaits the user's voice
                    Some(ActivityState::BackgroundTask) => {
                        ("◐", Style::default().fg(Color::Magenta))
                    } // working in shadow
                    Some(ActivityState::Idle) => ("○", Style::default().fg(Color::DarkGray)), // resting, awaiting the next word
                    Some(ActivityState::Unknown) | None => ("●", Style::default().fg(Color::Blue)), // breath obscured
                }
            } else {
                // Non-agent terminal - dimmed indicator
                ("-", Style::default().fg(Color::DarkGray))
            };

            let title = if term.title.is_empty() {
                "untitled"
            } else {
                &term.title
            };
            let truncated = if title.len() > 16 {
                format!("{}...", &title[..13])
            } else {
                title.to_string()
            };

            let base_style = if i == app.window_selected && is_active {
                Style::default()
                    .bg(Color::DarkGray)
                    .add_modifier(Modifier::BOLD)
            } else if !term.is_agent {
                // Dim non-agent terminals
                Style::default().fg(Color::DarkGray)
            } else {
                Style::default()
            };

            let line = Line::from(vec![
                Span::styled(indicator, ind_style),
                Span::raw(format!(" {:>3} ", term.addr.id)),
                Span::raw(truncated),
            ]);

            ListItem::new(line).style(base_style)
        })
        .collect();

    let list = List::new(items).block(block);
    f.render_widget(list, area);
}

/// Draw the Fired Tasks pane (F2)
fn draw_fired_pane(f: &mut Frame, area: Rect, app: &TuiApp) {
    let is_active = app.active_pane == Pane::Fired;
    let border_style = pane_border_style(is_active);

    let block = Block::default()
        .title(" Fired Tasks [F2] ")
        .borders(Borders::ALL)
        .border_style(border_style);

    if app.fired_tasks.is_empty() {
        let para = Paragraph::new("No fired tasks")
            .style(Style::default().fg(Color::DarkGray))
            .block(block);
        f.render_widget(para, area);
        return;
    }

    let items: Vec<ListItem> = app
        .fired_tasks
        .iter()
        .enumerate()
        .map(|(i, task)| {
            let alive = task.is_alive();
            let indicator = if alive { "🟢" } else { "⚫" };
            let id_short = &task.task_id[..8.min(task.task_id.len())];
            let prompt_preview = if task.prompt_preview.len() > 12 {
                format!("{}...", &task.prompt_preview[..9])
            } else {
                task.prompt_preview.clone()
            };

            let style = if i == app.fired_selected && is_active {
                Style::default()
                    .bg(Color::DarkGray)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default()
            };

            ListItem::new(format!("{} {} {}", indicator, id_short, prompt_preview)).style(style)
        })
        .collect();

    let list = List::new(items).block(block);
    f.render_widget(list, area);
}

/// Draw the Details pane (F3)
fn draw_details_pane(f: &mut Frame, area: Rect, app: &TuiApp) {
    let is_active = app.active_pane == Pane::Details;
    let border_style = pane_border_style(is_active);

    let block = Block::default()
        .title(" Details [F3] ")
        .borders(Borders::ALL)
        .border_style(border_style);

    let content = match &app.detail_content {
        DetailContent::None => "Select an item from Windows or Fired panes".to_string(),
        DetailContent::Window(w) => {
            let state_str = match &w.activity_state {
                Some(s) => format!("{:?}", s),
                None => "?".to_string(),
            };
            format!(
                "Agent Pane {}\n\
                 ─────────────────────────\n\
                 state: {}\n\
                 socket: {}\n\
                 cwd: {}\n\
                 session: {}\n\
                 title: {}\n\
                 focused: {}\n\
                 workspace: {}",
                w.id(),
                state_str,
                w.socket(),
                w.cwd.display(),
                w.session_id.as_deref().unwrap_or("?"),
                if w.title.is_empty() { "?" } else { &w.title },
                w.is_focused,
                w.workspace.map_or("?".to_string(), |n| n.to_string()),
            )
        }
        DetailContent::Terminal(t) => {
            format!(
                "Terminal {} {}\n\
                 ─────────────────────────\n\
                 socket: {}\n\
                 title: {}\n\
                 cwd: {}\n\
                 focused: {}\n\
                 workspace: {}",
                t.addr.id,
                if t.is_agent { "(agent)" } else { "(non-agent)" },
                t.addr.short(),
                if t.title.is_empty() { "?" } else { &t.title },
                t.cwd.display(),
                t.is_focused,
                t.workspace.map_or("?".to_string(), |n| n.to_string()),
            )
        }
        DetailContent::FiredTask(t) => {
            format!(
                "Fired Task\n\
                 ─────────────────────────\n\
                 task_id: {}\n\
                 pid: {}\n\
                 alive: {}\n\
                 prompt: {}\n\
                 workdir: {}\n\
                 ambient: {:?}",
                t.task_id,
                t.pid,
                t.is_alive(),
                t.prompt_preview,
                t.workdir.display(),
                t.ambient_sound,
            )
        }
        DetailContent::IpcMessage(entry) => {
            format!(
                "IPC Message\n\
                 ─────────────────────────\n\
                 time: {}\n\
                 direction: {}\n\n\
                 {}",
                entry.timestamp_str(),
                entry.direction.label(),
                // Pretty-print JSON
                serde_json::from_str::<serde_json::Value>(&entry.content)
                    .map(|v| serde_json::to_string_pretty(&v).unwrap_or(entry.content.clone()))
                    .unwrap_or(entry.content.clone())
            )
        }
    };

    let para = Paragraph::new(content)
        .block(block)
        .wrap(Wrap { trim: false });
    f.render_widget(para, area);
}

/// Draw the IPC Log pane (F4) - primary debugging view
fn draw_ipc_log_pane(f: &mut Frame, area: Rect, app: &TuiApp) {
    let is_active = app.active_pane == Pane::IpcLog;
    let border_style = pane_border_style(is_active);

    let auto_scroll_indicator = if app.ipc_auto_scroll { "▼" } else { " " };
    let title = format!(
        " IPC Log [F4] {} auto-scroll [a] ({}) ",
        auto_scroll_indicator,
        app.ipc_log.len()
    );

    let block = Block::default()
        .title(title)
        .borders(Borders::ALL)
        .border_style(border_style);

    if app.ipc_log.is_empty() {
        let placeholder = Paragraph::new("IPC traffic will appear here...")
            .style(Style::default().fg(Color::DarkGray))
            .block(block);
        f.render_widget(placeholder, area);
        return;
    }

    // Build list items from log entries
    let items: Vec<ListItem> = app
        .ipc_log
        .iter()
        .enumerate()
        .map(|(i, entry)| {
            // Color based on direction
            let (dir_style, dir_label) = match entry.direction {
                IpcDirection::Send => (Style::default().fg(Color::Cyan), "SEND"),
                IpcDirection::Recv => (Style::default().fg(Color::Green), "RECV"),
                IpcDirection::Event => (Style::default().fg(Color::Yellow), "EVNT"),
            };

            // Truncate content for display
            let max_content_len = area.width.saturating_sub(20) as usize;
            let content_display = if entry.content.len() > max_content_len {
                format!("{}...", &entry.content[..max_content_len.saturating_sub(3)])
            } else {
                entry.content.clone()
            };

            let line = Line::from(vec![
                Span::styled(entry.timestamp_str(), Style::default().fg(Color::DarkGray)),
                Span::raw(" "),
                Span::styled(dir_label, dir_style),
                Span::raw(" "),
                Span::raw(content_display),
            ]);

            let style = if i == app.ipc_selected && is_active {
                Style::default()
                    .bg(Color::DarkGray)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default()
            };

            ListItem::new(line).style(style)
        })
        .collect();

    let list = List::new(items).block(block);
    f.render_widget(list, area);
}

/// Draw help overlay
fn draw_help_overlay(f: &mut Frame, area: Rect) {
    let help_text = vec![
        Line::from(Span::styled(
            "Babel TUI Help",
            Style::default().add_modifier(Modifier::BOLD),
        )),
        Line::from(""),
        Line::from("Navigation:"),
        Line::from("  Tab / Shift+Tab  Cycle panes"),
        Line::from("  F1-F4            Jump to pane"),
        Line::from("  j/k, ↑/↓         Navigate lists"),
        Line::from("  Enter            Select item → Details"),
        Line::from(""),
        Line::from("Actions:"),
        Line::from("  r                Force refresh"),
        Line::from("  a                Toggle auto-scroll (IPC log)"),
        Line::from("  c                Clear IPC log"),
        Line::from("  ?                Toggle this help"),
        Line::from("  q, Esc           Quit"),
        Line::from(""),
        Line::from(Span::styled(
            "Press any key to close",
            Style::default().fg(Color::DarkGray),
        )),
    ];

    // Calculate centered overlay area
    let overlay_width = 50;
    let overlay_height = help_text.len() as u16 + 2;
    let x = (area.width.saturating_sub(overlay_width)) / 2;
    let y = (area.height.saturating_sub(overlay_height)) / 2;
    let overlay_area = Rect::new(x, y, overlay_width, overlay_height);

    let help = Paragraph::new(help_text)
        .block(
            Block::default()
                .title(" Help ")
                .borders(Borders::ALL)
                .border_style(Style::default().fg(Color::Yellow)),
        )
        .style(Style::default().bg(Color::Black));

    // Clear area behind overlay
    f.render_widget(ratatui::widgets::Clear, overlay_area);
    f.render_widget(help, overlay_area);
}

/// Get border style for a pane based on active state
fn pane_border_style(is_active: bool) -> Style {
    if is_active {
        Style::default().fg(Color::Cyan)
    } else {
        Style::default().fg(Color::DarkGray)
    }
}
