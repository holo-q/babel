//! Structured logging helpers for Spaceship daemons
//!
//! Provides a standardized format for logging events with full context inline,
//! because journald doesn't render tracing's structured fields by default.
//!
//! ## Format
//!
//! ```text
//! Pane(k42)::Focused
//! Pane(k42)::StateChanged { Idle → Thinking }
//! Pane(k42)::Pulse { 0.75, token_output }
//! Window(k42)::Added { ws:3, "claude - project" }
//! Session::Matched { k42 ↔ "uuid-short", high }
//! SFX::Failed { complete, "socket error" }
//! ```
//!
//! The pattern is: `Entity(id)::Event { context }` — inspired by Rust's
//! Debug formatting but optimized for log scanning.

use crate::events::{BabelEvent, PulseTrigger};

/// Format a BabelEvent for logging with full context
///
/// Returns a human-readable string suitable for tracing info/debug messages.
/// All relevant context is inlined so journald captures it.
pub fn format_event(event: &BabelEvent) -> String {
    match event {
        // ─── Pane Events ────────────────────────────────────────────────────────
        BabelEvent::PaneFocused {
            kitty_id,
            session_id,
        } => match session_id {
            Some(sid) => format!("Pane(k{})::Focused {{ {} }}", kitty_id, short_uuid(sid)),
            None => format!("Pane(k{})::Focused", kitty_id),
        },

        BabelEvent::PaneUnfocused {
            kitty_id,
            session_id,
        } => match session_id {
            Some(sid) => format!("Pane(k{})::Unfocused {{ {} }}", kitty_id, short_uuid(sid)),
            None => format!("Pane(k{})::Unfocused", kitty_id),
        },

        BabelEvent::SessionStateChanged {
            kitty_id,
            workspace,
            old_state,
            new_state,
            asking_question,
            ..
        } => {
            let ws = workspace.map_or(String::new(), |w| format!(" ws:{}", w));
            let ask = if *asking_question { " ?" } else { "" };
            format!(
                "Pane(k{})::State {{ {:?} → {:?}{}{} }}",
                kitty_id, old_state, new_state, ws, ask
            )
        }

        BabelEvent::ActivityPulse {
            kitty_id,
            workspace,
            intensity,
            trigger,
            ..
        } => {
            let ws = workspace.map_or(String::new(), |w| format!(" ws:{}", w));
            format!(
                "Pane(k{})::Pulse {{ {:.2}, {}{} }}",
                kitty_id,
                intensity,
                format_trigger(trigger),
                ws
            )
        }

        // ─── Window Events ──────────────────────────────────────────────────────
        BabelEvent::WindowAdded {
            kitty_id,
            title,
            workspace,
            agent_kind,
        } => {
            let ws = workspace.map_or(String::new(), |w| format!("ws:{} ", w));
            let short_title = truncate_title(title, 40);
            // Render agent only when non-default to keep Claude logs unchanged.
            let agent_tag = if *agent_kind == crate::AgentKind::default() {
                String::new()
            } else {
                format!("{} ", agent_kind)
            };
            format!(
                "Window(k{})::Added {{ {}{}{} }}",
                kitty_id, agent_tag, ws, short_title
            )
        }

        BabelEvent::WindowRemoved { kitty_id } => {
            format!("Window(k{})::Removed", kitty_id)
        }

        BabelEvent::WindowWorkspaceChanged {
            kitty_id,
            old_workspace,
            new_workspace,
        } => {
            let old = old_workspace.map_or("?".to_string(), |w| w.to_string());
            let new = new_workspace.map_or("?".to_string(), |w| w.to_string());
            format!("Window(k{})::Moved {{ {} → {} }}", kitty_id, old, new)
        }

        // ─── Terminal Events ────────────────────────────────────────────────────
        BabelEvent::TerminalOpened {
            kitty_id,
            title,
            workspace,
            ..
        } => {
            let ws = workspace.map_or(String::new(), |w| format!("ws:{} ", w));
            let short_title = truncate_title(title, 30);
            format!(
                "Terminal(k{})::Opened {{ {}{} }}",
                kitty_id, ws, short_title
            )
        }

        BabelEvent::TerminalClosed { kitty_id } => {
            format!("Terminal(k{})::Closed", kitty_id)
        }

        BabelEvent::TerminalBecameAgent { kitty_id, title } => {
            let short_title = truncate_title(title, 30);
            format!("Terminal(k{})::BecameAgent {{ {} }}", kitty_id, short_title)
        }

        // ─── Session Events ─────────────────────────────────────────────────────
        BabelEvent::SessionMatched {
            kitty_id,
            session_id,
            confidence,
        } => {
            format!(
                "Session::Matched {{ k{} ↔ {}, {} }}",
                kitty_id,
                short_uuid(session_id),
                confidence
            )
        }

        BabelEvent::SessionUpdated {
            session_id,
            project,
        } => {
            let short_proj = truncate_path(project, 30);
            format!(
                "Session::Updated {{ {}, {} }}",
                short_uuid(session_id),
                short_proj
            )
        }

        // ─── Workspace Events ───────────────────────────────────────────────────
        BabelEvent::WorkspaceTitleUpdated {
            workspace,
            title,
            window_count,
            ..
        } => {
            format!(
                "Workspace({})::Title {{ \"{}\" ({} win) }}",
                workspace, title, window_count
            )
        }

        // ─── Title Policy Events ────────────────────────────────────────────────
        BabelEvent::TitleGenerated {
            session_id,
            title,
            prompt_count,
            policy,
        } => {
            format!(
                "Title::Generated {{ {}, \"{}\" ({} prompts, {}) }}",
                short_uuid(session_id),
                title,
                prompt_count,
                policy
            )
        }

        BabelEvent::TitleSpliced {
            session_id, title, ..
        } => {
            format!(
                "Title::Spliced {{ {}, \"{}\" }}",
                short_uuid(session_id),
                title
            )
        }

        // ─── WSet Events ────────────────────────────────────────────────────────
        BabelEvent::WSetSaved {
            name,
            wspaces,
            windows,
        } => {
            format!(
                "WSet::Saved {{ \"{}\", {}ws, {}win }}",
                name, wspaces, windows
            )
        }

        BabelEvent::WSetLoaded {
            name,
            wspaces,
            windows,
            skipped,
        } => {
            if *skipped > 0 {
                format!(
                    "WSet::Loaded {{ \"{}\", {}ws, {}win, {} skipped }}",
                    name, wspaces, windows, skipped
                )
            } else {
                format!(
                    "WSet::Loaded {{ \"{}\", {}ws, {}win }}",
                    name, wspaces, windows
                )
            }
        }

        BabelEvent::WSetSwitched { from, to } => match from {
            Some(f) => format!("WSet::Switched {{ \"{}\" → \"{}\" }}", f, to),
            None => format!("WSet::Switched {{ → \"{}\" }}", to),
        },

        // ─── Hook Events (from Claude Code neural interface) ────────────────────
        BabelEvent::ToolStarted {
            session_id,
            kitty_id,
            tool_name,
        } => {
            let kit = kitty_id.map_or(String::new(), |k| format!(" k{}", k));
            format!(
                "Hook::ToolStarted {{ {}, {}{} }}",
                short_uuid(session_id),
                tool_name,
                kit
            )
        }

        BabelEvent::ToolCompleted {
            session_id,
            kitty_id,
            tool_name,
        } => {
            let kit = kitty_id.map_or(String::new(), |k| format!(" k{}", k));
            format!(
                "Hook::ToolCompleted {{ {}, {}{} }}",
                short_uuid(session_id),
                tool_name,
                kit
            )
        }

        BabelEvent::NotificationReceived {
            session_id,
            kitty_id,
            notif_type,
            message,
        } => {
            let kit = kitty_id.map_or(String::new(), |k| format!(" k{}", k));
            let msg = message
                .as_ref()
                .map_or(String::new(), |m| format!(" \"{}\"", truncate_title(m, 30)));
            format!(
                "Hook::Notification {{ {}, {}{}{} }}",
                short_uuid(session_id),
                notif_type,
                msg,
                kit
            )
        }

        BabelEvent::SessionStarted {
            session_id,
            kitty_id,
            cwd,
            resumed,
        } => {
            let kit = kitty_id.map_or(String::new(), |k| format!(" k{}", k));
            let res = if *resumed { " resumed" } else { "" };
            format!(
                "Hook::SessionStarted {{ {}, {}{}{} }}",
                short_uuid(session_id),
                cwd,
                res,
                kit
            )
        }

        BabelEvent::SubagentCompleted {
            session_id,
            kitty_id,
            subagent_id,
        } => {
            let kit = kitty_id.map_or(String::new(), |k| format!(" k{}", k));
            format!(
                "Hook::SubagentCompleted {{ {}, sub:{}{} }}",
                short_uuid(session_id),
                short_uuid(subagent_id),
                kit
            )
        }

        BabelEvent::TranscriptCompacting {
            session_id,
            kitty_id,
            transcript_path,
        } => {
            let kit = kitty_id.map_or(String::new(), |k| format!(" k{}", k));
            format!(
                "Hook::TranscriptCompacting {{ {}, {}{} }}",
                short_uuid(session_id),
                transcript_path,
                kit
            )
        }

        // ─── Daemon Events ──────────────────────────────────────────────────────
        BabelEvent::DaemonShutdown => "Daemon::Shutdown".to_string(),
    }
}

