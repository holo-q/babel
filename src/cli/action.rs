//! Action commands - state-changing operations (focus, send, set-*, fire, etc.)
//!
//! These commands modify window state, send input, or update metadata.
//! All operations go through BabelCore which handles daemon/ephemeral modes transparently.

use std::path::Path;

use anyhow::{Context, Result};

use claude_babel::core::BabelCore;
use claude_babel::utility::claude_discovery::enrich_window;

use super::{Target, resolve_target};

// ═══════════════════════════════════════════════════════════════════════════════
// Title Management
// ═══════════════════════════════════════════════════════════════════════════════

/// Set window title(s) - custom or auto-determined from session
///
/// With a title argument, sets that exact title on the target window(s).
/// Without a title argument, auto-determines the title from the session summary
/// (the "✳ Summary" format that Claude Code uses).
pub async fn cmd_set_title(core: &BabelCore, target: &Target, title: Option<&str>) -> Result<()> {
    let window_ids = resolve_target(core, target).await?;

    if window_ids.is_empty() {
        println!("No Claude windows found");
        return Ok(());
    }

    let mut windows = core.windows().await?;

    for window_id in window_ids {
        let window = windows.iter_mut().find(|w| w.id() == window_id);

        let new_title = if let Some(custom) = title {
            // Use custom title as-is
            custom.to_string()
        } else {
            // Auto-determine from session
            if let Some(win) = window {
                // Enrich to get session info if not already loaded
                let _ = enrich_window(win);

                if let Some(ref info) = win.session_info {
                    // Use first summary from session
                    info.summaries.first()
                        .map(|s| format!("✳ {}", s.summary))
                        .unwrap_or_else(|| win.title.clone())
                } else {
                    // No session info - keep existing title
                    continue;
                }
            } else {
                println!("Window {} not found", window_id);
                continue;
            }
        };

        // Set the title via BabelCore
        core.set_title(window_id, &new_title).await
            .with_context(|| format!("Failed to set title for window {}", window_id))?;

        println!("Set title for window {}: {}", window_id, new_title);
    }

    Ok(())
}

// ═══════════════════════════════════════════════════════════════════════════════
// Window Focus
// ═══════════════════════════════════════════════════════════════════════════════

/// Focus a Claude window - by ID or interactive rofi picker
pub async fn cmd_focus(core: &BabelCore, window_id: Option<u64>) -> Result<()> {
    // Direct focus if ID provided
    if let Some(id) = window_id {
        return focus_by_id(core, id).await;
    }

    // Interactive picker via rofi
    let windows = core.windows().await?;
    if windows.is_empty() {
        println!("No Claude sessions found");
        return Ok(());
    }

    // Sort by workspace for consistent ordering
    let mut windows = windows;
    windows.sort_by(|a, b| {
        let ws_a = a.workspace.unwrap_or(999);
        let ws_b = b.workspace.unwrap_or(999);
        ws_a.cmp(&ws_b)
            .then(a.os_window_id.cmp(&b.os_window_id))
            .then(a.id().cmp(&b.id()))
    });

    // Format entries for rofi: "[ws] title │ ~/path"
    let entries: Vec<(u64, String)> = windows.iter().map(|win| {
        let ws = match win.workspace {
            Some(-1) => "S".to_string(),  // Sticky
            Some(n) => format!("{}", n + 1),
            None => "?".to_string(),
        };

        // Strip ✳ prefix from active sessions
        let title = win.title.strip_prefix("✳ ").unwrap_or(&win.title);
        let title_short: String = title.chars().take(40).collect();
        let title_display = if title.len() > 40 {
            format!("{}…", title_short)
        } else {
            title_short
        };

        // Compact cwd
        let cwd = win.cwd
            .strip_prefix(dirs::home_dir().unwrap_or_default())
            .map(|p| format!("~/{}", p.display()))
            .unwrap_or_else(|_| win.cwd.display().to_string());

        let label = format!("[{}] {} │ {}", ws, title_display, cwd);
        (win.id(), label)
    }).collect();

    // Launch rofi
    let labels: Vec<&str> = entries.iter().map(|(_, l)| l.as_str()).collect();

    match rofi::Rofi::new(&labels).prompt("Claude").run() {
        Ok(choice) => {
            if let Some((id, _)) = entries.iter().find(|(_, l)| l == &choice) {
                focus_by_id(core, *id).await?;
            }
        }
        Err(rofi::Error::Interrupted) => {} // User cancelled (Esc)
        Err(e) => anyhow::bail!("Rofi error: {}", e),
    }

    Ok(())
}

