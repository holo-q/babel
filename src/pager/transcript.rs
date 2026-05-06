//! Transcript View - Right panel state
//!
//! Displays parsed messages from selected session.

use scrollparse::Message;
use serde::{Deserialize, Serialize};

/// Which transcript roles are visible in the preview pane.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TranscriptRoleFilter {
    #[default]
    All,
    Conversation,
    UserOnly,
}

impl TranscriptRoleFilter {
    pub fn cycle(self) -> Self {
        match self {
            Self::All => Self::Conversation,
            Self::Conversation => Self::UserOnly,
            Self::UserOnly => Self::All,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::All => "all",
            Self::Conversation => "conversation",
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
    /// Whether user/assistant message bodies should render as full newline rows
    /// instead of a one-row snip preview. Tool rows stay one-row regardless:
    /// transcript navigation must never degrade into scrolling through command
    /// output or JSON arguments by accident.
    pub expand_messages: bool,
    /// Role filter for cutting assistant/tool/status rows out of the transcript.
    pub role_filter: TranscriptRoleFilter,
    /// Cached total row count keyed by (expand, filter). Self-invalidates when
    /// settings change; must be cleared on message load/clear.
    pub cached_row_count: Option<(bool, TranscriptRoleFilter, usize)>,
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

    /// Toggle user/assistant message body expansion.
    pub fn toggle_message_expansion(&mut self) -> bool {
        self.expand_messages = !self.expand_messages;
        self.expand_messages
    }

    /// Cycle between all rows, conversation-only, and user-prompt-only transcript.
    pub fn toggle_role_filter(&mut self) -> TranscriptRoleFilter {
        self.role_filter = self.role_filter.cycle();
        self.role_filter
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
