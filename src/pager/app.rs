//! ResumeApp - Main pager state machine
//!
//! Coordinates session list and transcript view, handles key events.

use std::collections::HashMap;
use std::io;
use std::time::{Duration, Instant};

use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyModifiers};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use ratatui::backend::CrosstermBackend;
use ratatui::Terminal;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use vtr::{boundary, trace_error};

use crate::agent_kind::AgentKind;
use crate::events::BabelEvent;
use crate::ipc::{Request, Response};
use crate::utility::ipc::socket_path;

use super::demo::DemoMode;
use super::preferences::{
    load_resume_display_options, save_resume_display_options, ResumeDisplayOptions,
};
use super::project_metrics::ProjectTouchMetric;
use super::session_list::{
    CwdDisplayMode, EnrichedSession, ProjectFilterTag, SessionListState, SortColumn,
};
use super::transcript::TranscriptView;
use std::path::PathBuf;

/// Which panel has focus
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum PaneFocus {
    #[default]
    Sessions,
    Transcript,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SearchTarget {
    Sessions,
    Transcript,
}

impl SearchTarget {
    pub fn label(self) -> &'static str {
        match self {
            Self::Sessions => "list",
            Self::Transcript => "transcript",
        }
    }
}

#[derive(Debug, Clone)]
pub struct ResumeSelection {
    pub agent_kind: AgentKind,
    pub native_id: String,
    pub session_key: String,
    pub project_path: Option<PathBuf>,
}

pub enum ResumeAction {
    None,
    Quit,
    Launch {
        selection: ResumeSelection,
        keep_focus: bool,
    },
    YankIdentity(EnrichedSession),
    Refresh,
    SetHidden {
        session_key: String,
        hidden: bool,
    },
}

enum TranscriptLoadResult {
    Loaded {
        seq: u64,
        session_id: String,
        messages: Vec<scrollparse::Message>,
    },
    Notice {
        seq: u64,
        session_id: String,
        message: String,
    },
}

enum ProjectLoadResult {
    Loaded {
        session_key: String,
        projects: Vec<ProjectTouchMetric>,
    },
    Notice {
        session_key: String,
        message: String,
    },
}

enum LaunchResult {
    Success(String),
    Error(String),
}

struct PendingTranscriptLoad {
    seq: u64,
    agent_kind: AgentKind,
    session_id: String,
    requested_at: Instant,
}

struct TranscriptTarget {
    agent_kind: AgentKind,
    native_id: String,
    session_key: String,
}

#[derive(Debug, Clone)]
pub enum TouchedProjectsState {
    Empty,
    Loading,
    Loaded(Vec<ProjectTouchMetric>),
    Notice(String),
}

#[async_trait::async_trait]
pub trait ResumeSessionSource {
    async fn refresh_sessions(&mut self, force: bool) -> anyhow::Result<Vec<EnrichedSession>>;
    async fn launch_resume(&mut self, selection: &ResumeSelection) -> anyhow::Result<String>;
    async fn set_hidden(&mut self, session_key: &str, hidden: bool) -> anyhow::Result<()>;

    fn auto_refresh_enabled(&self) -> bool {
        false
    }
}

/// Main pager application state
pub struct ResumeApp {
    /// Session list state
    pub sessions: SessionListState,
    /// Transcript preview state
    pub transcript: TranscriptView,
    /// Which pane has focus
    pub focus: PaneFocus,
    /// Which independent search surface is currently being edited.
    pub active_search: Option<SearchTarget>,
    /// Search query buffer for the active search target.
    pub search_buffer: String,
    /// Whether the transcript preview pane is visible
    pub show_transcript: bool,
    /// Whether list text cells use middle snip markers instead of edge clipping.
    pub snip_columns: bool,
    /// Whether the turn count column is rendered as compact braille.
    pub braille_turns: bool,
    /// Last launcher/refresh status shown in the footer
    pub status_message: String,
    /// Cached/async per-session project-touch metric for cwd-column rendering.
    pub touched_projects: HashMap<String, TouchedProjectsState>,
    /// Display-only anonymizer used for screenshots/demos.
    pub demo: Option<DemoMode>,
    /// Display preferences changed and should be persisted after the key event.
    display_options_dirty: bool,
    /// Should exit
    pub should_exit: bool,
}

impl ResumeApp {
    pub fn new(sessions: Vec<EnrichedSession>, current_cwd: Option<PathBuf>) -> Self {
        Self::new_with_demo(sessions, current_cwd, None)
    }

    pub fn new_with_demo(
        sessions: Vec<EnrichedSession>,
        current_cwd: Option<PathBuf>,
        demo: Option<DemoMode>,
    ) -> Self {
        Self {
            sessions: SessionListState::new(sessions, current_cwd),
            transcript: TranscriptView::new(),
            focus: PaneFocus::Sessions,
            active_search: None,
            search_buffer: String::new(),
            show_transcript: true,
            snip_columns: true,
            braille_turns: false,
            status_message: "Enter: launch  r: refresh".to_string(),
            touched_projects: HashMap::new(),
            demo,
            display_options_dirty: false,
            should_exit: false,
        }
    }

