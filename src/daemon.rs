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

// ═══════════════════════════════════════════════════════════════════════════════
// Configuration Constants
// ═══════════════════════════════════════════════════════════════════════════════
//
// Tuning knobs for daemon behavior. Collected here for visibility and easy adjustment.

mod config {
    use std::time::Duration;

    /// Interval between kitty window polls (500ms = 2 Hz)
    ///
    /// Balance between responsiveness and CPU usage. Lower values catch
    /// window changes faster but increase polling overhead.
    pub const KITTY_POLL_INTERVAL: Duration = Duration::from_millis(500);

    /// Debounce interval for file watcher events
    ///
    /// When multiple files change rapidly (e.g., Claude writing to JSONL),
    /// coalesce events to avoid redundant index rebuilds.
    pub const FILE_WATCH_DEBOUNCE: Duration = Duration::from_millis(500);

    /// Minimum time between fingerprint index rebuilds
    ///
    /// Fingerprint extraction is expensive (reads 100 JSONL files).
    /// This debounce prevents thrashing when many sessions change.
    pub const FINGERPRINT_REBUILD_DEBOUNCE: Duration = Duration::from_secs(2);

    /// Maximum sessions in fingerprint index
    ///
    /// Limited to N most recent to bound memory usage and matching time.
    /// Older sessions fall out of the index but remain on disk.
    pub const FINGERPRINT_INDEX_LIMIT: usize = 100;

    /// Maximum cached window fingerprints
    ///
    /// Safety net to prevent unbounded growth. Should rarely trigger
    /// since we clean up on window removal.
    pub const FINGERPRINT_CACHE_LIMIT: usize = 100;

    /// Internal event channel capacity
    ///
    /// Buffer size for daemon events (KittyPoll, FileChange, Shutdown).
    /// If full, senders block - should be generous to avoid backpressure.
    pub const EVENT_CHANNEL_SIZE: usize = 100;
}

use anyhow::{Context, Result};
use notify::RecursiveMode;
use notify_debouncer_mini::{new_debouncer, DebouncedEventKind};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tokio::sync::{mpsc, RwLock};

use crate::utility::claude_storage::{claude_base, get_recent_sessions, get_session_info};
use crate::utility::claude_discovery::{enrich_window, load_wset, ClaudeWindow};
use crate::events::{BabelEvent, EventFilter, EventMessage, EventPublisher};
use crate::kitty::PaneAddr;
use crate::fingerprint::{
	SessionFingerprint, MatchConfidence,
	extract_from_scrollback, extract_from_jsonl, match_fingerprints,
};
use crate::utility::ipc::{create_listener, Request, Response};
use crate::kitty::{
    get_scrollback,  // used in fingerprint_match_addr
    focus_window_any, get_scrollback_any, send_text_any,
    list_all_panes, discover_all_instances, default_socket,
};
use crate::utility::claude_discovery::{get_window_activity_state, detect_claude_signals};
use crate::babel_storage::{init_db, mark_read, mark_unread, set_icon};
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
// Babel State
// ═══════════════════════════════════════════════════════════════════════════════

/// Summary entry for fast matching
#[derive(Debug, Clone)]
pub struct SummaryEntry {
	summary: String,
	session_id: String,
}

/// Lightweight terminal info for tracking all kitty windows (not just Claude)
///
/// This enables the TUI/monitor to see the full terminal flow - watching
/// terminals open, close, and transition to Claude sessions.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct TerminalInfo {
	/// Unique address of this pane (socket + kitty ID)
	pub addr: PaneAddr,
	/// Window title
	pub title: String,
	/// Working directory
	pub cwd: PathBuf,
	/// XFCE workspace number
	pub workspace: Option<i32>,
	/// Whether this terminal is running Claude (is_claude from detect_claude_signals)
	pub is_claude: bool,
	/// Whether this is the focused window
	pub is_focused: bool,
}

impl TerminalInfo {
	/// Get the kitty window ID (convenience)
	pub fn id(&self) -> u64 {
		self.addr.id
	}
}

/// Status of a kitty socket
///
/// Used to track which sockets are responsive and emit warnings
/// when multiple kitty instances are detected.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct SocketStatus {
	/// Whether this is the current/default socket
	pub is_current: bool,
	/// Whether the socket responded to queries
	pub is_responsive: bool,
	/// Number of panes in this instance
	pub pane_count: usize,
	/// Last error message if any
	pub last_error: Option<String>,
}

/// Babel state - shared across tasks
///
/// All window/terminal state is keyed by PaneAddr (socket + id) to support
/// multiple kitty instances. This enables graceful degradation when things
/// are "fucked" - multiple instances, dead sockets, etc.
pub struct BabelState {
	/// Current Claude windows (PaneAddr → ClaudeWindow)
	/// Keyed by PaneAddr to handle ID collisions across kitty instances
	pub windows: HashMap<PaneAddr, ClaudeWindow>,

