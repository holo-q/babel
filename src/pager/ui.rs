//! UI rendering for the resume pager
//!
//! Resume pager layout:
//! - Session list with running indicators
//! - Optional transcript preview with message rendering

use std::borrow::Cow;
use std::path::Path;

use ratatui::prelude::*;
use ratatui::widgets::{Block, Borders, Clear, List, ListItem, Paragraph};
use scrollparse::{Message, MessageKind};
use serde_json::Value;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::agent_kind::AgentKind;
use crate::session_row::{self, AgeTone, SessionRow, StateKind, TurnTone};

use super::app::{PaneFocus, ResumeApp, SearchTarget, TouchedProjectsState};
use super::one_line;
use super::project_metrics::{workgroup_style_for_path, ProjectTouchMetric, WorkgroupStyle};
use super::session_list::{CwdDisplayMode, EnrichedSession};
use super::transcript::{
    distill_prompt_thoughtstream, transcript_message_is_visible, transcript_message_row_count,
    TranscriptBodyMode, TranscriptRoleFilter,
};

const SELECTION_BG: Color = Color::Rgb(36, 54, 72);
const TOUCHED_PROJECTS_CWD_MAX_WIDTH: usize = 60;
const TOKEN_BRAILLE_COLUMN_WIDTH: usize = 3;
const BRAILLE_DOTS: [u8; 8] = [0x01, 0x02, 0x04, 0x40, 0x08, 0x10, 0x20, 0x80];

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
        // Keep a real gutter between the elastic session columns and the
        // transcript border. The list renderer also clamps each row to its own
        // budget, but the empty cell makes the split visually legible.
        let chunks = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([
                Constraint::Percentage(62),
                Constraint::Length(1),
                Constraint::Percentage(38),
            ])
            .split(main_area);

        draw_session_list(frame, app, chunks[0]);
        draw_transcript(frame, app, chunks[2]);
    } else {
        draw_session_list(frame, app, main_area);
    }
    draw_status_bar(frame, app, status_area);
}

