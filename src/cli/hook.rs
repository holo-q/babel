//! Hook handlers - the normalized lifecycle interface between harnesses and Babel.
//!
//! Harness-specific facts live in the roster (`AgentKind::spec()`): identity
//! fields, native event names, state transitions, read/unread effects, and
//! pulse semantics. This module executes those facts. Missing hooks are handled
//! by the spec being partial, not by scattering special cases through the
//! daemon or panel clients.

use std::io::Read as _;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::Deserialize;
use serde_json::{json, Map, Value};
use tracing::{debug, info, warn};

use babel::babel_storage::{
    init_db, mark_read, mark_unread, set_hook_state, set_last_workspace, HookState,
};
use babel::ipc::Request;
use babel::kitty::{
    default_socket, reset_border_color_on_socket, set_border_color_on_socket, PaneAddr,
};
use babel::utility::agent_discovery::tag_pane_addr;
use babel::utility::claude_storage::path_to_encoded;
use babel::utility::ipc::send_request;
use babel::{
    AgentKind, HarnessSupport, HookEventSpec, HookStateEffect, InstallStrategy, PulseEffect,
    ReadEffect,
};

/// Convert roster-level state effects into persisted hook state.
fn hook_state_from_effect(effect: HookStateEffect) -> HookState {
    match effect {
        HookStateEffect::Working => HookState::Working,
        HookStateEffect::Idle => HookState::Idle,
        HookStateEffect::ToolRunning => HookState::ToolRunning,
    }
}

/// Current pane address from kitty's hook environment.
fn current_pane_addr() -> Option<PaneAddr> {
    let id = std::env::var("KITTY_WINDOW_ID")
        .ok()
        .and_then(|v| v.parse().ok())?;
    let socket = std::env::var("KITTY_LISTEN_ON").unwrap_or_else(|_| default_socket());
    Some(PaneAddr::new(socket, id))
}

fn current_working_dir() -> PathBuf {
    if let Ok(pwd) = std::env::var("PWD") {
        PathBuf::from(pwd)
    } else {
        std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."))
    }
}

fn codex_notify_native_key(kitty_id: Option<u64>, cwd: &Path) -> String {
    let scope = path_to_encoded(cwd);
    match kitty_id {
        Some(id) => format!("{}:kitty-{}", scope, id),
        None => format!("{}:detached", scope),
    }
}

fn namespaced_session(agent_kind: AgentKind, native_id: &str) -> String {
    let expected = format!("{}:", agent_kind.slug());
    if native_id.starts_with(&expected) {
        native_id.to_string()
    } else {
        agent_kind.session_key(native_id)
    }
}

async fn ensure_pane_tag(session: &str, pane_addr: Option<&PaneAddr>) {
    if let Some(addr) = pane_addr {
        match tag_pane_addr(addr, session).await {
            Ok(()) => debug!(
                session,
                kitty_id = addr.id,
                socket = %addr.socket,
                "Tagged kitty pane for fast future discovery"
            ),
            Err(e) => debug!(
                session,
                kitty_id = addr.id,
                socket = %addr.socket,
                error = %e,
                "Failed to tag kitty pane"
            ),
        }
    }
}

/// Push hook flow into daemon memory via IPC.
async fn push_to_daemon(
    session: &str,
    pane_addr: Option<&PaneAddr>,
    agent_kind: AgentKind,
    hook_state: Option<HookState>,
    pulse: PulseEffect,
    read: ReadEffect,
    hook_type: &str,
) {
    let request = Request::HookEvent {
        session: session.to_string(),
        kitty_id: pane_addr.map(|addr| addr.id),
        pane_addr: pane_addr.cloned(),
        tmux_pane: std::env::var("TMUX_PANE").ok(),
        zellij_pane: std::env::var("ZELLIJ_PANE_ID").ok(),
        agent_kind,
        hook_state,
        pulse,
        read,
        hook_type: hook_type.to_string(),
    };
    match send_request(&request).await {
        Ok(_) => debug!(session, hook_type, "Hook pushed to daemon"),
        Err(e) => debug!(session, hook_type, error = %e, "Daemon push failed (sqlite fallback)"),
    }
}

