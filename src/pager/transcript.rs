//! Transcript View - Right panel state
//!
//! Displays parsed messages from selected session.

use scrollparse::{Message, MessageKind};
use serde::{Deserialize, Serialize};

pub const TRANSCRIPT_SNIP_MARKER: &str = "⌿";

/// How transcript message bodies are shaped before rendering.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TranscriptBodyMode {
    /// One terminal row per message, fitted with the row snip marker.
    #[default]
    Snip,
    /// Full user/assistant message bodies, wrapped by the transcript pane.
    Full,
    /// Keep user thoughtstream shape while folding pasted/context bulk.
    Thoughtstream,
}

impl TranscriptBodyMode {
    pub fn from_expand_messages(expand_messages: bool) -> Self {
        if expand_messages {
            Self::Full
        } else {
            Self::Snip
        }
    }

    pub fn cycle(self) -> Self {
        match self {
            Self::Snip => Self::Full,
            Self::Full => Self::Thoughtstream,
            Self::Thoughtstream => Self::Snip,
        }
    }

    pub fn previous(self) -> Self {
        match self {
            Self::Snip => Self::Thoughtstream,
            Self::Full => Self::Snip,
            Self::Thoughtstream => Self::Full,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Snip => "snip",
            Self::Full => "full",
            Self::Thoughtstream => "thought",
        }
    }

    pub fn expands_messages(self) -> bool {
        matches!(self, Self::Full | Self::Thoughtstream)
    }
}

/// Which transcript roles are visible in the preview pane.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TranscriptRoleFilter {
    #[default]
    All,
    Conversation,
    Condensed,
    UserOnly,
}

impl TranscriptRoleFilter {
    pub fn cycle(self) -> Self {
        match self {
            Self::All => Self::Conversation,
            Self::Conversation => Self::Condensed,
            Self::Condensed => Self::UserOnly,
            Self::UserOnly => Self::All,
        }
    }

    pub fn previous(self) -> Self {
        match self {
            Self::All => Self::UserOnly,
            Self::Conversation => Self::All,
            Self::Condensed => Self::Conversation,
            Self::UserOnly => Self::Condensed,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::All => "all",
            Self::Conversation => "conversation",
            Self::Condensed => "condensed",
            Self::UserOnly => "user prompts",
        }
    }
}

/// State for the transcript preview panel
#[derive(Debug, Default)]
pub struct TranscriptView {
    /// Parsed messages from JSONL
    pub messages: Vec<Message>,
    /// Current scroll offset
    pub scroll_offset: usize,
    /// Session ID being displayed
    pub session_id: Option<String>,
    /// Why there are no messages for the selected session.
    pub notice: Option<String>,
    /// Body shaping for user/assistant message rows. Tool rows stay one-row
    /// regardless: transcript navigation must never degrade into scrolling
    /// through command output or JSON arguments by accident.
    pub body_mode: TranscriptBodyMode,
    /// Role filter for cutting assistant/tool/status rows out of the transcript.
    pub role_filter: TranscriptRoleFilter,
    /// Free-text filter for this transcript pane. This is intentionally
    /// independent from the session-list filter so `/` can be used to inspect
    /// matching transcript rows while moving across sessions.
    pub search_query: String,
    /// Cached total row count keyed by (body, role, search). Self-invalidates
    /// when settings change; must be cleared on message load/clear.
    pub cached_row_count: Option<(TranscriptBodyMode, TranscriptRoleFilter, String, usize)>,
}

impl TranscriptView {
    pub fn new() -> Self {
        Self::default()
    }

    /// Load a new transcript
    pub fn load(&mut self, session_id: String, messages: Vec<Message>) {
        self.session_id = Some(session_id);
        self.messages = messages;
        self.scroll_bottom();
        self.notice = None;
        self.cached_row_count = None;
    }

    /// Show an empty-state notice for a selected session
    pub fn notice(&mut self, session_id: String, notice: String) {
        self.session_id = Some(session_id);
        self.messages.clear();
        self.scroll_offset = 0;
        self.notice = Some(notice);
        self.cached_row_count = None;
    }

    /// Clear the transcript
    pub fn clear(&mut self) {
        self.session_id = None;
        self.messages.clear();
        self.scroll_offset = 0;
        self.notice = None;
        self.cached_row_count = None;
    }

