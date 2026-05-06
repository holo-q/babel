//! UI rendering for the resume pager
//!
//! Two-panel layout:
//! - Left: Session list with running indicators
//! - Right: Transcript preview with message rendering

use ratatui::prelude::*;
use ratatui::widgets::{Block, Borders, List, ListItem, Paragraph, Wrap};
use scrollparse::MessageKind;

use crate::ActivityState;

use super::app::{PaneFocus, ResumeApp};
use super::session_list::{EnrichedSession, RunningStatus};

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

    let visible = app.sessions.visible_sessions();
    let list_height = inner.height as usize;
    let cursor = app.sessions.cursor;

    // Keep cursor visible
    let scroll_offset = if cursor < app.sessions.scroll_offset {
        cursor
    } else if cursor >= app.sessions.scroll_offset + list_height {
        cursor.saturating_sub(list_height.saturating_sub(1))
    } else {
        app.sessions.scroll_offset
    };

    let items: Vec<ListItem> = visible
        .iter()
        .skip(scroll_offset)
        .take(list_height.max(1))
        .enumerate()
        .map(|(idx, (_, session))| {
            let is_selected = idx + scroll_offset == app.sessions.cursor;
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

    // Create list widget
    let list = List::new(items).highlight_style(Style::default().add_modifier(Modifier::BOLD));

    // Render with scroll offset
    let list_state = &mut ratatui::widgets::ListState::default();
    list_state.select(Some(cursor.saturating_sub(scroll_offset)));

    frame.render_stateful_widget(list, inner, list_state);
}

/// Render a single session item
fn render_session_item(session: &EnrichedSession, is_selected: bool) -> ListItem<'static> {
    let accent = closest_ansi256_from_hex(session.agent_kind.accent_color());
    let bright =
        session.interactive && !session.hidden && !session.command_only && session.turn_count > 1;
    let harness_style = if bright {
        Style::default().fg(Color::Indexed(accent))
    } else {
        Style::default().fg(Color::DarkGray)
    };
    let text_style = if bright {
        Style::default().fg(Color::Indexed(accent))
    } else {
        Style::default().fg(Color::DarkGray)
    };
    let state_style = state_style(&session.running_status, bright);

    // Selection indicator
    let sel_char = if is_selected { '▸' } else { ' ' };

    let marker = if let Some(icon) = &session.custom_icon {
        icon.clone()
    } else if session.unread {
        "●".to_string()
    } else {
        " ".to_string()
    };

    let workspace = match &session.running_status {
        RunningStatus::Active {
            workspace: Some(ws),
            ..
        } => format!("{}", ws + 1),
        _ => String::new(),
    };

    let cwd = session
        .project_path
        .as_ref()
        .map(|p| abbreviate_path(p, 34))
        .unwrap_or_default();
    let title = sanitize_display(&session.title(), 42);
    let last_prompt = session
        .last_prompt
        .as_deref()
        .map(|p| sanitize_display(p, 42))
        .unwrap_or_default();
    let turns = if session.turn_count > 0 {
        format!("{}t", session.turn_count)
    } else {
        String::new()
    };
    let time = format_relative_time(session.last_seen_at);
    let title_style = if session.has_real_title() && bright {
        Style::default()
            .fg(Color::Indexed(accent))
            .add_modifier(Modifier::BOLD | Modifier::ITALIC)
    } else if session.has_real_title() {
        Style::default()
            .fg(Color::DarkGray)
            .add_modifier(Modifier::BOLD | Modifier::ITALIC)
    } else {
        text_style
    };

    let line1 = Line::from(vec![
        Span::raw(format!("{}", sel_char)),
        Span::styled(format!("{} ", marker), Style::default().fg(Color::Yellow)),
        Span::styled(format!("{:<8}", session.agent_kind.slug()), harness_style),
        Span::raw(" "),
        Span::styled(
            format!("{:<2}", session.running_status.indicator()),
            state_style,
        ),
        Span::raw(" "),
        Span::styled(
            format!("{:>2}", workspace),
            Style::default().fg(Color::DarkGray),
        ),
        Span::raw(" "),
        Span::styled(format!("{:<34}", cwd), Style::default().fg(Color::DarkGray)),
        Span::raw(" "),
        Span::styled(session.filter_tag(), Style::default().fg(Color::DarkGray)),
        Span::raw(" "),
        Span::styled(format!("{:>4}", time), Style::default().fg(Color::DarkGray)),
        Span::raw(" "),
        Span::styled(
            format!("{:>5}", turns),
            Style::default().fg(Color::DarkGray),
        ),
    ]);

    let line2 = Line::from(vec![
        Span::raw("   "),
        Span::styled(title, title_style),
        Span::raw("  "),
        Span::styled(last_prompt, text_style),
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
        let msg =
            app.transcript
                .notice
                .as_deref()
                .unwrap_or(if app.transcript.session_id.is_some() {
                    "Loading..."
                } else {
                    "Select a session to view transcript"
                });
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
    let padding = (area.width as usize).saturating_sub(left_len + right_len);

    let line = Line::from(vec![
        Span::styled(left, Style::default().fg(Color::DarkGray)),
        Span::raw(" ".repeat(padding.max(1))),
        Span::styled(right, Style::default().fg(Color::DarkGray)),
    ]);

    let para = Paragraph::new(line).style(Style::default().bg(Color::DarkGray).fg(Color::White));
    frame.render_widget(para, area);
}

// === Helper functions ===

/// Format unix seconds as relative time (same compact shape as ls-sessions).
fn format_relative_time(last_seen_at: i64) -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64;
    let secs = now - last_seen_at;
    if secs < 0 {
        return "now".to_string();
    }
    if secs < 60 {
        format!("{}s", secs)
    } else if secs < 3_600 {
        format!("{}m", secs / 60)
    } else if secs < 86_400 {
        format!("{}h", secs / 3_600)
    } else if secs < 2_592_000 {
        format!("{}d", secs / 86_400)
    } else {
        format!("{}mo", secs / 2_592_000)
    }
}

/// Abbreviate a path for display
fn abbreviate_path(path: &std::path::Path, max_len: usize) -> String {
    let path_str = path.to_string_lossy();

    // Replace home dir with ~
    if let Some(home) = dirs::home_dir() {
        let home_str = home.to_string_lossy();
        if path_str.starts_with(home_str.as_ref()) {
            let shortened = format!("~{}", &path_str[home_str.len()..]);
            return truncate_str(&shortened, max_len).to_string();
        }
    }

    truncate_str(&path_str, max_len).to_string()
}

fn sanitize_display(s: &str, max_chars: usize) -> String {
    let clean = s.replace('\n', "↵").replace('\r', "");
    if clean.chars().count() > max_chars {
        let short: String = clean.chars().take(max_chars).collect();
        format!("{}…", short)
    } else {
        clean
    }
}

/// Truncate a string to max length, adding ellipsis
fn truncate_str(s: &str, max_len: usize) -> String {
    if s.chars().count() <= max_len {
        s.to_string()
    } else {
        let short: String = s.chars().take(max_len.saturating_sub(1)).collect();
        format!("{}…", short)
    }
}

fn closest_ansi256_from_hex(hex: &str) -> u8 {
    let Some(hex) = hex.strip_prefix('#') else {
        return 8;
    };
    if hex.len() != 6 {
        return 8;
    }
    let r = u8::from_str_radix(&hex[0..2], 16).unwrap_or(102);
    let g = u8::from_str_radix(&hex[2..4], 16).unwrap_or(102);
    let b = u8::from_str_radix(&hex[4..6], 16).unwrap_or(102);
    let ri = ((r as u16) * 5 / 255) as u8;
    let gi = ((g as u16) * 5 / 255) as u8;
    let bi = ((b as u16) * 5 / 255) as u8;
    16 + 36 * ri + 6 * gi + bi
}

fn state_style(status: &RunningStatus, bright: bool) -> Style {
    if !bright {
        return Style::default().fg(Color::DarkGray);
    }

    match status {
        RunningStatus::Inactive => Style::default().fg(Color::DarkGray),
        RunningStatus::Active {
            hook_state,
            activity_state,
            ..
        } => match (*hook_state, activity_state) {
            (Some(crate::babel_storage::HookState::ToolRunning), _)
            | (Some(crate::babel_storage::HookState::Working), ActivityState::ToolUse)
            | (None, ActivityState::ToolUse) => Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
            (Some(crate::babel_storage::HookState::Working), ActivityState::PlanApproval)
            | (Some(crate::babel_storage::HookState::Working), ActivityState::BackgroundTask)
            | (None, ActivityState::PlanApproval)
            | (None, ActivityState::BackgroundTask) => Style::default().fg(Color::Magenta),
            (None, ActivityState::AwaitingInput) => Style::default().fg(Color::Green),
            (Some(crate::babel_storage::HookState::Idle), _)
            | (None, ActivityState::Idle)
            | (None, ActivityState::Unknown) => Style::default().fg(Color::DarkGray),
            _ => Style::default().fg(Color::Yellow),
        },
    }
}
