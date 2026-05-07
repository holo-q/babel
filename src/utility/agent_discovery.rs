//! Discovery - Match kitty panes to agent sessions
//!
//! This module implements the core discovery logic for identifying which kitty panes
//! are running an agent harness, then matching those panes to the corresponding
//! durable conversation sessions. Claude Code is currently the storage-backed
//! provider, so session metadata still resolves through ~/.claude/projects/.
//!
//! ## Matching Strategy
//!
//! 1. **Pre-tagged panes**: Check user_vars for "babel_session_id" - already matched
//! 2. **Title extraction**: Strip "✳ " prefix from Claude Code active titles
//! 3. **Conversation search**: Search ~/.claude/projects/ for matching summaries
//! 4. **Tagging**: Once matched, tag the pane with session ID for fast future lookups
//! 5. **CWD tiebreaker**: If multiple matches, use working directory proximity
//!
//! ## Window Title Formats
//!
//! - Active sessions: "✳ Summary Text Here"
//! - Idle terminals: "~/path: command"
//! - Some variations include project info
//!
//! The summary text is set by the provider and appears in the pane title.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use tracing::instrument;
use vtr::{boundary, checkpoint, effect, trace_error};

use crate::agent_kind::AgentKind;
use crate::backend::Pane;
use crate::fingerprint::{MatchConfidence, SessionFingerprint};
use crate::kitty::{
    close_pane, get_pane, get_recent_scrollback, list_panes, move_window_to_workspace,
    set_user_var, set_user_var_on_socket, set_window_geometry, PaneAddr,
};
use crate::utility::claude_storage::{
    find_session_by_summary, get_session_display_name, SessionInfo,
};
use crate::wset::WSet;

// ═══════════════════════════════════════════════════════════════════════════════
// Agent Detection
// ═══════════════════════════════════════════════════════════════════════════════

/// Detection signals for identifying agent sessions (Claude / Codex / …)
///
/// Multiple signals can be present simultaneously. A pane is considered an
/// agent session if ANY signal is positive. The `agent` field disambiguates
/// which harness is running — feeds the panel-color dispatch (Claude orange,
/// Codex cyan) and any agent-specific business logic.
///
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AgentMarkers {
    /// Agent process found in foreground cmdline (definitive)
    pub process_running: bool,
    /// "✳" prefix in title (Claude's active session indicator).
    /// Codex doesn't decorate the title; this stays false for codex panes
    /// even when an agent is active.
    pub title_indicator: bool,
    /// Has `babel_session_id` user_var (previously tagged by babel)
    pub babel_tagged: bool,
    /// Session ID if tagged
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    /// Which harness owns this pane. `Other` when no agent process or marker
    /// matched — caller can ignore those panes via `is_agent()` returning false.
    #[serde(default)]
    pub agent: AgentKind,
    /// Harness is running but did not expose a direct resume/session id, so the
    /// daemon should fall back to native storage keyed by cwd.
    pub infer_session_from_cwd: bool,
}

impl AgentMarkers {
    /// Returns true if any signal suggests this is an agent session
    /// (Claude, Codex, or anything else we learn to detect).
    pub fn is_agent(&self) -> bool {
        self.process_running || self.title_indicator || self.babel_tagged
    }

    /// Returns a short status string for display
    pub fn status(&self) -> &'static str {
        if self.process_running {
            "running" // Actively running agent process
        } else if self.title_indicator {
            "titled" // Has ✳ title but process exited (at shell prompt)
        } else if self.babel_tagged {
            "tagged" // Previously tagged but no other signals
        } else {
            "none" // Not an agent session
        }
    }

    /// Returns emoji indicator for the detection status
    pub fn indicator(&self) -> &'static str {
        if self.process_running {
            "●" // Solid - definitely running
        } else if self.title_indicator {
            "◐" // Half - session exists but at shell
        } else if self.babel_tagged {
            "○" // Empty - was tagged but no active signals
        } else {
            " " // Nothing
        }
    }
}

