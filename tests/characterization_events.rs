use chrono::{TimeZone, Utc};
use claude_babel::{
    events::{BabelEvent, EventFilter, EventMessage, PulseTrigger},
    ActivityState, AgentKind,
};
use serde_json::{json, Value};

fn assert_event_json_roundtrip(event: BabelEvent, expected: Value) {
    let encoded = serde_json::to_value(&event).unwrap();
    assert_eq!(encoded, expected);

    let decoded: BabelEvent = serde_json::from_value(encoded.clone()).unwrap();
    assert_eq!(serde_json::to_value(decoded).unwrap(), encoded);
}

#[test]
fn window_events_keep_legacy_kitty_id_wire_shape() {
    assert_event_json_roundtrip(
        BabelEvent::WindowAdded {
            kitty_id: 42,
            title: "claude - /home/nuck/project".to_string(),
            workspace: Some(3),
            agent_kind: AgentKind::Codex,
        },
        json!({
            "event": "window_added",
            "kitty_id": 42,
            "title": "claude - /home/nuck/project",
            "workspace": 3,
            "agent_kind": "codex"
        }),
    );

    assert_event_json_roundtrip(
        BabelEvent::WindowRemoved { kitty_id: 42 },
        json!({
            "event": "window_removed",
            "kitty_id": 42
        }),
    );
}

#[test]
fn session_state_changed_keeps_kitty_and_session_identity_wire_shape() {
    assert_event_json_roundtrip(
        BabelEvent::SessionStateChanged {
            kitty_id: 77,
            session_id: Some("session-abc".to_string()),
            workspace: Some(4),
            old_state: ActivityState::Idle,
            new_state: ActivityState::ToolUse,
            asking_question: true,
            agent_kind: AgentKind::Claude,
        },
        json!({
            "event": "session_state_changed",
            "kitty_id": 77,
            "session_id": "session-abc",
            "workspace": 4,
            "old_state": "idle",
            "new_state": "tool_use",
            "asking_question": true,
            "agent_kind": "claude"
        }),
    );
}

#[test]
fn activity_pulse_keeps_trigger_and_optional_session_wire_shape() {
    assert_event_json_roundtrip(
        BabelEvent::ActivityPulse {
            kitty_id: 88,
            session_id: None,
            workspace: Some(2),
            intensity: 0.625,
            trigger: PulseTrigger::HookLifecycle,
        },
        json!({
            "event": "activity_pulse",
            "kitty_id": 88,
            "session_id": null,
            "workspace": 2,
            "intensity": 0.625,
            "trigger": "hook_lifecycle"
        }),
    );
}

#[test]
fn hook_session_events_keep_session_id_and_optional_kitty_id_wire_shape() {
    assert_event_json_roundtrip(
        BabelEvent::SessionStarted {
            session_id: "sess-start".to_string(),
            kitty_id: Some(1001),
            cwd: "/home/nuck/holoq/repo-os/babel".to_string(),
            resumed: true,
        },
        json!({
            "event": "session_started",
            "session_id": "sess-start",
            "kitty_id": 1001,
            "cwd": "/home/nuck/holoq/repo-os/babel",
            "resumed": true
        }),
    );

    assert_event_json_roundtrip(
        BabelEvent::ToolStarted {
            session_id: "sess-tool".to_string(),
            kitty_id: Some(1002),
            tool_name: "Bash".to_string(),
        },
        json!({
            "event": "tool_started",
            "session_id": "sess-tool",
            "kitty_id": 1002,
            "tool_name": "Bash"
        }),
    );

    assert_event_json_roundtrip(
        BabelEvent::NotificationReceived {
            session_id: "sess-notify".to_string(),
            kitty_id: None,
            notif_type: "permission".to_string(),
            message: Some("allow command?".to_string()),
        },
        json!({
            "event": "notification_received",
            "session_id": "sess-notify",
            "kitty_id": null,
            "notif_type": "permission",
            "message": "allow command?"
        }),
    );

    assert_event_json_roundtrip(
        BabelEvent::TranscriptCompacting {
            session_id: "sess-compact".to_string(),
            kitty_id: Some(1003),
            transcript_path: "/home/nuck/.claude/projects/session.jsonl".to_string(),
        },
        json!({
            "event": "transcript_compacting",
            "session_id": "sess-compact",
            "kitty_id": 1003,
            "transcript_path": "/home/nuck/.claude/projects/session.jsonl"
        }),
    );
}

