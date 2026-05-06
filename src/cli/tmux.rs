//! tmux integration commands
//!
//! `babel tmux-bridge` — Live push of @babel_* options into tmux.
//! `babel tmux-setup` — Print tmux.conf integration snippet.
//! `babel tmux-status` — One-shot status string (fallback/debug).
//! `babel tmux-pane` — One-shot per-pane info (fallback/debug).

use anyhow::{Context, Result};
use std::collections::HashMap;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;

use babel::indicator::IndicatorEvent;
use babel::ipc::{Request, Response};
use babel::paint::{PaintEvent, WorkspacePaintEvent};
use babel::utility::ipc::socket_path;

// =============================================================================
// tmux command helpers
// =============================================================================

/// Run a tmux command against the default server. Non-async (fire-and-forget).
fn tmux_cmd(args: &[&str]) -> std::io::Result<std::process::Output> {
    std::process::Command::new("tmux").args(args).output()
}

/// Batch multiple tmux commands into a single invocation.
/// Uses `\;` separator to chain commands.
fn tmux_batch(commands: &[Vec<String>]) {
    if commands.is_empty() {
        return;
    }

    let mut args: Vec<String> = Vec::new();
    for (i, cmd) in commands.iter().enumerate() {
        if i > 0 {
            args.push(r"\;".to_string());
        }
        args.extend(cmd.iter().cloned());
    }

    let str_args: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
    let _ = tmux_cmd(&str_args);
}

// =============================================================================
// Paint ID → tmux pane ID mapping
// =============================================================================

/// Maps babel paint IDs (e.g., "k5@12345") to tmux pane IDs (e.g., "%3").
///
/// Built by querying the daemon's tracked panes and correlating by PaneAddr.
/// Tmux panes have connection strings starting with "tmux:" and their id
/// field IS the tmux pane number (from %N).
struct PaneMap {
    /// paint_id → tmux pane target ("%N")
    map: HashMap<String, String>,
}

impl PaneMap {
    fn new() -> Self {
        Self {
            map: HashMap::new(),
        }
    }

    /// Refresh the mapping by querying the daemon for current panes.
    async fn refresh(&mut self) -> Result<()> {
        let sock = socket_path();
        let mut stream = UnixStream::connect(&sock)
            .await
            .context("Failed to connect to daemon for pane map refresh")?;

        let mut req = serde_json::to_string(&Request::List)?;
        req.push('\n');
        stream.write_all(req.as_bytes()).await?;

        let mut reader = BufReader::new(stream);
        let mut line = String::new();
        reader.read_line(&mut line).await?;

        let resp: Response = serde_json::from_str(&line)?;

        self.map.clear();
        if let Response::Windows { windows } = resp {
            for w in &windows {
                // Tmux panes have connection strings starting with "tmux:"
                if w.addr.socket.starts_with("tmux:") {
                    let paint_id = format!("k{}", w.addr.short());
                    let tmux_target = format!("%{}", w.addr.id);
                    self.map.insert(paint_id, tmux_target);
                }
            }
        }

        Ok(())
    }

    fn get(&self, paint_id: &str) -> Option<&str> {
        self.map.get(paint_id).map(|s| s.as_str())
    }
}

// =============================================================================
// Activity state formatting
// =============================================================================

fn state_indicator(color: &str, ring_intensity: f64, has_outline: bool) -> &'static str {
    if has_outline {
        "?"
    } else if ring_intensity > 0.5 {
        "●"
    } else if ring_intensity > 0.1 {
        "◐"
    } else if color != "#666666" {
        "◑"
    } else {
        "○"
    }
}

fn state_word(color: &str, ring_intensity: f64, has_outline: bool) -> &'static str {
    if has_outline {
        "awaiting"
    } else if ring_intensity > 0.5 {
        "working"
    } else if ring_intensity > 0.1 {
        "active"
    } else if color != "#666666" {
        "busy"
    } else {
        "idle"
    }
}