/// Format a PulseTrigger for log output
fn format_trigger(trigger: &PulseTrigger) -> &'static str {
    match trigger {
        PulseTrigger::TokenOutput => "token",
        PulseTrigger::ToolStart => "tool_start",
        PulseTrigger::ToolComplete => "tool_done",
        PulseTrigger::UserInput => "input",
        PulseTrigger::StateTransition => "state",
        PulseTrigger::HookLifecycle => "hook",
    }
}

/// Shorten a UUID to first 8 chars for readability
fn short_uuid(uuid: &str) -> &str {
    if uuid.len() > 8 {
        &uuid[..8]
    } else {
        uuid
    }
}

/// Truncate a title, adding ellipsis if needed
fn truncate_title(title: &str, max_len: usize) -> String {
    if title.len() <= max_len {
        format!("\"{}\"", title)
    } else {
        format!("\"{}…\"", &title[..max_len - 1])
    }
}

/// Truncate a path from the left, showing the end
fn truncate_path(path: &str, max_len: usize) -> String {
    if path.len() <= max_len {
        path.to_string()
    } else {
        format!("…{}", &path[path.len() - max_len + 1..])
    }
}

// ═══════════════════════════════════════════════════════════════════════════════
// Logging macros for common patterns
// ═══════════════════════════════════════════════════════════════════════════════

