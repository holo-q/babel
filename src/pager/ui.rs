//! UI rendering for the resume pager
//!
//! Resume pager layout:
//! - Session list with running indicators
//! - Optional transcript preview with message rendering

use std::path::Path;

use ratatui::prelude::*;
use ratatui::widgets::{Block, Borders, Clear, List, ListItem, Paragraph};
use scrollparse::MessageKind;
use serde_json::Value;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::session_row::{self, SessionRow, StateKind};

use super::app::{PaneFocus, ResumeApp};
use super::session_list::CwdDisplayMode;

const SELECTION_BG: Color = Color::Rgb(36, 54, 72);
const USER_PROMPT_BG: Color = Color::Rgb(30, 52, 42);

/// Draw the pager UI
pub fn draw(frame: &mut Frame, app: &mut ResumeApp) {
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

    // Borderless/dynamic panes do not necessarily repaint every cell. Clear the
    // body first so fast cursor movement cannot leave stale row fragments behind.
    frame.render_widget(Clear, main_area);

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
fn draw_session_list(frame: &mut Frame, app: &mut ResumeApp, area: Rect) {
    let cwd_label = app
        .sessions
        .current_cwd
        .as_deref()
        .map(|cwd| cwd_label(cwd, app.sessions.cwd_display_mode))
        .unwrap_or_else(|| "cwd:?".to_string());
    let filter_label = match (app.sessions.show_all, app.sessions.show_hidden) {
        (true, true) => "all+hidden".to_string(),
        (true, false) => "all".to_string(),
        (false, true) => format!("{cwd_label}+hidden"),
        (false, false) => cwd_label,
    };
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

    let scroll_offset = scroll_offset_for_cursor(
        cursor,
        app.sessions.scroll_offset,
        list_height,
        visible.len(),
    );
    app.sessions.scroll_offset = scroll_offset;

    let viewport_rows: Vec<(usize, &SessionRow)> = visible_rows
        .iter()
        .skip(scroll_offset)
        .take(list_height.max(1))
        .map(|(idx, row)| (*idx, row))
        .collect();
    let measured_widths = RowWidths::measure(viewport_rows.iter().map(|(idx, row)| (*idx, *row)));
    let widths = measured_widths.fit(inner.width as usize);

    let items: Vec<ListItem> = viewport_rows
        .iter()
        .enumerate()
        .map(|(idx, (global_idx, row))| {
            let is_selected = idx + scroll_offset == app.sessions.cursor;
            render_session_item(*global_idx, row, &widths, inner.width as usize, is_selected)
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
    row_width: usize,
    is_selected: bool,
) -> ListItem<'static> {
    let accent = session_row::closest_ansi256_from_hex(row.accent);
    let running = row.is_running();
    let selected_bg = is_selected.then_some(SELECTION_BG);
    let gap = selected_style(Style::default(), selected_bg);
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
    let pad_style = selected_style(Style::default(), selected_bg);
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

    let mut line_width = 0;
    let mut spans = Vec::new();
    push_span(&mut spans, &mut line_width, " ", gap);
    push_span(&mut spans, &mut line_width, row.state_icon, state_style);
    push_span(&mut spans, &mut line_width, " ", gap);
    push_right_cell(
        &mut spans,
        &mut line_width,
        &row.harness,
        widths.harness,
        harness_style,
        pad_style,
    );
    push_span(&mut spans, &mut line_width, "  ", gap);
    push_left_cell(
        &mut spans,
        &mut line_width,
        &row.workspace,
        widths.workspace,
        row_dim,
        pad_style,
    );
    push_span(&mut spans, &mut line_width, "  ", gap);
    push_right_cell(
        &mut spans,
        &mut line_width,
        &row.cwd,
        widths.cwd,
        row_dim,
        pad_style,
    );
    push_span(&mut spans, &mut line_width, "  ", gap);
    push_span(&mut spans, &mut line_width, row.filter_tag, row_dim);
    push_span(&mut spans, &mut line_width, " ", gap);
    push_left_cell(
        &mut spans,
        &mut line_width,
        &row.time,
        widths.time,
        row_dim,
        pad_style,
    );
    push_span(&mut spans, &mut line_width, "  ", gap);
    push_left_cell(
        &mut spans,
        &mut line_width,
        &row.turns,
        widths.turns,
        row_dim,
        pad_style,
    );
    push_span(&mut spans, &mut line_width, "  ", gap);
    push_left_cell(
        &mut spans,
        &mut line_width,
        &(idx + 1).to_string(),
        widths.index,
        idx_style,
        pad_style,
    );
    push_span(&mut spans, &mut line_width, "  ", gap);
    push_right_cell(
        &mut spans,
        &mut line_width,
        &row.title,
        widths.title,
        title_style,
        pad_style,
    );
    push_span(&mut spans, &mut line_width, "  ", gap);
    push_right_cell(
        &mut spans,
        &mut line_width,
        &row.last_prompt,
        widths.prompt,
        text_style,
        pad_style,
    );
    if line_width < row_width {
        let trailing = " ".repeat(row_width - line_width);
        push_span(&mut spans, &mut line_width, trailing, gap);
    }

    let line = Line::from(spans);

    ListItem::new(line)
}

/// Draw the transcript preview panel
fn draw_transcript(frame: &mut Frame, app: &mut ResumeApp, area: Rect) {
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

    // Render one physical row per turn by default. Session transcripts often
    // contain pasted blocks, quoted prompts, or command output with many
    // embedded newlines; expanding those inline makes navigation laggy and turns
    // the right pane into a wrapping surface. The preview is a collapsed row:
    // prefix + ABC ... [cut] ... XYZ, with user prompt rows carrying a full-row
    // background so the conversation rhythm remains visible.
    let mut lines: Vec<Line> = Vec::new();

    for msg in &app.transcript.messages {
        let (prefix, style) = match &msg.kind {
            MessageKind::User => ("> ", Style::default().fg(Color::White).bg(USER_PROMPT_BG)),
            MessageKind::Assistant => ("● ", Style::default().fg(Color::Cyan)),
            MessageKind::ToolCall { name, args } => {
                lines.push(collapsed_message_line(
                    "● ",
                    &tool_call_preview(name, args),
                    Style::default().fg(Color::Yellow),
                    inner.width as usize,
                ));
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

        lines.push(collapsed_message_line(
            prefix,
            &msg.content,
            style,
            inner.width as usize,
        ));
    }

    let max_offset = lines.len().saturating_sub(inner.height as usize);
    app.transcript.scroll_offset = app.transcript.scroll_offset.min(max_offset);
    let visible_lines: Vec<Line> = lines
        .into_iter()
        .skip(app.transcript.scroll_offset)
        .collect();

    let para = Paragraph::new(visible_lines);
    frame.render_widget(para, inner);
}

/// Draw the status bar at the bottom
fn draw_status_bar(frame: &mut Frame, app: &ResumeApp, area: Rect) {
    let session_count = app.sessions.visible_sessions().len();
    let total = app.sessions.sessions.len();
    let width = area.width as usize;
    if width == 0 {
        return;
    }

    let keybinds = if app.is_searching {
        "Enter:confirm  Esc:cancel"
    } else {
        "Tab:cwd/all  c:cwd label  h:hidden  H:hide  r:refresh  t:transcript  j/k:nav  Enter:launch  /:search  q:quit"
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

    let left_width = display_width(&left);
    let right_width = display_width(&right);
    let (left, right, padding) = if left_width + right_width <= width {
        (left, right, width.saturating_sub(left_width + right_width))
    } else if right_width < width {
        let left_max = width.saturating_sub(right_width + 1);
        let left = if left_max == 0 {
            String::new()
        } else {
            truncate_str(&left, left_max)
        };
        (left, right, 1)
    } else {
        (String::new(), truncate_str(&right, width), 0)
    };

    let status_style = Style::default().bg(Color::DarkGray).fg(Color::White);
    let muted_status_style = Style::default().bg(Color::DarkGray).fg(Color::Gray);

    let line = Line::from(vec![
        Span::styled(left, muted_status_style),
        Span::styled(" ".repeat(padding), status_style),
        Span::styled(right, status_style),
    ]);

    let para = Paragraph::new(line).style(status_style);
    frame.render_widget(para, area);
}

// === Helper functions ===

fn cwd_label(cwd: &Path, mode: CwdDisplayMode) -> String {
    let value = match mode {
        CwdDisplayMode::Relative => relative_cwd_label(cwd),
        CwdDisplayMode::Absolute => cwd.display().to_string(),
        CwdDisplayMode::Project => cwd
            .file_name()
            .and_then(|name| name.to_str())
            .map(str::to_string)
            .unwrap_or_else(|| cwd.display().to_string()),
    };
    format!("cwd:{value}")
}

fn relative_cwd_label(cwd: &Path) -> String {
    if let Some(home) = dirs::home_dir() {
        if let Ok(relative) = cwd.strip_prefix(&home) {
            let text = relative.display().to_string();
            if !text.is_empty() {
                return text;
            }
        }
    }
    session_row::abbreviate_path(cwd, 72)
}

#[derive(Clone, Copy, Default)]
struct RowWidths {
    state: usize,
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
            widths.state = widths.state.max(display_width(row.state_icon));
            widths.index = widths.index.max(format!("{}", idx + 1).len());
            widths.harness = widths.harness.max(row.harness.len());
            widths.workspace = widths.workspace.max(display_width(&row.workspace));
            widths.cwd = widths.cwd.max(display_width(&row.cwd));
            widths.time = widths.time.max(display_width(&row.time));
            widths.turns = widths.turns.max(display_width(&row.turns));
            widths.title = widths.title.max(display_width(&row.title));
            widths.prompt = widths.prompt.max(display_width(&row.last_prompt));
        }

        widths.state = widths.state.max(1);
        widths.index = widths.index.max(1);
        widths
    }

    fn fit(mut self, row_width: usize) -> Self {
        if row_width == 0 {
            return self;
        }

        let min_total = self.total_width();
        if min_total < row_width {
            self.distribute_extra(row_width - min_total);
            return self;
        }

        self.shrink_to(row_width);
        self
    }

    fn total_width(&self) -> usize {
        // Leading state cluster:
        // " " + state icon + " " + harness + "  " + workspace + "  " + cwd
        // + "  " + filter + " " + time + "  " + turns + "  " + index + "  "
        // + title + "  " + prompt.
        1 + self.state
            + 1
            + self.harness
            + 2
            + self.workspace
            + 2
            + self.cwd
            + 2
            + 1
            + 1
            + self.time
            + 2
            + self.turns
            + 2
            + self.index
            + 2
            + self.title
            + 2
            + self.prompt
    }

    fn distribute_extra(&mut self, mut extra: usize) {
        // The browser should use the full row, but path/status columns stay
        // compact. The semantic text cells — thread/title and prompt/message —
        // are the useful elastic surfaces.
        let title_extra = extra / 2;
        self.title += title_extra;
        extra = extra.saturating_sub(title_extra);
        self.prompt += extra;
    }

    fn shrink_to(&mut self, row_width: usize) {
        while self.total_width() > row_width && self.prompt > 8 {
            self.prompt -= 1;
        }
        while self.total_width() > row_width && self.title > 8 {
            self.title -= 1;
        }
        while self.total_width() > row_width && self.cwd > 12 {
            self.cwd -= 1;
        }
    }
}

fn push_span(
    spans: &mut Vec<Span<'static>>,
    line_width: &mut usize,
    text: impl Into<String>,
    style: Style,
) {
    let text = text.into();
    *line_width += display_width(&text);
    spans.push(Span::styled(text, style));
}

fn push_left_cell(
    spans: &mut Vec<Span<'static>>,
    line_width: &mut usize,
    text: &str,
    width: usize,
    text_style: Style,
    pad_style: Style,
) {
    let fitted = fit_cell_text(text, width);
    let text_width = display_width(&fitted);
    if text_width < width {
        push_span(spans, line_width, " ".repeat(width - text_width), pad_style);
    }
    push_span(spans, line_width, fitted, text_style);
}

fn push_right_cell(
    spans: &mut Vec<Span<'static>>,
    line_width: &mut usize,
    text: &str,
    width: usize,
    text_style: Style,
    pad_style: Style,
) {
    let fitted = fit_cell_text(text, width);
    push_span(spans, line_width, fitted.clone(), text_style);
    let text_width = display_width(&fitted);
    if text_width < width {
        push_span(spans, line_width, " ".repeat(width - text_width), pad_style);
    }
}

fn fit_cell_text(text: &str, width: usize) -> String {
    if width == 0 {
        String::new()
    } else {
        middle_truncate_str(text, width)
    }
}

fn display_width(text: &str) -> usize {
    UnicodeWidthStr::width(text)
}

fn scroll_offset_for_cursor(
    cursor: usize,
    current_offset: usize,
    viewport_height: usize,
    row_count: usize,
) -> usize {
    if viewport_height == 0 || row_count <= viewport_height {
        return 0;
    }

    let max_offset = row_count.saturating_sub(viewport_height);
    let margin = (viewport_height / 4).clamp(1, 4);
    let upper_edge = current_offset.saturating_add(margin);
    let lower_edge = current_offset
        .saturating_add(viewport_height)
        .saturating_sub(1 + margin);

    let next_offset = if cursor < upper_edge {
        cursor.saturating_sub(margin)
    } else if cursor > lower_edge {
        cursor.saturating_sub(viewport_height.saturating_sub(1 + margin))
    } else {
        current_offset
    };

    next_offset.min(max_offset)
}

fn unix_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}

fn tool_call_preview(name: &str, args: &str) -> String {
    let Ok(input) = serde_json::from_str::<Value>(args) else {
        return raw_tool_call_preview(name, args);
    };

    match name {
        "Bash" => string_field(&input, "command")
            .map(|command| format!("Bash: {}", one_line(command)))
            .unwrap_or_else(|| raw_tool_call_preview(name, args)),
        "exec_command" => string_field(&input, "cmd")
            .or_else(|| string_field(&input, "command"))
            .map(|command| format!("exec: {}", one_line(command)))
            .unwrap_or_else(|| raw_tool_call_preview(name, args)),
        "Edit" => {
            edit_tool_preview("Edit", &input).unwrap_or_else(|| raw_tool_call_preview(name, args))
        }
        "MultiEdit" => {
            multi_edit_tool_preview(&input).unwrap_or_else(|| raw_tool_call_preview(name, args))
        }
        "Write" => {
            file_tool_preview("Write", &input).unwrap_or_else(|| raw_tool_call_preview(name, args))
        }
        "Read" => read_tool_preview(&input).unwrap_or_else(|| raw_tool_call_preview(name, args)),
        "Grep" => grep_tool_preview(&input).unwrap_or_else(|| raw_tool_call_preview(name, args)),
        "Glob" => glob_tool_preview(&input).unwrap_or_else(|| raw_tool_call_preview(name, args)),
        "NotebookEdit" => edit_tool_preview("NotebookEdit", &input)
            .unwrap_or_else(|| raw_tool_call_preview(name, args)),
        "apply_patch" => "apply_patch".to_string(),
        _ => raw_tool_call_preview(name, args),
    }
}

fn raw_tool_call_preview(name: &str, args: &str) -> String {
    if args.is_empty() {
        name.to_string()
    } else {
        format!("{}({})", name, truncate_str(args, 48))
    }
}

fn edit_tool_preview(label: &str, input: &Value) -> Option<String> {
    let path = input_path(input)?;
    let mut preview = format!("{label}: {}", short_path(path));
    if let Some(range) = line_range(input) {
        preview.push(' ');
        preview.push_str(&range);
    } else if let Some(old_string) = string_field(input, "old_string") {
        let line_count = old_string.lines().count().max(1);
        preview.push_str(&format!(" replace {line_count}L"));
    }
    Some(preview)
}

fn multi_edit_tool_preview(input: &Value) -> Option<String> {
    let path = input_path(input)?;
    let edit_count = input
        .get("edits")
        .and_then(Value::as_array)
        .map(|edits| edits.len())
        .unwrap_or(0);
    if edit_count == 0 {
        Some(format!("MultiEdit: {}", short_path(path)))
    } else {
        Some(format!(
            "MultiEdit: {} {edit_count} edits",
            short_path(path)
        ))
    }
}

fn file_tool_preview(label: &str, input: &Value) -> Option<String> {
    let path = input_path(input)?;
    let mut preview = format!("{label}: {}", short_path(path));
    if let Some(range) = line_range(input) {
        preview.push(' ');
        preview.push_str(&range);
    }
    Some(preview)
}

fn read_tool_preview(input: &Value) -> Option<String> {
    let path = input_path(input)?;
    let mut preview = format!("Read: {}", short_path(path));
    if let Some(range) = line_range(input) {
        preview.push(' ');
        preview.push_str(&range);
    } else if let Some(offset) = number_field(input, "offset") {
        preview.push_str(&format!(" L{offset}"));
        if let Some(limit) = number_field(input, "limit") {
            preview.push_str(&format!(
                "-{}",
                offset.saturating_add(limit).saturating_sub(1)
            ));
        }
    }
    Some(preview)
}

fn grep_tool_preview(input: &Value) -> Option<String> {
    let pattern = string_field(input, "pattern")?;
    let scope = string_field(input, "path")
        .or_else(|| string_field(input, "include"))
        .map(short_path);
    Some(match scope {
        Some(scope) => format!("Grep: {} in {}", one_line(pattern), scope),
        None => format!("Grep: {}", one_line(pattern)),
    })
}

fn glob_tool_preview(input: &Value) -> Option<String> {
    let pattern = string_field(input, "pattern")?;
    let scope = string_field(input, "path").map(short_path);
    Some(match scope {
        Some(scope) => format!("Glob: {} in {}", one_line(pattern), scope),
        None => format!("Glob: {}", one_line(pattern)),
    })
}

fn input_path(input: &Value) -> Option<&str> {
    string_field(input, "file_path")
        .or_else(|| string_field(input, "path"))
        .or_else(|| string_field(input, "notebook_path"))
}

fn line_range(input: &Value) -> Option<String> {
    let start = number_field(input, "line")
        .or_else(|| number_field(input, "line_number"))
        .or_else(|| number_field(input, "start_line"));
    let end = number_field(input, "end_line");
    match (start, end) {
        (Some(start), Some(end)) if end != start => Some(format!("L{start}-{end}")),
        (Some(start), _) => Some(format!("L{start}")),
        _ => None,
    }
}

fn string_field<'a>(value: &'a Value, key: &str) -> Option<&'a str> {
    value.get(key)?.as_str()
}