    pub fn apply_display_options(&mut self, options: ResumeDisplayOptions) {
        self.sessions.show_all = options.show_all;
        self.sessions.hidden_display_mode = options.hidden_display_mode;
        self.sessions.cwd_display_mode = options.cwd_display_mode;
        self.sessions
            .set_sort(options.sort_column, options.sort_direction);
        self.sessions.group_mode = options.group_mode;
        self.sessions.invalidate_visible_indices();
        self.show_transcript = options.show_transcript;
        self.snip_columns = options.snip_columns;
        self.braille_turns = options.braille_turns;
        self.transcript.body_mode = if options.transcript_body_mode.expands_messages() {
            options.transcript_body_mode
        } else {
            crate::pager::TranscriptBodyMode::from_expand_messages(options.expand_messages)
        };
        self.transcript.role_filter = options.transcript_role_filter;
        if !self.show_transcript && self.focus == PaneFocus::Transcript {
            self.focus = PaneFocus::Sessions;
        }
    }

    fn display_options(&self) -> ResumeDisplayOptions {
        ResumeDisplayOptions {
            show_all: self.sessions.show_all,
            hidden_display_mode: self.sessions.hidden_display_mode,
            cwd_display_mode: self.sessions.cwd_display_mode,
            sort_column: self.sessions.sort_column,
            sort_direction: self.sessions.sort_direction,
            group_mode: self.sessions.group_mode,
            show_transcript: self.show_transcript,
            snip_columns: self.snip_columns,
            braille_turns: self.braille_turns,
            transcript_body_mode: self.transcript.body_mode,
            expand_messages: self.transcript.body_mode.expands_messages(),
            transcript_role_filter: self.transcript.role_filter,
        }
    }

    fn mark_display_options_dirty(&mut self) {
        self.display_options_dirty = true;
    }

    fn take_display_options_dirty(&mut self) -> bool {
        let dirty = self.display_options_dirty;
        self.display_options_dirty = false;
        dirty
    }

    /// Handle key event and return any launcher-level action it requested.
    pub fn handle_key(&mut self, key: KeyEvent) -> ResumeAction {
        // Ctrl+C always exits
        if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
            self.should_exit = true;
            return ResumeAction::Quit;
        }

        // Handle search mode
        if self.active_search.is_some() {
            self.handle_search_key(key);
            return ResumeAction::None;
        }

