//! TuiApp - Application state and event loop
//!
//! Contains the main TUI state struct and the async event loop that
//! handles keyboard input, daemon events, and periodic refresh.

use std::io;
use std::time::Duration;

use anyhow::{Context, Result};
use crossterm::{
    event::{self, Event, KeyCode, KeyEvent, KeyModifiers},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{backend::CrosstermBackend, Terminal};
use tokio::time::Instant;
use tracing::{debug, info, warn};

use crate::daemon::TerminalInfo;
use crate::fire::FiredTask;
use crate::utility::claude_discovery::ClaudeWindow;
use crate::utility::ipc::{self, Request, Response};

use super::ipc_client::{IpcLogEntry, LoggingIpcClient};
use super::ui;

/// Which pane is currently focused
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Pane {
    #[default]
    Windows,
    Fired,
    Details,
    IpcLog,
}

impl Pane {
    /// Cycle to next pane
    pub fn next(self) -> Self {
        match self {
            Pane::Windows => Pane::Fired,
            Pane::Fired => Pane::Details,
            Pane::Details => Pane::IpcLog,
            Pane::IpcLog => Pane::Windows,
        }
    }

    /// Cycle to previous pane
    pub fn prev(self) -> Self {
        match self {
            Pane::Windows => Pane::IpcLog,
            Pane::Fired => Pane::Windows,
            Pane::Details => Pane::Fired,
            Pane::IpcLog => Pane::Details,
        }
    }
}

/// Content shown in the Details pane
#[derive(Debug, Clone)]
pub enum DetailContent {
    Window(Box<ClaudeWindow>),
    Terminal(TerminalInfo),
    FiredTask(FiredTask),
    IpcMessage(super::ipc_client::IpcLogEntry),
    None,
}

/// Main TUI application state
pub struct TuiApp {
    /// IPC client with traffic logging
    pub client: LoggingIpcClient,
    /// Daemon uptime (from ping response)
    pub daemon_uptime: Duration,

    // ─── Data from daemon ───────────────────────────────────────────────────────
    /// Claude windows from daemon cache
    pub windows: Vec<ClaudeWindow>,
    /// All kitty terminals (including non-Claude) for visibility
    pub terminals: Vec<TerminalInfo>,
    /// Fire-and-forget tasks from filesystem
    pub fired_tasks: Vec<FiredTask>,

    // ─── UI State ───────────────────────────────────────────────────────────────
    /// Currently active pane
    pub active_pane: Pane,
    /// Selected index in windows list
    pub window_selected: usize,
    /// Selected index in fired tasks list
    pub fired_selected: usize,
    /// Scroll offset in IPC log
    pub ipc_scroll: usize,
    /// Auto-scroll IPC log to bottom
    pub ipc_auto_scroll: bool,
    /// Cached IPC log entries (refreshed each frame)
    pub ipc_log: Vec<IpcLogEntry>,
    /// Selected index in IPC log
    pub ipc_selected: usize,
    /// Content for details pane
    pub detail_content: DetailContent,
    /// Show help overlay
    pub show_help: bool,

    // ─── Timing ─────────────────────────────────────────────────────────────────
    /// Last refresh time
    pub last_refresh: Instant,
}

impl TuiApp {
    /// Create new TUI app, connecting to daemon
    ///
    /// Exits with error if daemon is not running.
    pub async fn new() -> Result<Self> {
        // Check daemon is running first
        let client = LoggingIpcClient::new();

        let uptime = match client.send_request(&Request::Ping).await? {
            Response::Pong { uptime_secs } => Duration::from_secs(uptime_secs),
            other => {
                anyhow::bail!("Unexpected response to ping: {:?}", other);
            }
        };

        info!("Connected to daemon, uptime: {:?}", uptime);

        let mut app = Self {
            client,
            daemon_uptime: uptime,
            windows: Vec::new(),
            terminals: Vec::new(),
            fired_tasks: Vec::new(),
            active_pane: Pane::Windows,
            window_selected: 0,
            fired_selected: 0,
            ipc_scroll: 0,
            ipc_auto_scroll: true,
            ipc_log: Vec::new(),
            ipc_selected: 0,
            detail_content: DetailContent::None,
            show_help: false,
            last_refresh: Instant::now(),
        };

        // Initial data fetch
        app.refresh_windows().await?;
        app.refresh_terminals().await?;
        app.refresh_fired_tasks()?;

        Ok(app)
    }

    /// Refresh windows list from daemon
    pub async fn refresh_windows(&mut self) -> Result<()> {
        match self.client.send_request(&Request::List).await? {
            Response::Windows { windows } => {
                self.windows = windows;
                // Clamp selection
                if self.window_selected >= self.windows.len() && !self.windows.is_empty() {
                    self.window_selected = self.windows.len() - 1;
                }
            }
            Response::Error { message } => {
                warn!("Failed to list windows: {}", message);
            }
            _ => {}
        }
        self.last_refresh = Instant::now();
        Ok(())
    }

    /// Refresh terminals list from daemon (all kitty windows, not just Claude)
    pub async fn refresh_terminals(&mut self) -> Result<()> {
        match self.client.send_request(&Request::ListTerminals).await? {
            Response::Terminals { terminals } => {
                self.terminals = terminals;
            }
            Response::Error { message } => {
                warn!("Failed to list terminals: {}", message);
            }
            _ => {}
        }
        Ok(())
    }

    /// Refresh fired tasks from filesystem
    pub fn refresh_fired_tasks(&mut self) -> Result<()> {
        self.fired_tasks = crate::fire::list_running_tasks().unwrap_or_default();
        // Clamp selection
        if self.fired_selected >= self.fired_tasks.len() && !self.fired_tasks.is_empty() {
            self.fired_selected = self.fired_tasks.len() - 1;
        }
        Ok(())
    }

    /// Refresh daemon uptime
    pub async fn refresh_uptime(&mut self) -> Result<()> {
        if let Ok(Response::Pong { uptime_secs }) =
            self.client.send_request(&Request::Ping).await
        {
            self.daemon_uptime = Duration::from_secs(uptime_secs);
        }
        Ok(())
    }

    /// Refresh IPC log from client
    pub async fn refresh_ipc_log(&mut self) {
        self.ipc_log = self.client.get_log().await;
        // Auto-scroll to bottom if enabled
        if self.ipc_auto_scroll && !self.ipc_log.is_empty() {
            self.ipc_selected = self.ipc_log.len() - 1;
        }
    }

    /// Handle keyboard input
    pub fn handle_key(&mut self, key: KeyEvent) -> bool {
        // Help overlay takes precedence
        if self.show_help {
            self.show_help = false;
            return true;
        }

        match key.code {
            // Quit
            KeyCode::Char('q') | KeyCode::Esc => return false,

            // Help
            KeyCode::Char('?') => {
                self.show_help = true;
            }

            // Pane navigation
            KeyCode::Tab => {
                if key.modifiers.contains(KeyModifiers::SHIFT) {
                    self.active_pane = self.active_pane.prev();
                } else {
                    self.active_pane = self.active_pane.next();
                }
            }
            KeyCode::F(1) => self.active_pane = Pane::Windows,
            KeyCode::F(2) => self.active_pane = Pane::Fired,
            KeyCode::F(3) => self.active_pane = Pane::Details,
            KeyCode::F(4) => self.active_pane = Pane::IpcLog,

            // List navigation
            KeyCode::Char('j') | KeyCode::Down => self.move_selection(1),
            KeyCode::Char('k') | KeyCode::Up => self.move_selection(-1),

            // Select item
            KeyCode::Enter => self.select_current(),

            // IPC log controls
            KeyCode::Char('a') if self.active_pane == Pane::IpcLog => {
                self.ipc_auto_scroll = !self.ipc_auto_scroll;
            }
            KeyCode::Char('c') if self.active_pane == Pane::IpcLog => {
                // Will be handled in async context
            }

            _ => {}
        }

        true
    }

    /// Move selection in current list
    fn move_selection(&mut self, delta: i32) {
        match self.active_pane {
            Pane::Windows => {
                if self.terminals.is_empty() {
                    return;
                }
                let new = (self.window_selected as i32 + delta)
                    .clamp(0, self.terminals.len() as i32 - 1) as usize;
                self.window_selected = new;
            }
            Pane::Fired => {
                if self.fired_tasks.is_empty() {
                    return;
                }
                let new = (self.fired_selected as i32 + delta)
                    .clamp(0, self.fired_tasks.len() as i32 - 1) as usize;
                self.fired_selected = new;
            }
            Pane::IpcLog => {
                if self.ipc_log.is_empty() {
                    return;
                }
                let new = (self.ipc_selected as i32 + delta)
                    .clamp(0, self.ipc_log.len() as i32 - 1) as usize;
                self.ipc_selected = new;
                self.ipc_auto_scroll = false;
            }
            Pane::Details => {
                // Details pane doesn't have list navigation (yet)
            }
        }
    }

    /// Select current item and show in details
    fn select_current(&mut self) {
        match self.active_pane {
            Pane::Windows => {
                // Get selected terminal
                if let Some(term) = self.terminals.get(self.window_selected) {
                    if term.is_claude {
                        // For Claude terminals, show full ClaudeWindow details
                        if let Some(window) = self.windows.iter().find(|w| w.kitty_id == term.kitty_id) {
                            self.detail_content = DetailContent::Window(Box::new(window.clone()));
                        } else {
                            // Fallback to TerminalInfo if window not found
                            self.detail_content = DetailContent::Terminal(term.clone());
                        }
                    } else {
                        // For non-Claude terminals, show TerminalInfo
                        self.detail_content = DetailContent::Terminal(term.clone());
                    }
                }
            }
            Pane::Fired => {
                if let Some(task) = self.fired_tasks.get(self.fired_selected) {
                    self.detail_content = DetailContent::FiredTask(task.clone());
                }
            }
            Pane::IpcLog => {
                if let Some(entry) = self.ipc_log.get(self.ipc_selected) {
                    self.detail_content = DetailContent::IpcMessage(entry.clone());
                }
            }
            Pane::Details => {
                // Details pane - no selection
            }
        }
    }
}

/// Entry point - run the TUI application
pub async fn run_tui() -> Result<()> {
    // Check daemon is running before entering TUI mode
    if !ipc::is_daemon_running().await {
        anyhow::bail!(
            "Daemon not running. Start with: babel daemon\n\
             The TUI requires an active daemon connection for IPC inspection."
        );
    }

    // Setup terminal
    enable_raw_mode().context("Failed to enable raw mode")?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen).context("Failed to enter alternate screen")?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    // Create app
    let mut app = TuiApp::new().await?;

    // Run event loop
    let result = run_event_loop(&mut terminal, &mut app).await;

    // Cleanup terminal
    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    terminal.show_cursor()?;

    result
}