/// Analyze a pane for agent session signals (Claude or Codex).
///
/// Walks the foreground process list and matches the first agent we recognize.
/// Title-prefix detection is Claude-specific (the "✳" sigil); codex doesn't
/// decorate titles, so codex panes are picked up purely via cmdline / babel-tag.
pub fn detect_agent_signals(window: &Pane) -> AgentMarkers {
    let mut detected_agent: Option<AgentKind> = None;
    for proc in &window.foreground_processes {
        if let Some(kind) = AgentKind::from_cmdline(&proc.cmdline) {
            detected_agent = Some(kind);
            break;
        }
    }
    let process_running = detected_agent.is_some();

    let title_indicator = window.title.starts_with("✳");

    let babel_tagged = window
        .user_vars
        .get("babel_session_id")
        .map(|s| !s.is_empty())
        .unwrap_or(false);

    let session_id = window
        .user_vars
        .get("babel_session_id")
        .filter(|s| !s.is_empty())
        .cloned();

    // If a process matched, trust that. Otherwise, the title sigil is
    // Claude-specific so default to Claude when only the title or babel-tag
    // signal fired. Codex without a live process leaves no fingerprint, so
    // there's no codex-only-tagged inference path yet.
    let agent = detected_agent
        .or_else(|| {
            if title_indicator || babel_tagged {
                Some(AgentKind::Claude)
            } else {
                None
            }
        })
        .unwrap_or_default();

    let infer_session_from_cwd =
        process_running && session_id.is_none() && matches!(detected_agent, Some(AgentKind::Codex));

    AgentMarkers {
        process_running,
        title_indicator,
        babel_tagged,
        session_id,
        agent,
        infer_session_from_cwd,
    }
}

/// Get the current activity state of an agent pane by pane ID.
///
/// Convenience function that fetches scrollback and detects activity state.
/// Returns Unknown on any error (window not found, kitten failure, etc.)
#[instrument(level = "debug", fields(kitty_id = id))]
pub async fn get_pane_activity_state(id: u64) -> scrollparse::claude::ActivityState {
    match get_recent_scrollback(id, 20).await {
        Ok(scrollback) => scrollparse::claude::detect_activity_state(&scrollback),
        Err(_) => scrollparse::claude::ActivityState::Unknown,
    }
}

/// Get both activity state and scrollback content for an agent pane.
///
/// Activity detection result with all derived state
#[derive(Debug, Clone)]
pub struct ActivityResult {
    /// Current activity state (Idle, Thinking, ToolUse, etc.)
    pub state: scrollparse::claude::ActivityState,
    /// True if the agent's last message ended with a question
    pub asking_question: bool,
    /// Raw scrollback content (for pulse tracking)
    pub scrollback: String,
}

/// Returns activity state, asking_question, and scrollback for use in both
/// state detection and activity pulse tracking. Avoids double-fetching scrollback.
///
/// Returns (Unknown, false, empty string) on any error.
///
/// NOTE: This uses the default socket. For windows on other sockets, use
/// `get_activity_with_scrollback_on_socket` instead.
#[instrument(level = "debug", fields(kitty_id = id))]
pub async fn get_pane_activity_with_scrollback(id: u64) -> ActivityResult {
    match get_recent_scrollback(id, 50).await {
        Ok(scrollback) => {
            let state = scrollparse::claude::detect_activity_state(&scrollback);
            let asking_question = scrollparse::claude::detect_asking_question(&scrollback);
            ActivityResult {
                state,
                asking_question,
                scrollback,
            }
        }
        Err(_) => ActivityResult {
            state: scrollparse::claude::ActivityState::Unknown,
            asking_question: false,
            scrollback: String::new(),
        },
    }
}

/// Get activity state and scrollback for a pane on a specific socket
///
/// Socket-aware variant that works with windows on non-default kitty instances.
/// Takes a PaneAddr to correctly route the query to the right socket.
///
/// Returns ActivityResult with Unknown state on any error.
#[instrument(level = "debug", skip(addr), fields(kitty_id = addr.id))]
pub async fn get_activity_with_scrollback_on_socket(
    addr: &crate::kitty::PaneAddr,
) -> ActivityResult {
    use crate::kitty::get_recent_scrollback_on_socket;

    match get_recent_scrollback_on_socket(&addr.socket, addr.id, 50).await {
        Ok(scrollback) => {
            let state = scrollparse::claude::detect_activity_state(&scrollback);
            let asking_question = scrollparse::claude::detect_asking_question(&scrollback);
            ActivityResult {
                state,
                asking_question,
                scrollback,
            }
        }
        Err(_) => ActivityResult {
            state: scrollparse::claude::ActivityState::Unknown,
            asking_question: false,
            scrollback: String::new(),
        },
    }
}

