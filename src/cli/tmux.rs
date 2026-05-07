//! tmux integration — sink adapter + setup/fallback commands
//!
//! The bridge core lives in `cli/bridge.rs`. This module provides:
//! - `TmuxSink` — delivers BridgeState as tmux set-option commands
//! - `cmd_tmux_setup` — prints tmux.conf integration snippet
//! - `cmd_tmux_status` / `cmd_tmux_pane` — one-shot fallback/debug

use anyhow::{Context, Result};

use babel::ipc::{Request, Response};
use super::bridge::{BridgePaneState, BridgeSink, BridgeState, PaneMetaCache};

// =============================================================================
// tmux command helpers
// =============================================================================

fn tmux_cmd(args: &[&str]) -> std::io::Result<std::process::Output> {
    std::process::Command::new("tmux").args(args).output()
}

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
// TmuxSink — delivers BridgeState into tmux user options
// =============================================================================

pub struct TmuxSink {
    meta: PaneMetaCache,
}

impl TmuxSink {
    pub fn new() -> Self {
        Self {
            meta: PaneMetaCache::new(),
        }
    }
}

impl BridgeSink for TmuxSink {
    fn deliver(&mut self, state: &BridgeState) -> Result<()> {
        let mut commands: Vec<Vec<String>> = Vec::new();

        // Per-pane options
        for (paint_id, pane) in &state.panes {
            let Some(target) = self.meta.get(paint_id).and_then(|m| m.target.clone()) else {
                continue;
            };

            let set = |key: &str, val: &str| -> Vec<String> {
                vec![
                    "set-option".into(), "-p".into(), "-t".into(),
                    target.clone(), key.into(), val.into(),
                ]
            };

            commands.push(set("@babel_state", &pane.state));
            commands.push(set("@babel_color", &pane.color));
            commands.push(set("@babel_agent", &pane.agent));
            commands.push(set("@babel_title", &pane.title));
            commands.push(set("@babel_session", &pane.session_id));
            commands.push(set("@babel_pane", &pane.formatted));
        }

        // Global options
        let set_g = |key: &str, val: &str| -> Vec<String> {
            vec!["set-option".into(), "-g".into(), key.into(), val.into()]
        };

        commands.push(set_g("@babel_status", &state.status));
        commands.push(set_g("@babel_status_plain", &state.status_plain));
        commands.push(set_g("@babel_working_count", &state.working.to_string()));
        commands.push(set_g("@babel_awaiting_count", &state.awaiting.to_string()));
        commands.push(set_g("@babel_tracked_count", &state.tracked.to_string()));

        // Flush
        tmux_batch(&commands);
        let _ = tmux_cmd(&["refresh-client", "-S"]);

        Ok(())
    }

    fn name(&self) -> &'static str {
        "tmux"
    }
}

// =============================================================================
// babel tmux-bridge (alias for babel bridge --tmux)
// =============================================================================

pub async fn cmd_tmux_bridge() -> Result<()> {
    super::bridge::run_bridge(Box::new(TmuxSink::new())).await
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
run-shell -b "babel bridge --tmux"

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
#   @babel_pane    — formatted: "claude ● working — babel:refactor"
#   @babel_agent   — harness name: "claude", "codex", "gemini"
#   @babel_state   — activity: "working", "idle", "awaiting", "active"
#   @babel_color   — hex accent
#   @babel_title   — session title
#   @babel_session — session ID
"#
    );
}

// =============================================================================
// One-shot fallback commands
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

pub async fn cmd_tmux_pane(pane_id: &str) -> Result<()> {
    use babel::model::PaneSelector;

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
