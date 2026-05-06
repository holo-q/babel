//! UI rendering for the resume pager
//!
//! Resume pager layout:
//! - Session list with running indicators
//! - Optional transcript preview with message rendering

use ratatui::prelude::*;
use ratatui::widgets::{Block, Borders, List, ListItem, Paragraph, Wrap};
use scrollparse::MessageKind;

use crate::session_row::{self, SessionRow, StateKind};

use super::app::{PaneFocus, ResumeApp};

const SELECTION_BG: Color = Color::Rgb(36, 54, 72);

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

    if app.show_transcript {
        // Keep the session list wide enough to preserve the `ls-sessions` row grammar.
        let chunks = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Percentage(62), Constraint::Percentage(38)])
            .split(main_area);

        draw_session_list(frame, app, chunks[0]);
        draw_transcript(frame, app, chunks[1]);
    } else {
        draw_session_list(frame, app, main_area);
    }
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

    let title_style = if app.focus == PaneFocus::Sessions {
        Style::default()
            .fg(Color::Cyan)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(Color::DarkGray)
    };
    let header = Rect { height: 1, ..area };
    frame.render_widget(Paragraph::new(title).style(title_style), header);
    let inner = Rect {
        y: area.y.saturating_add(1),
        height: area.height.saturating_sub(1),
        ..area
    };

    let visible = app.sessions.visible_sessions();
    let list_height = inner.height as usize;
    let cursor = app.sessions.cursor;
    let now = unix_now();
    let visible_rows: Vec<(usize, SessionRow)> = visible
        .iter()
        .map(|(idx, session)| (*idx, session.row(now)))
        .collect();
    let widths = RowWidths::measure(visible_rows.iter().map(|(idx, row)| (*idx, row)));

    // Keep cursor visible
    let scroll_offset = if cursor < app.sessions.scroll_offset {
        cursor
    } else if cursor >= app.sessions.scroll_offset + list_height {
        cursor.saturating_sub(list_height.saturating_sub(1))
    } else {
        app.sessions.scroll_offset
    };

    let items: Vec<ListItem> = visible_rows
        .iter()
        .skip(scroll_offset)
        .take(list_height.max(1))
        .enumerate()
        .map(|(idx, (global_idx, row))| {
            let is_selected = idx + scroll_offset == app.sessions.cursor;
            render_session_item(*global_idx, row, &widths, is_selected)
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

    let list = List::new(items).highlight_style(Style::default().bg(SELECTION_BG));

    // Render with scroll offset
    let list_state = &mut ratatui::widgets::ListState::default();
    list_state.select(Some(cursor.saturating_sub(scroll_offset)));

    frame.render_stateful_widget(list, inner, list_state);
}

/// Render a single session item with the same cell order as `ls-sessions`.
fn render_session_item(
    idx: usize,
    row: &SessionRow,
    widths: &RowWidths,
    is_selected: bool,
) -> ListItem<'static> {
    let accent = session_row::closest_ansi256_from_hex(row.accent);
    let running = row.is_running();
    let selected_bg = is_selected.then_some(SELECTION_BG);
    let gap = selected_style(row_style(Style::default(), running), selected_bg);
    let harness_style = if row.bright {
        Style::default().fg(Color::Indexed(accent))
    } else {
        Style::default().fg(Color::DarkGray)
    };
    let harness_style = selected_style(row_style(harness_style, running), selected_bg);
    let text_style = if row.bright {
        Style::default().fg(Color::Indexed(accent))
    } else {
        Style::default().fg(Color::DarkGray)
    };
    let text_style = selected_style(row_style(text_style, running), selected_bg);
    let state_style = if row.bright {
        state_style(row.state_kind)
    } else {
        Style::default().fg(Color::DarkGray)
    };
    let state_style = selected_style(row_style(state_style, running), selected_bg);
    let row_dim = selected_style(
        row_style(Style::default().fg(Color::DarkGray), running),
        selected_bg,
    );
    let raw_idx_style = if is_selected {
        Style::default()
            .fg(Color::White)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default()
    };
    let idx_style = selected_style(row_style(raw_idx_style, running), selected_bg);

    let title_style = if row.has_title && row.bright {
        Style::default()
            .fg(Color::Indexed(accent))
            .add_modifier(Modifier::BOLD | Modifier::ITALIC)
    } else if row.has_title {
        Style::default()
            .fg(Color::DarkGray)
            .add_modifier(Modifier::BOLD | Modifier::ITALIC)
    } else if row.bright {
        Style::default().fg(Color::Indexed(accent))
    } else {
        Style::default().fg(Color::DarkGray)
    };
    let title_style = selected_style(row_style(title_style, running), selected_bg);

    let line = Line::from(vec![
        Span::styled(" ", gap),
        Span::styled(row.state_icon, state_style),
        Span::styled(" ", gap),
        Span::styled(
            format!("{:<w$}", row.harness, w = widths.harness),
            harness_style,
        ),
        Span::styled("  ", gap),
        Span::styled(
            format!("{:>w$}", row.workspace, w = widths.workspace),
            row_dim,
        ),
        Span::styled("  ", gap),
        Span::styled(format!("{:<w$}", row.cwd, w = widths.cwd), row_dim),
        Span::styled("  ", gap),
        Span::styled(row.filter_tag, row_dim),
        Span::styled(" ", gap),
        Span::styled(format!("{:>w$}", row.time, w = widths.time), row_dim),
        Span::styled("  ", gap),
        Span::styled(format!("{:>w$}", row.turns, w = widths.turns), row_dim),
        Span::styled("  ", gap),
        Span::styled(format!("{:>w$}", idx + 1, w = widths.index), idx_style),
        Span::styled("  ", gap),
        Span::styled(format!("{:<w$}", row.title, w = widths.title), title_style),
        Span::styled("  ", gap),
        Span::styled(
            format!("{:<w$}", row.last_prompt, w = widths.prompt),
            text_style,
        ),
    ]);

    ListItem::new(line)
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
        "Tab:cwd/all  r:refresh  t:transcript  j/k:nav  Enter:launch  /:search  q:quit"
    };

    let left = if app.status_message.is_empty() {
        format!(" {} of {} sessions", session_count, total)
    } else {
        format!(
            " {} of {} sessions  {}",
            session_count, total, app.status_message
        )
    };
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

