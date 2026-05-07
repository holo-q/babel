//! Prompt-history queries across native harness sessions.
//!
//! `babel prompts` is a directory-context view: it scans native session stores,
//! loads readable transcripts where babel has a parser, and emits user prompts
//! with optional local context rows. Transcript parsers currently preserve line
//! order but not per-message timestamps, so rows are ordered by session
//! `last_seen_at` and then newest prompt within that session. Count windows use
//! an age-bucket spread instead of a raw global truncate: the command should
//! surface the directory's prompt history, not let one 100-turn session eat the
//! entire page.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use console::{style, Term};
use ratatui::style::{Color, Modifier, Style as TuiStyle};
use ratatui::text::Line;
use scrollparse::{Message, MessageKind};
use serde::Serialize;

use babel::harness;
use babel::native_sessions::{NativeSession, SessionFilters};
use babel::pager::{
    distill_prompt_thoughtstream, distilled_human_prompt, prepare_transcript_messages,
    transcript_palette, transcript_visible_lines, TranscriptBodyMode, TranscriptRoleFilter,
    TRANSCRIPT_SNIP_MARKER,
};

const DEFAULT_PROMPT_COUNT: usize = 20;

#[derive(Clone, Copy, Debug)]
enum PromptWindow {
    Count(usize),
    Since(i64),
}

#[derive(Debug)]
struct PromptArgs {
    root: PathBuf,
    window: PromptWindow,
}

#[derive(Serialize)]
struct PromptOutput {
    session: String,
    harness: String,
    project: Option<String>,
    line: usize,
    prompt: String,
    context: Vec<PromptContextOutput>,
}

#[derive(Serialize)]
struct PromptContextOutput {
    line: usize,
    role: &'static str,
    text: String,
}

#[derive(Clone)]
struct PromptRow {
    session: NativeSession,
    line: usize,
    prompt: String,
    context: Vec<PromptContextRow>,
    messages: Vec<Message>,
}

#[derive(Clone)]
struct PromptContextRow {
    line: usize,
    role: &'static str,
    text: String,
}

struct PromptSessionRows {
    rows: Vec<PromptRow>,
    cursor: usize,
}

struct PromptBucket {
    key: u8,
    weight: usize,
    sessions: Vec<PromptSessionRows>,
    cursor: usize,
}

pub async fn cmd_prompts(
    args: Vec<String>,
    recursive: bool,
    context_rows: Option<usize>,
    token_budget: Option<usize>,
    filter: Option<String>,
    json: bool,
) -> Result<()> {
    let spec = parse_prompt_args(args)?;
    let filter = filter.map(|value| value.to_lowercase());
    let now = unix_now();
    let mut sessions = sessions_for_context(&spec.root, recursive);
    sessions.sort_by(|a, b| b.last_seen_at.cmp(&a.last_seen_at));

    let mut session_rows = Vec::new();
    for session in sessions {
        if let PromptWindow::Since(seconds) = spec.window {
            if session.last_seen_at < now.saturating_sub(seconds) {
                continue;
            }
        }

        let Some(path) = harness::find_session_transcript(session.agent_kind, &session.native_id)
            .with_context(|| format!("find transcript for {}", session.native_id))?
        else {
            continue;
        };
        let Ok(mut messages) = harness::parse_transcript(session.agent_kind, &path) else {
            continue;
        };
        prepare_transcript_messages(&mut messages);
        let rows = prompt_rows_for_session(
            session,
            &messages,
            context_rows,
            token_budget,
            filter.as_deref(),
        );
        if !rows.is_empty() {
            session_rows.push(PromptSessionRows { rows, cursor: 0 });
        }
    }

    let rows = match spec.window {
        PromptWindow::Count(count) => spread_prompt_rows(session_rows, count, now),
        PromptWindow::Since(_) => session_rows
            .into_iter()
            .flat_map(|session| session.rows)
            .collect(),
    };

    if json {
        let out = rows.iter().map(prompt_output).collect::<Vec<_>>();
        println!("{}", serde_json::to_string_pretty(&out)?);
    } else {
        print_prompt_rows(&spec.root, recursive, &rows);
    }
    Ok(())
}

fn parse_prompt_args(args: Vec<String>) -> Result<PromptArgs> {
    let mut root = None;
    let mut window = None;

    for arg in args {
        if let Some(parsed) = parse_window_arg(&arg)? {
            if window.replace(parsed).is_some() {
                anyhow::bail!("multiple prompt windows supplied");
            }
        } else if root.replace(PathBuf::from(&arg)).is_some() {
            anyhow::bail!("multiple prompt paths supplied");
        }
    }

    Ok(PromptArgs {
        root: root.unwrap_or(std::env::current_dir()?),
        window: window.unwrap_or(PromptWindow::Count(DEFAULT_PROMPT_COUNT)),
    })
}