        match key.code {
            // Exit
            KeyCode::Char('q') | KeyCode::Esc => {
                self.should_exit = true;
                ResumeAction::Quit
            }

            // Navigation
            KeyCode::Char('j') | KeyCode::Down => {
                if self.focus == PaneFocus::Sessions {
                    self.sessions.cursor_down();
                } else {
                    self.transcript.scroll_down(1);
                }
                ResumeAction::None
            }
            KeyCode::Char('k') | KeyCode::Up => {
                if self.focus == PaneFocus::Sessions {
                    self.sessions.cursor_up();
                } else {
                    self.transcript.scroll_up(1);
                }
                ResumeAction::None
            }
            KeyCode::Char('g') => {
                if self.focus == PaneFocus::Sessions {
                    self.sessions.cursor_top();
                } else {
                    self.transcript.scroll_top();
                }
                ResumeAction::None
            }
            KeyCode::Char('G') => {
                if self.focus == PaneFocus::Sessions {
                    self.sessions.cursor_bottom();
                } else {
                    self.transcript.scroll_bottom();
                }
                ResumeAction::None
            }

            // Page navigation
            KeyCode::PageDown => {
                if self.focus == PaneFocus::Sessions {
                    self.sessions.cursor_jump(10);
                } else {
                    self.transcript.scroll_down(10);
                }
                ResumeAction::None
            }
            KeyCode::PageUp => {
                if self.focus == PaneFocus::Sessions {
                    self.sessions.cursor_jump_back(10);
                } else {
                    self.transcript.scroll_up(10);
                }
                ResumeAction::None
            }

            // Tab - toggle cwd/all
            KeyCode::Tab => {
                self.sessions.toggle_show_all();
                self.mark_display_options_dirty();
                ResumeAction::None
            }

            // Mark/unmark selected session as hidden. This is intentionally a
            // plain toggle separate from `h`, which only changes visibility
            // modes; hidden-row mutation should not depend on shift chords.
            KeyCode::Char('x') if !key.modifiers.contains(KeyModifiers::ALT) => {
                let Some((session_key, hidden)) = self.sessions.toggle_selected_hidden() else {
                    return ResumeAction::None;
                };
                ResumeAction::SetHidden {
                    session_key,
                    hidden,
                }
            }

            // Toggle hidden sessions
            KeyCode::Char('h') if !key.modifiers.contains(KeyModifiers::ALT) => {
                let mode = self.sessions.cycle_hidden_display_mode();
                self.status_message = format!("hidden display: {}", mode.label());
                self.mark_display_options_dirty();
                ResumeAction::None
            }

            // Reverse hidden display cycle. Kept as an unadvertised escape
            // hatch; the footer shows the lowercase forward verb only.
            KeyCode::Char('h' | 'H') if key.modifiers.contains(KeyModifiers::ALT) => {
                let mode = self.sessions.cycle_hidden_display_mode_reverse();
                self.status_message = format!("hidden display: {}", mode.label());
                self.mark_display_options_dirty();
                ResumeAction::None
            }

            // Cycle cwd display label: relative -> absolute -> project name -> touched projects.
            KeyCode::Char('c') => {
                let mode = self.sessions.cycle_cwd_display_mode();
                self.status_message = format!("cwd display: {}", mode.label());
                self.mark_display_options_dirty();
                ResumeAction::None
            }

            // Reverse cwd display cycle.
            KeyCode::Char('C') => {
                let mode = self.sessions.cycle_cwd_display_mode_reverse();
                self.status_message = format!("cwd display: {}", mode.label());
                self.mark_display_options_dirty();
                ResumeAction::None
            }

            // Toggle transcript preview
            KeyCode::Char('t') => {
                self.show_transcript = !self.show_transcript;
                if !self.show_transcript && self.focus == PaneFocus::Transcript {
                    self.focus = PaneFocus::Sessions;
                }
                self.mark_display_options_dirty();
                ResumeAction::None
            }

            // Contextual snip toggle: list focus controls column middle-snips;
            // transcript focus controls expanded conversation bodies. Tool rows
            // remain clamped regardless so command output never floods the pane.
            KeyCode::Char('s' | 'S') => {
                if self.focus == PaneFocus::Transcript {
                    let mode = if key.code == KeyCode::Char('S') {
                        self.transcript.cycle_body_mode_reverse()
                    } else {
                        self.transcript.cycle_body_mode()
                    };
                    self.status_message = format!("transcript messages: {}", mode.label());
                } else {
                    self.snip_columns = !self.snip_columns;
                    self.status_message = if self.snip_columns {
                        "columns: snip".to_string()
                    } else {
                        "columns: clip".to_string()
                    };
                }
                self.mark_display_options_dirty();
                ResumeAction::None
            }

            // Toggle transcript role filter.
            KeyCode::Char('u') => {
                let mode = self.transcript.toggle_role_filter();
                self.status_message = format!("transcript filter: {}", mode.label());
                self.mark_display_options_dirty();
                ResumeAction::None
            }

            // Reverse transcript role filter.
            KeyCode::Char('U') => {
                let mode = self.transcript.toggle_role_filter_reverse();
                self.status_message = format!("transcript filter: {}", mode.label());
                self.mark_display_options_dirty();
                ResumeAction::None
            }

            // Toggle compact braille turn-density column.
            KeyCode::Char('b') => {
                self.braille_turns = !self.braille_turns;
                self.status_message = if self.braille_turns {
                    "turns: braille".to_string()
                } else {
                    "turns: raw".to_string()
                };
                self.mark_display_options_dirty();
                ResumeAction::None
            }

            // Cycle day grouping: none -> created day -> modified day. The
            // session cursor remains a session cursor; group headers are a
            // render projection so actions never land on a header row.
            KeyCode::Char('d') => {
                let mode = self.sessions.cycle_group_mode();
                self.status_message = format!("group: {}", mode.label());
                self.mark_display_options_dirty();
                ResumeAction::None
            }
            KeyCode::Char('D') => {
                let mode = self.sessions.cycle_group_mode_reverse();
                self.status_message = format!("group: {}", mode.label());
                self.mark_display_options_dirty();
                ResumeAction::None
            }

            // Force refresh
            KeyCode::Char('r') => {
                self.status_message = "refreshing sessions...".to_string();
                ResumeAction::Refresh
            }

            // Focus switching
            KeyCode::Char('l') | KeyCode::Right => {
                if self.show_transcript {
                    self.focus = PaneFocus::Transcript;
                }
                ResumeAction::None
            }
            KeyCode::Left => {
                self.focus = PaneFocus::Sessions;
                ResumeAction::None
            }

            KeyCode::Char('/') => {
                self.begin_contextual_search();
                ResumeAction::None
            }

            // Focus the list to the selected cwd column scope. In touched mode
            // the selected row can represent several projects, so the tag
            // becomes a multi-project filter once the metrics are loaded.
            KeyCode::Char('o') => {
                match self.project_filter_from_selection() {
                    Ok(filter)
                        if self
                            .sessions
                            .project_filter
                            .as_ref()
                            .map(|current| current == &filter)
                            .unwrap_or(false) =>
                    {
                        self.sessions.clear_project_filter();
                        self.status_message = "project filter cleared".to_string();
                    }
                    Ok(filter) => {
                        let label = filter.label.clone();
                        self.sessions.set_project_filter(filter);
                        self.status_message = format!("project filter: {label}");
                    }
                    Err(message) => {
                        self.status_message = message;
                    }
                }
                ResumeAction::None
            }

            // Resume selected session
            KeyCode::Enter => {
                if let Some(session) = self.sessions.selected() {
                    return ResumeAction::Launch {
                        selection: ResumeSelection {
                            agent_kind: session.agent_kind,
                            native_id: session.native_id.clone(),
                            session_key: session.session_key.clone(),
                            project_path: session.project_path.clone(),
                        },
                        keep_focus: key.modifiers.contains(KeyModifiers::SHIFT),
                    };
                }
                ResumeAction::None
            }

            // Yank full disk/native identity for debugging the selected row.
            KeyCode::Char('y') => self
                .sessions
                .selected()
                .cloned()
                .map(ResumeAction::YankIdentity)
                .unwrap_or(ResumeAction::None),

            // Sort session list by visible column index. `#` is a generated
            // row number, not source data; `0` covers the tenth data column.
            // 1 state, 2 harness, 3 workspace, 4 cwd, 5 filter, 6 ct,
            // 7 mt, 8 turns, 9 thread, 0 prompt.
            KeyCode::Char(ch) if self.focus == PaneFocus::Sessions => {
                let Some(column) = SortColumn::from_key(ch) else {
                    return ResumeAction::None;
                };
                self.sessions.sort_by_column(column);
                self.status_message = format!(
                    "sort: {} {}",
                    self.sessions.sort_column.label(),
                    self.sessions.sort_direction.indicator()
                );
                self.mark_display_options_dirty();
                ResumeAction::None
            }

            _ => ResumeAction::None,
        }
    }

    fn handle_search_key(&mut self, key: KeyEvent) {
        let Some(target) = self.active_search else {
            return;
        };
        match key.code {
            KeyCode::Enter | KeyCode::Esc => {
                self.active_search = None;
                if key.code == KeyCode::Esc {
                    self.search_buffer.clear();
                    self.clear_search_target(target);
                }
            }
            KeyCode::Char('w') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                delete_previous_search_word(&mut self.search_buffer);
                self.apply_search_buffer_to_target(target);
            }
            KeyCode::Backspace if key.modifiers.contains(KeyModifiers::CONTROL) => {
                delete_previous_search_word(&mut self.search_buffer);
                self.apply_search_buffer_to_target(target);
            }
            KeyCode::Backspace => {
                self.search_buffer.pop();
                self.apply_search_buffer_to_target(target);
            }
            KeyCode::Char(c) => {
                self.search_buffer.push(c);
                self.apply_search_buffer_to_target(target);
            }
            _ => {}
        }
    }

    pub fn is_searching(&self) -> bool {
        self.active_search.is_some()
    }

    fn begin_contextual_search(&mut self) {
        let target = if self.focus == PaneFocus::Transcript && self.show_transcript {
            SearchTarget::Transcript
        } else {
            SearchTarget::Sessions
        };
        self.search_buffer = match target {
            SearchTarget::Sessions => self.sessions.filter_query.clone(),
            SearchTarget::Transcript => self.transcript.search_query.clone(),
        };
        self.active_search = Some(target);
        self.status_message = format!("{} search", target.label());
    }

    fn apply_search_buffer_to_target(&mut self, target: SearchTarget) {
        match target {
            SearchTarget::Sessions => self.sessions.set_filter(self.search_buffer.clone()),
            SearchTarget::Transcript => self
                .transcript
                .set_search_filter(self.search_buffer.clone()),
        }
    }

    fn clear_search_target(&mut self, target: SearchTarget) {
        match target {
            SearchTarget::Sessions => self.sessions.set_filter(String::new()),
            SearchTarget::Transcript => self.transcript.clear_search_filter(),
        }
    }

    fn selected_transcript_target(&mut self) -> Option<TranscriptTarget> {
        self.sessions.selected().map(|session| TranscriptTarget {
            agent_kind: session.agent_kind,
            native_id: session.native_id.clone(),
            session_key: session.session_key.clone(),
        })
    }

    fn project_filter_from_selection(&mut self) -> Result<ProjectFilterTag, String> {
        let mode = self.sessions.cwd_display_mode;
        let Some(session) = self.sessions.selected().cloned() else {
            return Err("no selected session".to_string());
        };

        if mode == CwdDisplayMode::TouchedProjects {
            match self.touched_projects.get(&session.session_key) {
                Some(TouchedProjectsState::Loaded(projects)) if !projects.is_empty() => {
                    let paths: Vec<PathBuf> = projects
                        .iter()
                        .map(|project| project.path.clone())
                        .collect();
                    return Ok(ProjectFilterTag::new(
                        format!("touch:{}", project_filter_leafs(&paths)),
                        paths,
                        true,
                    ));
                }
                Some(TouchedProjectsState::Loaded(_)) | Some(TouchedProjectsState::Empty) => {}
                Some(TouchedProjectsState::Loading) | None => {
                    return Err("touched projects loading; press o again".to_string());
                }
                Some(TouchedProjectsState::Notice(message)) => {
                    return Err(format!("touched projects unavailable: {message}"));
                }
            }
        }

        let Some(path) = session.project_path else {
            return Err("selected session has no project path".to_string());
        };
        Ok(ProjectFilterTag::new(
            project_filter_label(&path, mode),
            vec![path],
            false,
        ))
    }
}

