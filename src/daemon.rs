//! Babel Daemon - Background service for Claude session tracking
//!
//! The daemon maintains:
//! - Live mapping of kitty windows → Claude sessions
//! - Cached session info from ~/.claude
//! - Summary index for fast matching
//!
//! It watches:
//! - Kitty windows (polling every 500ms)
//! - ~/.claude/projects/ (inotify for new/changed files)
//!
//! CLI commands query the daemon over unix socket for instant responses.

use anyhow::{Context, Result};
use notify::RecursiveMode;
use notify_debouncer_mini::{new_debouncer, DebouncedEventKind};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tokio::sync::{mpsc, RwLock};

use crate::claude_storage::{claude_base, get_recent_sessions, get_session_info};
use crate::discovery::{enrich_window, ClaudeWindow};
use crate::events::{BabelEvent, EventFilter, EventMessage, EventPublisher};
use crate::fingerprint::{
    SessionFingerprint, MatchConfidence,
    extract_from_scrollback, extract_from_jsonl, match_fingerprints
};
use crate::ipc::{create_listener, Request, Response};
use crate::kitty::{find_claude_windows, focus_window, get_scrollback, send_text};
use crate::overlay::{init_db, mark_read, set_icon};
use crate::wset::{WSet, get_current_wset_name, set_current_wset_name, list_wsets};

use std::sync::atomic::{AtomicBool, Ordering};

/// Global trace flag - set via run_daemon_with_trace
/// When enabled, debug-level tracing is used instead of trace-level
pub static TRACE: AtomicBool = AtomicBool::new(false);

macro_rules! trace {
    ($($arg:tt)*) => {
        if TRACE.load(Ordering::Relaxed) {
            tracing::debug!($($arg)*);
        } else {
            tracing::trace!($($arg)*);
        }
    };
}

// ═══════════════════════════════════════════════════════════════════════════════
// Daemon State
// ═══════════════════════════════════════════════════════════════════════════════

/// Summary entry for fast matching
#[derive(Debug, Clone)]
struct SummaryEntry {
    summary: String,
    session_id: String,
}

/// Daemon state - shared across tasks
pub struct DaemonState {
    /// Current Claude windows (kitty_id → ClaudeWindow)
    pub windows: HashMap<u64, ClaudeWindow>,

    /// Summary index for fast title→session matching
    pub summary_index: Vec<SummaryEntry>,

    /// Fingerprint index for matching (session_id → fingerprint)
    /// Limited to 100 most recent sessions for performance
    pub fingerprint_index: HashMap<String, SessionFingerprint>,

    /// Cached fingerprints for windows (kitty_id → fingerprint)
    /// Extracted from scrollback, used for matching
    pub window_fingerprints: HashMap<u64, SessionFingerprint>,

    /// Cached session states for windows (kitty_id → SessionState)
    /// Used to detect state changes and emit SessionStateChanged events
    pub window_states: HashMap<u64, crate::state::SessionState>,

    /// When the daemon started
    pub start_time: Instant,

    /// Last time we scanned kitty windows
    pub last_kitty_scan: Instant,

    /// Last time we rebuilt the fingerprint index (debounce frequent rebuilds)
    pub last_fingerprint_rebuild: Instant,

    /// Event publisher for push notifications
    pub event_publisher: EventPublisher,

    /// Current workspace titles (workspace → title)
    /// Authoritative source - frontends query this via IPC
    pub workspace_titles: HashMap<i32, String>,
}

impl Default for DaemonState {
    fn default() -> Self {
        Self::new()
    }
}

impl DaemonState {
    pub fn new() -> Self {
        Self {
            windows: HashMap::new(),
            summary_index: Vec::new(),
            fingerprint_index: HashMap::new(),
            window_fingerprints: HashMap::new(),
            window_states: HashMap::new(),
            start_time: Instant::now(),
            last_kitty_scan: Instant::now(),
            last_fingerprint_rebuild: Instant::now(),
            event_publisher: EventPublisher::new(),
            workspace_titles: HashMap::new(),
        }
    }

