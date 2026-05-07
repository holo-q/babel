//! Unified bridge — subscribes to babel paint stream, delivers state to sinks.
//!
//! The bridge is the shared data pipeline between the babel daemon and any
//! visual surface (tmux options, zellij pipe, JSON stdout). Delivery adapters
//! implement [`BridgeSink`] to handle the last-mile push.
//!
//! ```text
//! babel daemon ──paint──→ bridge core ──→ TmuxSink / ZellijSink / JsonSink
//! ```

use anyhow::{Context, Result};
use serde::Serialize;
use std::collections::BTreeMap;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;

use babel::indicator::IndicatorEvent;
use babel::ipc::{Request, Response};
use babel::paint::{PaintEvent, WorkspacePaintEvent};
use babel::utility::ipc::socket_path;

// =============================================================================
// Bridge state — the structured snapshot pushed to sinks
// =============================================================================

#[derive(Debug, Default, Clone, Serialize)]
pub struct BridgeState {
    pub status: String,
    pub status_plain: String,
    pub working: u32,
    pub awaiting: u32,
    pub tracked: u32,
    pub panes: BTreeMap<String, BridgePaneState>,
}

#[derive(Debug, Clone, Serialize)]
pub struct BridgePaneState {
    pub agent: String,
    pub state: String,
    pub title: String,
    pub session_id: String,
    pub color: String,
    pub indicator: String,
    pub formatted: String,
}

// =============================================================================
// Sink trait — how state gets delivered to the target
// =============================================================================

pub trait BridgeSink {
    fn deliver(&mut self, state: &BridgeState) -> Result<()>;
    fn name(&self) -> &'static str;
}

// =============================================================================
// Pane metadata cache
// =============================================================================

pub struct PaneMeta {
    pub agent: String,
    pub title: String,
    pub session_id: String,
    /// Backend-specific pane target (e.g., "%5" for tmux)
    pub target: Option<String>,
}

pub struct PaneMetaCache {
    map: std::collections::HashMap<String, PaneMeta>,
}

impl PaneMetaCache {
    pub fn new() -> Self {
        Self {
            map: std::collections::HashMap::new(),
        }
    }

    pub async fn refresh(&mut self) -> Result<()> {
        let sock = socket_path();
        let mut stream = UnixStream::connect(&sock)
            .await
            .context("daemon connect failed for meta refresh")?;

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
                let paint_id = format!("k{}", w.addr.short());
                let target = if w.addr.socket.starts_with("tmux:") {
                    Some(format!("%{}", w.addr.id))
                } else if w.addr.socket.starts_with("zellij:") {
                    Some(w.addr.id.to_string())
                } else {
                    None
                };
                self.map.insert(
                    paint_id,
                    PaneMeta {
                        agent: w.agent_kind.slug().to_string(),
                        title: w.title.clone(),
                        session_id: w.session_id.clone().unwrap_or_default(),
                        target,
                    },
                );
            }
        }

        Ok(())
    }

    pub fn get(&self, paint_id: &str) -> Option<&PaneMeta> {
        self.map.get(paint_id)
    }
}

// =============================================================================
// Activity formatting
// =============================================================================

pub fn state_indicator(color: &str, ring_intensity: f64, has_outline: bool) -> &'static str {
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

pub fn state_word(color: &str, ring_intensity: f64, has_outline: bool) -> &'static str {
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
// Paint event processing
// =============================================================================

pub fn process_paint_event(
    event: &PaintEvent,
    meta: &PaneMetaCache,
    state: &mut BridgeState,
) {
    match event {
        PaintEvent::Window(IndicatorEvent::Set {
            id,
            color,
            ring_intensity,
            has_outline,
            ..
        }) => {
            let indicator = state_indicator(color, *ring_intensity, *has_outline).to_string();
            let word = state_word(color, *ring_intensity, *has_outline).to_string();

            let (agent, title, session_id) = if let Some(m) = meta.get(id) {
                (m.agent.clone(), m.title.clone(), m.session_id.clone())
            } else {
                (String::new(), String::new(), String::new())
            };

            let formatted = if title.is_empty() {
                format!("{agent} {indicator} {word}")
            } else {
                format!("{agent} {indicator} {word} — {title}")
            };

            state.panes.insert(
                id.clone(),
                BridgePaneState {
                    agent,
                    state: word,
                    title,
                    session_id,
                    color: color.clone(),
                    indicator,
                    formatted,
                },
            );
        }
        PaintEvent::Window(IndicatorEvent::Remove { id }) => {
            state.panes.remove(id);
        }
        PaintEvent::Window(IndicatorEvent::Clear) => {
            state.panes.clear();
        }
        PaintEvent::Workspace(WorkspacePaintEvent::Set { .. }) => {
            // Workspace events don't carry enough to reconstruct global counts —
            // we recompute from pane states below.
        }
        PaintEvent::Workspace(WorkspacePaintEvent::Remove { .. })
        | PaintEvent::Workspace(WorkspacePaintEvent::Clear) => {}
        PaintEvent::Reset => {
            *state = BridgeState::default();
            return;
        }
    }

    // Recompute global counts from pane states
    state.working = state
        .panes
        .values()
        .filter(|p| p.state == "working" || p.state == "active")
        .count() as u32;
    state.awaiting = state
        .panes
        .values()
        .filter(|p| p.state == "awaiting")
        .count() as u32;
    state.tracked = state.panes.len() as u32;

    // Build formatted status strings
    let dots: String = (0..state.tracked)
        .map(|i| if i < state.working { '●' } else { '○' })
        .collect();

    let mut parts = vec![dots];
    if state.working > 0 {
        parts.push(format!("{} working", state.working));
    }
    if state.awaiting > 0 {
        parts.push(format!("{} await", state.awaiting));
    }
    parts.push(format!("{} tracked", state.tracked));
    state.status = parts.join(" | ");

    let mut plain = Vec::new();
    if state.working > 0 {
        plain.push(format!("{} working", state.working));
    }
    if state.awaiting > 0 {
        plain.push(format!("{} await", state.awaiting));
    }
    plain.push(format!("{} tracked", state.tracked));
    state.status_plain = plain.join(" | ");
}