/// Main event loop
async fn run_event_loop(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    app: &mut TuiApp,
) -> Result<()> {
    let tick_rate = Duration::from_millis(100);
    let refresh_rate = Duration::from_millis(500);
    let mut last_tick = Instant::now();

    loop {
        // Refresh IPC log before each draw (fast - just copies from mutex)
        app.refresh_ipc_log().await;

        // Draw UI
        terminal.draw(|f| ui::draw(f, app))?;

        // Calculate timeout for event poll
        let timeout = tick_rate.saturating_sub(last_tick.elapsed());

        // Poll for events
        if event::poll(timeout)? {
            if let Event::Key(key) = event::read()? {
                if !app.handle_key(key) {
                    break; // Quit requested
                }

                // Handle async key actions
                if key.code == KeyCode::Char('c') && app.active_pane == Pane::IpcLog {
                    app.client.clear_log().await;
                }
                if key.code == KeyCode::Char('r') {
                    app.refresh_windows().await?;
                    app.refresh_terminals().await?;
                    app.refresh_fired_tasks()?;
                }
            }
        }

        // Tick - periodic refresh
        if last_tick.elapsed() >= tick_rate {
            last_tick = Instant::now();

            // Periodic data refresh
            if app.last_refresh.elapsed() >= refresh_rate {
                app.refresh_windows().await?;
                app.refresh_terminals().await?;
                app.refresh_fired_tasks()?;
                app.refresh_uptime().await?;
            }
        }
    }

    debug!("TUI exiting");
    Ok(())
}
