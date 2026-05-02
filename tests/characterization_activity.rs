use claude_babel::babel_storage::{
    get_hook_state, get_metadata, mark_read, mark_unread, set_hook_state, HookState,
};
use claude_babel::paint::{resolve_color, workspace_css_class, workspace_is_urgent};
use claude_babel::utility::ipc::Request;
use claude_babel::{AgentKind, HookStateEffect, PulseEffect, ReadEffect};
use rusqlite::Connection;
use scrollparse::claude::ActivityState;
use serde_json::json;

fn metadata_conn() -> Connection {
    let conn = Connection::open_in_memory().unwrap();
    conn.execute_batch(
        "CREATE TABLE session_metadata (
            session_id TEXT PRIMARY KEY,
            icon TEXT,
            is_read INTEGER DEFAULT 0,
            chapter_history TEXT,
            notes TEXT,
            hook_state TEXT DEFAULT 'idle',
            last_hook_at INTEGER
        );",
    )
    .unwrap();
    conn
}

#[test]
fn hook_state_strings_are_the_storage_boundary() {
    assert_eq!(HookState::Working.as_str(), "working");
    assert_eq!(HookState::Idle.as_str(), "idle");
    assert_eq!(HookState::ToolRunning.as_str(), "tool_running");

    assert_eq!(HookState::from_str("working"), HookState::Working);
    assert_eq!(HookState::from_str("idle"), HookState::Idle);
    assert_eq!(HookState::from_str("tool_running"), HookState::ToolRunning);
    assert_eq!(HookState::from_str("unknown"), HookState::Idle);
    assert_eq!(HookState::from_str("ToolRunning"), HookState::Idle);
}

#[test]
fn read_unread_and_hook_state_share_one_metadata_row() -> anyhow::Result<()> {
    let conn = metadata_conn();
    let session = "claude:wave-zero";

    assert!(get_metadata(&conn, session)?.is_none());

    mark_unread(&conn, session)?;
    let meta = get_metadata(&conn, session)?.unwrap();
    assert!(!meta.is_read);
    assert_eq!(meta.hook_state, HookState::Idle);
    assert!(meta.last_hook_at.is_none());

    mark_read(&conn, session)?;
    set_hook_state(&conn, session, HookState::Working)?;
    let meta = get_metadata(&conn, session)?.unwrap();
    assert!(meta.is_read);
    assert_eq!(meta.hook_state, HookState::Working);
    assert!(meta.last_hook_at.is_some());
    assert_eq!(get_hook_state(&conn, session)?, Some(HookState::Working));

    mark_unread(&conn, session)?;
    let meta = get_metadata(&conn, session)?.unwrap();
    assert!(!meta.is_read);
    assert_eq!(
        meta.hook_state,
        HookState::Working,
        "read/unread updates do not reset hook state"
    );

    Ok(())
}

#[test]
fn paint_color_resolution_prefers_hook_truth_then_activity_detail() {
    assert_eq!(
        resolve_color(
            AgentKind::Claude,
            Some(HookState::Idle),
            Some(ActivityState::ToolUse)
        ),
        "#666666"
    );
    assert_eq!(
        resolve_color(
            AgentKind::Codex,
            Some(HookState::ToolRunning),
            Some(ActivityState::AwaitingInput)
        ),
        AgentKind::Codex.accent_color()
    );
    assert_eq!(
        resolve_color(
            AgentKind::Claude,
            Some(HookState::Working),
            Some(ActivityState::PlanApproval)
        ),
        "#c080f0"
    );
    assert_eq!(
        resolve_color(
            AgentKind::Claude,
            Some(HookState::Working),
            Some(ActivityState::BackgroundTask)
        ),
        "#40f0c0"
    );
    assert_eq!(
        resolve_color(AgentKind::Claude, None, Some(ActivityState::AwaitingInput)),
        "#f04080"
    );
    assert_eq!(resolve_color(AgentKind::Claude, None, None), "#666666");
}

#[test]
fn workspace_activity_class_and_urgency_are_aggregate_activity_snapshots() {
    assert_eq!(workspace_css_class(&[]), None);
    assert_eq!(
        workspace_css_class(&[ActivityState::Idle, ActivityState::Unknown]),
        Some("claude-idle")
    );
    assert_eq!(
        workspace_css_class(&[ActivityState::Thinking, ActivityState::ToolUse]),
        Some("claude-busy-all")
    );
    assert_eq!(
        workspace_css_class(&[ActivityState::Thinking, ActivityState::Idle]),
        Some("claude-busy")
    );
    assert_eq!(
        workspace_css_class(&[ActivityState::ToolUse, ActivityState::AwaitingInput]),
        Some("claude-await")
    );

    assert!(!workspace_is_urgent(&[ActivityState::Thinking]));
    assert!(workspace_is_urgent(&[
        ActivityState::Idle,
        ActivityState::AwaitingInput
    ]));
}

#[test]
fn hook_roster_events_lock_state_read_and_pulse_effects() {
    let prompt = AgentKind::Claude.spec().event("UserPromptSubmit").unwrap();
    assert_eq!(prompt.canonical, "prompt");
    assert_eq!(prompt.state, Some(HookStateEffect::Working));
    assert_eq!(prompt.read, ReadEffect::MarkRead);
    assert_eq!(prompt.pulse, PulseEffect::Prompt);

    let stop = AgentKind::Claude.spec().event("stop").unwrap();
    assert_eq!(stop.native, "Stop");
    assert_eq!(stop.state, Some(HookStateEffect::Idle));
    assert_eq!(stop.read, ReadEffect::MarkUnread);
    assert_eq!(stop.pulse, PulseEffect::Finished);

    let codex_attention = AgentKind::Codex.spec().event("PermissionRequest").unwrap();
    assert_eq!(codex_attention.canonical, "notification");
    assert_eq!(codex_attention.state, None);
    assert_eq!(codex_attention.read, ReadEffect::Preserve);
    assert_eq!(codex_attention.pulse, PulseEffect::Attention);

    let gemini_pre_tool = AgentKind::Gemini.spec().event("pre-tool").unwrap();
    assert_eq!(gemini_pre_tool.native, "BeforeTool");
    assert_eq!(gemini_pre_tool.state, Some(HookStateEffect::ToolRunning));
}

#[test]
fn hook_ipc_request_serializes_current_wire_shape() -> Result<(), serde_json::Error> {
    let request = Request::HookEvent {
        session: "codex:workspace:kitty-7".to_string(),
        kitty_id: Some(7),
        pane_addr: None,
        agent_kind: AgentKind::Codex,
        hook_state: Some(HookState::Idle),
        pulse: PulseEffect::Finished,
        hook_type: "stop".to_string(),
    };

    assert_eq!(
        serde_json::to_value(request)?,
        json!({
            "cmd": "hook_event",
            "session": "codex:workspace:kitty-7",
            "kitty_id": 7,
            "pane_addr": null,
            "agent_kind": "codex",
            "hook_state": "Idle",
            "pulse": "finished",
            "hook_type": "stop"
        })
    );

    Ok(())
}
