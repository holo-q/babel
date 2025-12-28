//! Babel Daemon - The watchful eye atop the tower
//!
//! The daemon observes all Claude workers in the tower, maintaining awareness of
//! their states, conversations, and movements between workspaces. It speaks to no
//! one directly—it only watches and reports. When the Captain arrives, this daemon
//! will serve as their eyes: the ground truth of who is working, who awaits input,
//! who has fallen silent. For now, it simply keeps the tower's ledger.
//!
//! The daemon maintains:
//! - Live mapping of kitty panes → Claude sessions
//! - Cached session info from ~/.claude
//! - Summary index for fast matching
//!
//! It watches:
//! - Kitty panes (polling every 500ms)
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
    /// Used for: new windows, session state changes, scrollback activity.
    /// Workspace changes are instant via wnck signals (not polling).
    /// 500ms is fine since most changes are caught by wnck.
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
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tokio::sync::{mpsc, RwLock};

use crate::utility::claude_storage::{claude_base, get_recent_sessions, get_session_info};
use crate::utility::claude_discovery::{enrich_window, load_wset, ClaudePane};
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
    discover_all_instances, default_socket,
    set_border_color_on_socket, reset_border_color_on_socket,
};
use crate::utility::claude_discovery::{get_window_activity_with_scrollback, get_activity_with_scrollback_on_socket, detect_claude_signals};
use crate::babel_storage::{init_db, mark_read, mark_unread, set_icon};
use crate::wset::{WSet, get_current_wset_name, set_current_wset_name, list_wsets};
use vtr::trace::{VtrLayer, RingBuffer, TraceSnapshot, generate_trace_id, with_trace_id};
use vtr::{checkpoint, effect, state, trace_error, boundary};

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;

/// Global trace flag - set via run_daemon_with_trace
/// When enabled, debug-level tracing is used instead of trace-level
pub static TRACE: AtomicBool = AtomicBool::new(false);

/// VTR ring buffer capacity for daemon event capture
/// 50K events captures substantial parallel operation history for debugging
const VTR_BUFFER_CAPACITY: usize = 50_000;

/// Global VTR buffer handle - captures all tracing events for debugging
/// Access via `vtr_buffer()` to drain on error or inspect parallel operations
pub static VTR_BUFFER: once_cell::sync::OnceCell<Arc<Mutex<RingBuffer<TraceSnapshot>>>> =
    once_cell::sync::OnceCell::new();

/// Get the VTR ring buffer handle for event inspection
///
/// Returns None if daemon logging hasn't been initialized yet.
/// Use this to drain events on error or inspect parallel operation traces.
pub fn vtr_buffer() -> Option<Arc<Mutex<RingBuffer<TraceSnapshot>>>> {
    VTR_BUFFER.get().cloned()
}

/// Initialize daemon-specific logging with VtrLayer
///
/// This builds a custom tracing subscriber that includes:
/// - EnvFilter from spaceship logging.toml config
/// - CompactingStderrLayer for depth-aware stderr output with log compaction
/// - VtrLayer for ring buffer event capture (50K events)
///
/// CompactingStderrLayer consolidates consecutive duplicate entries (same file:line:vtr_kind)
/// and shows a `×N` suffix. This reduces noise from polling loops like `kitty::get-text`.
///
/// Stderr output is captured by systemd → journald, so `spacejn babel` sees it.
///
/// The VtrLayer buffer is stored in VTR_BUFFER for access via vtr_buffer().
/// On error, the buffer can be drained to inspect the execution history.
///
/// # Arguments
/// * `args` - LoggingArgs with debug flag
pub fn init_daemon_logging(args: &spaceship_std::LoggingArgs) {
    use std::io::IsTerminal;
    use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt, EnvFilter, reload};
    use vtr::trace::CompactingStderrLayer;

    let debug = args.debug;
    // Enable trace level when debug mode is on so instrumented spans with context fields
    // (like kitty_id) are created and the context column shows actual values
    let filter_str = if debug {
        "trace".to_string()
    } else {
        spaceship_std::logging::load_level("babel", "claude_babel")
    };

    let filter = EnvFilter::new(&filter_str);
    let (filter_layer, reload_handle) = reload::Layer::new(filter);

    // Create VtrLayer and store buffer handle
    let vtr_layer = VtrLayer::new(VTR_BUFFER_CAPACITY);
    let buffer_handle = vtr_layer.buffer();
    VTR_BUFFER.set(buffer_handle).expect("VTR_BUFFER already initialized");

    // CompactingStderrLayer provides:
    // - VTR-style formatting with depth markers, semantic markers, syntax highlighting
    // - Log compaction: consecutive duplicate entries (same file:line:vtr_kind) show ×N suffix
    // - Ideal for polling loops like kitty::get-text that fire repeatedly
    //
    // Output goes to stderr, systemd captures it → journald, spacejn displays it.
    let is_tty = std::io::stderr().is_terminal();
    let stderr_layer = CompactingStderrLayer::new()
        .with_color(is_tty || debug)
        .with_context_field("kitty_id")  // Display pane ID column when available
        .with_context_width(4);

    tracing_subscriber::registry()
        .with(filter_layer)
        .with(stderr_layer)
        .with(vtr_layer)
        .init();

    // Spawn SIGHUP handler for hot-reload (skip in debug mode - fixed level)
    if !debug {
        std::thread::spawn(move || {
            use signal_hook::iterator::Signals;
            let mut signals = Signals::new(&[signal_hook::consts::SIGHUP]).unwrap();
            for _ in signals.forever() {
                let new_filter_str = spaceship_std::logging::load_level("babel", "claude_babel");
                let new_filter = EnvFilter::new(&new_filter_str);
                if reload_handle.reload(new_filter).is_ok() {
                    tracing::info!("Logging config reloaded");
                } else {
                    break; // Subscriber dropped, exit thread
                }
            }
        });
    }
}

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
	/// X11/Wayland window ID for geometry lookup
	/// Used for screen-position sorting (left→right ordering for connectors)
	pub platform_window_id: u64,
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

/// Per-window scrollback activity tracking for ActivityPulse events
///
/// Tracks scrollback changes to detect when Claude is outputting tokens
/// or executing tools. Used to emit fine-grained activity pulses for
/// reactive UI animations in frontends like richspace-babel.
#[derive(Debug, Clone)]
pub struct ScrollbackActivity {
	/// Fast hash of scrollback content (for change detection)
	/// Using FxHash for speed - collisions are tolerable for this use case
	pub content_hash: u64,
	/// Scrollback length in bytes (for computing delta intensity)
	pub byte_length: usize,
	/// Recent delta sizes (rolling window for intensity smoothing)
	/// Stored as (bytes_delta, timestamp) pairs
	pub recent_deltas: std::collections::VecDeque<(usize, Instant)>,
	/// Timestamp of last content change
	pub last_change: Option<Instant>,
}

impl ScrollbackActivity {
	pub fn new() -> Self {
		Self {
			content_hash: 0,
			byte_length: 0,
			recent_deltas: std::collections::VecDeque::with_capacity(10),
			last_change: None,
		}
	}