/// Find all kitty panes that appear to be running an agent.
///
/// A pane is considered an agent session if any of these are true:
/// - Has a recognized agent process in the foreground process list
/// - Has title starting with "✳ " (active agent session indicator)
/// - Has `babel_session_id` user_var set (previously tagged by babel)
///
/// This catches sessions that have exited to shell prompt but still have
/// the ✳ title, or panes that were previously identified and tagged.
pub async fn find_agent_panes() -> Result<Vec<Pane>> {
    use crate::backend::kitty::kitty_pane_to_pane;

    let all_panes = list_panes().await?;

    let agent_panes = all_panes
        .into_iter()
        .map(kitty_pane_to_pane)
        .filter(|pane| detect_agent_signals(pane).is_agent())
        .collect();

    Ok(agent_panes)
}

/// A single agent in the tower - one voice among many.
///
/// Each AgentPane represents an individual agent session running in a kitty
/// terminal pane. In the Babel architecture, these are the workers: autonomous panes
/// each engaged in their own conversation, yet observable and (soon) directable by
/// a Captain orchestrator. The pane carries both its terminal identity (`addr`) and
/// its conversational soul (`session_id`, `fingerprint`).
///
/// Uniquely identified by `addr` (PaneAddr = socket + kitty pane ID).
/// This supports multiple kitty instances where pane IDs may collide.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentPane {
    /// Unique address of this pane across all kitty instances
    pub addr: PaneAddr,
    pub title: String,
    pub session_id: Option<String>,
    pub session_info: Option<SessionInfo>,
    pub cwd: PathBuf,
    pub is_focused: bool,
    /// Kitty OS window ID (internal)
    pub os_window_id: u64,
    /// X11/Wayland window ID
    pub platform_window_id: u64,
    /// Workspace number (-1 = sticky/all, 0+ = workspace index)
    pub workspace: Option<i32>,
    /// Which harness owns this pane (Claude vs Codex). Set from cmdline
    /// detection at discovery time; flows downstream to the panel-color
    /// dispatch so each agent gets its own dot family.
    #[serde(default)]
    pub agent_kind: AgentKind,
    /// Pane screen geometry for spatial sorting (from patched kitty)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub screen: Option<crate::backend::ScreenGeometry>,
    /// Current activity state (Idle, Thinking, ToolUse, etc.)
    /// Populated from scrollback analysis when available
    #[serde(skip_serializing_if = "Option::is_none")]
    pub activity_state: Option<scrollparse::claude::ActivityState>,
    /// Hook state—ground truth from Claude Code lifecycle events
    /// This is deterministic: Working (after prompt submit) or Idle (after stop)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hook_state: Option<crate::babel_storage::HookState>,
    /// Extracted fingerprint from scrollback (for debugging/verification)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fingerprint: Option<SessionFingerprint>,
    /// Match confidence if matched via fingerprinting
    #[serde(skip_serializing_if = "Option::is_none")]
    pub match_confidence: Option<MatchConfidence>,
}

impl AgentPane {
    // ─── Convenience Accessors ──────────────────────────────────────────────────

    /// Get the kitty pane ID.
    pub fn id(&self) -> u64 {
        self.addr.id
    }

    /// Get the socket path
    pub fn socket(&self) -> &str {
        &self.addr.socket
    }

    /// Check if this pane is on the current/default kitty socket.
    pub fn is_current_socket(&self) -> bool {
        self.addr.is_current_socket()
    }

    // ─── Operations ─────────────────────────────────────────────────────────────