fn parse_window_arg(arg: &str) -> Result<Option<PromptWindow>> {
    if let Ok(count) = arg.parse::<usize>() {
        return Ok(Some(PromptWindow::Count(count)));
    }

    let Some((amount, unit)) = split_duration(arg) else {
        return Ok(None);
    };
    let amount = amount
        .parse::<i64>()
        .with_context(|| format!("invalid duration {arg:?}"))?;
    let seconds = match unit {
        "s" => amount,
        "m" => amount * 60,
        "h" => amount * 60 * 60,
        "d" => amount * 24 * 60 * 60,
        "w" => amount * 7 * 24 * 60 * 60,
        "mo" => amount * 30 * 24 * 60 * 60,
        _ => return Ok(None),
    };
    Ok(Some(PromptWindow::Since(seconds)))
}

fn split_duration(arg: &str) -> Option<(&str, &str)> {
    let split = arg.find(|ch: char| !ch.is_ascii_digit())?;
    if split == 0 {
        return None;
    }
    let (amount, unit) = arg.split_at(split);
    Some((amount, unit))
}

fn sessions_for_context(root: &Path, recursive: bool) -> Vec<NativeSession> {
    let filters = SessionFilters {
        sub: true,
        oneshot: true,
        commands: true,
        all: true,
    };
    let root = comparable_path(root);
    babel::native_sessions::scan_all(None, &filters)
        .into_iter()
        .filter(|session| {
            session.project_path.as_deref().is_some_and(|path| {
                project_matches(&comparable_path(Path::new(path)), &root, recursive)
            })
        })
        .collect()
}

fn project_matches(project: &Path, root: &Path, recursive: bool) -> bool {
    if recursive {
        project.starts_with(root)
    } else {
        project == root
    }
}

fn comparable_path(path: &Path) -> PathBuf {
    expand_home(path)
        .canonicalize()
        .unwrap_or_else(|_| expand_home(path))
}

fn expand_home(path: &Path) -> PathBuf {
    let Some(text) = path.to_str() else {
        return path.to_path_buf();
    };
    if text == "~" {
        return dirs::home_dir().unwrap_or_else(|| path.to_path_buf());
    }
    if let Some(rest) = text.strip_prefix("~/") {
        return dirs::home_dir()
            .map(|home| home.join(rest))
            .unwrap_or_else(|| path.to_path_buf());
    }
    path.to_path_buf()
}

fn prompt_rows_for_session(
    session: NativeSession,
    messages: &[Message],
    context_rows: Option<usize>,
    token_budget: Option<usize>,
    filter: Option<&str>,
) -> Vec<PromptRow> {
    let mut rows = Vec::new();
    for (idx, message) in messages.iter().enumerate().rev() {
        if !matches!(message.kind, MessageKind::User) {
            continue;
        }
        let Some(prompt) = distilled_human_prompt(&message.content) else {
            continue;
        };
        if let Some(filter) = filter {
            if !prompt.to_lowercase().contains(filter) {
                continue;
            }
        }
        let start = prompt_slice_start(messages, idx, context_rows, token_budget);
        let mut row_messages = messages[start..=idx].to_vec();
        if let Some(current) = row_messages.last_mut() {
            current.content = prompt.clone();
        }
        distill_context_user_messages(&mut row_messages);
        rows.push(PromptRow {
            session: session.clone(),
            line: message.line,
            prompt,
            context: prompt_context(&row_messages, 0, row_messages.len().saturating_sub(1)),
            messages: row_messages,
        });
    }
    rows
}

fn distill_context_user_messages(messages: &mut [Message]) {
    if messages.is_empty() {
        return;
    }

    let prompt_idx = messages.len().saturating_sub(1);
    for message in &mut messages[..prompt_idx] {
        if matches!(message.kind, MessageKind::User) {
            message.content = distill_prompt_thoughtstream(&message.content)
                .unwrap_or_else(|| TRANSCRIPT_SNIP_MARKER.to_string());
        }
    }
}