// =============================================================================
// Bridge event loop
// =============================================================================

/// Run the bridge: subscribe to paint stream, process events, deliver to sink.
pub async fn run_bridge(mut sink: Box<dyn BridgeSink>) -> Result<()> {
    eprintln!("babel bridge ({}): connecting to daemon...", sink.name());

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
            eprintln!(
                "babel bridge ({}): subscribed (id: {subscriber_id})",
                sink.name()
            );
        }
        _ => {
            anyhow::bail!("Unexpected subscription response: {ack_line}");
        }
    }

    let mut state = BridgeState::default();
    let mut meta = PaneMetaCache::new();
    if let Err(e) = meta.refresh().await {
        eprintln!("babel bridge: meta refresh failed: {e}");
    }
    let mut last_meta_refresh = tokio::time::Instant::now();
    let mut dirty = false;

    let mut line = String::new();
    loop {
        line.clear();

        let read_result =
            tokio::time::timeout(Duration::from_millis(100), reader.read_line(&mut line)).await;

        match read_result {
            Ok(Ok(0)) => {
                eprintln!("babel bridge ({}): daemon disconnected", sink.name());
                break;
            }
            Ok(Ok(_)) => {
                let resp: Response = match serde_json::from_str(&line) {
                    Ok(r) => r,
                    Err(_) => continue,
                };
                if let Response::PaintEvent { event } = resp {
                    process_paint_event(&event, &meta, &mut state);
                    dirty = true;
                }
            }
            Ok(Err(e)) => {
                eprintln!("babel bridge ({}): read error: {e}", sink.name());
                break;
            }
            Err(_) => {} // timeout — flush below
        }

        if dirty {
            dirty = false;
            if let Err(e) = sink.deliver(&state) {
                eprintln!("babel bridge ({}): deliver failed: {e}", sink.name());
            }
        }

        // Refresh pane metadata every 5s
        if last_meta_refresh.elapsed() > Duration::from_secs(5) {
            if let Err(e) = meta.refresh().await {
                eprintln!("babel bridge: meta refresh failed: {e}");
            }
            last_meta_refresh = tokio::time::Instant::now();
        }
    }

    Ok(())
}

// =============================================================================
// Built-in sinks
// =============================================================================

/// JSON to stdout — for debugging or piping to external tools.
pub struct JsonSink;

impl BridgeSink for JsonSink {
    fn deliver(&mut self, state: &BridgeState) -> Result<()> {
        let json = serde_json::to_string(state)?;
        println!("{json}");
        Ok(())
    }

    fn name(&self) -> &'static str {
        "json"
    }
}

// =============================================================================
// CLI entry point
// =============================================================================

pub async fn cmd_bridge(target: Option<&str>) -> Result<()> {
    let sink: Box<dyn BridgeSink> = match target {
        Some("tmux") => Box::new(super::tmux::TmuxSink::new()),
        Some("zellij") => Box::new(super::zellij::ZellijSink),
        Some("json") => Box::new(JsonSink),
        None => {
            // Auto-detect
            if std::env::var("TMUX").is_ok() {
                Box::new(super::tmux::TmuxSink::new())
            } else if std::env::var("ZELLIJ").is_ok() {
                Box::new(super::zellij::ZellijSink)
            } else {
                Box::new(JsonSink)
            }
        }
        Some(other) => anyhow::bail!("Unknown bridge target: {other}. Use tmux, zellij, or json."),
    };

    run_bridge(sink).await
}
