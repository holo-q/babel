//! Transcript View - Right panel state
//!
//! Displays parsed messages from selected session.

use scrollparse::Message;

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
}

impl TranscriptView {
    pub fn new() -> Self {
        Self::default()
    }

    /// Load a new transcript
    pub fn load(&mut self, session_id: String, messages: Vec<Message>) {
        self.session_id = Some(session_id);
        self.messages = messages;
        self.scroll_offset = 0;
        self.notice = None;
    }

    /// Show an empty-state notice for a selected session
    pub fn notice(&mut self, session_id: String, notice: String) {
        self.session_id = Some(session_id);
        self.messages.clear();
        self.scroll_offset = 0;
        self.notice = Some(notice);
    }

    /// Clear the transcript
    pub fn clear(&mut self) {
        self.session_id = None;
        self.messages.clear();
        self.scroll_offset = 0;
        self.notice = None;
    }

    /// Scroll down
    pub fn scroll_down(&mut self, lines: usize) {
        let max = self.messages.len().saturating_sub(1);
        self.scroll_offset = (self.scroll_offset + lines).min(max);
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
        self.scroll_offset = self.messages.len().saturating_sub(1);
    }
}