    /// Focus this pane.
    pub async fn focus(&self) -> Result<()> {
        crate::kitty::focus_pane_on_socket(&self.addr.socket, self.addr.id).await
    }

    /// Send text to this pane's input.
    pub async fn send_text(&self, text: &str) -> Result<()> {
        crate::kitty::send_text_on_socket(&self.addr.socket, self.addr.id, text).await
    }

    /// Set a user variable on this pane.
    pub async fn set_user_var(&self, key: &str, value: &str) -> Result<()> {
        crate::kitty::set_user_var_on_socket(&self.addr.socket, self.addr.id, key, value).await
    }

    /// Set the title of this pane.
    pub async fn set_title(&self, title: &str) -> Result<()> {
        crate::kitty::set_title_on_socket(&self.addr.socket, self.addr.id, title).await
    }

    /// Get the full scrollback buffer
    pub async fn scrollback(&self) -> Result<String> {
        crate::kitty::get_scrollback_on_socket(&self.addr.socket, self.addr.id).await
    }

    /// Get the last N lines of scrollback
    pub async fn recent_scrollback(&self, lines: usize) -> Result<String> {
        crate::kitty::get_recent_scrollback_on_socket(&self.addr.socket, self.addr.id, lines).await
    }

    /// Close this pane.
    pub async fn close(&self) -> Result<()> {
        crate::kitty::close_pane_on_socket(&self.addr.socket, self.addr.id).await
    }
}

/// Fast discovery - just finds agent panes without expensive session matching.
///
/// This is O(1) kitty call + O(1) wmctrl call - no filesystem scanning.
/// For session info, call `enrich_pane` on specific panes.
pub async fn discover_agent_panes() -> Result<Vec<AgentPane>> {
    use crate::kitty::get_all_workspaces;

    let agent_panes = find_agent_panes()
        .await
        .context("Failed to find agent panes")?;

    // Get workspace mappings in one call
    let workspaces = get_all_workspaces();

    let discovered = agent_panes
        .into_iter()
        .map(|pane| {
            // Re-run detection so we know which agent owns this pane.
            // find_agent_panes already filters with the same predicate,
            // but it discards the agent kind — re-detect here to keep the
            // discovery API a pure pipeline (no joined state).
            let markers = detect_agent_signals(&pane);

            // Check if already tagged (fast - just reads user_vars from kitty output)
            let session_id = pane
                .user_vars
                .get("babel_session_id")
                .filter(|s| !s.is_empty())
                .cloned();

            // Look up workspace for this OS window
            let workspace = pane
                .platform_window_id
                .and_then(|pid| workspaces.get(&pid).copied());

            AgentPane {
                addr: pane.addr(),
                title: pane.title.clone(),
                session_id,
                session_info: None, // Lazy - only load on demand
                cwd: pane.cwd.clone(),
                is_focused: pane.is_focused,
                os_window_id: pane.os_window_id().unwrap_or(0),
                platform_window_id: pane.platform_window_id.unwrap_or(0),
                workspace,
                agent_kind: markers.agent,
                screen: pane.screen().cloned(),
                activity_state: None, // Populated by daemon from cached scrollback analysis
                hook_state: None,     // Populated by daemon from babel_storage
                fingerprint: None,    // Only populated in daemon with --details
                match_confidence: None,
            }
        })
        .collect();

    Ok(discovered)
}

/// Enrich a pane with full session info (expensive - searches ~/.claude).
///
/// Call this only when you need the full session metadata for a specific pane.
/// This does the filesystem scan to match pane title → conversation file.
pub fn enrich_pane(pane: &mut AgentPane) -> Result<()> {
    // If already has session_info, skip
    if pane.session_info.is_some() {
        return Ok(());
    }

    // Use shared resolution logic
    if let Some(info) = resolve_session(pane.session_id.as_deref(), &pane.title, pane.id())? {
        pane.session_id = Some(info.session_id.clone());
        pane.session_info = Some(info);
    }

    Ok(())
}