async fn apply_read_effect(
    session: &str,
    agent_kind: AgentKind,
    pane_addr: Option<&PaneAddr>,
    read: ReadEffect,
) {
    let Ok(conn) = init_db() else {
        return;
    };

    match read {
        ReadEffect::Preserve => {}
        ReadEffect::MarkRead => {
            if let Err(e) = mark_read(&conn, session) {
                warn!(session, error = %e, "Failed to mark session read");
            }
            if let Some(addr) = pane_addr {
                if let Err(e) = reset_border_color_on_socket(&addr.socket, addr.id).await {
                    debug!(error = %e, "Failed to reset unread border");
                }
            }
        }
        ReadEffect::MarkUnread => {
            if let Err(e) = mark_unread(&conn, session) {
                warn!(session, error = %e, "Failed to mark session unread");
            }
            if let Some(addr) = pane_addr {
                let color = agent_kind.accent_color();
                if let Err(e) =
                    set_border_color_on_socket(&addr.socket, addr.id, color, color).await
                {
                    debug!(error = %e, "Failed to set unread border");
                }
            }
        }
    }
}

/// Execute one normalized hook flow.
async fn execute_hook_flow(
    session: &str,
    agent_kind: AgentKind,
    pane_addr: Option<PaneAddr>,
    event: &HookEventSpec,
) -> Result<()> {
    debug!(
        agent = %agent_kind,
        session,
        native = event.native,
        canonical = event.canonical,
        state = ?event.state,
        read = ?event.read,
        pulse = ?event.pulse,
        "Hook flow dispatch"
    );

    ensure_pane_tag(session, pane_addr.as_ref()).await;

    let hook_state = event.state.map(hook_state_from_effect);
    if let Some(state) = hook_state {
        if let Ok(conn) = init_db() {
            if let Err(e) = set_hook_state(&conn, session, state) {
                warn!(session, error = %e, "Failed to set hook state");
            }
        }
    }

    // Persist workspace on every hook event so resume can restore placement
    if let Some(addr) = pane_addr.as_ref() {
        if let Ok(Some(pane)) = babel::kitty::get_window(addr.id).await {
            if let Some(ws) = pane.workspace() {
                if let Ok(conn) = init_db() {
                    let _ = set_last_workspace(&conn, session, ws);
                }
            }
        }
    }

    apply_read_effect(session, agent_kind, pane_addr.as_ref(), event.read).await;
    push_to_daemon(
        session,
        pane_addr.as_ref(),
        agent_kind,
        hook_state,
        event.pulse,
        event.read,
        event.canonical,
    )
    .await;

    Ok(())
}

fn synthetic_event(
    native: &'static str,
    canonical: &'static str,
    state: Option<HookStateEffect>,
    read: ReadEffect,
    pulse: PulseEffect,
) -> HookEventSpec {
    HookEventSpec {
        native,
        canonical,
        state,
        read,
        pulse,
    }
}

/// Handle Stop hook - worker has finished speaking.
pub async fn handle_stop(
    session: &str,
    kitty_id: Option<u64>,
    _transcript: Option<&str>,
) -> Result<()> {
    let session = namespaced_session(AgentKind::Claude, session);
    let pane_addr = kitty_id.map(|id| PaneAddr::new(default_socket(), id));
    let event = synthetic_event(
        "Stop",
        "stop",
        Some(HookStateEffect::Idle),
        ReadEffect::MarkUnread,
        PulseEffect::Finished,
    );
    execute_hook_flow(&session, AgentKind::Claude, pane_addr, &event).await
}

/// Handle UserPromptSubmit hook.
pub async fn handle_prompt(session: &str, kitty_id: Option<u64>) -> Result<()> {
    let session = namespaced_session(AgentKind::Claude, session);
    let pane_addr = kitty_id.map(|id| PaneAddr::new(default_socket(), id));
    let event = synthetic_event(
        "UserPromptSubmit",
        "prompt",
        Some(HookStateEffect::Working),
        ReadEffect::MarkRead,
        PulseEffect::Prompt,
    );
    execute_hook_flow(&session, AgentKind::Claude, pane_addr, &event).await
}

/// Handle PreToolUse hook.
pub async fn handle_pre_tool(
    session: &str,
    kitty_id: Option<u64>,
    tool_name: &str,
    _tool_input: Option<&str>,
) -> Result<()> {
    debug!(
        session,
        kitty_id,
        tool = tool_name,
        "Hook: PreToolUse received"
    );
    let session = namespaced_session(AgentKind::Claude, session);
    let pane_addr = kitty_id.map(|id| PaneAddr::new(default_socket(), id));
    let event = synthetic_event(
        "PreToolUse",
        "pre-tool",
        Some(HookStateEffect::ToolRunning),
        ReadEffect::Preserve,
        PulseEffect::Tool,
    );
    execute_hook_flow(&session, AgentKind::Claude, pane_addr, &event).await
}