    /// Refresh kitty windows
    ///
    /// Returns list of workspaces that had windows added or removed,
    /// for triggering title re-summarization.
    pub fn refresh_windows(&mut self) -> Result<Vec<i32>> {
        use crate::kitty::get_all_workspaces;

        let kitty_windows = find_claude_windows()?;

        // Get workspace mappings in one call
        let workspaces = get_all_workspaces();

        // Build new windows map, preserving enriched data where possible
        let mut new_windows = HashMap::new();

        for kw in kitty_windows {
            let window_id = kw.id;
            let workspace = workspaces.get(&kw.platform_window_id).copied();

            // Check if we have existing data for this window (use get, not remove)
            let mut claude_window = if let Some(existing) = self.windows.get(&window_id) {
                // Clone existing and update dynamic fields
                let mut updated = existing.clone();
                updated.workspace = workspace;
                updated.is_focused = kw.is_focused;

                // Reset agent-* sessions to force re-matching via fingerprint
                if updated.session_id.as_ref().is_some_and(|id| id.starts_with("agent-")) {
                    updated.session_id = None;
                    updated.session_info = None;
                    updated.match_confidence = None;
                }

                // Preserve existing session info if title hasn't changed
                if updated.title != kw.title {
                    // Title changed - need to re-match
                    updated.title = kw.title.clone();
                    updated.session_id = None;
                    updated.session_info = None;
                }
                updated
            } else {
                // New window - check for existing tag but ignore agent-* sessions
                // (they were matched before we had fingerprinting)
                let existing_session = kw.user_vars.get("babel_session_id")
                    .filter(|id| !id.starts_with("agent-"))
                    .cloned();
                ClaudeWindow {
                    kitty_id: kw.id,
                    title: kw.title.clone(),
                    session_id: existing_session,
                    session_info: None,
                    cwd: kw.cwd.clone(),
                    is_focused: kw.is_focused,
                    os_window_id: kw.os_window_id,
                    platform_window_id: kw.platform_window_id,
                    workspace,
                    fingerprint: None,
                    match_confidence: None,
                }
            };

            // Try to match unmatched windows using summary index
            if claude_window.session_id.is_none() {
                trace!("Window {} needs matching (title: {})", window_id, claude_window.title);

                if let Some(session_id) = self.match_title_to_session(&claude_window.title) {
                    trace!("  → Title matched to session: {}", session_id);
                    claude_window.session_id = Some(session_id);
                } else {
                    trace!("  → Title match failed, trying fingerprint...");
                }
            }

            // Fallback: try fingerprint matching if title match failed
            if claude_window.session_id.is_none() {
                if let Some((session_id, confidence, fingerprint)) = self.fingerprint_match(window_id) {
                    trace!("  → Fingerprint matched to session: {} ({:?})", session_id, confidence);
                    // Tag the window for future fast lookups
                    let _ = crate::discovery::tag_window(window_id, &session_id);
                    claude_window.session_id = Some(session_id);
                    claude_window.match_confidence = Some(confidence);
                    claude_window.fingerprint = Some(fingerprint);
                } else {
                    trace!("  → Fingerprint match failed (no confident match)");
                }
            }

            new_windows.insert(window_id, claude_window);
        }

        // Detect and emit events for window changes
        let old_ids: std::collections::HashSet<_> = self.windows.keys().cloned().collect();
        let new_ids: std::collections::HashSet<_> = new_windows.keys().cloned().collect();

        // Track workspaces that need re-summarization
        let mut changed_workspaces: std::collections::HashSet<i32> = std::collections::HashSet::new();

        // Windows added
        for &id in new_ids.difference(&old_ids) {
            if let Some(w) = new_windows.get(&id) {
                self.event_publisher.publish(BabelEvent::WindowAdded {
                    kitty_id: id,
                    title: w.title.clone(),
                    workspace: w.workspace,
                });
                // Track workspace for re-summarization
                if let Some(ws) = w.workspace {
                    changed_workspaces.insert(ws);
                }
            }
        }

        // Windows removed
        for &id in old_ids.difference(&new_ids) {
            // Get workspace from old windows before removal
            if let Some(w) = self.windows.get(&id) {
                if let Some(ws) = w.workspace {
                    changed_workspaces.insert(ws);
                }
            }
            self.event_publisher.publish(BabelEvent::WindowRemoved { kitty_id: id });
        }

        // Check for focus changes
        let old_focused = self.windows.values().find(|w| w.is_focused).map(|w| w.kitty_id);
        let new_focused = new_windows.values().find(|w| w.is_focused).map(|w| w.kitty_id);
        if old_focused != new_focused {
            if let Some(id) = new_focused {
                if let Some(w) = new_windows.get(&id) {
                    self.event_publisher.publish(BabelEvent::WindowFocused {
                        kitty_id: id,
                        session_id: w.session_id.clone(),
                    });
                }
            }
        }

        // Detect session state changes and emit events
        // This enables richspace-babel to track Claude activity per-workspace
        for (kitty_id, window) in &new_windows {
            let new_state = crate::state::get_window_state(*kitty_id);
            let old_state = self.window_states.get(kitty_id).copied();

            match old_state {
                Some(old) if old != new_state => {
                    // State changed - emit event
                    trace!("Window {} state change: {:?} -> {:?}", kitty_id, old, new_state);
                    self.event_publisher.publish(BabelEvent::SessionStateChanged {
                        kitty_id: *kitty_id,
                        session_id: window.session_id.clone(),
                        workspace: window.workspace,
                        old_state: old,
                        new_state,
                    });
                    self.window_states.insert(*kitty_id, new_state);
                }
                None => {
                    // New window - initialize state (no event, WindowAdded already fired)
                    self.window_states.insert(*kitty_id, new_state);
                }
                _ => {
                    // State unchanged
                }
            }
        }

        // Clean up states for removed windows
        self.window_states.retain(|id, _| new_windows.contains_key(id));

        self.windows = new_windows;
        self.last_kitty_scan = Instant::now();

        // Return workspaces that changed for re-summarization
        Ok(changed_workspaces.into_iter().collect())
    }

