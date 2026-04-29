//! UI rendering for the resume pager
//!
//! Two-panel layout:
//! - Left: Session list with running indicators
//! - Right: Transcript preview with message rendering

use ratatui::prelude::*;
use ratatui::widgets::{Block, Borders, List, ListItem, Paragraph, Wrap};
use scrollparse::MessageKind;

use super::app::{PaneFocus, ResumeApp};
use super::session_list::RunningStatus;

/// Draw the pager UI
pub fn draw(frame: &mut Frame, app: &ResumeApp) {
    let area = frame.area();

    // Reserve bottom row for status bar
    let main_area = Rect {
        height: area.height.saturating_sub(1),
        ..area
    };
    let status_area = Rect {
        y: area.height.saturating_sub(1),
        height: 1,
        ..area
    };

    // Split into left (40%) and right (60%) panels
    let chunks = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(40), Constraint::Percentage(60)])
        .split(main_area);

    draw_session_list(frame, app, chunks[0]);
    draw_transcript(frame, app, chunks[1]);
    draw_status_bar(frame, app, status_area);
}

/// Draw the session list panel
fn draw_session_list(frame: &mut Frame, app: &ResumeApp, area: Rect) {
    let filter_label = if app.sessions.show_all { "all" } else { "cwd" };
    let title = if app.is_searching {
        format!("Sessions [{}] /{}", filter_label, app.search_buffer)
    } else {
        format!("Sessions [{}]", filter_label)
    };

    let border_style = if app.focus == PaneFocus::Sessions {
        Style::default().fg(Color::Cyan)
    } else {
        Style::default().fg(Color::DarkGray)
    };

    let block = Block::default()
        .title(title)
        .borders(Borders::ALL)
        .border_style(border_style);

    let inner = block.inner(area);
    frame.render_widget(block, area);

    // Build list items
    let visible = app.sessions.visible_sessions();
    let items: Vec<ListItem> = visible
        .iter()
        .enumerate()
        .map(|(idx, (_, session))| {
            let is_selected = idx == app.sessions.cursor;
            render_session_item(session, is_selected)
        })
        .collect();

    if items.is_empty() {
        let empty_msg = if app.sessions.filter_query.is_empty() {
            "No sessions found"
        } else {
            "No matches"
        };
        let para = Paragraph::new(empty_msg).style(Style::default().fg(Color::DarkGray));
        frame.render_widget(para, inner);
        return;
    }

    // Calculate visible range for scrolling
    let list_height = inner.height as usize;
    let cursor = app.sessions.cursor;

    // Keep cursor visible
    let scroll_offset = if cursor < app.sessions.scroll_offset {
        cursor
    } else if cursor >= app.sessions.scroll_offset + list_height {
        cursor.saturating_sub(list_height - 1)
    } else {
        app.sessions.scroll_offset
    };

    // Create list widget
    let list = List::new(items).highlight_style(Style::default().add_modifier(Modifier::BOLD));

    // Render with scroll offset
    let list_state = &mut ratatui::widgets::ListState::default();
    list_state.select(Some(cursor.saturating_sub(scroll_offset)));

    frame.render_stateful_widget(list, inner, list_state);
}

/// Render a single session item
fn render_session_item(
    session: &super::session_list::EnrichedSession,
    is_selected: bool,
) -> ListItem<'static> {
    let info = &session.info;

    // Running status indicator
    let status_char = session.running_status.indicator();
    let status_style = match &session.running_status {
        RunningStatus::Inactive => Style::default().fg(Color::DarkGray),
        RunningStatus::Active { .. } => Style::default().fg(Color::Green),
    };

    // Selection indicator
    let sel_char = if is_selected { '▸' } else { ' ' };

    // Summary (truncated)
    let summary = info
        .summaries
        .first()
        .map(|s| s.summary.as_str())
        .unwrap_or("(no summary)");
    let summary_truncated: String = summary.chars().take(35).collect();
    let summary_display = if summary.len() > 35 {
        format!("{}…", summary_truncated)
    } else {
        summary_truncated
    };

    // Session ID (first 8 chars)
    let id_short = &info.session_id[..8.min(info.session_id.len())];

    // Message count
    let msg_count = info.message_count;

    // Relative timestamp
    let time_str = info
        .last_timestamp
        .as_ref()
        .and_then(|ts| format_relative_time(ts))
        .unwrap_or_else(|| "?".to_string());

    // Project path (abbreviated)
    let project_display = abbreviate_path(&info.project);

    // Build the display lines
    let line1 = Line::from(vec![
        Span::raw(format!("{}", sel_char)),
        Span::styled(format!("{} ", status_char), status_style),
        Span::styled(id_short.to_string(), Style::default().fg(Color::Blue)),
        Span::raw(" "),
        Span::styled(
            summary_display,
            Style::default().fg(if is_selected {
                Color::White
            } else {
                Color::Gray
            }),
        ),
    ]);

    let line2 = Line::from(vec![
        Span::raw("   └─ "),
        Span::styled(project_display, Style::default().fg(Color::DarkGray)),
        Span::raw(" "),
        Span::styled(
            format!("{}m", msg_count),
            Style::default().fg(Color::DarkGray),
        ),
        Span::raw(" "),
        Span::styled(time_str, Style::default().fg(Color::DarkGray)),
    ]);

    ListItem::new(vec![line1, line2])
}