	/// All kitty terminals (PaneAddr → TerminalInfo)
	/// Includes both Claude and non-Claude terminals for full visibility
	pub terminals: HashMap<PaneAddr, TerminalInfo>,

	/// Summary index for fast title→session matching
	pub summary_index: Vec<SummaryEntry>,

	/// Fingerprint index for matching (session_id → fingerprint)
	/// Limited to 100 most recent sessions for performance
	pub fingerprint_index: HashMap<String, SessionFingerprint>,

	/// Session path cache (session_id → .jsonl path)
	/// Populated during summary index rebuild for O(1) session lookup
	pub session_paths: HashMap<String, PathBuf>,

	/// Cached fingerprints for windows (PaneAddr → fingerprint)
	/// Extracted from scrollback, used for matching
	pub window_fingerprints: HashMap<PaneAddr, SessionFingerprint>,

	/// Cached activity states for windows (PaneAddr → ActivityState)
	/// Used to detect state changes and emit SessionStateChanged events
	pub window_states: HashMap<PaneAddr, scrollparse::claude::ActivityState>,

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

	/// Known kitty sockets and their status
	/// Populated during window refresh to track multi-socket situations
	pub socket_status: HashMap<String, SocketStatus>,
}

impl Default for BabelState {
	fn default() -> Self {
		Self::new()
	}
}

impl BabelState {
	pub fn new() -> Self {
		Self {
			windows: HashMap::new(),
			terminals: HashMap::new(),
			summary_index: Vec::new(),
			fingerprint_index: HashMap::new(),
			session_paths: HashMap::new(),
			window_fingerprints: HashMap::new(),
			window_states: HashMap::new(),
			start_time: Instant::now(),
			last_kitty_scan: Instant::now(),
			last_fingerprint_rebuild: Instant::now(),
			event_publisher: EventPublisher::new(),
			workspace_titles: HashMap::new(),
			socket_status: HashMap::new(),
		}
	}

	/// Refresh kitty windows from ALL sockets
	///
	/// Returns list of workspaces that had windows added or removed,
	/// for triggering title re-summarization.
	///
	/// Multi-socket support: polls all discovered kitty instances (not just default).
	/// This enables graceful degradation when multiple instances exist.
	pub fn refresh_windows(&mut self) -> Result<Vec<i32>> {
		use crate::kitty::get_all_workspaces;

		// ─── Multi-Socket Discovery ─────────────────────────────────────────────────
		// Query all kitty instances, update socket_status
		let instances = discover_all_instances();
		let current_socket = default_socket();

		self.socket_status = instances.iter()
			.map(|i| (i.socket.clone(), SocketStatus {
				is_current: i.socket == current_socket,
				is_responsive: i.is_responsive,
				pane_count: i.panes.len(),
				last_error: i.error.clone(),
			}))
			.collect();

		// Get ALL kitty panes from ALL sockets
		let all_panes = list_all_panes()?;

		// Filter to just Claude windows for the main tracking
		let kitty_windows: Vec<_> = all_panes.iter()
			.filter(|p| detect_claude_signals(p).is_claude())
			.cloned()
			.collect();

		// Get workspace mappings in one call
		let workspaces = get_all_workspaces();

		// ─── Terminal Tracking (ALL kitty windows from ALL sockets) ─────────────────
		// Build terminal info for all panes and emit terminal events
		let mut new_terminals: HashMap<PaneAddr, TerminalInfo> = HashMap::new();
		let old_terminal_addrs: std::collections::HashSet<_> = self.terminals.keys().cloned().collect();

		for pane in &all_panes {
			let addr = pane.addr();
			let workspace = workspaces.get(&pane.platform_window_id).copied();
			let is_claude = detect_claude_signals(pane).is_claude();

			new_terminals.insert(addr.clone(), TerminalInfo {
				addr,
				title: pane.title.clone(),
				cwd: pane.cwd.clone(),
				workspace,
				is_claude,
				is_focused: pane.is_focused,
			});
		}

		let new_terminal_addrs: std::collections::HashSet<_> = new_terminals.keys().cloned().collect();

		// Emit TerminalOpened events for new terminals
		for addr in new_terminal_addrs.difference(&old_terminal_addrs) {
			if let Some(t) = new_terminals.get(addr) {
				self.event_publisher.publish(BabelEvent::TerminalOpened {
					kitty_id: addr.id,
					title: t.title.clone(),
					cwd: t.cwd.clone(),
					workspace: t.workspace,
				});
			}
		}

		// Emit TerminalClosed events for removed terminals
		for addr in old_terminal_addrs.difference(&new_terminal_addrs) {
			self.event_publisher.publish(BabelEvent::TerminalClosed { kitty_id: addr.id });
		}

		// Emit TerminalBecameClaude for terminals that just became Claude sessions
		for (addr, new_term) in &new_terminals {
			if let Some(old_term) = self.terminals.get(addr) {
				if !old_term.is_claude && new_term.is_claude {
					self.event_publisher.publish(BabelEvent::TerminalBecameClaude {
						kitty_id: addr.id,
						title: new_term.title.clone(),
					});
				}
			}
		}

		self.terminals = new_terminals;

		// ─── Claude Window Tracking ─────────────────────────────────────────────────
		// Build new windows map, preserving enriched data where possible
		let mut new_windows: HashMap<PaneAddr, ClaudeWindow> = HashMap::new();

		for kw in kitty_windows {
			let addr = kw.addr();
			let workspace = workspaces.get(&kw.platform_window_id).copied();

			// Check if we have existing data for this window (use get, not remove)
			let mut claude_window = if let Some(existing) = self.windows.get(&addr) {
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
					addr: addr.clone(),
					title: kw.title.clone(),
					session_id: existing_session,
					session_info: None,
					cwd: kw.cwd.clone(),
					is_focused: kw.is_focused,
					os_window_id: kw.os_window_id,
					platform_window_id: kw.platform_window_id,
					workspace,
					activity_state: None, // Will be populated from window_states cache
					fingerprint: None,
					match_confidence: None,
				}
			};

			// Try to match unmatched windows using summary index
			if claude_window.session_id.is_none() {
				trace!("Window {} needs matching (title: {})", addr.short(), claude_window.title);

				if let Some(session_id) = self.match_title_to_session(&claude_window.title) {
					trace!("  → Title matched to session: {}", session_id);
					claude_window.session_id = Some(session_id);
				} else {
					trace!("  → Title match failed, will defer fingerprinting");
				}
			}

			new_windows.insert(addr, claude_window);
		}

