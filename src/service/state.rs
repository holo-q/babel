//! Domain state owned by the service layer.
//!
//! Daemon runtime code still applies most effects today, but the state model
//! belongs here: panes, terminals, matching indexes, paint caches, and legacy
//! compatibility DTOs. Keeping these types outside `daemon.rs` lets core and
//! tests depend on the service boundary without importing the runtime loop.

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::PathBuf;
use std::time::Instant;

use crate::events::EventPublisher;
use crate::fingerprint::SessionFingerprint;
use crate::kitty::PaneAddr;
use crate::model::PaneActivity;
use crate::paint::PaintEvent;
use crate::utility::agent_discovery::AgentPane;

/// Summary entry for fast title→session matching.
///
/// Fields are crate-visible because the daemon's `match_title_to_session` and
/// `rebuild_summary_index` methods construct/inspect entries directly. They
/// are not part of the public service API.
#[derive(Debug, Clone)]
pub struct SummaryEntry {
    pub(crate) summary: String,
    pub(crate) session_id: String,
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
    /// X11/Wayland window ID for geometry lookup.
    ///
    /// Used for screen-position sorting (left-to-right ordering for
    /// connectors).
    pub platform_window_id: u64,
}

impl TerminalInfo {
    /// Get the kitty window ID (convenience).
    pub fn id(&self) -> u64 {
        self.addr.id
    }
}

/// Status of a kitty socket.
///
/// Used to track which sockets are responsive and emit warnings when multiple
/// kitty instances are detected.
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

/// Per-window scrollback activity tracking for ActivityPulse events.
///
/// Tracks scrollback changes to detect when an agent is outputting tokens or
/// executing tools. Used to emit fine-grained activity pulses for reactive UI
/// animations in frontends like richspace-babel.
#[derive(Debug, Clone)]
pub struct ScrollbackActivity {
    /// Fast hash of scrollback content (for change detection).
    pub content_hash: u64,
    /// Scrollback length in bytes (for computing delta intensity).
    pub byte_length: usize,
    /// Recent delta sizes (rolling window for intensity smoothing).
    ///
    /// Stored as (bytes_delta, timestamp) pairs.
    pub recent_deltas: VecDeque<(usize, Instant)>,
    /// Timestamp of last content change.
    pub last_change: Option<Instant>,
}

impl ScrollbackActivity {
    pub fn new() -> Self {
        Self {
            content_hash: 0,
            byte_length: 0,
            recent_deltas: VecDeque::with_capacity(10),
            last_change: None,
        }
    }

    /// Update with new scrollback content, return (changed, delta_bytes).
    pub fn update(&mut self, scrollback: &str) -> (bool, usize) {
        use std::collections::hash_map::DefaultHasher;
        use std::hash::{Hash, Hasher};

        let mut hasher = DefaultHasher::new();
        scrollback.hash(&mut hasher);
        let new_hash = hasher.finish();

        let new_len = scrollback.len();
        let delta = new_len.saturating_sub(self.byte_length);

        if new_hash != self.content_hash {
            self.content_hash = new_hash;
            self.byte_length = new_len;
            self.last_change = Some(Instant::now());

            self.recent_deltas.push_back((delta, Instant::now()));
            while self.recent_deltas.len() > 10 {
                self.recent_deltas.pop_front();
            }

            (true, delta)
        } else {
            (false, 0)
        }
    }

    /// Compute activity intensity from recent deltas (0.0-1.0).
    pub fn compute_intensity(&self) -> f32 {
        if self.recent_deltas.is_empty() {
            return 0.0;
        }

        let now = Instant::now();
        let window = std::time::Duration::from_secs(2);

        let recent_bytes: usize = self
            .recent_deltas
            .iter()
            .filter(|(_, ts)| now.duration_since(*ts) < window)
            .map(|(bytes, _)| *bytes)
            .sum();

        match recent_bytes {
            0..=100 => recent_bytes as f32 / 100.0 * 0.3,
            101..=500 => 0.3 + (recent_bytes - 100) as f32 / 400.0 * 0.3,
            _ => (0.6 + (recent_bytes - 500) as f32 / 1000.0 * 0.4).min(1.0),
        }
    }
}

impl Default for ScrollbackActivity {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Debug)]
pub(crate) struct PaneActivityApply {
    pub(crate) old_state: Option<scrollparse::claude::ActivityState>,
    pub(crate) new_state: scrollparse::claude::ActivityState,
    pub(crate) state_changed: bool,
    pub(crate) initialized: bool,
}

/// Babel state - shared across daemon and local core mode.
///
/// All window/terminal state is keyed by PaneAddr (socket + id) to support
/// multiple kitty instances.
pub struct BabelState {
    pub panes: HashMap<PaneAddr, AgentPane>,
    pub terminals: HashMap<PaneAddr, TerminalInfo>,
    pub summary_index: Vec<SummaryEntry>,
    pub fingerprint_index: HashMap<String, SessionFingerprint>,
    pub session_paths: HashMap<String, PathBuf>,
    pub pane_fingerprints: HashMap<PaneAddr, SessionFingerprint>,
    pub pane_states: HashMap<PaneAddr, scrollparse::claude::ActivityState>,
    pub pane_activity_snapshots: HashMap<PaneAddr, PaneActivity>,
    pub pane_activity: HashMap<PaneAddr, ScrollbackActivity>,
    pub start_time: Instant,
    pub last_kitty_scan: Instant,
    pub last_fingerprint_rebuild: Instant,
    pub event_publisher: EventPublisher,
    pub paint_publisher: tokio::sync::broadcast::Sender<PaintEvent>,
    pub pane_ring: HashMap<PaneAddr, f64>,
    pub pane_unread: HashSet<PaneAddr>,
    pub workspace_awaiting_since: HashMap<i32, Instant>,
    pub workspace_titles: HashMap<i32, String>,
    pub socket_status: HashMap<String, SocketStatus>,
    pub workspace_cache: HashMap<u64, i32>,
    pub workspace_cache_dirty: bool,
}

impl Default for BabelState {
    fn default() -> Self {
        Self::new()
    }
}

impl BabelState {
    pub fn new() -> Self {
        let (paint_tx, _) = tokio::sync::broadcast::channel(256);
        Self {
            panes: HashMap::new(),
            terminals: HashMap::new(),
            summary_index: Vec::new(),
            fingerprint_index: HashMap::new(),
            session_paths: HashMap::new(),
            pane_fingerprints: HashMap::new(),
            pane_states: HashMap::new(),
            pane_activity_snapshots: HashMap::new(),
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
            workspace_cache_dirty: true,
        }
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
