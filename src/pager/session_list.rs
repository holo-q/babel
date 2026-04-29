//! Session List - Left panel state
//!
//! Manages the list of sessions with running status indicators.

use crate::utility::claude_storage::SessionInfo;
use crate::ActivityState;
use std::path::PathBuf;

/// Running status of a session
#[derive(Debug, Clone, Default)]
pub enum RunningStatus {
    /// Not running in any pane
    #[default]
    Inactive,
    /// Running in a specific pane
    Active {
        pane_id: u64,
        workspace: Option<i32>,
        activity_state: ActivityState,
    },
}

impl RunningStatus {
    /// Get status indicator character
    pub fn indicator(&self) -> char {
        match self {
            RunningStatus::Inactive => '○',
            RunningStatus::Active { activity_state, .. } => match activity_state {
                ActivityState::Idle => '◐',
                _ => '●',
            },
        }
    }

    /// Whether this session is running
    pub fn is_running(&self) -> bool {
        matches!(self, RunningStatus::Active { .. })
    }
}

/// Session with running status enrichment
#[derive(Debug, Clone)]
pub struct EnrichedSession {
    pub info: SessionInfo,
    pub running_status: RunningStatus,
}

/// State for the session list panel
#[derive(Debug)]
pub struct SessionListState {
    /// All sessions
    pub sessions: Vec<EnrichedSession>,
    /// Current cursor position
    pub cursor: usize,
    /// Scroll offset for display
    pub scroll_offset: usize,
    /// Whether showing all projects or just cwd
    pub show_all: bool,
    /// Current working directory for cwd filtering
    pub current_cwd: Option<PathBuf>,
    /// Search/filter query
    pub filter_query: String,
}

impl SessionListState {
    pub fn new(sessions: Vec<EnrichedSession>, current_cwd: Option<PathBuf>) -> Self {
        Self {
            sessions,
            cursor: 0,
            scroll_offset: 0,
            show_all: false,
            current_cwd,
            filter_query: String::new(),
        }
    }

    /// Get visible sessions (cwd-filtered + search-filtered)
    pub fn visible_sessions(&self) -> Vec<(usize, &EnrichedSession)> {
        self.sessions
            .iter()
            .enumerate()
            .filter(|(_, s)| {
                // First: cwd filter (unless show_all)
                if !self.show_all {
                    if let Some(cwd) = &self.current_cwd {
                        let matches_cwd = s.info.project.starts_with(cwd)
                            || s.info
                                .cwd
                                .as_ref()
                                .map(|c| c.starts_with(cwd))
                                .unwrap_or(false);
                        if !matches_cwd {
                            return false;
                        }
                    }
                }

                // Second: search filter
                if !self.filter_query.is_empty() {
                    let matches_query = s
                        .info
                        .summaries
                        .iter()
                        .any(|sum| sum.summary.to_lowercase().contains(&self.filter_query))
                        || s.info
                            .session_id
                            .to_lowercase()
                            .contains(&self.filter_query);
                    if !matches_query {
                        return false;
                    }
                }

                true
            })
            .collect()
    }

    /// Get currently selected session
    pub fn selected(&self) -> Option<&EnrichedSession> {
        let visible = self.visible_sessions();
        visible.get(self.cursor).map(|(_, s)| *s)
    }

    /// Move cursor down
    pub fn cursor_down(&mut self) {
        let count = self.visible_sessions().len();
        if count > 0 && self.cursor < count - 1 {
            self.cursor += 1;
        }
    }

    /// Move cursor up
    pub fn cursor_up(&mut self) {
        if self.cursor > 0 {
            self.cursor -= 1;
        }
    }

    /// Jump to top
    pub fn cursor_top(&mut self) {
        self.cursor = 0;
    }

    /// Jump to bottom
    pub fn cursor_bottom(&mut self) {
        let count = self.visible_sessions().len();
        if count > 0 {
            self.cursor = count - 1;
        }
    }

    /// Toggle show_all filter
    pub fn toggle_show_all(&mut self) {
        self.show_all = !self.show_all;
        // Reset cursor if out of bounds after filter change
        let count = self.visible_sessions().len();
        if self.cursor >= count {
            self.cursor = count.saturating_sub(1);
        }
    }

    /// Update filter query
    pub fn set_filter(&mut self, query: String) {
        self.filter_query = query.to_lowercase();
        // Reset cursor if out of bounds after filter change
        let count = self.visible_sessions().len();
        if self.cursor >= count {
            self.cursor = count.saturating_sub(1);
        }
    }
}
