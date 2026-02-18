//! Hook handlers—the neural interface between Claude Code and Babel
//!
//! These commands are called by Claude Code hooks to signal lifecycle events.
//! They receive session_id directly from Claude Code, bypassing fingerprint matching.
//!
//! The hooks are the direct neural link: Claude Code whispers, Babel listens.

use anyhow::{Result, Context};
use tracing::{info, debug, warn};

use claude_babel::babel_storage::{init_db, mark_read, mark_unread, set_hook_state, HookState};
use claude_babel::kitty::{
    set_border_color_on_socket, reset_border_color_on_socket, default_socket,
};
use claude_babel::utility::ipc::{send_request, Request};

/// Push hook state into daemon memory via IPC (fire-and-forget).
///
/// Sends a HookEvent to the daemon so it updates BabelState immediately
/// instead of waiting for the next poll tick. Falls through silently if
/// the daemon isn't running — sqlite write is the durable fallback.
async fn push_to_daemon(session: &str, kitty_id: Option<u64>, hook_state: HookState, hook_type: &str) {
    let request = Request::HookEvent {
        session: session.to_string(),
        kitty_id,
        hook_state,
        hook_type: hook_type.to_string(),
    };
    match send_request(&request).await {
        Ok(_) => debug!(session, hook_type, "Hook pushed to daemon"),
        Err(e) => debug!(session, hook_type, error = %e, "Daemon push failed (sqlite fallback)"),
    }
}

/// Unread border colors—amber glow when the worker calls
const UNREAD_ACTIVE: &str = "#f67400";
const UNREAD_INACTIVE: &str = "#7a3a00";

/// Handle Stop hook—worker has finished speaking
///
/// Marks session as unread, sets state to Idle, and lights the ring (amber border).
pub async fn handle_stop(
    session: &str,
    kitty_id: Option<u64>,
    _transcript: Option<&str>,
) -> Result<()> {
    debug!(session, kitty_id, "Hook: Stop received");

    // Update state and read status in one transaction
    if let Ok(conn) = init_db() {
        // Set hook state to Idle (worker finished, awaiting input)
        if let Err(e) = set_hook_state(&conn, session, HookState::Idle) {
            warn!(session, error = %e, "Failed to set hook state to Idle");
        }

        // Mark session as unread
        if let Err(e) = mark_unread(&conn, session) {
            warn!(session, error = %e, "Failed to mark session unread");
        } else {
            info!(session, "Hook: Stop → Idle, unread");
        }
    }

    // Light the ring if we have a kitty window ID
    if let Some(id) = kitty_id {
        let socket = default_socket();
        if let Err(e) = set_border_color_on_socket(&socket, id, UNREAD_ACTIVE, UNREAD_INACTIVE).await {
            debug!(error = %e, "Failed to set unread border (ring)");
        } else {
            debug!(id, "Ring lit: amber");
        }
    }

    // Push to daemon memory immediately (bypasses poll lag)
    push_to_daemon(session, kitty_id, HookState::Idle, "stop").await;

    Ok(())
}

/// Handle UserPromptSubmit hook—the Captain speaks
///
/// Marks session as read, sets state to Working, and dims the ring (restore theme border).
pub async fn handle_prompt(
    session: &str,
    kitty_id: Option<u64>,
) -> Result<()> {
    debug!(session, kitty_id, "Hook: Prompt received");

    // Update state and read status in one transaction
    if let Ok(conn) = init_db() {
        // Set hook state to Working (user submitted, Claude processing)
        if let Err(e) = set_hook_state(&conn, session, HookState::Working) {
            warn!(session, error = %e, "Failed to set hook state to Working");
        }

        // Mark session as read
        if let Err(e) = mark_read(&conn, session) {
            warn!(session, error = %e, "Failed to mark session read");
        } else {
            info!(session, "Hook: Prompt → Working, read");
        }
    }

    // Dim the ring if we have a kitty window ID
    if let Some(id) = kitty_id {
        let socket = default_socket();
        if let Err(e) = reset_border_color_on_socket(&socket, id).await {
            debug!(error = %e, "Failed to reset border (ring)");
        } else {
            debug!(id, "Ring dimmed: theme default");
        }
    }

    // Push to daemon memory immediately (bypasses poll lag)
    push_to_daemon(session, kitty_id, HookState::Working, "prompt").await;

    Ok(())
}

/// Handle PreToolUse hook—tool execution begins
///
/// Sets state to ToolRunning for finer-grained activity tracking.
/// Logs tool invocations for telemetry.
pub async fn handle_pre_tool(
    session: &str,
    kitty_id: Option<u64>,
    tool_name: &str,
    _tool_input: Option<&str>,
) -> Result<()> {
    debug!(session, kitty_id, tool = tool_name, "Hook: PreToolUse received");

    if let Ok(conn) = init_db() {
        // Set hook state to ToolRunning
        if let Err(e) = set_hook_state(&conn, session, HookState::ToolRunning) {
            warn!(session, error = %e, "Failed to set hook state to ToolRunning");
        } else {
            debug!(session, tool = tool_name, "Hook: PreToolUse → ToolRunning");
        }
    }

    push_to_daemon(session, kitty_id, HookState::ToolRunning, "pre_tool").await;

    Ok(())
}

/// Handle PostToolUse hook—tool execution completed
///
/// Returns state to Working after tool completes.
pub async fn handle_post_tool(
    session: &str,
    kitty_id: Option<u64>,
    tool_name: &str,
    _tool_output: Option<&str>,
) -> Result<()> {
    debug!(session, kitty_id, tool = tool_name, "Hook: PostToolUse received");

    if let Ok(conn) = init_db() {
        // Return to Working state
        if let Err(e) = set_hook_state(&conn, session, HookState::Working) {
            warn!(session, error = %e, "Failed to set hook state to Working");
        } else {
            debug!(session, tool = tool_name, "Hook: PostToolUse → Working");
        }
    }

    push_to_daemon(session, kitty_id, HookState::Working, "post_tool").await;

    Ok(())
}