    /// Cycle user/assistant body shaping.
    pub fn cycle_body_mode(&mut self) -> TranscriptBodyMode {
        self.body_mode = self.body_mode.cycle();
        self.cached_row_count = None;
        self.body_mode
    }

    pub fn cycle_body_mode_reverse(&mut self) -> TranscriptBodyMode {
        self.body_mode = self.body_mode.previous();
        self.cached_row_count = None;
        self.body_mode
    }

    /// Cycle between all rows, conversation-only, condensed conversation, and
    /// user-prompt-only transcript.
    pub fn toggle_role_filter(&mut self) -> TranscriptRoleFilter {
        self.role_filter = self.role_filter.cycle();
        self.cached_row_count = None;
        self.role_filter
    }

    pub fn toggle_role_filter_reverse(&mut self) -> TranscriptRoleFilter {
        self.role_filter = self.role_filter.previous();
        self.cached_row_count = None;
        self.role_filter
    }

    pub fn set_search_filter(&mut self, query: String) {
        if self.search_query == query {
            return;
        }
        self.search_query = query;
        self.cached_row_count = None;
        self.scroll_bottom();
    }

    pub fn clear_search_filter(&mut self) {
        self.set_search_filter(String::new());
    }

    /// Scroll down
    pub fn scroll_down(&mut self, lines: usize) {
        self.scroll_offset = self.scroll_offset.saturating_add(lines);
    }

    /// Scroll up
    pub fn scroll_up(&mut self, lines: usize) {
        self.scroll_offset = self.scroll_offset.saturating_sub(lines);
    }

    /// Jump to top
    pub fn scroll_top(&mut self) {
        self.scroll_offset = 0;
    }

    /// Jump to bottom
    pub fn scroll_bottom(&mut self) {
        self.scroll_offset = usize::MAX;
    }
}

// --- Transcript geometry helpers ---
// Canonical home for row-count / visibility predicates used by both
// TranscriptView's scroll-snapshot logic and the UI rendering pipeline.
// Keeping a single copy prevents scroll-position vs render desync.

pub(crate) fn transcript_message_row_count(
    message: &Message,
    body_mode: TranscriptBodyMode,
    role_filter: TranscriptRoleFilter,
) -> usize {
    if !transcript_message_is_visible(&message.kind, role_filter) {
        return 0;
    }

    match body_mode {
        TranscriptBodyMode::Snip => 1,
        TranscriptBodyMode::Full if transcript_message_can_expand(&message.kind) => {
            expanded_message_row_count(&message.content)
        }
        TranscriptBodyMode::Thoughtstream if transcript_message_can_expand(&message.kind) => {
            distill_prompt_thoughtstream(&message.content)
                .map(|content| expanded_message_row_count(&content))
                .unwrap_or(1)
        }
        _ => 1,
    }
}

pub(crate) fn transcript_message_is_visible(
    kind: &MessageKind,
    role_filter: TranscriptRoleFilter,
) -> bool {
    match role_filter {
        TranscriptRoleFilter::All => true,
        TranscriptRoleFilter::Conversation => {
            matches!(kind, MessageKind::User | MessageKind::Assistant)
        }
        TranscriptRoleFilter::Condensed => {
            matches!(kind, MessageKind::User | MessageKind::Assistant)
        }
        TranscriptRoleFilter::UserOnly => matches!(kind, MessageKind::User),
    }
}

pub(crate) fn transcript_message_can_expand(kind: &MessageKind) -> bool {
    matches!(kind, MessageKind::User | MessageKind::Assistant)
}

pub(crate) fn expanded_message_row_count(content: &str) -> usize {
    let mut rows = 1;
    let mut chars = content.chars().peekable();
    while let Some(ch) = chars.next() {
        match ch {
            '\n' => rows += 1,
            '\r' => {
                rows += 1;
                if chars.peek() == Some(&'\n') {
                    chars.next();
                }
            }
            _ => {}
        }
    }
    rows
}

pub fn distilled_human_prompt(content: &str) -> Option<String> {
    let text = content.trim();
    if text.is_empty() {
        return None;
    }

    if is_harness_scaffold_prompt(text) || is_delegated_worker_prompt(text) {
        return None;
    }

    distill_prompt_thoughtstream(text)
}