/// Match a single pane to its session.
///
/// Matching algorithm:
/// 1. Check user_vars for "babel_session_id" - already tagged
/// 2. If not tagged:
///    - Extract summary from title (strip "✳ " prefix)
///    - Search ~/.claude/projects/ for matching summaries
///    - If found, tag pane with session_id for future lookups
/// 3. Return the session ID (as SessionInfo) if matched, None otherwise
pub fn match_pane_to_session(pane: &Pane) -> Result<Option<SessionInfo>> {
    let existing_session_id = pane
        .user_vars
        .get("babel_session_id")
        .filter(|s| !s.is_empty())
        .map(|s| s.as_str());

    resolve_session(existing_session_id, &pane.title, pane.id)
}

/// Core session resolution logic (shared by enrich_pane and match_pane_to_session)
///
/// 1. If session_id provided, try to load directly (O(1) if cached, O(n×m) otherwise)
/// 2. Extract summary from title and search by summary
/// 3. Tag pane with result for fast future lookups
fn resolve_session(
    session_id: Option<&str>,
    title: &str,
    pane_id: u64,
) -> Result<Option<SessionInfo>> {
    // Try loading by session_id first (already tagged)
    if let Some(sid) = session_id {
        if let Some(info) = load_session_by_id(sid, None)? {
            return Ok(Some(info));
        }
        // Session file missing/moved - fall through to re-match by title
    }

    // Extract summary from title
    let summary = extract_summary_from_title(title);
    if summary.is_empty() {
        return Ok(None); // Not an active title-backed session
    }

    // Find matching session by searching conversation files
    let session_info = find_session_by_summary(&summary)?;

    // Tag the pane for fast future lookups
    if let Some(ref info) = session_info {
        let _ = tag_pane(pane_id, &info.session_id);
    }

    Ok(session_info)
}

/// Resolve the best available human title for a tracked pane.
///
/// Resolution order:
/// 1. Harness-native display name if the session ID maps to durable storage
/// 2. Session summary or first prompt if session info is loaded
/// 3. Pane title stripped of Claude's active prefix
pub fn resolve_pane_title(pane: &AgentPane) -> String {
    let display_name = pane
        .session_id
        .as_ref()
        .and_then(|id| get_session_display_name(id));

    if let Some(name) = display_name {
        return name;
    }

    if let Some(ref info) = pane.session_info {
        if let Some(summary) = info.summaries.first().map(|s| s.summary.clone()) {
            return summary;
        }

        if let Some(first_prompt) = info.first_prompt.clone() {
            return first_prompt;
        }
    }

    pane.title
        .trim()
        .trim_start_matches("✳ ")
        .trim()
        .to_string()
}

/// Tag a pane with its session ID for fast future lookups.
///
/// Uses kitty's user_vars feature to persistently associate a session ID with a pane.
/// The tag survives as long as the pane exists.
#[instrument(level = "debug", fields(kitty_id = kitty_id))]
pub async fn tag_pane(kitty_id: u64, session_id: &str) -> Result<()> {
    set_user_var(kitty_id, "babel_session_id", session_id)
        .await
        .context("Failed to tag pane with session ID")
}

/// Tag a pane with full socket precision.
///
/// Hooks run inside the pane and can provide both `KITTY_LISTEN_ON` and
/// `KITTY_WINDOW_ID`. Using the full address prevents cross-instance ID
/// collisions when multiple kitty towers exist.
#[instrument(level = "debug", fields(kitty_id = addr.id, socket = %addr.socket))]
pub async fn tag_pane_addr(addr: &PaneAddr, session_id: &str) -> Result<()> {
    set_user_var_on_socket(&addr.socket, addr.id, "babel_session_id", session_id)
        .await
        .context("Failed to tag pane with session ID")
}

// ============================================================================
// Internal Implementation
// ============================================================================

/// Extract summary text from a window title
///
/// Strips the "✳ " prefix from active agent sessions.
/// Returns empty string if not an active session.
fn extract_summary_from_title(title: &str) -> String {
    if let Some(summary) = title.strip_prefix("✳ ") {
        summary.trim().to_string()
    } else {
        String::new()
    }
}