		// Detect and emit events for window changes
		let old_addrs: std::collections::HashSet<_> = self.windows.keys().cloned().collect();
		let new_addrs: std::collections::HashSet<_> = new_windows.keys().cloned().collect();

		// Track workspaces that need re-summarization
		let mut changed_workspaces: std::collections::HashSet<i32> = std::collections::HashSet::new();

		// Windows added
		for addr in new_addrs.difference(&old_addrs) {
			if let Some(w) = new_windows.get(addr) {
				self.event_publisher.publish(BabelEvent::WindowAdded {
					kitty_id: addr.id,
					title: w.title.clone(),
					workspace: w.workspace,
				});
				// Track workspace for re-summarization
				if let Some(ws) = w.workspace {
					changed_workspaces.insert(ws);
				}
			}
		}

		// Windows removed - clean up cached fingerprints and states
		for addr in old_addrs.difference(&new_addrs) {
			// Get workspace from old windows before removal
			if let Some(w) = self.windows.get(addr) {
				if let Some(ws) = w.workspace {
					changed_workspaces.insert(ws);
				}
			}

			// Clean up cached data for closed window
			self.window_fingerprints.remove(addr);
			self.window_states.remove(addr);

			self.event_publisher.publish(BabelEvent::WindowRemoved { kitty_id: addr.id });
		}

		// Check for focus changes
		let old_focused = self.windows.values().find(|w| w.is_focused).map(|w| w.addr.clone());
		let new_focused = new_windows.values().find(|w| w.is_focused).map(|w| w.addr.clone());
		if old_focused != new_focused {
			// Emit WindowUnfocused for the window that lost focus
			if let Some(ref addr) = old_focused {
				if let Some(w) = self.windows.get(addr) {
					self.event_publisher.publish(BabelEvent::WindowUnfocused {
						kitty_id: addr.id,
						session_id: w.session_id.clone(),
					});
				}
			}
			// Emit WindowFocused for the window that gained focus
			if let Some(ref addr) = new_focused {
				if let Some(w) = new_windows.get(addr) {
					self.event_publisher.publish(BabelEvent::WindowFocused {
						kitty_id: addr.id,
						session_id: w.session_id.clone(),
					});
				}
			}
		}

		// Detect session state changes and emit events
		// This enables richspace-babel to track Claude activity per-workspace
		for (addr, window) in &new_windows {
			let new_state = get_window_activity_state(addr.id);
			let old_state = self.window_states.get(addr).copied();

			match old_state {
				Some(old) if old != new_state => {
					// State changed - emit event
					trace!("Window {} state change: {:?} -> {:?}", addr.short(), old, new_state);
					self.event_publisher.publish(BabelEvent::SessionStateChanged {
						kitty_id: addr.id,
						session_id: window.session_id.clone(),
						workspace: window.workspace,
						old_state: old,
						new_state,
					});
					self.window_states.insert(addr.clone(), new_state);

					// Auto-unread when Claude finishes working and awaits input
					// This ensures users see the yellow dot when there's new content to review
					if new_state == scrollparse::claude::ActivityState::AwaitingInput {
						if let Some(ref session_id) = window.session_id {
							if let Err(e) = init_db().and_then(|conn| mark_unread(&conn, session_id)) {
								tracing::warn!(session_id, error = %e, "Failed to auto-unread session");
							}
						}
					}
				}
				None => {
					// New window - initialize state (no event, WindowAdded already fired)
					self.window_states.insert(addr.clone(), new_state);
				}
				_ => {
					// State unchanged
				}
			}
		}