/// Draw the session list panel
fn draw_session_list(frame: &mut Frame, app: &mut ResumeApp, area: Rect) {
    let title_style = if app.focus == PaneFocus::Sessions {
        Style::default()
            .fg(Color::Cyan)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(Color::DarkGray)
    };
    let mut title_spans = vec![Span::styled("Sessions [", title_style)];
    title_spans.extend(session_filter_label_spans(app, title_style));
    title_spans.push(Span::styled("]", title_style));
    if matches!(app.active_search, Some(SearchTarget::Sessions)) {
        title_spans.push(Span::styled(
            format!(" /{}", app.search_buffer),
            title_style,
        ));
    } else if !app.sessions.filter_query.is_empty() {
        title_spans.push(Span::styled(
            format!(" /{}", app.sessions.filter_query),
            title_style,
        ));
    }
    let header = Rect { height: 1, ..area };
    frame.render_widget(
        Paragraph::new(session_header_line(
            title_spans,
            app.status_message.as_str(),
            header.width as usize,
        )),
        header,
    );
    let body = Rect {
        y: area.y.saturating_add(1),
        height: area.height.saturating_sub(1),
        ..area
    };
    if body.height == 0 {
        return;
    }
    let column_header = Rect { height: 1, ..body };
    let inner = Rect {
        y: body.y.saturating_add(1),
        height: body.height.saturating_sub(1),
        ..body
    };

    let list_height = inner.height as usize;
    let cursor = app.sessions.cursor;
    let now = unix_now();
    let visible_len = app.sessions.visible_count();

    let scroll_offset =
        scroll_offset_for_cursor(cursor, app.sessions.scroll_offset, list_height, visible_len);
    app.sessions.scroll_offset = scroll_offset;

    let visible_indices = app.sessions.visible_indices().to_vec();
    let max_visible_turn_count = visible_indices
        .iter()
        .map(|idx| app.sessions.sessions[*idx].turn_count)
        .max()
        .unwrap_or(0);
    let viewport_indices: Vec<usize> = visible_indices
        .iter()
        .skip(scroll_offset)
        .take(list_height.max(1))
        .copied()
        .collect();
    let viewport_rows: Vec<RenderedSessionRow> = viewport_indices
        .iter()
        .map(|idx| {
            let session = &app.sessions.sessions[*idx];
            let mut row = session.row(now);
            let cwd = cwd_cell_for_session(session, &app.sessions.cwd_display_mode, app);
            row.cwd = cwd.plain_text();
            RenderedSessionRow {
                idx: *idx,
                row,
                cwd,
            }
        })
        .collect();
    let measured_widths = RowWidths::measure(
        app.sessions.cwd_display_mode,
        app.braille_tokens,
        viewport_rows
            .iter()
            .map(|rendered| (rendered.idx, &rendered.row, rendered.cwd.measure_width())),
    );
    let widths = measured_widths.fit(inner.width as usize);
    frame.render_widget(
        Paragraph::new(render_session_header(
            &widths,
            app.sessions.cwd_display_mode,
            app.braille_tokens,
            inner.width as usize,
        )),
        column_header,
    );

    let items: Vec<ListItem> = viewport_rows
        .iter()
        .enumerate()
        .map(|(idx, rendered)| {
            let is_selected = idx + scroll_offset == app.sessions.cursor;
            render_session_item(
                rendered,
                &widths,
                inner.width as usize,
                is_selected,
                app.snip_columns,
                app.braille_tokens,
                max_visible_turn_count,
            )
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
fn render_session_header(
    widths: &RowWidths,
    cwd_display_mode: CwdDisplayMode,
    braille_tokens: bool,
    row_width: usize,
) -> Line<'static> {
    let header_style = Style::default()
        .fg(Color::DarkGray)
        .add_modifier(Modifier::BOLD);
    let pad_style = Style::default();

    let mut line_width = 0;
    let mut spans = Vec::new();
    push_span(&mut spans, &mut line_width, " ", pad_style);
    push_span(&mut spans, &mut line_width, "s", header_style);
    push_span(&mut spans, &mut line_width, " ", pad_style);
    push_right_cell(
        &mut spans,
        &mut line_width,
        "harness",
        widths.harness,
        header_style,
        pad_style,
    );
    push_span(&mut spans, &mut line_width, "  ", pad_style);
    push_left_cell(
        &mut spans,
        &mut line_width,
        "ws",
        widths.workspace,
        header_style,
        pad_style,
    );
    push_span(&mut spans, &mut line_width, "  ", pad_style);
    push_right_cell(
        &mut spans,
        &mut line_width,
        cwd_display_mode.column_label(),
        widths.cwd,
        header_style,
        pad_style,
    );
    push_span(&mut spans, &mut line_width, "  ", pad_style);
    push_span(&mut spans, &mut line_width, "f", header_style);
    push_span(&mut spans, &mut line_width, " ", pad_style);
    push_left_cell(
        &mut spans,
        &mut line_width,
        "ct",
        widths.created_time,
        header_style,
        pad_style,
    );
    push_span(&mut spans, &mut line_width, " ", pad_style);
    push_left_cell(
        &mut spans,
        &mut line_width,
        "mt",
        widths.modified_time,
        header_style,
        pad_style,
    );
    push_span(&mut spans, &mut line_width, "  ", pad_style);
    push_left_cell(
        &mut spans,
        &mut line_width,
        token_column_label(braille_tokens),
        widths.turns,
        header_style,
        pad_style,
    );
    push_span(&mut spans, &mut line_width, "  ", pad_style);
    push_left_cell(
        &mut spans,
        &mut line_width,
        "#",
        widths.index,
        header_style,
        pad_style,
    );
    push_span(&mut spans, &mut line_width, "  ", pad_style);
    push_right_cell(
        &mut spans,
        &mut line_width,
        "thread",
        widths.title,
        header_style,
        pad_style,
    );
    push_span(&mut spans, &mut line_width, "  ", pad_style);
    push_right_cell(
        &mut spans,
        &mut line_width,
        "prompt",
        widths.prompt,
        header_style,
        pad_style,
    );
    finish_row_line(spans, line_width, row_width, pad_style)
}

fn render_session_item(
    rendered: &RenderedSessionRow,
    widths: &RowWidths,
    row_width: usize,
    is_selected: bool,
    snip_columns: bool,
    braille_tokens: bool,
    max_turn_count: u32,
) -> ListItem<'static> {
    ListItem::new(render_session_line(
        rendered,
        widths,
        row_width,
        is_selected,
        snip_columns,
        braille_tokens,
        max_turn_count,
    ))
}

fn render_session_line(
    rendered: &RenderedSessionRow,
    widths: &RowWidths,
    row_width: usize,
    is_selected: bool,
    snip_columns: bool,
    braille_tokens: bool,
    max_turn_count: u32,
) -> Line<'static> {
    let idx = rendered.idx;
    let row = &rendered.row;
    let accent = row.ansi256;
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
    let created_time_style = selected_style(
        row_style(age_style(row.created_time_tone, row.bright), running),
        selected_bg,
    );
    let modified_time_style = selected_style(
        row_style(age_style(row.modified_time_tone, row.bright), running),
        selected_bg,
    );
    let turns_style = selected_style(
        row_style(turn_style(row.turn_tone, row.bright), running),
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
    push_cwd_cell(
        &mut spans,
        &mut line_width,
        &rendered.cwd,
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
        &row.created_time,
        widths.created_time,
        created_time_style,
        pad_style,
    );
    push_span(&mut spans, &mut line_width, " ", gap);
    push_left_cell(
        &mut spans,
        &mut line_width,
        &row.modified_time,
        widths.modified_time,
        modified_time_style,
        pad_style,
    );
    push_span(&mut spans, &mut line_width, "  ", gap);
    push_left_cell(
        &mut spans,
        &mut line_width,
        &token_cell_text(row.turn_count, max_turn_count, widths.turns, braille_tokens),
        widths.turns,
        turns_style,
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
    push_right_cell_mode(
        &mut spans,
        &mut line_width,
        &row.title,
        widths.title,
        title_style,
        pad_style,
        snip_columns,
    );
    push_span(&mut spans, &mut line_width, "  ", gap);
    push_right_cell_mode(
        &mut spans,
        &mut line_width,
        &row.last_prompt,
        widths.prompt,
        text_style,
        pad_style,
        snip_columns,
    );
    finish_row_line(spans, line_width, row_width, gap)
}

/// Draw the transcript preview panel
fn draw_transcript(frame: &mut Frame, app: &mut ResumeApp, area: Rect) {
    let title = transcript_title(app);

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

    // Render one physical row per turn by default. `s` toggles expansion for
    // user/assistant message bodies only; tool calls and tool outputs remain
    // clamped to one row so transcript navigation never degenerates into
    // scrolling through command output or large JSON argument blocks.
    let row_width = inner.width as usize;
    let palette = transcript_palette(
        app.sessions
            .selected()
            .map(|session| session.agent_kind)
            .unwrap_or_default(),
    );
    let row_count = cached_transcript_row_count(&mut app.transcript);
    let max_offset = row_count.saturating_sub(inner.height as usize);
    app.transcript.scroll_offset = app.transcript.scroll_offset.min(max_offset);
    if row_count == 0 && !app.transcript.search_query.trim().is_empty() {
        let para = Paragraph::new(format!(
            "No transcript matches /{}",
            app.transcript.search_query
        ))
        .style(Style::default().fg(Color::DarkGray));
        frame.render_widget(para, inner);
        return;
    }

    let lines = transcript_visible_lines(
        &app.transcript.messages,
        app.transcript.body_mode,
        app.transcript.scroll_offset,
        inner.height as usize,
        row_width,
        palette,
        app.transcript.role_filter,
        &app.transcript.search_query,
    );

    let para = Paragraph::new(lines);
    frame.render_widget(para, inner);
}

/// Draw the status bar at the bottom
fn draw_status_bar(frame: &mut Frame, app: &mut ResumeApp, area: Rect) {
    let session_count = app.sessions.visible_count();
    let total = app.sessions.sessions.len();
    let width = area.width as usize;
    if width == 0 {
        return;
    }

    let mut left_spans = Vec::new();
    let mut left_width = 0;
    let base_style = Style::default().fg(Color::Gray);
    push_span(
        &mut left_spans,
        &mut left_width,
        format!(" {session_count}/{total} sessions"),
        base_style,
    );

    if !app.is_searching() {
        push_status_button(
            &mut left_spans,
            &mut left_width,
            "Tab",
            if app.sessions.show_all { "all" } else { "cwd" },
            app.sessions.show_all,
        );
        push_status_button(
            &mut left_spans,
            &mut left_width,
            "c",
            app.sessions.cwd_display_mode.column_label(),
            app.sessions.cwd_display_mode != CwdDisplayMode::Relative,
        );
        push_status_button(
            &mut left_spans,
            &mut left_width,
            "o",
            "focus",
            app.sessions.project_filter.is_some(),
        );
        push_status_button(
            &mut left_spans,
            &mut left_width,
            "b",
            if app.braille_tokens { "braille" } else { "raw" },
            app.braille_tokens,
        );
        push_status_button(
            &mut left_spans,
            &mut left_width,
            "h/A-h",
            app.sessions.hidden_display_mode.label(),
            app.sessions.hidden_display_mode != Default::default(),
        );
        push_status_button(
            &mut left_spans,
            &mut left_width,
            "t",
            "transcript",
            app.show_transcript,
        );
        push_status_button(
            &mut left_spans,
            &mut left_width,
            "s/S",
            if app.focus == PaneFocus::Transcript {
                app.transcript.body_mode.label()
            } else if app.snip_columns {
                "cols-snip"
            } else {
                "cols-clip"
            },
            app.focus == PaneFocus::Transcript || app.snip_columns,
        );
        push_status_button(
            &mut left_spans,
            &mut left_width,
            "u/U",
            app.transcript.role_filter.label(),
            app.transcript.role_filter != TranscriptRoleFilter::All,
        );
    } else if let Some(target) = app.active_search {
        push_span(&mut left_spans, &mut left_width, "  /", base_style);
        push_span(
            &mut left_spans,
            &mut left_width,
            format!("{} ", target.label()),
            Style::default().fg(Color::DarkGray),
        );
        push_span(
            &mut left_spans,
            &mut left_width,
            app.search_buffer.clone(),
            Style::default().fg(Color::White),
        );
    }

    let keybinds = if app.is_searching() {
        "Enter confirm  Esc cancel  C-w/C-BS word"
    } else {
        "H hide  y yank  r refresh  c/C cwd  h/A-h hidden  s/S snip  u/U roles  o focus  b tokens  / search  Enter launch  q quit"
    };
    let right = format!("{} ", keybinds);
    let right_width = display_width(&right);

    let (left_spans, left_width, right, padding) = if left_width + right_width <= width {
        (
            left_spans,
            left_width,
            right,
            width.saturating_sub(left_width + right_width),
        )
    } else if right_width < width {
        let left_max = width.saturating_sub(right_width + 1);
        (
            clamp_spans_to_width(left_spans, left_max),
            left_max,
            right,
            1,
        )
    } else {
        (Vec::new(), 0, truncate_str(&right, width), 0)
    };

    let status_style = Style::default();

    let mut spans = left_spans;
    let rendered_left_width = left_width.min(width);
    let max_padding = width.saturating_sub(rendered_left_width + display_width(&right));
    spans.push(Span::styled(
        " ".repeat(padding.min(max_padding)),
        status_style,
    ));
    spans.push(Span::styled(right, Style::default().fg(Color::DarkGray)));

    let para = Paragraph::new(Line::from(clamp_spans_to_width(spans, width))).style(status_style);
    frame.render_widget(para, area);
}

fn session_header_line(
    left_spans: Vec<Span<'static>>,
    status_message: &str,
    width: usize,
) -> Line<'static> {
    if width == 0 || status_message.is_empty() {
        return Line::from(clamp_spans_to_width(left_spans, width));
    }

    const LEFT_FLOOR: usize = 10;
    let status_style = Style::default()
        .fg(Color::DarkGray)
        .add_modifier(Modifier::DIM);
    let status_budget = width.saturating_sub(LEFT_FLOOR + 1);
    if status_budget == 0 {
        return Line::from(clamp_spans_to_width(left_spans, width));
    }

    let status = middle_truncate_str(status_message, status_budget);
    let status_width = display_width(&status);
    let left_budget = width.saturating_sub(status_width + 1);
    let mut spans = clamp_spans_to_width(left_spans, left_budget);
    let left_width = spans_display_width(&spans);
    let padding = width.saturating_sub(left_width + status_width);

    spans.push(Span::raw(" ".repeat(padding)));
    let mut line_width = left_width + padding;
    push_text_with_dim_cut_marker(&mut spans, &mut line_width, status, status_style);
    Line::from(clamp_spans_to_width(spans, width))
}

fn push_status_button(
    spans: &mut Vec<Span<'static>>,
    line_width: &mut usize,
    key: &'static str,
    label: &'static str,
    active: bool,
) {
    let bg = if active {
        Color::Rgb(54, 72, 72)
    } else {
        Color::Rgb(28, 32, 34)
    };
    let fg = if active {
        Color::White
    } else {
        Color::DarkGray
    };
    let key_style = Style::default()
        .fg(fg)
        .bg(bg)
        .add_modifier(Modifier::BOLD | Modifier::UNDERLINED);
    let label_style = Style::default().fg(fg).bg(bg);

    push_span(spans, line_width, "  ", Style::default());
    push_span(spans, line_width, " ", label_style);
    push_span(spans, line_width, key.to_string(), key_style);
    push_span(spans, line_width, " ", label_style);
    push_span(spans, line_width, label.to_string(), label_style);
    push_span(spans, line_width, " ", label_style);
}

// === Helper functions ===

fn transcript_title(app: &ResumeApp) -> String {
    let filter = app.transcript.role_filter.label();
    let search = if matches!(app.active_search, Some(SearchTarget::Transcript)) {
        (!app.search_buffer.is_empty()).then(|| format!(" /{}", app.search_buffer))
    } else {
        (!app.transcript.search_query.is_empty())
            .then(|| format!(" /{}", app.transcript.search_query))
    }
    .unwrap_or_default();
    let cwd = app
        .transcript
        .session_id
        .as_deref()
        .and_then(|id| {
            app.sessions
                .sessions
                .iter()
                .find(|session| session.native_id == id)
        })
        .and_then(|session| session.project_path.as_ref())
        .map(|path| format!(" cwd:{}", path.display()))
        .unwrap_or_default();

    match &app.transcript.session_id {
        Some(id) => format!(
            "Transcript [{}] [{filter}]{search}{cwd}",
            &id[..8.min(id.len())]
        ),
        None => format!("Transcript [{filter}]{search}{cwd}"),
    }
}

fn session_filter_label_spans(app: &mut ResumeApp, title_style: Style) -> Vec<Span<'static>> {
    let base = if app.sessions.show_all || app.sessions.project_filter.is_some() {
        "all".to_string()
    } else {
        app.sessions
            .current_cwd
            .as_deref()
            .map(cwd_scope_label)
            .unwrap_or_else(|| "cwd:?".to_string())
    };
    let label = if let Some(suffix) = app.sessions.hidden_display_mode.suffix() {
        format!("{base}{suffix}")
    } else {
        base
    };
    let mut spans = vec![Span::styled(label, title_style)];
    if let Some(filter) = app.sessions.project_filter.as_ref() {
        spans.push(Span::styled(" ", title_style));
        spans.push(Span::styled(
            format!("o:{}", filter.label),
            title_style
                .fg(Color::LightCyan)
                .add_modifier(Modifier::BOLD),
        ));
    }
    spans
}

fn frequency_rgb(count: u32, max_count: u32, normal: (u8, u8, u8)) -> Color {
    let pct = if max_count == 0 {
        0.0
    } else {
        count as f32 / max_count as f32
    }
    .clamp(0.0, 1.0);
    let floor = dimmed_frequency_floor(normal);
    let mix = |low: u8, high: u8| (low as f32 + (high as f32 - low as f32) * pct).round() as u8;
    Color::Rgb(
        mix(floor.0, normal.0),
        mix(floor.1, normal.1),
        mix(floor.2, normal.2),
    )
}

fn dimmed_frequency_floor(normal: (u8, u8, u8)) -> (u8, u8, u8) {
    // Frequency is visual weight, not identity. Keep the workgroup hue even for
    // low-touch projects by dimming toward a terminal-bg-ish floor instead of
    // washing every color through the same neutral gray.
    const DARK_BG: (f32, f32, f32) = (28.0, 32.0, 34.0);
    const HUE_KEEP: f32 = 0.42;
    (
        (DARK_BG.0 + (normal.0 as f32 - DARK_BG.0) * HUE_KEEP).round() as u8,
        (DARK_BG.1 + (normal.1 as f32 - DARK_BG.1) * HUE_KEEP).round() as u8,
        (DARK_BG.2 + (normal.2 as f32 - DARK_BG.2) * HUE_KEEP).round() as u8,
    )
}