fn project_filter_label(path: &PathBuf, mode: CwdDisplayMode) -> String {
    match mode {
        CwdDisplayMode::Absolute => format!("abs:{}", path.display()),
        CwdDisplayMode::Project | CwdDisplayMode::TouchedProjects => {
            format!("proj:{}", project_leaf(path))
        }
        CwdDisplayMode::Relative => format!("cwd:{}", relative_project_path(path)),
    }
}

fn project_filter_leafs(paths: &[PathBuf]) -> String {
    const MAX_LEAFS: usize = 4;
    let mut leafs: Vec<String> = paths.iter().take(MAX_LEAFS).map(project_leaf).collect();
    if paths.len() > MAX_LEAFS {
        leafs.push("…".to_string());
    }
    leafs.join(",")
}

fn project_leaf(path: &PathBuf) -> String {
    path.file_name()
        .and_then(|name| name.to_str())
        .map(str::to_string)
        .unwrap_or_else(|| path.display().to_string())
}

fn relative_project_path(path: &PathBuf) -> String {
    dirs::home_dir()
        .and_then(|home| {
            path.strip_prefix(home)
                .ok()
                .map(|relative| relative.display().to_string())
        })
        .filter(|label| !label.is_empty())
        .unwrap_or_else(|| path.display().to_string())
}

fn delete_previous_search_word(buffer: &mut String) {
    while buffer
        .chars()
        .last()
        .map(char::is_whitespace)
        .unwrap_or(false)
    {
        buffer.pop();
    }

    while buffer
        .chars()
        .last()
        .map(|ch| !ch.is_whitespace())
        .unwrap_or(false)
    {
        buffer.pop();
    }
}