    /// Match a window title to a session using the summary index
    fn match_title_to_session(&self, title: &str) -> Option<String> {
        // Extract summary from title (strip "✳ " prefix)
        let summary = title.strip_prefix("✳ ")?.trim();

        // Search index for match (case-insensitive contains)
        let summary_lower = summary.to_lowercase();
        self.summary_index
            .iter()
            .find(|e| e.summary.to_lowercase().contains(&summary_lower))
            .map(|e| e.session_id.clone())
    }

    /// Rebuild summary index from ~/.claude/projects
    pub fn rebuild_summary_index(&mut self) -> Result<()> {
        let projects_dir = claude_base().join("projects");
        if !projects_dir.exists() {
            return Ok(());
        }

        let mut index = Vec::new();

        // Scan all project directories
        for project_entry in std::fs::read_dir(&projects_dir)? {
            let project_entry = project_entry?;
            let project_path = project_entry.path();

            if !project_path.is_dir() {
                continue;
            }

            // Scan session files in this project
            for session_entry in std::fs::read_dir(&project_path)? {
                let session_entry = session_entry?;
                let session_path = session_entry.path();

                if session_path.extension().and_then(|s| s.to_str()) != Some("jsonl") {
                    continue;
                }

                // Extract session ID from filename
                let session_id = session_path
                    .file_stem()
                    .and_then(|s| s.to_str())
                    .unwrap_or("")
                    .to_string();

                // Skip agent-spawned sessions - they pollute title matching
                // (their summaries often match main window titles)
                if session_id.starts_with("agent-") {
                    continue;
                }

                // Get summaries from file (quick scan of first lines)
                if let Ok(info) = get_session_info(&session_path) {
                    for summary in info.summaries {
                        index.push(SummaryEntry {
                            summary: summary.summary,
                            session_id: session_id.clone(),
                        });
                    }
                }
            }
        }

        self.summary_index = index;
        Ok(())
    }

    /// Rebuild fingerprint index from ~/.claude/projects
    /// Scans 100 most recent sessions by file modification time
    /// Debounced to avoid excessive rebuilds when multiple JSONL files change rapidly
    pub fn rebuild_fingerprint_index(&mut self) -> Result<()> {
        use crate::claude_storage::{list_projects, list_sessions};

        // Debounce: skip rebuild if last rebuild was less than 2 seconds ago
        const REBUILD_DEBOUNCE: Duration = Duration::from_secs(2);
        if self.last_fingerprint_rebuild.elapsed() < REBUILD_DEBOUNCE {
            return Ok(());
        }

        let projects_dir = claude_base().join("projects");
        if !projects_dir.exists() {
            return Ok(());
        }

        // Collect all session files with modification times
        // Filter out agent-* sessions (subagents shouldn't match main windows)
        let mut session_files: Vec<(PathBuf, std::time::SystemTime)> = Vec::new();

        for project_dir in list_projects()? {
            for session_path in list_sessions(&project_dir)? {
                // Skip agent-spawned sessions - they're subagents, not main interactive sessions
                if let Some(stem) = session_path.file_stem().and_then(|s| s.to_str()) {
                    if stem.starts_with("agent-") {
                        continue;
                    }
                }
                if let Ok(meta) = std::fs::metadata(&session_path) {
                    if let Ok(mtime) = meta.modified() {
                        session_files.push((session_path, mtime));
                    }
                }
            }
        }

        // Sort by modification time (newest first) and take 100
        session_files.sort_by(|a, b| b.1.cmp(&a.1));
        session_files.truncate(100);

        // Build index
        let mut index = HashMap::new();
        for (path, _) in session_files {
            if let Some(session_id) = path.file_stem().and_then(|s| s.to_str()) {
                if let Ok(mut fp) = extract_from_jsonl(&path) {
                    fp.session_id = Some(session_id.to_string());
                    index.insert(session_id.to_string(), fp);
                }
            }
        }

        self.fingerprint_index = index;
        self.last_fingerprint_rebuild = Instant::now();
        tracing::info!(session_count = self.fingerprint_index.len(), "Rebuilt fingerprint index");
        Ok(())
    }

