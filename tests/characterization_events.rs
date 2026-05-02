use babel::{
    events::{BabelEvent, EventFilter, EventMessage, PulseTrigger},
    ActivityState, AgentKind, PaneAddr,
};
use chrono::{TimeZone, Utc};
use serde_json::{json, Value};

const SOCK: &str = "unix:/run/user/1000/kitty.sock-12345";

fn addr(id: u64) -> PaneAddr {
    PaneAddr::new(SOCK, id)
}

fn assert_event_json_roundtrip(event: BabelEvent, expected: Value) {
    let encoded = serde_json::to_value(&event).unwrap();
    assert_eq!(encoded, expected);

    let decoded: BabelEvent = serde_json::from_value(encoded.clone()).unwrap();
    assert_eq!(serde_json::to_value(decoded).unwrap(), encoded);
}

#[test]
fn window_lifecycle_events_carry_pane_addr_on_the_wire() {
    assert_event_json_roundtrip(
        BabelEvent::WindowAdded {
            addr: addr(42),
            title: "claude - /home/example/project".to_string(),
            workspace: Some(3),
            agent_kind: AgentKind::Codex,
        },
        json!({
            "event": "window_added",
            "addr": { "socket": SOCK, "id": 42 },
            "title": "claude - /home/example/project",
            "workspace": 3,
            "agent_kind": "codex"
        }),
    );

    assert_event_json_roundtrip(
        BabelEvent::WindowRemoved { addr: addr(42) },
        json!({
            "event": "window_removed",
            "addr": { "socket": SOCK, "id": 42 }
        }),
    );
}

#[test]
fn focus_and_workspace_events_carry_pane_addr_on_the_wire() {
    assert_event_json_roundtrip(
        BabelEvent::PaneFocused {
            addr: addr(13),
            session_id: Some("sess-focused".to_string()),
        },
        json!({
            "event": "pane_focused",
            "addr": { "socket": SOCK, "id": 13 },
            "session_id": "sess-focused"
        }),
    );

    assert_event_json_roundtrip(
        BabelEvent::PaneUnfocused {
            addr: addr(13),
            session_id: None,
        },
        json!({
            "event": "pane_unfocused",
            "addr": { "socket": SOCK, "id": 13 },
            "session_id": null
        }),
    );

    assert_event_json_roundtrip(
        BabelEvent::WindowWorkspaceChanged {
            addr: addr(21),
            old_workspace: Some(1),
            new_workspace: Some(4),
        },
        json!({
            "event": "window_workspace_changed",
            "addr": { "socket": SOCK, "id": 21 },
            "old_workspace": 1,
            "new_workspace": 4
        }),
    );
}

#[test]
fn terminal_events_carry_pane_addr_on_the_wire() {
    assert_event_json_roundtrip(
        BabelEvent::TerminalOpened {
            addr: addr(101),
            title: "~/project: fish".to_string(),
            cwd: std::path::PathBuf::from("/home/example/project"),
            workspace: Some(2),
        },
        json!({
            "event": "terminal_opened",
            "addr": { "socket": SOCK, "id": 101 },
            "title": "~/project: fish",
            "cwd": "/home/example/project",
            "workspace": 2
        }),
    );

    assert_event_json_roundtrip(
        BabelEvent::TerminalClosed { addr: addr(101) },
        json!({
            "event": "terminal_closed",
            "addr": { "socket": SOCK, "id": 101 }
        }),
    );

    assert_event_json_roundtrip(
        BabelEvent::TerminalBecameAgent {
            addr: addr(101),
            title: "✳ thinking".to_string(),
        },
        json!({
            "event": "terminal_became_agent",
            "addr": { "socket": SOCK, "id": 101 },
            "title": "✳ thinking"
        }),
    );
}

#[test]
fn session_match_event_carries_pane_addr_and_session_id() {
    assert_event_json_roundtrip(
        BabelEvent::SessionMatched {
            addr: addr(44),
            session_id: "sess-match".to_string(),
            confidence: "high".to_string(),
        },
        json!({
            "event": "session_matched",
            "addr": { "socket": SOCK, "id": 44 },
            "session_id": "sess-match",
            "confidence": "high"
        }),
    );
}