/// Run the resume pager TUI
pub async fn run_resume_pager<S>(
    source: &mut S,
    show_all: bool,
    sessions: Vec<EnrichedSession>,
    demo: Option<DemoMode>,
) -> anyhow::Result<()>
where
    S: ResumeSessionSource,
{
    // Get current working directory for cwd filtering
    let current_cwd = std::env::current_dir().ok();

    if sessions.is_empty() {
        println!("No sessions found");
        return Ok(());
    }

    // Setup terminal
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    // Create app with current_cwd for filtering
    let mut app = ResumeApp::new_with_demo(sessions, current_cwd, demo);
    let mut display_options = load_resume_display_options();
    // `--all` is an explicit launch-time request. The saved preference still
    // seeds ordinary runs, while this flag can force the broader list open.
    if show_all {
        display_options.show_all = true;
    }
    app.apply_display_options(display_options);

    let (transcript_tx, mut transcript_rx) = mpsc::channel(8);
    let (project_tx, mut project_rx) = mpsc::channel(8);
    let mut transcript_seq = 0;
    let mut desired_transcript_key: Option<String> = None;
    let mut pending_transcript: Option<PendingTranscriptLoad> = None;
    let mut active_transcript_load: Option<JoinHandle<()>> = None;
    // `babel resume` is a launcher, so list navigation has to stay hot even
    // while transcript JSONL files are large. Selection changes only schedule a
    // debounced background parse; stale workers lose by sequence number.
    sync_selected_transcript_target(
        &mut app,
        &mut desired_transcript_key,
        &mut pending_transcript,
        &mut transcript_seq,
    );

    let mut daemon_events = if source.auto_refresh_enabled() {
        Some(spawn_daemon_refresh_listener())
    } else {
        None
    };
    let mut auto_refresh_pending = false;
    let mut last_auto_refresh = Instant::now() - Duration::from_secs(1);

    let (launch_tx, mut launch_rx) = mpsc::channel::<LaunchResult>(4);
    let refocus_ctx = detect_refocus_context();

    let mut needs_redraw = true;

    // Event loop — dirty-driven: only draw when state actually changed.
    // Input events are drained in batch before drawing so held-key scrolling
    // never renders intermediate frames.
    loop {
        if needs_redraw {
            queue_visible_project_metrics(&mut app, &project_tx);
            terminal.draw(|f| super::ui::draw(f, &mut app))?;
            needs_redraw = false;
        }

        if event::poll(std::time::Duration::from_millis(100))? {
            loop {
                match event::read()? {
                    Event::Key(key) if key.kind != event::KeyEventKind::Release => {
                        match app.handle_key(key) {
                            ResumeAction::None => {}
                            ResumeAction::Quit => {}
                            ResumeAction::Launch {
                                selection,
                                keep_focus,
                            } => {
                                let short_id: String =
                                    selection.native_id.chars().take(8).collect();
                                app.status_message = format!(
                                    "launching {} {short_id}...",
                                    selection.agent_kind.slug(),
                                );
                                let tx = launch_tx.clone();
                                let refocus = keep_focus.then(|| refocus_ctx.clone()).flatten();
                                tokio::spawn(async move {
                                    let msg = match launch_harness_resume(&selection).await {
                                        Ok(msg) => {
                                            if let Some(ctx) = refocus {
                                                tokio::time::sleep(Duration::from_millis(150))
                                                    .await;
                                                let _ = ctx
                                                    .backend
                                                    .focus_pane(&ctx.conn, ctx.pane_id)
                                                    .await;
                                            }
                                            LaunchResult::Success(msg)
                                        }
                                        Err(e) => {
                                            LaunchResult::Error(format!("launch failed: {e}"))
                                        }
                                    };
                                    let _ = tx.send(msg).await;
                                });
                            }
                            ResumeAction::YankIdentity(session) => {
                                match super::identity::session_identity_json(&session).and_then(
                                    |text| {
                                        super::identity::copy_to_clipboard(&text)
                                            .map(|target| (target, text.len()))
                                    },
                                ) {
                                    Ok((target, bytes)) => {
                                        app.status_message =
                                            format!("yanked identity ({bytes} bytes via {target})");
                                    }
                                    Err(e) => {
                                        app.status_message = format!("yank failed: {e}");
                                    }
                                }
                            }
                            ResumeAction::Refresh => {
                                refresh_app_sessions(&mut app, source, true, "refreshed").await?;
                                sync_selected_transcript_target(
                                    &mut app,
                                    &mut desired_transcript_key,
                                    &mut pending_transcript,
                                    &mut transcript_seq,
                                );
                            }
                            ResumeAction::SetHidden {
                                session_key,
                                hidden,
                            } => match source.set_hidden(&session_key, hidden).await {
                                Ok(()) => {
                                    app.status_message = if hidden {
                                        "hid session".to_string()
                                    } else {
                                        "unhid session".to_string()
                                    };
                                }
                                Err(e) => {
                                    app.sessions.set_hidden_by_key(&session_key, !hidden);
                                    app.status_message = format!("hide failed: {e}");
                                }
                            },
                        }
                        persist_display_options_if_dirty(&mut app);
                        sync_selected_transcript_target(
                            &mut app,
                            &mut desired_transcript_key,
                            &mut pending_transcript,
                            &mut transcript_seq,
                        );
                        needs_redraw = true;
                    }
                    Event::Resize(..) => {
                        needs_redraw = true;
                    }
                    _ => {}
                }

                if app.should_exit || !event::poll(std::time::Duration::ZERO)? {
                    break;
                }
            }
        }

        while let Ok(result) = transcript_rx.try_recv() {
            apply_transcript_result(&mut app, result, transcript_seq);
            needs_redraw = true;
        }

        while let Ok(result) = project_rx.try_recv() {
            apply_project_result(&mut app, result);
            needs_redraw = true;
        }

        while let Ok(result) = launch_rx.try_recv() {
            match result {
                LaunchResult::Success(msg) => app.status_message = msg,
                LaunchResult::Error(msg) => app.status_message = msg,
            }
            needs_redraw = true;
        }

        if let Some(pending) = pending_transcript.take() {
            if pending.requested_at.elapsed() >= Duration::from_millis(120) {
                if let Some(handle) = active_transcript_load.take() {
                    handle.abort();
                }
                active_transcript_load = Some(spawn_transcript_load(
                    pending.seq,
                    pending.agent_kind,
                    pending.session_id,
                    transcript_tx.clone(),
                ));
            } else {
                pending_transcript = Some(pending);
            }
        }

        if let Some(rx) = daemon_events.as_mut() {
            while rx.try_recv().is_ok() {
                auto_refresh_pending = true;
            }
        }

        if auto_refresh_pending && last_auto_refresh.elapsed() >= Duration::from_millis(250) {
            auto_refresh_pending = false;
            last_auto_refresh = Instant::now();
            refresh_app_sessions(&mut app, source, false, "auto-refreshed").await?;
            sync_selected_transcript_target(
                &mut app,
                &mut desired_transcript_key,
                &mut pending_transcript,
                &mut transcript_seq,
            );
            needs_redraw = true;
        }

        if app.should_exit {
            break;
        }
    }

    // Cleanup terminal
    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    terminal.show_cursor()?;

    Ok(())
}