#[derive(Default)]
struct RowWidths {
    index: usize,
    harness: usize,
    workspace: usize,
    cwd: usize,
    time: usize,
    turns: usize,
    title: usize,
    prompt: usize,
}

impl RowWidths {
    fn measure<'a>(rows: impl Iterator<Item = (usize, &'a SessionRow)>) -> Self {
        let mut widths = Self::default();

        for (idx, row) in rows {
            widths.index = widths.index.max(format!("{}", idx + 1).len());
            widths.harness = widths.harness.max(row.harness.len());
            widths.workspace = widths.workspace.max(row.workspace.len());
            widths.cwd = widths.cwd.max(row.cwd.len());
            widths.time = widths.time.max(row.time.len());
            widths.turns = widths.turns.max(row.turns.len());
            widths.title = widths.title.max(row.title.chars().count());
            widths.prompt = widths.prompt.max(row.last_prompt.chars().count());
        }

        widths.index = widths.index.max(1);
        widths
    }
}

fn unix_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
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

fn row_style(style: Style, running: bool) -> Style {
    if running {
        style.add_modifier(Modifier::UNDERLINED)
    } else {
        style
    }
}

fn selected_style(style: Style, selected_bg: Option<Color>) -> Style {
    if let Some(bg) = selected_bg {
        style.bg(bg)
    } else {
        style
    }
}

fn state_style(state_kind: StateKind) -> Style {
    match state_kind {
        StateKind::Idle => Style::default().fg(Color::DarkGray),
        StateKind::Working => Style::default().fg(Color::Yellow),
        StateKind::ToolRunning => Style::default()
            .fg(Color::Cyan)
            .add_modifier(Modifier::BOLD),
        StateKind::Thinking => Style::default().fg(Color::Yellow),
        StateKind::PlanApproval => Style::default().fg(Color::Magenta),
        StateKind::AwaitingInput => Style::default().fg(Color::Green),
        StateKind::BackgroundTask => Style::default().fg(Color::Magenta),
        StateKind::Unknown | StateKind::NotRunning => Style::default().fg(Color::DarkGray),
    }
}