/// Draw the transcript preview panel
fn draw_transcript(frame: &mut Frame, app: &ResumeApp, area: Rect) {
    let title = match &app.transcript.session_id {
        Some(id) => format!("Transcript [{}]", &id[..8.min(id.len())]),
        None => "Transcript".to_string(),
    };

    let border_style = if app.focus == PaneFocus::Transcript {
        Style::default().fg(Color::Cyan)
    } else {
        Style::default().fg(Color::DarkGray)
    };

    let block = Block::default()
        .title(title)
        .borders(Borders::ALL)
        .border_style(border_style);

    let inner = block.inner(area);
    frame.render_widget(block, area);

    if app.transcript.messages.is_empty() {
        let msg = if app.transcript.session_id.is_some() {
            "Loading..."
        } else {
            "Select a session to view transcript"
        };
        let para = Paragraph::new(msg).style(Style::default().fg(Color::DarkGray));
        frame.render_widget(para, inner);
        return;
    }

    // Render messages
    let mut lines: Vec<Line> = Vec::new();

    for msg in app
        .transcript
        .messages
        .iter()
        .skip(app.transcript.scroll_offset)
    {
        let (prefix, style) = match &msg.kind {
            MessageKind::User => ("> ", Style::default().fg(Color::Green)),
            MessageKind::Assistant => ("● ", Style::default().fg(Color::Cyan)),
            MessageKind::ToolCall { name, args } => {
                // Format tool call header
                let tool_line = if args.is_empty() {
                    format!("● {}", name)
                } else {
                    format!("● {}({})", name, truncate_str(args, 30))
                };
                lines.push(Line::from(Span::styled(
                    tool_line,
                    Style::default().fg(Color::Yellow),
                )));
                continue;
            }
            MessageKind::ToolOutput => ("  ⎿ ", Style::default().fg(Color::DarkGray)),
            MessageKind::Status => (
                "",
                Style::default()
                    .fg(Color::DarkGray)
                    .add_modifier(Modifier::DIM),
            ),
        };

        // Split content into lines and add prefix to first
        let content_lines: Vec<&str> = msg.content.lines().collect();
        for (i, line) in content_lines.iter().enumerate() {
            let line_prefix = if i == 0 { prefix } else { "  " };
            let truncated = truncate_str(line, inner.width as usize - 4);
            lines.push(Line::from(vec![
                Span::styled(line_prefix.to_string(), style),
                Span::styled(truncated, style),
            ]));
        }

        // Add blank line between messages for readability
        lines.push(Line::from(""));
    }

    let para = Paragraph::new(lines).wrap(Wrap { trim: false });
    frame.render_widget(para, inner);
}

/// Draw the status bar at the bottom
fn draw_status_bar(frame: &mut Frame, app: &ResumeApp, area: Rect) {
    let session_count = app.sessions.visible_sessions().len();
    let total = app.sessions.sessions.len();

    let keybinds = if app.is_searching {
        "Enter:confirm  Esc:cancel"
    } else {
        "Tab:cwd/all  j/k:nav  Enter:resume  /:search  q:quit"
    };

    let left = format!(" {} of {} sessions", session_count, total);
    let right = format!("{} ", keybinds);

    let left_len = left.len();
    let right_len = right.len();
    let padding = area.width as usize - left_len - right_len;

    let line = Line::from(vec![
        Span::styled(left, Style::default().fg(Color::DarkGray)),
        Span::raw(" ".repeat(padding.max(1))),
        Span::styled(right, Style::default().fg(Color::DarkGray)),
    ]);

    let para = Paragraph::new(line).style(Style::default().bg(Color::DarkGray).fg(Color::White));
    frame.render_widget(para, area);
}

// === Helper functions ===

/// Format timestamp as relative time (e.g., "2h ago", "3d ago")
fn format_relative_time(timestamp: &str) -> Option<String> {
    use chrono::{DateTime, Utc};

    let dt: DateTime<Utc> = timestamp.parse().ok()?;
    let now = Utc::now();
    let duration = now.signed_duration_since(dt);

    let secs = duration.num_seconds();
    if secs < 60 {
        Some("now".to_string())
    } else if secs < 3600 {
        Some(format!("{}m", secs / 60))
    } else if secs < 86400 {
        Some(format!("{}h", secs / 3600))
    } else if secs < 604800 {
        Some(format!("{}d", secs / 86400))
    } else {
        Some(format!("{}w", secs / 604800))
    }
}

/// Abbreviate a path for display
fn abbreviate_path(path: &std::path::Path) -> String {
    let path_str = path.to_string_lossy();

    // Replace home dir with ~
    if let Some(home) = dirs::home_dir() {
        let home_str = home.to_string_lossy();
        if path_str.starts_with(home_str.as_ref()) {
            let shortened = format!("~{}", &path_str[home_str.len()..]);
            return truncate_str(&shortened, 30).to_string();
        }
    }

    truncate_str(&path_str, 30).to_string()
}

/// Truncate a string to max length, adding ellipsis
fn truncate_str(s: &str, max_len: usize) -> String {
    if s.len() <= max_len {
        s.to_string()
    } else {
        format!("{}…", &s[..max_len.saturating_sub(1)])
    }
}
