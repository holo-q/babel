//! Paint stream — UX-level commands emitted to puppet clients.
//!
//! Babel is authoritative over color, ring intensity, scale, outline,
//! workspace aggregate state. Clients (richmon-babel, richspace-babel)
//! subscribe via Request::SubscribePaint and forward verbatim to their
//! respective renderers — they hold no UX policy.
//!
//! ## Why a separate stream?
//!
//! BabelEvent stays for diagnostic tools (`babel watch`, treasure-panel,
//! future LLM-context introspection). PaintEvent is the renderer contract:
//! stable, ready-to-paint, no semantic decoding required by consumers.
//! When upstream bugs (e.g. claude-code #15897 blocking PreToolUse) change
//! how state arrives internally, the paint stream stays bit-identical to
//! consumers — that's the contract.
//!
//! ## Stream shape
//!
//! Two strands, parallel and idempotent:
//! - Window paint: per-pane dot — color, ring, scale, outline, workspace, x_pos.
//!   Reuses `IndicatorEvent` (the pre-existing per-window paint format) so
//!   richmon-babel forwards it byte-for-byte to its panel widget.
//! - Workspace paint: per-workspace aggregate — CSS class, urgent flag,
//!   awaiting timer, window count, optional title. Richspace-babel writes
//!   these into the panel widget's state.json.
//!
//! Each subscription replays the full current image on connect, so panel
//! restarts reconverge without waiting for the next event.

use scrollparse::claude::ActivityState;
use serde::{Deserialize, Serialize};

use crate::babel_storage::HookState;
use crate::indicator::IndicatorEvent;
use crate::AgentKind;

/// One inactive dot color for all agents and all "not actively working"
/// discovery states. Unknown/no-hook-data should not become an accidental
/// agent-kind shade in the panel; it is merely the same quiet baseline until
/// babel has a stronger visual command to send.
const INACTIVE_DOT_COLOR: &str = "#666666";

/// Per-workspace aggregate paint state. Babel computes this from the
/// per-pane state map; richspace-babel forwards verbatim into its
/// state.json so the richspace panel widget can paint the workspace
/// button (CSS class for color, urgent for attention badge, label for
/// the count, tooltip with awaiting timer).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type")]
pub enum WorkspacePaintEvent {
    Set {
        workspace: u32,
        /// CSS class for the panel widget (claude-idle, claude-busy,
        /// claude-busy-all, claude-await, or None for clear).
        css_class: Option<String>,
        /// True if at least one window is in AwaitingInput.
        is_urgent: bool,
        /// Seconds since the workspace entered AwaitingInput, if any.
        awaiting_seconds: Option<u64>,
        /// Number of tracked panes on this workspace.
        window_count: usize,
        /// Optional human-readable title for the workspace.
        title: Option<String>,
    },
    Remove {
        workspace: u32,
    },
    Clear,
}

/// The puppet-stream event. Subscribers receive these via SubscribePaint.
///
/// Window and Workspace strands ride the same stream so a single subscriber
/// connection covers both clients' needs (richmon ignores Workspace,
/// richspace ignores Window). Reset signals daemon-level reset — clients
/// drop their cache and replay incoming Set events.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "kind")]
pub enum PaintEvent {
    Window(IndicatorEvent),
    Workspace(WorkspacePaintEvent),
    /// Reset signal — daemon restart, full state replay incoming.
    Reset,
}

impl PaintEvent {
    pub fn to_json(&self) -> String {
        serde_json::to_string(self).unwrap_or_default()
    }
}

/// Compute the indicator color from agent kind + hook state + activity state.
///
/// MOVED FROM richmon-babel — babel is now authoritative over UX.
/// Active/working color comes from the harness accent. Idle/Awaiting/Plan
/// remain universal state colors unless a future paint field carries a durable
/// unread ring color separately. Returns the hex color including '#'.
pub fn resolve_color(
    agent_kind: AgentKind,
    hook_state: Option<HookState>,
    activity_state: Option<ActivityState>,
) -> &'static str {
    let working_hue = agent_kind.accent_color();
    let tool_hue = agent_kind.accent_color();
    match (hook_state, activity_state) {
        // Hook says Idle → trust it absolutely
        (Some(HookState::Idle), _) => INACTIVE_DOT_COLOR,
        // Hook says ToolRunning → most precise state
        (Some(HookState::ToolRunning), _) => tool_hue,
        // Hook says Working → use activity_state for granularity
        (Some(HookState::Working), Some(ActivityState::Thinking)) => working_hue,
        (Some(HookState::Working), Some(ActivityState::ToolUse)) => tool_hue,
        (Some(HookState::Working), Some(ActivityState::PlanApproval)) => "#c080f0",
        (Some(HookState::Working), Some(ActivityState::BackgroundTask)) => "#40f0c0",
        (Some(HookState::Working), _) => working_hue,
        // No hook state → fall back to activity_state
        (None, Some(ActivityState::Idle)) => INACTIVE_DOT_COLOR,
        (None, Some(ActivityState::Thinking)) => working_hue,
        (None, Some(ActivityState::ToolUse)) => tool_hue,
        (None, Some(ActivityState::PlanApproval)) => "#c080f0",
        (None, Some(ActivityState::BackgroundTask)) => "#40f0c0",
        (None, Some(ActivityState::AwaitingInput)) => "#f04080",
        (None, Some(ActivityState::Unknown)) | (None, None) => INACTIVE_DOT_COLOR,
    }
}

/// Compute the workspace CSS class from a slice of pane activity states.
/// Returns None when the workspace has no tracked panes (Empty).
///
/// MOVED FROM richspace-babel/aggregator.rs — babel is now authoritative.
pub fn workspace_css_class(states: &[ActivityState]) -> Option<&'static str> {
    if states.is_empty() {
        return None;
    }
    let has_awaiting = states.iter().any(|s| *s == ActivityState::AwaitingInput);
    let is_busy = |s: &ActivityState| matches!(s, ActivityState::Thinking | ActivityState::ToolUse);
    let all_busy = states.iter().all(is_busy);
    let any_busy = states.iter().any(is_busy);
    Some(if has_awaiting {
        "claude-await"
    } else if all_busy {
        "claude-busy-all"
    } else if any_busy {
        "claude-busy"
    } else {
        "claude-idle"
    })
}

/// True if any state in the slice indicates a pane awaiting user input.
/// Convenience for callers building WorkspacePaintEvent::Set { is_urgent }.
pub fn workspace_is_urgent(states: &[ActivityState]) -> bool {
    states.iter().any(|s| *s == ActivityState::AwaitingInput)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inactive_dot_color_is_homogeneous_across_agents_and_discovery_quality() {
        let cases = [
            (AgentKind::Claude, Some(HookState::Idle), None),
            (AgentKind::Codex, Some(HookState::Idle), None),
            (AgentKind::Claude, None, Some(ActivityState::Idle)),
            (AgentKind::Codex, None, Some(ActivityState::Idle)),
            (AgentKind::Claude, None, Some(ActivityState::Unknown)),
            (AgentKind::Codex, None, Some(ActivityState::Unknown)),
            (AgentKind::Claude, None, None),
            (AgentKind::Codex, None, None),
        ];

        for (agent_kind, hook_state, activity_state) in cases {
            assert_eq!(
                resolve_color(agent_kind, hook_state, activity_state),
                INACTIVE_DOT_COLOR
            );
        }
    }
}