/// Handle PostToolUse hook.
pub async fn handle_post_tool(
    session: &str,
    kitty_id: Option<u64>,
    tool_name: &str,
    _tool_output: Option<&str>,
) -> Result<()> {
    debug!(
        session,
        kitty_id,
        tool = tool_name,
        "Hook: PostToolUse received"
    );
    let session = namespaced_session(AgentKind::Claude, session);
    let pane_addr = kitty_id.map(|id| PaneAddr::new(default_socket(), id));
    let event = synthetic_event(
        "PostToolUse",
        "post-tool",
        Some(HookStateEffect::Working),
        ReadEffect::Preserve,
        PulseEffect::Tool,
    );
    execute_hook_flow(&session, AgentKind::Claude, pane_addr, &event).await
}

/// Handle Notification hook.
pub async fn handle_notification(
    session: &str,
    kitty_id: Option<u64>,
    notif_type: &str,
    message: Option<&str>,
) -> Result<()> {
    info!(
        session,
        kitty_id,
        notif_type,
        message = message.unwrap_or("<none>"),
        "Hook: Notification received"
    );
    let session = namespaced_session(AgentKind::Claude, session);
    let pane_addr = kitty_id.map(|id| PaneAddr::new(default_socket(), id));
    let event = synthetic_event(
        "Notification",
        "notification",
        None,
        ReadEffect::Preserve,
        PulseEffect::Attention,
    );
    execute_hook_flow(&session, AgentKind::Claude, pane_addr, &event).await
}

/// Handle SessionStart hook.
pub async fn handle_session_start(
    session: &str,
    kitty_id: Option<u64>,
    cwd: &str,
    resumed: bool,
) -> Result<()> {
    info!(
        session,
        kitty_id, cwd, resumed, "Hook: SessionStart received"
    );
    let session = namespaced_session(AgentKind::Claude, session);
    let pane_addr = kitty_id.map(|id| PaneAddr::new(default_socket(), id));
    let event = synthetic_event(
        "SessionStart",
        "session-start",
        Some(HookStateEffect::Working),
        ReadEffect::Preserve,
        PulseEffect::Session,
    );
    execute_hook_flow(&session, AgentKind::Claude, pane_addr, &event).await
}

/// Handle SubagentStop hook.
pub async fn handle_subagent_stop(
    session: &str,
    kitty_id: Option<u64>,
    subagent_id: &str,
) -> Result<()> {
    debug!(
        session,
        kitty_id, subagent_id, "Hook: SubagentStop received"
    );
    let session = namespaced_session(AgentKind::Claude, session);
    let pane_addr = kitty_id.map(|id| PaneAddr::new(default_socket(), id));
    let event = synthetic_event(
        "SubagentStop",
        "subagent-stop",
        None,
        ReadEffect::Preserve,
        PulseEffect::Finished,
    );
    execute_hook_flow(&session, AgentKind::Claude, pane_addr, &event).await
}

/// Handle PreCompact hook.
pub async fn handle_pre_compact(
    session: &str,
    kitty_id: Option<u64>,
    transcript_path: &str,
) -> Result<()> {
    info!(
        session,
        kitty_id, transcript_path, "Hook: PreCompact received"
    );
    let session = namespaced_session(AgentKind::Claude, session);
    let pane_addr = kitty_id.map(|id| PaneAddr::new(default_socket(), id));
    let event = synthetic_event(
        "PreCompact",
        "pre-compact",
        None,
        ReadEffect::Preserve,
        PulseEffect::Compact,
    );
    execute_hook_flow(&session, AgentKind::Claude, pane_addr, &event).await
}

#[derive(Debug, Deserialize)]
struct CodexNotifyPayload {
    #[serde(rename = "type")]
    event_type: String,
    #[serde(default)]
    message: Option<String>,
    #[serde(
        default,
        alias = "last-assistant-message",
        alias = "last_assistant_message"
    )]
    last_assistant_message: Option<String>,
    #[serde(default, alias = "input-messages", alias = "input_messages")]
    input_messages: Option<serde_json::Value>,
    #[serde(default)]
    cwd: Option<String>,
}