	/// Update with new scrollback content, return (changed, delta_bytes)
	pub fn update(&mut self, scrollback: &str) -> (bool, usize) {
		use std::hash::{Hash, Hasher};
		use std::collections::hash_map::DefaultHasher;

		// Fast hash (FxHash would be better but DefaultHasher is fine)
		let mut hasher = DefaultHasher::new();
		scrollback.hash(&mut hasher);
		let new_hash = hasher.finish();

		let new_len = scrollback.len();
		let delta = new_len.saturating_sub(self.byte_length);

		if new_hash != self.content_hash {
			self.content_hash = new_hash;
			self.byte_length = new_len;
			self.last_change = Some(Instant::now());

			// Track delta for intensity smoothing
			self.recent_deltas.push_back((delta, Instant::now()));
			// Keep only last 10 deltas
			while self.recent_deltas.len() > 10 {
				self.recent_deltas.pop_front();
			}

			(true, delta)
		} else {
			(false, 0)
		}
	}

	/// Compute activity intensity from recent deltas (0.0-1.0)
	///
	/// Higher intensity when:
	/// - Recent large deltas (lots of output)
	/// - Frequent changes (rapid token output)
	pub fn compute_intensity(&self) -> f32 {
		if self.recent_deltas.is_empty() {
			return 0.0;
		}

		let now = Instant::now();
		let window = std::time::Duration::from_secs(2);

		// Sum recent deltas within window
		let recent_bytes: usize = self.recent_deltas
			.iter()
			.filter(|(_, ts)| now.duration_since(*ts) < window)
			.map(|(bytes, _)| *bytes)
			.sum();

		// Map to intensity:
		// - 0-100 bytes: 0.0-0.3 (few tokens)
		// - 100-500 bytes: 0.3-0.6 (steady output)
		// - 500+ bytes: 0.6-1.0 (rapid output/tool execution)
		let intensity = match recent_bytes {
			0..=100 => recent_bytes as f32 / 100.0 * 0.3,
			101..=500 => 0.3 + (recent_bytes - 100) as f32 / 400.0 * 0.3,
			_ => (0.6 + (recent_bytes - 500) as f32 / 1000.0 * 0.4).min(1.0),
		};

		intensity
	}
}

impl Default for ScrollbackActivity {
	fn default() -> Self {
		Self::new()
	}
}

/// Babel state - shared across tasks
///
/// All window/terminal state is keyed by PaneAddr (socket + id) to support
/// multiple kitty instances. This enables graceful degradation when things
/// are "fucked" - multiple instances, dead sockets, etc.
pub struct BabelState {
	/// Current Claude panes (PaneAddr → ClaudePane)
	/// Keyed by PaneAddr to handle ID collisions across kitty instances
	pub windows: HashMap<PaneAddr, ClaudePane>,

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
	/// Tracking each worker's breath—their current state in the cycle
	pub window_states: HashMap<PaneAddr, scrollparse::claude::ActivityState>,