pub fn distill_prompt_thoughtstream(content: &str) -> Option<String> {
    let without_brace_blocks = remove_depth0_brace_blocks(content);
    let clamped = clamp_middle_paragraphs(&without_brace_blocks);
    let distilled = collapse_blank_lines(&clamped);
    if distilled.is_empty() || distilled == TRANSCRIPT_SNIP_MARKER {
        None
    } else {
        Some(distilled)
    }
}

fn clamp_middle_paragraphs(content: &str) -> String {
    let paragraphs = prompt_paragraphs(content);
    match paragraphs.len() {
        0 => String::new(),
        1 | 2 => paragraphs.join("\n\n"),
        _ => format!(
            "{}\n\n{}\n\n{}",
            paragraphs.first().expect("paragraph exists"),
            TRANSCRIPT_SNIP_MARKER,
            paragraphs.last().expect("paragraph exists")
        ),
    }
}

fn prompt_paragraphs(content: &str) -> Vec<String> {
    let mut paragraphs = Vec::new();
    let mut current = Vec::<&str>::new();

    for line in content.lines() {
        if line.trim().is_empty() {
            push_prompt_paragraph(&mut paragraphs, &mut current);
        } else {
            current.push(line.trim_end());
        }
    }
    push_prompt_paragraph(&mut paragraphs, &mut current);

    paragraphs
}

fn push_prompt_paragraph(paragraphs: &mut Vec<String>, current: &mut Vec<&str>) {
    let paragraph = clamp_paragraph_lines(current).trim().to_string();
    if !paragraph.is_empty() {
        paragraphs.push(paragraph);
    }
    current.clear();
}

fn clamp_paragraph_lines(lines: &[&str]) -> String {
    match lines.len() {
        0 => String::new(),
        1 | 2 => lines.join("\n"),
        _ => format!(
            "{}\n{}\n{}",
            lines.first().expect("line exists"),
            TRANSCRIPT_SNIP_MARKER,
            lines.last().expect("line exists")
        ),
    }
}

fn remove_depth0_brace_blocks(content: &str) -> String {
    let mut out = String::new();
    let mut depth = 0usize;
    let mut in_string = false;
    let mut escaped = false;

    for ch in content.chars() {
        if depth == 0 {
            if ch == '{' {
                depth = 1;
                in_string = false;
                escaped = false;
                append_snip_marker(&mut out);
            } else {
                out.push(ch);
            }
            continue;
        }

        if escaped {
            escaped = false;
            continue;
        }
        if ch == '\\' && in_string {
            escaped = true;
            continue;
        }
        if ch == '"' {
            in_string = !in_string;
            continue;
        }
        if in_string {
            continue;
        }
        match ch {
            '{' => depth += 1,
            '}' => depth = depth.saturating_sub(1),
            _ => {}
        }
    }

    out
}

fn append_snip_marker(out: &mut String) {
    if out.ends_with(TRANSCRIPT_SNIP_MARKER) {
        return;
    }
    if !out.is_empty() && !out.ends_with(char::is_whitespace) {
        out.push(' ');
    }
    out.push_str(TRANSCRIPT_SNIP_MARKER);
    out.push(' ');
}

fn collapse_blank_lines(content: &str) -> String {
    let mut out = String::new();
    let mut blank_pending = false;

    for line in content.lines().map(str::trim_end) {
        if line.trim().is_empty() {
            blank_pending = !out.is_empty();
            continue;
        }
        if blank_pending && !out.ends_with('\n') {
            out.push('\n');
        }
        if !out.is_empty() {
            out.push('\n');
        }
        out.push_str(line);
        blank_pending = false;
    }

    out.trim().to_string()
}

fn is_harness_scaffold_prompt(text: &str) -> bool {
    const XML_SCAFFOLD_PREFIXES: &[&str] = &[
        "<system-reminder>",
        "<local-command-caveat>",
        "<command-name>",
        "<command-message>",
        "<command-args>",
        "<environment_context>",
    ];

    XML_SCAFFOLD_PREFIXES
        .iter()
        .any(|prefix| text.starts_with(prefix))
}

fn is_delegated_worker_prompt(text: &str) -> bool {
    text.starts_with("You are Worker ")
        || text.starts_with("Read-only audit subtask")
        || text.starts_with("Deep performance audit pass ")
        || (text.starts_with("You are working in ") && text.contains("DONE LOOKS LIKE"))
        || (text.starts_with("We are in ")
            && text.contains("Your lane:")
            && text.contains("You are not alone in the codebase"))
}