/// Handle a Codex notify payload - legacy single-shot turn-complete signal.
pub async fn handle_codex_notify(payload: &str) -> Result<()> {
    let payload: CodexNotifyPayload =
        serde_json::from_str(payload).context("Failed to parse Codex notify payload")?;

    if payload.event_type != "agent-turn-complete" {
        debug!(event_type = %payload.event_type, "Ignoring unsupported Codex notify event");
        return Ok(());
    }

    let pane_addr = current_pane_addr();
    let cwd = payload
        .cwd
        .map(PathBuf::from)
        .unwrap_or_else(current_working_dir);
    let native = codex_notify_native_key(pane_addr.as_ref().map(|addr| addr.id), &cwd);
    let session = AgentKind::Codex.session_key(&native);

    info!(
        session,
        kitty_id = ?pane_addr.as_ref().map(|addr| addr.id),
        cwd = %cwd.display(),
        message = payload.message.as_deref().unwrap_or("<none>"),
        last_assistant_message = payload.last_assistant_message.as_deref().unwrap_or("<none>"),
        has_input_messages = payload.input_messages.is_some(),
        "Codex notify received"
    );

    let event = synthetic_event(
        "agent-turn-complete",
        "stop",
        Some(HookStateEffect::Idle),
        ReadEffect::MarkUnread,
        PulseEffect::Finished,
    );
    execute_hook_flow(&session, AgentKind::Codex, pane_addr, &event).await
}

#[derive(Deserialize)]
struct HookPayload {
    #[serde(default)]
    tool_name: Option<String>,
    #[serde(rename = "type", default)]
    notif_type: Option<String>,
    #[serde(default)]
    message: Option<String>,
    #[serde(default)]
    cwd: Option<String>,
    #[serde(default)]
    resumed: Option<bool>,
    #[serde(default)]
    subagent_id: Option<String>,
    #[serde(default)]
    transcript_path: Option<String>,
}

fn payload_field(value: &Value, field: &str) -> Option<String> {
    value.get(field).and_then(|v| match v {
        Value::String(s) if !s.is_empty() => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        _ => None,
    })
}

fn extract_identity(raw: &Value, agent_kind: AgentKind) -> Option<String> {
    let spec = agent_kind.spec();
    for field in spec.identity_fields {
        if let Some(value) = payload_field(raw, field) {
            return Some(value);
        }
    }
    for env_key in spec.env_identity_fields {
        if let Ok(value) = std::env::var(env_key) {
            if !value.is_empty() {
                return Some(value);
            }
        }
    }
    None
}

