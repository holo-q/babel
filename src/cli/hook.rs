//! Hook handlers—the neural interface between Claude Code and Babel
//!
//! These commands are called by Claude Code hooks to signal lifecycle events.
//! They receive session_id directly from Claude Code, bypassing fingerprint matching.
//!
//! The hooks are the direct neural link: Claude Code whispers, Babel listens.

use anyhow::{Result, Context};
use tracing::{info, debug, warn};

use claude_babel::babel_storage::{init_db, mark_read, mark_unread};
use claude_babel::kitty::{
    set_border_color_on_socket, reset_border_color_on_socket, default_socket,
};

/// Unread border colors—amber glow when the worker calls
const UNREAD_ACTIVE: &str = "#f67400";
const UNREAD_INACTIVE: &str = "#7a3a00";

/// Handle Stop hook—worker has finished speaking
///
/// Marks session as unread and lights the ring (amber border).
pub async fn handle_stop(
    session: &str,
    kitty_id: Option<u64>,
    _transcript: Option<&str>,
) -> Result<()> {
    debug!(session, kitty_id, "Hook: Stop received");

    // Mark session as unread in the database
    if let Err(e) = init_db().and_then(|conn| mark_unread(&conn, session)) {
        warn!(session, error = %e, "Failed to mark session unread");
    } else {
        info!(session, "Marked unread via hook");
    }

    // Light the ring if we have a kitty window ID
    if let Some(id) = kitty_id {
        let socket = default_socket();
        if let Err(e) = set_border_color_on_socket(&socket, id, UNREAD_ACTIVE, UNREAD_INACTIVE) {
            debug!(error = %e, "Failed to set unread border (ring)");
        } else {
            debug!(id, "Ring lit: amber");
        }
    }

    Ok(())
}

/// Handle UserPromptSubmit hook—the Captain speaks
///
/// Marks session as read and dims the ring (restore theme border).
pub async fn handle_prompt(
    session: &str,
    kitty_id: Option<u64>,
) -> Result<()> {
    debug!(session, kitty_id, "Hook: Prompt received");

    // Mark session as read in the database
    if let Err(e) = init_db().and_then(|conn| mark_read(&conn, session)) {
        warn!(session, error = %e, "Failed to mark session read");
    } else {
        info!(session, "Marked read via hook");
    }

    // Dim the ring if we have a kitty window ID
    if let Some(id) = kitty_id {
        let socket = default_socket();
        if let Err(e) = reset_border_color_on_socket(&socket, id) {
            debug!(error = %e, "Failed to reset border (ring)");
        } else {
            debug!(id, "Ring dimmed: theme default");
        }
    }

    Ok(())
}

/// Install babel hooks into Claude Code settings
///
/// Modifies ~/.claude/settings.json to register our hook handlers.
pub async fn install_hooks(dry_run: bool) -> Result<()> {
    use serde_json::{json, Value};

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

    let on_stop = hooks_dir.join("on-stop");
    let on_prompt = hooks_dir.join("on-prompt");

    // Verify hook scripts exist
    if !on_stop.exists() {
        anyhow::bail!("Hook script not found: {}", on_stop.display());
    }
    if !on_prompt.exists() {
        anyhow::bail!("Hook script not found: {}", on_prompt.display());
    }

    // Build hooks configuration
    let babel_hooks = json!({
        "Stop": [
            {
                "matcher": "",
                "hooks": [
                    {
                        "type": "command",
                        "command": on_stop.to_string_lossy()
                    }
                ]
            }
        ],
        "UserPromptSubmit": [
            {
                "matcher": "",
                "hooks": [
                    {
                        "type": "command",
                        "command": on_prompt.to_string_lossy()
                    }
                ]
            }
        ]
    });

    // Merge hooks into settings
    if let Some(existing_hooks) = settings.get_mut("hooks") {
        if let Some(obj) = existing_hooks.as_object_mut() {
            // Merge our hooks with existing ones
            for (event, config) in babel_hooks.as_object().unwrap() {
                obj.insert(event.clone(), config.clone());
            }
        }
    } else {
        settings["hooks"] = babel_hooks;
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
        println!("Hooks registered:");
        println!("  Stop → {}", on_stop.display());
        println!("  UserPromptSubmit → {}", on_prompt.display());
        println!();
        println!("⚠ Note: Review changes in Claude Code with /hooks command");
    }

    Ok(())
}