fn style_rgb(style: Style) -> Option<(u8, u8, u8)> {
    match style.fg {
        Some(Color::Rgb(r, g, b)) => Some((r, g, b)),
        Some(Color::Indexed(ansi)) => ansi256_to_rgb(ansi),
        Some(Color::Cyan) => Some((0, 255, 255)),
        Some(Color::White) => Some((255, 255, 255)),
        Some(Color::Gray) => Some((160, 160, 160)),
        Some(Color::DarkGray) => Some((80, 80, 80)),
        _ => None,
    }
}

fn ansi256_to_rgb(ansi: u8) -> Option<(u8, u8, u8)> {
    Some(session_row::ansi256_rgb(ansi))
}

fn hex_rgb(hex: &str) -> Option<(u8, u8, u8)> {
    let hex = hex.strip_prefix('#')?;
    if hex.len() != 6 {
        return None;
    }
    let r = u8::from_str_radix(&hex[0..2], 16).ok()?;
    let g = u8::from_str_radix(&hex[2..4], 16).ok()?;
    let b = u8::from_str_radix(&hex[4..6], 16).ok()?;
    Some((r, g, b))
}

fn cwd_scope_label(cwd: &Path) -> String {
    format!("cwd:{}", relative_cwd_label(cwd))
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

struct RenderedSessionRow {
    idx: usize,
    row: SessionRow,
    cwd: CwdCell,
}

#[derive(Clone)]
enum CwdCell {
    Text(String),
    WorkgroupText(StyledCwdLabel),
    TouchlessText(String),
    Projects(Vec<ProjectTouchMetric>),
    Loading,
    Empty,
    Error,
}

#[derive(Clone)]
struct StyledCwdLabel {
    workgroup_prefix: String,
    suffix: String,
    ansi256: u8,
}

impl StyledCwdLabel {
    fn plain_text(&self) -> String {
        format!("{}{}", self.workgroup_prefix, self.suffix)
    }
}

impl CwdCell {
    fn plain_text(&self) -> String {
        match self {
            Self::Text(text) => text.clone(),
            Self::WorkgroupText(label) => label.plain_text(),
            Self::TouchlessText(text) => text.clone(),
            Self::Projects(projects) if projects.is_empty() => String::new(),
            Self::Projects(projects) => projects
                .iter()
                .take(4)
                .map(|project| project_leaf_label(&project.path))
                .collect::<Vec<_>>()
                .join(", "),
            Self::Loading => "projects:loading".to_string(),
            Self::Empty => String::new(),
            Self::Error => "projects:error".to_string(),
        }
    }

    fn measure_width(&self) -> usize {
        let width = display_width(&self.plain_text());
        if matches!(self, Self::Projects(_)) {
            width.min(TOUCHED_PROJECTS_CWD_MAX_WIDTH)
        } else {
            width
        }
    }
}

fn cwd_cell_for_session(
    session: &EnrichedSession,
    mode: &CwdDisplayMode,
    app: &ResumeApp,
) -> CwdCell {
    match mode {
        CwdDisplayMode::Relative => session
            .project_path
            .as_deref()
            .map(|path| styled_cwd_cell(path, *mode))
            .unwrap_or(CwdCell::Empty),
        CwdDisplayMode::Absolute => session
            .project_path
            .as_ref()
            .map(|path| styled_cwd_cell(path, *mode))
            .unwrap_or(CwdCell::Empty),
        CwdDisplayMode::Project => session
            .project_path
            .as_ref()
            .map(|path| styled_cwd_cell(path, *mode))
            .unwrap_or_else(|| {
                session
                    .project_path
                    .as_ref()
                    .map(|path| CwdCell::Text(path.display().to_string()))
                    .unwrap_or(CwdCell::Empty)
            }),
        CwdDisplayMode::TouchedProjects => match app.touched_projects.get(&session.session_key) {
            Some(TouchedProjectsState::Loaded(projects)) if projects.is_empty() => session
                .project_path
                .as_deref()
                .map(relative_cwd_label)
                .map(CwdCell::TouchlessText)
                .unwrap_or(CwdCell::Empty),
            Some(TouchedProjectsState::Loaded(projects)) => CwdCell::Projects(projects.clone()),
            Some(TouchedProjectsState::Notice(_)) => CwdCell::Error,
            Some(TouchedProjectsState::Empty | TouchedProjectsState::Loading) | None => {
                CwdCell::Loading
            }
        },
    }
}

fn styled_cwd_cell(path: &Path, mode: CwdDisplayMode) -> CwdCell {
    let label = match mode {
        CwdDisplayMode::Relative | CwdDisplayMode::TouchedProjects => relative_cwd_label(path),
        CwdDisplayMode::Absolute => path.display().to_string(),
        CwdDisplayMode::Project => project_leaf_label(path),
    };

    let Some(style) = workgroup_style_for_path(path) else {
        return CwdCell::Text(label);
    };
    styled_cwd_label(path, mode, label.clone(), &style)
        .map(CwdCell::WorkgroupText)
        .unwrap_or(CwdCell::Text(label))
}

fn styled_cwd_label(
    _path: &Path,
    mode: CwdDisplayMode,
    label: String,
    style: &WorkgroupStyle,
) -> Option<StyledCwdLabel> {
    if label.is_empty() {
        return None;
    }

    if mode == CwdDisplayMode::Project {
        return Some(StyledCwdLabel {
            workgroup_prefix: label,
            suffix: String::new(),
            ansi256: style.ansi256,
        });
    }

    let workgroup_label = match mode {
        CwdDisplayMode::Absolute => style.root.display().to_string(),
        CwdDisplayMode::Relative | CwdDisplayMode::TouchedProjects => {
            relative_cwd_label(&style.root)
        }
        CwdDisplayMode::Project => unreachable!("project mode returned above"),
    };

    if workgroup_label.is_empty() {
        return None;
    }

    if let Some(suffix) = label.strip_prefix(&workgroup_label) {
        if !(suffix.is_empty() || suffix.starts_with(std::path::MAIN_SEPARATOR)) {
            return Some(StyledCwdLabel {
                workgroup_prefix: label,
                suffix: String::new(),
                ansi256: style.ansi256,
            });
        }
        return Some(StyledCwdLabel {
            workgroup_prefix: workgroup_label,
            suffix: suffix.to_string(),
            ansi256: style.ansi256,
        });
    }

    Some(StyledCwdLabel {
        workgroup_prefix: label,
        suffix: String::new(),
        ansi256: style.ansi256,
    })
}

fn project_leaf_label(path: &Path) -> String {
    path.file_name()
        .and_then(|name| name.to_str())
        .map(str::to_string)
        .unwrap_or_else(|| path.display().to_string())
}

#[derive(Clone, Copy, Default)]
struct RowWidths {
    state: usize,
    index: usize,
    harness: usize,
    workspace: usize,
    cwd: usize,
    created_time: usize,
    modified_time: usize,
    turns: usize,
    title: usize,
    prompt: usize,
}

impl RowWidths {
    const TITLE_BUDGET_PERCENT: usize = 42;
    const PROMPT_FLOOR_PERCENT: usize = 35;

    fn measure<'a>(
        cwd_display_mode: CwdDisplayMode,
        braille_tokens: bool,
        rows: impl Iterator<Item = (usize, &'a SessionRow, usize)>,
    ) -> Self {
        let mut widths = Self::header_minimums(cwd_display_mode, braille_tokens);
        let rows: Vec<_> = rows.collect();
        let max_turn_count = rows
            .iter()
            .map(|(_, row, _)| row.turn_count)
            .max()
            .unwrap_or(0);

        for (idx, row, cwd_width) in rows {
            widths.state = widths.state.max(display_width(row.state_icon));
            widths.index = widths.index.max(format!("{}", idx + 1).len());
            widths.harness = widths.harness.max(row.harness.len());
            widths.workspace = widths.workspace.max(display_width(&row.workspace));
            widths.cwd = widths.cwd.max(cwd_width);
            widths.created_time = widths.created_time.max(display_width(&row.created_time));
            widths.modified_time = widths.modified_time.max(display_width(&row.modified_time));
            widths.turns = widths.turns.max(display_width(&token_cell_text(
                row.turn_count,
                max_turn_count,
                TOKEN_BRAILLE_COLUMN_WIDTH,
                braille_tokens,
            )));
            widths.title = widths.title.max(display_width(&row.title));
            widths.prompt = widths.prompt.max(display_width(&row.last_prompt));
        }

        widths.state = widths.state.max(1);
        widths.index = widths.index.max(1);
        widths
    }

    fn header_minimums(cwd_display_mode: CwdDisplayMode, braille_tokens: bool) -> Self {
        Self {
            state: 1,
            index: display_width("#"),
            harness: display_width("harness"),
            workspace: display_width("ws"),
            cwd: display_width(cwd_display_mode.column_label()),
            created_time: display_width("ct"),
            modified_time: display_width("mt"),
            turns: display_width(token_column_label(braille_tokens)),
            title: display_width("thread"),
            prompt: display_width("prompt"),
        }
    }

    fn fit(mut self, row_width: usize) -> Self {
        if row_width == 0 {
            return self;
        }

        self.shrink_fixed_columns_to(row_width);
        self.fit_text_columns(row_width);
        self.shrink_to(row_width);
        self
    }

    fn total_width(&self) -> usize {
        // Leading state cluster:
        // " " + state icon + " " + harness + "  " + workspace + "  " + cwd
        // + "  " + filter + " " + ct + " " + mt + "  " + turns + "  " + index + "  "
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
            + self.created_time
            + 1
            + self.modified_time
            + 2
            + self.turns
            + 2
            + self.index
            + 2
            + self.title
            + 2
            + self.prompt
    }

    fn fixed_width(&self) -> usize {
        self.total_width()
            .saturating_sub(self.title)
            .saturating_sub(self.prompt)
    }

    fn fit_text_columns(&mut self, row_width: usize) {
        let fixed_width = self.fixed_width();
        if fixed_width >= row_width {
            self.title = 0;
            self.prompt = 0;
            return;
        }

        let budget = row_width - fixed_width;
        let title_floor = 1.min(budget);
        let prompt_floor = if budget > title_floor { 1 } else { 0 };
        let mut title_cap = ((budget * Self::TITLE_BUDGET_PERCENT) / 100).max(title_floor);
        title_cap = title_cap.min(budget.saturating_sub(prompt_floor));
        let prompt_floor_target = ((budget * Self::PROMPT_FLOOR_PERCENT) / 100)
            .max(prompt_floor)
            .min(budget.saturating_sub(title_floor));

        self.title = self.title.min(title_cap);
        self.prompt = self.prompt.min(budget.saturating_sub(self.title));

        if self.prompt < prompt_floor_target {
            let needed = prompt_floor_target - self.prompt;
            let donated = needed.min(self.title.saturating_sub(title_floor));
            self.title = self.title.saturating_sub(donated);
            self.prompt += donated;
        }

        let used = self.title + self.prompt;
        if used < budget {
            let extra = budget - used;
            let title_room = title_cap.saturating_sub(self.title);
            let title_extra = (extra / 3).min(title_room);
            self.title += title_extra;
            self.prompt += extra - title_extra;
        }
    }

    fn shrink_fixed_columns_to(&mut self, row_width: usize) {
        let min_text_width = 2.min(row_width);
        self.shrink_fixed_column(row_width, min_text_width, |widths| &mut widths.cwd, 1);
        self.shrink_fixed_column(row_width, min_text_width, |widths| &mut widths.workspace, 1);
        self.shrink_fixed_column(row_width, min_text_width, |widths| &mut widths.harness, 1);
        self.shrink_fixed_column(row_width, min_text_width, |widths| &mut widths.turns, 1);
        self.shrink_fixed_column(
            row_width,
            min_text_width,
            |widths| &mut widths.created_time,
            1,
        );
        self.shrink_fixed_column(
            row_width,
            min_text_width,
            |widths| &mut widths.modified_time,
            1,
        );
        self.shrink_fixed_column(row_width, min_text_width, |widths| &mut widths.index, 1);
    }

    fn shrink_to(&mut self, row_width: usize) {
        self.shrink_column(row_width, |widths| &mut widths.title, 1);
        self.shrink_column(row_width, |widths| &mut widths.prompt, 1);
        self.shrink_column(row_width, |widths| &mut widths.cwd, 1);
        self.shrink_column(row_width, |widths| &mut widths.workspace, 1);
        self.shrink_column(row_width, |widths| &mut widths.harness, 1);
        self.shrink_column(row_width, |widths| &mut widths.turns, 1);
        self.shrink_column(row_width, |widths| &mut widths.created_time, 1);
        self.shrink_column(row_width, |widths| &mut widths.modified_time, 1);
        self.shrink_column(row_width, |widths| &mut widths.index, 1);
    }

    fn shrink_column(
        &mut self,
        row_width: usize,
        column: impl Fn(&mut Self) -> &mut usize,
        floor: usize,
    ) {
        while self.total_width() > row_width {
            let width = column(self);
            if *width <= floor {
                break;
            }
            *width -= 1;
        }
    }

    fn shrink_fixed_column(
        &mut self,
        row_width: usize,
        min_text_width: usize,
        column: impl Fn(&mut Self) -> &mut usize,
        floor: usize,
    ) {
        while self.fixed_width() + min_text_width > row_width {
            let width = column(self);
            if *width <= floor {
                break;
            }
            *width -= 1;
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

fn push_text_with_dim_cut_marker(
    spans: &mut Vec<Span<'static>>,
    line_width: &mut usize,
    text: impl Into<String>,
    style: Style,
) {
    let text = text.into();
    if !text.contains(CUT_MARKER_GLYPH) {
        push_span(spans, line_width, text, style);
        return;
    }

    let mut rest = text.as_str();
    while let Some(idx) = rest.find(CUT_MARKER_GLYPH) {
        let (before, after_before) = rest.split_at(idx);
        if !before.is_empty() {
            push_span(spans, line_width, before.to_string(), style);
        }
        push_span(
            spans,
            line_width,
            CUT_MARKER_GLYPH,
            style.add_modifier(Modifier::DIM),
        );
        rest = &after_before[CUT_MARKER_GLYPH.len()..];
    }

    if !rest.is_empty() {
        push_span(spans, line_width, rest.to_string(), style);
    }
}

fn finish_row_line(
    mut spans: Vec<Span<'static>>,
    mut line_width: usize,
    row_width: usize,
    fill_style: Style,
) -> Line<'static> {
    if row_width == 0 {
        return Line::from("");
    }

    if line_width < row_width {
        let pad = " ".repeat(row_width - line_width);
        push_span(&mut spans, &mut line_width, pad, fill_style);
    }

    Line::from(clamp_spans_to_width(spans, row_width))
}

fn clamp_spans_to_width(spans: Vec<Span<'static>>, max_width: usize) -> Vec<Span<'static>> {
    let mut out = Vec::new();
    let mut used = 0;

    for span in spans {
        if used >= max_width {
            break;
        }

        let span_width = display_width(span.content.as_ref());
        if used + span_width <= max_width {
            used += span_width;
            out.push(span);
            continue;
        }

        let remaining = max_width - used;
        let clipped = take_width_from_start(span.content.as_ref(), remaining);
        if !clipped.is_empty() {
            out.push(Span::styled(clipped, span.style));
        }
        break;
    }

    out
}

fn spans_display_width(spans: &[Span<'_>]) -> usize {
    spans
        .iter()
        .map(|span| display_width(span.content.as_ref()))
        .sum()
}

fn push_left_cell(
    spans: &mut Vec<Span<'static>>,
    line_width: &mut usize,
    text: &str,
    width: usize,
    text_style: Style,
    pad_style: Style,
) {
    push_left_cell_mode(spans, line_width, text, width, text_style, pad_style, true);
}

fn push_left_cell_mode(
    spans: &mut Vec<Span<'static>>,
    line_width: &mut usize,
    text: &str,
    width: usize,
    text_style: Style,
    pad_style: Style,
    snip: bool,
) {
    let fitted = fit_cell_text(text, width, snip);
    let text_width = display_width(&fitted);
    if text_width < width {
        push_span(spans, line_width, " ".repeat(width - text_width), pad_style);
    }
    push_text_with_dim_cut_marker(spans, line_width, fitted, text_style);
}

fn push_right_cell(
    spans: &mut Vec<Span<'static>>,
    line_width: &mut usize,
    text: &str,
    width: usize,
    text_style: Style,
    pad_style: Style,
) {
    push_right_cell_mode(spans, line_width, text, width, text_style, pad_style, true);
}

fn push_right_cell_mode(
    spans: &mut Vec<Span<'static>>,
    line_width: &mut usize,
    text: &str,
    width: usize,
    text_style: Style,
    pad_style: Style,
    snip: bool,
) {
    let fitted = fit_cell_text(text, width, snip);
    push_text_with_dim_cut_marker(spans, line_width, fitted.clone(), text_style);
    let text_width = display_width(&fitted);
    if text_width < width {
        push_span(spans, line_width, " ".repeat(width - text_width), pad_style);
    }
}

fn push_cwd_cell(
    spans: &mut Vec<Span<'static>>,
    line_width: &mut usize,
    cell: &CwdCell,
    width: usize,
    text_style: Style,
    pad_style: Style,
) {
    match cell {
        CwdCell::Projects(projects) if !projects.is_empty() => {
            let start_width = *line_width;
            let max_count = projects
                .iter()
                .map(|project| project.touch_count)
                .max()
                .unwrap_or(1);
            let mut used = 0;
            for (idx, project) in projects.iter().take(4).enumerate() {
                let prefix = if idx == 0 { "" } else { ", " };
                let label = project_leaf_label(&project.path);
                let segment = format!("{prefix}{label}");
                let segment_width = display_width(&segment);
                if used + segment_width > width {
                    if used < width {
                        push_span(spans, line_width, "…", text_style);
                    }
                    break;
                }
                used += segment_width;
                let normal = project
                    .ansi256
                    .and_then(ansi256_to_rgb)
                    .or_else(|| style_rgb(text_style))
                    .unwrap_or((120, 120, 120));
                push_span(
                    spans,
                    line_width,
                    segment,
                    text_style.fg(frequency_rgb(project.touch_count, max_count, normal)),
                );
            }
            let rendered_width = line_width.saturating_sub(start_width);
            if rendered_width < width {
                push_span(
                    spans,
                    line_width,
                    " ".repeat(width.saturating_sub(rendered_width)),
                    pad_style,
                );
            }
        }
        CwdCell::WorkgroupText(label) => {
            push_styled_cwd_label(spans, line_width, label, width, text_style, pad_style);
        }
        CwdCell::TouchlessText(text) => push_right_cell(
            spans,
            line_width,
            text,
            width,
            text_style.fg(Color::DarkGray),
            pad_style,
        ),
        _ => push_right_cell(
            spans,
            line_width,
            &cell.plain_text(),
            width,
            text_style,
            pad_style,
        ),
    }
}

fn push_styled_cwd_label(
    spans: &mut Vec<Span<'static>>,
    line_width: &mut usize,
    label: &StyledCwdLabel,
    width: usize,
    text_style: Style,
    pad_style: Style,
) {
    let full = label.plain_text();
    let fitted = fit_cell_text(&full, width, true);
    let styled = text_style.fg(Color::Indexed(label.ansi256));
    let text_width = display_width(&fitted);

    if fitted == full {
        push_span(spans, line_width, label.workgroup_prefix.clone(), styled);
        push_span(spans, line_width, label.suffix.clone(), text_style);
    } else if let Some(suffix) = fitted.strip_prefix(&label.workgroup_prefix) {
        push_span(spans, line_width, label.workgroup_prefix.clone(), styled);
        push_text_with_dim_cut_marker(spans, line_width, suffix.to_string(), text_style);
    } else {
        // Middle truncation preserves the left edge, which is where the workgroup
        // root appears in relative/absolute labels. If the split point no longer
        // matches cleanly, keep the visible cwd token workgroup-colored instead of
        // losing scope color completely.
        push_text_with_dim_cut_marker(spans, line_width, fitted, styled);
    }

    if text_width < width {
        push_span(
            spans,
            line_width,
            " ".repeat(width.saturating_sub(text_width)),
            pad_style,
        );
    }
}

fn fit_cell_text(text: &str, width: usize, snip: bool) -> String {
    if width == 0 {
        String::new()
    } else if snip {
        middle_truncate_str(text, width)
    } else {
        take_width_from_start(text, width)
    }
}

fn token_column_label(braille_tokens: bool) -> &'static str {
    if braille_tokens {
        "tok"
    } else {
        "turns"
    }
}

fn token_cell_text(
    turn_count: u32,
    max_turn_count: u32,
    column_width: usize,
    braille_tokens: bool,
) -> String {
    if !braille_tokens {
        return if turn_count > 0 {
            turn_count.to_string()
        } else {
            String::new()
        };
    }
    braille_token_bar(turn_count, max_turn_count, column_width.max(1))
}

fn braille_token_bar(turn_count: u32, max_turn_count: u32, width: usize) -> String {
    if turn_count == 0 || max_turn_count == 0 || width == 0 {
        return String::new();
    }

    let max_dots = width.saturating_mul(8);
    let filled = (((turn_count as u64) * (max_dots as u64) + (max_turn_count as u64 - 1))
        / max_turn_count as u64)
        .clamp(1, max_dots as u64) as usize;
    let mut remaining = filled;
    let mut out = String::new();

    for _ in 0..width {
        let dots = remaining.min(8);
        remaining = remaining.saturating_sub(dots);
        if dots == 0 {
            out.push(' ');
            continue;
        }
        let mask = BRAILLE_DOTS
            .iter()
            .take(dots)
            .fold(0u8, |mask, dot| mask | dot);
        out.push(char::from_u32(0x2800 + mask as u32).unwrap_or(' '));
    }

    out
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

/// Pre-sanitize transcript messages at load time so per-frame rendering
/// skips ANSI stripping and tool-call preview formatting entirely.
pub fn prepare_transcript_messages(messages: &mut [Message]) {
    for msg in messages.iter_mut() {
        match &msg.kind {
            MessageKind::ToolCall { name, args } => {
                msg.content = sanitize_transcript_text(&tool_call_preview(name, args));
            }
            _ => {
                msg.content = sanitize_transcript_text(&msg.content);
            }
        }
    }
}

fn cached_transcript_row_count(view: &mut super::transcript::TranscriptView) -> usize {
    if let Some((body_mode, filter, search, count)) = &view.cached_row_count {
        if *body_mode == view.body_mode
            && *filter == view.role_filter
            && search == &view.search_query
        {
            return *count;
        }
    }
    let count = transcript_rendered_row_count(
        &view.messages,
        view.body_mode,
        view.role_filter,
        &view.search_query,
    );
    view.cached_row_count = Some((
        view.body_mode,
        view.role_filter,
        view.search_query.clone(),
        count,
    ));
    count
}

fn transcript_rendered_row_count(
    messages: &[Message],
    body_mode: TranscriptBodyMode,
    role_filter: TranscriptRoleFilter,
    search_query: &str,
) -> usize {
    transcript_render_items(messages, role_filter, search_query)
        .iter()
        .map(|item| transcript_render_item_row_count(item, body_mode, role_filter))
        .sum()
}

pub fn transcript_visible_lines(
    messages: &[Message],
    body_mode: TranscriptBodyMode,
    scroll_offset: usize,
    height: usize,
    row_width: usize,
    palette: TranscriptPalette,
    role_filter: TranscriptRoleFilter,
    search_query: &str,
) -> Vec<Line<'static>> {
    let mut remaining_skip = scroll_offset;
    let mut lines = Vec::new();
    for item in transcript_render_items(messages, role_filter, search_query) {
        let row_count = transcript_render_item_row_count(&item, body_mode, role_filter);
        if remaining_skip >= row_count {
            remaining_skip -= row_count;
            continue;
        }

        match item {
            TranscriptRenderItem::Message { msg, gap_before } => {
                let message_lines =
                    transcript_message_lines(msg, body_mode, row_width, palette, gap_before);
                for line in message_lines.into_iter().skip(remaining_skip) {
                    if lines.len() >= height {
                        return lines;
                    }
                    lines.push(line);
                }
            }
            TranscriptRenderItem::Gap => {
                if remaining_skip == 0 {
                    if lines.len() >= height {
                        return lines;
                    }
                    lines.push(transcript_gap_line());
                }
            }
        }
        remaining_skip = 0;
    }

    lines
}

#[derive(Clone, Copy)]
enum TranscriptRenderItem<'a> {
    Message { msg: &'a Message, gap_before: bool },
    Gap,
}

fn transcript_render_items<'a>(
    messages: &'a [Message],
    role_filter: TranscriptRoleFilter,
    search_query: &str,
) -> Vec<TranscriptRenderItem<'a>> {
    let items = match role_filter {
        TranscriptRoleFilter::All => messages
            .iter()
            .map(|msg| TranscriptRenderItem::Message {
                msg,
                gap_before: false,
            })
            .collect(),
        TranscriptRoleFilter::Conversation => conversation_transcript_render_items(messages),
        TranscriptRoleFilter::Condensed => condensed_transcript_render_items(messages),
        TranscriptRoleFilter::UserOnly => messages
            .iter()
            .filter(|msg| matches!(msg.kind, MessageKind::User))
            .map(|msg| TranscriptRenderItem::Message {
                msg,
                gap_before: false,
            })
            .collect(),
    };
    filter_transcript_render_items(items, search_query)
}

fn filter_transcript_render_items<'a>(
    items: Vec<TranscriptRenderItem<'a>>,
    search_query: &str,
) -> Vec<TranscriptRenderItem<'a>> {
    let needle = search_query.trim().to_lowercase();
    if needle.is_empty() {
        return items;
    }

    items
        .into_iter()
        .filter(|item| match item {
            TranscriptRenderItem::Message { msg, .. } => {
                msg.content.to_lowercase().contains(&needle)
            }
            TranscriptRenderItem::Gap => false,
        })
        .collect()
}