/// Handle a hook event by reading JSON from stdin.
pub async fn handle_stdin(event: &str, agent_kind: AgentKind) -> Result<()> {
    let spec = agent_kind.spec();
    if !matches!(spec.support, HarnessSupport::Supported) {
        warn!(agent = %agent_kind, "Harness has no direct hook support");
        return Ok(());
    }

    let Some(event_spec) = spec.event(event) else {
        warn!(agent = %agent_kind, event, "Unknown hook event for harness");
        return Ok(());
    };

    let mut input = String::new();
    std::io::stdin()
        .read_to_string(&mut input)
        .context("Failed to read hook payload from stdin")?;

    let raw: Value = serde_json::from_str(&input).context("Failed to parse hook JSON payload")?;
    let payload: HookPayload =
        serde_json::from_value(raw.clone()).context("Failed to parse hook JSON payload")?;

    let native_id = match extract_identity(&raw, agent_kind) {
        Some(s) => s,
        None => {
            debug!(agent = %agent_kind, "No stable identity in payload/env, skipping");
            return Ok(());
        }
    };

    let session = agent_kind.session_key(&native_id);
    let pane_addr = current_pane_addr();
    debug!(
        agent = %agent_kind,
        event,
        native = event_spec.native,
        canonical = event_spec.canonical,
        session,
        kitty_id = ?pane_addr.as_ref().map(|addr| addr.id),
        "Hook stdin dispatch"
    );

    match event_spec.canonical {
        "pre-tool" => {
            let tool = payload.tool_name.as_deref().unwrap_or("unknown");
            debug!(session, tool, "Hook tool start");
        }
        "post-tool" => {
            let tool = payload.tool_name.as_deref().unwrap_or("unknown");
            debug!(session, tool, "Hook tool end");
        }
        "session-start" => {
            let cwd = payload.cwd.as_deref().unwrap_or(".");
            let resumed = payload.resumed.unwrap_or(false);
            info!(session, cwd, resumed, "Hook session start");
        }
        "notification" => {
            let notif_type = payload.notif_type.as_deref().unwrap_or("unknown");
            info!(
                session,
                notif_type,
                message = payload.message.as_deref().unwrap_or("<none>"),
                "Hook notification"
            );
        }
        "subagent-stop" => {
            debug!(
                session,
                subagent_id = payload.subagent_id.as_deref().unwrap_or("unknown"),
                "Hook subagent stop"
            );
        }
        "pre-compact" => {
            info!(
                session,
                transcript = payload.transcript_path.as_deref().unwrap_or(""),
                "Hook pre compact"
            );
        }
        "session-end" => {
            let reason = payload_field(&raw, "reason").unwrap_or_else(|| "unknown".into());
            info!(session, reason = %reason, "Hook session end");
        }
        "subagent-start" => {
            let agent_type = payload_field(&raw, "agent_type").unwrap_or_else(|| "unknown".into());
            debug!(session, agent_type = %agent_type, "Hook subagent start");
        }
        "stop-failure" => {
            let error_type = payload_field(&raw, "error_type").unwrap_or_else(|| "unknown".into());
            warn!(session, error_type = %error_type, "Hook stop failure");
        }
        "post-compact" => {
            info!(session, "Hook post compact");
        }
        "permission" => {
            let tool = payload.tool_name.as_deref().unwrap_or("unknown");
            info!(session, tool, "Hook permission request");
        }
        "setup" => {
            let trigger = payload_field(&raw, "trigger").unwrap_or_else(|| "unknown".into());
            info!(session, trigger = %trigger, "Hook setup");
        }
        "prompt-expand" => {
            let command = payload_field(&raw, "command_name").unwrap_or_else(|| "unknown".into());
            debug!(session, command = %command, "Hook prompt expansion");
        }
        "permission-denied" => {
            let tool = payload.tool_name.as_deref().unwrap_or("unknown");
            let reason = payload_field(&raw, "denial_reason").unwrap_or_else(|| "unknown".into());
            info!(session, tool, reason = %reason, "Hook permission denied");
        }
        "post-tool-fail" => {
            let tool = payload.tool_name.as_deref().unwrap_or("unknown");
            let error = payload_field(&raw, "error_message").unwrap_or_else(|| "unknown".into());
            warn!(session, tool, error = %error, "Hook tool failure");
        }
        "post-tool-batch" => {
            debug!(session, "Hook tool batch complete");
        }
        "task-created" => {
            let task_id = payload_field(&raw, "task_id").unwrap_or_else(|| "unknown".into());
            debug!(session, task_id = %task_id, "Hook task created");
        }
        "task-completed" => {
            let task_id = payload_field(&raw, "task_id").unwrap_or_else(|| "unknown".into());
            debug!(session, task_id = %task_id, "Hook task completed");
        }
        "teammate-idle" => {
            let teammate = payload_field(&raw, "teammate_id").unwrap_or_else(|| "unknown".into());
            info!(session, teammate = %teammate, "Hook teammate idle");
        }
        "instructions-loaded" => {
            let file = payload_field(&raw, "file_path").unwrap_or_else(|| "unknown".into());
            let reason = payload_field(&raw, "load_reason").unwrap_or_else(|| "unknown".into());
            debug!(session, file = %file, reason = %reason, "Hook instructions loaded");
        }
        "config-change" => {
            let source = payload_field(&raw, "config_source").unwrap_or_else(|| "unknown".into());
            info!(session, source = %source, "Hook config change");
        }
        "cwd-changed" => {
            let new_cwd = payload_field(&raw, "new_cwd").unwrap_or_else(|| "unknown".into());
            debug!(session, new_cwd = %new_cwd, "Hook cwd changed");
        }
        "file-changed" => {
            let file = payload_field(&raw, "file_path").unwrap_or_else(|| "unknown".into());
            debug!(session, file = %file, "Hook file changed");
        }
        "worktree-create" => {
            debug!(session, "Hook worktree create");
        }
        "worktree-remove" => {
            debug!(session, "Hook worktree remove");
        }
        "elicitation" => {
            let server = payload_field(&raw, "server_name").unwrap_or_else(|| "unknown".into());
            info!(session, server = %server, "Hook elicitation request");
        }
        "elicitation-result" => {
            let server = payload_field(&raw, "server_name").unwrap_or_else(|| "unknown".into());
            debug!(session, server = %server, "Hook elicitation result");
        }
        _ => {}
    }

    execute_hook_flow(&session, agent_kind, pane_addr, event_spec).await
}