#[test]
fn event_message_flattens_event_payload_at_top_level() {
    let message = EventMessage {
        timestamp: Utc.with_ymd_and_hms(2026, 5, 1, 12, 30, 45).unwrap(),
        seq: 17,
        event: BabelEvent::WindowRemoved { kitty_id: 9001 },
    };

    assert_eq!(
        serde_json::to_value(message).unwrap(),
        json!({
            "timestamp": "2026-05-01T12:30:45Z",
            "seq": 17,
            "event": "window_removed",
            "kitty_id": 9001
        })
    );
}

#[test]
fn legacy_agent_event_payloads_without_agent_kind_still_decode_as_claude() {
    let window_added: BabelEvent = serde_json::from_value(json!({
        "event": "window_added",
        "kitty_id": 7,
        "title": "legacy claude pane",
        "workspace": null
    }))
    .unwrap();

    match &window_added {
        BabelEvent::WindowAdded {
            kitty_id,
            agent_kind,
            ..
        } => {
            assert_eq!(*kitty_id, 7);
            assert_eq!(*agent_kind, AgentKind::Claude);
        }
        other => panic!("expected WindowAdded, got {other:?}"),
    }
    assert_eq!(
        serde_json::to_value(window_added).unwrap(),
        json!({
            "event": "window_added",
            "kitty_id": 7,
            "title": "legacy claude pane",
            "workspace": null,
            "agent_kind": "claude"
        })
    );

    let state_changed: BabelEvent = serde_json::from_value(json!({
        "event": "session_state_changed",
        "kitty_id": 8,
        "session_id": "legacy-session",
        "workspace": null,
        "old_state": "thinking",
        "new_state": "awaiting_input",
        "asking_question": false
    }))
    .unwrap();

    match &state_changed {
        BabelEvent::SessionStateChanged {
            kitty_id,
            session_id,
            agent_kind,
            ..
        } => {
            assert_eq!(*kitty_id, 8);
            assert_eq!(session_id.as_deref(), Some("legacy-session"));
            assert_eq!(*agent_kind, AgentKind::Claude);
        }
        other => panic!("expected SessionStateChanged, got {other:?}"),
    }
    assert_eq!(
        serde_json::to_value(state_changed).unwrap(),
        json!({
            "event": "session_state_changed",
            "kitty_id": 8,
            "session_id": "legacy-session",
            "workspace": null,
            "old_state": "thinking",
            "new_state": "awaiting_input",
            "asking_question": false,
            "agent_kind": "claude"
        })
    );
}

#[test]
fn legacy_terminal_became_claude_alias_still_decodes() {
    let event: BabelEvent = serde_json::from_value(json!({
        "event": "terminal_became_claude",
        "kitty_id": 11,
        "title": "claude"
    }))
    .unwrap();

    assert_eq!(
        serde_json::to_value(event).unwrap(),
        json!({
            "event": "terminal_became_agent",
            "kitty_id": 11,
            "title": "claude"
        })
    );
}

#[test]
fn event_filter_matches_empty_or_exact_snake_case_event_names() {
    let window_added = BabelEvent::WindowAdded {
        kitty_id: 1,
        title: "pane".to_string(),
        workspace: None,
        agent_kind: AgentKind::Claude,
    };
    let window_removed = BabelEvent::WindowRemoved { kitty_id: 1 };
    let pulse = BabelEvent::ActivityPulse {
        kitty_id: 1,
        session_id: Some("sess".to_string()),
        workspace: None,
        intensity: 1.0,
        trigger: PulseTrigger::TokenOutput,
    };
    let state_changed = BabelEvent::SessionStateChanged {
        kitty_id: 1,
        session_id: Some("sess".to_string()),
        workspace: None,
        old_state: ActivityState::Thinking,
        new_state: ActivityState::AwaitingInput,
        asking_question: false,
        agent_kind: AgentKind::Claude,
    };

    let empty = EventFilter::new();
    assert!(empty.matches(&window_added));
    assert!(empty.matches(&window_removed));
    assert!(empty.matches(&pulse));
    assert!(empty.matches(&state_changed));

    let selective = EventFilter::with_events(vec![
        "window_added".to_string(),
        "activity_pulse".to_string(),
        "session_state_changed".to_string(),
    ]);
    assert!(selective.matches(&window_added));
    assert!(selective.matches(&pulse));
    assert!(selective.matches(&state_changed));
    assert!(!selective.matches(&window_removed));

    let case_sensitive = EventFilter::with_events(vec!["WindowAdded".to_string()]);
    assert!(!case_sensitive.matches(&window_added));
}