		// Clean up states for removed windows
		self.window_states.retain(|addr, _| new_windows.contains_key(addr));

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

	/// Get list of window addresses that need fingerprint matching
	///
	/// Called with read lock to identify windows that need matching.
	/// Caller then releases lock and does expensive I/O.
	pub fn get_windows_needing_fingerprints(&self) -> Vec<PaneAddr> {
		self.windows
		    .iter()
		    .filter(|(_, w)| w.session_id.is_none())
		    .map(|(addr, _)| addr.clone())
		    .collect()
	}

	/// Apply fingerprint matching results to a window
	///
	/// This applies the results from `fingerprint_match_addr` back to state.
	/// Called with write lock after I/O completes.
	pub fn apply_fingerprint_result(
		&mut self,
		addr: &PaneAddr,
		session_id: String,
		confidence: MatchConfidence,
		fingerprint: SessionFingerprint,
	) {
		// Tag the window for future fast lookups
		let _ = crate::utility::claude_discovery::tag_window(addr.id, &session_id);

		// Cache the fingerprint
		self.cache_fingerprint(addr.clone(), fingerprint.clone());

		// Update the window in our state
		if let Some(window) = self.windows.get_mut(addr) {
			window.session_id = Some(session_id);
			window.match_confidence = Some(confidence);
			window.fingerprint = Some(fingerprint);
		}
	}

