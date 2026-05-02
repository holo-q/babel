//! Babel Daemon - The watchful eye atop the tower
//!
//! The daemon observes all agent panes in the tower, maintaining awareness of
//! their states, conversations, and movements between workspaces. It speaks to no
//! one directly—it only watches and reports. When the Captain arrives, this daemon
//! will serve as their eyes: the ground truth of who is working, who awaits input,
//! who has fallen silent. For now, it simply keeps the tower's ledger.
//!
//! The daemon maintains:
//! - Live mapping of kitty panes → agent sessions
//! - Cached session info from ~/.claude
//! - Summary index for fast matching
//!
//! It watches:
//! - Kitty panes (polling every 1000ms)
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

    /// Interval between kitty window polls (2000ms = 0.5 Hz)
    ///
    /// Structural heartbeat only — discovers new/closed panes, focus changes,
    /// and pane↔session binding. Real-time activity state comes from Claude Code
    /// hooks (Stop/Prompt/PreTool/PostTool), workspace changes from wnck signals.
    /// 2s latency for window discovery is imperceptible for a monitoring daemon.
    pub const KITTY_POLL_INTERVAL: Duration = Duration::from_millis(2000);

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

use crate::babel_storage::{init_db, mark_read, mark_unread, set_icon};
use crate::events::{BabelEvent, EventFilter, EventMessage, EventPublisher};
use crate::fingerprint::{
    extract_from_jsonl, extract_from_scrollback, match_fingerprints, MatchConfidence,
    SessionFingerprint,
};
use crate::indicator::IndicatorEvent;
use crate::kitty::{PaneAddr, PaneSelector};
use crate::kitty::{
    default_socket,
    discover_all_instances,
    focus_pane_any,
    focus_pane_on_socket,
    get_scrollback, // used in fingerprint_match_addr
    get_scrollback_any,
    get_scrollback_on_socket,
    reset_border_color_on_socket,
    send_text_any,
    send_text_on_socket,
    set_border_color_on_socket,
};
use crate::paint::{
    resolve_color, workspace_css_class, workspace_is_urgent, PaintEvent, WorkspacePaintEvent,
};
use crate::service::activity::hook_state_activity;
use crate::utility::agent_discovery::{
    detect_agent_signals, get_activity_with_scrollback_on_socket, get_pane_activity_with_scrollback,
};
use crate::utility::agent_discovery::{enrich_pane, load_wset, AgentPane};
use crate::utility::claude_storage::{claude_base, get_recent_sessions, get_session_info};
use crate::utility::ipc::{create_listener, Request, Response};
use crate::wset::{get_current_wset_name, list_wsets, set_current_wset_name, WSet};
use crate::{AgentKind, PulseEffect};
use vtr::trace::{generate_trace_id, with_trace_id, RingBuffer, TraceSnapshot, VtrLayer};
use vtr::{boundary, checkpoint, effect, state, trace_error};

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
    use spaceship_std::CompactingJournaldLayer;
    use std::io::IsTerminal;
    use tracing_subscriber::{layer::SubscriberExt, reload, util::SubscriberInitExt, EnvFilter};
    use vtr::trace::CompactingStderrLayer;

    let debug = args.debug;
    // Enable trace level when debug mode is on so instrumented spans with context fields
    // (like kitty_id) are created and the context column shows actual values
    let filter_str = if debug {
        "trace".to_string()
    } else {
        spaceship_std::logging::load_level("babel", "babel")
    };

    let filter = EnvFilter::new(&filter_str);
    let (filter_layer, reload_handle) = reload::Layer::new(filter);

    // Create VtrLayer and store buffer handle
    let vtr_layer = VtrLayer::new(VTR_BUFFER_CAPACITY);
    let buffer_handle = vtr_layer.buffer();
    VTR_BUFFER
        .set(buffer_handle)
        .expect("VTR_BUFFER already initialized");

    let is_tty = std::io::stderr().is_terminal();

    // Use exactly ONE output layer - using both creates duplicate entries in journald
    // (stderr via systemd capture + direct journal_send).
    //
    // TTY/debug mode: CompactingStderrLayer for visual output with depth markers and colors
    // Daemon mode: CompactingJournaldLayer with structured VTR fields for view-time collapse
    if is_tty || debug {
        let stderr_layer = CompactingStderrLayer::new()
            .with_color(true)
            .with_context_field("kitty_id")
            .with_context_width(4);

        tracing_subscriber::registry()
            .with(filter_layer)
            .with(stderr_layer)
            .with(vtr_layer)
            .init();
    } else {
        // Daemon mode: structured journald fields for view-time collapse
        // VTR_KIND, VTR_DEPTH, VTR_SPAN_ID, VTR_STRUCTURAL_HASH enable spacejn
        // to reconstruct span trees and apply block-wide collapse.
        let journald_layer = CompactingJournaldLayer::new()
            .expect("Failed to connect to journald")
            .with_syslog_identifier("babel".to_string());

        tracing_subscriber::registry()
            .with(filter_layer)
            .with(journald_layer)
            .with(vtr_layer)
            .init();
    }

    // Spawn SIGHUP handler for hot-reload (skip in debug mode - fixed level)
    if !debug {
        std::thread::spawn(move || {
            use signal_hook::iterator::Signals;
            let mut signals = Signals::new(&[signal_hook::consts::SIGHUP]).unwrap();
            for _ in signals.forever() {
                let new_filter_str = spaceship_std::logging::load_level("babel", "babel");
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

/// Lightweight terminal info for tracking all kitty panes (not just agents).
///
/// This enables the TUI/monitor to see the full terminal flow - watching
/// terminals open, close, and transition to agent sessions.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct TerminalInfo {
    /// Unique address of this pane (socket + kitty ID)
    pub addr: PaneAddr,
    /// Pane title
    pub title: String,
    /// Working directory
    pub cwd: PathBuf,
    /// Best-effort foreground command reported by kitty.
    ///
    /// This is diagnostic only: terminal TUIs often run under a shell, so the
    /// command can be "fish"/"bash" even when the pane is hosting an agent.
    #[serde(default)]
    pub foreground_command: Option<String>,
    /// XFCE workspace number
    pub workspace: Option<i32>,
    /// Whether this terminal is running a tracked agent.
    #[serde(alias = "is_claude")]
    pub is_agent: bool,
    /// Whether this is the focused pane
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
        use std::collections::hash_map::DefaultHasher;
        use std::hash::{Hash, Hasher};

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
        let recent_bytes: usize = self
            .recent_deltas
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
    /// Current agent panes (PaneAddr → AgentPane)
    /// Keyed by PaneAddr to handle ID collisions across kitty instances
    pub panes: HashMap<PaneAddr, AgentPane>,

    /// All kitty terminals (PaneAddr → TerminalInfo)
    /// Includes both agent and non-agent terminals for full visibility
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
    pub pane_fingerprints: HashMap<PaneAddr, SessionFingerprint>,

    /// Cached activity states for windows (PaneAddr → ActivityState)
    /// Used to detect state changes and emit SessionStateChanged events
    /// Tracking each worker's breath—their current state in the cycle
    pub pane_states: HashMap<PaneAddr, scrollparse::claude::ActivityState>,

    /// Scrollback activity tracking for ActivityPulse events (PaneAddr → ScrollbackActivity)
    /// Tracks content hashes and deltas to detect token output / tool execution
    pub pane_activity: HashMap<PaneAddr, ScrollbackActivity>,

    /// When the daemon started
    pub start_time: Instant,

    /// Last time we scanned kitty windows
    pub last_kitty_scan: Instant,

    /// Last time we rebuilt the fingerprint index (debounce frequent rebuilds)
    pub last_fingerprint_rebuild: Instant,

    /// Event publisher for push notifications
    pub event_publisher: EventPublisher,

    /// Paint stream publisher — fully-resolved UX commands for puppet clients.
    ///
    /// Parallel to event_publisher, but carries `PaintEvent`s
    /// (color/ring/CSS class) instead of semantic `BabelEvent`s. Capacity
    /// 256 — paint events can spike during ActivityPulse rings, but each
    /// is small; the broader window allows panel restarts to catch up.
    pub paint_publisher: tokio::sync::broadcast::Sender<PaintEvent>,

    /// Per-pane ring glow intensity (PaneAddr → 0.0..1.0).
    /// Bumped by ActivityPulse, decayed by the pulse-decay loop.
    /// The aura around each worker dot — bright on active output, fading
    /// to quiet when the worker rests.
    pub pane_ring: HashMap<PaneAddr, f64>,

    /// Pane-local unread state (PaneAddr).
    ///
    /// Storage remains the durable read/unread source of truth. This cache is
    /// the daemon's live paint hint: unread survives pulse decay and clears on
    /// focus/read transitions, so a finished agent can be idle while still
    /// carrying a visible ring until the pane is heard.
    pub pane_unread: HashSet<PaneAddr>,

    /// Per-workspace awaiting timer — when did the workspace enter
    /// AwaitingInput? None when no pane on the workspace is awaiting.
    /// Drives the WorkspacePaintEvent.awaiting_seconds field for tooltips.
    pub workspace_awaiting_since: HashMap<i32, Instant>,

    /// Current workspace titles (workspace → title)
    /// Authoritative source - frontends query this via IPC
    pub workspace_titles: HashMap<i32, String>,

    /// Known kitty sockets and their status
    /// Populated during window refresh to track multi-socket situations
    pub socket_status: HashMap<String, SocketStatus>,

    /// Cached workspace mappings (platform_window_id → workspace number)
    /// Invalidated on wnck WorkspaceChange events. Eliminates `wmctrl -l` subprocess
    /// on every poll tick — workspace mappings only change on window move/workspace switch.
    pub workspace_cache: HashMap<u64, i32>,

    /// Whether workspace_cache needs refresh (dirty on WorkspaceChange, startup)
    pub workspace_cache_dirty: bool,
}

impl Default for BabelState {
    fn default() -> Self {
        Self::new()
    }
}

impl BabelState {
    pub fn new() -> Self {
        // Paint broadcast capacity 256 — generous so panel restarts that race
        // a burst of pulses still catch up before lagging.
        let (paint_tx, _) = tokio::sync::broadcast::channel(256);
        Self {
            panes: HashMap::new(),
            terminals: HashMap::new(),
            summary_index: Vec::new(),
            fingerprint_index: HashMap::new(),
            session_paths: HashMap::new(),
            pane_fingerprints: HashMap::new(),
            pane_states: HashMap::new(),
            pane_activity: HashMap::new(),
            start_time: Instant::now(),
            last_kitty_scan: Instant::now(),
            last_fingerprint_rebuild: Instant::now(),
            event_publisher: EventPublisher::new(),
            paint_publisher: paint_tx,
            pane_ring: HashMap::new(),
            pane_unread: HashSet::new(),
            workspace_awaiting_since: HashMap::new(),
            workspace_titles: HashMap::new(),
            socket_status: HashMap::new(),
            workspace_cache: HashMap::new(),
            workspace_cache_dirty: true, // Force first poll to populate
        }
    }

    // ═══════════════════════════════════════════════════════════════════════
    // Paint Stream Emission
    // ═══════════════════════════════════════════════════════════════════════
    //
    // Babel is authoritative over UX. Every state change that affects how a
    // pane or workspace should paint goes through these helpers, so the
    // PaintEvent stream stays bit-identical to consumers regardless of which
    // internal pathway (poll, hook, workspace-change) triggered it.

    /// Format a pane address as the indicator key panel widgets expect.
    ///
    /// Address-derived rather than `k{id}`-only, because kitty pane ids are
    /// unique only inside a single remote-control socket. Two live panes with
    /// id 5 on different sockets would have collapsed onto the same paint
    /// dot under the legacy format. The new shape mirrors `PaneAddr::short()`
    /// (`<id>@<sock-pid>`) so traces, panel ids, and log prints all line up.
    /// The id is opaque to consumers (richmon, richspace-babel) — they key
    /// state by it but do not parse it, so widening the encoding is safe.
    pub fn paint_pane_id(addr: &PaneAddr) -> String {
        format!("k{}", addr.short())
    }

    /// Publish a single PaintEvent on the paint stream. No-op if no
    /// subscribers — paint is fire-and-forget like the event stream.
    fn publish_paint(&self, event: PaintEvent) {
        let _ = self.paint_publisher.send(event);
    }

    /// Compute and publish the per-window paint event for a pane.
    ///
    /// Pulls color from `resolve_color(agent_kind, hook_state, activity_state)`
    /// and ring intensity from `pane_ring`. x_pos comes from the pane's
    /// patched-kitty `screen` field — caller is expected to have refreshed
    /// the pane's geometry before calling this for position-affecting events
    /// (WindowAdded, WorkspaceChanged); for in-place state changes we use
    /// the cached value, which is fine for sorting stability.
    fn emit_pane_paint(&self, addr: &PaneAddr) {
        let Some(window) = self.panes.get(addr) else {
            return;
        };
        let activity_state = self.pane_states.get(addr).copied();
        let color = resolve_color(window.agent_kind, window.hook_state, activity_state);
        let x_pos = window.screen.as_ref().map(|s| s.x);
        const UNREAD_RING_INTENSITY: f64 = 0.75;
        let has_unread_ring = self.pane_unread.contains(addr);
        let ring_intensity =
            self.pane_ring
                .get(addr)
                .copied()
                .unwrap_or(0.0)
                .max(if has_unread_ring {
                    UNREAD_RING_INTENSITY
                } else {
                    0.0
                });
        let ring_color = has_unread_ring.then(|| window.agent_kind.accent_color().to_string());
        let event = PaintEvent::Window(IndicatorEvent::Set {
            id: Self::paint_pane_id(addr),
            color: color.to_string(),
            workspace: window.workspace.unwrap_or(0) as u32,
            x_pos,
            ring_intensity,
            ring_color,
            has_outline: false,
            scale: 1.0,
        });
        self.publish_paint(event);
    }

    /// Publish a Window Remove paint event for a closed pane. Takes the full
    /// address so the paint id matches the one originally published — bare
    /// kitty id alone is not enough to disambiguate across sockets.
    fn emit_pane_remove_paint(&self, addr: &PaneAddr) {
        self.publish_paint(PaintEvent::Window(IndicatorEvent::Remove {
            id: Self::paint_pane_id(addr),
        }));
    }

    /// Compute and publish the workspace aggregate paint event.
    ///
    /// Walks current windows on the workspace, gathers their cached activity
    /// states, computes CSS class via `paint::workspace_css_class`. Updates
    /// `workspace_awaiting_since` so the urgent timer is monotonic until the
    /// state leaves AwaitingInput.
    fn emit_workspace_paint(&mut self, workspace: i32) {
        // Gather states for panes on this workspace.
        let states: Vec<scrollparse::claude::ActivityState> = self
            .panes
            .iter()
            .filter(|(_, w)| w.workspace == Some(workspace))
            .filter_map(|(addr, _)| self.pane_states.get(addr).copied())
            .collect();

        // Empty workspace → emit Remove so panel can clear the styling.
        if states.is_empty() {
            self.workspace_awaiting_since.remove(&workspace);
            self.publish_paint(PaintEvent::Workspace(WorkspacePaintEvent::Remove {
                workspace: workspace as u32,
            }));
            return;
        }

        let css_class = workspace_css_class(&states).map(|s| s.to_string());
        let is_urgent = workspace_is_urgent(&states);

        // Awaiting timer: set on entry, clear on exit. Monotonic across
        // ticks so the tooltip can show "Waiting: 3m 14s".
        if is_urgent {
            self.workspace_awaiting_since
                .entry(workspace)
                .or_insert_with(Instant::now);
        } else {
            self.workspace_awaiting_since.remove(&workspace);
        }
        let awaiting_seconds = self
            .workspace_awaiting_since
            .get(&workspace)
            .map(|t| t.elapsed().as_secs());

        let title = self.workspace_titles.get(&workspace).cloned();

        self.publish_paint(PaintEvent::Workspace(WorkspacePaintEvent::Set {
            workspace: workspace as u32,
            css_class,
            is_urgent,
            awaiting_seconds,
            window_count: states.len(),
            title,
        }));
    }

    /// Bump a pane's ring intensity (additive up to 1.0) and publish a
    /// Window paint event with the new value. Decay loop will fade it back.
    pub fn bump_ring_and_emit(&mut self, addr: &PaneAddr, delta: f64) {
        let entry = self.pane_ring.entry(addr.clone()).or_insert(0.0);
        *entry = (*entry + delta).min(1.0);
        self.emit_pane_paint(addr);
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
    pub async fn refresh_panes(&mut self, skip_activity_fetch: bool) -> Result<Vec<i32>> {
        use crate::kitty::get_all_workspaces;

        // ─── Multi-Socket Discovery ─────────────────────────────────────────────────
        // Query all kitty instances, update socket_status
        // IMPORTANT: We extract panes from instances to avoid a second round of kitten @ ls calls
        let instances = discover_all_instances().await;
        let current_socket = default_socket();

        self.socket_status = instances
            .iter()
            .map(|i| {
                (
                    i.socket.clone(),
                    SocketStatus {
                        is_current: i.socket == current_socket,
                        is_responsive: i.is_responsive,
                        pane_count: i.panes.len(),
                        last_error: i.error.clone(),
                    },
                )
            })
            .collect();

        // Extract all panes from discovered instances (already fetched above)
        // This avoids the duplicate kitten @ ls calls that list_all_panes() would do
        let all_panes: Vec<_> = instances.into_iter().flat_map(|i| i.panes).collect();

        // Filter to tracked agent panes for the main session state.
        let agent_kitty_panes: Vec<_> = all_panes
            .iter()
            .filter(|p| detect_agent_signals(p).is_agent())
            .cloned()
            .collect();

        // Get workspace mappings — use cache unless dirty (invalidated by wnck WorkspaceChange)
        // Eliminates 1× `wmctrl -l` subprocess per poll tick in steady state.
        //
        // Self-healing: if any discovered pane's platform_window_id is absent from
        // the cache, force a refresh. wnck's window-opened only connects the
        // workspace-changed handler — it doesn't fire a WorkspaceChange — so a freshly
        // spawned kitty OS window would otherwise stay invisible to the cache and
        // render as "Workspace ?" until the next manual invalidation.
        let cache_misses_pane = !self.workspace_cache_dirty
            && all_panes
                .iter()
                .any(|p| !self.workspace_cache.contains_key(&p.platform_window_id));
        if cache_misses_pane {
            self.workspace_cache_dirty = true;
        }
        let workspaces = if self.workspace_cache_dirty {
            let ws = get_all_workspaces();
            self.workspace_cache = ws.clone();
            self.workspace_cache_dirty = false;
            ws
        } else {
            self.workspace_cache.clone()
        };

        // ─── Terminal Tracking (ALL kitty windows from ALL sockets) ─────────────────
        // Build terminal info for all panes and emit terminal events
        let mut new_terminals: HashMap<PaneAddr, TerminalInfo> = HashMap::new();
        let old_terminal_addrs: std::collections::HashSet<_> =
            self.terminals.keys().cloned().collect();

        for pane in &all_panes {
            let addr = pane.addr();
            let workspace = workspaces.get(&pane.platform_window_id).copied();
            let is_agent = detect_agent_signals(pane).is_agent();

            new_terminals.insert(
                addr.clone(),
                TerminalInfo {
                    addr,
                    title: pane.title.clone(),
                    cwd: pane.cwd.clone(),
                    foreground_command: pane
                        .foreground_processes
                        .first()
                        .and_then(|p| p.cmdline.first())
                        .map(|s| s.rsplit('/').next().unwrap_or(s).to_string()),
                    workspace,
                    is_agent,
                    is_focused: pane.is_focused,
                    platform_window_id: pane.platform_window_id,
                },
            );
        }

        let new_terminal_addrs: std::collections::HashSet<_> =
            new_terminals.keys().cloned().collect();

        // Emit TerminalOpened events for new terminals
        for addr in new_terminal_addrs.difference(&old_terminal_addrs) {
            if let Some(t) = new_terminals.get(addr) {
                self.event_publisher.publish(BabelEvent::TerminalOpened {
                    addr: addr.clone(),
                    title: t.title.clone(),
                    cwd: t.cwd.clone(),
                    workspace: t.workspace,
                });
            }
        }

        // Emit TerminalClosed events for removed terminals
        for addr in old_terminal_addrs.difference(&new_terminal_addrs) {
            self.event_publisher.publish(BabelEvent::TerminalClosed {
                addr: addr.clone(),
            });
        }

        // Emit TerminalBecameAgent for terminals that just became agent sessions
        for (addr, new_term) in &new_terminals {
            if let Some(old_term) = self.terminals.get(addr) {
                if !old_term.is_agent && new_term.is_agent {
                    self.event_publisher
                        .publish(BabelEvent::TerminalBecameAgent {
                            addr: addr.clone(),
                            title: new_term.title.clone(),
                        });
                }
            }
        }

        self.terminals = new_terminals;

        // ─── Agent Pane Tracking ─────────────────────────────────────────────────
        // Build new pane map, preserving enriched data where possible
        let mut new_windows: HashMap<PaneAddr, AgentPane> = HashMap::new();

        for kw in agent_kitty_panes {
            let addr = kw.addr();
            let workspace = workspaces.get(&kw.platform_window_id).copied();

            // Check if we have existing data for this pane (use get, not remove)
            let mut agent_pane = if let Some(existing) = self.panes.get(&addr) {
                // Clone existing and update dynamic fields
                let mut updated = existing.clone();
                updated.workspace = workspace;
                updated.is_focused = kw.is_focused;
                // Refresh agent kind every tick — a pane that exits claude and
                // launches codex (or vice versa) reuses the kitty window id, so
                // the cached value would lie. Cmdline detection is cheap.
                updated.agent_kind = detect_agent_signals(&kw).agent;

                // Reset agent-* sessions to force re-matching via fingerprint
                if updated
                    .session_id
                    .as_ref()
                    .is_some_and(|id| id.starts_with("agent-"))
                {
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
                // New pane - check for existing tag but ignore agent-* sessions
                // (they were matched before we had fingerprinting)
                let existing_session = kw
                    .user_vars
                    .get("babel_session_id")
                    .filter(|id| !id.starts_with("agent-"))
                    .cloned();
                // Detect harness once at construction; cached on the pane so
                // panel-color dispatch (Claude orange vs Codex cyan) doesn't
                // need to re-walk foreground processes on every event.
                let agent_kind = detect_agent_signals(&kw).agent;
                AgentPane {
                    addr: addr.clone(),
                    title: kw.title.clone(),
                    session_id: existing_session,
                    session_info: None,
                    cwd: kw.cwd.clone(),
                    is_focused: kw.is_focused,
                    os_window_id: kw.os_window_id,
                    platform_window_id: kw.platform_window_id,
                    workspace,
                    agent_kind,
                    screen: kw.screen.clone(),
                    activity_state: None, // Will be populated from pane_states cache
                    hook_state: None,     // Populated later from babel_storage
                    fingerprint: None,
                    match_confidence: None,
                }
            };

            // Try to match unmatched panes using summary index
            if agent_pane.session_id.is_none() {
                trace!(
                    "Window {} needs matching (title: {})",
                    addr.short(),
                    agent_pane.title
                );

                if let Some(session_id) = self.match_title_to_session(&agent_pane.title) {
                    trace!("  → Title matched to session: {}", session_id);
                    agent_pane.session_id = Some(session_id);
                } else {
                    trace!("  → Title match failed, will defer fingerprinting");
                }
            }

            new_windows.insert(addr, agent_pane);
        }

        // Detect and emit events for window changes
        let old_addrs: std::collections::HashSet<_> = self.panes.keys().cloned().collect();
        let new_addrs: std::collections::HashSet<_> = new_windows.keys().cloned().collect();

        // Track workspaces that need re-summarization
        let mut changed_workspaces: std::collections::HashSet<i32> =
            std::collections::HashSet::new();

        // Windows added
        // Stage paint emissions: assign self.panes first so emit_pane_paint
        // can read the freshly-added pane, then emit the per-window paint
        // and per-workspace aggregate. The diagnostic BabelEvent fires here too.
        let added_addrs: Vec<PaneAddr> = new_addrs.difference(&old_addrs).cloned().collect();
        let removed_addrs: Vec<PaneAddr> = old_addrs.difference(&new_addrs).cloned().collect();

        for addr in &added_addrs {
            if let Some(w) = new_windows.get(addr) {
                self.event_publisher.publish(BabelEvent::WindowAdded {
                    addr: addr.clone(),
                    title: w.title.clone(),
                    workspace: w.workspace,
                    agent_kind: w.agent_kind,
                });
                // Track workspace for re-summarization
                if let Some(ws) = w.workspace {
                    changed_workspaces.insert(ws);
                }
            }
        }

        // Windows removed - clean up cached fingerprints and states
        for addr in &removed_addrs {
            // Get workspace from old windows before removal
            if let Some(w) = self.panes.get(addr) {
                if let Some(ws) = w.workspace {
                    changed_workspaces.insert(ws);
                }
            }

            // Clean up cached data for closed window
            self.pane_fingerprints.remove(addr);
            self.pane_states.remove(addr);
            self.pane_ring.remove(addr);
            self.pane_unread.remove(addr);

            self.event_publisher.publish(BabelEvent::WindowRemoved {
                addr: addr.clone(),
            });
            // Paint: tell clients to drop the dot. Pass the full address so
            // the Remove id matches the addr-derived Set id we published when
            // the pane first appeared.
            self.emit_pane_remove_paint(addr);
        }

        // Check for focus changes
        let old_focused = self
            .panes
            .values()
            .find(|w| w.is_focused)
            .map(|w| w.addr.clone());
        let new_focused = new_windows
            .values()
            .find(|w| w.is_focused)
            .map(|w| w.addr.clone());
        let mut read_changed_addrs: HashSet<PaneAddr> = HashSet::new();
        if old_focused != new_focused {
            // Emit PaneUnfocused for the pane that lost focus
            if let Some(ref addr) = old_focused {
                if let Some(w) = self.panes.get(addr) {
                    self.event_publisher.publish(BabelEvent::PaneUnfocused {
                        addr: addr.clone(),
                        session_id: w.session_id.clone(),
                    });
                }
            }
            // Emit PaneFocused for the pane that gained focus
            if let Some(ref addr) = new_focused {
                if let Some(w) = new_windows.get(addr) {
                    self.event_publisher.publish(BabelEvent::PaneFocused {
                        addr: addr.clone(),
                        session_id: w.session_id.clone(),
                    });
                    // Mark as read when pane gains focus—the worker's voice is now heard
                    if let Some(ref session_id) = w.session_id {
                        if let Err(e) = init_db().and_then(|conn| mark_read(&conn, session_id)) {
                            trace_error!("failed to mark as read on focus", session_id, error = %e);
                        }
                        if self.pane_unread.remove(addr) {
                            read_changed_addrs.insert(addr.clone());
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
        // Used by richspace-babel to update per-workspace dot display.
        // Capture both old and new workspace per moved pane so the paint pass
        // below knows which workspaces need aggregate recomputation.
        let mut paint_workspaces: HashSet<i32> = HashSet::new();
        let mut moved_addrs: Vec<PaneAddr> = Vec::new();
        for addr in old_addrs.intersection(&new_addrs) {
            let old_ws = self.panes.get(addr).and_then(|w| w.workspace);
            let new_ws = new_windows.get(addr).and_then(|w| w.workspace);

            if old_ws != new_ws {
                self.event_publisher
                    .publish(BabelEvent::WindowWorkspaceChanged {
                        addr: addr.clone(),
                        old_workspace: old_ws,
                        new_workspace: new_ws,
                    });
                // Track both workspaces for re-summarization
                if let Some(ws) = old_ws {
                    changed_workspaces.insert(ws);
                    paint_workspaces.insert(ws);
                }
                if let Some(ws) = new_ws {
                    changed_workspaces.insert(ws);
                    paint_workspaces.insert(ws);
                }
                moved_addrs.push(addr.clone());
                trace!(
                    "Window {} moved: workspace {:?} -> {:?}",
                    addr.short(),
                    old_ws,
                    new_ws
                );
            }
        }

        // Workspaces touched by add/remove also need paint recomputation.
        for addr in &added_addrs {
            if let Some(w) = new_windows.get(addr) {
                if let Some(ws) = w.workspace {
                    paint_workspaces.insert(ws);
                }
            }
        }
        for addr in &removed_addrs {
            if let Some(w) = self.panes.get(addr) {
                if let Some(ws) = w.workspace {
                    paint_workspaces.insert(ws);
                }
            }
        }

        // Detect session state changes and emit events
        // This enables richspace-babel to track agent activity per-workspace
        // Also track scrollback changes for ActivityPulse events
        //
        // PERFORMANCE: This loop is expensive (~100ms per window via kitten subprocess).
        // Skip when skip_activity_fetch=true for fast structural refreshes.
        // Cached activity states from previous polls are still available.
        //
        // Paint emission: collect addrs that need a fresh window paint event
        // (state change or pulse) and workspaces whose aggregate must be
        // recomputed. Actual paint emission happens AFTER `self.panes =
        // new_windows` so the helpers can resolve color from the live state.
        let mut paint_addrs: HashSet<PaneAddr> = HashSet::new();
        let mut pulse_addrs: HashMap<PaneAddr, f64> = HashMap::new();
        if !skip_activity_fetch {
            for (addr, window) in &new_windows {
                // Get state, asking_question, and scrollback in one fetch to avoid double I/O
                let activity = get_pane_activity_with_scrollback(addr.id).await;
                let new_state = activity.state;
                let old_state = self.pane_states.get(addr).copied();

                // ─── State Change Detection ─────────────────────────────────────────────
                match old_state {
                    Some(old) if old != new_state => {
                        // State changed - emit event
                        state!("pane_activity", format!("{:?}", old) => format!("{:?}", new_state), window = addr.short());
                        self.event_publisher
                            .publish(BabelEvent::SessionStateChanged {
                                addr: addr.clone(),
                                session_id: window.session_id.clone(),
                                workspace: window.workspace,
                                old_state: old,
                                new_state,
                                asking_question: activity.asking_question,
                                agent_kind: window.agent_kind,
                            });
                        self.pane_states.insert(addr.clone(), new_state);
                        paint_addrs.insert(addr.clone());
                        if let Some(ws) = window.workspace {
                            paint_workspaces.insert(ws);
                        }

                        // Also emit ActivityPulse on state transitions
                        self.event_publisher.publish(BabelEvent::ActivityPulse {
                            addr: addr.clone(),
                            session_id: window.session_id.clone(),
                            workspace: window.workspace,
                            intensity: 0.8, // State transitions are significant
                            trigger: crate::events::PulseTrigger::StateTransition,
                        });
                        // Stage the ring bump so paint pass sees the brighter aura.
                        let entry = pulse_addrs.entry(addr.clone()).or_insert(0.0);
                        *entry = (*entry + 0.8).min(1.0);

                        // Clear unread state whenever work resumes from an awaiting turn.
                        if old == scrollparse::claude::ActivityState::AwaitingInput
                            && new_state != scrollparse::claude::ActivityState::AwaitingInput
                        {
                            if let Some(ref session_id) = window.session_id {
                                if let Err(e) =
                                    init_db().and_then(|conn| mark_read(&conn, session_id))
                                {
                                    trace_error!("failed to auto-read session", session_id, error = %e);
                                }
                                if self.pane_unread.remove(addr) {
                                    paint_addrs.insert(addr.clone());
                                }
                                if let Err(e) =
                                    reset_border_color_on_socket(&addr.socket, addr.id).await
                                {
                                    effect!("xfconf", "reset_border", error = e.to_string());
                                }
                            }
                        }

                        // Auto-unread when scrollback says the agent is awaiting input.
                        // The durable visual is a ring over the current dot state, not
                        // a fake activity state.
                        if new_state == scrollparse::claude::ActivityState::AwaitingInput {
                            if let Some(ref session_id) = window.session_id {
                                if let Err(e) =
                                    init_db().and_then(|conn| mark_unread(&conn, session_id))
                                {
                                    trace_error!("failed to auto-unread session", session_id, error = %e);
                                }
                                if self.pane_unread.insert(addr.clone()) {
                                    paint_addrs.insert(addr.clone());
                                }
                                // Keep the legacy terminal border cue in sync with the
                                // daemon-owned paint ring.
                                if let Err(e) = set_border_color_on_socket(
                                    &addr.socket,
                                    addr.id,
                                    "#f67400",
                                    "#7a3a00",
                                )
                                .await
                                {
                                    effect!("xfconf", "set_unread_border", error = e.to_string());
                                }
                            }
                        }
                    }
                    None => {
                        // New window - initialize state (no event, WindowAdded already fired)
                        self.pane_states.insert(addr.clone(), new_state);
                        paint_addrs.insert(addr.clone());
                        if let Some(ws) = window.workspace {
                            paint_workspaces.insert(ws);
                        }
                    }
                    _ => {
                        // State unchanged
                    }
                }

                // ─── Activity Pulse Detection ───────────────────────────────────────────
                // Track scrollback changes to emit fine-grained activity pulses
                // for reactive UI animations (heartbeat blinks, etc.)
                if !activity.scrollback.is_empty() {
                    let pane_activity = self.pane_activity.entry(addr.clone()).or_default();
                    let (changed, _delta_bytes) = pane_activity.update(&activity.scrollback);

                    if changed {
                        let intensity = pane_activity.compute_intensity();

                        // Determine trigger type based on current state
                        let trigger = match new_state {
                            scrollparse::claude::ActivityState::ToolUse => {
                                crate::events::PulseTrigger::ToolStart
                            } // hands begin moving
                            scrollparse::claude::ActivityState::Thinking => {
                                crate::events::PulseTrigger::TokenOutput
                            } // soul breathing tokens
                            _ => crate::events::PulseTrigger::TokenOutput,
                        };

                        // Only emit if intensity is meaningful (avoid noise)
                        if intensity > 0.05 {
                            self.event_publisher.publish(BabelEvent::ActivityPulse {
                                addr: addr.clone(),
                                session_id: window.session_id.clone(),
                                workspace: window.workspace,
                                intensity,
                                trigger,
                            });
                            // Ring bump for the paint stream — additive, will
                            // decay back via the pulse-decay loop.
                            let entry = pulse_addrs.entry(addr.clone()).or_insert(0.0);
                            *entry = (*entry + intensity as f64).min(1.0);
                        }
                    }
                }
            }
        } // end skip_activity_fetch check

        // Clean up states for removed windows
        self.pane_states
            .retain(|addr, _| new_windows.contains_key(addr));
        self.pane_activity
            .retain(|addr, _| new_windows.contains_key(addr));
        self.pane_ring
            .retain(|addr, _| new_windows.contains_key(addr));
        self.pane_unread
            .retain(|addr| new_windows.contains_key(addr));

        self.panes = new_windows;
        self.last_kitty_scan = Instant::now();

        // ═══════════════════════════════════════════════════════════════════
        // Paint Emission Pass
        // ═══════════════════════════════════════════════════════════════════
        // Now that self.panes is the live state, fan out paint events:
        // 1. Added panes get a Set with fresh color/workspace/x_pos.
        // 2. Moved panes get a Set with the new workspace number.
        // 3. State-changed panes get a Set with the new color.
        // 4. Pulse bumps add to ring_intensity, then emit Set.
        // 5. Workspaces whose pane composition or state mix changed get a
        //    Workspace aggregate paint event.

        for addr in &added_addrs {
            self.emit_pane_paint(addr);
        }
        for addr in &moved_addrs {
            paint_addrs.insert(addr.clone());
        }
        for addr in &read_changed_addrs {
            paint_addrs.insert(addr.clone());
        }
        for addr in &paint_addrs {
            self.emit_pane_paint(addr);
        }
        for (addr, intensity) in pulse_addrs {
            // bump_ring_and_emit wraps emit_pane_paint after updating ring.
            self.bump_ring_and_emit(&addr, intensity);
        }
        for ws in &paint_workspaces {
            self.emit_workspace_paint(*ws);
        }

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
    #[tracing::instrument(skip(self), fields(window_count = self.panes.len()))]
    pub fn get_panes_needing_fingerprints(&self) -> Vec<(PaneAddr, PathBuf)> {
        self.panes
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
        self.panes
            .values()
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
        let _ = crate::utility::agent_discovery::tag_pane(addr.id, &session_id);

        // Cache the fingerprint
        self.cache_fingerprint(addr.clone(), fingerprint.clone());

        // Update the window in our state
        if let Some(window) = self.panes.get_mut(addr) {
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
        let old_state = self.pane_states.get(addr).copied();
        let window = match self.panes.get(addr) {
            Some(w) => w,
            None => return, // Window no longer exists
        };
        // Capture workspace before touching mutable state — needed for the
        // post-update paint pass.
        let pane_workspace = window.workspace;
        // Paint staging — flags set during the existing publish path, then
        // applied at the end of the function after borrows on self.panes
        // are released. emit_pane_paint takes &self, but bump_ring_and_emit
        // and emit_workspace_paint take &mut self.
        let mut paint_state_changed = false;
        let mut paint_state_pulse_intensity: f64 = 0.0;
        let mut paint_pulse_intensity: Option<f64> = None;

        // ─── State Change Detection ─────────────────────────────────────────────
        match old_state {
            Some(old) if old != new_state => {
                // State changed - emit event
                state!("pane_activity", format!("{:?}", old) => format!("{:?}", new_state), window = addr.short());
                self.event_publisher
                    .publish(BabelEvent::SessionStateChanged {
                        addr: addr.clone(),
                        session_id: window.session_id.clone(),
                        workspace: window.workspace,
                        old_state: old,
                        new_state,
                        asking_question,
                        agent_kind: window.agent_kind,
                    });
                self.pane_states.insert(addr.clone(), new_state);
                paint_state_changed = true;

                // Also emit ActivityPulse on state transitions
                self.event_publisher.publish(BabelEvent::ActivityPulse {
                    addr: addr.clone(),
                    session_id: window.session_id.clone(),
                    workspace: window.workspace,
                    intensity: 0.8, // State transitions are significant
                    trigger: crate::events::PulseTrigger::StateTransition,
                });
                paint_state_pulse_intensity = 0.8;

                // Clear unread state whenever work resumes from an awaiting turn.
                // This preserves the Claude prompt hook behavior and lets Codex's
                // notify-only integration recover cleanly once scrollback shows
                // the pane is active again.
                if old == scrollparse::claude::ActivityState::AwaitingInput
                    && new_state != scrollparse::claude::ActivityState::AwaitingInput
                {
                    if let Some(ref session_id) = window.session_id {
                        if let Err(e) = init_db().and_then(|conn| mark_read(&conn, session_id)) {
                            trace_error!("failed to auto-read session", session_id, error = %e);
                        }
                        if self.pane_unread.remove(addr) {
                            paint_state_changed = true;
                        }
                        if let Err(e) = reset_border_color_on_socket(&addr.socket, addr.id).await {
                            effect!("xfconf", "reset_border", error = e.to_string());
                        }
                    }
                }

                // Auto-unread when scrollback says the agent is awaiting input.
                // The paint stream keeps this as a durable ring over the current state.
                if new_state == scrollparse::claude::ActivityState::AwaitingInput {
                    if let Some(ref session_id) = window.session_id {
                        if let Err(e) = init_db().and_then(|conn| mark_unread(&conn, session_id)) {
                            trace_error!("failed to auto-unread session", session_id, error = %e);
                        }
                        if self.pane_unread.insert(addr.clone()) {
                            paint_state_changed = true;
                        }
                        // Keep the legacy terminal border cue in sync with the paint ring.
                        if let Err(e) =
                            set_border_color_on_socket(&addr.socket, addr.id, "#f67400", "#7a3a00")
                                .await
                        {
                            effect!("xfconf", "set_unread_border", error = e.to_string());
                        }
                    }
                }
            }
            None => {
                // New window - initialize state (no event)
                self.pane_states.insert(addr.clone(), new_state);
                paint_state_changed = true;
            }
            _ => {
                // State unchanged
            }
        }

        // ─── Activity Pulse Detection ───────────────────────────────────────────
        if !scrollback.is_empty() {
            let activity = self.pane_activity.entry(addr.clone()).or_default();
            let (changed, _delta_bytes) = activity.update(&scrollback);

            if changed {
                let intensity = activity.compute_intensity();
                // Reading the worker's breath to pulse the right signal
                let trigger = match new_state {
                    scrollparse::claude::ActivityState::ToolUse => {
                        crate::events::PulseTrigger::ToolStart
                    } // hands begin moving
                    scrollparse::claude::ActivityState::Thinking => {
                        crate::events::PulseTrigger::TokenOutput
                    } // soul breathing tokens
                    _ => crate::events::PulseTrigger::TokenOutput,
                };

                // Only emit if intensity is meaningful
                if intensity > 0.05 {
                    self.event_publisher.publish(BabelEvent::ActivityPulse {
                        addr: addr.clone(),
                        session_id: window.session_id.clone(),
                        workspace: window.workspace,
                        intensity,
                        trigger,
                    });
                    // Stage paint pulse — applied after the borrow on
                    // self.panes is released (end of function).
                    paint_pulse_intensity = Some(intensity as f64);
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
                                if let Err(e) =
                                    storage.record_file_touch(session_id, &op.path, &op.operation)
                                {
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

        // ═══════════════════════════════════════════════════════════════════
        // Paint Emission Pass
        // ═══════════════════════════════════════════════════════════════════
        // Apply staged paint events now that the &self.panes borrow is gone.
        if paint_state_changed {
            self.emit_pane_paint(addr);
            if let Some(ws) = pane_workspace {
                self.emit_workspace_paint(ws);
            }
        }
        // Pulse-derived ring bumps: state transitions add 0.8, scrollback
        // pulses add their own intensity. Combine into one bump call so the
        // dot only repaints once even with both signals at the same tick.
        let total_pulse = paint_state_pulse_intensity + paint_pulse_intensity.unwrap_or(0.0);
        if total_pulse > 0.0 {
            self.bump_ring_and_emit(addr, total_pulse);
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

        trace!(
            "fingerprint_match({}) - index has {} sessions, cwd={}",
            addr.short(),
            fingerprint_index.len(),
            kitty_cwd.display()
        );

        // Get scrollback using the pane's socket (EXPENSIVE I/O - done without lock)
        let scrollback = match get_scrollback_on_socket(&addr.socket, addr.id).await {
            Ok(s) => s,
            Err(e) => {
                trace_error!("failed to get scrollback", addr = %addr.short(), error = %e);
                return None;
            }
        };

        trace!(
            "  scrollback: {} bytes, {} lines",
            scrollback.len(),
            scrollback.lines().count()
        );

        // Extract fingerprint from scrollback, but use kitty's CWD (reliable)
        // instead of trying to parse Claude's status bar (unreliable format)
        let mut window_fp = extract_from_scrollback(&scrollback);
        window_fp.cwd = Some(kitty_cwd.to_path_buf());

        trace!(
            "  extracted: first_prompt={:?}, prompts={}, tools={:?}, cwd={:?}",
            window_fp
                .first_prompt
                .as_ref()
                .map(|s| &s[..s.len().min(40)]),
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
            let cwd_matches: Vec<_> = fingerprint_index
                .iter()
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
                    cwd_matches.len(),
                    window_cwd
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
                            if cwd.to_string_lossy().contains("babel") {
                                tracing::debug!(
                                    "Index adding session {} with CWD {:?}",
                                    session_id,
                                    cwd
                                );
                            }
                        }
                    }
                    index.insert(session_id.to_string(), fp);
                }
            }
        }
        checkpoint!(
            "index_built",
            sessions = index.len(),
            with_cwd = sessions_with_cwd
        );

        self.fingerprint_index = index;
        self.last_fingerprint_rebuild = Instant::now();
        checkpoint!(
            "fingerprint_index_rebuilt",
            session_count = self.fingerprint_index.len()
        );
        Ok(())
    }

    /// Cache a fingerprint for a window
    ///
    /// Called after successful fingerprint matching to avoid re-extraction
    #[tracing::instrument(skip(self, fingerprint), fields(addr = %addr.short(), cache_size = self.pane_fingerprints.len()))]
    fn cache_fingerprint(&mut self, addr: PaneAddr, fingerprint: SessionFingerprint) {
        self.pane_fingerprints.insert(addr, fingerprint);

        // Safety net: enforce maximum cache size to prevent unbounded growth
        // This should rarely trigger since we clean up on window removal,
        // but protects against edge cases (e.g., fingerprint extraction spam)
        if self.pane_fingerprints.len() > config::FINGERPRINT_CACHE_LIMIT {
            // Remove oldest entries (just prevent unbounded growth, not critical which ones)
            while self.pane_fingerprints.len() > config::FINGERPRINT_CACHE_LIMIT {
                if let Some(key) = self.pane_fingerprints.keys().next().cloned() {
                    self.pane_fingerprints.remove(&key);
                }
            }
        }
    }

    // ═══════════════════════════════════════════════════════════════════════════════
    // Address-First Lookup + Legacy ID Resolution
    // ═══════════════════════════════════════════════════════════════════════════════
    //
    // Canonical live identity is `PaneAddr { socket, id }`. Internal code that
    // already has a `PaneAddr` should reach panes/state via `find_pane`/`find_pane_mut`,
    // never by stripping to `addr.id` and re-resolving. The legacy `u64` family
    // exists only for IPC/CLI edges that haven't been migrated yet, and it
    // refuses to silently pick a winner when the same kitty id is live on more
    // than one socket — multi-kitty collisions are real and the correctness
    // cost of the wrong pane firing is much worse than a clean refusal.

    /// Address-first pane lookup. Use this whenever the caller already has a
    /// `PaneAddr` in scope; do not detour through `addr.id`.
    pub fn find_pane(&self, addr: &PaneAddr) -> Option<&AgentPane> {
        self.panes.get(addr)
    }

    /// Mutable address-first pane lookup.
    pub fn find_pane_mut(&mut self, addr: &PaneAddr) -> Option<&mut AgentPane> {
        self.panes.get_mut(addr)
    }

    /// Address-first activity-state lookup. Use this whenever the caller already
    /// has a `PaneAddr` in scope.
    pub fn pane_activity_state(
        &self,
        addr: &PaneAddr,
    ) -> Option<scrollparse::claude::ActivityState> {
        self.pane_states.get(addr).copied()
    }

    /// Resolve a bare kitty id to a single live `PaneAddr` with explicit
    /// ambiguity awareness. The legacy IPC/CLI surface still hands us raw u64
    /// pane ids; this is the one place that converts that to a canonical
    /// address, and it tells the caller when the answer is not unique.
    pub fn resolve_legacy_pane_id(&self, id: u64) -> PaneIdResolution {
        let mut matches: Vec<PaneAddr> = self
            .panes
            .keys()
            .filter(|addr| addr.id == id)
            .cloned()
            .collect();
        match matches.len() {
            0 => PaneIdResolution::NotFound,
            1 => PaneIdResolution::Found(matches.remove(0)),
            _ => PaneIdResolution::Ambiguous(matches),
        }
    }

    /// Legacy edge shim: resolve a raw kitty id to a single pane.
    ///
    /// Returns `None` on either "not found" or "ambiguous" — refusing to act
    /// on an ambiguous id is the whole point. Ambiguity is logged at warn
    /// level with every candidate address so multi-kitty collisions surface
    /// in the trace instead of corrupting state.
    pub fn find_pane_by_id(&self, id: u64) -> Option<&AgentPane> {
        match self.resolve_legacy_pane_id(id) {
            PaneIdResolution::Found(addr) => self.panes.get(&addr),
            PaneIdResolution::Ambiguous(candidates) => {
                trace_legacy_id_ambiguous(id, &candidates);
                None
            }
            PaneIdResolution::NotFound => None,
        }
    }

    /// Legacy edge shim, mutable. See `find_pane_by_id` for ambiguity rules.
    pub fn find_pane_by_id_mut(&mut self, id: u64) -> Option<&mut AgentPane> {
        match self.resolve_legacy_pane_id(id) {
            PaneIdResolution::Found(addr) => self.panes.get_mut(&addr),
            PaneIdResolution::Ambiguous(candidates) => {
                trace_legacy_id_ambiguous(id, &candidates);
                None
            }
            PaneIdResolution::NotFound => None,
        }
    }

    /// Legacy edge shim: resolve a raw kitty id to its `PaneAddr`. Returns
    /// `None` on ambiguity for the same reason as `find_pane_by_id`.
    pub fn find_addr_by_id(&self, id: u64) -> Option<PaneAddr> {
        match self.resolve_legacy_pane_id(id) {
            PaneIdResolution::Found(addr) => Some(addr),
            PaneIdResolution::Ambiguous(candidates) => {
                trace_legacy_id_ambiguous(id, &candidates);
                None
            }
            PaneIdResolution::NotFound => None,
        }
    }

    /// Get activity state for a window by its kitty ID. Legacy edge — prefer
    /// `pane_activity_state(&PaneAddr)` whenever the address is in scope.
    pub fn get_activity_state(&self, id: u64) -> Option<scrollparse::claude::ActivityState> {
        let addr = self.find_addr_by_id(id)?;
        self.pane_states.get(&addr).copied()
    }
}

/// Outcome of resolving a raw kitty id back to canonical `PaneAddr` identity.
///
/// Multi-kitty collisions are real: a pane id is unique only inside a single
/// kitty remote-control socket. When the legacy IPC/CLI surface hands us a
/// bare u64, the answer is genuinely "found / ambiguous / not found", and the
/// daemon refuses to silently pick a winner in the ambiguous case.
#[derive(Debug, Clone)]
pub enum PaneIdResolution {
    Found(PaneAddr),
    Ambiguous(Vec<PaneAddr>),
    NotFound,
}

/// Emit a warn-level trace for a legacy u64 lookup that could not be made
/// unambiguous. Centralized so every legacy shim refuses ambiguity the same
/// way and the trace shape is consistent for grep/jq.
fn trace_legacy_id_ambiguous(id: u64, candidates: &[PaneAddr]) {
    let shorts: Vec<String> = candidates.iter().map(|a| a.short()).collect();
    trace_error!(
        "ambiguous legacy pane id across sockets — refusing to choose",
        id,
        candidates = shorts.join(",")
    );
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
pub async fn run_daemon_traced(enable_scrollparse: bool) -> Result<()> {
    TRACE.store(true, Ordering::Relaxed);
    checkpoint!("trace_mode_enabled");
    run_daemon(enable_scrollparse).await
}

/// Run the daemon
#[vtr::trace_errors]
pub async fn run_daemon(enable_scrollparse: bool) -> Result<()> {
    // ─── Startup Banner ─────────────────────────────────────────────────────────
    checkpoint!(
        "startup",
        version = env!("CARGO_PKG_VERSION"),
        scrollparse = enable_scrollparse
    );

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
        s.rebuild_summary_index()
            .context("Failed to build summary index")?;
        s.rebuild_fingerprint_index()
            .context("Failed to build fingerprint index")?;
        let _ = s
            .refresh_panes(!enable_scrollparse)
            .await
            .context("Failed initial window scan")?;

        // Compute meaningful stats
        let sessions_with_fingerprints = s.fingerprint_index.len();
        let total_summaries = s.summary_index.len();
        let windows_found = s.panes.len();
        let windows_identified = s.panes.values().filter(|w| w.session_id.is_some()).count();
        let workspaces_active: std::collections::HashSet<_> =
            s.panes.values().filter_map(|w| w.workspace).collect();

        // Log startup state - include key numbers in message for journald visibility
        if windows_found > 0 {
            tracing::info!(
                "Discovered {} windows ({} identified) across {} workspaces",
                windows_found,
                windows_identified,
                workspaces_active.len()
            );
        }

        tracing::info!(
            "Indexed {} sessions ({} with fingerprints)",
            total_summaries,
            sessions_with_fingerprints
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

            checkpoint!(
                "file_watcher_ready",
                path = projects_dir.display().to_string()
            );

            for result in rx {
                match result {
                    Ok(events) => {
                        for event in events {
                            if event.kind == DebouncedEventKind::Any {
                                let _ =
                                    rt.block_on(watch_tx.send(DaemonEvent::FileChange(event.path)));
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

    // Spawn wnck workspace change watcher (instant detection, no polling).
    // GTK/WNCK is intentionally optional so protocol/library consumers of
    // babel do not link GTK. Without the feature, normal refresh paths
    // still converge on workspace changes through polling/cache invalidation.
    #[cfg(feature = "wnck-watch")]
    {
        let wnck_tx = event_tx.clone();
        let rt = tokio::runtime::Handle::current();
        std::thread::spawn(move || {
            spawn_wnck_watcher(wnck_tx, rt);
        });
    }

    // ─── Pulse-Decay Loop ────────────────────────────────────────────────────
    // Ring intensity bumps from ActivityPulse must fade smoothly. Tick every
    // 50ms, decay each pane's ring by ~0.05 — full fade in ~1s. Only emit a
    // paint update when the value crosses a meaningful threshold, so we
    // don't flood the broadcast with imperceptible deltas.
    let decay_state = Arc::clone(&state);
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(std::time::Duration::from_millis(50));
        const DECAY_PER_TICK: f64 = 0.05;
        const REPAINT_THRESHOLD: f64 = 0.05;
        loop {
            tick.tick().await;
            let mut s = decay_state.write().await;
            // Collect addrs to update — split read from emit to satisfy the
            // borrow checker (emit_pane_paint is &self, but mutating
            // pane_ring needs &mut).
            let addrs_to_repaint: Vec<PaneAddr> = s
                .pane_ring
                .iter_mut()
                .filter_map(|(addr, ring)| {
                    if *ring <= 0.0 {
                        return None;
                    }
                    let prev = *ring;
                    *ring = (*ring - DECAY_PER_TICK).max(0.0);
                    // Repaint only when crossing a quantization step.
                    let crossed =
                        (prev / REPAINT_THRESHOLD).floor() != (*ring / REPAINT_THRESHOLD).floor();
                    if crossed {
                        Some(addr.clone())
                    } else {
                        None
                    }
                })
                .collect();
            for addr in &addrs_to_repaint {
                s.emit_pane_paint(addr);
            }
        }
    });

    // ─── IPC Socket ──────────────────────────────────────────────────────────────
    let listener = create_listener().await?;
    let socket_path = crate::utility::ipc::socket_path();
    checkpoint!("ipc_listening", socket = socket_path.display().to_string());

    // ─── Ready ──────────────────────────────────────────────────────────────────
    checkpoint!("daemon_ready");

    // Backpressure guard: prevents overlapping background poll tasks.
    // Without this, slow Phase 2 (scrollback fetch, ~100ms/window) causes
    // unbounded task accumulation when poll interval < task duration.
    let poll_bg_running = Arc::new(AtomicBool::new(false));

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
                            let ws = match s.refresh_panes(true).await { // Skip activity fetch!
                                Ok(ws) => ws,
                                Err(e) => {
                                    trace_error!("window refresh failed", error = %e);
                                    vec![]
                                }
                            };
                            let addrs: Vec<_> = if enable_scrollparse {
                                s.panes.keys().cloned().collect()
                            } else {
                                Vec::new()
                            };
                            (ws, addrs)
                        }; // Lock released - main loop continues immediately!

                        // Spawn background task for slow I/O operations
                        // This keeps the main select! loop responsive to IPC
                        // Backpressure: skip if previous background task still running
                        if poll_bg_running.compare_exchange(false, true, Ordering::AcqRel, Ordering::Relaxed).is_err() {
                            tracing::trace!("poll_bg: previous still running, skipping");
                            continue;
                        }
                        let state_clone = Arc::clone(&state);
                        let summarizer_clone = Arc::clone(&summarizer);
                        let bg_trace_id = poll_trace_id.clone();
                        let bg_flag = Arc::clone(&poll_bg_running);
                        tokio::spawn(async move {
                            let bg_span = tracing::debug_span!("poll_background", trace_id = %bg_trace_id);
                            let _bg_guard = bg_span.enter();
                            if enable_scrollparse {
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
                            }

                            // Phase 4: Get windows needing fingerprints + snapshot index + claimed sessions
                            // Arc-wrapped to avoid per-window deep clones (audit #2: allocation churn)
                            let (needs_matching, fingerprint_index, claimed_sessions) = {
                                let s = state_clone.read().await;
                                let needs = s.get_panes_needing_fingerprints();
                                let index = Arc::new(s.fingerprint_index.clone());
                                let claimed = Arc::new(s.get_claimed_sessions());
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
                                    let fingerprint_index = Arc::clone(&fingerprint_index);
                                    let claimed_sessions = Arc::clone(&claimed_sessions);
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

                            // Flush marker: signals end of poll cycle for view-time collapse
                            // spacejn uses this as block boundary hint for pattern alignment
                            tracing::trace!(vtr_kind = "flush", "poll_cycle_boundary");

                            // Release backpressure guard — next poll tick can spawn again
                            bg_flag.store(false, Ordering::Release);
                        });
                    }
                    DaemonEvent::FileChange(path) => {
                        // Rebuild fingerprint index on JSONL changes, debounced
                        // Multiple active agent sessions write JSONL concurrently —
                        // without debounce this fires repeatedly and thrashes the index
                        if path.extension().and_then(|s| s.to_str()) == Some("jsonl") {
                            let mut s = state.write().await;
                            if s.last_fingerprint_rebuild.elapsed() > config::FINGERPRINT_REBUILD_DEBOUNCE {
                                let _ = s.rebuild_fingerprint_index();
                                s.last_fingerprint_rebuild = Instant::now();
                            }
                        }
                    }
                    DaemonEvent::WorkspaceChange { platform_window_id } => {
                        // Instant workspace change from wnck - trigger immediate poll
                        // Invalidate workspace cache so refresh_panes re-runs wmctrl
                        tracing::debug!(platform_window_id, "WNCK workspace change detected");
                        let changed_workspaces = {
                            let mut s = state.write().await;
                            s.workspace_cache_dirty = true;
                            s.refresh_panes(true).await.unwrap_or_default()
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

/// Summarize agent sessions on a workspace and emit WorkspaceTitleUpdated event
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
        let windows: Vec<_> = s
            .panes
            .values()
            .filter(|w| w.workspace == Some(workspace))
            .collect();

        if windows.is_empty() {
            return;
        }

        let sessions: Vec<SessionSummaryInput> = windows
            .iter()
            .map(|w| {
                SessionSummaryInput {
                    project_path: w.cwd.to_string_lossy().to_string(),
                    recent_activity: None, // Could extract from scrollback later
                    window_title: Some(w.title.clone()),
                }
            })
            .collect();

        let window_count = windows.len();
        let primary_session = windows
            .iter()
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

        s.event_publisher
            .publish(BabelEvent::WorkspaceTitleUpdated {
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

/// Handle a paint-stream subscriber (long-lived, streams PaintEvents).
///
/// Sends Subscribed ack, then replays the current paint state, then streams
/// live paint events until disconnect. Paint events are fully resolved UX
/// commands — clients (richmon-babel, richspace-babel) forward verbatim.
async fn handle_paint_subscriber(
    mut stream: UnixStream,
    mut rx: tokio::sync::broadcast::Receiver<PaintEvent>,
    subscriber_id: u64,
    replay: Vec<PaintEvent>,
) -> Result<()> {
    // Ack — same shape as the BabelEvent subscribe ack.
    let ack = Response::Subscribed { subscriber_id };
    let mut ack_json = serde_json::to_string(&ack)?;
    ack_json.push('\n');
    stream.write_all(ack_json.as_bytes()).await?;

    // Replay current paint state so panel restarts converge instantly.
    for event in replay {
        let response = Response::PaintEvent { event };
        let mut json = serde_json::to_string(&response)?;
        json.push('\n');
        if let Err(e) = stream.write_all(json.as_bytes()).await {
            tracing::debug!(subscriber_id, error = %e, "Paint subscriber disconnected during replay");
            return Ok(());
        }
    }

    // Stream live paint events.
    loop {
        match rx.recv().await {
            Ok(event) => {
                let response = Response::PaintEvent { event };
                let mut json = serde_json::to_string(&response)?;
                json.push('\n');
                if let Err(e) = stream.write_all(json.as_bytes()).await {
                    tracing::debug!(subscriber_id, error = %e, "Paint subscriber disconnected");
                    break;
                }
            }
            Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                trace_error!("paint subscriber lagged", subscriber_id, dropped_events = n);
            }
            Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                trace_error!("paint broadcast closed");
                break;
            }
        }
    }

    Ok(())
}

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

    let request: Request = serde_json::from_str(&line).context("Failed to parse client request")?;

    boundary!(
        "ipc",
        "request",
        request = std::any::type_name_of_val(&request)
    );

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

    // Handle SubscribePaint specially — also long-lived. Replays full paint
    // state on connect so panel restarts converge instantly without waiting
    // for the next event.
    if let Request::SubscribePaint = request {
        let mut s = state.write().await;
        let rx = s.paint_publisher.subscribe();
        let subscriber_id = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);
        // Snapshot replay: collect window addrs and workspaces under the
        // write lock, drop the lock, then walk them synthesizing paint
        // events from current state. This avoids holding the lock during
        // network I/O.
        let replay_addrs: Vec<PaneAddr> = s.panes.keys().cloned().collect();
        let replay_workspaces: Vec<i32> = {
            let mut ws: HashSet<i32> = s.panes.values().filter_map(|w| w.workspace).collect();
            // Also include workspaces with cached awaiting timers — they may
            // have just been emptied; the client needs the Remove signal.
            ws.extend(s.workspace_awaiting_since.keys().copied());
            ws.into_iter().collect()
        };

        // Build the replay batch under the lock so paint helpers can read
        // self.panes / pane_states / workspace_awaiting_since.
        let mut replay_events: Vec<PaintEvent> = Vec::new();
        replay_events.push(PaintEvent::Reset);
        for addr in &replay_addrs {
            if let Some(window) = s.panes.get(addr) {
                let activity_state = s.pane_states.get(addr).copied();
                let color = resolve_color(window.agent_kind, window.hook_state, activity_state);
                let x_pos = window.screen.as_ref().map(|s| s.x);
                const UNREAD_RING_INTENSITY: f64 = 0.75;
                let has_unread_ring = s.pane_unread.contains(addr);
                let ring_intensity =
                    s.pane_ring
                        .get(addr)
                        .copied()
                        .unwrap_or(0.0)
                        .max(if has_unread_ring {
                            UNREAD_RING_INTENSITY
                        } else {
                            0.0
                        });
                let ring_color =
                    has_unread_ring.then(|| window.agent_kind.accent_color().to_string());
                replay_events.push(PaintEvent::Window(IndicatorEvent::Set {
                    id: BabelState::paint_pane_id(addr),
                    color: color.to_string(),
                    workspace: window.workspace.unwrap_or(0) as u32,
                    x_pos,
                    ring_intensity,
                    ring_color,
                    has_outline: false,
                    scale: 1.0,
                }));
            }
        }
        for ws in &replay_workspaces {
            // Mutable borrow on self for emit_workspace_paint conflicts with
            // the loop above; instead synthesize the event inline using a
            // local copy of the state slice, mirroring emit_workspace_paint.
            let states: Vec<scrollparse::claude::ActivityState> = s
                .panes
                .iter()
                .filter(|(_, w)| w.workspace == Some(*ws))
                .filter_map(|(addr, _)| s.pane_states.get(addr).copied())
                .collect();
            if states.is_empty() {
                replay_events.push(PaintEvent::Workspace(WorkspacePaintEvent::Remove {
                    workspace: *ws as u32,
                }));
                continue;
            }
            let css_class = workspace_css_class(&states).map(|c| c.to_string());
            let is_urgent = workspace_is_urgent(&states);
            let awaiting_seconds = s
                .workspace_awaiting_since
                .get(ws)
                .map(|t| t.elapsed().as_secs());
            let title = s.workspace_titles.get(ws).cloned();
            replay_events.push(PaintEvent::Workspace(WorkspacePaintEvent::Set {
                workspace: *ws as u32,
                css_class,
                is_urgent,
                awaiting_seconds,
                window_count: states.len(),
                title,
            }));
        }
        drop(s);
        drop(reader);

        return handle_paint_subscriber(stream, rx, subscriber_id, replay_events).await;
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
        let geometries: Vec<(i32, i32)> = items
            .iter()
            .map(|item| {
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
            })
            .collect();

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

    /// List all agent panes with activity states
    ///
    /// Panes are sorted by screen position (left→right, then top→bottom)
    /// to ensure consistent ordering for panel plugins and other connectors.
    pub async fn list(state: &Arc<RwLock<BabelState>>) -> Response {
        let s = state.read().await;

        // Fetch hook states from database for sessions we know about
        let hook_states: std::collections::HashMap<String, crate::babel_storage::HookState> =
            if let Ok(conn) = init_db() {
                s.panes
                    .values()
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

        let mut panes: Vec<AgentPane> = s
            .panes
            .values()
            .map(|w| {
                let mut win = w.clone();
                // Address-first: w.addr is in scope, no need to round-trip
                // through w.id() (and risk a multi-socket collision).
                win.activity_state = s.pane_activity_state(&w.addr);
                // Populate hook_state from database if we have a session_id
                if let Some(ref sid) = w.session_id {
                    win.hook_state = hook_states.get(sid).copied();
                }
                win
            })
            .collect();
        drop(s); // Release lock before geometry lookups

        // AgentPane doesn't carry pane geometry; fallback to OS window
        sort_by_screen_position(&mut panes, |w| w.platform_window_id, |_| None);
        Response::Windows { windows: panes }
    }

    /// List all terminals (agent and non-agent)
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
            Err(e) => Response::Error {
                message: format!("Failed to list panes: {}", e),
            },
        }
    }

    /// List kitty socket status (multi-instance awareness)
    pub async fn list_sockets(state: &Arc<RwLock<BabelState>>) -> Response {
        let s = state.read().await;
        Response::Sockets {
            sockets: s.socket_status.clone(),
        }
    }

    /// List panes with fingerprints (expensive - extracts from scrollback)
    ///
    /// Panes are sorted by screen position (left→right, then top→bottom)
    /// to ensure consistent ordering for panel plugins and other connectors.
    pub async fn list_with_fingerprints(state: &Arc<RwLock<BabelState>>) -> Response {
        let s = state.read().await;
        let mut panes: Vec<AgentPane> = s
            .panes
            .values()
            .map(|w| {
                let mut win = w.clone();
                // Address-first lookup; bare w.id() would silently pick a
                // winner across sockets when the same id is live in two.
                win.activity_state = s.pane_activity_state(&w.addr);
                win
            })
            .collect();
        drop(s); // Release lock before expensive operations

        for pane in &mut panes {
            if pane.fingerprint.is_none() {
                if let Ok(scrollback) = get_scrollback(pane.id()).await {
                    let fp = extract_from_scrollback(&scrollback);
                    pane.fingerprint = Some(fp);
                }
            }
            if pane.session_info.is_none() {
                let _ = enrich_pane(pane);
            }
        }

        // Sort after fingerprinting (geometry lookup is cheap compared to scrollback)
        // AgentPane doesn't carry pane geometry; fallback to OS window
        sort_by_screen_position(&mut panes, |w| w.platform_window_id, |_| None);
        Response::Windows { windows: panes }
    }

    /// Get status of a specific pane or focused pane.
    ///
    /// `target` is the canonical IPC target carrying either a PaneAddr (no
    /// scan needed) or a legacy bare id (scan all sockets). `None` selects
    /// the focused pane.
    pub async fn status(
        state: &Arc<RwLock<BabelState>>,
        target: Option<&PaneSelector>,
    ) -> Response {
        let s = state.read().await;
        let pane = match target {
            Some(PaneSelector::Addr(addr)) => s.panes.get(addr).cloned(),
            Some(PaneSelector::Id(id)) => s.find_pane_by_id(*id).cloned(),
            None => s.panes.values().find(|w| w.is_focused).cloned(),
        };
        Response::Window {
            window: Box::new(pane),
        }
    }

    /// Enrich a pane with session info.
    pub async fn enrich(state: &Arc<RwLock<BabelState>>, target: &PaneSelector) -> Response {
        let mut s = state.write().await;
        let pane_opt = match target {
            PaneSelector::Addr(addr) => s.panes.get_mut(addr),
            PaneSelector::Id(id) => s.find_pane_by_id_mut(*id),
        };
        if let Some(pane) = pane_opt {
            if let Err(e) = enrich_pane(pane) {
                return Response::Error {
                    message: format!("Failed to enrich: {}", e),
                };
            }
            Response::Window {
                window: Box::new(Some(pane.clone())),
            }
        } else {
            Response::Error {
                message: "Pane not found".to_string(),
            }
        }
    }

    /// Focus a pane (canonical address: no scan; legacy id: scan all sockets).
    pub async fn focus(target: &PaneSelector) -> Response {
        match target {
            PaneSelector::Addr(addr) => match focus_pane_on_socket(&addr.socket, addr.id).await {
                Ok(()) => Response::Ok {
                    message: format!("Focused pane {}", addr.short()),
                },
                Err(e) => Response::Error {
                    message: format!("Focus failed: {}", e),
                },
            },
            PaneSelector::Id(id) => match focus_pane_any(*id).await {
                Ok(result) => {
                    let message = if result.is_non_current {
                        format!(
                            "⚠ Focused pane {} on non-current socket: {}",
                            id,
                            result.addr.short()
                        )
                    } else {
                        format!("Focused pane {}", id)
                    };
                    Response::Ok { message }
                }
                Err(e) => Response::Error {
                    message: format!("Focus failed: {}", e),
                },
            },
        }
    }

    /// Get scrollback from a pane.
    pub async fn scroll(target: &PaneSelector) -> Response {
        match target {
            PaneSelector::Addr(addr) => {
                match get_scrollback_on_socket(&addr.socket, addr.id).await {
                    Ok(text) => Response::Scrollback { text },
                    Err(e) => Response::Error {
                        message: format!("Scroll failed: {}", e),
                    },
                }
            }
            PaneSelector::Id(id) => match get_scrollback_any(*id).await {
                Ok(result) => {
                    if result.is_non_current {
                        trace_error!("scrollback from non-current socket", pane_id = id, addr = %result.addr.short());
                    }
                    Response::Scrollback {
                        text: result.result,
                    }
                }
                Err(e) => Response::Error {
                    message: format!("Scroll failed: {}", e),
                },
            },
        }
    }

    /// Send text to a pane (with Enter/CR).
    pub async fn send(target: &PaneSelector, text: &str) -> Response {
        let text_with_cr = format!("{}\r", text);
        match target {
            PaneSelector::Addr(addr) => {
                match send_text_on_socket(&addr.socket, addr.id, &text_with_cr).await {
                    Ok(()) => Response::Ok {
                        message: format!("Sent to pane {}", addr.short()),
                    },
                    Err(e) => Response::Error {
                        message: format!("Send failed: {}", e),
                    },
                }
            }
            PaneSelector::Id(id) => match send_text_any(*id, &text_with_cr).await {
                Ok(result) => {
                    let message = if result.is_non_current {
                        format!(
                            "⚠ Sent to pane {} on non-current socket: {}",
                            id,
                            result.addr.short()
                        )
                    } else {
                        format!("Sent to pane {}", id)
                    };
                    Response::Ok { message }
                }
                Err(e) => Response::Error {
                    message: format!("Send failed: {}", e),
                },
            },
        }
    }

    /// Type text to a pane without pressing Enter.
    ///
    /// Unlike `send`, this doesn't append a carriage return, so the text
    /// is typed into the input area but not submitted.
    pub async fn type_text(target: &PaneSelector, text: &str) -> Response {
        match target {
            PaneSelector::Addr(addr) => {
                match send_text_on_socket(&addr.socket, addr.id, text).await {
                    Ok(()) => Response::Ok {
                        message: format!("Typed to pane {}", addr.short()),
                    },
                    Err(e) => Response::Error {
                        message: format!("Type failed: {}", e),
                    },
                }
            }
            PaneSelector::Id(id) => match send_text_any(*id, text).await {
                Ok(result) => {
                    let message = if result.is_non_current {
                        format!(
                            "⚠ Typed to pane {} on non-current socket: {}",
                            id,
                            result.addr.short()
                        )
                    } else {
                        format!("Typed to pane {}", id)
                    };
                    Response::Ok { message }
                }
                Err(e) => Response::Error {
                    message: format!("Type failed: {}", e),
                },
            },
        }
    }

    /// Check if a pane has pending (unsent) input.
    ///
    /// Retrieves the scrollback and analyzes the last line to detect
    /// text typed but not yet submitted.
    ///
    /// TODO: Integrate with scrollparse for more robust detection:
    /// - Detect multiline input
    /// - Handle plan mode selection UI
    /// - Support save/restore of pending input during broadcast
    pub async fn has_pending_input(target: &PaneSelector) -> Response {
        let pane_id = target.id();
        let scrollback = match target {
            PaneSelector::Addr(addr) => get_scrollback_on_socket(&addr.socket, addr.id)
                .await
                .map_err(|e| e.to_string()),
            PaneSelector::Id(id) => get_scrollback_any(*id)
                .await
                .map(|r| r.result)
                .map_err(|e| e.to_string()),
        };
        match scrollback {
            Ok(text) => {
                let (has_pending, pending_text) = detect_pending_input_from_scrollback(&text);
                Response::PendingInput {
                    window_id: pane_id,
                    has_pending,
                    pending_text,
                }
            }
            Err(e) => Response::Error {
                message: format!("Failed to check pending input: {}", e),
            },
        }
    }

    /// Tag a pane with an icon.
    pub async fn tag(
        state: &Arc<RwLock<BabelState>>,
        target: &PaneSelector,
        icon: &str,
    ) -> Response {
        let s = state.read().await;
        let pane = match target {
            PaneSelector::Addr(addr) => s.panes.get(addr),
            PaneSelector::Id(id) => s.find_pane_by_id(*id),
        };
        if let Some(pane) = pane {
            if let Some(session_id) = &pane.session_id {
                match init_db().and_then(|conn| set_icon(&conn, session_id, icon)) {
                    Ok(()) => Response::Ok {
                        message: format!("Tagged pane {} with {}", pane.id(), icon),
                    },
                    Err(e) => Response::Error {
                        message: format!("Tag failed: {}", e),
                    },
                }
            } else {
                Response::Error {
                    message: "Pane has no session".to_string(),
                }
            }
        } else {
            Response::Error {
                message: "Pane not found".to_string(),
            }
        }
    }

    /// Mark a pane as read.
    pub async fn mark_read_handler(
        state: &Arc<RwLock<BabelState>>,
        target: &PaneSelector,
    ) -> Response {
        let s = state.read().await;
        let pane = match target {
            PaneSelector::Addr(addr) => s.panes.get(addr),
            PaneSelector::Id(id) => s.find_pane_by_id(*id),
        };
        if let Some(pane) = pane {
            if let Some(session_id) = &pane.session_id {
                match init_db().and_then(|conn| mark_read(&conn, session_id)) {
                    Ok(()) => Response::Ok {
                        message: format!("Marked pane {} as read", pane.id()),
                    },
                    Err(e) => Response::Error {
                        message: format!("Mark read failed: {}", e),
                    },
                }
            } else {
                Response::Error {
                    message: "Pane has no session".to_string(),
                }
            }
        } else {
            Response::Error {
                message: "Pane not found".to_string(),
            }
        }
    }

    /// Get recent session history
    pub fn history(limit: usize) -> Response {
        match get_recent_sessions(limit) {
            Ok(sessions) => Response::History { sessions },
            Err(e) => Response::Error {
                message: format!("History failed: {}", e),
            },
        }
    }

    /// Ping - return daemon uptime
    pub async fn ping(state: &Arc<RwLock<BabelState>>) -> Response {
        let s = state.read().await;
        Response::Pong {
            uptime_secs: s.start_time.elapsed().as_secs(),
        }
    }

    /// Refresh windows and run fingerprint matching
    pub async fn refresh(state: &Arc<RwLock<BabelState>>) -> Response {
        // Phase 1: Refresh windows with full activity fetch (explicit refresh request)
        let window_count = {
            let mut s = state.write().await;
            match s.refresh_panes(false).await {
                // Full refresh with activity states
                Ok(_) => s.panes.len(),
                Err(e) => {
                    return Response::Error {
                        message: format!("Refresh failed: {}", e),
                    }
                }
            }
        };

        // Phase 2: Get windows needing fingerprints + index + claimed sessions
        let (needs_matching, fingerprint_index, mut claimed_sessions) = {
            let s = state.read().await;
            (
                s.get_panes_needing_fingerprints(),
                s.fingerprint_index.clone(),
                s.get_claimed_sessions(),
            )
        };

        // Phase 3: Do expensive I/O without lock
        // Pass kitty CWD (reliable) instead of extracting from scrollback
        // Sequential processing: update claimed_sessions as we go to avoid double-matching
        for (addr, cwd) in needs_matching {
            if let Some((session_id, confidence, fingerprint)) = BabelState::fingerprint_match_addr(
                &addr,
                &cwd,
                &fingerprint_index,
                &claimed_sessions,
            )
            .await
            {
                // Mark this session as claimed before processing next window
                claimed_sessions.insert(session_id.clone());
                let mut s = state.write().await;
                s.apply_fingerprint_result(&addr, session_id, confidence, fingerprint);
            }
        }

        Response::Ok {
            message: format!("Refreshed {} windows", window_count),
        }
    }

    /// Get workspace titles
    pub async fn titles(state: &Arc<RwLock<BabelState>>) -> Response {
        let s = state.read().await;
        let titles: std::collections::HashMap<String, String> = s
            .workspace_titles
            .iter()
            .map(|(k, v)| (k.to_string(), v.clone()))
            .collect();
        Response::Titles { titles }
    }

    /// Resolve the best available title for a platform window or pane.
    pub async fn get_title(
        state: &Arc<RwLock<BabelState>>,
        target: crate::utility::ipc::TitleTarget,
    ) -> Response {
        use crate::utility::ipc::TitleTarget;

        let s = state.read().await;
        let window = match target {
            TitleTarget::PlatformWindow { platform_window_id } => s
                .panes
                .values()
                .find(|w| w.platform_window_id == platform_window_id),
            TitleTarget::Pane {
                os_window_id,
                pane_id,
            } => s
                .panes
                .values()
                .find(|w| w.os_window_id == os_window_id && w.id() == pane_id),
        };

        Response::Title {
            title: window.map(crate::utility::agent_discovery::resolve_pane_title),
        }
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
                s.panes
                    .values()
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
            s.workspace_titles
                .get(&ws)
                .cloned()
                .map(|t| format!("Workspace {}: {}", ws, t))
                .unwrap_or_else(|| format!("Workspace {}: (no windows)", ws))
        } else {
            workspaces_to_refresh
                .iter()
                .filter_map(|ws| {
                    s.workspace_titles
                        .get(ws)
                        .map(|t| format!("  {}: {}", ws, t))
                })
                .collect::<Vec<_>>()
                .join("\n")
        };

        Response::Ok {
            message: format!("Refreshed titles:\n{}", titles),
        }
    }

    // ─── WSet Handlers ─────────────────────────────────────────────────────────

    pub async fn wset_save(state: &Arc<RwLock<BabelState>>, name: Option<String>) -> Response {
        let wset_name = match name {
            Some(n) => n,
            None => get_current_wset_name()
                .ok()
                .flatten()
                .unwrap_or_else(|| "default".to_string()),
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
                Response::WSetSaved {
                    name: wset_name,
                    wspaces,
                    windows,
                }
            }
            Err(e) => Response::Error {
                message: format!("Failed to save WSet: {}", e),
            },
        }
    }

    pub async fn wset_load(
        state: &Arc<RwLock<BabelState>>,
        name: Option<String>,
        dry_run: bool,
    ) -> Response {
        let wset_name = match name {
            Some(n) => n,
            None => match get_current_wset_name() {
                Ok(Some(n)) => n,
                Ok(None) => {
                    return Response::Error {
                        message: "No current WSet. Specify a name or run 'babel save' first."
                            .to_string(),
                    }
                }
                Err(e) => {
                    return Response::Error {
                        message: format!("Failed to read current WSet: {}", e),
                    }
                }
            },
        };

        let wset = match WSet::load(&wset_name) {
            Ok(w) => w,
            Err(e) => {
                return Response::Error {
                    message: format!("Failed to load WSet '{}': {}", wset_name, e),
                }
            }
        };

        let wspaces = wset.wspaces.len();
        let windows = wset.window_count();

        if dry_run {
            return Response::WSetLoaded {
                name: wset_name,
                wspaces,
                windows,
                skipped: vec![],
                dry_run: true,
            };
        }

        let skipped = match load_wset(&wset).await {
            Ok(s) => s,
            Err(e) => {
                return Response::Error {
                    message: format!("Failed to load WSet: {}", e),
                }
            }
        };

        if let Err(e) = set_current_wset_name(&wset_name) {
            effect!("wset", "set_current_name", error = e.to_string());
        }

        {
            let mut s = state.write().await;
            let _ = s.refresh_panes(true).await; // Quick refresh, activity via periodic poll
        }

        Response::WSetLoaded {
            name: wset_name,
            wspaces,
            windows,
            skipped,
            dry_run: false,
        }
    }

    pub fn wset_list() -> Response {
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

    pub fn wset_current() -> Response {
        match get_current_wset_name() {
            Ok(name) => Response::WSetCurrent { name },
            Err(e) => Response::Error {
                message: format!("Failed to get current WSet: {}", e),
            },
        }
    }

    pub fn wset_delete(name: &str) -> Response {
        match WSet::delete(name) {
            Ok(()) => Response::Ok {
                message: format!("Deleted WSet '{}'", name),
            },
            Err(e) => Response::Error {
                message: format!("Failed to delete WSet '{}': {}", name, e),
            },
        }
    }

    pub fn wset_rename(old: &str, new: &str) -> Response {
        match WSet::rename(old, new) {
            Ok(()) => Response::Ok {
                message: format!("Renamed WSet '{}' to '{}'", old, new),
            },
            Err(e) => Response::Error {
                message: format!("Failed to rename WSet: {}", e),
            },
        }
    }

    pub fn wset_describe(name: &str, description: Option<String>) -> Response {
        match WSet::load(name) {
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

    /// Handle hook events pushed directly from Claude Code hooks via IPC.
    ///
    /// This is the direct neural link: instead of hooks writing sqlite and waiting
    /// for the next poll tick to propagate, they push state here instantly.
    ///
    /// On SessionStart (with kitty_id): binds kitty_id ↔ session_id in-memory,
    /// eliminating the need for expensive fingerprint matching (Phase 5).
    ///
    /// On Stop/Prompt/PreTool/PostTool: updates hook_state on the AgentPane
    /// and publishes SessionStateChanged events to subscribers immediately.
    pub async fn hook_event(
        state: &Arc<RwLock<BabelState>>,
        session: &str,
        kitty_id: Option<u64>,
        pane_addr: Option<PaneAddr>,
        agent_kind: AgentKind,
        hook_state: Option<crate::babel_storage::HookState>,
        pulse: PulseEffect,
        read: crate::ReadEffect,
        hook_type: &str,
    ) -> Response {
        let mut s = state.write().await;

        // Find the AgentPane by full address first, then legacy kitty_id,
        // then session. The full address prevents cross-socket collisions.
        let addr = if let Some(addr) = pane_addr {
            if s.panes.contains_key(&addr) {
                Some(addr)
            } else {
                None
            }
        } else if let Some(kid) = kitty_id {
            s.panes
                .iter()
                .find(|(a, _)| a.id == kid)
                .map(|(a, _)| a.clone())
        } else {
            s.panes
                .iter()
                .find(|(_, w)| w.session_id.as_deref() == Some(session))
                .map(|(a, _)| a.clone())
        };

        if let Some(addr) = addr {
            // Extract workspace + agent_kind before releasing the mutable
            // borrow on windows. agent_kind rides on the SessionStateChanged
            // event so panel widgets stay color-correct on hook-driven state
            // changes too (not just poll-driven ones).
            let workspace = s.panes.get(&addr).and_then(|w| w.workspace);
            let resolved_agent_kind = s
                .panes
                .get(&addr)
                .map(|w| {
                    if w.agent_kind == AgentKind::Other {
                        agent_kind
                    } else {
                        w.agent_kind
                    }
                })
                .unwrap_or(agent_kind);

            // Mutate the window: bind session_id + update hook_state
            if let Some(window) = s.panes.get_mut(&addr) {
                if window.session_id.is_none() {
                    tracing::info!(
                        kitty_id = addr.id,
                        session,
                        hook_type,
                        "Hook bound session_id → pane (fingerprinting bypassed)"
                    );
                    window.session_id = Some(session.to_string());
                }
                if window.agent_kind == AgentKind::Other {
                    window.agent_kind = agent_kind;
                }
                if let Some(hook_state) = hook_state {
                    window.hook_state = Some(hook_state);
                }
            }
            // Mutable borrow on s.panes dropped here

            if let Some(hook_state) = hook_state {
                let activity_state = hook_state_activity(hook_state);

                let old_state = s
                    .pane_states
                    .get(&addr)
                    .copied()
                    .unwrap_or(scrollparse::claude::ActivityState::Unknown);

                if old_state != activity_state {
                    s.pane_states.insert(addr.clone(), activity_state);
                    s.event_publisher
                        .publish(crate::events::BabelEvent::SessionStateChanged {
                            addr: addr.clone(),
                            session_id: Some(session.to_string()),
                            workspace,
                            old_state,
                            new_state: activity_state,
                            asking_question: false, // Hooks don't carry this signal yet
                            agent_kind: resolved_agent_kind,
                        });
                    // Paint: hooks are ground truth, so the dot must reflect the
                    // new color immediately and the workspace aggregate must
                    // recompute (a single AwaitingInput pane flips the workspace
                    // to claude-await).
                    s.emit_pane_paint(&addr);
                    if let Some(ws) = workspace {
                        s.emit_workspace_paint(ws);
                    }
                }
            }

            let read_changed = match read {
                crate::ReadEffect::Preserve => false,
                crate::ReadEffect::MarkRead => s.pane_unread.remove(&addr),
                crate::ReadEffect::MarkUnread => s.pane_unread.insert(addr.clone()),
            };
            if read_changed {
                s.emit_pane_paint(&addr);
            }

            let pulse_intensity = match pulse {
                PulseEffect::None => 0.0,
                PulseEffect::Session => 0.4,
                PulseEffect::Prompt => 0.5,
                PulseEffect::Tool => 0.65,
                PulseEffect::Finished => 0.9,
                PulseEffect::Attention => 0.75,
                PulseEffect::Compact => 0.55,
            };
            if pulse_intensity > 0.0 {
                s.event_publisher
                    .publish(crate::events::BabelEvent::ActivityPulse {
                        addr: addr.clone(),
                        session_id: Some(session.to_string()),
                        workspace,
                        intensity: pulse_intensity,
                        trigger: crate::events::PulseTrigger::HookLifecycle,
                    });
                s.bump_ring_and_emit(&addr, pulse_intensity as f64);
            }

            Response::Ok {
                message: format!(
                    "hook_event: {} → {:?} (kitty {})",
                    session, hook_state, addr.id
                ),
            }
        } else {
            // Window not yet discovered by polling — this is normal during startup
            // or if the hook fires before the first poll tick. The sqlite write
            // ensures the state is picked up on next poll.
            tracing::debug!(
                session,
                ?kitty_id,
                hook_type,
                "Hook event for unknown window (will catch on next poll)"
            );
            Response::Ok {
                message: format!(
                    "hook_event: {} → {:?} (window not yet tracked)",
                    session, hook_state
                ),
            }
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
        Request::Status { target } => handlers::status(state, target.as_ref()).await,
        Request::History { limit } => handlers::history(limit),
        Request::Ping => handlers::ping(state).await,
        Request::Titles => handlers::titles(state).await,

        // ─── Pane Handlers ──────────────────────────────────────────────────────
        Request::Enrich { target } => handlers::enrich(state, &target).await,
        Request::Focus { target } => handlers::focus(&target).await,
        Request::Scroll { target } => handlers::scroll(&target).await,
        Request::Send { target, text } => {
            let pane_id = target.id();
            let response = handlers::send(&target, &text).await;
            // Trigger workspace re-summarization on user prompt
            // User just sent new instructions to the agent, context is changing
            if matches!(response, Response::Ok { .. }) {
                let workspace = {
                    let s = state.read().await;
                    // Route through the legacy shim so a same-id collision
                    // across sockets refuses cleanly instead of summarizing
                    // the wrong workspace.
                    match target.addr() {
                        Some(addr) => s.panes.get(addr).and_then(|w| w.workspace),
                        None => s.find_pane_by_id(pane_id).and_then(|w| w.workspace),
                    }
                };
                if let Some(ws) = workspace {
                    summarize_workspace(ws, state, summarizer).await;
                }
            }
            response
        }
        Request::Type { target, text } => handlers::type_text(&target, &text).await,
        Request::HasPendingInput { target } => handlers::has_pending_input(&target).await,

        // ─── State Handlers ─────────────────────────────────────────────────────
        Request::Tag { target, icon } => handlers::tag(state, &target, &icon).await,
        Request::MarkRead { target } => handlers::mark_read_handler(state, &target).await,
        Request::Refresh => handlers::refresh(state).await,
        Request::GetTitle { target } => handlers::get_title(state, target).await,
        Request::TitleRefresh { workspace } => {
            handlers::title_refresh(state, summarizer, workspace).await
        }

        // ─── WSet Handlers ──────────────────────────────────────────────────────
        Request::WSetSave { name } => handlers::wset_save(state, name).await,
        Request::WSetLoad { name, dry_run } => handlers::wset_load(state, name, dry_run).await,
        Request::WSetList => handlers::wset_list(),
        Request::WSetCurrent => handlers::wset_current(),
        Request::WSetDelete { name } => handlers::wset_delete(&name),
        Request::WSetRename { old, new } => handlers::wset_rename(&old, &new),
        Request::WSetDescribe { name, description } => handlers::wset_describe(&name, description),

        // ─── Hook Event Handler (direct push from Claude Code hooks) ───────────
        Request::HookEvent {
            session,
            kitty_id,
            pane_addr,
            agent_kind,
            hook_state,
            pulse,
            read,
            hook_type,
        } => {
            handlers::hook_event(
                state, &session, kitty_id, pane_addr, agent_kind, hook_state, pulse, read,
                &hook_type,
            )
            .await
        }

        // ─── System Handlers ────────────────────────────────────────────────────
        Request::Shutdown => Response::Ok {
            message: "Shutting down".to_string(),
        },

        // Solo mode (debugging feature - isolate single pane)
        // TODO: Implement solo mode handler when needed
        Request::Solo { target } => Response::Error {
            message: format!("Solo mode not yet implemented (target: {:?})", target),
        },

        // Subscribe is handled specially in handle_client
        Request::Subscribe { .. } => Response::Error {
            message: "Subscribe requests must be handled via handle_client".to_string(),
        },
        // SubscribePaint is also handled in handle_client (long-lived stream).
        Request::SubscribePaint => Response::Error {
            message: "SubscribePaint requests must be handled via handle_client".to_string(),
        },
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
#[cfg(feature = "wnck-watch")]
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
                let event = DaemonEvent::WorkspaceChange {
                    platform_window_id: xid,
                };
                let _ = rt.block_on(tx.send(event));
            });
        }
    };

    // Connect to existing windows
    for window in screen.get_windows() {
        connect_window(window);
    }

    // Connect to new windows as they open
    //
    // Also fire a WorkspaceChange to invalidate the workspace_cache. The pane belongs
    // to a platform_window_id we've never seen — without this, refresh_panes would
    // hand back a stale cache that doesn't include the new pid, and the pane would
    // render as "Workspace ?" until something else dirtied the cache.
    let connect_fn = connect_window.clone();
    let opened_tx = tx.clone();
    let opened_rt = rt.clone();
    screen.connect_window_opened(move |window| {
        let xid = window.xid();
        tracing::trace!(xid, "WNCK: window-opened");
        connect_fn(window);
        let event = DaemonEvent::WorkspaceChange {
            platform_window_id: xid,
        };
        let _ = opened_rt.block_on(opened_tx.send(event));
    });

    checkpoint!("wnck_watcher_ready");

    // Run GLib main loop (blocks forever, processing signals)
    let main_loop = glib::MainLoop::new(None, false);
    main_loop.run();
}