    /// Try to match a window to a session using fingerprints
    /// Called when title matching fails
    /// Returns (session_id, confidence, fingerprint) if match found with confidence >= Medium
    pub fn fingerprint_match(&mut self, kitty_id: u64) -> Option<(String, MatchConfidence, SessionFingerprint)> {
        trace!("fingerprint_match({}) - index has {} sessions", kitty_id, self.fingerprint_index.len());

        // Get scrollback and extract fingerprint
        let scrollback = match get_scrollback(kitty_id) {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!(kitty_id, error = %e, "Failed to get scrollback");
                return None;
            }
        };

        trace!("  scrollback: {} bytes, {} lines", scrollback.len(), scrollback.lines().count());

        let window_fp = extract_from_scrollback(&scrollback);
        trace!("  extracted: first_prompt={:?}, prompts={}, tools={:?}, cwd={:?}",
            window_fp.first_prompt.as_ref().map(|s| &s[..s.len().min(40)]),
            window_fp.recent_prompts.len(),
            window_fp.tool_sequence,
            window_fp.cwd
        );

        // Cache the fingerprint
        self.window_fingerprints.insert(kitty_id, window_fp.clone());

        // Find best match in index
        let mut best_match: Option<(String, MatchConfidence)> = None;
        let mut top_matches: Vec<(String, MatchConfidence)> = Vec::new();

        for (session_id, session_fp) in &self.fingerprint_index {
            let confidence = match_fingerprints(&window_fp, session_fp);

            if confidence > MatchConfidence::None {
                top_matches.push((session_id.clone(), confidence));
            }

            if confidence >= MatchConfidence::Medium {
                if let Some((_, best_conf)) = &best_match {
                    if confidence > *best_conf {
                        best_match = Some((session_id.clone(), confidence));
                    }
                } else {
                    best_match = Some((session_id.clone(), confidence));
                }
            }
        }

        // Sort and show top matches in trace
        top_matches.sort_by(|a, b| b.1.cmp(&a.1));
        if !top_matches.is_empty() {
            trace!("  top matches:");
            for (sid, conf) in top_matches.iter().take(5) {
                trace!("    {:?}: {}", conf, sid);
            }
        } else {
            trace!("  no matches above None confidence");
        }

        if let Some((session_id, confidence)) = best_match {
            tracing::info!(
                kitty_id,
                session_id,
                ?confidence,
                "Fingerprint matched window to session"
            );
            Some((session_id, confidence, window_fp))
        } else {
            trace!("  no match >= Medium confidence");
            None
        }
    }
}

// ═══════════════════════════════════════════════════════════════════════════════
// Event Types
// ═══════════════════════════════════════════════════════════════════════════════

/// Internal daemon events
enum DaemonEvent {
    /// Kitty poll tick
    KittyPoll,
    /// File system change in ~/.claude
    FileChange(PathBuf),
    /// Shutdown signal
    Shutdown,
}

// ═══════════════════════════════════════════════════════════════════════════════
// Daemon Entry Point
// ═══════════════════════════════════════════════════════════════════════════════

/// Run the daemon with trace mode enabled
/// This enables debug-level logging for detailed fingerprint matching traces
pub async fn run_daemon_traced() -> Result<()> {
    TRACE.store(true, Ordering::Relaxed);
    tracing::info!("Trace mode enabled - debug logging activated");
    run_daemon().await
}

