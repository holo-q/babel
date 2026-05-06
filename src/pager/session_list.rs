//! Session List - Left panel state
//!
//! Manages the same cross-harness session surface that `ls-sessions` prints,
//! with pager-only cursor/search state layered on top.

use crate::agent_kind::AgentKind;
use crate::babel_storage::HookState;
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
        focused: bool,
        hook_state: Option<HookState>,
        activity_state: ActivityState,
    },
}

impl RunningStatus {
    /// Get status indicator character
    pub fn indicator(&self) -> &'static str {
        match self {
            RunningStatus::Inactive => " ",
            RunningStatus::Active {
                hook_state,
                activity_state,
                ..
            } => match (*hook_state, activity_state) {
                (Some(HookState::Idle), _) => "○",
                (Some(HookState::ToolRunning), _) => "⚙",
                (Some(HookState::Working), ActivityState::Thinking) => "⚡",
                (Some(HookState::Working), ActivityState::ToolUse) => "⚙",
                (Some(HookState::Working), ActivityState::PlanApproval) => "📋",
                (Some(HookState::Working), ActivityState::BackgroundTask) => "◐",
                (Some(HookState::Working), _) => "●",
                (None, ActivityState::Thinking) => "⚡",
                (None, ActivityState::ToolUse) => "⚙",
                (None, ActivityState::PlanApproval) => "📋",
                (None, ActivityState::AwaitingInput) => "◆",
                (None, ActivityState::BackgroundTask) => "◐",
                (None, ActivityState::Idle) => "○",
                (None, ActivityState::Unknown) => "◌",
            },
        }
    }

    /// Whether this session is running
    pub fn is_running(&self) -> bool {
        matches!(self, RunningStatus::Active { .. })
    }
}

/// Session with `ls-sessions` catalog fields plus running status enrichment.
#[derive(Debug, Clone)]
pub struct EnrichedSession {
    pub agent_kind: AgentKind,
    pub native_id: String,
    pub session_key: String,
    pub project_path: Option<PathBuf>,
    pub display_name: Option<String>,
    pub generated_title: Option<String>,
    pub last_prompt: Option<String>,
    pub turn_count: u32,
    pub last_seen_at: i64,
    pub interactive: bool,
    pub command_only: bool,
    pub has_title: bool,
    pub hidden: bool,
    pub custom_icon: Option<String>,
    pub unread: bool,
    pub running_status: RunningStatus,
}

impl EnrichedSession {
    pub fn title(&self) -> String {
        self.generated_title
            .as_ref()
            .or(self.display_name.as_ref())
            .cloned()
            .unwrap_or_else(|| self.native_id.clone())
    }

    pub fn has_real_title(&self) -> bool {
        self.generated_title.is_some() || self.has_title
    }

    pub fn filter_tag(&self) -> &'static str {
        if self.hidden {
            "▪"
        } else if !self.interactive {
            "⊞"
        } else if self.command_only {
            "/"
        } else if self.turn_count == 1 {
            "◇"
        } else {
            " "
        }
    }
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
                        let matches_cwd = s
                            .project_path
                            .as_ref()
                            .map(|project| project.starts_with(cwd))
                            .unwrap_or(false);
                        if !matches_cwd {
                            return false;
                        }
                    }
                }

                // Second: search filter
                if !self.filter_query.is_empty() {
                    let matches_query = s.title().to_lowercase().contains(&self.filter_query)
                        || s.native_id.to_lowercase().contains(&self.filter_query)
                        || s.session_key.to_lowercase().contains(&self.filter_query)
                        || s.agent_kind.slug().contains(&self.filter_query)
                        || s.last_prompt
                            .as_ref()
                            .map(|p| p.to_lowercase().contains(&self.filter_query))
                            .unwrap_or(false)
                        || s.project_path
                            .as_ref()
                            .map(|p| {
                                p.to_string_lossy()
                                    .to_lowercase()
                                    .contains(&self.filter_query)
                            })
                            .unwrap_or(false);
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
