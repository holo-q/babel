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
use vtr::trace_error;

use crate::agent_kind::AgentKind;
use crate::events::BabelEvent;
use crate::ipc::{Request, Response};
use crate::utility::ipc::socket_path;

use super::preferences::{
    load_resume_display_options, save_resume_display_options, ResumeDisplayOptions,
};
use super::project_metrics::ProjectTouchMetric;
use super::session_list::{CwdDisplayMode, EnrichedSession, SessionListState, SortColumn};
use super::transcript::TranscriptView;
use std::path::PathBuf;

/// Which panel has focus
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum PaneFocus {
    #[default]
    Sessions,
    Transcript,
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
    Launch(ResumeSelection),
    Refresh,
    SetHidden { session_key: String, hidden: bool },
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
    /// Whether search mode is active
    pub is_searching: bool,
    /// Search query buffer
    pub search_buffer: String,
    /// Whether the transcript preview pane is visible
    pub show_transcript: bool,
    /// Whether list text cells use middle snip markers instead of edge clipping.
    pub snip_columns: bool,
    /// Last launcher/refresh status shown in the footer
    pub status_message: String,
    /// Cached/async per-session project-touch metric for cwd-column rendering.
    pub touched_projects: HashMap<String, TouchedProjectsState>,
    /// Display preferences changed and should be persisted after the key event.
    display_options_dirty: bool,
    /// Should exit
    pub should_exit: bool,
}

impl ResumeApp {
    pub fn new(sessions: Vec<EnrichedSession>, current_cwd: Option<PathBuf>) -> Self {
        Self {
            sessions: SessionListState::new(sessions, current_cwd),
            transcript: TranscriptView::new(),
            focus: PaneFocus::Sessions,
            is_searching: false,
            search_buffer: String::new(),
            show_transcript: true,
            snip_columns: true,
            status_message: "Enter: launch  r: refresh".to_string(),
            touched_projects: HashMap::new(),
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
        self.sessions.invalidate_visible_indices();
        self.show_transcript = options.show_transcript;
        self.snip_columns = options.snip_columns;
        self.transcript.expand_messages = options.expand_messages;
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
            show_transcript: self.show_transcript,
            snip_columns: self.snip_columns,
            expand_messages: self.transcript.expand_messages,
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
        if self.is_searching {
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

            // Mark/unmark selected session as hidden
            KeyCode::Char(ch)
                if ch == 'H' || (ch == 'h' && key.modifiers.contains(KeyModifiers::SHIFT)) =>
            {
                let Some((session_key, hidden)) = self.sessions.toggle_selected_hidden() else {
                    return ResumeAction::None;
                };
                ResumeAction::SetHidden {
                    session_key,
                    hidden,
                }
            }

            // Toggle hidden sessions
            KeyCode::Char('h') => {
                let mode = self.sessions.cycle_hidden_display_mode();
                self.status_message = format!("hidden display: {}", mode.label());
                self.mark_display_options_dirty();
                ResumeAction::None
            }

            // Cycle cwd display label: relative -> absolute -> project name.
            KeyCode::Char('c') => {
                let mode = self.sessions.cycle_cwd_display_mode();
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
            KeyCode::Char('s') => {
                if self.focus == PaneFocus::Transcript {
                    let expanded = self.transcript.toggle_message_expansion();
                    self.status_message = if expanded {
                        "transcript messages: full".to_string()
                    } else {
                        "transcript messages: snip".to_string()
                    };
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
                self.transcript.toggle_role_filter();
                self.status_message = "transcript filter changed".to_string();
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

            // Search
            KeyCode::Char('/') => {
                self.is_searching = true;
                self.search_buffer.clear();
                ResumeAction::None
            }

            // Resume selected session
            KeyCode::Enter => {
                if let Some(session) = self.sessions.selected() {
                    return ResumeAction::Launch(ResumeSelection {
                        agent_kind: session.agent_kind,
                        native_id: session.native_id.clone(),
                        session_key: session.session_key.clone(),
                        project_path: session.project_path.clone(),
                    });
                }
                ResumeAction::None
            }

            // Sort session list by visible column index. `#` is a generated
            // row number, not source data; `0` covers the tenth data column.
            // 1 state, 2 harness, 3 workspace, 4 cwd, 5 filter, 6 mt,
            // 7 ct, 8 turns, 9 thread, 0 prompt.
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
        match key.code {
            KeyCode::Enter | KeyCode::Esc => {
                self.is_searching = false;
                if key.code == KeyCode::Esc {
                    self.search_buffer.clear();
                    self.sessions.set_filter(String::new());
                }
            }
            KeyCode::Backspace => {
                self.search_buffer.pop();
                self.sessions.set_filter(self.search_buffer.clone());
            }
            KeyCode::Char(c) => {
                self.search_buffer.push(c);
                self.sessions.set_filter(self.search_buffer.clone());
            }
            _ => {}
        }
    }

    fn selected_transcript_target(&mut self) -> Option<TranscriptTarget> {
        self.sessions.selected().map(|session| TranscriptTarget {
            agent_kind: session.agent_kind,
            native_id: session.native_id.clone(),
            session_key: session.session_key.clone(),
        })
    }
}

/// Run the resume pager TUI
pub async fn run_resume_pager<S>(
    source: &mut S,
    show_all: bool,
    sessions: Vec<EnrichedSession>,
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
    let mut app = ResumeApp::new(sessions, current_cwd);
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
                            ResumeAction::Launch(selection) => {
                                let short_id: String =
                                    selection.native_id.chars().take(8).collect();
                                app.status_message = format!(
                                    "launching {} {short_id}...",
                                    selection.agent_kind.slug(),
                                );
                                let tx = launch_tx.clone();
                                let refocus = refocus_ctx.clone();
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
    let spec = selection.agent_kind.spec();
    let resume_cmd = spec.resume_command(&selection.native_id).ok_or_else(|| {
        anyhow::anyhow!(
            "{} has no resume command",
            selection.agent_kind.display_name()
        )
    })?;
    let parts: Vec<&str> = resume_cmd.split_whitespace().collect();
    if parts.is_empty() {
        return Err(anyhow::anyhow!(
            "Empty resume command for {}",
            selection.agent_kind.display_name()
        ));
    }

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

    let (backend, conn) = crate::backend::detect_current_backend()?;
    let launched = backend.launch_pane(&conn, &parts, &cwd).await?;

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
    let lookup = match agent_kind {
        AgentKind::Claude => crate::utility::claude_storage::find_session_transcript(&session_id),
        AgentKind::Codex => crate::harness::codex::transcript::find_session_transcript(&session_id),
        _ => Ok(None),
    };

    match lookup {
        Ok(Some(path)) => {
            let parsed = match agent_kind {
                AgentKind::Claude => super::jsonl_parser::parse_transcript(&path),
                AgentKind::Codex => crate::harness::codex::transcript::parse_transcript(&path),
                _ => Ok(Vec::new()),
            };

            match parsed {
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
            }
        }
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
    if app.sessions.cwd_display_mode != CwdDisplayMode::TouchedProjects {
        return;
    }

    let scroll_offset = app.sessions.selection.scroll_offset;
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