	/// Perform fingerprint matching for a window address without holding any lock
	///
	/// This does the expensive I/O (get_scrollback) and matching logic.
	/// Called outside any locks to avoid blocking readers.
	///
	/// Takes fingerprint_index as parameter to avoid needing &self.
	pub fn fingerprint_match_addr(
		addr: &PaneAddr,
		fingerprint_index: &HashMap<String, SessionFingerprint>,
	) -> Option<(String, MatchConfidence, SessionFingerprint)> {
		use crate::kitty::get_scrollback_on_socket;

		trace!("fingerprint_match({}) - index has {} sessions", addr.short(), fingerprint_index.len());

		// Get scrollback using the pane's socket (EXPENSIVE I/O - done without lock)
		let scrollback = match get_scrollback_on_socket(&addr.socket, addr.id) {
			Ok(s) => s,
			Err(e) => {
				tracing::warn!(addr = %addr.short(), error = %e, "Failed to get scrollback");
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

		// Find best match in index
		let mut best_match: Option<(String, MatchConfidence)> = None;
		let mut top_matches: Vec<(String, MatchConfidence)> = Vec::new();

		for (session_id, session_fp) in fingerprint_index {
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
                addr = %addr.short(),
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

	/// Rebuild summary index from ~/.claude/projects
	pub fn rebuild_summary_index(&mut self) -> Result<()> {
		let projects_dir = claude_base().join("projects");
		if !projects_dir.exists() {
			return Ok(());
		}

		let mut index = Vec::new();
		let mut paths = HashMap::new();

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

				// Cache session path for O(1) lookup (include agent sessions)
				paths.insert(session_id.clone(), session_path.clone());

				// Skip agent-spawned sessions for summary index - they pollute title matching
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
		self.session_paths = paths;
		Ok(())
	}

	/// Rebuild fingerprint index from ~/.claude/projects
	/// Scans most recent sessions by file modification time
	/// Debounced to avoid excessive rebuilds when multiple JSONL files change rapidly
	pub fn rebuild_fingerprint_index(&mut self) -> Result<()> {
		use crate::utility::claude_storage::{list_projects, list_sessions};

		// Debounce: skip rebuild if last rebuild was too recent
		if self.last_fingerprint_rebuild.elapsed() < config::FINGERPRINT_REBUILD_DEBOUNCE {
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

		// Sort by modification time (newest first) and limit
		session_files.sort_by(|a, b| b.1.cmp(&a.1));
		session_files.truncate(config::FINGERPRINT_INDEX_LIMIT);

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

	/// Cache a fingerprint for a window
	///
	/// Called after successful fingerprint matching to avoid re-extraction
	fn cache_fingerprint(&mut self, addr: PaneAddr, fingerprint: SessionFingerprint) {
		self.window_fingerprints.insert(addr, fingerprint);

		// Safety net: enforce maximum cache size to prevent unbounded growth
		// This should rarely trigger since we clean up on window removal,
		// but protects against edge cases (e.g., fingerprint extraction spam)
		if self.window_fingerprints.len() > config::FINGERPRINT_CACHE_LIMIT {
			// Remove oldest entries (just prevent unbounded growth, not critical which ones)
			while self.window_fingerprints.len() > config::FINGERPRINT_CACHE_LIMIT {
				if let Some(key) = self.window_fingerprints.keys().next().cloned() {
					self.window_fingerprints.remove(&key);
				}
			}
		}
	}

	// ═══════════════════════════════════════════════════════════════════════════════
	// ID → PaneAddr Resolution Helpers
	// ═══════════════════════════════════════════════════════════════════════════════
	//
	// IPC requests use window_id: u64 for backwards compatibility.
	// These helpers resolve IDs to windows by searching across all sockets.

	/// Find a window by its kitty ID (searches across all sockets)
	///
	/// Returns None if no window with that ID exists in any socket
	pub fn find_window_by_id(&self, id: u64) -> Option<&ClaudeWindow> {
		self.windows.values().find(|w| w.id() == id)
	}

	/// Find a window by its kitty ID (mutable, searches across all sockets)
	pub fn find_window_by_id_mut(&mut self, id: u64) -> Option<&mut ClaudeWindow> {
		self.windows.values_mut().find(|w| w.id() == id)
	}

	/// Find the PaneAddr for a given window ID
	pub fn find_addr_by_id(&self, id: u64) -> Option<&PaneAddr> {
		self.windows.iter().find(|(_, w)| w.id() == id).map(|(addr, _)| addr)
	}

	/// Get activity state for a window by its kitty ID
	pub fn get_activity_state(&self, id: u64) -> Option<scrollparse::claude::ActivityState> {
		self.find_addr_by_id(id)
			.and_then(|addr| self.window_states.get(addr))
			.cloned()
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
	let state = Arc::new(RwLock::new(BabelState::new()));

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

		// Multi-socket warning at startup
		let socket_count = s.socket_status.len();
		if socket_count > 1 {
			tracing::warn!(
				"⚠ Multiple kitty instances detected ({} sockets)",
				socket_count
			);
			for (socket, status) in &s.socket_status {
				let marker = if status.is_current { "●" } else { "○" };
				let short = socket.rsplit("kitty.sock-").next().unwrap_or(socket);
				tracing::warn!("  {} {} ({} panes)", marker, short, status.pane_count);
			}
		}
	}

	// Create event channel
	let (event_tx, mut event_rx) = mpsc::channel::<DaemonEvent>(config::EVENT_CHANNEL_SIZE);

	// Spawn kitty poller
	let poll_tx = event_tx.clone();
	tokio::spawn(async move {
		let mut interval = tokio::time::interval(config::KITTY_POLL_INTERVAL);
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

			let mut debouncer = new_debouncer(config::FILE_WATCH_DEBOUNCE, tx).unwrap();
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
								let _ = rt.block_on(watch_tx.send(DaemonEvent::FileChange(event.path)));
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
	let socket_path = crate::utility::ipc::socket_path();
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
                        // Phase 1: Quick refresh with lock (no I/O)
                        let changed_workspaces = {
                            let mut s = state.write().await;
                            match s.refresh_windows() {
                                Ok(ws) => ws,
                                Err(e) => {
                                    tracing::warn!(error = ?e, "Window refresh failed");
                                    vec![]
                                }
                            }
                        }; // Lock released here

                        // Phase 2: Get windows needing fingerprints + copy of fingerprint index
                        // Both are quick read operations
                        let (needs_matching, fingerprint_index) = {
                            let s = state.read().await;
                            let needs = s.get_windows_needing_fingerprints();
                            let index = s.fingerprint_index.clone();
                            (needs, index)
                        }; // Lock released here

                        // Phase 3: Do expensive I/O (get_scrollback) WITHOUT ANY LOCK
                        // This allows concurrent readers to proceed unblocked
                        for addr in needs_matching {
                            if let Some((session_id, confidence, fingerprint)) =
                                BabelState::fingerprint_match_addr(&addr, &fingerprint_index)
                            {
                                // Phase 4: Apply result with write lock (quick operation)
                                let mut s = state.write().await;
                                s.apply_fingerprint_result(&addr, session_id, confidence, fingerprint);
                            }
                        }

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
	let _ = std::fs::remove_file(crate::utility::ipc::socket_path());
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
	state: &Arc<RwLock<BabelState>>,
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
	state: Arc<RwLock<BabelState>>,
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

// ═══════════════════════════════════════════════════════════════════════════════
// Request Handlers
// ═══════════════════════════════════════════════════════════════════════════════
//
// Extracted handlers for each IPC request type. Each handler is a focused function
// that can be tested independently. The process_request dispatcher delegates to these.

mod handlers {
	use super::*;

	/// List all Claude windows with activity states
	pub async fn list(state: &Arc<RwLock<BabelState>>) -> Response {
		let s = state.read().await;
		let windows: Vec<ClaudeWindow> = s.windows.values()
			.map(|w| {
				let mut win = w.clone();
				win.activity_state = s.get_activity_state(w.id());
				win
			})
			.collect();
		Response::Windows { windows }
	}

	/// List all terminals (Claude and non-Claude)
	pub async fn list_terminals(state: &Arc<RwLock<BabelState>>) -> Response {
		let s = state.read().await;
		let terminals: Vec<TerminalInfo> = s.terminals.values().cloned().collect();
		Response::Terminals { terminals }
	}

	/// List windows with fingerprints (expensive - extracts from scrollback)
	pub async fn list_with_fingerprints(state: &Arc<RwLock<BabelState>>) -> Response {
		let s = state.read().await;
		let mut windows: Vec<ClaudeWindow> = s.windows.values()
			.map(|w| {
				let mut win = w.clone();
				win.activity_state = s.get_activity_state(w.id());
				win
			})
			.collect();
		drop(s); // Release lock before expensive operations

		for win in &mut windows {
			if win.fingerprint.is_none() {
				if let Ok(scrollback) = get_scrollback(win.id()) {
					let fp = extract_from_scrollback(&scrollback);
					win.fingerprint = Some(fp);
				}
			}
			if win.session_info.is_none() {
				let _ = enrich_window(win);
			}
		}

		Response::Windows { windows }
	}

	/// Get status of a specific window or focused window
	pub async fn status(state: &Arc<RwLock<BabelState>>, window_id: Option<u64>) -> Response {
		let s = state.read().await;
		let window = if let Some(id) = window_id {
			s.find_window_by_id(id).cloned()
		} else {
			s.windows.values().find(|w| w.is_focused).cloned()
		};
		Response::Window { window: Box::new(window) }
	}

	/// Enrich a window with session info
	pub async fn enrich(state: &Arc<RwLock<BabelState>>, window_id: u64) -> Response {
		let mut s = state.write().await;
		if let Some(window) = s.find_window_by_id_mut(window_id) {
			if let Err(e) = enrich_window(window) {
				return Response::Error { message: format!("Failed to enrich: {}", e) };
			}
			Response::Window { window: Box::new(Some(window.clone())) }
		} else {
			Response::Error { message: "Window not found".to_string() }
		}
	}

	/// Focus a window (may be on non-current socket)
	pub fn focus(window_id: u64) -> Response {
		match focus_window_any(window_id) {
			Ok(result) => {
				let message = if result.is_non_current {
					format!("⚠ Focused window {} on non-current socket: {}", window_id, result.addr.short())
				} else {
					format!("Focused window {}", window_id)
				};
				Response::Ok { message }
			}
			Err(e) => Response::Error { message: format!("Focus failed: {}", e) },
		}
	}

	/// Get scrollback from a window
	pub fn scroll(window_id: u64) -> Response {
		match get_scrollback_any(window_id) {
			Ok(result) => {
				if result.is_non_current {
					tracing::warn!(window_id, addr = %result.addr.short(), "Scrollback from non-current socket");
				}
				Response::Scrollback { text: result.result }
			}
			Err(e) => Response::Error { message: format!("Scroll failed: {}", e) },
		}
	}

	/// Send text to a window
	pub fn send(window_id: u64, text: &str) -> Response {
		match send_text_any(window_id, text) {
			Ok(result) => {
				let message = if result.is_non_current {
					format!("⚠ Sent to window {} on non-current socket: {}", window_id, result.addr.short())
				} else {
					format!("Sent to window {}", window_id)
				};
				Response::Ok { message }
			}
			Err(e) => Response::Error { message: format!("Send failed: {}", e) },
		}
	}

	/// Tag a window with an icon
	pub async fn tag(state: &Arc<RwLock<BabelState>>, window_id: u64, icon: &str) -> Response {
		let s = state.read().await;
		if let Some(window) = s.find_window_by_id(window_id) {
			if let Some(session_id) = &window.session_id {
				match init_db().and_then(|conn| set_icon(&conn, session_id, icon)) {
					Ok(()) => Response::Ok { message: format!("Tagged {} with {}", window_id, icon) },
					Err(e) => Response::Error { message: format!("Tag failed: {}", e) },
				}
			} else {
				Response::Error { message: "Window has no session".to_string() }
			}
		} else {
			Response::Error { message: "Window not found".to_string() }
		}
	}

	/// Mark a window as read
	pub async fn mark_read_handler(state: &Arc<RwLock<BabelState>>, window_id: u64) -> Response {
		let s = state.read().await;
		if let Some(window) = s.find_window_by_id(window_id) {
			if let Some(session_id) = &window.session_id {
				match init_db().and_then(|conn| mark_read(&conn, session_id)) {
					Ok(()) => Response::Ok { message: format!("Marked {} as read", window_id) },
					Err(e) => Response::Error { message: format!("Mark read failed: {}", e) },
				}
			} else {
				Response::Error { message: "Window has no session".to_string() }
			}
		} else {
			Response::Error { message: "Window not found".to_string() }
		}
	}

	/// Get recent session history
	pub fn history(limit: usize) -> Response {
		match get_recent_sessions(limit) {
			Ok(sessions) => Response::History { sessions },
			Err(e) => Response::Error { message: format!("History failed: {}", e) },
		}
	}

	/// Ping - return daemon uptime
	pub async fn ping(state: &Arc<RwLock<BabelState>>) -> Response {
		let s = state.read().await;
		Response::Pong { uptime_secs: s.start_time.elapsed().as_secs() }
	}

	/// Refresh windows and run fingerprint matching
	pub async fn refresh(state: &Arc<RwLock<BabelState>>) -> Response {
		// Phase 1: Refresh windows (quick, no I/O)
		let window_count = {
			let mut s = state.write().await;
			match s.refresh_windows() {
				Ok(_) => s.windows.len(),
				Err(e) => return Response::Error { message: format!("Refresh failed: {}", e) }
			}
		};

		// Phase 2: Get windows needing fingerprints + index
		let (needs_matching, fingerprint_index) = {
			let s = state.read().await;
			(s.get_windows_needing_fingerprints(), s.fingerprint_index.clone())
		};

		// Phase 3: Do expensive I/O without lock
		for addr in needs_matching {
			if let Some((session_id, confidence, fingerprint)) =
				BabelState::fingerprint_match_addr(&addr, &fingerprint_index)
			{
				let mut s = state.write().await;
				s.apply_fingerprint_result(&addr, session_id, confidence, fingerprint);
			}
		}

		Response::Ok { message: format!("Refreshed {} windows", window_count) }
	}

	/// Get workspace titles
	pub async fn titles(state: &Arc<RwLock<BabelState>>) -> Response {
		let s = state.read().await;
		let titles: std::collections::HashMap<String, String> = s.workspace_titles
			.iter()
			.map(|(k, v)| (k.to_string(), v.clone()))
			.collect();
		Response::Titles { titles }
	}

	/// Refresh workspace titles
	pub async fn title_refresh(
		state: &Arc<RwLock<BabelState>>,
		summarizer: &Arc<crate::summarizer::WorkspaceSummarizer>,
		workspace: Option<i32>,
	) -> Response {
		let workspaces_to_refresh: Vec<i32> = {
			let s = state.read().await;
			if let Some(ws) = workspace {
				vec![ws]
			} else {
				s.windows.values()
					.filter_map(|w| w.workspace)
					.collect::<std::collections::HashSet<_>>()
					.into_iter()
					.collect()
			}
		};

		if let Some(ws) = workspace {
			summarizer.invalidate(ws).await;
		} else {
			summarizer.clear_cache().await;
		}

		for ws in &workspaces_to_refresh {
			summarize_workspace(*ws, state, summarizer).await;
		}

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

	// ─── WSet Handlers ─────────────────────────────────────────────────────────

	pub async fn wset_save(state: &Arc<RwLock<BabelState>>, name: Option<String>) -> Response {
		let wset_name = match name {
			Some(n) => n,
			None => get_current_wset_name().ok().flatten().unwrap_or_else(|| "default".to_string()),
		};

		let s = state.read().await;
		let mut wset = WSet::from_babel_state(&wset_name, &s);
		drop(s);

		match wset.save() {
			Ok(_) => {
				let wspaces = wset.wspaces.len();
				let windows = wset.window_count();
				if let Err(e) = set_current_wset_name(&wset_name) {
					tracing::warn!(error = %e, "Failed to set current wset name");
				}
				Response::WSetSaved { name: wset_name, wspaces, windows }
			}
			Err(e) => Response::Error { message: format!("Failed to save WSet: {}", e) },
		}
	}

	pub async fn wset_load(state: &Arc<RwLock<BabelState>>, name: Option<String>, dry_run: bool) -> Response {
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

		let wset = match WSet::load(&wset_name) {
			Ok(w) => w,
			Err(e) => return Response::Error {
				message: format!("Failed to load WSet '{}': {}", wset_name, e),
			},
		};

		let wspaces = wset.wspaces.len();
		let windows = wset.window_count();

		if dry_run {
			return Response::WSetLoaded { name: wset_name, wspaces, windows, skipped: vec![], dry_run: true };
		}

		let skipped = match load_wset(&wset).await {
			Ok(s) => s,
			Err(e) => return Response::Error { message: format!("Failed to load WSet: {}", e) },
		};

		if let Err(e) = set_current_wset_name(&wset_name) {
			tracing::warn!(error = %e, "Failed to set current wset name");
		}

		{
			let mut s = state.write().await;
			let _ = s.refresh_windows();
		}

		Response::WSetLoaded { name: wset_name, wspaces, windows, skipped, dry_run: false }
	}

	pub fn wset_list() -> Response {
		match list_wsets() {
			Ok(wsets) => {
				let current = get_current_wset_name().ok().flatten();
				Response::WSetList { wsets, current }
			}
			Err(e) => Response::Error { message: format!("Failed to list WSet files: {}", e) },
		}
	}

	pub fn wset_current() -> Response {
		match get_current_wset_name() {
			Ok(name) => Response::WSetCurrent { name },
			Err(e) => Response::Error { message: format!("Failed to get current WSet: {}", e) },
		}
	}

	pub fn wset_delete(name: &str) -> Response {
		match WSet::delete(name) {
			Ok(()) => Response::Ok { message: format!("Deleted WSet '{}'", name) },
			Err(e) => Response::Error { message: format!("Failed to delete WSet '{}': {}", name, e) },
		}
	}

	pub fn wset_rename(old: &str, new: &str) -> Response {
		match WSet::rename(old, new) {
			Ok(()) => Response::Ok { message: format!("Renamed WSet '{}' to '{}'", old, new) },
			Err(e) => Response::Error { message: format!("Failed to rename WSet: {}", e) },
		}
	}

	pub fn wset_describe(name: &str, description: Option<String>) -> Response {
		match WSet::load(name) {
			Ok(mut wset) => {
				wset.meta.description = description.clone();
				match wset.save() {
					Ok(_) => {
						let desc = description.unwrap_or_else(|| "(cleared)".to_string());
						Response::Ok { message: format!("Set description for '{}': {}", name, desc) }
					}
					Err(e) => Response::Error { message: format!("Failed to save WSet: {}", e) },
				}
			}
			Err(e) => Response::Error { message: format!("Failed to load WSet '{}': {}", name, e) },
		}
	}
}

/// Dispatch IPC requests to appropriate handlers
///
/// This dispatcher routes requests to focused handler functions.
/// Each handler is independently testable and has clear responsibility.
async fn process_request(
	request: Request,
	state: &Arc<RwLock<BabelState>>,
	summarizer: &Arc<crate::summarizer::WorkspaceSummarizer>,
) -> Response {
	match request {
		// ─── Query Handlers ─────────────────────────────────────────────────────
		Request::List => handlers::list(state).await,
		Request::ListTerminals => handlers::list_terminals(state).await,
		Request::ListWithFingerprints => handlers::list_with_fingerprints(state).await,
		Request::Status { window_id } => handlers::status(state, window_id).await,
		Request::History { limit } => handlers::history(limit),
		Request::Ping => handlers::ping(state).await,
		Request::Titles => handlers::titles(state).await,

		// ─── Window Handlers ────────────────────────────────────────────────────
		Request::Enrich { window_id } => handlers::enrich(state, window_id).await,
		Request::Focus { window_id } => handlers::focus(window_id),
		Request::Scroll { window_id } => handlers::scroll(window_id),
		Request::Send { window_id, text } => handlers::send(window_id, &text),

		// ─── State Handlers ─────────────────────────────────────────────────────
		Request::Tag { window_id, icon } => handlers::tag(state, window_id, &icon).await,
		Request::MarkRead { window_id } => handlers::mark_read_handler(state, window_id).await,
		Request::Refresh => handlers::refresh(state).await,
		Request::TitleRefresh { workspace } => handlers::title_refresh(state, summarizer, workspace).await,

		// ─── WSet Handlers ──────────────────────────────────────────────────────
		Request::WSetSave { name } => handlers::wset_save(state, name).await,
		Request::WSetLoad { name, dry_run } => handlers::wset_load(state, name, dry_run).await,
		Request::WSetList => handlers::wset_list(),
		Request::WSetCurrent => handlers::wset_current(),
		Request::WSetDelete { name } => handlers::wset_delete(&name),
		Request::WSetRename { old, new } => handlers::wset_rename(&old, &new),
		Request::WSetDescribe { name, description } => handlers::wset_describe(&name, description),

		// ─── System Handlers ────────────────────────────────────────────────────
		Request::Shutdown => Response::Ok { message: "Shutting down".to_string() },

		// Subscribe is handled specially in handle_client
		Request::Subscribe { .. } => Response::Error {
			message: "Subscribe requests must be handled via handle_client".to_string(),
		}
	}
}