/// Focus a window by its kitty ID (via BabelCore)
async fn focus_by_id(core: &BabelCore, window_id: u64) -> Result<()> {
    core.focus(window_id).await
        .context("Failed to focus window")?;
    println!("Focused window {}", window_id);
    Ok(())
}

// ═══════════════════════════════════════════════════════════════════════════════
// Scrollback Retrieval
// ═══════════════════════════════════════════════════════════════════════════════

/// Get scrollback from a window (via BabelCore)
pub async fn cmd_get_scrollback(core: &BabelCore, window_id: u64, lines: Option<usize>) -> Result<()> {
    let scrollback = core.scrollback(window_id, lines).await
        .context("Failed to get scrollback")?;
    print!("{}", scrollback);
    Ok(())
}

// ═══════════════════════════════════════════════════════════════════════════════
// Send Text
// ═══════════════════════════════════════════════════════════════════════════════

/// Send text to window(s) (via BabelCore)
pub async fn cmd_send(core: &BabelCore, target: &Target, text: &str) -> Result<()> {
    let window_ids = resolve_target(core, target).await?;

    if window_ids.is_empty() {
        println!("No Claude windows found");
        return Ok(());
    }

    for window_id in window_ids {
        core.send(window_id, text).await
            .with_context(|| format!("Failed to send text to window {}", window_id))?;
        println!("Sent text to window {}", window_id);
    }
    Ok(())
}

// ═══════════════════════════════════════════════════════════════════════════════
// Metadata Management
// ═══════════════════════════════════════════════════════════════════════════════

/// Set icon (emoji tag) for window(s) (via BabelCore)
pub async fn cmd_set_icon(core: &BabelCore, target: &Target, icon: &str) -> Result<()> {
    let window_ids = resolve_target(core, target).await?;

    if window_ids.is_empty() {
        println!("No Claude windows found");
        return Ok(());
    }

    for window_id in window_ids {
        core.set_icon(window_id, icon).await
            .with_context(|| format!("Failed to set icon for window {}", window_id))?;
        println!("Set icon for window {}: {}", window_id, icon);
    }
    Ok(())
}

/// Mark window(s) as read (via BabelCore)
pub async fn cmd_set_read(core: &BabelCore, target: &Target) -> Result<()> {
    let window_ids = resolve_target(core, target).await?;

    if window_ids.is_empty() {
        println!("No Claude windows found");
        return Ok(());
    }

    for window_id in window_ids {
        core.mark_read(window_id).await
            .with_context(|| format!("Failed to mark window {} as read", window_id))?;
        println!("Marked window {} as read", window_id);
    }
    Ok(())
}

// ═══════════════════════════════════════════════════════════════════════════════
// Fire-and-Forget Sessions
// ═══════════════════════════════════════════════════════════════════════════════

/// Fire a prompt to Claude in a detached background session
pub async fn cmd_fire(
    core: &mut BabelCore,
    prompt: &str,
    workdir: Option<&Path>,
    ambient: Option<String>,
) -> Result<()> {
    let task = core.fire(prompt, workdir, ambient).await
        .context("Failed to fire Claude session")?;

    println!("⚡ Fired: {}", task.prompt_preview);
    println!("   Task ID: {}", task.task_id);
    println!("   PID: {}", task.pid);
    println!("   Workdir: {}", task.workdir.display());
    if let Some(ref sound) = task.ambient_sound {
        println!("   Ambient: {}", sound);
    }

    Ok(())
}