fn conversation_transcript_render_items(messages: &[Message]) -> Vec<TranscriptRenderItem<'_>> {
    let mut items = Vec::new();
    let mut seen_visible = false;
    let mut hidden_since_visible = false;

    for msg in messages {
        if !transcript_message_is_visible(&msg.kind, TranscriptRoleFilter::Conversation) {
            if seen_visible {
                hidden_since_visible = true;
            }
            continue;
        }

        items.push(TranscriptRenderItem::Message {
            msg,
            gap_before: hidden_since_visible,
        });
        seen_visible = true;
        hidden_since_visible = false;
    }

    items
}

fn condensed_transcript_render_items(messages: &[Message]) -> Vec<TranscriptRenderItem<'_>> {
    let mut items = Vec::new();
    let mut idx = 0;

    while idx < messages.len() {
        if matches!(messages[idx].kind, MessageKind::User) {
            items.push(TranscriptRenderItem::Message {
                msg: &messages[idx],
                gap_before: false,
            });
            idx += 1;
            continue;
        }

        let block_start = idx;
        while idx < messages.len() && !matches!(messages[idx].kind, MessageKind::User) {
            idx += 1;
        }
        push_condensed_assistant_block(&messages[block_start..idx], &mut items);
    }

    items
}

fn push_condensed_assistant_block<'a>(
    block: &'a [Message],
    items: &mut Vec<TranscriptRenderItem<'a>>,
) {
    let assistant_positions: Vec<usize> = block
        .iter()
        .enumerate()
        .filter_map(|(idx, msg)| matches!(msg.kind, MessageKind::Assistant).then_some(idx))
        .collect();

    let Some(&first) = assistant_positions.first() else {
        return;
    };
    let last = *assistant_positions.last().unwrap_or(&first);

    items.push(TranscriptRenderItem::Message {
        msg: &block[first],
        gap_before: false,
    });

    if last == first {
        return;
    }

    if last > first + 1 {
        items.push(TranscriptRenderItem::Gap);
    }
    items.push(TranscriptRenderItem::Message {
        msg: &block[last],
        gap_before: false,
    });
}