fn spread_prompt_rows(sessions: Vec<PromptSessionRows>, count: usize, now: i64) -> Vec<PromptRow> {
    if count == 0 {
        return Vec::new();
    }

    let mut buckets = age_buckets(sessions, now);
    let mut out = Vec::with_capacity(count);
    while out.len() < count {
        let mut progressed = false;
        for bucket in &mut buckets {
            for _ in 0..bucket.weight {
                if out.len() >= count {
                    break;
                }
                if let Some(row) = bucket.take_next() {
                    out.push(row);
                    progressed = true;
                }
            }
        }
        if !progressed {
            break;
        }
    }

    out.sort_by(|a, b| {
        b.session
            .last_seen_at
            .cmp(&a.session.last_seen_at)
            .then_with(|| b.line.cmp(&a.line))
    });
    out
}

fn age_buckets(sessions: Vec<PromptSessionRows>, now: i64) -> Vec<PromptBucket> {
    let mut buckets = Vec::<PromptBucket>::new();
    for session in sessions {
        let key = session
            .rows
            .first()
            .map(|row| age_bucket_key(now.saturating_sub(row.session.last_seen_at)))
            .unwrap_or(u8::MAX);
        if let Some(bucket) = buckets.iter_mut().find(|bucket| bucket.key == key) {
            bucket.sessions.push(session);
        } else {
            buckets.push(PromptBucket {
                key,
                weight: age_bucket_weight(key),
                sessions: vec![session],
                cursor: 0,
            });
        }
    }
    buckets.sort_by_key(|bucket| bucket.key);
    buckets
}

fn age_bucket_key(seconds_ago: i64) -> u8 {
    const DAY: i64 = 24 * 60 * 60;
    match seconds_ago.max(0) / DAY {
        0 => 0,
        1 => 1,
        2..=7 => 7,
        8..=30 => 30,
        _ => u8::MAX,
    }
}

fn age_bucket_weight(key: u8) -> usize {
    match key {
        0 => 4,
        1 => 3,
        7 => 2,
        _ => 1,
    }
}

impl PromptBucket {
    fn take_next(&mut self) -> Option<PromptRow> {
        if self.sessions.is_empty() {
            return None;
        }

        for _ in 0..self.sessions.len() {
            let idx = self.cursor % self.sessions.len();
            self.cursor = (idx + 1) % self.sessions.len();
            let session = &mut self.sessions[idx];
            if session.cursor < session.rows.len() {
                let row = session.rows[session.cursor].clone();
                session.cursor += 1;
                return Some(row);
            }
        }
        None
    }
}

fn prompt_slice_start(
    messages: &[Message],
    prompt_idx: usize,
    context_rows: Option<usize>,
    token_budget: Option<usize>,
) -> usize {
    if let Some(tokens) = token_budget {
        return token_context_start(messages, prompt_idx, tokens);
    }
    let rows = context_rows.unwrap_or(0);
    prompt_idx.saturating_sub(rows)
}

fn prompt_context(messages: &[Message], start: usize, prompt_idx: usize) -> Vec<PromptContextRow> {
    messages[start..prompt_idx]
        .iter()
        .map(context_row)
        .collect()
}

fn token_context_start(messages: &[Message], prompt_idx: usize, token_budget: usize) -> usize {
    if token_budget == 0 {
        return prompt_idx;
    }

    let mut start = prompt_idx;
    let mut used = 0;
    for (idx, message) in messages[..prompt_idx].iter().enumerate().rev() {
        let cost = approx_tokens(&message.content);
        if used > 0 && used + cost > token_budget {
            break;
        }
        used += cost;
        start = idx;
        if used >= token_budget {
            break;
        }
    }
    start
}

fn approx_tokens(text: &str) -> usize {
    (text.chars().count().max(1) + 3) / 4
}

fn context_row(message: &Message) -> PromptContextRow {
    PromptContextRow {
        line: message.line,
        role: role_name(&message.kind),
        text: message.content.clone(),
    }
}

fn role_name(kind: &MessageKind) -> &'static str {
    match kind {
        MessageKind::User => "user",
        MessageKind::Assistant => "assistant",
        MessageKind::ToolCall { .. } => "tool_call",
        MessageKind::ToolOutput => "tool_output",
        MessageKind::Status => "status",
    }
}

fn prompt_output(row: &PromptRow) -> PromptOutput {
    PromptOutput {
        session: row.session.native_id.clone(),
        harness: row.session.agent_kind.slug().to_string(),
        project: row.session.project_path.clone(),
        line: row.line,
        prompt: row.prompt.clone(),
        context: row
            .context
            .iter()
            .map(|ctx| PromptContextOutput {
                line: ctx.line,
                role: ctx.role,
                text: ctx.text.clone(),
            })
            .collect(),
    }
}