/// Load session info by session ID
///
/// If `path_cache` is provided and contains the session_id, uses O(1) lookup.
/// Otherwise falls back to O(n×m) scan of all projects/sessions.
///
/// The cache is populated by BabelState::rebuild_summary_index() in daemon mode.
pub fn load_session_by_id(
    session_id: &str,
    path_cache: Option<&std::collections::HashMap<String, std::path::PathBuf>>,
) -> Result<Option<SessionInfo>> {
    use crate::utility::claude_storage::{get_session_info, list_projects, list_sessions};

    // Fast path: use cache if available
    if let Some(cache) = path_cache {
        if let Some(session_path) = cache.get(session_id) {
            if session_path.exists() {
                return Ok(Some(get_session_info(session_path)?));
            }
        }
        // Cache miss or stale - fall through to scan
    }

    // Slow path: search all projects for this session ID
    for project_dir in list_projects()? {
        for session_path in list_sessions(&project_dir)? {
            if let Some(file_stem) = session_path.file_stem().and_then(|s| s.to_str()) {
                if file_stem == session_id {
                    let info = get_session_info(&session_path)?;
                    return Ok(Some(info));
                }
            }
        }
    }

    Ok(None)
}

// ═══════════════════════════════════════════════════════════════════════════════
// Session Spawning
// ═══════════════════════════════════════════════════════════════════════════════

/// Spawn an agent session in a new kitty pane.
///
/// The current storage-backed provider resumes through `kitty-claude`; this
/// wrapper keeps the domain API agent-first while the launch implementation
/// remains provider-specific. Always spawns on the MAIN kitty socket (lowest
/// PID) to consolidate panes after multi-instance accidents. Returns the new
/// kitty pane ID after a brief delay for the pane to appear.
pub async fn spawn_agent_session(session_id: &str, cwd: &std::path::Path) -> Result<Option<u64>> {
    use std::collections::HashSet;
    use std::process::{Command, Stdio};
    use tokio::time::{sleep, Duration};

    // Verify the session exists in ~/.claude before spawning
    let claude_base = crate::utility::claude_storage::claude_base();
    let projects_dir = claude_base.join("projects");

    // Search for the session file in any project directory
    let mut session_exists = false;
    if projects_dir.exists() {
        for entry in std::fs::read_dir(&projects_dir)? {
            let entry = entry?;
            let session_path = entry.path().join(format!("{}.jsonl", session_id));
            if session_path.exists() {
                session_exists = true;
                break;
            }
        }
    }

    if !session_exists {
        trace_error!("session file not found", session_id = session_id);
        return Ok(None);
    }

    // CRITICAL: Capture existing pane IDs BEFORE spawning.
    // This prevents false-positive detection of pre-existing panes.
    let existing_ids: HashSet<u64> = find_agent_panes().await?.iter().map(|w| w.id).collect();
    checkpoint!("spawn_prep", existing_count = existing_ids.len());

    // Get the main socket (lowest PID) - consolidates to primary kitty instance
    // This ensures wset load always restores to the "real" kitty, not orphan instances
    let main_socket = crate::kitty::main_socket();

    // Spawn kitty-claude with the session
    // kitty-claude handles random background selection and consistent styling
    // KITTY_LISTEN_ON forces targeting the main socket, not whatever socket
    // the daemon happens to be running from
    let mut cmd = Command::new("kitty-claude");
    cmd.args(["-d", &cwd.to_string_lossy()])
        // -r is now a proper flag in kitty-claude (not passed through to claude)
        .args(["-r", session_id])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());

    // Target main socket explicitly - critical for multi-instance consolidation
    if let Some(socket) = &main_socket {
        cmd.env("KITTY_LISTEN_ON", socket);
        boundary!(
            "kitty",
            "spawn_session",
            socket = socket,
            session_id = session_id
        );
    }

    let _child = cmd.spawn().context("Failed to spawn kitty-claude")?;

    // Wait for pane to appear with retry.
    // kitty -1 (single-instance) returns immediately, pane spawns async.
    // Retry detection a few times with increasing delays
    for attempt in 1..=5 {
        let delay = Duration::from_millis(300 * attempt);
        sleep(delay).await;

        let panes = find_agent_panes().await?;

        for pane in &panes {
            // Skip panes that existed before spawn.
            if existing_ids.contains(&pane.id) {
                continue;
            }
            // Found a new pane - verify it's at the right cwd.
            if pane.cwd == cwd {
                let _ = set_user_var(pane.id, "babel_session_id", session_id).await;
                checkpoint!("pane_detected", pane_id = pane.id, session_id = session_id);
                return Ok(Some(pane.id));
            }
        }

        tracing::debug!(attempt, "Pane not detected yet, retrying...");
    }

    // If we can't find a new pane after retries, return None.
    tracing::warn!(
        session_id,
        existing_count = existing_ids.len(),
        "Spawned session but couldn't find new pane after 5 attempts"
    );
    Ok(None)
}