/// Launch a session resume in the current terminal backend.
///
/// Resolves the harness resume command, determines the working directory
/// (with Claude-specific fallback to ~/.claude/projects/), spawns a new
/// pane via the backend trait, and restores the session's last-known
/// desktop workspace when available.
pub async fn launch_harness_resume(selection: &ResumeSelection) -> anyhow::Result<String> {
    let cwd = selection
        .project_path
        .as_ref()
        .filter(|p| p.exists())
        .cloned()
        .or_else(|| {
            if selection.agent_kind == crate::AgentKind::Claude {
                crate::utility::claude_storage::get_session_cwd(&selection.native_id).ok()
            } else {
                None
            }
        })
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));

    let parts = crate::harness::resume_command_parts(
        selection.agent_kind,
        &selection.native_id,
        Some(&cwd),
    )
    .ok_or_else(|| {
        anyhow::anyhow!(
            "{} has no resume command",
            selection.agent_kind.display_name()
        )
    })?;
    if parts.is_empty() {
        return Err(anyhow::anyhow!(
            "Empty resume command for {}",
            selection.agent_kind.display_name()
        ));
    }
    boundary!(
        "harness",
        "resume",
        agent = selection.agent_kind.slug(),
        native_id = selection.native_id.as_str(),
        cwd = format!("{:?}", cwd),
        cmd = parts.join(" ")
    );

    let launched = launch_command(&parts, &cwd).await?;

    if let Some(platform_window_id) = launched.platform_window_id {
        let target_workspace = crate::babel_storage::init_db()
            .ok()
            .and_then(|db| {
                crate::babel_storage::get_metadata(&db, &selection.session_key)
                    .ok()
                    .flatten()
            })
            .and_then(|m| m.last_workspace);
        if let Some(workspace) = target_workspace {
            if let Err(e) = crate::desktop::move_window_to_workspace(platform_window_id, workspace)
            {
                tracing::debug!(
                    error = %e,
                    workspace,
                    "workspace move failed for resumed session"
                );
            }
        }
    }

    let short_id: String = selection.native_id.chars().take(8).collect();
    Ok(format!(
        "launched {} {}",
        selection.agent_kind.slug(),
        short_id
    ))
}

/// Launch a command in a new terminal pane.
///
/// Checks `[launch].command` in babel.toml first. If set, expands the template
/// and runs it as a shell command. Otherwise falls back to the detected backend's
/// native `launch_pane()`.
///
/// Template placeholders:
/// - `{cmd}` — full command string (e.g., "claude --resume abc123")
/// - `{cwd}` — working directory path
/// - `{args}` — arguments only (without the binary name)
async fn launch_command(
    parts: &[String],
    cwd: &std::path::Path,
) -> anyhow::Result<crate::backend::LaunchedPane> {
    let config = crate::config::load_config().unwrap_or_default();

    if let Some(template) = &config.launch.command {
        let cmd_str = parts.join(" ");
        let args_str = parts[1..].join(" ");
        let cwd_str = cwd.to_string_lossy();

        let expanded = template
            .replace("{cmd}", &cmd_str)
            .replace("{cwd}", &cwd_str)
            .replace("{args}", &args_str);

        vtr::boundary!("launch", "custom", cmd = expanded.as_str());

        let output = tokio::process::Command::new("sh")
            .args(["-c", &expanded])
            .output()
            .await
            .map_err(|e| anyhow::anyhow!("custom launch command failed: {e}"))?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            anyhow::bail!("launch command failed: {stderr}");
        }

        Ok(crate::backend::LaunchedPane {
            pane_id: 0,
            platform_window_id: None,
        })
    } else {
        let part_refs: Vec<&str> = parts.iter().map(String::as_str).collect();
        let (backend, conn) = crate::backend::detect_current_backend()?;
        backend.launch_pane(&conn, &part_refs, cwd).await
    }
}