fn print_prompt_rows(root: &Path, recursive: bool, rows: &[PromptRow]) {
    println!(
        "Prompt history for {}{} ({}):",
        style(root.display()).bold(),
        if recursive { " recursively" } else { "" },
        rows.len()
    );
    println!();

    let row_width = terminal_transcript_width();
    let mut last_session = None;
    for row in rows {
        let session_id = row.session.native_id.as_str();
        if last_session != Some(session_id) {
            let project = row.session.project_path.as_deref().unwrap_or("");
            println!(
                "{} {} {}",
                style(row.session.agent_kind.slug()).cyan(),
                style(short_id(session_id)).dim(),
                style(project).dim()
            );
            last_session = Some(session_id);
        }
        print_transcript_slice(row, row_width);
    }
}

fn short_id(id: &str) -> String {
    id.chars().take(8).collect()
}

fn print_transcript_slice(row: &PromptRow, row_width: usize) {
    let lines = transcript_visible_lines(
        &row.messages,
        TranscriptBodyMode::Thoughtstream,
        0,
        usize::MAX,
        row_width,
        transcript_palette(row.session.agent_kind),
        TranscriptRoleFilter::All,
        "",
    );
    for line in lines {
        println!("  {}", line_styled_ansi(&line));
    }
}

fn line_styled_ansi(line: &Line<'_>) -> String {
    let mut out = String::new();
    for span in &line.spans {
        out.push_str(&style_ansi(span.style));
        out.push_str(span.content.as_ref());
    }
    out.push_str("\x1b[0m");
    out
}

fn style_ansi(style: TuiStyle) -> String {
    let mut codes = Vec::new();

    if style.add_modifier.contains(Modifier::BOLD) {
        codes.push("1".to_string());
    }
    if style.add_modifier.contains(Modifier::DIM) {
        codes.push("2".to_string());
    }
    if style.add_modifier.contains(Modifier::ITALIC) {
        codes.push("3".to_string());
    }
    if style.add_modifier.contains(Modifier::UNDERLINED) {
        codes.push("4".to_string());
    }
    if style.add_modifier.contains(Modifier::SLOW_BLINK) {
        codes.push("5".to_string());
    }
    if style.add_modifier.contains(Modifier::REVERSED) {
        codes.push("7".to_string());
    }
    if style.add_modifier.contains(Modifier::HIDDEN) {
        codes.push("8".to_string());
    }
    if style.add_modifier.contains(Modifier::CROSSED_OUT) {
        codes.push("9".to_string());
    }
    if let Some(fg) = style.fg {
        codes.push(color_ansi(fg, false));
    }
    if let Some(bg) = style.bg {
        codes.push(color_ansi(bg, true));
    }

    if codes.is_empty() {
        "\x1b[0m".to_string()
    } else {
        format!("\x1b[{}m", codes.join(";"))
    }
}

fn color_ansi(color: Color, background: bool) -> String {
    let base = if background { 48 } else { 38 };
    match color {
        Color::Reset => {
            if background {
                "49".to_string()
            } else {
                "39".to_string()
            }
        }
        Color::Black => named_color_ansi(background, 0),
        Color::Red => named_color_ansi(background, 1),
        Color::Green => named_color_ansi(background, 2),
        Color::Yellow => named_color_ansi(background, 3),
        Color::Blue => named_color_ansi(background, 4),
        Color::Magenta => named_color_ansi(background, 5),
        Color::Cyan => named_color_ansi(background, 6),
        Color::Gray => named_color_ansi(background, 7),
        Color::DarkGray => named_color_ansi(background, 8),
        Color::LightRed => named_color_ansi(background, 9),
        Color::LightGreen => named_color_ansi(background, 10),
        Color::LightYellow => named_color_ansi(background, 11),
        Color::LightBlue => named_color_ansi(background, 12),
        Color::LightMagenta => named_color_ansi(background, 13),
        Color::LightCyan => named_color_ansi(background, 14),
        Color::White => named_color_ansi(background, 15),
        Color::Indexed(index) => format!("{base};5;{index}"),
        Color::Rgb(r, g, b) => format!("{base};2;{r};{g};{b}"),
    }
}

fn named_color_ansi(background: bool, index: u8) -> String {
    match (background, index) {
        (false, 0..=7) => (30 + index).to_string(),
        (true, 0..=7) => (40 + index).to_string(),
        (false, 8..=15) => (90 + index - 8).to_string(),
        (true, 8..=15) => (100 + index - 8).to_string(),
        _ => unreachable!("named ANSI color index must fit 0..=15"),
    }
}

fn terminal_transcript_width() -> usize {
    usize::from(Term::stdout().size().1)
        .saturating_sub(2)
        .max(1)
}

fn unix_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}