fn number_field(value: &Value, key: &str) -> Option<u64> {
    value.get(key)?.as_u64()
}

fn one_line(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn short_path(path: &str) -> String {
    let home = std::env::var("HOME").ok();
    let compact = home
        .as_deref()
        .and_then(|home| path.strip_prefix(home).map(|rest| format!("~{rest}")))
        .unwrap_or_else(|| path.to_string());

    let normalized = compact.replace('\\', "/");
    let parts: Vec<&str> = normalized
        .split('/')
        .filter(|part| !part.is_empty())
        .collect();
    if parts.len() <= 3 || normalized.starts_with("~") && parts.len() <= 4 {
        return normalized;
    }

    format!("…/{}", parts[parts.len().saturating_sub(3)..].join("/"))
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

fn collapsed_message_line(
    prefix: &str,
    content: &str,
    style: Style,
    row_width: usize,
) -> Line<'static> {
    if row_width == 0 {
        return Line::from("");
    }

    let prefix_width = display_width(prefix);
    let content_width = row_width.saturating_sub(prefix_width);
    let clean = content.split_whitespace().collect::<Vec<_>>().join(" ");
    let preview = middle_truncate_str(&clean, content_width);

    let mut spans = Vec::new();
    let mut line_width = 0;
    push_span(&mut spans, &mut line_width, prefix.to_string(), style);
    push_span(&mut spans, &mut line_width, preview, style);

    if line_width < row_width {
        let pad_width = row_width - line_width;
        push_span(&mut spans, &mut line_width, " ".repeat(pad_width), style);
    }

    Line::from(spans)
}

fn middle_truncate_str(s: &str, max_width: usize) -> String {
    if display_width(s) <= max_width {
        return s.to_string();
    }

    const MARKER: &str = "… [cut] …";
    let marker_width = display_width(MARKER);
    if max_width <= marker_width + 2 {
        return truncate_str(s, max_width);
    }

    let keep_width = max_width - marker_width;
    let head_budget = keep_width / 2;
    let tail_budget = keep_width.saturating_sub(head_budget);
    let head = take_width_from_start(s, head_budget);
    let tail = take_width_from_end(s, tail_budget);
    format!("{head}{MARKER}{tail}")
}

fn take_width_from_start(s: &str, max_width: usize) -> String {
    let mut out = String::new();
    let mut width = 0;
    for ch in s.chars() {
        let ch_width = ch.width().unwrap_or(0);
        if width + ch_width > max_width {
            break;
        }
        out.push(ch);
        width += ch_width;
    }
    out
}

fn take_width_from_end(s: &str, max_width: usize) -> String {
    let mut chars = Vec::new();
    let mut width = 0;
    for ch in s.chars().rev() {
        let ch_width = ch.width().unwrap_or(0);
        if width + ch_width > max_width {
            break;
        }
        chars.push(ch);
        width += ch_width;
    }
    chars.into_iter().rev().collect()
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_bash_tool_call_with_command() {
        let args = r#"{"command":"uv run birdideas timeline 24h --max 5","description":"Verify"}"#;
        assert_eq!(
            tool_call_preview("Bash", args),
            "Bash: uv run birdideas timeline 24h --max 5"
        );
    }

    #[test]
    fn formats_codex_exec_command_with_cmd() {
        let args = r#"{"cmd":"git status --short","workdir":"/home/nuck/holoq/repo-os/babel"}"#;
        assert_eq!(
            tool_call_preview("exec_command", args),
            "exec: git status --short"
        );
    }

    #[test]
    fn formats_edit_tool_call_with_short_path_and_replace_count() {
        let args = r#"{"file_path":"/home/nuck/holoq/repo-os/babel/src/pager/ui.rs","old_string":"a\nb\n","new_string":"c\n"}"#;
        assert_eq!(
            tool_call_preview("Edit", args),
            "Edit: …/src/pager/ui.rs replace 2L"
        );
    }

    #[test]
    fn formats_read_tool_call_with_line_range() {
        let args = r#"{"file_path":"/home/nuck/holoq/repo-os/babel/src/pager/ui.rs","offset":120,"limit":20}"#;
        assert_eq!(
            tool_call_preview("Read", args),
            "Read: …/src/pager/ui.rs L120-139"
        );
    }

    #[test]
    fn formats_grep_tool_call_with_scope() {
        let args = r#"{"pattern":"ToolCall","path":"src/pager"}"#;
        assert_eq!(
            tool_call_preview("Grep", args),
            "Grep: ToolCall in src/pager"
        );
    }

    #[test]
    fn row_widths_expand_title_and_prompt_to_fill_row() {
        let widths = RowWidths {
            state: 1,
            index: 2,
            harness: 6,
            workspace: 1,
            cwd: 12,
            time: 2,
            turns: 3,
            title: 10,
            prompt: 10,
        };
        let fitted = widths.fit(widths.total_width() + 20);
        assert_eq!(fitted.cwd, 12);
        assert_eq!(fitted.title, 20);
        assert_eq!(fitted.prompt, 20);
        assert_eq!(fitted.total_width(), widths.total_width() + 20);
    }

    #[test]
    fn row_widths_shrink_flexible_text_cells_to_viewport() {
        let widths = RowWidths {
            state: 1,
            index: 2,
            harness: 6,
            workspace: 1,
            cwd: 40,
            time: 2,
            turns: 4,
            title: 80,
            prompt: 80,
        };
        let fitted = widths.fit(100);
        assert!(fitted.total_width() <= 100);
        assert!(fitted.prompt <= widths.prompt);
        assert!(fitted.title <= widths.title);
    }

    #[test]
    fn cwd_label_can_show_absolute_path() {
        let path = Path::new("/home/nuck/holoq/repo-os/babel");
        assert_eq!(
            cwd_label(path, CwdDisplayMode::Absolute),
            "cwd:/home/nuck/holoq/repo-os/babel"
        );
    }

    #[test]
    fn cwd_label_can_show_project_name() {
        let path = Path::new("/home/nuck/holoq/repo-os/babel");
        assert_eq!(cwd_label(path, CwdDisplayMode::Project), "cwd:babel");
    }
}
