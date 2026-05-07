//! Session List - Left panel state
//!
//! Manages the same cross-harness session surface that `ls-sessions` prints,
//! with pager-only cursor/search state layered on top.

use crate::agent_kind::AgentKind;
use crate::babel_storage::HookState;
use crate::session_row::{self, LiveSessionState, SessionRow, SessionRowInput};
use crate::ActivityState;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CwdDisplayMode {
    Relative,
    Absolute,
    Project,
    TouchedProjects,
}

impl CwdDisplayMode {
    pub fn next(self) -> Self {
        match self {
            Self::Relative => Self::Absolute,
            Self::Absolute => Self::Project,
            Self::Project => Self::TouchedProjects,
            Self::TouchedProjects => Self::Relative,
        }
    }

    pub fn previous(self) -> Self {
        match self {
            Self::Relative => Self::TouchedProjects,
            Self::Absolute => Self::Relative,
            Self::Project => Self::Absolute,
            Self::TouchedProjects => Self::Project,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Relative => "relative",
            Self::Absolute => "absolute",
            Self::Project => "project",
            Self::TouchedProjects => "touched projects",
        }
    }

    pub fn column_label(self) -> &'static str {
        match self {
            Self::Relative => "cwd",
            Self::Absolute => "abs",
            Self::Project => "proj",
            Self::TouchedProjects => "touch",
        }
    }
}