fn persist_display_options_if_dirty(app: &mut ResumeApp) {
    if !app.take_display_options_dirty() {
        return;
    }

    if let Err(e) = save_resume_display_options(&app.display_options()) {
        trace_error!("resume display options save failed", error = %e);
        app.status_message = format!("display options save failed: {e}");
    }
}

fn sync_selected_transcript_target(
    app: &mut ResumeApp,
    desired_key: &mut Option<String>,
    pending: &mut Option<PendingTranscriptLoad>,
    seq: &mut u64,
) {
    let Some(target) = app.selected_transcript_target() else {
        if desired_key.is_some() {
            app.transcript.clear();
            *desired_key = None;
            *pending = None;
        }
        return;
    };

    if desired_key.as_deref() == Some(&target.session_key) {
        return;
    }

    *seq = seq.saturating_add(1);
    *desired_key = Some(target.session_key.clone());
    *pending = None;

    if !matches!(target.agent_kind, AgentKind::Claude | AgentKind::Codex) {
        app.transcript.notice(
            target.native_id,
            format!(
                "{} transcript preview is not wired yet",
                target.agent_kind.display_name()
            ),
        );
        return;
    }

    app.transcript.notice(
        target.native_id.clone(),
        "Loading transcript...".to_string(),
    );
    *pending = Some(PendingTranscriptLoad {
        seq: *seq,
        agent_kind: target.agent_kind,
        session_id: target.native_id,
        requested_at: Instant::now(),
    });
}

fn spawn_project_load(
    agent_kind: AgentKind,
    native_id: String,
    session_key: String,
    tx: mpsc::Sender<ProjectLoadResult>,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let worker_native_id = native_id.clone();
        let worker_session_key = session_key.clone();
        let result = tokio::task::spawn_blocking(move || {
            match super::project_metrics::load_cached_session_projects(
                agent_kind,
                &worker_native_id,
                &worker_session_key,
            ) {
                Ok(projects) => ProjectLoadResult::Loaded {
                    session_key: worker_session_key,
                    projects,
                },
                Err(e) => ProjectLoadResult::Notice {
                    session_key: worker_session_key,
                    message: format!("project metrics unavailable: {e}"),
                },
            }
        })
        .await
        .unwrap_or_else(|e| ProjectLoadResult::Notice {
            session_key,
            message: format!("project metric worker failed: {e}"),
        });
        let _ = tx.send(result).await;
    })
}

fn spawn_transcript_load(
    seq: u64,
    agent_kind: AgentKind,
    session_id: String,
    tx: mpsc::Sender<TranscriptLoadResult>,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let loader_session_id = session_id.clone();
        let result = tokio::task::spawn_blocking(move || {
            load_harness_transcript(seq, agent_kind, loader_session_id)
        })
        .await
        .unwrap_or_else(|e| TranscriptLoadResult::Notice {
            seq,
            session_id: session_id.clone(),
            message: format!("Transcript worker failed: {e}"),
        });
        let _ = tx.send(result).await;
    })
}

fn load_harness_transcript(
    seq: u64,
    agent_kind: AgentKind,
    session_id: String,
) -> TranscriptLoadResult {
    let lookup = crate::harness::find_session_transcript(agent_kind, &session_id);

    match lookup {
        Ok(Some(path)) => match crate::harness::parse_transcript(agent_kind, &path) {
            Ok(mut messages) => {
                super::ui::prepare_transcript_messages(&mut messages);
                TranscriptLoadResult::Loaded {
                    seq,
                    session_id,
                    messages,
                }
            }
            Err(e) => {
                trace_error!("transcript parse failed", session_id = session_id.as_str(), error = %e);
                TranscriptLoadResult::Notice {
                    seq,
                    session_id,
                    message: "Transcript parse failed".to_string(),
                }
            }
        },
        Ok(None) => {
            trace_error!("transcript not found", session_id = session_id.as_str());
            TranscriptLoadResult::Notice {
                seq,
                session_id,
                message: "Transcript not found".to_string(),
            }
        }
        Err(e) => {
            trace_error!("transcript find failed", session_id = session_id.as_str(), error = %e);
            TranscriptLoadResult::Notice {
                seq,
                session_id,
                message: "Transcript lookup failed".to_string(),
            }
        }
    }
}

fn queue_visible_project_metrics(app: &mut ResumeApp, tx: &mpsc::Sender<ProjectLoadResult>) {
    if app.sessions.cwd_display_mode != CwdDisplayMode::TouchedProjects
        && !app.sessions.project_filter_needs_touched_projects()
    {
        return;
    }

    let scroll_offset = app.sessions.scroll_offset;
    let indices: Vec<usize> = app
        .sessions
        .visible_indices()
        .iter()
        .skip(scroll_offset)
        .take(64)
        .copied()
        .collect();

    for idx in indices {
        let session = &app.sessions.sessions[idx];
        if !matches!(session.agent_kind, AgentKind::Claude | AgentKind::Codex) {
            app.touched_projects
                .entry(session.session_key.clone())
                .or_insert_with(|| {
                    TouchedProjectsState::Notice(format!(
                        "{} project metrics are not wired yet",
                        session.agent_kind.display_name()
                    ))
                });
            app.sessions
                .set_touched_projects_for_session(session.session_key.clone(), Vec::new());
            continue;
        }

        if app.touched_projects.contains_key(&session.session_key) {
            continue;
        }

        app.touched_projects
            .insert(session.session_key.clone(), TouchedProjectsState::Loading);
        drop(spawn_project_load(
            session.agent_kind,
            session.native_id.clone(),
            session.session_key.clone(),
            tx.clone(),
        ));
    }
}