// =============================================================================
// babel tmux-bridge
// =============================================================================

/// Run the live tmux bridge.
///
/// Subscribes to the daemon's paint stream and pushes @babel_* tmux options.
/// Designed to be started via `run-shell -b "babel tmux-bridge"` in tmux.conf.
pub async fn cmd_tmux_bridge() -> Result<()> {
    eprintln!("babel tmux-bridge: connecting to daemon...");

    let sock = socket_path();
    let mut stream = UnixStream::connect(&sock)
        .await
        .context("Failed to connect to babel daemon. Is it running?")?;

    // Subscribe to paint stream
    let mut req = serde_json::to_string(&Request::SubscribePaint)?;
    req.push('\n');
    stream.write_all(req.as_bytes()).await?;

    let mut reader = BufReader::new(stream);

    // Read subscription ack
    let mut ack_line = String::new();
    reader.read_line(&mut ack_line).await?;
    let ack: Response =
        serde_json::from_str(&ack_line).context("Failed to parse subscription ack")?;

    match ack {
        Response::Subscribed { subscriber_id } => {
            eprintln!("babel tmux-bridge: subscribed (id: {subscriber_id})");
        }
        _ => {
            anyhow::bail!("Unexpected subscription response: {ack_line}");
        }
    }

    // Build initial pane map
    let mut pane_map = PaneMap::new();
    if let Err(e) = pane_map.refresh().await {
        eprintln!("babel tmux-bridge: pane map refresh failed: {e}");
    }

    // Batch accumulator + flush timer
    let mut pending_commands: Vec<Vec<String>> = Vec::new();
    let mut global_state = GlobalState::default();
    let mut last_pane_refresh = tokio::time::Instant::now();

    // Read paint events
    let mut line = String::new();
    loop {
        line.clear();

        // Use a timeout so we can flush batched commands even if events slow down
        let read_result =
            tokio::time::timeout(Duration::from_millis(100), reader.read_line(&mut line)).await;

        match read_result {
            Ok(Ok(0)) => {
                eprintln!("babel tmux-bridge: daemon disconnected");
                break;
            }
            Ok(Ok(_)) => {
                let resp: Response = match serde_json::from_str(&line) {
                    Ok(r) => r,
                    Err(_) => continue,
                };

                if let Response::PaintEvent { event } = resp {
                    process_paint_event(
                        &event,
                        &pane_map,
                        &mut pending_commands,
                        &mut global_state,
                    );
                }
            }
            Ok(Err(e)) => {
                eprintln!("babel tmux-bridge: read error: {e}");
                break;
            }
            Err(_) => {
                // Timeout — flush pending commands if any
            }
        }

        // Flush batched commands
        if !pending_commands.is_empty() {
            let commands = std::mem::take(&mut pending_commands);
            tmux_batch(&commands);
            // Force tmux to re-evaluate format strings
            let _ = tmux_cmd(&["refresh-client", "-S"]);
        }

        // Periodically refresh pane map (every 5s)
        if last_pane_refresh.elapsed() > Duration::from_secs(5) {
            if let Err(e) = pane_map.refresh().await {
                eprintln!("babel tmux-bridge: pane map refresh failed: {e}");
            }
            last_pane_refresh = tokio::time::Instant::now();
        }
    }

    Ok(())
}

/// Accumulated global state for status bar.
#[derive(Default)]
struct GlobalState {
    working: u32,
    awaiting: u32,
    tracked: u32,
}

impl GlobalState {
    fn status_string(&self) -> String {
        let dots: String = (0..self.tracked)
            .map(|i| if i < self.working { '●' } else { '○' })
            .collect();
        let mut parts = vec![dots];
        if self.working > 0 {
            parts.push(format!("{} working", self.working));
        }
        if self.awaiting > 0 {
            parts.push(format!("{} await", self.awaiting));
        }
        parts.push(format!("{} tracked", self.tracked));
        parts.join(" | ")
    }