impl Default for CwdDisplayMode {
    fn default() -> Self {
        Self::Relative
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HiddenDisplayMode {
    Normal,
    Manual,
    All,
}

impl HiddenDisplayMode {
    pub fn next(self) -> Self {
        match self {
            Self::Normal => Self::Manual,
            Self::Manual => Self::All,
            Self::All => Self::Normal,
        }
    }

    pub fn previous(self) -> Self {
        match self {
            Self::Normal => Self::All,
            Self::Manual => Self::Normal,
            Self::All => Self::Manual,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Normal => "normal",
            Self::Manual => "manual hidden",
            Self::All => "all hidden",
        }
    }

    pub fn suffix(self) -> Option<&'static str> {
        match self {
            Self::Normal => None,
            Self::Manual => Some("+hidden"),
            Self::All => Some("+all-hidden"),
        }
    }
}

impl Default for HiddenDisplayMode {
    fn default() -> Self {
        Self::Normal
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SortDirection {
    Ascending,
    Descending,
}

impl SortDirection {
    pub fn toggle(self) -> Self {
        match self {
            Self::Ascending => Self::Descending,
            Self::Descending => Self::Ascending,
        }
    }

    pub fn indicator(self) -> &'static str {
        match self {
            Self::Ascending => "↑",
            Self::Descending => "↓",
        }
    }
}

impl Default for SortDirection {
    fn default() -> Self {
        Self::Descending
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SortColumn {
    State,
    Harness,
    Workspace,
    Cwd,
    Filter,
    CreatedTime,
    ModifiedTime,
    Turns,
    Thread,
    Prompt,
}

impl SortColumn {
    pub fn from_key(ch: char) -> Option<Self> {
        match ch {
            '1' => Some(Self::State),
            '2' => Some(Self::Harness),
            '3' => Some(Self::Workspace),
            '4' => Some(Self::Cwd),
            '5' => Some(Self::Filter),
            '6' => Some(Self::CreatedTime),
            '7' => Some(Self::ModifiedTime),
            '8' => Some(Self::Turns),
            '9' => Some(Self::Thread),
            '0' => Some(Self::Prompt),
            _ => None,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::State => "state",
            Self::Harness => "harness",
            Self::Workspace => "workspace",
            Self::Cwd => "cwd",
            Self::Filter => "filter",
            Self::CreatedTime => "ct",
            Self::ModifiedTime => "mt",
            Self::Turns => "turns",
            Self::Thread => "thread",
            Self::Prompt => "prompt",
        }
    }
}

impl Default for SortColumn {
    fn default() -> Self {
        Self::ModifiedTime
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectFilterTag {
    pub label: String,
    pub paths: Vec<PathBuf>,
    pub match_touched: bool,
}

impl ProjectFilterTag {
    pub fn new(label: impl Into<String>, paths: Vec<PathBuf>, match_touched: bool) -> Self {
        let mut paths = paths;
        paths.sort();
        paths.dedup();
        Self {
            label: label.into(),
            paths,
            match_touched,
        }
    }
}

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
                (Some(HookState::Working), ActivityState::Thinking) => "↯",
                (Some(HookState::Working), ActivityState::ToolUse) => "⚙",
                (Some(HookState::Working), ActivityState::PlanApproval) => "▣",
                (Some(HookState::Working), ActivityState::BackgroundTask) => "◐",
                (Some(HookState::Working), _) => "●",
                (None, ActivityState::Thinking) => "↯",
                (None, ActivityState::ToolUse) => "⚙",
                (None, ActivityState::PlanApproval) => "▣",
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
    pub created_at: i64,
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
    /// Pre-sanitize display strings so per-frame rendering skips
    /// the split_whitespace+join+truncate dance.
    pub fn pre_sanitize(&mut self) {
        const TEXT_MAX: usize = 160;
        if let Some(ref mut t) = self.generated_title {
            *t = session_row::sanitize_display(t, TEXT_MAX);
        }
        if let Some(ref mut t) = self.display_name {
            *t = session_row::sanitize_display(t, TEXT_MAX);
        }
        if let Some(ref mut t) = self.last_prompt {
            *t = session_row::sanitize_display(t, TEXT_MAX);
        }
    }

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

    /// Hidden by ls-sessions' default signal policy, even without user overlay.
    pub fn policy_hidden(&self) -> bool {
        !self.interactive || self.command_only || self.turn_count == 1
    }

    pub fn row(&self, now: i64) -> SessionRow {
        session_row::session_row(
            SessionRowInput {
                agent_kind: self.agent_kind,
                native_id: &self.native_id,
                project_path: self.project_path.as_deref(),
                display_name: self.display_name.as_deref(),
                generated_title: self.generated_title.as_deref(),
                last_prompt: self.last_prompt.as_deref(),
                turn_count: self.turn_count,
                created_at: self.created_at,
                last_seen_at: self.last_seen_at,
                interactive: self.interactive,
                command_only: self.command_only,
                has_title: self.has_title,
                hidden: self.hidden,
                live: match &self.running_status {
                    RunningStatus::Inactive => None,
                    RunningStatus::Active {
                        workspace,
                        hook_state,
                        activity_state,
                        ..
                    } => Some(LiveSessionState {
                        workspace: *workspace,
                        hook_state: *hook_state,
                        activity_state: Some(*activity_state),
                    }),
                },
                text_max_chars: 160,
                pre_sanitized: true,
            },
            now,
        )
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
    /// Which hidden session categories are included in the visible list.
    pub hidden_display_mode: HiddenDisplayMode,
    /// Current working directory for cwd filtering
    pub current_cwd: Option<PathBuf>,
    /// How the cwd filter is shown in the header.
    pub cwd_display_mode: CwdDisplayMode,
    /// Column currently sorting the visible session projection.
    pub sort_column: SortColumn,
    /// Direction currently sorting the visible session projection.
    pub sort_direction: SortDirection,
    /// Search/filter query
    pub filter_query: String,
    /// Structured project focus tag selected from the cwd column with `o`.
    pub project_filter: Option<ProjectFilterTag>,
    /// Cached touched-project roots, keyed by session key, so project focus can
    /// match the multi-project cwd column without re-parsing transcripts on the
    /// render path.
    touched_project_paths: HashMap<String, Vec<PathBuf>>,
    /// Cached visible row indices. The full catalog can be large because the
    /// TUI keeps low-signal sessions loaded for instant hidden-mode tabulation.
    visible_indices: Vec<usize>,
    visible_dirty: bool,
}

impl SessionListState {
    pub fn new(sessions: Vec<EnrichedSession>, current_cwd: Option<PathBuf>) -> Self {
        Self {
            sessions,
            cursor: 0,
            scroll_offset: 0,
            show_all: false,
            hidden_display_mode: HiddenDisplayMode::Normal,
            current_cwd,
            cwd_display_mode: CwdDisplayMode::Relative,
            sort_column: SortColumn::ModifiedTime,
            sort_direction: SortDirection::Descending,
            filter_query: String::new(),
            project_filter: None,
            touched_project_paths: HashMap::new(),
            visible_indices: Vec::new(),
            visible_dirty: true,
        }
    }

    /// Get visible session indices (hidden-filtered + cwd-filtered + search-filtered).
    pub fn visible_indices(&mut self) -> &[usize] {
        self.ensure_visible_indices();
        &self.visible_indices
    }

    pub fn visible_count(&mut self) -> usize {
        self.visible_indices().len()
    }

    /// Get currently selected session
    pub fn selected(&mut self) -> Option<&EnrichedSession> {
        let selected_index = self.selected_index()?;
        self.sessions.get(selected_index)
    }

    /// Toggle the selected session's hidden bit in memory.
    ///
    /// In hidden-display mode this flips the row between dimmed/regular. In
    /// normal mode, hiding the selected row removes it from the visible list.
    pub fn toggle_selected_hidden(&mut self) -> Option<(String, bool)> {
        let selected_index = self.selected_index()?;
        let session = self.sessions.get_mut(selected_index)?;
        session.hidden = !session.hidden;
        let session_key = session.session_key.clone();
        let hidden = session.hidden;
        self.invalidate_visible_indices();
        self.clamp_cursor();
        Some((session_key, hidden))
    }

    /// Set a session's hidden bit in memory by stable session key.
    pub fn set_hidden_by_key(&mut self, session_key: &str, hidden: bool) {
        if let Some(session) = self
            .sessions
            .iter_mut()
            .find(|session| session.session_key == session_key)
        {
            session.hidden = hidden;
            self.invalidate_visible_indices();
        }
        self.clamp_cursor();
    }

    pub fn cursor_down(&mut self) {
        self.cursor_jump(1);
    }

    pub fn cursor_up(&mut self) {
        self.cursor = self.cursor.saturating_sub(1);
    }

    pub fn cursor_jump(&mut self, n: usize) {
        let count = self.visible_count();
        if count > 0 {
            self.cursor = (self.cursor + n).min(count - 1);
        }
    }

    pub fn cursor_jump_back(&mut self, n: usize) {
        self.cursor = self.cursor.saturating_sub(n);
    }

    pub fn cursor_top(&mut self) {
        self.cursor = 0;
    }

    pub fn cursor_bottom(&mut self) {
        let count = self.visible_count();
        if count > 0 {
            self.cursor = count - 1;
        }
    }

    /// Toggle show_all filter
    pub fn toggle_show_all(&mut self) {
        self.show_all = !self.show_all;
        self.invalidate_visible_indices();
        self.clamp_cursor();
    }

    /// Toggle hidden session display.
    pub fn cycle_hidden_display_mode(&mut self) -> HiddenDisplayMode {
        self.hidden_display_mode = self.hidden_display_mode.next();
        self.invalidate_visible_indices();
        self.clamp_cursor();
        self.hidden_display_mode
    }

    pub fn cycle_hidden_display_mode_reverse(&mut self) -> HiddenDisplayMode {
        self.hidden_display_mode = self.hidden_display_mode.previous();
        self.invalidate_visible_indices();
        self.clamp_cursor();
        self.hidden_display_mode
    }

    pub fn cycle_cwd_display_mode(&mut self) -> CwdDisplayMode {
        self.cwd_display_mode = self.cwd_display_mode.next();
        self.cwd_display_mode
    }

    pub fn cycle_cwd_display_mode_reverse(&mut self) -> CwdDisplayMode {
        self.cwd_display_mode = self.cwd_display_mode.previous();
        self.cwd_display_mode
    }

    pub fn set_sort(&mut self, column: SortColumn, direction: SortDirection) {
        self.sort_column = column;
        self.sort_direction = direction;
        self.invalidate_visible_indices();
        self.clamp_cursor();
    }

    pub fn sort_by_column(&mut self, column: SortColumn) {
        if self.sort_column == column {
            self.sort_direction = self.sort_direction.toggle();
        } else {
            self.sort_column = column;
            self.sort_direction = SortDirection::Descending;
        }
        self.invalidate_visible_indices();
        self.clamp_cursor();
    }

    /// Update filter query
    pub fn set_filter(&mut self, query: String) {
        self.filter_query = query.to_lowercase();
        self.invalidate_visible_indices();
        // Reset cursor if out of bounds after filter change
        self.clamp_cursor();
    }

    pub fn set_project_filter(&mut self, filter: ProjectFilterTag) {
        self.project_filter = Some(filter);
        self.cursor = 0;
        self.scroll_offset = 0;
        self.invalidate_visible_indices();
        self.clamp_cursor();
    }

    pub fn clear_project_filter(&mut self) -> bool {
        if self.project_filter.take().is_some() {
            self.cursor = 0;
            self.scroll_offset = 0;
            self.invalidate_visible_indices();
            self.clamp_cursor();
            true
        } else {
            false
        }
    }

    pub fn project_filter_needs_touched_projects(&self) -> bool {
        self.project_filter
            .as_ref()
            .map(|filter| filter.match_touched)
            .unwrap_or(false)
    }

    pub fn set_touched_projects_for_session(&mut self, session_key: String, paths: Vec<PathBuf>) {
        self.touched_project_paths.insert(session_key, paths);
        if self.project_filter_needs_touched_projects() {
            self.invalidate_visible_indices();
            self.clamp_cursor();
        }
    }

    /// Replace session data while preserving the current semantic selection.
    pub fn replace_sessions(&mut self, sessions: Vec<EnrichedSession>) {
        let selected_key = self
            .selected_index()
            .and_then(|idx| self.sessions.get(idx))
            .map(|s| s.session_key.clone());
        self.sessions = sessions;
        self.invalidate_visible_indices();

        if let Some(selected_key) = selected_key {
            let visible = self.visible_indices().to_vec();
            if let Some(cursor) = visible
                .iter()
                .position(|idx| self.sessions[*idx].session_key == selected_key)
            {
                self.cursor = cursor;
            }
        }

        self.clamp_cursor();
    }

    fn clamp_cursor(&mut self) {
        let count = self.visible_count();
        if self.cursor >= count {
            self.cursor = count.saturating_sub(1);
        }
    }

    fn selected_index(&mut self) -> Option<usize> {
        let cursor = self.cursor;
        self.visible_indices().get(cursor).copied()
    }

    pub(crate) fn invalidate_visible_indices(&mut self) {
        self.visible_dirty = true;
    }

    fn ensure_visible_indices(&mut self) {
        if !self.visible_dirty {
            return;
        }

        self.visible_indices.clear();
        for (idx, session) in self.sessions.iter().enumerate() {
            if self.session_is_visible(session) {
                self.visible_indices.push(idx);
            }
        }
        let sessions = &self.sessions;
        let sort_column = self.sort_column;
        let sort_direction = self.sort_direction;
        self.visible_indices.sort_by(|left, right| {
            compare_sessions(
                &sessions[*left],
                &sessions[*right],
                sort_column,
                sort_direction,
            )
        });
        self.visible_dirty = false;
    }

    fn session_is_visible(&self, session: &EnrichedSession) -> bool {
        // Resume keeps the full native session catalog loaded so `h` can
        // tabulate from normal ls-sessions visibility, to manual overlay-hidden
        // rows, to every low-signal hidden category (subagents, command-only,
        // oneshots) without rescanning.
        match self.hidden_display_mode {
            HiddenDisplayMode::Normal if session.hidden || session.policy_hidden() => return false,
            HiddenDisplayMode::Manual if session.policy_hidden() => return false,
            HiddenDisplayMode::Normal | HiddenDisplayMode::Manual | HiddenDisplayMode::All => {}
        }

        if !self.show_all && self.project_filter.is_none() {
            if let Some(cwd) = &self.current_cwd {
                let matches_cwd = session
                    .project_path
                    .as_ref()
                    .map(|project| project.starts_with(cwd))
                    .unwrap_or(false);
                if !matches_cwd {
                    return false;
                }
            }
        }

        if let Some(filter) = &self.project_filter {
            if !self.session_matches_project_filter(session, filter) {
                return false;
            }
        }

        if !self.filter_query.is_empty() {
            return session.title().to_lowercase().contains(&self.filter_query)
                || session
                    .native_id
                    .to_lowercase()
                    .contains(&self.filter_query)
                || session
                    .session_key
                    .to_lowercase()
                    .contains(&self.filter_query)
                || session.agent_kind.slug().contains(&self.filter_query)
                || session
                    .last_prompt
                    .as_ref()
                    .map(|p| p.to_lowercase().contains(&self.filter_query))
                    .unwrap_or(false)
                || session
                    .project_path
                    .as_ref()
                    .map(|p| {
                        p.to_string_lossy()
                            .to_lowercase()
                            .contains(&self.filter_query)
                    })
                    .unwrap_or(false);
        }

        true
    }

    fn session_matches_project_filter(
        &self,
        session: &EnrichedSession,
        filter: &ProjectFilterTag,
    ) -> bool {
        if filter.paths.is_empty() {
            return true;
        }

        if session
            .project_path
            .as_deref()
            .map(|path| path_matches_any_filter_path(path, &filter.paths))
            .unwrap_or(false)
        {
            return true;
        }

        if !filter.match_touched {
            return false;
        }

        match self.touched_project_paths.get(&session.session_key) {
            Some(paths) => paths
                .iter()
                .any(|path| path_matches_any_filter_path(path, &filter.paths)),
            // A touched-project focus tag needs background metrics before the
            // list can know whether a row belongs. Keep unknown rows visible
            // long enough for queue_visible_project_metrics to hydrate them;
            // loaded misses collapse out as results arrive.
            None => true,
        }
    }
}

fn path_matches_any_filter_path(path: &Path, filter_paths: &[PathBuf]) -> bool {
    filter_paths
        .iter()
        .any(|filter| path.starts_with(filter) || filter.starts_with(path))
}

fn compare_sessions(
    left: &EnrichedSession,
    right: &EnrichedSession,
    column: SortColumn,
    direction: SortDirection,
) -> std::cmp::Ordering {
    let ordering = match column {
        SortColumn::State => running_rank(left).cmp(&running_rank(right)),
        SortColumn::Harness => left.agent_kind.slug().cmp(right.agent_kind.slug()),
        SortColumn::Workspace => workspace_rank(left).cmp(&workspace_rank(right)),
        SortColumn::Cwd => left.project_path.cmp(&right.project_path),
        SortColumn::Filter => left.filter_tag().cmp(right.filter_tag()),
        SortColumn::CreatedTime => left.created_at.cmp(&right.created_at),
        SortColumn::ModifiedTime => left.last_seen_at.cmp(&right.last_seen_at),
        SortColumn::Turns => left.turn_count.cmp(&right.turn_count),
        SortColumn::Thread => left.title().cmp(&right.title()),
        SortColumn::Prompt => left.last_prompt.cmp(&right.last_prompt),
    }
    .then_with(|| left.session_key.cmp(&right.session_key));

    match direction {
        SortDirection::Ascending => ordering,
        SortDirection::Descending => ordering.reverse(),
    }
}

fn running_rank(session: &EnrichedSession) -> u8 {
    match &session.running_status {
        RunningStatus::Active {
            focused: true,
            activity_state: ActivityState::AwaitingInput,
            ..
        } => 6,
        RunningStatus::Active { focused: true, .. } => 5,
        RunningStatus::Active {
            activity_state: ActivityState::AwaitingInput,
            ..
        } => 4,
        RunningStatus::Active {
            activity_state: ActivityState::Thinking | ActivityState::ToolUse,
            ..
        } => 3,
        RunningStatus::Active { .. } => 2,
        RunningStatus::Inactive => 1,
    }
}

fn workspace_rank(session: &EnrichedSession) -> Option<i32> {
    match &session.running_status {
        RunningStatus::Active { workspace, .. } => *workspace,
        RunningStatus::Inactive => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hidden_display_cycle_is_reversible() {
        assert_eq!(HiddenDisplayMode::Normal.next(), HiddenDisplayMode::Manual);
        assert_eq!(HiddenDisplayMode::Manual.next(), HiddenDisplayMode::All);
        assert_eq!(HiddenDisplayMode::All.next(), HiddenDisplayMode::Normal);
        assert_eq!(HiddenDisplayMode::Normal.previous(), HiddenDisplayMode::All);
        assert_eq!(HiddenDisplayMode::All.previous(), HiddenDisplayMode::Manual);
        assert_eq!(
            HiddenDisplayMode::Manual.previous(),
            HiddenDisplayMode::Normal
        );
    }
}