/// Handle Notification hook—system alerts
///
/// Logs notifications for visibility. Permission notifications may flash the ring.
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

    // Future: flash ring for permission notifications
    // if notif_type == "permission" { ... }

    Ok(())
}

/// Handle SessionStart hook—session begins or resumes
///
/// This is the most important hook for the daemon: it carries session_id + kitty_id,
/// enabling instant pane↔session binding without expensive fingerprint matching.
/// Before this hook existed, Phase 5 had to fetch full scrollback and compare
/// against 100 JSONL fingerprints to figure out which session a window belongs to.
pub async fn handle_session_start(
    session: &str,
    kitty_id: Option<u64>,
    cwd: &str,
    resumed: bool,
) -> Result<()> {
    info!(
        session,
        kitty_id,
        cwd,
        resumed,
        "Hook: SessionStart received"
    );

    // Push to daemon — this binds kitty_id ↔ session_id immediately,
    // bypassing the entire fingerprint matching pipeline (Phase 5)
    push_to_daemon(session, kitty_id, HookState::Working, "session_start").await;

    Ok(())
}

/// Handle SubagentStop hook—subagent finished
///
/// Logs when Task tool subagents complete their work.
pub async fn handle_subagent_stop(
    session: &str,
    kitty_id: Option<u64>,
    subagent_id: &str,
) -> Result<()> {
    debug!(
        session,
        kitty_id,
        subagent_id,
        "Hook: SubagentStop received"
    );

    Ok(())
}

/// Handle PreCompact hook—transcript compression imminent
///
/// Called before Claude Code compresses the conversation transcript.
/// Good opportunity to archive the full transcript if needed.
pub async fn handle_pre_compact(
    session: &str,
    kitty_id: Option<u64>,
    transcript_path: &str,
) -> Result<()> {
    info!(
        session,
        kitty_id,
        transcript_path,
        "Hook: PreCompact received—transcript compression imminent"
    );

    // Future: could archive transcript here before compression

    Ok(())
}

/// All 8 Claude Code hook events and their script names
const HOOK_SCRIPTS: &[(&str, &str)] = &[
    ("Stop", "on-stop"),
    ("UserPromptSubmit", "on-prompt"),
    ("PreToolUse", "on-tool-pre"),
    ("PostToolUse", "on-tool-post"),
    ("Notification", "on-notification"),
    ("SessionStart", "on-session-start"),
    ("SubagentStop", "on-subagent-stop"),
    ("PreCompact", "on-pre-compact"),
];

/// Install babel hooks into Claude Code settings
///
/// Modifies ~/.claude/settings.json to register all 8 hook handlers.
/// Hook scripts live in ~/.config/babel/hooks/ and are written in nu.
pub async fn install_hooks(dry_run: bool) -> Result<()> {
    use serde_json::{json, Map, Value};

    let settings_path = dirs::home_dir()
        .context("No home directory")?
        .join(".claude/settings.json");

    // Read existing settings
    let content = std::fs::read_to_string(&settings_path)
        .context("Failed to read ~/.claude/settings.json")?;

    let mut settings: Value = serde_json::from_str(&content)
        .context("Failed to parse settings.json")?;

    // Hook script paths
    let hooks_dir = dirs::config_dir()
        .context("No config directory")?
        .join("babel/hooks");

    // Verify all hook scripts exist
    let mut missing = Vec::new();
    for (_, script_name) in HOOK_SCRIPTS {
        let script_path = hooks_dir.join(script_name);
        if !script_path.exists() {
            missing.push(script_path);
        }
    }

    if !missing.is_empty() {
        let missing_list: Vec<_> = missing.iter().map(|p| p.display().to_string()).collect();
        anyhow::bail!(
            "Missing hook scripts ({}/{}):\n  {}",
            missing.len(),
            HOOK_SCRIPTS.len(),
            missing_list.join("\n  ")
        );
    }

    // Build hooks configuration for all 8 events
    let mut babel_hooks = Map::new();
    for (event_name, script_name) in HOOK_SCRIPTS {
        let script_path = hooks_dir.join(script_name);
        babel_hooks.insert(
            (*event_name).to_string(),
            json!([{
                "matcher": "",
                "hooks": [{
                    "type": "command",
                    "command": script_path.to_string_lossy()
                }]
            }]),
        );
    }

    // Merge hooks into settings
    if let Some(existing_hooks) = settings.get_mut("hooks") {
        if let Some(obj) = existing_hooks.as_object_mut() {
            // Merge our hooks with existing ones
            for (event, config) in babel_hooks {
                obj.insert(event, config);
            }
        }
    } else {
        settings["hooks"] = Value::Object(babel_hooks);
    }

    // Output
    let output = serde_json::to_string_pretty(&settings)?;

    if dry_run {
        println!("Would write to {}:\n", settings_path.display());
        println!("{}", output);
        println!("\n(dry run - no changes made)");
    } else {
        std::fs::write(&settings_path, &output)
            .context("Failed to write settings.json")?;

        println!("✓ Installed babel hooks to {}", settings_path.display());
        println!();
        println!("Hooks registered ({}):", HOOK_SCRIPTS.len());
        for (event_name, script_name) in HOOK_SCRIPTS {
            let script_path = hooks_dir.join(script_name);
            println!("  {} → {}", event_name, script_path.display());
        }
        println!();
        println!("⚠ Note: Review changes in Claude Code with /hooks command");
    }

    Ok(())
}