/// Load a WSet by closing all existing agent panes and spawning new ones.
///
/// Returns a list of session IDs that couldn't be restored (file missing, etc.)
pub async fn load_wset(wset: &WSet) -> Result<Vec<String>> {
    use tokio::time::{sleep, Duration};

    let mut skipped: Vec<String> = Vec::new();

    // Step 1: Close all existing agent panes
    effect!("wset", "close_panes", wset = wset.meta.name.as_str());
    for pane in find_agent_panes().await? {
        if let Err(e) = close_pane(pane.id).await {
            tracing::warn!(kitty_id = pane.id, error = %e, "Failed to close pane");
        }
    }

    // Brief pause to let panes close.
    sleep(Duration::from_millis(300)).await;

    // Step 2: Spawn windows for each wspace
    for wspace in &wset.wspaces {
        effect!(
            "wset",
            "spawn_workspace",
            workspace = wspace.index,
            windows = wspace.windows.len()
        );

        for window_config in &wspace.windows {
            match spawn_agent_session(&window_config.session_id, &window_config.cwd).await {
                Ok(Some(kitty_id)) => {
                    // Move to correct workspace and restore geometry
                    // Need to get platform_window_id first
                    if let Ok(Some(win)) = get_pane(kitty_id).await {
                        // Step 1: Move to correct workspace
                        if let Err(e) =
                            move_window_to_workspace(win.platform_window_id, wspace.index)
                        {
                            tracing::warn!(
                                kitty_id,
                                workspace = wspace.index,
                                error = %e,
                                "Failed to move window to workspace"
                            );
                        }

                        // Step 2: Restore precise geometry if available
                        // This enables multi-monitor restoration
                        if let Some(ref geom) = window_config.geometry {
                            // Brief pause for workspace move to complete
                            sleep(Duration::from_millis(50)).await;

                            if let Err(e) = set_window_geometry(win.platform_window_id, geom) {
                                tracing::warn!(
                                    kitty_id,
                                    x = geom.x, y = geom.y,
                                    w = geom.width, h = geom.height,
                                    error = %e,
                                    "Failed to restore window geometry"
                                );
                            } else {
                                tracing::debug!(
                                    kitty_id,
                                    x = geom.x, y = geom.y,
                                    w = geom.width, h = geom.height,
                                    monitor = ?geom.monitor,
                                    "Restored window geometry"
                                );
                            }
                        }
                    }
                }
                Ok(None) => {
                    // Session file doesn't exist or window not found
                    skipped.push(window_config.session_id.clone());
                }
                Err(e) => {
                    tracing::warn!(
                        session_id = %window_config.session_id,
                        error = %e,
                        "Failed to spawn session"
                    );
                    skipped.push(window_config.session_id.clone());
                }
            }

            // Small delay between spawns to avoid overwhelming the system
            sleep(Duration::from_millis(200)).await;
        }
    }

    checkpoint!(
        "wset_loaded",
        total_spawned = wset.window_count() - skipped.len(),
        failed = skipped.len()
    );

    Ok(skipped)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_extract_summary_from_title() {
        assert_eq!(
            extract_summary_from_title("✳ Pipeline Rewrite Assessment"),
            "Pipeline Rewrite Assessment"
        );
        assert_eq!(extract_summary_from_title("~/path: command"), "");
        assert_eq!(extract_summary_from_title("✳ Summary"), "Summary");
    }
}
