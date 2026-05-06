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
}

#[async_trait::async_trait]
pub trait ResumeSessionSource {
    async fn refresh_sessions(&mut self, force: bool) -> anyhow::Result<Vec<EnrichedSession>>;
    async fn launch_resume(&mut self, selection: &ResumeSelection) -> anyhow::Result<String>;

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
                    self.load_selected_transcript();
                } else {
                    self.transcript.scroll_down(1);
                }
                ResumeAction::None
            }
            KeyCode::Char('k') | KeyCode::Up => {
                if self.focus == PaneFocus::Sessions {
                    self.sessions.cursor_up();
                    self.load_selected_transcript();
                } else {
                    self.transcript.scroll_up(1);
                }
                ResumeAction::None
            }
            KeyCode::Char('g') => {
                if self.focus == PaneFocus::Sessions {
                    self.sessions.cursor_top();
                    self.load_selected_transcript();
                } else {
                    self.transcript.scroll_top();
                }
                ResumeAction::None
            }
            KeyCode::Char('G') => {
                if self.focus == PaneFocus::Sessions {
                    self.sessions.cursor_bottom();
                    self.load_selected_transcript();
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
                    self.load_selected_transcript();
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
                    self.load_selected_transcript();
                } else {
                    self.transcript.scroll_up(10);
                }
                ResumeAction::None
            }

            // Tab - toggle cwd/all
            KeyCode::Tab => {
                self.sessions.toggle_show_all();
                self.load_selected_transcript();
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
            KeyCode::Char('h') | KeyCode::Left => {
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

    fn load_selected_transcript(&mut self) {
        // Get selected session
        let session = match self.sessions.selected() {
            Some(s) => s,
            None => {
                self.transcript.clear();
                return;
            }
        };

        let session_id = &session.native_id;

        // Skip if already loaded
        if self.transcript.session_id.as_deref() == Some(session_id) {
            return;
        }

        if session.agent_kind != AgentKind::Claude {
            self.transcript.notice(
                session_id.clone(),
                format!(
                    "{} transcript preview is not wired yet",
                    session.agent_kind.display_name()
                ),
            );
            return;
        }

        // Find and parse the transcript JSONL
        match crate::utility::claude_storage::find_session_transcript(session_id) {
            Ok(Some(path)) => match super::jsonl_parser::parse_transcript(&path) {
                Ok(messages) => {
                    self.transcript.load(session_id.clone(), messages);
                }
                Err(e) => {
                    trace_error!("transcript parse failed", session_id = session_id, error = %e);
                    self.transcript
                        .notice(session_id.clone(), "Transcript parse failed".to_string());
                }
            },
            Ok(None) => {
                trace_error!("transcript not found", session_id = session_id);
                self.transcript
                    .notice(session_id.clone(), "Transcript not found".to_string());
            }
            Err(e) => {
                trace_error!("transcript find failed", session_id = session_id, error = %e);
                self.transcript
                    .notice(session_id.clone(), "Transcript lookup failed".to_string());
            }
        }
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

    // Initial transcript load
    app.load_selected_transcript();

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
        terminal.draw(|f| super::ui::draw(f, &app))?;

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
                    }
                }
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
    app.load_selected_transcript();
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
