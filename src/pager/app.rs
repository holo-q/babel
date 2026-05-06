//! ResumeApp - Main pager state machine
//!
//! Coordinates session list and transcript view, handles key events.

use std::io;

use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyModifiers};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use ratatui::backend::CrosstermBackend;
use ratatui::Terminal;
use vtr::trace_error;

use crate::agent_kind::AgentKind;
use crate::core::BabelCore;

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
    pub project_path: Option<PathBuf>,
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
    /// Should exit
    pub should_exit: bool,
    /// Selected session to resume (set on Enter)
    pub selected_session: Option<ResumeSelection>,
}

impl ResumeApp {
    pub fn new(sessions: Vec<EnrichedSession>, current_cwd: Option<PathBuf>) -> Self {
        Self {
            sessions: SessionListState::new(sessions, current_cwd),
            transcript: TranscriptView::new(),
            focus: PaneFocus::Sessions,
            is_searching: false,
            search_buffer: String::new(),
            should_exit: false,
            selected_session: None,
        }
    }

    /// Handle key event, return true to continue, false to exit
    pub fn handle_key(&mut self, key: KeyEvent) -> bool {
        // Ctrl+C always exits
        if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
            self.should_exit = true;
            return false;
        }

        // Handle search mode
        if self.is_searching {
            return self.handle_search_key(key);
        }

        match key.code {
            // Exit
            KeyCode::Char('q') | KeyCode::Esc => {
                self.should_exit = true;
                false
            }

            // Navigation
            KeyCode::Char('j') | KeyCode::Down => {
                if self.focus == PaneFocus::Sessions {
                    self.sessions.cursor_down();
                    self.load_selected_transcript();
                } else {
                    self.transcript.scroll_down(1);
                }
                true
            }
            KeyCode::Char('k') | KeyCode::Up => {
                if self.focus == PaneFocus::Sessions {
                    self.sessions.cursor_up();
                    self.load_selected_transcript();
                } else {
                    self.transcript.scroll_up(1);
                }
                true
            }
            KeyCode::Char('g') => {
                if self.focus == PaneFocus::Sessions {
                    self.sessions.cursor_top();
                    self.load_selected_transcript();
                } else {
                    self.transcript.scroll_top();
                }
                true
            }
            KeyCode::Char('G') => {
                if self.focus == PaneFocus::Sessions {
                    self.sessions.cursor_bottom();
                    self.load_selected_transcript();
                } else {
                    self.transcript.scroll_bottom();
                }
                true
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
                true
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
                true
            }

            // Tab - toggle cwd/all
            KeyCode::Tab => {
                self.sessions.toggle_show_all();
                self.load_selected_transcript();
                true
            }

            // Focus switching
            KeyCode::Char('l') | KeyCode::Right => {
                self.focus = PaneFocus::Transcript;
                true
            }
            KeyCode::Char('h') | KeyCode::Left => {
                self.focus = PaneFocus::Sessions;
                true
            }

            // Search
            KeyCode::Char('/') => {
                self.is_searching = true;
                self.search_buffer.clear();
                true
            }

            // Resume selected session
            KeyCode::Enter => {
                if let Some(session) = self.sessions.selected() {
                    self.selected_session = Some(ResumeSelection {
                        agent_kind: session.agent_kind,
                        native_id: session.native_id.clone(),
                        project_path: session.project_path.clone(),
                    });
                    self.should_exit = true;
                    return false;
                }
                true
            }

            _ => true,
        }
    }

    fn handle_search_key(&mut self, key: KeyEvent) -> bool {
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
        true
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
pub async fn run_resume_pager(
    _core: &BabelCore,
    show_all: bool,
    sessions: Vec<EnrichedSession>,
) -> anyhow::Result<Option<ResumeSelection>> {
    // Get current working directory for cwd filtering
    let current_cwd = std::env::current_dir().ok();

    if sessions.is_empty() {
        println!("No sessions found");
        return Ok(None);
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
                if !app.handle_key(key) {
                    break;
                }
            }
        }

        if app.should_exit {
            break;
        }
    }

    // Cleanup terminal
    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    terminal.show_cursor()?;

    Ok(app.selected_session)
}
