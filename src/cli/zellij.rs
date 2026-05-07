//! Zellij integration commands
//!
//! `babel zellij-bridge` — Subscribes to babel paint stream, pipes state to the
//! babel-zellij WASM plugin via `zellij pipe --name babel`.

use anyhow::{Context, Result};
use std::collections::BTreeMap;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;

use babel::indicator::IndicatorEvent;
use babel::ipc::{Request, Response};
use babel::paint::{PaintEvent, WorkspacePaintEvent};
use babel::utility::ipc::socket_path;
use serde::Serialize;

// =============================================================================
// State pushed to the zellij plugin
// =============================================================================

/// Matches the `BabelState` struct in the babel-zellij plugin.
#[derive(Debug, Default, Clone, Serialize)]
struct PluginState {
    status: String,
    working: u32,
    awaiting: u32,
    tracked: u32,
    panes: BTreeMap<String, PaneState>,
}

#[derive(Debug, Clone, Serialize)]
struct PaneState {
    agent: String,
    state: String,
    title: String,
    color: String,
    indicator: String,
}

// =============================================================================
// babel zellij-bridge
// =============================================================================

/// Run the live zellij bridge.
///
/// Subscribes to the daemon's paint stream and pipes state to the babel-zellij
/// WASM plugin via `zellij pipe --name babel -- <json>`.
pub async fn cmd_zellij_bridge() -> Result<()> {
    eprintln!("babel zellij-bridge: connecting to daemon...");

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
            eprintln!("babel zellij-bridge: subscribed (id: {subscriber_id})");
        }
        _ => {
            anyhow::bail!("Unexpected subscription response: {ack_line}");
        }
    }

    let mut state = PluginState::default();
    let mut dirty = false;

    // Read paint events
    let mut line = String::new();
    loop {
        line.clear();

        let read_result =
            tokio::time::timeout(Duration::from_millis(100), reader.read_line(&mut line)).await;

        match read_result {
            Ok(Ok(0)) => {
                eprintln!("babel zellij-bridge: daemon disconnected");
                break;
            }
            Ok(Ok(_)) => {
                let resp: Response = match serde_json::from_str(&line) {
                    Ok(r) => r,
                    Err(_) => continue,
                };

                if let Response::PaintEvent { event } = resp {
                    process_paint_event(&event, &mut state);
                    dirty = true;
                }
            }
            Ok(Err(e)) => {
                eprintln!("babel zellij-bridge: read error: {e}");
                break;
            }
            Err(_) => {
                // Timeout — flush if dirty
            }
        }

        // Pipe state to zellij plugin
        if dirty {
            dirty = false;
            let json = serde_json::to_string(&state).unwrap_or_default();
            let _ = std::process::Command::new("zellij")
                .args(["pipe", "--name", "babel", "--", &json])
                .output();
        }
    }

    Ok(())
}

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

fn process_paint_event(event: &PaintEvent, state: &mut PluginState) {
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

            state.panes.insert(
                id.clone(),
                PaneState {
                    agent: String::new(), // enriched by pane map later
                    state: word,
                    title: String::new(),
                    color: color.clone(),
                    indicator,
                },
            );
        }
        PaintEvent::Window(IndicatorEvent::Remove { id }) => {
            state.panes.remove(id);
        }
        PaintEvent::Window(IndicatorEvent::Clear) => {
            state.panes.clear();
        }
        PaintEvent::Workspace(WorkspacePaintEvent::Set {
            is_urgent,
            window_count,
            ..
        }) => {
            state.tracked = *window_count as u32;
            if *is_urgent {
                state.awaiting = state.awaiting.max(1);
            }
        }
        PaintEvent::Workspace(WorkspacePaintEvent::Remove { .. })
        | PaintEvent::Workspace(WorkspacePaintEvent::Clear) => {}
        PaintEvent::Reset => {
            *state = PluginState::default();
        }
    }

    // Recompute working count from pane states
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

    // Build formatted status
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
}