/// Run the daemon
pub async fn run_daemon() -> Result<()> {
    // ─── Startup Banner ─────────────────────────────────────────────────────────
    tracing::info!("babel v{}", env!("CARGO_PKG_VERSION"));

    // Initialize state
    let state = Arc::new(RwLock::new(DaemonState::new()));

    // Initialize workspace summarizer
    let summarizer = Arc::new(crate::summarizer::WorkspaceSummarizer::new());

    // ─── Initial Indexing ───────────────────────────────────────────────────────
    {
        let mut s = state.write().await;
        s.rebuild_summary_index().context("Failed to build summary index")?;
        s.rebuild_fingerprint_index().context("Failed to build fingerprint index")?;
        let _ = s.refresh_windows().context("Failed initial window scan")?;

        // Compute meaningful stats
        let sessions_with_fingerprints = s.fingerprint_index.len();
        let total_summaries = s.summary_index.len();
        let windows_found = s.windows.len();
        let windows_identified = s.windows.values().filter(|w| w.session_id.is_some()).count();
        let workspaces_active: std::collections::HashSet<_> = s.windows.values()
            .filter_map(|w| w.workspace)
            .collect();

        // Log startup state - include key numbers in message for journald visibility
        if windows_found > 0 {
            tracing::info!(
                "Discovered {} windows ({} identified) across {} workspaces",
                windows_found, windows_identified, workspaces_active.len()
            );
        }

        tracing::info!(
            "Indexed {} sessions ({} with fingerprints)",
            total_summaries, sessions_with_fingerprints
        );
    }

    // Create event channel
    let (event_tx, mut event_rx) = mpsc::channel::<DaemonEvent>(100);

    // Spawn kitty poller
    let poll_tx = event_tx.clone();
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_millis(500));
        loop {
            interval.tick().await;
            if poll_tx.send(DaemonEvent::KittyPoll).await.is_err() {
                break;
            }
        }
    });

    // Spawn file watcher
    let watch_tx = event_tx.clone();
    let projects_dir = claude_base().join("projects");
    if projects_dir.exists() {
        // Capture runtime handle before spawning thread
        let rt = tokio::runtime::Handle::current();
        std::thread::spawn(move || {
            let (tx, rx) = std::sync::mpsc::channel();

            let mut debouncer = new_debouncer(Duration::from_millis(500), tx).unwrap();
            debouncer
                .watcher()
                .watch(&projects_dir, RecursiveMode::Recursive)
                .unwrap();

            tracing::info!(path = %projects_dir.display(), "Watching sessions");

            for result in rx {
                match result {
                    Ok(events) => {
                        for event in events {
                            if event.kind == DebouncedEventKind::Any {
                                let _ = rt.block_on(
                                    watch_tx.send(DaemonEvent::FileChange(event.path))
                                );
                            }
                        }
                    }
                    Err(e) => tracing::error!(error = ?e, "File watcher error"),
                }
            }
        });
    }

    // Spawn signal handler
    let signal_tx = event_tx.clone();
    tokio::spawn(async move {
        tokio::signal::ctrl_c().await.ok();
        let _ = signal_tx.send(DaemonEvent::Shutdown).await;
    });

    // ─── IPC Socket ──────────────────────────────────────────────────────────────
    let listener = create_listener().await?;
    let socket_path = crate::ipc::socket_path();
    tracing::info!(socket = %socket_path.display(), "IPC listening");

    // ─── Ready ──────────────────────────────────────────────────────────────────
    tracing::info!("Ready");

    // Main event loop
    loop {
        tokio::select! {
            // Handle IPC connections
            Ok((stream, _)) = listener.accept() => {
                let state = Arc::clone(&state);
                let summarizer = Arc::clone(&summarizer);
                tokio::spawn(async move {
                    if let Err(e) = handle_client(stream, state, summarizer).await {
                        tracing::warn!(error = ?e, "IPC client error");
                    }
                });
            }

            // Handle internal events
            Some(event) = event_rx.recv() => {
                match event {
                    DaemonEvent::KittyPoll => {
                        let changed_workspaces = {
                            let mut s = state.write().await;
                            match s.refresh_windows() {
                                Ok(ws) => ws,
                                Err(e) => {
                                    tracing::warn!(error = ?e, "Window refresh failed");
                                    vec![]
                                }
                            }
                        };

                        // Spawn summarization for changed workspaces
                        // Always run - summarizer falls back to project names when API key isn't set
                        if !changed_workspaces.is_empty() {
                            let summarizer = Arc::clone(&summarizer);
                            let state = Arc::clone(&state);
                            tokio::spawn(async move {
                                for ws in changed_workspaces {
                                    summarize_workspace(ws, &state, &summarizer).await;
                                }
                            });
                        }
                    }
                    DaemonEvent::FileChange(path) => {
                        // Rebuild fingerprint index on JSONL changes
                        // Note: This could be optimized with a last_rebuild timestamp check
                        if path.extension().and_then(|s| s.to_str()) == Some("jsonl") {
                            let mut s = state.write().await;
                            let _ = s.rebuild_fingerprint_index();
                        }
                    }
                    DaemonEvent::Shutdown => {
                        tracing::info!("Received shutdown signal");
                        break;
                    }
                }
            }
        }
    }

    // Cleanup socket
    let _ = std::fs::remove_file(crate::ipc::socket_path());
    Ok(())
}

// ═══════════════════════════════════════════════════════════════════════════════
// Workspace Summarization
// ═══════════════════════════════════════════════════════════════════════════════

/// Summarize Claude sessions on a workspace and emit WorkspaceTitleUpdated event
///
/// Called async after window changes to generate human-readable workspace titles
/// like "refactoring auth system" via Haiku.
async fn summarize_workspace(
    workspace: i32,
    state: &Arc<RwLock<DaemonState>>,
    summarizer: &Arc<crate::summarizer::WorkspaceSummarizer>,
) {
    use crate::summarizer::SessionSummaryInput;

    // Gather session info for this workspace (release lock before async call)
    let (sessions, window_count, primary_session) = {
        let s = state.read().await;
        let windows: Vec<_> = s.windows.values()
            .filter(|w| w.workspace == Some(workspace))
            .collect();

        if windows.is_empty() {
            return;
        }

        let sessions: Vec<SessionSummaryInput> = windows.iter().map(|w| {
            SessionSummaryInput {
                project_path: w.cwd.to_string_lossy().to_string(),
                recent_activity: None, // Could extract from scrollback later
                window_title: Some(w.title.clone()),
            }
        }).collect();

        let window_count = windows.len();
        let primary_session = windows.iter()
            .find(|w| w.is_focused)
            .or(windows.first())
            .and_then(|w| w.session_id.clone());

        (sessions, window_count, primary_session)
    };

    // Call summarizer (async HTTP to Haiku) - lock released during this call
    let title = match summarizer.summarize(workspace, sessions).await {
        Ok(t) if !t.is_empty() => t,
        Ok(_) => return, // Empty title, skip event
        Err(e) => {
            tracing::warn!(workspace, error = %e, "Workspace summarization failed");
            return;
        }
    };

    // Re-acquire lock to store title and publish event
    {
        let mut s = state.write().await;
        trace!("Workspace {} title: {}", workspace, title);

        // Store in authoritative state (frontends query this)
        s.workspace_titles.insert(workspace, title.clone());

        s.event_publisher.publish(BabelEvent::WorkspaceTitleUpdated {
            workspace,
            title,
            window_count,
            primary_session,
        });
    }
}