#[test]
fn session_state_changed_carries_pane_addr_and_session_identity() {
    assert_event_json_roundtrip(
        BabelEvent::SessionStateChanged {
            addr: addr(77),
            session_id: Some("session-abc".to_string()),
            workspace: Some(4),
            old_state: ActivityState::Idle,
            new_state: ActivityState::ToolUse,
            asking_question: true,
            agent_kind: AgentKind::Claude,
        },
        json!({
            "event": "session_state_changed",
            "addr": { "socket": SOCK, "id": 77 },
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
fn activity_pulse_carries_pane_addr_and_trigger() {
    assert_event_json_roundtrip(
        BabelEvent::ActivityPulse {
            addr: addr(88),
            session_id: None,
            workspace: Some(2),
            intensity: 0.625,
            trigger: PulseTrigger::HookLifecycle,
        },
        json!({
            "event": "activity_pulse",
            "addr": { "socket": SOCK, "id": 88 },
            "session_id": null,
            "workspace": 2,
            "intensity": 0.625,
            "trigger": "hook_lifecycle"
        }),
    );
}

#[test]
fn pane_addr_disambiguates_same_id_across_sockets_in_state_changed() {
    // Two different kitty instances can both run pane id 7. Encoded BabelEvents
    // for those panes must therefore differ on the wire — that's the whole
    // reason live-pane events carry the address rather than just the raw id.
    let one = BabelEvent::SessionStateChanged {
        addr: PaneAddr::new("unix:/run/user/1000/kitty.sock-111", 7),
        session_id: Some("a".to_string()),
        workspace: None,
        old_state: ActivityState::Idle,
        new_state: ActivityState::Thinking,
        asking_question: false,
        agent_kind: AgentKind::Claude,
    };
    let two = BabelEvent::SessionStateChanged {
        addr: PaneAddr::new("unix:/run/user/1000/kitty.sock-222", 7),
        session_id: Some("b".to_string()),
        workspace: None,
        old_state: ActivityState::Idle,
        new_state: ActivityState::Thinking,
        asking_question: false,
        agent_kind: AgentKind::Claude,
    };

    let one_json = serde_json::to_value(&one).unwrap();
    let two_json = serde_json::to_value(&two).unwrap();

    assert_eq!(one_json["addr"]["id"], 7);
    assert_eq!(two_json["addr"]["id"], 7);
    assert_ne!(one_json["addr"]["socket"], two_json["addr"]["socket"]);
    assert_ne!(one_json, two_json);
}

#[test]
fn hook_session_events_keep_session_id_and_optional_kitty_id_wire_shape() {
    // Hook payloads do not currently carry a socket alongside their kitty id,
    // so these events stay session-keyed with a kitty_id hint until the hook
    // ingestion path can attach a full PaneAddr.
    assert_event_json_roundtrip(
        BabelEvent::SessionStarted {
            session_id: "sess-start".to_string(),
            kitty_id: Some(1001),
            cwd: "/home/example/projects/babel".to_string(),
            resumed: true,
        },
        json!({
            "event": "session_started",
            "session_id": "sess-start",
            "kitty_id": 1001,
            "cwd": "/home/example/projects/babel",
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
            transcript_path: "/home/example/.claude/projects/session.jsonl".to_string(),
        },
        json!({
            "event": "transcript_compacting",
            "session_id": "sess-compact",
            "kitty_id": 1003,
            "transcript_path": "/home/example/.claude/projects/session.jsonl"
        }),
    );
}

#[test]
fn event_message_flattens_event_payload_at_top_level() {
    let message = EventMessage {
        timestamp: Utc.with_ymd_and_hms(2026, 5, 1, 12, 30, 45).unwrap(),
        seq: 17,
        event: BabelEvent::WindowRemoved { addr: addr(9001) },
    };

    assert_eq!(
        serde_json::to_value(message).unwrap(),
        json!({
            "timestamp": "2026-05-01T12:30:45Z",
            "seq": 17,
            "event": "window_removed",
            "addr": { "socket": SOCK, "id": 9001 }
        })
    );
}

#[test]
fn legacy_agent_event_payloads_without_agent_kind_still_decode_as_claude() {
    let window_added: BabelEvent = serde_json::from_value(json!({
        "event": "window_added",
        "addr": { "socket": SOCK, "id": 7 },
        "title": "legacy claude pane",
        "workspace": null
    }))
    .unwrap();

    match &window_added {
        BabelEvent::WindowAdded {
            addr,
            agent_kind,
            ..
        } => {
            assert_eq!(addr.id, 7);
            assert_eq!(*agent_kind, AgentKind::Claude);
        }
        other => panic!("expected WindowAdded, got {other:?}"),
    }
    assert_eq!(
        serde_json::to_value(window_added).unwrap(),
        json!({
            "event": "window_added",
            "addr": { "socket": SOCK, "id": 7 },
            "title": "legacy claude pane",
            "workspace": null,
            "agent_kind": "claude"
        })
    );

    let state_changed: BabelEvent = serde_json::from_value(json!({
        "event": "session_state_changed",
        "addr": { "socket": SOCK, "id": 8 },
        "session_id": "legacy-session",
        "workspace": null,
        "old_state": "thinking",
        "new_state": "awaiting_input",
        "asking_question": false
    }))
    .unwrap();

    match &state_changed {
        BabelEvent::SessionStateChanged {
            addr,
            session_id,
            agent_kind,
            ..
        } => {
            assert_eq!(addr.id, 8);
            assert_eq!(session_id.as_deref(), Some("legacy-session"));
            assert_eq!(*agent_kind, AgentKind::Claude);
        }
        other => panic!("expected SessionStateChanged, got {other:?}"),
    }
    assert_eq!(
        serde_json::to_value(state_changed).unwrap(),
        json!({
            "event": "session_state_changed",
            "addr": { "socket": SOCK, "id": 8 },
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
        "addr": { "socket": SOCK, "id": 11 },
        "title": "claude"
    }))
    .unwrap();

    assert_eq!(
        serde_json::to_value(event).unwrap(),
        json!({
            "event": "terminal_became_agent",
            "addr": { "socket": SOCK, "id": 11 },
            "title": "claude"
        })
    );
}

#[test]
fn event_filter_matches_empty_or_exact_snake_case_event_names() {
    let window_added = BabelEvent::WindowAdded {
        addr: addr(1),
        title: "pane".to_string(),
        workspace: None,
        agent_kind: AgentKind::Claude,
    };
    let window_removed = BabelEvent::WindowRemoved { addr: addr(1) };
    let pulse = BabelEvent::ActivityPulse {
        addr: addr(1),
        session_id: Some("sess".to_string()),
        workspace: None,
        intensity: 1.0,
        trigger: PulseTrigger::TokenOutput,
    };
    let state_changed = BabelEvent::SessionStateChanged {
        addr: addr(1),
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