fn transcript_render_item_row_count(
    item: &TranscriptRenderItem<'_>,
    body_mode: TranscriptBodyMode,
    role_filter: TranscriptRoleFilter,
) -> usize {
    match item {
        TranscriptRenderItem::Message { msg, .. } => {
            transcript_message_row_count(msg, body_mode, role_filter)
        }
        TranscriptRenderItem::Gap => 1,
    }
}

fn transcript_gap_line() -> Line<'static> {
    Line::from(Span::styled(
        "⋮",
        Style::default()
            .fg(Color::DarkGray)
            .add_modifier(Modifier::DIM),
    ))
}

fn transcript_message_lines(
    msg: &Message,
    body_mode: TranscriptBodyMode,
    row_width: usize,
    palette: TranscriptPalette,
    gap_before: bool,
) -> Vec<Line<'static>> {
    // Content is pre-sanitized at load time by prepare_transcript_messages.
    // ToolCall content holds the pre-computed preview string.
    let content = transcript_render_content(&msg.content);
    let assistant_prefix = if gap_before { "⋮ " } else { "● " };
    match &msg.kind {
        MessageKind::User if body_mode == TranscriptBodyMode::Full => {
            expanded_message_lines("> ", &content, palette.user_style(), row_width)
        }
        MessageKind::Assistant if body_mode == TranscriptBodyMode::Full => expanded_message_lines(
            assistant_prefix,
            &content,
            palette.assistant_style(),
            row_width,
        ),
        MessageKind::User if body_mode == TranscriptBodyMode::Thoughtstream => {
            let content = transcript_thoughtstream_content(&content);
            expanded_message_lines("> ", &content, palette.user_style(), row_width)
        }
        MessageKind::Assistant if body_mode == TranscriptBodyMode::Thoughtstream => {
            let content = transcript_thoughtstream_content(&content);
            expanded_message_lines(
                assistant_prefix,
                &content,
                palette.assistant_style(),
                row_width,
            )
        }
        MessageKind::User => vec![collapsed_message_line(
            "> ",
            &content,
            palette.user_style(),
            row_width,
        )],
        MessageKind::Assistant => vec![collapsed_message_line(
            assistant_prefix,
            &content,
            palette.assistant_style(),
            row_width,
        )],
        MessageKind::ToolCall { .. } => vec![collapsed_message_line(
            "● ",
            &content,
            Style::default().fg(Color::Yellow),
            row_width,
        )],
        MessageKind::ToolOutput => vec![collapsed_message_line(
            "  ⎿ ",
            &content,
            Style::default().fg(Color::DarkGray),
            row_width,
        )],
        MessageKind::Status => vec![collapsed_message_line(
            "",
            &content,
            Style::default()
                .fg(Color::DarkGray)
                .add_modifier(Modifier::DIM),
            row_width,
        )],
    }
}

fn transcript_thoughtstream_content(content: &str) -> Cow<'_, str> {
    distill_prompt_thoughtstream(content)
        .map(Cow::Owned)
        .unwrap_or(Cow::Borrowed(super::transcript::TRANSCRIPT_SNIP_MARKER))
}

fn transcript_render_content(content: &str) -> Cow<'_, str> {
    if content
        .chars()
        .any(|ch| ch == '\x1b' || ch.is_control() && !matches!(ch, '\n' | '\r' | '\t'))
    {
        Cow::Owned(sanitize_transcript_text(content))
    } else {
        Cow::Borrowed(content)
    }
}

