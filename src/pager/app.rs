//! ResumeApp - Main pager state machine
//!
//! Coordinates session list and transcript view, handles key events.

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

use super::session_list::{EnrichedSession, SessionListState};
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
    /// Last launcher/refresh status shown in the footer
    pub status_message: String,
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
            status_message: "Enter: launch  r: refresh".to_string(),
            should_exit: false,
        }
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
                    for _ in 0..10 {
                        self.sessions.cursor_down();
                    }
                } else {
                    self.transcript.scroll_down(10);
                }
                ResumeAction::None
            }
            KeyCode::PageUp => {
                if self.focus == PaneFocus::Sessions {
                    for _ in 0..10 {
                        self.sessions.cursor_up();
                    }
                } else {
                    self.transcript.scroll_up(10);
                }
                ResumeAction::None
            }

            // Tab - toggle cwd/all
            KeyCode::Tab => {
                self.sessions.toggle_show_all();
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
                self.sessions.toggle_show_hidden();
                ResumeAction::None
            }

            // Cycle cwd display label: relative -> absolute -> project name.
            KeyCode::Char('c') => {
                let mode = self.sessions.cycle_cwd_display_mode();
                self.status_message = format!("cwd display: {}", mode.label());
                ResumeAction::None
            }

            // Toggle transcript preview
            KeyCode::Char('t') => {
                self.show_transcript = !self.show_transcript;
                if !self.show_transcript && self.focus == PaneFocus::Transcript {
                    self.focus = PaneFocus::Sessions;
                }
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

    fn selected_transcript_target(&self) -> Option<TranscriptTarget> {
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
    // If --all flag passed, start in show_all mode
    app.sessions.show_all = show_all;

    let (transcript_tx, mut transcript_rx) = mpsc::channel(8);
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

    // Event loop
    loop {
        // Draw UI
        terminal.draw(|f| super::ui::draw(f, &mut app))?;

        // Poll for events
        if event::poll(std::time::Duration::from_millis(100))? {
            if let Event::Key(key) = event::read()? {
                if key.kind == event::KeyEventKind::Release {
                    continue;
                }
                match app.handle_key(key) {
                    ResumeAction::None => {}
                    ResumeAction::Quit => break,
                    ResumeAction::Launch(selection) => {
                        match source.launch_resume(&selection).await {
                            Ok(message) => app.status_message = message,
                            Err(e) => app.status_message = format!("launch failed: {e}"),
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
                sync_selected_transcript_target(
                    &mut app,
                    &mut desired_transcript_key,
                    &mut pending_transcript,
                    &mut transcript_seq,
                );
            }
        }

        while let Ok(result) = transcript_rx.try_recv() {
            apply_transcript_result(&mut app, result, transcript_seq);
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
                Ok(messages) => TranscriptLoadResult::Loaded {
                    seq,
                    session_id,
                    messages,
                },
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