// ═══════════════════════════════════════════════════════════════════════════════
// Client Handler
// ═══════════════════════════════════════════════════════════════════════════════

/// Handle a subscriber connection (long-lived, streams events)
async fn handle_subscriber(
    mut stream: UnixStream,
    mut rx: tokio::sync::broadcast::Receiver<EventMessage>,
    filter: EventFilter,
    subscriber_id: u64,
) -> Result<()> {
    // Send subscription acknowledgment
    let ack = Response::Subscribed { subscriber_id };
    let mut ack_json = serde_json::to_string(&ack)?;
    ack_json.push('\n');
    stream.write_all(ack_json.as_bytes()).await?;

    // Stream events to subscriber
    loop {
        match rx.recv().await {
            Ok(msg) => {
                if filter.matches(&msg.event) {
                    let response = Response::Event { event: msg };
                    let mut json = serde_json::to_string(&response)?;
                    json.push('\n');

                    if let Err(e) = stream.write_all(json.as_bytes()).await {
                        tracing::debug!(subscriber_id, error = %e, "Subscriber disconnected");
                        break;
                    }
                }
            }
            Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                tracing::warn!(subscriber_id, dropped_events = n, "Subscriber lagged");
            }
            Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                tracing::error!("Event broadcast channel closed");
                break;
            }
        }
    }

    Ok(())
}

async fn handle_client(
    mut stream: UnixStream,
    state: Arc<RwLock<DaemonState>>,
    summarizer: Arc<crate::summarizer::WorkspaceSummarizer>,
) -> Result<()> {
    let mut reader = BufReader::new(&mut stream);
    let mut line = String::new();

    // Read one request
    let bytes_read = reader.read_line(&mut line).await?;
    if bytes_read == 0 {
        return Ok(()); // Connection closed
    }

    let request: Request = serde_json::from_str(&line)
        .context("Failed to parse client request")?;

    // Handle Subscribe specially - needs long-lived connection
    if let Request::Subscribe { events } = request {
        let s = state.read().await;
        let rx = s.event_publisher.subscribe();
        let subscriber_id = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);
        let filter = EventFilter::with_events(events);
        drop(s);
        drop(reader);

        return handle_subscriber(stream, rx, filter, subscriber_id).await;
    }

    let response = process_request(request, &state, &summarizer).await;

    // Write response
    drop(reader); // Release borrow
    let mut response_json = serde_json::to_string(&response)?;
    response_json.push('\n');
    stream.write_all(response_json.as_bytes()).await?;

    Ok(())
}