#[derive(Clone, Copy)]
pub struct TranscriptPalette {
    assistant_text: Color,
    user_text: Color,
    user_highlight_bg: Color,
}

impl TranscriptPalette {
    fn user_style(self) -> Style {
        // User prompts need a row highlight, but the highlight is a neutral
        // inverse of normal text, not an inverse of the harness swatch. The
        // harness color remains the foreground cue.
        Style::default()
            .fg(self.user_text)
            .bg(self.user_highlight_bg)
    }

    fn assistant_style(self) -> Style {
        Style::default().fg(self.assistant_text)
    }
}

fn inverted_rgb_color(color: Color) -> Color {
    let (r, g, b) = color_to_rgb(color).unwrap_or((255, 255, 255));
    Color::Rgb(255 - r, 255 - g, 255 - b)
}

fn color_to_rgb(color: Color) -> Option<(u8, u8, u8)> {
    match color {
        Color::Rgb(r, g, b) => Some((r, g, b)),
        Color::Black => Some((0, 0, 0)),
        Color::Red => Some((255, 0, 0)),
        Color::Green => Some((0, 255, 0)),
        Color::Yellow => Some((255, 255, 0)),
        Color::Blue => Some((0, 0, 255)),
        Color::Magenta => Some((255, 0, 255)),
        Color::Cyan => Some((0, 255, 255)),
        Color::Gray => Some((160, 160, 160)),
        Color::DarkGray => Some((80, 80, 80)),
        Color::White => Some((255, 255, 255)),
        _ => None,
    }
}

fn normal_text_fg() -> Color {
    Color::White
}

pub fn transcript_palette(agent_kind: AgentKind) -> TranscriptPalette {
    let (r, g, b) = hex_rgb(agent_kind.accent_color()).unwrap_or((160, 160, 160));
    let accent = Color::Rgb(r, g, b);

    TranscriptPalette {
        assistant_text: accent,
        user_text: accent,
        user_highlight_bg: inverted_rgb_color(normal_text_fg()),
    }
}

fn sanitize_transcript_text(text: &str) -> String {
    #[derive(Clone, Copy)]
    enum EscapeState {
        Ground,
        Esc,
        Csi,
        Osc,
        String,
        StringEsc,
        Designate,
    }

    let mut out = String::with_capacity(text.len());
    let mut state = EscapeState::Ground;

    for ch in text.chars() {
        match state {
            EscapeState::Ground => match ch {
                '\x1b' => state = EscapeState::Esc,
                '\n' | '\r' | '\t' => out.push(ch),
                ch if ch.is_control() => {}
                _ => out.push(ch),
            },
            EscapeState::Esc => {
                state = match ch {
                    '[' => EscapeState::Csi,
                    ']' => EscapeState::Osc,
                    'P' | 'X' | '^' | '_' => EscapeState::String,
                    '(' | ')' | '*' | '+' | '-' | '.' | '/' | '#' | '%' => EscapeState::Designate,
                    _ => EscapeState::Ground,
                };
            }
            EscapeState::Csi => {
                if ('@'..='~').contains(&ch) {
                    state = EscapeState::Ground;
                }
            }
            EscapeState::Osc => match ch {
                '\x07' => state = EscapeState::Ground,
                '\x1b' => state = EscapeState::StringEsc,
                _ => {}
            },
            EscapeState::String => {
                if ch == '\x1b' {
                    state = EscapeState::StringEsc;
                }
            }
            EscapeState::StringEsc => {
                state = if ch == '\\' {
                    EscapeState::Ground
                } else {
                    EscapeState::String
                };
            }
            EscapeState::Designate => {
                state = EscapeState::Ground;
            }
        }
    }

    out
}

fn expanded_message_lines(
    prefix: &str,
    content: &str,
    style: Style,
    row_width: usize,
) -> Vec<Line<'static>> {
    if row_width == 0 {
        return vec![Line::from("")];
    }

    let prefix_width = display_width(prefix);
    let continuation_prefix = " ".repeat(prefix_width);
    let mut lines = Vec::new();

    for (idx, part) in expanded_message_parts(content).into_iter().enumerate() {
        let line_prefix = if idx == 0 {
            prefix.to_string()
        } else {
            continuation_prefix.clone()
        };
        let mut spans = Vec::new();
        let mut line_width = 0;
        push_span(&mut spans, &mut line_width, line_prefix, style);
        push_span(&mut spans, &mut line_width, part, style);
        if line_width < row_width {
            let pad_width = row_width - line_width;
            push_span(&mut spans, &mut line_width, " ".repeat(pad_width), style);
        }
        lines.push(Line::from(spans));
    }

    if lines.is_empty() {
        vec![collapsed_message_line(prefix, "", style, row_width)]
    } else {
        lines
    }
}

fn expanded_message_parts(content: &str) -> Vec<String> {
    let normalized = content.replace("\r\n", "\n").replace('\r', "\n");
    normalized
        .split('\n')
        .map(|part| part.to_string())
        .collect()
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

    let mut spans = Vec::new();
    let mut line_width = 0;
    push_span(&mut spans, &mut line_width, prefix.to_string(), style);
    push_collapsed_preview(&mut spans, &mut line_width, &clean, content_width, style);

    if line_width < row_width {
        let pad_width = row_width - line_width;
        push_span(&mut spans, &mut line_width, " ".repeat(pad_width), style);
    }

    Line::from(spans)
}

fn push_collapsed_preview(
    spans: &mut Vec<Span<'static>>,
    line_width: &mut usize,
    content: &str,
    max_width: usize,
    style: Style,
) {
    if display_width(content) <= max_width {
        push_span(spans, line_width, content.to_string(), style);
        return;
    }

    let marker_width = display_width(SNIP_MARKER);
    if max_width <= marker_width + 2 {
        push_span(spans, line_width, truncate_str(content, max_width), style);
        return;
    }

    let keep_width = max_width - marker_width;
    let head_budget = keep_width / 2;
    let tail_budget = keep_width.saturating_sub(head_budget);
    let head = take_width_from_start(content, head_budget);
    let tail = take_width_from_end(content, tail_budget);
    push_span(spans, line_width, head, style);
    push_span(spans, line_width, " ".to_string(), style);
    push_span(
        spans,
        line_width,
        CUT_MARKER_GLYPH,
        style.add_modifier(Modifier::DIM | Modifier::ITALIC | Modifier::CROSSED_OUT),
    );
    push_span(spans, line_width, " ".to_string(), style);
    push_span(spans, line_width, tail, style);
}

const CUT_MARKER_GLYPH: &str = "⌿";
const SNIP_MARKER: &str = " ⌿ ";