fn apply_project_result(app: &mut ResumeApp, result: ProjectLoadResult) {
    match result {
        ProjectLoadResult::Loaded {
            session_key,
            projects,
        } => {
            let paths = projects
                .iter()
                .map(|project| project.path.clone())
                .collect();
            app.sessions
                .set_touched_projects_for_session(session_key.clone(), paths);
            app.touched_projects
                .insert(session_key, TouchedProjectsState::Loaded(projects));
        }
        ProjectLoadResult::Notice {
            session_key,
            message,
        } => {
            trace_error!(
                "project metric load failed",
                session_key = session_key.as_str(),
                message = message.as_str()
            );
            app.sessions
                .set_touched_projects_for_session(session_key.clone(), Vec::new());
            app.touched_projects
                .insert(session_key, TouchedProjectsState::Notice(message));
        }
    }
}

fn apply_transcript_result(app: &mut ResumeApp, result: TranscriptLoadResult, current_seq: u64) {
    match result {
        TranscriptLoadResult::Loaded {
            seq,
            session_id,
            messages,
        } => {
            if seq == current_seq {
                app.transcript.load(session_id, messages);
            }
        }
        TranscriptLoadResult::Notice {
            seq,
            session_id,
            message,
        } => {
            if seq == current_seq {
                app.transcript.notice(session_id, message);
            }
        }
    }
}

async fn refresh_app_sessions<S>(
    app: &mut ResumeApp,
    source: &mut S,
    force: bool,
    label: &str,
) -> anyhow::Result<()>
where
    S: ResumeSessionSource,
{
    let sessions = source.refresh_sessions(force).await?;
    app.sessions.replace_sessions(sessions);
    // Project metrics are keyed by stable session_key — retain across refreshes.
    // Only prune entries for sessions that no longer exist in the catalog.
    let live_keys: std::collections::HashSet<&str> = app
        .sessions
        .sessions
        .iter()
        .map(|s| s.session_key.as_str())
        .collect();
    app.touched_projects
        .retain(|key, _| live_keys.contains(key.as_str()));
    app.status_message = format!("{label} {} sessions", app.sessions.sessions.len());
    Ok(())
}

fn spawn_daemon_refresh_listener() -> mpsc::Receiver<()> {
    let (tx, rx) = mpsc::channel(64);

    tokio::spawn(async move {
        if let Err(e) = daemon_refresh_listener(tx).await {
            trace_error!("resume pager daemon listener failed", error = %e);
        }
    });

    rx
}

async fn daemon_refresh_listener(tx: mpsc::Sender<()>) -> anyhow::Result<()> {
    let mut stream = UnixStream::connect(socket_path()).await?;
    let request = Request::Subscribe {
        events: vec![
            "window_added".to_string(),
            "window_removed".to_string(),
            "terminal_opened".to_string(),
            "terminal_closed".to_string(),
            "terminal_became_agent".to_string(),
            "session_matched".to_string(),
            "session_updated".to_string(),
            "session_state_changed".to_string(),
            "session_started".to_string(),
            "tool_started".to_string(),
            "tool_completed".to_string(),
            "title_generated".to_string(),
            "title_spliced".to_string(),
        ],
    };
    let mut request_json = serde_json::to_string(&request)?;
    request_json.push('\n');
    stream.write_all(request_json.as_bytes()).await?;

    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    reader.read_line(&mut line).await?;
    match serde_json::from_str::<Response>(&line)? {
        Response::Subscribed { .. } => {}
        Response::Error { message } => anyhow::bail!("subscription failed: {message}"),
        other => anyhow::bail!("unexpected subscription response: {other:?}"),
    }

    loop {
        line.clear();
        if reader.read_line(&mut line).await? == 0 {
            break;
        }

        if let Response::Event { event } = serde_json::from_str::<Response>(&line)? {
            let shutdown = matches!(event.event, BabelEvent::DaemonShutdown);
            let _ = tx.send(()).await;
            if shutdown {
                break;
            }
        }
    }

    Ok(())
}

/// Backend + connection + pane ID for refocusing the pager after launch.
#[derive(Clone)]
struct RefocusContext {
    backend: std::sync::Arc<dyn crate::backend::TerminalBackend>,
    conn: String,
    pane_id: u64,
}

fn detect_refocus_context() -> Option<RefocusContext> {
    if let Ok(id_str) = std::env::var("KITTY_WINDOW_ID") {
        if let Ok(pane_id) = id_str.parse::<u64>() {
            return Some(RefocusContext {
                backend: std::sync::Arc::new(crate::backend::kitty::KittyBackend),
                conn: crate::backend::kitty::default_socket(),
                pane_id,
            });
        }
    }

    if let Ok(pane_str) = std::env::var("TMUX_PANE") {
        let raw = pane_str.strip_prefix('%').unwrap_or(&pane_str);
        if let Ok(pane_id) = raw.parse::<u64>() {
            if let Ok(tmux_val) = std::env::var("TMUX") {
                if let Some(socket) = tmux_val.splitn(3, ',').next() {
                    return Some(RefocusContext {
                        backend: std::sync::Arc::new(crate::backend::tmux::TmuxBackend),
                        conn: format!("tmux:{socket}"),
                        pane_id,
                    });
                }
            }
        }
    }

    None
}