    fn status_plain(&self) -> String {
        let mut parts = Vec::new();
        if self.working > 0 {
            parts.push(format!("{} working", self.working));
        }
        if self.awaiting > 0 {
            parts.push(format!("{} await", self.awaiting));
        }
        parts.push(format!("{} tracked", self.tracked));
        parts.join(" | ")
    }
}

fn process_paint_event(
    event: &PaintEvent,
    pane_map: &PaneMap,
    commands: &mut Vec<Vec<String>>,
    global: &mut GlobalState,
) {
    match event {
        PaintEvent::Window(IndicatorEvent::Set {
            id,
            color,
            ring_intensity,
            ring_color: _,
            has_outline,
            scale: _,
            workspace: _,
            x_pos: _,
        }) => {
            // Find the tmux pane for this paint ID
            if let Some(tmux_target) = pane_map.get(id) {
                let indicator = state_indicator(color, *ring_intensity, *has_outline);
                let state = state_word(color, *ring_intensity, *has_outline);

                // Per-pane options
                commands.push(vec![
                    "set-option".into(),
                    "-p".into(),
                    "-t".into(),
                    tmux_target.into(),
                    "@babel_state".into(),
                    state.into(),
                ]);
                commands.push(vec![
                    "set-option".into(),
                    "-p".into(),
                    "-t".into(),
                    tmux_target.into(),
                    "@babel_color".into(),
                    color.clone(),
                ]);

                // Formatted pane string — agent + indicator
                // We don't know the agent name from the paint event alone,
                // so use the state as the formatted line for now.
                // The pane map refresh can populate @babel_agent separately.
                let pane_str = format!("{indicator} {state}");
                commands.push(vec![
                    "set-option".into(),
                    "-p".into(),
                    "-t".into(),
                    tmux_target.into(),
                    "@babel_pane".into(),
                    pane_str,
                ]);
            }
        }
        PaintEvent::Window(IndicatorEvent::Remove { id }) => {
            if let Some(tmux_target) = pane_map.get(id) {
                // Clear per-pane options
                for opt in &[
                    "@babel_pane",
                    "@babel_agent",
                    "@babel_state",
                    "@babel_color",
                    "@babel_title",
                    "@babel_session",
                ] {
                    commands.push(vec![
                        "set-option".into(),
                        "-p".into(),
                        "-u".into(),
                        "-t".into(),
                        tmux_target.into(),
                        (*opt).into(),
                    ]);
                }
            }
        }
        PaintEvent::Window(IndicatorEvent::Clear) => {
            // Reset handled per-pane on Remove events
        }
        PaintEvent::Workspace(WorkspacePaintEvent::Set {
            workspace: _,
            css_class: _,
            is_urgent,
            awaiting_seconds: _,
            window_count,
            title: _,
        }) => {
            // Accumulate workspace stats into global state
            // Note: workspace events are per-workspace; we'd need to aggregate
            // across all workspaces for the global counts. For now, use the
            // most recent workspace event as a proxy — the daemon sends these
            // on every state change.
            if *is_urgent {
                global.awaiting = global.awaiting.max(1);
            }
            global.tracked = (*window_count) as u32;
        }
        PaintEvent::Workspace(WorkspacePaintEvent::Remove { .. })
        | PaintEvent::Workspace(WorkspacePaintEvent::Clear) => {}
        PaintEvent::Reset => {
            *global = GlobalState::default();
        }
    }

    // Update global status options on every event (cheap — just string formatting)
    commands.push(vec![
        "set-option".into(),
        "-g".into(),
        "@babel_status".into(),
        global.status_string(),
    ]);
    commands.push(vec![
        "set-option".into(),
        "-g".into(),
        "@babel_status_plain".into(),
        global.status_plain(),
    ]);
    commands.push(vec![
        "set-option".into(),
        "-g".into(),
        "@babel_working_count".into(),
        global.working.to_string(),
    ]);
    commands.push(vec![
        "set-option".into(),
        "-g".into(),
        "@babel_awaiting_count".into(),
        global.awaiting.to_string(),
    ]);
    commands.push(vec![
        "set-option".into(),
        "-g".into(),
        "@babel_tracked_count".into(),
        global.tracked.to_string(),
    ]);
}