fn middle_truncate_str(s: &str, max_width: usize) -> String {
    if display_width(s) <= max_width {
        return s.to_string();
    }

    let marker_width = display_width(SNIP_MARKER);
    if max_width <= marker_width + 2 {
        return truncate_str(s, max_width);
    }

    let keep_width = max_width - marker_width;
    let head_budget = keep_width / 2;
    let tail_budget = keep_width.saturating_sub(head_budget);
    let head = take_width_from_start(s, head_budget);
    let tail = take_width_from_end(s, tail_budget);
    format!("{head}{SNIP_MARKER}{tail}")
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

fn age_style(tone: AgeTone, bright: bool) -> Style {
    let (r, g, b) = tone.rgb();
    let style = Style::default().fg(Color::Rgb(r, g, b));
    let style = if tone.is_bold() {
        style.add_modifier(Modifier::BOLD)
    } else {
        style
    };
    if tone.should_dim(bright) {
        style.add_modifier(Modifier::DIM)
    } else {
        style
    }
}

fn turn_style(tone: TurnTone, bright: bool) -> Style {
    let (r, g, b) = tone.rgb();
    let style = Style::default().fg(Color::Rgb(r, g, b));
    let style = if tone.is_bold() {
        style.add_modifier(Modifier::BOLD)
    } else {
        style
    };
    if tone.should_dim(bright) {
        style.add_modifier(Modifier::DIM)
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
        let args = r#"{"cmd":"git status --short","workdir":"/workspace/babel"}"#;
        assert_eq!(
            tool_call_preview("exec_command", args),
            "exec: git status --short"
        );
    }

    #[test]
    fn formats_edit_tool_call_with_short_path_and_replace_count() {
        let args = r#"{"file_path":"/workspace/babel/src/pager/ui.rs","old_string":"a\nb\n","new_string":"c\n"}"#;
        assert_eq!(
            tool_call_preview("Edit", args),
            "Edit: …/src/pager/ui.rs replace 2L"
        );
    }

    #[test]
    fn formats_read_tool_call_with_line_range() {
        let args = r#"{"file_path":"/workspace/babel/src/pager/ui.rs","offset":120,"limit":20}"#;
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
            created_time: 2,
            modified_time: 2,
            turns: 3,
            title: 10,
            prompt: 10,
        };
        let fitted = widths.fit(widths.total_width() + 20);
        assert_eq!(fitted.cwd, 12);
        assert!(fitted.prompt > fitted.title);
        assert_eq!(fitted.total_width(), widths.total_width() + 20);
    }

    #[test]
    fn row_width_measurement_includes_column_header_labels() {
        let widths = RowWidths::measure(CwdDisplayMode::Relative, false, std::iter::empty());

        assert_eq!(widths.state, 1);
        assert_eq!(widths.harness, "harness".len());
        assert_eq!(widths.workspace, "ws".len());
        assert_eq!(widths.cwd, "cwd".len());
        assert_eq!(widths.created_time, "ct".len());
        assert_eq!(widths.modified_time, "mt".len());
        assert_eq!(widths.turns, "turns".len());
        assert_eq!(widths.index, "#".len());
        assert_eq!(widths.title, "thread".len());
        assert_eq!(widths.prompt, "prompt".len());
    }

    #[test]
    fn token_cell_raw_mode_uses_plain_number() {
        assert_eq!(token_cell_text(54, 100, 3, false), "54");
        assert_eq!(token_cell_text(0, 100, 3, false), "");
    }

    #[test]
    fn token_cell_braille_mode_uses_maximum_dot_resolution() {
        assert_eq!(braille_token_bar(8, 8, 1), "⣿");
        assert_eq!(braille_token_bar(24, 24, 3), "⣿⣿⣿");
        assert_eq!(display_width(&token_cell_text(12, 24, 3, true)), 3);
    }

    #[test]
    fn project_frequency_color_preserves_workgroup_hue_when_dimmed() {
        let Color::Rgb(r, g, b) = frequency_rgb(1, 4, (0, 135, 175)) else {
            panic!("frequency color should render as rgb");
        };

        assert!(g > r, "cyan workgroup should keep green above red");
        assert!(b > r, "cyan workgroup should keep blue above red");
        assert!(b >= 70, "dimmed cyan should not collapse into neutral gray");
    }

    #[test]
    fn row_widths_shrink_flexible_text_cells_to_viewport() {
        let widths = RowWidths {
            state: 1,
            index: 2,
            harness: 6,
            workspace: 1,
            cwd: 40,
            created_time: 2,
            modified_time: 2,
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
    fn row_widths_respect_narrow_split_pane_width() {
        let widths = RowWidths {
            state: 1,
            index: 3,
            harness: 6,
            workspace: 28,
            cwd: 54,
            created_time: 4,
            modified_time: 4,
            turns: 4,
            title: 90,
            prompt: 120,
        };

        let fitted = widths.fit(82);

        assert!(fitted.total_width() <= 82);
        assert!(fitted.prompt < widths.prompt);
        assert!(fitted.title < widths.title);
        assert!(fitted.cwd < widths.cwd);
    }

    #[test]
    fn row_widths_cap_thread_so_prompt_keeps_room() {
        let widths = RowWidths {
            state: 1,
            index: 3,
            harness: 6,
            workspace: 1,
            cwd: 20,
            created_time: 4,
            modified_time: 4,
            turns: 4,
            title: 240,
            prompt: 160,
        };

        let fitted = widths.fit(140);
        let text_budget = 140 - fitted.fixed_width();

        assert!(fitted.total_width() <= 140);
        assert!(fitted.title <= (text_budget * RowWidths::TITLE_BUDGET_PERCENT) / 100);
        assert!(fitted.prompt >= (text_budget * RowWidths::PROMPT_FLOOR_PERCENT) / 100);
        assert!(fitted.prompt > fitted.title);
    }

    #[test]
    fn session_header_is_clamped_to_render_budget() {
        let widths = RowWidths {
            state: 1,
            index: 3,
            harness: 12,
            workspace: 28,
            cwd: 70,
            created_time: 6,
            modified_time: 6,
            turns: 6,
            title: 120,
            prompt: 120,
        };

        let line = render_session_header(&widths, CwdDisplayMode::Relative, false, 50);

        assert!(line_display_width(&line) <= 50);
    }

    #[test]
    fn session_row_is_clamped_to_render_budget() {
        let row = SessionRow {
            state_icon: "●",
            state_kind: StateKind::Working,
            harness: "codex".to_string(),
            native_id: "abc123".to_string(),
            filter_tag: " ",
            workspace: "123456789".to_string(),
            cwd: "~/holoq/repo-os/babel".to_string(),
            created_time: "456h".to_string(),
            created_time_tone: AgeTone::Month,
            modified_time: "123h".to_string(),
            modified_time_tone: AgeTone::Week,
            time: "123h".to_string(),
            time_tone: AgeTone::Week,
            turn_count: 999,
            turn_tone: TurnTone::from_turn_count(999),
            turns: "999t".to_string(),
            title: "a very long generated title that should never cross into transcript"
                .to_string(),
            last_prompt: "a very long user prompt that should be clipped to the list pane"
                .to_string(),
            accent: "#10a37f",
            ansi256: 36,
            bright: true,
            has_title: true,
        };
        let rendered = RenderedSessionRow {
            idx: 123,
            row,
            cwd: CwdCell::Text("~/holoq/repo-os/babel".to_string()),
        };
        let widths = RowWidths {
            state: 1,
            index: 3,
            harness: 12,
            workspace: 28,
            cwd: 70,
            created_time: 6,
            modified_time: 6,
            turns: 6,
            title: 120,
            prompt: 120,
        }
        .fit(60);

        let line = render_session_line(&rendered, &widths, 60, true, true, false, 999);

        assert!(line_display_width(&line) <= 60);
    }

    #[test]
    fn middle_truncation_uses_cut_marker_glyph() {
        let text = "abcdefghijklmnopqrstuvwxyz0123456789";
        let truncated = middle_truncate_str(text, 18);
        assert!(truncated.contains(CUT_MARKER_GLYPH));
        assert!(truncated.contains(SNIP_MARKER));
        assert!(!truncated.contains("snip"));
        assert!(!truncated.contains("[cut]"));
    }

    #[test]
    fn cell_middle_truncation_dims_cut_marker_span() {
        let mut spans = Vec::new();
        let mut width = 0;
        push_right_cell_mode(
            &mut spans,
            &mut width,
            "abcdefghijklmnopqrstuvwxyz0123456789",
            18,
            Style::default().fg(Color::White),
            Style::default(),
            true,
        );

        let cut = spans
            .iter()
            .find(|span| span.content.as_ref() == CUT_MARKER_GLYPH)
            .expect("cut marker span");
        assert!(cut.style.add_modifier.contains(Modifier::DIM));
    }

    #[test]
    fn collapsed_message_styles_cut_marker() {
        let line = collapsed_message_line(
            "> ",
            "abcdefghijklmnopqrstuvwxyz0123456789",
            Style::default().fg(Color::White),
            22,
        );
        let cut = line
            .spans
            .iter()
            .find(|span| span.content.as_ref() == CUT_MARKER_GLYPH)
            .expect("cut marker span");
        let cut_idx = line
            .spans
            .iter()
            .position(|span| span.content.as_ref() == CUT_MARKER_GLYPH)
            .expect("cut marker index");
        assert_eq!(line.spans[cut_idx - 1].content.as_ref(), " ");
        assert_eq!(line.spans[cut_idx + 1].content.as_ref(), " ");
        assert!(cut.style.add_modifier.contains(Modifier::DIM));
        assert!(cut.style.add_modifier.contains(Modifier::ITALIC));
        assert!(cut.style.add_modifier.contains(Modifier::CROSSED_OUT));
    }

    #[test]
    fn transcript_sanitizer_strips_ansi_and_control_sequences() {
        let text =
            "\x1b[31mred\x1b[0m plain \x1b]8;;https://example.test\x07link\x1b]8;;\x07 \x07done";

        assert_eq!(sanitize_transcript_text(text), "red plain link done");
    }

    #[test]
    fn transcript_tool_output_rendering_does_not_emit_escape_bytes() {
        let msg = Message {
            kind: MessageKind::ToolOutput,
            content: "\x1b[31mfailed\x1b[0m\n\x1b[2Kstill here".to_string(),
            line: 0,
        };

        let lines = transcript_message_lines(
            &msg,
            TranscriptBodyMode::Snip,
            80,
            transcript_palette(AgentKind::Codex),
            false,
        );
        let rendered = lines[0]
            .spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect::<String>();

        assert!(rendered.contains("failed"));
        assert!(rendered.contains("still here"));
        assert!(!rendered.contains('\x1b'));
        assert!(!rendered.contains("[31m"));
        assert!(!rendered.contains("[2K"));
    }

    #[test]
    fn expanded_messages_use_newline_rows_but_tool_rows_stay_clamped() {
        let messages = vec![
            Message {
                kind: MessageKind::User,
                content: "alpha\nbeta\ngamma".to_string(),
                line: 0,
            },
            Message {
                kind: MessageKind::ToolCall {
                    name: "Bash".to_string(),
                    args: "{\"command\":\"one\\ntwo\\nthree\"}".to_string(),
                },
                content: "ignored".to_string(),
                line: 1,
            },
            Message {
                kind: MessageKind::ToolOutput,
                content: "out one\nout two\nout three".to_string(),
                line: 2,
            },
        ];

        assert_eq!(
            transcript_rendered_row_count(
                &messages,
                TranscriptBodyMode::Snip,
                TranscriptRoleFilter::All,
                ""
            ),
            3
        );
        assert_eq!(
            transcript_rendered_row_count(
                &messages,
                TranscriptBodyMode::Full,
                TranscriptRoleFilter::All,
                ""
            ),
            5
        );
    }

    #[test]
    fn expanded_message_visible_lines_can_scroll_inside_one_message() {
        let messages = vec![Message {
            kind: MessageKind::Assistant,
            content: "one\ntwo\nthree".to_string(),
            line: 0,
        }];

        let lines = transcript_visible_lines(
            &messages,
            TranscriptBodyMode::Full,
            1,
            1,
            40,
            transcript_palette(AgentKind::Claude),
            TranscriptRoleFilter::All,
            "",
        );
        let text = lines[0]
            .spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect::<String>();

        assert!(text.contains("two"));
        assert!(!text.contains("snip"));
    }

    #[test]
    fn transcript_palette_uses_harness_swatch_for_user_and_assistant_text() {
        let palette = transcript_palette(AgentKind::Codex);

        assert_eq!(palette.assistant_style().fg, Some(Color::Rgb(16, 163, 127)));
        assert_eq!(palette.user_style().fg, Some(Color::Rgb(16, 163, 127)));
        assert_eq!(palette.user_style().bg, Some(Color::Rgb(0, 0, 0)));
        assert!(!palette
            .user_style()
            .add_modifier
            .contains(Modifier::REVERSED));
    }

    #[test]
    fn transcript_user_prompt_palette_uses_inverted_normal_text_background() {
        let palette = transcript_palette(AgentKind::Cursor);

        assert_eq!(palette.user_style().fg, Some(palette.user_text));
        assert_eq!(
            palette.user_style().bg,
            Some(inverted_rgb_color(normal_text_fg()))
        );
        assert!(!palette
            .user_style()
            .add_modifier
            .contains(Modifier::REVERSED));
    }

    #[test]
    fn user_only_transcript_filter_hides_assistant_and_tool_rows() {
        let messages = vec![
            Message {
                kind: MessageKind::Assistant,
                content: "assistant".to_string(),
                line: 0,
            },
            Message {
                kind: MessageKind::ToolCall {
                    name: "Bash".to_string(),
                    args: "{\"command\":\"echo nope\"}".to_string(),
                },
                content: "ignored".to_string(),
                line: 1,
            },
            Message {
                kind: MessageKind::User,
                content: "first prompt\nsecond line".to_string(),
                line: 2,
            },
            Message {
                kind: MessageKind::ToolOutput,
                content: "tool output".to_string(),
                line: 3,
            },
        ];

        assert_eq!(
            transcript_rendered_row_count(
                &messages,
                TranscriptBodyMode::Full,
                TranscriptRoleFilter::UserOnly,
                ""
            ),
            2
        );
        let lines = transcript_visible_lines(
            &messages,
            TranscriptBodyMode::Full,
            0,
            4,
            80,
            transcript_palette(AgentKind::Claude),
            TranscriptRoleFilter::UserOnly,
            "",
        );
        let text = lines
            .iter()
            .flat_map(|line| line.spans.iter())
            .map(|span| span.content.as_ref())
            .collect::<String>();

        assert!(text.contains("first prompt"));
        assert!(text.contains("second line"));
        assert!(!text.contains("assistant"));
        assert!(!text.contains("echo nope"));
        assert!(!text.contains("tool output"));
    }

    #[test]
    fn conversation_transcript_filter_hides_tools_but_keeps_assistant() {
        let messages = vec![
            Message {
                kind: MessageKind::User,
                content: "user prompt".to_string(),
                line: 0,
            },
            Message {
                kind: MessageKind::Assistant,
                content: "assistant prose".to_string(),
                line: 1,
            },
            Message {
                kind: MessageKind::ToolCall {
                    name: "Bash".to_string(),
                    args: "{\"command\":\"echo hidden\"}".to_string(),
                },
                content: "ignored".to_string(),
                line: 2,
            },
            Message {
                kind: MessageKind::ToolOutput,
                content: "tool output".to_string(),
                line: 3,
            },
            Message {
                kind: MessageKind::Status,
                content: "status".to_string(),
                line: 4,
            },
        ];

        assert_eq!(
            transcript_rendered_row_count(
                &messages,
                TranscriptBodyMode::Snip,
                TranscriptRoleFilter::Conversation,
                ""
            ),
            2
        );
        let lines = transcript_visible_lines(
            &messages,
            TranscriptBodyMode::Snip,
            0,
            5,
            80,
            transcript_palette(AgentKind::Claude),
            TranscriptRoleFilter::Conversation,
            "",
        );
        let text = lines
            .iter()
            .flat_map(|line| line.spans.iter())
            .map(|span| span.content.as_ref())
            .collect::<String>();

        assert!(text.contains("user prompt"));
        assert!(text.contains("assistant prose"));
        assert!(!text.contains("echo hidden"));
        assert!(!text.contains("tool output"));
        assert!(!text.contains("status"));
    }

    #[test]
    fn conversation_transcript_filter_marks_eclipsed_tool_gap_on_next_assistant() {
        let messages = vec![
            Message {
                kind: MessageKind::User,
                content: "user prompt".to_string(),
                line: 0,
            },
            Message {
                kind: MessageKind::ToolCall {
                    name: "Bash".to_string(),
                    args: "{\"command\":\"echo hidden\"}".to_string(),
                },
                content: "ignored".to_string(),
                line: 1,
            },
            Message {
                kind: MessageKind::ToolOutput,
                content: "tool output".to_string(),
                line: 2,
            },
            Message {
                kind: MessageKind::Assistant,
                content: "assistant prose".to_string(),
                line: 3,
            },
        ];

        let lines = transcript_visible_lines(
            &messages,
            TranscriptBodyMode::Snip,
            0,
            5,
            80,
            transcript_palette(AgentKind::Claude),
            TranscriptRoleFilter::Conversation,
            "",
        );
        let text = lines
            .iter()
            .flat_map(|line| line.spans.iter())
            .map(|span| span.content.as_ref())
            .collect::<String>();

        assert!(text.contains("> user prompt"));
        assert!(text.contains("⋮ assistant prose"));
        assert!(!text.contains("● assistant prose"));
        assert!(!text.contains("echo hidden"));
        assert!(!text.contains("tool output"));
    }

    #[test]
    fn condensed_transcript_filter_collapses_middle_assistant_nodes() {
        let messages = vec![
            Message {
                kind: MessageKind::User,
                content: "user prompt".to_string(),
                line: 0,
            },
            Message {
                kind: MessageKind::Assistant,
                content: "first assistant".to_string(),
                line: 1,
            },
            Message {
                kind: MessageKind::ToolCall {
                    name: "Bash".to_string(),
                    args: "{\"command\":\"echo hidden\"}".to_string(),
                },
                content: "ignored".to_string(),
                line: 2,
            },
            Message {
                kind: MessageKind::Assistant,
                content: "middle assistant".to_string(),
                line: 3,
            },
            Message {
                kind: MessageKind::Status,
                content: "status".to_string(),
                line: 4,
            },
            Message {
                kind: MessageKind::Assistant,
                content: "last assistant".to_string(),
                line: 5,
            },
        ];

        assert_eq!(
            transcript_rendered_row_count(
                &messages,
                TranscriptBodyMode::Snip,
                TranscriptRoleFilter::Condensed,
                ""
            ),
            4
        );
        let lines = transcript_visible_lines(
            &messages,
            TranscriptBodyMode::Snip,
            0,
            8,
            80,
            transcript_palette(AgentKind::Claude),
            TranscriptRoleFilter::Condensed,
            "",
        );
        let rendered: Vec<String> = lines
            .iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect::<String>()
            })
            .collect();

        assert_eq!(rendered.len(), 4);
        assert!(rendered[0].contains("> user prompt"));
        assert!(rendered[1].contains("first assistant"));
        assert_eq!(rendered[2], "⋮");
        assert!(rendered[3].contains("last assistant"));
        assert!(!rendered.join("\n").contains("middle assistant"));
        assert!(!rendered.join("\n").contains("echo hidden"));
        assert!(!rendered.join("\n").contains("status"));
    }

    #[test]
    fn transcript_search_filters_visible_rows_without_list_filter() {
        let messages = vec![
            Message {
                kind: MessageKind::User,
                content: "ordinary prompt".to_string(),
                line: 0,
            },
            Message {
                kind: MessageKind::Assistant,
                content: "needle response".to_string(),
                line: 1,
            },
            Message {
                kind: MessageKind::Assistant,
                content: "other response".to_string(),
                line: 2,
            },
        ];

        assert_eq!(
            transcript_rendered_row_count(
                &messages,
                TranscriptBodyMode::Snip,
                TranscriptRoleFilter::All,
                "needle"
            ),
            1
        );
        let lines = transcript_visible_lines(
            &messages,
            TranscriptBodyMode::Snip,
            0,
            3,
            80,
            transcript_palette(AgentKind::Codex),
            TranscriptRoleFilter::All,
            "needle",
        );
        let text = lines
            .iter()
            .flat_map(|line| line.spans.iter())
            .map(|span| span.content.as_ref())
            .collect::<String>();

        assert!(text.contains("needle response"));
        assert!(!text.contains("ordinary prompt"));
        assert!(!text.contains("other response"));
    }

    #[test]
    fn transcript_role_filter_cycles_through_all_conversation_condensed_user() {
        assert_eq!(
            TranscriptRoleFilter::All.cycle(),
            TranscriptRoleFilter::Conversation
        );
        assert_eq!(
            TranscriptRoleFilter::Conversation.cycle(),
            TranscriptRoleFilter::Condensed
        );
        assert_eq!(
            TranscriptRoleFilter::Condensed.cycle(),
            TranscriptRoleFilter::UserOnly
        );
        assert_eq!(
            TranscriptRoleFilter::UserOnly.cycle(),
            TranscriptRoleFilter::All
        );
        assert_eq!(
            TranscriptRoleFilter::All.previous(),
            TranscriptRoleFilter::UserOnly
        );
        assert_eq!(
            TranscriptRoleFilter::UserOnly.previous(),
            TranscriptRoleFilter::Condensed
        );
    }

    #[test]
    fn transcript_body_mode_cycle_is_reversible() {
        assert_eq!(TranscriptBodyMode::Snip.cycle(), TranscriptBodyMode::Full);
        assert_eq!(
            TranscriptBodyMode::Snip.previous(),
            TranscriptBodyMode::Thoughtstream
        );
        assert_eq!(
            TranscriptBodyMode::Thoughtstream.previous(),
            TranscriptBodyMode::Full
        );
    }

    #[test]
    fn transcript_title_includes_role_filter_label() {
        let mut app = ResumeApp::new(Vec::new(), None);
        assert_eq!(transcript_title(&mut app), "Transcript [all]");

        app.transcript.session_id = Some("abcdef123456".to_string());
        app.transcript.role_filter = TranscriptRoleFilter::Conversation;

        assert_eq!(
            transcript_title(&mut app),
            "Transcript [abcdef12] [conversation]"
        );
    }

    #[test]
    fn transcript_title_includes_selected_session_cwd() {
        let session = EnrichedSession {
            agent_kind: AgentKind::Codex,
            native_id: "abcdef123456".to_string(),
            session_key: "codex:abcdef123456".to_string(),
            project_path: Some(Path::new("/workspace/babel").to_path_buf()),
            display_name: Some("thread".to_string()),
            generated_title: None,
            last_prompt: None,
            turn_count: 1,
            created_at: 1,
            last_seen_at: 2,
            interactive: true,
            command_only: false,
            has_title: false,
            hidden: false,
            custom_icon: None,
            unread: false,
            running_status: super::super::session_list::RunningStatus::Inactive,
        };
        let mut app = ResumeApp::new(vec![session], None);
        app.transcript.session_id = Some("abcdef123456".to_string());

        assert_eq!(
            transcript_title(&mut app),
            "Transcript [abcdef12] [all] cwd:/workspace/babel"
        );
    }

    #[test]
    fn cwd_scope_label_uses_filter_scope_not_display_mode() {
        let path = Path::new("/workspace/project");
        assert_eq!(cwd_scope_label(path), "cwd:/workspace/project");
    }

    #[test]
    fn cwd_display_mode_column_labels_are_status_bar_labels() {
        assert_eq!(
            [
                CwdDisplayMode::Relative.column_label(),
                CwdDisplayMode::Absolute.column_label(),
                CwdDisplayMode::Project.column_label(),
                CwdDisplayMode::TouchedProjects.column_label(),
            ],
            ["cwd", "abs", "proj", "touch"]
        );
    }

    #[test]
    fn session_header_uses_cwd_display_mode_column_label() {
        let widths = RowWidths {
            state: 1,
            index: 1,
            harness: 7,
            workspace: 2,
            cwd: 5,
            created_time: 2,
            modified_time: 2,
            turns: 5,
            title: 6,
            prompt: 6,
        };
        let line = render_session_header(&widths, CwdDisplayMode::TouchedProjects, false, 80);
        let text = line
            .spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect::<String>();

        assert!(text.contains("touch"));
    }

    fn line_display_width(line: &Line<'_>) -> usize {
        line.spans
            .iter()
            .map(|span| display_width(span.content.as_ref()))
            .sum()
    }
}