	/// Scrollback activity tracking for ActivityPulse events (PaneAddr → ScrollbackActivity)
	/// Tracks content hashes and deltas to detect token output / tool execution
	pub window_activity: HashMap<PaneAddr, ScrollbackActivity>,

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
			window_activity: HashMap::new(),
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
	///
	/// # Performance
	/// When `skip_activity_fetch` is true, skips the expensive scrollback fetches
	/// used for activity state detection. Use this for quick structural refreshes
	/// that don't need real-time activity states (cached values are used instead).
	/// The periodic poll passes `false` to keep activity states current.
	#[tracing::instrument(skip(self), fields(skip_activity = skip_activity_fetch))]
	pub async fn refresh_windows(&mut self, skip_activity_fetch: bool) -> Result<Vec<i32>> {
		use crate::kitty::get_all_workspaces;

		// ─── Multi-Socket Discovery ─────────────────────────────────────────────────
		// Query all kitty instances, update socket_status
		// IMPORTANT: We extract panes from instances to avoid a second round of kitten @ ls calls
		let instances = discover_all_instances().await;
		let current_socket = default_socket();

		self.socket_status = instances.iter()
			.map(|i| (i.socket.clone(), SocketStatus {
				is_current: i.socket == current_socket,
				is_responsive: i.is_responsive,
				pane_count: i.panes.len(),
				last_error: i.error.clone(),
			}))
			.collect();

		// Extract all panes from discovered instances (already fetched above)
		// This avoids the duplicate kitten @ ls calls that list_all_panes() would do
		let all_panes: Vec<_> = instances.into_iter()
			.flat_map(|i| i.panes)
			.collect();

		// Filter to just Claude panes for the main tracking
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
				platform_window_id: pane.platform_window_id,
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
		let mut new_windows: HashMap<PaneAddr, ClaudePane> = HashMap::new();

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
				ClaudePane {
					addr: addr.clone(),
					title: kw.title.clone(),
					session_id: existing_session,
					session_info: None,
					cwd: kw.cwd.clone(),
					is_focused: kw.is_focused,
					os_window_id: kw.os_window_id,
					platform_window_id: kw.platform_window_id,
					workspace,
					screen: kw.screen.clone(),
					activity_state: None, // Will be populated from window_states cache
					hook_state: None, // Populated later from babel_storage
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
			// Emit PaneUnfocused for the pane that lost focus
			if let Some(ref addr) = old_focused {
				if let Some(w) = self.windows.get(addr) {
					self.event_publisher.publish(BabelEvent::PaneUnfocused {
						kitty_id: addr.id,
						session_id: w.session_id.clone(),
					});
				}
			}
			// Emit PaneFocused for the pane that gained focus
			if let Some(ref addr) = new_focused {
				if let Some(w) = new_windows.get(addr) {
					self.event_publisher.publish(BabelEvent::PaneFocused {
						kitty_id: addr.id,
						session_id: w.session_id.clone(),
					});
					// Mark as read when pane gains focus—the worker's voice is now heard
					if let Some(ref session_id) = w.session_id {
						if let Err(e) = init_db().and_then(|conn| mark_read(&conn, session_id)) {
							trace_error!("failed to mark as read on focus", session_id, error = %e);
						}
						// Dim the ring—the worker's call has been answered
						if let Err(e) = reset_border_color_on_socket(&addr.socket, addr.id).await {
							effect!("xfconf", "reset_border", error = e.to_string());
						}
					}
				}
			}
		}

		// Check for workspace changes (windows that moved between workspaces)
		// Used by richspace-babel to update per-workspace dot display
		for addr in old_addrs.intersection(&new_addrs) {
			let old_ws = self.windows.get(addr).and_then(|w| w.workspace);
			let new_ws = new_windows.get(addr).and_then(|w| w.workspace);

			if old_ws != new_ws {
				self.event_publisher.publish(BabelEvent::WindowWorkspaceChanged {
					kitty_id: addr.id,
					old_workspace: old_ws,
					new_workspace: new_ws,
				});
				// Track both workspaces for re-summarization
				if let Some(ws) = old_ws {
					changed_workspaces.insert(ws);
				}
				if let Some(ws) = new_ws {
					changed_workspaces.insert(ws);
				}
				trace!("Window {} moved: workspace {:?} -> {:?}", addr.short(), old_ws, new_ws);
			}
		}

		// Detect session state changes and emit events
		// This enables richspace-babel to track Claude activity per-workspace
		// Also track scrollback changes for ActivityPulse events
		//
		// PERFORMANCE: This loop is expensive (~100ms per window via kitten subprocess).
		// Skip when skip_activity_fetch=true for fast structural refreshes.
		// Cached activity states from previous polls are still available.
		if !skip_activity_fetch {
		for (addr, window) in &new_windows {
			// Get state, asking_question, and scrollback in one fetch to avoid double I/O
			let activity = get_window_activity_with_scrollback(addr.id).await;
			let new_state = activity.state;
			let old_state = self.window_states.get(addr).copied();

			// ─── State Change Detection ─────────────────────────────────────────────
			match old_state {
				Some(old) if old != new_state => {
					// State changed - emit event
					state!("window_activity", format!("{:?}", old) => format!("{:?}", new_state), window = addr.short());
					self.event_publisher.publish(BabelEvent::SessionStateChanged {
						kitty_id: addr.id,
						session_id: window.session_id.clone(),
						workspace: window.workspace,
						old_state: old,
						new_state,
						asking_question: activity.asking_question,
					});
					self.window_states.insert(addr.clone(), new_state);

					// Also emit ActivityPulse on state transitions
					self.event_publisher.publish(BabelEvent::ActivityPulse {
						kitty_id: addr.id,
						session_id: window.session_id.clone(),
						workspace: window.workspace,
						intensity: 0.8, // State transitions are significant
						trigger: crate::events::PulseTrigger::StateTransition,
					});

					// Auto-unread when Claude finishes working and awaits input
					// This ensures users see the yellow dot when there's new content to review
					// Worker has spoken, awaits the user's voice
					if new_state == scrollparse::claude::ActivityState::AwaitingInput {
						if let Some(ref session_id) = window.session_id {
							if let Err(e) = init_db().and_then(|conn| mark_unread(&conn, session_id)) {
								trace_error!("failed to auto-unread session", session_id, error = %e);
							}
							// Light the ring—the worker calls for attention
							// Warm amber for unread, drawing the eye to unheard voices
							if let Err(e) = set_border_color_on_socket(&addr.socket, addr.id, "#f67400", "#7a3a00").await {
								effect!("xfconf", "set_unread_border", error = e.to_string());
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

			// ─── Activity Pulse Detection ───────────────────────────────────────────
			// Track scrollback changes to emit fine-grained activity pulses
			// for reactive UI animations (heartbeat blinks, etc.)
			if !activity.scrollback.is_empty() {
				let window_activity = self.window_activity.entry(addr.clone()).or_default();
				let (changed, _delta_bytes) = window_activity.update(&activity.scrollback);

				if changed {
					let intensity = window_activity.compute_intensity();

					// Determine trigger type based on current state
					let trigger = match new_state {
						scrollparse::claude::ActivityState::ToolUse => crate::events::PulseTrigger::ToolStart,     // hands begin moving
						scrollparse::claude::ActivityState::Thinking => crate::events::PulseTrigger::TokenOutput,  // soul breathing tokens
						_ => crate::events::PulseTrigger::TokenOutput,
					};

					// Only emit if intensity is meaningful (avoid noise)
					if intensity > 0.05 {
						self.event_publisher.publish(BabelEvent::ActivityPulse {
							kitty_id: addr.id,
							session_id: window.session_id.clone(),
							workspace: window.workspace,
							intensity,
							trigger,
						});
					}
				}
			}
		}
		} // end skip_activity_fetch check

		// Clean up states for removed windows
		self.window_states.retain(|addr, _| new_windows.contains_key(addr));
		self.window_activity.retain(|addr, _| new_windows.contains_key(addr));

		self.windows = new_windows;
		self.last_kitty_scan = Instant::now();

		// Return workspaces that changed for re-summarization
		Ok(changed_workspaces.into_iter().collect())
	}

	/// Match a window title to a session using the summary index
	#[tracing::instrument(skip(self), fields(title = %title, index_size = self.summary_index.len()))]
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

	/// Get list of window addresses + CWDs that need fingerprint matching
	///
	/// Returns (addr, cwd) pairs. CWD comes from kitty pane info (reliable),
	/// NOT from scrollback extraction (unreliable - Claude status bar format varies).
	///
	/// Called with read lock to identify windows that need matching.
	/// Caller then releases lock and does expensive I/O.
	#[tracing::instrument(skip(self), fields(window_count = self.windows.len()))]
	pub fn get_windows_needing_fingerprints(&self) -> Vec<(PaneAddr, PathBuf)> {
		self.windows
		    .iter()
		    .filter(|(_, w)| w.session_id.is_none())
		    .map(|(addr, w)| (addr.clone(), w.cwd.clone()))
		    .collect()
	}

	/// Get session IDs already claimed by existing windows
	///
	/// Used to exclude these from "unique CWD" matching - if a session is already
	/// assigned to another window, it shouldn't count when checking if a CWD is unique.
	pub fn get_claimed_sessions(&self) -> HashSet<String> {
		self.windows.values()
			.filter_map(|w| w.session_id.clone())
			.collect()
	}

	/// Apply fingerprint matching results to a window
	///
	/// This applies the results from `fingerprint_match_addr` back to state.
	/// Called with write lock after I/O completes.
	#[tracing::instrument(skip(self, fingerprint), fields(addr = %addr.short(), session_id = %session_id, confidence = ?confidence))]
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

	/// Apply activity state update for a window
	///
	/// This processes activity state and scrollback changes, emitting events as needed.
	/// Called with write lock after the expensive I/O (scrollback fetch) completes.
	/// Designed to be quick - just state updates and event emission.
	pub async fn apply_activity_update(
		&mut self,
		addr: &PaneAddr,
		new_state: scrollparse::claude::ActivityState,
		asking_question: bool,
		scrollback: String,
	) {
		let old_state = self.window_states.get(addr).copied();
		let window = match self.windows.get(addr) {
			Some(w) => w,
			None => return, // Window no longer exists
		};

		// ─── State Change Detection ─────────────────────────────────────────────
		match old_state {
			Some(old) if old != new_state => {
				// State changed - emit event
				state!("window_activity", format!("{:?}", old) => format!("{:?}", new_state), window = addr.short());
				self.event_publisher.publish(BabelEvent::SessionStateChanged {
					kitty_id: addr.id,
					session_id: window.session_id.clone(),
					workspace: window.workspace,
					old_state: old,
					new_state,
					asking_question,
				});
				self.window_states.insert(addr.clone(), new_state);

				// Also emit ActivityPulse on state transitions
				self.event_publisher.publish(BabelEvent::ActivityPulse {
					kitty_id: addr.id,
					session_id: window.session_id.clone(),
					workspace: window.workspace,
					intensity: 0.8, // State transitions are significant
					trigger: crate::events::PulseTrigger::StateTransition,
				});

				// Auto-unread when Claude finishes working and awaits input
				// Worker has spoken, awaits the user's voice
				if new_state == scrollparse::claude::ActivityState::AwaitingInput {
					if let Some(ref session_id) = window.session_id {
						if let Err(e) = init_db().and_then(|conn| mark_unread(&conn, session_id)) {
							trace_error!("failed to auto-unread session", session_id, error = %e);
						}
						// Light the ring—the worker calls for attention
						if let Err(e) = set_border_color_on_socket(&addr.socket, addr.id, "#f67400", "#7a3a00").await {
							effect!("xfconf", "set_unread_border", error = e.to_string());
						}
					}
				}
			}
			None => {
				// New window - initialize state (no event)
				self.window_states.insert(addr.clone(), new_state);
			}
			_ => {
				// State unchanged
			}
		}

		// ─── Activity Pulse Detection ───────────────────────────────────────────
		if !scrollback.is_empty() {
			let activity = self.window_activity.entry(addr.clone()).or_default();
			let (changed, _delta_bytes) = activity.update(&scrollback);

			if changed {
				let intensity = activity.compute_intensity();
				// Reading the worker's breath to pulse the right signal
			let trigger = match new_state {
					scrollparse::claude::ActivityState::ToolUse => crate::events::PulseTrigger::ToolStart,     // hands begin moving
					scrollparse::claude::ActivityState::Thinking => crate::events::PulseTrigger::TokenOutput,  // soul breathing tokens
					_ => crate::events::PulseTrigger::TokenOutput,
				};

				// Only emit if intensity is meaningful
				if intensity > 0.05 {
					self.event_publisher.publish(BabelEvent::ActivityPulse {
						kitty_id: addr.id,
						session_id: window.session_id.clone(),
						workspace: window.workspace,
						intensity,
						trigger,
					});
				}

				// ─── File Index Update ───────────────────────────────────────────────
				// Extract file operations from scrollback and record them
				// Only process if we have a session_id (we need it to track touches)
				if let Some(ref session_id) = window.session_id {
					let file_ops = crate::file_index::extract_file_operations(&scrollback);
					if !file_ops.is_empty() {
						// Record touches to database (best effort - don't fail on DB errors)
						if let Ok(storage) = crate::babel_storage::BabelStorage::open() {
							for op in &file_ops {
								if let Err(e) = storage.record_file_touch(session_id, &op.path, &op.operation) {
									trace_error!(
										"failed to record file touch",
										session_id,
										path = %op.path,
										operation = %op.operation,
										error = %e
									);
								}
							}
							trace!(
								session_id,
								count = file_ops.len(),
								"Recorded file touches from scrollback"
							);
						}
					}
				}
			}
		}
	}

	/// Perform fingerprint matching for a window address without holding any lock
	///
	/// This does the expensive I/O (get_scrollback) and matching logic.
	/// Called outside any locks to avoid blocking readers.
	///
	/// Takes fingerprint_index as parameter to avoid needing &self.
	/// Takes kitty_cwd from pane info (reliable) instead of extracting from scrollback
	/// (unreliable - Claude's status bar format varies and may scroll off).
	/// Takes claimed_sessions to exclude from "unique CWD" matching - prevents
	/// double-matching when multiple windows are processed in parallel.
	#[tracing::instrument(skip(fingerprint_index, claimed_sessions), fields(addr = %addr.short(), cwd = %kitty_cwd.display(), index_size = fingerprint_index.len()))]
	pub async fn fingerprint_match_addr(
		addr: &PaneAddr,
		kitty_cwd: &Path,
		fingerprint_index: &HashMap<String, SessionFingerprint>,
		claimed_sessions: &HashSet<String>,
	) -> Option<(String, MatchConfidence, SessionFingerprint)> {
		use crate::kitty::get_scrollback_on_socket;

		trace!("fingerprint_match({}) - index has {} sessions, cwd={}", addr.short(), fingerprint_index.len(), kitty_cwd.display());

		// Get scrollback using the pane's socket (EXPENSIVE I/O - done without lock)
		let scrollback = match get_scrollback_on_socket(&addr.socket, addr.id).await {
			Ok(s) => s,
			Err(e) => {
				trace_error!("failed to get scrollback", addr = %addr.short(), error = %e);
				return None;
			}
		};

		trace!("  scrollback: {} bytes, {} lines", scrollback.len(), scrollback.lines().count());

		// Extract fingerprint from scrollback, but use kitty's CWD (reliable)
		// instead of trying to parse Claude's status bar (unreliable format)
		let mut window_fp = extract_from_scrollback(&scrollback);
		window_fp.cwd = Some(kitty_cwd.to_path_buf());

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

		// If no Medium+ match found, check for unique CWD match among UNCLAIMED sessions
		// When only ONE unclaimed session has the same CWD as the window,
		// that's a strong enough signal to accept as a match
		if best_match.is_none() && window_fp.cwd.is_some() {
			let window_cwd = window_fp.cwd.as_ref().unwrap();
			// Count unclaimed sessions with matching CWD (exclude sessions already assigned to other windows)
			let cwd_matches: Vec<_> = fingerprint_index.iter()
				.filter(|(sid, fp)| {
					fp.cwd.as_ref() == Some(window_cwd) && !claimed_sessions.contains(*sid)
				})
				.map(|(sid, _)| sid.clone())
				.collect();

			if cwd_matches.len() == 1 {
				tracing::info!(
					"Unique CWD match: window CWD {:?} matches only unclaimed session {} (excluding {} claimed)",
					window_cwd, cwd_matches[0], claimed_sessions.len()
				);
				best_match = Some((cwd_matches[0].clone(), MatchConfidence::Medium));
			} else if cwd_matches.len() > 1 {
				tracing::debug!(
					"Multiple unclaimed sessions ({}) with matching CWD {:?}, need more signals",
					cwd_matches.len(), window_cwd
				);
			}
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
	#[tracing::instrument(skip(self))]
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
	#[tracing::instrument(skip(self))]
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
		let mut sessions_with_cwd = 0;
		for (path, _) in session_files {
			if let Some(session_id) = path.file_stem().and_then(|s| s.to_str()) {
				if let Ok(mut fp) = extract_from_jsonl(&path) {
					fp.session_id = Some(session_id.to_string());
					if fp.cwd.is_some() {
						sessions_with_cwd += 1;
						if let Some(cwd) = &fp.cwd {
							if cwd.to_string_lossy().contains("claude-babel") {
								tracing::debug!("Index adding session {} with CWD {:?}", session_id, cwd);
							}
						}
					}
					index.insert(session_id.to_string(), fp);
				}
			}
		}
		checkpoint!("index_built", sessions = index.len(), with_cwd = sessions_with_cwd);

		self.fingerprint_index = index;
		self.last_fingerprint_rebuild = Instant::now();
		checkpoint!("fingerprint_index_rebuilt", session_count = self.fingerprint_index.len());
		Ok(())
	}

	/// Cache a fingerprint for a window
	///
	/// Called after successful fingerprint matching to avoid re-extraction
	#[tracing::instrument(skip(self, fingerprint), fields(addr = %addr.short(), cache_size = self.window_fingerprints.len()))]
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
	pub fn find_window_by_id(&self, id: u64) -> Option<&ClaudePane> {
		self.windows.values().find(|w| w.id() == id)
	}

	/// Find a window by its kitty ID (mutable, searches across all sockets)
	pub fn find_window_by_id_mut(&mut self, id: u64) -> Option<&mut ClaudePane> {
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
	/// Window workspace changed (from wnck signal) - triggers immediate poll
	WorkspaceChange { platform_window_id: u64 },
	/// Shutdown signal
	Shutdown,
}

// ═══════════════════════════════════════════════════════════════════════════════
// Daemon Entry Point
// ═══════════════════════════════════════════════════════════════════════════════

/// Run the daemon with trace mode enabled
/// This enables debug-level logging for detailed fingerprint matching traces
#[vtr::trace_errors]
pub async fn run_daemon_traced() -> Result<()> {
	TRACE.store(true, Ordering::Relaxed);
	checkpoint!("trace_mode_enabled");
	run_daemon().await
}

/// Run the daemon
#[vtr::trace_errors]
pub async fn run_daemon() -> Result<()> {
	// ─── Startup Banner ─────────────────────────────────────────────────────────
	checkpoint!("startup", version = env!("CARGO_PKG_VERSION"));

	// ─── Kitty Config Validation ────────────────────────────────────────────────
	// Babel requires kitty with remote control enabled. Validate config on startup
	// and abort with helpful error message if misconfigured.
	let kitty_config = crate::kitty::validate_kitty_config()
		.map_err(|e| anyhow::anyhow!("Kitty config validation failed:\n{}", e))?;

	checkpoint!(
		"kitty_validated",
		config = kitty_config.config_path.display().to_string(),
		socket_base = format!("{:?}", kitty_config.listen_on_base)
	);

	// Log socket topology - helps diagnose multi-instance issues
	let sockets = crate::kitty::find_all_sockets();
	let main_socket = crate::kitty::main_socket();
	let orphan_count = sockets.len().saturating_sub(1);

	if orphan_count > 0 {
		trace_error!(
			"multiple kitty instances detected",
			main = ?main_socket,
			orphans = orphan_count
		);
	} else if let Some(ref main) = main_socket {
		checkpoint!("kitty_socket", socket = main.as_str());
	} else {
		trace_error!("no kitty sockets found");
	}

	// Initialize state
	let state = Arc::new(RwLock::new(BabelState::new()));

	// Initialize workspace summarizer
	let summarizer = Arc::new(crate::summarizer::WorkspaceSummarizer::new());

	// ─── Initial Indexing ───────────────────────────────────────────────────────
	{
		let mut s = state.write().await;
		s.rebuild_summary_index().context("Failed to build summary index")?;
		s.rebuild_fingerprint_index().context("Failed to build fingerprint index")?;
		let _ = s.refresh_windows(false).await.context("Failed initial window scan")?;

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

			checkpoint!("file_watcher_ready", path = projects_dir.display().to_string());

			for result in rx {
				match result {
					Ok(events) => {
						for event in events {
							if event.kind == DebouncedEventKind::Any {
								let _ = rt.block_on(watch_tx.send(DaemonEvent::FileChange(event.path)));
							}
						}
					}
					Err(e) => trace_error!("file watcher error", error = ?e),
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

	// Spawn wnck workspace change watcher (instant detection, no polling)
	// Runs GLib main loop in separate thread, sends events on workspace changes
	let wnck_tx = event_tx.clone();
	let rt = tokio::runtime::Handle::current();
	std::thread::spawn(move || {
		spawn_wnck_watcher(wnck_tx, rt);
	});

	// ─── IPC Socket ──────────────────────────────────────────────────────────────
	let listener = create_listener().await?;
	let socket_path = crate::utility::ipc::socket_path();
	checkpoint!("ipc_listening", socket = socket_path.display().to_string());

	// ─── Ready ──────────────────────────────────────────────────────────────────
	checkpoint!("daemon_ready");

	// Main event loop
	loop {
		tokio::select! {
            // Handle IPC connections
            Ok((stream, _)) = listener.accept() => {
                let state = Arc::clone(&state);
                let summarizer = Arc::clone(&summarizer);
                tokio::spawn(async move {
                    if let Err(e) = handle_client(stream, state, summarizer).await {
                        trace_error!("IPC client error", error = %e);
                    }
                });
            }

            // Handle internal events
            Some(event) = event_rx.recv() => {
                match event {
                    DaemonEvent::KittyPoll => {
                        // Generate correlation ID for this poll cycle
                        let poll_trace_id = generate_trace_id();
                        let poll_span = tracing::debug_span!("poll_cycle", trace_id = %poll_trace_id);
                        let _poll_guard = poll_span.enter();

                        // Phase 1: Quick structural refresh with lock (skip activity I/O)
                        // This returns IMMEDIATELY so IPC requests aren't blocked
                        let (changed_workspaces, window_addrs) = {
                            let mut s = state.write().await;
                            let ws = match s.refresh_windows(true).await { // Skip activity fetch!
                                Ok(ws) => ws,
                                Err(e) => {
                                    trace_error!("window refresh failed", error = %e);
                                    vec![]
                                }
                            };
                            let addrs: Vec<_> = s.windows.keys().cloned().collect();
                            (ws, addrs)
                        }; // Lock released - main loop continues immediately!

                        // Spawn background task for slow I/O operations
                        // This keeps the main select! loop responsive to IPC
                        let state_clone = Arc::clone(&state);
                        let summarizer_clone = Arc::clone(&summarizer);
                        let bg_trace_id = poll_trace_id.clone();
                        tokio::spawn(async move {
                            let bg_span = tracing::debug_span!("poll_background", trace_id = %bg_trace_id);
                            let _bg_guard = bg_span.enter();
                            // Phase 2: Fetch activity states (expensive, ~100ms/window)
                            // Using join_all for concurrent async I/O across all windows
                            let activity_futures: Vec<_> = window_addrs.iter()
                                .map(|addr| {
                                    let addr = addr.clone();
                                    async move {
                                        // Use socket-aware function to handle multi-instance kitty
                                        let activity = get_activity_with_scrollback_on_socket(&addr).await;
                                        (addr, activity)
                                    }
                                })
                                .collect();
                            let activity_results = futures::future::join_all(activity_futures).await;

                            // Phase 3: Apply activity updates with quick write lock
                            {
                                let mut s = state_clone.write().await;
                                for (addr, activity) in activity_results {
                                    s.apply_activity_update(&addr, activity.state, activity.asking_question, activity.scrollback).await;
                                }
                            }

                            // Phase 4: Get windows needing fingerprints + copy index + claimed sessions
                            let (needs_matching, fingerprint_index, claimed_sessions) = {
                                let s = state_clone.read().await;
                                let needs = s.get_windows_needing_fingerprints();
                                let index = s.fingerprint_index.clone();
                                let claimed = s.get_claimed_sessions();
                                checkpoint!(
                                    "phase_4",
                                    needs_count = needs.len(),
                                    index_size = index.len(),
                                    claimed_count = claimed.len()
                                );
                                (needs, index, claimed)
                            };

                            // Phase 5: Do fingerprint matching I/O (expensive)
                            // Pass kitty CWD (reliable) instead of extracting from scrollback
                            // Pass claimed_sessions to exclude from "unique CWD" matching
                            if !needs_matching.is_empty() {
                                checkpoint!("phase_5", windows = needs_matching.len());
                            }
                            let fingerprint_futures: Vec<_> = needs_matching.iter()
                                .map(|(addr, cwd)| {
                                    let addr = addr.clone();
                                    let cwd = cwd.clone();
                                    let fingerprint_index = fingerprint_index.clone();
                                    let claimed_sessions = claimed_sessions.clone();
                                    async move {
                                        BabelState::fingerprint_match_addr(&addr, &cwd, &fingerprint_index, &claimed_sessions).await
                                            .map(|(session_id, confidence, fingerprint)| {
                                                (addr, session_id, confidence, fingerprint)
                                            })
                                    }
                                })
                                .collect();
                            let fingerprint_results: Vec<_> = futures::future::join_all(fingerprint_futures).await
                                .into_iter()
                                .flatten()
                                .collect();

                            // Phase 6: Apply fingerprint results with quick write lock
                            if !fingerprint_results.is_empty() {
                                let mut s = state_clone.write().await;
                                for (addr, session_id, confidence, fingerprint) in fingerprint_results {
                                    s.apply_fingerprint_result(&addr, session_id, confidence, fingerprint);
                                }
                            }

                            // Phase 7: Summarization for changed workspaces
                            if !changed_workspaces.is_empty() {
                                for ws in changed_workspaces {
                                    summarize_workspace(ws, &state_clone, &summarizer_clone).await;
                                }
                            }
                        });
                    }
                    DaemonEvent::FileChange(path) => {
                        // Rebuild fingerprint index on JSONL changes
                        // Note: This could be optimized with a last_rebuild timestamp check
                        if path.extension().and_then(|s| s.to_str()) == Some("jsonl") {
                            let mut s = state.write().await;
                            let _ = s.rebuild_fingerprint_index();
                        }
                    }
                    DaemonEvent::WorkspaceChange { platform_window_id } => {
                        // Instant workspace change from wnck - trigger immediate poll
                        // Use skip_activity_fetch=true for fast response (activity updates via periodic poll)
                        tracing::debug!(platform_window_id, "WNCK workspace change detected");
                        let changed_workspaces = {
                            let mut s = state.write().await;
                            s.refresh_windows(true).await.unwrap_or_default()
                        };
                        // Trigger summarization for affected workspaces
                        if !changed_workspaces.is_empty() {
                            let summarizer = Arc::clone(&summarizer);
                            let state = Arc::clone(&state);
                            // Correlation ID for this workspace change batch
                            let ws_trace_id = generate_trace_id();
                            tokio::spawn(async move {
                                let ws_span = tracing::debug_span!("workspace_summarization", trace_id = %ws_trace_id);
                                let _ws_guard = ws_span.enter();
                                for ws in changed_workspaces {
                                    summarize_workspace(ws, &state, &summarizer).await;
                                }
                            });
                        }
                    }
                    DaemonEvent::Shutdown => {
                        checkpoint!("shutdown_received");
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
			trace_error!("workspace summarization failed", workspace, error = %e);
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
				trace_error!("subscriber lagged", subscriber_id, dropped_events = n);
			}
			Err(tokio::sync::broadcast::error::RecvError::Closed) => {
				trace_error!("event broadcast closed");
				break;
			}
		}
	}

	Ok(())
}

#[vtr::trace_errors]
async fn handle_client(
	mut stream: UnixStream,
	state: Arc<RwLock<BabelState>>,
	summarizer: Arc<crate::summarizer::WorkspaceSummarizer>,
) -> Result<()> {
	// Generate correlation ID for this request - enables tracing across all operations
	let trace_id = generate_trace_id();
	let span = tracing::info_span!("ipc_request", %trace_id);
	let _guard = span.enter();

	let mut reader = BufReader::new(&mut stream);
	let mut line = String::new();

	// Read one request
	let bytes_read = reader.read_line(&mut line).await?;
	if bytes_read == 0 {
		return Ok(()); // Connection closed
	}

	let request: Request = serde_json::from_str(&line)
		.context("Failed to parse client request")?;

	boundary!("ipc", "request", request = std::any::type_name_of_val(&request));

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
	use crate::kitty::get_window_geometry;

	// ═══════════════════════════════════════════════════════════════════════════════
	// Screen Position Sorting
	// ═══════════════════════════════════════════════════════════════════════════════
	//
	// Connectors (panel plugins, richmon, etc.) expect windows ordered by their
	// on-screen position: left→right, then top→bottom. This matches the visual
	// layout users see and enables position-based UI (e.g., workspace indicators).

	/// Sort items by screen position (left→right, then top→bottom)
	///
	/// Two-tier geometry lookup:
	/// 1. **Pane-level geometry** (preferred): Uses `get_pane_screen` callback to extract
	///    kitty's `screen.x/y` from items - provides per-pane coordinates even for split panes
	///    within the same OS window. Available on patched kitty with geometry fields.
	/// 2. **OS window geometry** (fallback): Uses xdotool to get platform_window_id position.
	///    All panes in the same OS window share the same coordinates with this method.
	///
	/// This gracefully degrades: new kitty with geometry → pane-level sort,
	/// old kitty without geometry → OS window-level sort (existing behavior).
	///
	/// Performance: O(n) xdotool calls where n = unique platform_window_ids without pane geometry.
	fn sort_by_screen_position<T, F, G>(items: &mut [T], get_platform_id: F, get_pane_screen: G)
	where
		F: Fn(&T) -> u64,
		G: Fn(&T) -> Option<(i32, i32)>,
	{
		use std::collections::HashMap;

		// Cache OS window geometries for items without pane-level geometry
		// Only calls xdotool for unique platform_window_ids that we actually need
		let mut os_geometry_cache: HashMap<u64, (i32, i32)> = HashMap::new();

		// Build per-item geometry: prefer pane screen, fallback to OS window
		let geometries: Vec<(i32, i32)> = items.iter().map(|item| {
			// First try pane-level geometry (from patched kitty)
			if let Some((x, y)) = get_pane_screen(item) {
				return (x, y);
			}

			// Fallback: OS window geometry via xdotool
			let pid = get_platform_id(item);
			*os_geometry_cache.entry(pid).or_insert_with(|| {
				get_window_geometry(pid)
					.map(|g| (g.x, g.y))
					.unwrap_or((i32::MAX, i32::MAX))
			})
		}).collect();

		// Create indices and sort by geometry
		let mut indices: Vec<usize> = (0..items.len()).collect();
		indices.sort_by_key(|&i| geometries[i]);

		// Reorder items in-place using the sorted indices
		// Uses a permutation cycle approach to avoid extra allocation
		let mut sorted = vec![false; items.len()];
		for i in 0..items.len() {
			if sorted[i] {
				continue;
			}
			let mut j = i;
			while indices[j] != i {
				let next = indices[j];
				items.swap(j, next);
				sorted[j] = true;
				j = next;
			}
			sorted[j] = true;
		}
	}

	/// List all Claude panes with activity states
	///
	/// Windows are sorted by screen position (left→right, then top→bottom)
	/// to ensure consistent ordering for panel plugins and other connectors.
	pub async fn list(state: &Arc<RwLock<BabelState>>) -> Response {
		let s = state.read().await;

		// Fetch hook states from database for sessions we know about
		let hook_states: std::collections::HashMap<String, crate::babel_storage::HookState> =
			if let Ok(conn) = init_db() {
				s.windows.values()
					.filter_map(|w| w.session_id.as_ref())
					.filter_map(|sid| {
						crate::babel_storage::get_hook_state(&conn, sid)
							.ok()
							.flatten()
							.map(|state| (sid.clone(), state))
					})
					.collect()
			} else {
				std::collections::HashMap::new()
			};

		let mut windows: Vec<ClaudePane> = s.windows.values()
			.map(|w| {
				let mut win = w.clone();
				win.activity_state = s.get_activity_state(w.id());
				// Populate hook_state from database if we have a session_id
				if let Some(ref sid) = w.session_id {
					win.hook_state = hook_states.get(sid).copied();
				}
				win
			})
			.collect();
		drop(s); // Release lock before geometry lookups

		// ClaudePane doesn't carry pane geometry; fallback to OS window
		sort_by_screen_position(&mut windows, |w| w.platform_window_id, |_| None);
		Response::Windows { windows }
	}

	/// List all terminals (Claude and non-Claude)
	///
	/// Terminals are sorted by screen position (left→right, then top→bottom)
	/// to ensure consistent ordering for panel plugins and other connectors.
	pub async fn list_terminals(state: &Arc<RwLock<BabelState>>) -> Response {
		let s = state.read().await;
		let mut terminals: Vec<TerminalInfo> = s.terminals.values().cloned().collect();
		drop(s); // Release lock before geometry lookups

		// TerminalInfo doesn't carry pane geometry; fallback to OS window
		sort_by_screen_position(&mut terminals, |t| t.platform_window_id, |_| None);
		Response::Terminals { terminals }
	}

	/// List raw kitty panes from all sockets (no enrichment)
	///
	/// Unlike list_terminals, this returns raw KittyPane data directly from kitty,
	/// without any babel enrichment. Queries all responsive kitty instances.
	///
	/// Panes are sorted by screen position (left→right, then top→bottom)
	/// to ensure consistent ordering for panel plugins and other connectors.
	pub async fn list_panes() -> Response {
		match crate::kitty::list_all_panes().await {
			Ok(mut panes) => {
				// Use pane-level screen geometry when available (patched kitty)
				// Falls back to OS window geometry for older kitty versions
				sort_by_screen_position(
					&mut panes,
					|p| p.platform_window_id,
					|p| p.screen.as_ref().map(|s| (s.x, s.y)),
				);
				Response::Panes { panes }
			}
			Err(e) => Response::Error { message: format!("Failed to list panes: {}", e) },
		}
	}

	/// List kitty socket status (multi-instance awareness)
	pub async fn list_sockets(state: &Arc<RwLock<BabelState>>) -> Response {
		let s = state.read().await;
		Response::Sockets { sockets: s.socket_status.clone() }
	}

	/// List windows with fingerprints (expensive - extracts from scrollback)
	///
	/// Windows are sorted by screen position (left→right, then top→bottom)
	/// to ensure consistent ordering for panel plugins and other connectors.
	pub async fn list_with_fingerprints(state: &Arc<RwLock<BabelState>>) -> Response {
		let s = state.read().await;
		let mut windows: Vec<ClaudePane> = s.windows.values()
			.map(|w| {
				let mut win = w.clone();
				win.activity_state = s.get_activity_state(w.id());
				win
			})
			.collect();
		drop(s); // Release lock before expensive operations

		for win in &mut windows {
			if win.fingerprint.is_none() {
				if let Ok(scrollback) = get_scrollback(win.id()).await {
					let fp = extract_from_scrollback(&scrollback);
					win.fingerprint = Some(fp);
				}
			}
			if win.session_info.is_none() {
				let _ = enrich_window(win);
			}
		}

		// Sort after fingerprinting (geometry lookup is cheap compared to scrollback)
		// ClaudePane doesn't carry pane geometry; fallback to OS window
		sort_by_screen_position(&mut windows, |w| w.platform_window_id, |_| None);
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
	pub async fn focus(window_id: u64) -> Response {
		match focus_window_any(window_id).await {
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
	pub async fn scroll(window_id: u64) -> Response {
		match get_scrollback_any(window_id).await {
			Ok(result) => {
				if result.is_non_current {
					trace_error!("scrollback from non-current socket", window_id, addr = %result.addr.short());
				}
				Response::Scrollback { text: result.result }
			}
			Err(e) => Response::Error { message: format!("Scroll failed: {}", e) },
		}
	}

	/// Send text to a window (with Enter/CR)
	pub async fn send(window_id: u64, text: &str) -> Response {
		// Append CR to submit the text
		let text_with_cr = format!("{}\r", text);
		match send_text_any(window_id, &text_with_cr).await {
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

	/// Type text to a window without pressing Enter
	///
	/// Unlike `send`, this doesn't append a carriage return, so the text
	/// is typed into the input area but not submitted.
	pub async fn type_text(window_id: u64, text: &str) -> Response {
		// No CR - just type the text as-is
		match send_text_any(window_id, text).await {
			Ok(result) => {
				let message = if result.is_non_current {
					format!("⚠ Typed to window {} on non-current socket: {}", window_id, result.addr.short())
				} else {
					format!("Typed to window {}", window_id)
				};
				Response::Ok { message }
			}
			Err(e) => Response::Error { message: format!("Type failed: {}", e) },
		}
	}

	/// Check if a window has pending (unsent) input
	///
	/// Retrieves the scrollback and analyzes the last line to detect
	/// text typed but not yet submitted.
	///
	/// TODO: Integrate with scrollparse for more robust detection:
	/// - Detect multiline input
	/// - Handle plan mode selection UI
	/// - Support save/restore of pending input during broadcast
	pub async fn has_pending_input(window_id: u64) -> Response {
		match get_scrollback_any(window_id).await {
			Ok(result) => {
				let (has_pending, pending_text) = detect_pending_input_from_scrollback(&result.result);
				Response::PendingInput {
					window_id,
					has_pending,
					pending_text,
				}
			}
			Err(e) => Response::Error { message: format!("Failed to check pending input: {}", e) },
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
		// Phase 1: Refresh windows with full activity fetch (explicit refresh request)
		let window_count = {
			let mut s = state.write().await;
			match s.refresh_windows(false).await { // Full refresh with activity states
				Ok(_) => s.windows.len(),
				Err(e) => return Response::Error { message: format!("Refresh failed: {}", e) }
			}
		};

		// Phase 2: Get windows needing fingerprints + index + claimed sessions
		let (needs_matching, fingerprint_index, mut claimed_sessions) = {
			let s = state.read().await;
			(s.get_windows_needing_fingerprints(), s.fingerprint_index.clone(), s.get_claimed_sessions())
		};

		// Phase 3: Do expensive I/O without lock
		// Pass kitty CWD (reliable) instead of extracting from scrollback
		// Sequential processing: update claimed_sessions as we go to avoid double-matching
		for (addr, cwd) in needs_matching {
			if let Some((session_id, confidence, fingerprint)) =
				BabelState::fingerprint_match_addr(&addr, &cwd, &fingerprint_index, &claimed_sessions).await
			{
				// Mark this session as claimed before processing next window
				claimed_sessions.insert(session_id.clone());
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
					effect!("wset", "set_current_name", error = e.to_string());
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
			effect!("wset", "set_current_name", error = e.to_string());
		}

		{
			let mut s = state.write().await;
			let _ = s.refresh_windows(true).await; // Quick refresh, activity via periodic poll
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
		Request::ListPanes => handlers::list_panes().await,
		Request::ListSockets => handlers::list_sockets(state).await,
		Request::ListWithFingerprints => handlers::list_with_fingerprints(state).await,
		Request::Status { window_id } => handlers::status(state, window_id).await,
		Request::History { limit } => handlers::history(limit),
		Request::Ping => handlers::ping(state).await,
		Request::Titles => handlers::titles(state).await,

		// ─── Window Handlers ────────────────────────────────────────────────────
		Request::Enrich { window_id } => handlers::enrich(state, window_id).await,
		Request::Focus { window_id } => handlers::focus(window_id).await,
		Request::Scroll { window_id } => handlers::scroll(window_id).await,
		Request::Send { window_id, text } => {
			let response = handlers::send(window_id, &text).await;
			// Trigger workspace re-summarization on user prompt
			// User just sent new instructions to Claude, context is changing
			if matches!(response, Response::Ok { .. }) {
				let workspace = {
					let s = state.read().await;
					s.windows.values()
						.find(|w| w.id() == window_id)
						.and_then(|w| w.workspace)
				};
				if let Some(ws) = workspace {
					summarize_workspace(ws, state, summarizer).await;
				}
			}
			response
		}
		Request::Type { window_id, text } => handlers::type_text(window_id, &text).await,
		Request::HasPendingInput { window_id } => handlers::has_pending_input(window_id).await,

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

		// Solo mode (debugging feature - isolate single pane)
		// TODO: Implement solo mode handler when needed
		Request::Solo { window_id } => Response::Error {
			message: format!("Solo mode not yet implemented (window_id: {:?})", window_id),
		},

		// Subscribe is handled specially in handle_client
		Request::Subscribe { .. } => Response::Error {
			message: "Subscribe requests must be handled via handle_client".to_string(),
		}
	}
}

// ═══════════════════════════════════════════════════════════════════════════════
// Pending Input Detection
// ═══════════════════════════════════════════════════════════════════════════════

/// Detect if there's pending (unsent) input in a Claude Code terminal scrollback
///
/// Analyzes the last line of scrollback to detect text after the prompt.
/// Returns (has_pending, pending_text) where pending_text is the actual text
/// if it can be extracted.
///
/// # Detection Strategy
///
/// Claude Code's prompt is typically `> ` at the start of a line. If there's
/// text after `> ` on the last non-empty line, that's pending input.
///
/// # Current Limitations
///
/// - Only detects simple `> text` patterns
/// - May miss multiline input or complex prompt states
/// - Cannot distinguish between prompt and shell output in some edge cases
///
/// TODO: Integrate with scrollparse for more robust detection:
/// - Detect multiline input (continuation prompts)
/// - Handle plan mode input (`y/n`, selection UI)
/// - Support save/restore of pending input during broadcast
/// - Detect input in different prompt modes (shell, edit, etc.)
fn detect_pending_input_from_scrollback(scrollback: &str) -> (bool, Option<String>) {
    // Get the last non-empty lines
    let lines: Vec<&str> = scrollback
        .lines()
        .rev()
        .filter(|l| !l.trim().is_empty())
        .take(3)
        .collect();

    if lines.is_empty() {
        return (false, None);
    }

    let last_line = lines[0].trim();

    // Check for Claude Code prompt with content: `> something`
    // The prompt is `> ` followed by user input
    if let Some(content) = last_line.strip_prefix("> ") {
        let trimmed = content.trim();
        if !trimmed.is_empty() {
            return (true, Some(trimmed.to_string()));
        }
        // Just `> ` with no content - no pending input
        return (false, None);
    }

    // Check for bare prompt endings that might have content
    // These are edge cases where the prompt indicator is at the end
    if last_line.ends_with('>') && !last_line.ends_with("->") {
        // Might be a prompt line without visible cursor content
        // Can't extract the pending text reliably here
        // TODO: scrollparse integration needed for accurate detection
        return (false, None);
    }

    // Check if we're in a state where input is being typed but the line
    // doesn't start with `> ` (possible continuation or shell state)
    //
    // TODO: scrollparse can help here by:
    // - Detecting SessionState (is it at a prompt? in plan mode?)
    // - Parsing the visible UI elements to understand context
    // - Handling continuation lines in multiline input

    (false, None)
}

// ═══════════════════════════════════════════════════════════════════════════════
// WNCK Workspace Change Watcher
// ═══════════════════════════════════════════════════════════════════════════════

/// Spawn wnck watcher for instant workspace change detection
///
/// Runs a GLib main loop in the current thread, watching for window workspace
/// changes via libwnck signals. Much faster than polling - events fire instantly
/// when windows move between workspaces.
///
/// # Architecture
///
/// - Connects to wnck Screen's "window-opened" signal
/// - For each window, connects to "workspace-changed" signal
/// - Sends DaemonEvent::WorkspaceChange through the tokio channel
/// - Runs GLib main loop (blocking) to process signals
fn spawn_wnck_watcher(tx: mpsc::Sender<DaemonEvent>, rt: tokio::runtime::Handle) {
    use std::cell::RefCell;
    use std::collections::HashSet;
    use std::rc::Rc;

    // Initialize GTK (required for wnck)
    if gtk::init().is_err() {
        trace_error!("failed to initialize GTK for wnck watcher");
        return;
    }

    // Set client type before getting screen
    wnck_rs::set_client_type(wnck_rs::ClientType::Pager);

    let screen = match wnck_rs::Screen::get_default() {
        Some(s) => s,
        None => {
            trace_error!("failed to get wnck screen");
            return;
        }
    };

    // Force initial update to get current windows
    screen.force_update();

    // Track windows we've connected signals to (avoid duplicates)
    let connected_windows: Rc<RefCell<HashSet<u64>>> = Rc::new(RefCell::new(HashSet::new()));

    // Helper to connect workspace-changed signal to a window
    let connect_window = {
        let tx = tx.clone();
        let rt = rt.clone();
        let connected = Rc::clone(&connected_windows);

        move |window: wnck_rs::Window| {
            let xid = window.xid();

            // Skip if already connected
            if !connected.borrow_mut().insert(xid) {
                return;
            }

            let tx = tx.clone();
            let rt = rt.clone();

            window.connect_workspace_changed(move || {
                tracing::trace!(xid, "WNCK: workspace-changed signal");
                let event = DaemonEvent::WorkspaceChange { platform_window_id: xid };
                let _ = rt.block_on(tx.send(event));
            });
        }
    };

    // Connect to existing windows
    for window in screen.get_windows() {
        connect_window(window);
    }

    // Connect to new windows as they open
    let connect_fn = connect_window.clone();
    screen.connect_window_opened(move |window| {
        tracing::trace!(xid = window.xid(), "WNCK: window-opened");
        connect_fn(window);
    });

    checkpoint!("wnck_watcher_ready");

    // Run GLib main loop (blocks forever, processing signals)
    let main_loop = glib::MainLoop::new(None, false);
    main_loop.run();
}