fn hook_json_for(agent_kind: AgentKind) -> Value {
    let spec = agent_kind.spec();
    let mut hooks = Map::new();
    for event in spec.events {
        hooks.insert(
            event.native.to_string(),
            json!([{
                "matcher": "",
                "hooks": [{
                    "type": "command",
                    "command": format!("babel hook stdin {} --agent {}", event.canonical, spec.slug)
                }]
            }]),
        );
    }
    json!({ "hooks": hooks })
}

fn hook_toml_for(agent_kind: AgentKind) -> Result<String> {
    let spec = agent_kind.spec();
    let mut out = String::new();
    for event in spec.events {
        out.push_str("[[hooks]]\n");
        out.push_str(&format!("event = \"{}\"\n", event.native));
        out.push_str("matcher = \"\"\n");
        out.push_str(&format!(
            "command = \"babel hook stdin {} --agent {}\"\n\n",
            event.canonical, spec.slug
        ));
    }
    Ok(out)
}

fn install_claude_hooks(dry_run: bool) -> Result<()> {
    let settings_path = dirs::home_dir()
        .context("No home directory")?
        .join(".claude/settings.json");

    let mut settings: Value = if settings_path.exists() {
        let content = std::fs::read_to_string(&settings_path)
            .context("Failed to read ~/.claude/settings.json")?;
        serde_json::from_str(&content).context("Failed to parse settings.json")?
    } else {
        json!({})
    };

    let generated = hook_json_for(AgentKind::Claude);
    if let Some(new_hooks) = generated.get("hooks").and_then(|v| v.as_object()) {
        if let Some(existing_hooks) = settings.get_mut("hooks").and_then(|v| v.as_object_mut()) {
            for (event, config) in new_hooks {
                existing_hooks.insert(event.clone(), config.clone());
            }
        } else {
            settings["hooks"] = Value::Object(new_hooks.clone());
        }
    }

    let output = serde_json::to_string_pretty(&settings)?;
    if dry_run {
        println!("Would write to {}:\n", settings_path.display());
        println!("{}", output);
        println!("\n(dry run - no changes made)");
    } else {
        if let Some(parent) = settings_path.parent() {
            std::fs::create_dir_all(parent).context("Failed to create ~/.claude directory")?;
        }
        std::fs::write(&settings_path, &output).context("Failed to write settings.json")?;
        println!("Installed babel hooks to {}", settings_path.display());
    }
    Ok(())
}

fn print_snippet(agent_kind: AgentKind) -> Result<()> {
    let spec = agent_kind.spec();
    println!("{} ({})", spec.display, spec.slug);
    match spec.install {
        InstallStrategy::JsonSnippet => {
            println!(
                "{}",
                serde_json::to_string_pretty(&hook_json_for(agent_kind))?
            );
        }
        InstallStrategy::TomlSnippet => {
            print!("{}", hook_toml_for(agent_kind)?);
        }
        InstallStrategy::FilesystemSnippet => {
            println!(".clinerules/hooks/<Event>/* commands should invoke:");
            for event in spec.events {
                println!(
                    "  {} -> babel hook stdin {} --agent {}",
                    event.native, event.canonical, spec.slug
                );
            }
        }
        InstallStrategy::BridgeContract => {
            println!(
                "Bridge callback should send JSON to: babel hook stdin <canonical-event> --agent {}",
                spec.slug
            );
            println!("Required identity field: provider-native stable session/task id");
            println!("Canonical payload:");
            println!(
                "{}",
                serde_json::to_string_pretty(&json!({
                    "session_id": "stable-native-id",
                    "tool_name": "optional-tool",
                    "cwd": "optional-working-directory"
                }))?
            );
        }
        InstallStrategy::Unsupported => {
            println!("Unsupported: no stable lifecycle hook + identity surface.");
        }
        InstallStrategy::AutoJsonSettings => {
            println!(
                "{}",
                serde_json::to_string_pretty(&hook_json_for(agent_kind))?
            );
        }
    }
    Ok(())
}

/// Install or print babel hooks for supported harnesses.
pub async fn install_hooks(dry_run: bool, targets: &[crate::cli::HookInstallTarget]) -> Result<()> {
    use crate::cli::HookInstallTarget;

    let targets: Vec<HookInstallTarget> = if targets.is_empty() {
        vec![HookInstallTarget::Claude, HookInstallTarget::Codex]
    } else {
        targets.to_vec()
    };

    for target in targets {
        let kind = target.agent_kind();
        match kind.spec().install {
            InstallStrategy::AutoJsonSettings if kind == AgentKind::Claude => {
                install_claude_hooks(dry_run)?
            }
            _ => print_snippet(kind)?,
        }
    }

    Ok(())
}