async fn process_request(
    request: Request,
    state: &Arc<RwLock<DaemonState>>,
    summarizer: &Arc<crate::summarizer::WorkspaceSummarizer>,
) -> Response {
    match request {
        Request::List => {
            let s = state.read().await;
            let windows: Vec<ClaudeWindow> = s.windows.values().cloned().collect();
            Response::Windows { windows }
        }

        Request::ListWithFingerprints => {
            let s = state.read().await;
            let mut windows: Vec<ClaudeWindow> = s.windows.values().cloned().collect();
            drop(s); // Release lock before expensive operations

            // Extract fingerprints and enrich with session info for each window
            for win in &mut windows {
                // Extract fingerprint if not already cached
                if win.fingerprint.is_none() {
                    if let Ok(scrollback) = get_scrollback(win.kitty_id) {
                        let fp = extract_from_scrollback(&scrollback);
                        win.fingerprint = Some(fp);
                    }
                }

                // Enrich with session info if we have a session_id
                if win.session_info.is_none() {
                    let _ = enrich_window(win);
                }
            }

            Response::Windows { windows }
        }

        Request::Status { window_id } => {
            let s = state.read().await;
            let window = if let Some(id) = window_id {
                s.windows.get(&id).cloned()
            } else {
                s.windows.values().find(|w| w.is_focused).cloned()
            };
            Response::Window { window: Box::new(window) }
        }

        Request::Enrich { window_id } => {
            let mut s = state.write().await;
            if let Some(window) = s.windows.get_mut(&window_id) {
                if let Err(e) = enrich_window(window) {
                    return Response::Error {
                        message: format!("Failed to enrich: {}", e),
                    };
                }
                Response::Window {
                    window: Box::new(Some(window.clone())),
                }
            } else {
                Response::Error {
                    message: "Window not found".to_string(),
                }
            }
        }

        Request::Focus { window_id } => match focus_window(window_id) {
            Ok(()) => Response::Ok {
                message: format!("Focused window {}", window_id),
            },
            Err(e) => Response::Error {
                message: format!("Focus failed: {}", e),
            },
        },

        Request::Scroll { window_id } => match get_scrollback(window_id) {
            Ok(text) => Response::Scrollback { text },
            Err(e) => Response::Error {
                message: format!("Scroll failed: {}", e),
            },
        },

        Request::Send { window_id, text } => match send_text(window_id, &text) {
            Ok(()) => Response::Ok {
                message: format!("Sent to window {}", window_id),
            },
            Err(e) => Response::Error {
                message: format!("Send failed: {}", e),
            },
        },

        Request::Tag { window_id, icon } => {
            let s = state.read().await;
            if let Some(window) = s.windows.get(&window_id) {
                if let Some(session_id) = &window.session_id {
                    match init_db().and_then(|conn| set_icon(&conn, session_id, &icon)) {
                        Ok(()) => Response::Ok {
                            message: format!("Tagged {} with {}", window_id, icon),
                        },
                        Err(e) => Response::Error {
                            message: format!("Tag failed: {}", e),
                        },
                    }
                } else {
                    Response::Error {
                        message: "Window has no session".to_string(),
                    }
                }
            } else {
                Response::Error {
                    message: "Window not found".to_string(),
                }
            }
        }

        Request::MarkRead { window_id } => {
            let s = state.read().await;
            if let Some(window) = s.windows.get(&window_id) {
                if let Some(session_id) = &window.session_id {
                    match init_db().and_then(|conn| mark_read(&conn, session_id)) {
                        Ok(()) => Response::Ok {
                            message: format!("Marked {} as read", window_id),
                        },
                        Err(e) => Response::Error {
                            message: format!("Mark read failed: {}", e),
                        },
                    }
                } else {
                    Response::Error {
                        message: "Window has no session".to_string(),
                    }
                }
            } else {
                Response::Error {
                    message: "Window not found".to_string(),
                }
            }
        }

        Request::History { limit } => match get_recent_sessions(limit) {
            Ok(sessions) => Response::History { sessions },
            Err(e) => Response::Error {
                message: format!("History failed: {}", e),
            },
        },

        Request::Ping => {
            let s = state.read().await;
            Response::Pong {
                uptime_secs: s.start_time.elapsed().as_secs(),
            }
        }

        Request::Shutdown => Response::Ok {
            message: "Shutting down".to_string(),
        },

        Request::Refresh => {
            let mut s = state.write().await;
            match s.refresh_windows() {
                Ok(_changed) => Response::Ok {
                    message: format!("Refreshed {} windows", s.windows.len()),
                },
                Err(e) => Response::Error {
                    message: format!("Refresh failed: {}", e),
                }
            }
        }

        Request::Titles => {
            let s = state.read().await;
            // Convert i32 keys to strings for JSON compatibility
            let titles: std::collections::HashMap<String, String> = s.workspace_titles
                .iter()
                .map(|(k, v)| (k.to_string(), v.clone()))
                .collect();
            Response::Titles { titles }
        }

        Request::TitleRefresh { workspace } => {
            // Get workspaces to refresh
            let workspaces_to_refresh: Vec<i32> = {
                let s = state.read().await;
                if let Some(ws) = workspace {
                    vec![ws]
                } else {
                    // All unique workspaces with Claude windows
                    s.windows.values()
                        .filter_map(|w| w.workspace)
                        .collect::<std::collections::HashSet<_>>()
                        .into_iter()
                        .collect()
                }
            };

            // Invalidate summarizer cache
            if let Some(ws) = workspace {
                summarizer.invalidate(ws).await;
            } else {
                summarizer.clear_cache().await;
            }

            // Regenerate titles synchronously
            for ws in &workspaces_to_refresh {
                summarize_workspace(*ws, state, summarizer).await;
            }

            // Return updated titles
            let s = state.read().await;
            let titles = if let Some(ws) = workspace {
                s.workspace_titles.get(&ws).cloned()
                    .map(|t| format!("Workspace {}: {}", ws, t))
                    .unwrap_or_else(|| format!("Workspace {}: (no windows)", ws))
            } else {
                workspaces_to_refresh.iter()
                    .filter_map(|ws| s.workspace_titles.get(ws).map(|t| format!("  {}: {}", ws, t)))
                    .collect::<Vec<_>>()
                    .join("\n")
            };

            Response::Ok { message: format!("Refreshed titles:\n{}", titles) }
        }

        // ─── WSet Operations ────────────────────────────────────────────────────

        Request::WSetSave { name } => {
            // Determine name: provided or current or "default"
            let wset_name = match name {
                Some(n) => n,
                None => get_current_wset_name().ok().flatten().unwrap_or_else(|| "default".to_string()),
            };

            // Build WSet from current daemon state
            let s = state.read().await;
            let mut wset = WSet::from_daemon_state(&wset_name, &s);
            drop(s);

            // Save to disk
            match wset.save() {
                Ok(_) => {
                    let wspaces = wset.wspaces.len();
                    let windows = wset.window_count();

                    // Update _current
                    if let Err(e) = set_current_wset_name(&wset_name) {
                        tracing::warn!(error = %e, "Failed to set current wset name");
                    }

                    Response::WSetSaved { name: wset_name, wspaces, windows }
                }
                Err(e) => Response::Error {
                    message: format!("Failed to save WSet: {}", e),
                },
            }
        }

        Request::WSetLoad { name, dry_run } => {
            // Determine name: provided or current
            let wset_name = match name {
                Some(n) => n,
                None => match get_current_wset_name() {
                    Ok(Some(n)) => n,
                    Ok(None) => return Response::Error {
                        message: "No current WSet. Specify a name or run 'babel save' first.".to_string(),
                    },
                    Err(e) => return Response::Error {
                        message: format!("Failed to read current WSet: {}", e),
                    },
                },
            };

            // Load WSet from disk
            let wset = match WSet::load(&wset_name) {
                Ok(w) => w,
                Err(e) => return Response::Error {
                    message: format!("Failed to load WSet '{}': {}", wset_name, e),
                },
            };

            let wspaces = wset.wspaces.len();
            let windows = wset.window_count();

            if dry_run {
                // Just return what would happen
                return Response::WSetLoaded {
                    name: wset_name,
                    wspaces,
                    windows,
                    skipped: vec![],
                    dry_run: true,
                };
            }

            // Actually load: close existing windows and spawn new ones
            // This is handled by the kitty module's spawn functions
            let skipped = match crate::kitty::load_wset(&wset).await {
                Ok(s) => s,
                Err(e) => return Response::Error {
                    message: format!("Failed to load WSet: {}", e),
                },
            };

            // Update _current
            if let Err(e) = set_current_wset_name(&wset_name) {
                tracing::warn!(error = %e, "Failed to set current wset name");
            }

            // Trigger window refresh to pick up new windows
            {
                let mut s = state.write().await;
                let _ = s.refresh_windows();
            }

            Response::WSetLoaded {
                name: wset_name,
                wspaces,
                windows,
                skipped,
                dry_run: false,
            }
        }

        Request::WSetList => {
            match list_wsets() {
                Ok(wsets) => {
                    let current = get_current_wset_name().ok().flatten();
                    Response::WSetList { wsets, current }
                }
                Err(e) => Response::Error {
                    message: format!("Failed to list WSet files: {}", e),
                },
            }
        }

        Request::WSetCurrent => {
            match get_current_wset_name() {
                Ok(name) => Response::WSetCurrent { name },
                Err(e) => Response::Error {
                    message: format!("Failed to get current WSet: {}", e),
                },
            }
        }

        Request::WSetDelete { name } => {
            match WSet::delete(&name) {
                Ok(()) => Response::Ok {
                    message: format!("Deleted WSet '{}'", name),
                },
                Err(e) => Response::Error {
                    message: format!("Failed to delete WSet '{}': {}", name, e),
                },
            }
        }

        Request::WSetRename { old, new } => {
            match WSet::rename(&old, &new) {
                Ok(()) => Response::Ok {
                    message: format!("Renamed WSet '{}' to '{}'", old, new),
                },
                Err(e) => Response::Error {
                    message: format!("Failed to rename WSet: {}", e),
                },
            }
        }

        Request::WSetDescribe { name, description } => {
            // Load, update description, save
            match WSet::load(&name) {
                Ok(mut wset) => {
                    wset.meta.description = description.clone();
                    match wset.save() {
                        Ok(_) => {
                            let desc = description.unwrap_or_else(|| "(cleared)".to_string());
                            Response::Ok {
                                message: format!("Set description for '{}': {}", name, desc),
                            }
                        }
                        Err(e) => Response::Error {
                            message: format!("Failed to save WSet: {}", e),
                        },
                    }
                }
                Err(e) => Response::Error {
                    message: format!("Failed to load WSet '{}': {}", name, e),
                },
            }
        }

        // Subscribe is handled specially in handle_client before reaching process_request
        Request::Subscribe { .. } => Response::Error {
            message: "Subscribe requests must be handled via handle_client".to_string(),
        }
    }
}