/// List running fire-and-forget tasks
pub fn cmd_fire_ls(json: bool) -> Result<()> {
    let tasks = BabelCore::fired_tasks()
        .context("Failed to list fired tasks")?;

    if tasks.is_empty() {
        if !json {
            println!("No running fire tasks");
        } else {
            println!("[]");
        }
        return Ok(());
    }

    if json {
        println!("{}", serde_json::to_string_pretty(&tasks)?);
    } else {
        println!("Running fire tasks:\n");
        for task in &tasks {
            let alive = if task.is_alive() { "🟢" } else { "⚫" };
            println!("{} [{}] PID {} - {}", alive, task.task_id, task.pid, task.prompt_preview);
            println!("     {}", task.workdir.display());
            if let Some(ref sound) = task.ambient_sound {
                println!("     🔊 {}", sound);
            }
        }
    }

    Ok(())
}

/// Clean up finished fire tasks
pub fn cmd_fire_clean() -> Result<()> {
    let cleaned = BabelCore::cleanup_fired()
        .context("Failed to clean up fired tasks")?;

    if cleaned == 0 {
        println!("No finished tasks to clean up");
    } else {
        println!("Cleaned up {} finished task(s)", cleaned);
    }

    Ok(())
}

// ═══════════════════════════════════════════════════════════════════════════════
// Event Monitor
// ═══════════════════════════════════════════════════════════════════════════════

/// Stream daemon events to stdout as JSON lines
///
/// Subscribes to daemon events and prints each one as a JSON line.
/// Connection stays open until Ctrl+C or daemon shutdown.
pub async fn cmd_monitor(filter: Vec<String>) -> Result<()> {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    use tokio::net::UnixStream;
    use claude_babel::utility::ipc::{socket_path, Request, Response};

    let sock_path = socket_path();

    eprintln!("Connecting to daemon at {}...", sock_path.display());

    let mut stream = UnixStream::connect(&sock_path)
        .await
        .with_context(|| format!("Failed to connect to daemon at {}", sock_path.display()))?;

    // Send Subscribe request
    let request = Request::Subscribe { events: filter.clone() };
    let mut request_json = serde_json::to_string(&request)?;
    request_json.push('\n');
    stream.write_all(request_json.as_bytes()).await?;

    // Read subscription acknowledgment
    let (reader, writer) = stream.into_split();
    let mut reader = BufReader::new(reader);
    let mut line = String::new();
    reader.read_line(&mut line).await?;

    let response: Response = serde_json::from_str(&line)
        .context("Failed to parse subscription response")?;

    match response {
        Response::Subscribed { subscriber_id } => {
            if filter.is_empty() {
                eprintln!("Subscribed (id: {}) - streaming all events...", subscriber_id);
            } else {
                eprintln!("Subscribed (id: {}) - filtering: {:?}", subscriber_id, filter);
            }
            eprintln!("Press Ctrl+C to stop\n");
        }
        Response::Error { message } => {
            anyhow::bail!("Subscription failed: {}", message);
        }
        other => {
            anyhow::bail!("Unexpected response: {:?}", other);
        }
    }

    // Stream events until connection closes
    loop {
        line.clear();
        let bytes_read = reader.read_line(&mut line).await?;

        if bytes_read == 0 {
            eprintln!("\nConnection closed by daemon");
            break;
        }

        // Parse and re-serialize for pretty printing if it's an event
        match serde_json::from_str::<Response>(&line) {
            Ok(Response::Event { event }) => {
                // Print just the event as a JSON line
                println!("{}", serde_json::to_string(&event)?);
            }
            Ok(other) => {
                // Print other responses as-is
                println!("{}", serde_json::to_string(&other)?);
            }
            Err(e) => {
                eprintln!("Parse error: {} - raw: {}", e, line.trim());
            }
        }
    }

    // Explicit drop to avoid unused warning
    drop(writer);

    Ok(())
}