/// Log a babel event at INFO level with full context
#[macro_export]
macro_rules! log_event {
    ($event:expr) => {
        tracing::info!("{}", $crate::logging::format_event($event))
    };
}

/// Log a babel event at DEBUG level with full context
#[macro_export]
macro_rules! log_event_debug {
    ($event:expr) => {
        tracing::debug!("{}", $crate::logging::format_event($event))
    };
}

/// Log a babel event at TRACE level with full context
#[macro_export]
macro_rules! log_event_trace {
    ($event:expr) => {
        tracing::trace!("{}", $crate::logging::format_event($event))
    };
}

// ═══════════════════════════════════════════════════════════════════════════════
// SFX/Error Logging Helpers
// ═══════════════════════════════════════════════════════════════════════════════

/// Format an SFX trigger failure
pub fn format_sfx_failed(event: &str, error: &str) -> String {
    format!("SFX::Failed {{ {}, \"{}\" }}", event, error)
}

/// Format an SFX trigger success
pub fn format_sfx_triggered(event: &str) -> String {
    format!("SFX::Triggered {{ {} }}", event)
}

/// Format an IPC error
pub fn format_ipc_error(operation: &str, error: &str) -> String {
    format!("IPC::Error {{ {}, \"{}\" }}", operation, error)
}

/// Format a connection event
pub fn format_connected(target: &str, path: &str) -> String {
    format!("{}::Connected {{ {} }}", target, truncate_path(path, 50))
}

/// Format a disconnection event
pub fn format_disconnected(target: &str, reason: &str) -> String {
    format!("{}::Disconnected {{ \"{}\" }}", target, reason)
}

// ═══════════════════════════════════════════════════════════════════════════════
// Tests
// ═══════════════════════════════════════════════════════════════════════════════

#[cfg(test)]
mod tests {
    use super::*;
    use scrollparse::claude::ActivityState;

    #[test]
    fn test_pane_focused() {
        let event = BabelEvent::PaneFocused {
            kitty_id: 42,
            session_id: Some("a1b2c3d4-e5f6-7890-abcd-ef1234567890".to_string()),
        };
        let formatted = format_event(&event);
        assert_eq!(formatted, "Pane(k42)::Focused { a1b2c3d4 }");
    }

    #[test]
    fn test_state_changed() {
        let event = BabelEvent::SessionStateChanged {
            kitty_id: 42,
            session_id: None,
            workspace: Some(3),
            old_state: ActivityState::Idle,
            new_state: ActivityState::Thinking,
            asking_question: true,
            agent_kind: crate::AgentKind::Claude,
        };
        let formatted = format_event(&event);
        assert_eq!(formatted, "Pane(k42)::State { Idle → Thinking ws:3 ? }");
    }

    #[test]
    fn test_activity_pulse() {
        let event = BabelEvent::ActivityPulse {
            kitty_id: 42,
            session_id: None,
            workspace: Some(1),
            intensity: 0.75,
            trigger: PulseTrigger::TokenOutput,
        };
        let formatted = format_event(&event);
        assert_eq!(formatted, "Pane(k42)::Pulse { 0.75, token ws:1 }");
    }

    #[test]
    fn test_window_added() {
        let event = BabelEvent::WindowAdded {
            kitty_id: 42,
            title: "claude - /home/user/project".to_string(),
            workspace: Some(2),
            agent_kind: crate::AgentKind::Claude,
        };
        let formatted = format_event(&event);
        assert_eq!(
            formatted,
            "Window(k42)::Added { ws:2 \"claude - /home/user/project\" }"
        );
    }

    #[test]
    fn test_session_matched() {
        let event = BabelEvent::SessionMatched {
            kitty_id: 42,
            session_id: "abcd1234-5678-90ab-cdef-1234567890ab".to_string(),
            confidence: "high".to_string(),
        };
        let formatted = format_event(&event);
        assert_eq!(formatted, "Session::Matched { k42 ↔ abcd1234, high }");
    }

    #[test]
    fn test_sfx_helpers() {
        assert_eq!(
            format_sfx_failed("complete", "socket error"),
            "SFX::Failed { complete, \"socket error\" }"
        );
        assert_eq!(format_sfx_triggered("ding"), "SFX::Triggered { ding }");
    }
}