// =============================================================================
// babel tmux-setup
// =============================================================================

pub fn cmd_tmux_setup() {
    println!(
        r#"# Babel tmux integration
# Add to your ~/.tmux.conf or source from your tmux plugin manager

# Per-pane header bar showing agent state (optional)
set -g pane-border-status top
set -g pane-border-format " #{{@babel_pane}} "

# Global status — add to your status-right (adjust placement to taste)
set -ga status-right " #{{@babel_status}} "

# Start live bridge in background
run-shell -b "babel tmux-bridge"

# Available tmux options (read via #{{@babel_*}} in format strings):
#
# Global:
#   @babel_status         — formatted: "●●○ 2 working | 1 await | 3 tracked"
#   @babel_status_plain   — no unicode: "2 working | 1 await | 3 tracked"
#   @babel_working_count  — raw number
#   @babel_awaiting_count — raw number
#   @babel_tracked_count  — raw number
#
# Per-pane:
#   @babel_pane    — formatted: "claude ● working"
#   @babel_agent   — harness name: "claude", "codex", "gemini"
#   @babel_state   — activity: "working", "idle", "awaiting", "active"
#   @babel_color   — hex accent
#   @babel_title   — session title
#   @babel_session — session ID
"#
    );
}

// =============================================================================
// babel tmux-status (one-shot fallback)
// =============================================================================

pub async fn cmd_tmux_status() -> Result<()> {
    let resp = babel::utility::ipc::send_request(&Request::List).await?;

    if let Response::Windows { windows } = resp {
        let total = windows.len();
        let working = windows
            .iter()
            .filter(|w| {
                matches!(
                    w.hook_state,
                    Some(
                        babel::babel_storage::HookState::Working
                            | babel::babel_storage::HookState::ToolRunning
                    )
                )
            })
            .count();
        let awaiting = windows
            .iter()
            .filter(|w| {
                w.activity_state
                    .as_ref()
                    .is_some_and(|a| matches!(a, scrollparse::claude::ActivityState::AwaitingInput))
            })
            .count();

        let dots: String = (0..total)
            .map(|i| if i < working { '●' } else { '○' })
            .collect();

        let mut parts = vec![dots];
        if working > 0 {
            parts.push(format!("{working} working"));
        }
        if awaiting > 0 {
            parts.push(format!("{awaiting} await"));
        }
        parts.push(format!("{total} tracked"));

        print!("{}", parts.join(" | "));
    }

    Ok(())
}

// =============================================================================
// babel tmux-pane (one-shot fallback)
// =============================================================================

pub async fn cmd_tmux_pane(pane_id: &str) -> Result<()> {
    use babel::model::PaneSelector;

    // Parse %N format
    let id: u64 = pane_id
        .strip_prefix('%')
        .unwrap_or(pane_id)
        .parse()
        .context("Invalid pane ID — expected %N or N")?;

    let resp = babel::utility::ipc::send_request(&Request::Status {
        target: Some(PaneSelector::Id(id)),
    })
    .await?;

    if let Response::Window { window } = resp {
        if let Some(w) = *window {
            let agent = w.agent_kind.slug();
            let state = w
                .activity_state
                .map(|a| format!("{a:?}").to_lowercase())
                .unwrap_or_else(|| "idle".into());
            print!("{agent} {state}");
            if !w.title.is_empty() {
                print!(" — {}", w.title);
            }
        }
    }

    Ok(())
}
