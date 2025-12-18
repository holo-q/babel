//! Action commands - state-changing operations (focus, send, set-*, fire, etc.)
//!
//! These commands modify window state, send input, or update metadata.
//! All operations go through BabelCore which handles daemon/ephemeral modes transparently.

use std::path::Path;

use anyhow::{Context, Result};
use tracing::instrument;

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
#[instrument(level = "debug", skip(core, title))]
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

/// Focus a Claude window - by ID or interactive scrollparse-pager picker
///
/// With --content flag, enables searching window scrollback content.
/// Without it, searches window titles only (faster).
#[instrument(level = "debug", skip(core))]
pub async fn cmd_focus(core: &BabelCore, window_id: Option<u64>, content_mode: bool) -> Result<()> {
    // Direct focus if ID provided
    if let Some(id) = window_id {
        return focus_by_id(core, id).await;
    }

    // Get all Claude windows
    let windows = core.windows().await?;
    if windows.is_empty() {
        println!("No Claude windows found");
        return Ok(());
    }

    // Format windows for pager consumption
    let input = format_windows_for_pager(&windows, content_mode)?;

    // Launch scrollparse-pager in window selection mode
    let mut child = std::process::Command::new("scrollparse-pager")
        .args(["--window-select"])
        .args(if content_mode {
            vec!["--content-mode"]
        } else {
            vec![]
        })
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .spawn()
        .context("Failed to launch scrollparse-pager")?;

    // Write window data to pager stdin
    if let Some(mut stdin) = child.stdin.take() {
        use std::io::Write;
        stdin
            .write_all(input.as_bytes())
            .context("Failed to write to pager stdin")?;
    }

    // Wait for pager to complete and read selected ID
    let output = child
        .wait_with_output()
        .context("Failed to wait for pager")?;

    if !output.status.success() {
        // User cancelled (Esc) or pager error
        return Ok(());
    }

    let selected_id: u64 = String::from_utf8_lossy(&output.stdout)
        .trim()
        .parse()
        .context("Failed to parse selected window ID")?;

    focus_by_id(core, selected_id).await
}

/// Format ClaudeWindow list for scrollparse-pager consumption
///
/// Output format is JSONL with __window__ wrapper:
/// {"__window__": {"id": 42, "title": "...", "ws": 1, "cwd": "...", "focused": false}}
///
/// If include_content=true, also fetches scrollback for each window (slow).
fn format_windows_for_pager(
    windows: &[claude_babel::utility::claude_discovery::ClaudeWindow],
    include_content: bool,
) -> Result<String> {
    use serde_json::json;

    let mut lines = Vec::new();

    for win in windows {
        let mut obj = json!({
            "__window__": {
                "id": win.addr.id,
                "title": win.title,
                "ws": win.workspace,
                "cwd": win.cwd.display().to_string(),
                "focused": win.is_focused,
            }
        });

        // Include scrollback content if content mode (enables search)
        // NOTE: This fetches scrollback for EVERY window, making it slower.
        // Trade-off: slower startup vs. ability to search content.
        if include_content {
            if let Ok(scrollback) = win.scrollback() {
                obj["__window__"]["content"] = json!(scrollback);
            }
        }

        lines.push(serde_json::to_string(&obj)?);
    }

    Ok(lines.join("\n"))
}

/// Focus a window by its kitty ID (via BabelCore)
#[instrument(level = "debug", skip(core))]
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
///
/// Target can be a window ID or "." for current window.
/// "*" (all) is not supported - use a specific target.
#[instrument(level = "debug", skip(core))]
pub async fn cmd_get_scrollback(core: &BabelCore, target: &Target, lines: Option<usize>) -> Result<()> {
    let window_id = match target {
        Target::Window(id) => *id,
        Target::Current => {
            let (id, _socket) = super::current_pane_info()?;
            id
        }
        Target::All => {
            anyhow::bail!("Cannot get scrollback from all windows. Use a specific window ID or '.' for current.");
        }
    };

    let scrollback = core.scrollback(window_id, lines).await
        .context("Failed to get scrollback")?;
    print!("{}", scrollback);
    Ok(())
}

// ═══════════════════════════════════════════════════════════════════════════════
// Send / Type / Broadcast Text
// ═══════════════════════════════════════════════════════════════════════════════

/// Send text to window(s) with Enter (via BabelCore)
///
/// If any targeted window has pending (unsent) input, the operation is aborted
/// unless force=true. This prevents accidentally overwriting user's typed text.
///
/// TODO: When scrollparse improves, support save/restore of pending input:
/// - Save pending text before sending
/// - After send completes, restore the pending text
/// - This enables broadcast without losing in-progress inputs
#[instrument(level = "debug", skip(core, text))]
pub async fn cmd_send(core: &BabelCore, target: &Target, text: &str, force: bool) -> Result<()> {
    let window_ids = resolve_target(core, target).await?;

    if window_ids.is_empty() {
        println!("No Claude windows found");
        return Ok(());
    }

    // Check for pending input unless --force
    if !force {
        let windows_with_pending = check_pending_inputs(core, &window_ids).await?;
        if !windows_with_pending.is_empty() {
            println!("⚠ Aborted: {} window(s) have unsent text in the input area:", windows_with_pending.len());
            for (id, text) in &windows_with_pending {
                if let Some(t) = text {
                    println!("  Window {}: \"{}\"", id, truncate(t, 40));
                } else {
                    println!("  Window {}: (pending input detected)", id);
                }
            }
            println!("\nUse --force to send anyway (will overwrite pending input)");
            // TODO: Add --save-restore flag once scrollparse supports reliable input extraction
            // This would save pending text, send the new text, then restore the pending text
            return Ok(());
        }
    }

    for window_id in window_ids {
        core.send(window_id, text).await
            .with_context(|| format!("Failed to send text to window {}", window_id))?;
        println!("Sent to window {}", window_id);
    }
    Ok(())
}

/// Type text to window(s) without Enter (via BabelCore)
///
/// Types text into the input area without submitting. If any targeted window
/// has pending input, the operation is aborted unless force=true.
#[instrument(level = "debug", skip(core, text))]
pub async fn cmd_type(core: &BabelCore, target: &Target, text: &str, force: bool) -> Result<()> {
    let window_ids = resolve_target(core, target).await?;

    if window_ids.is_empty() {
        println!("No Claude windows found");
        return Ok(());
    }

    // Check for pending input unless --force
    if !force {
        let windows_with_pending = check_pending_inputs(core, &window_ids).await?;
        if !windows_with_pending.is_empty() {
            println!("⚠ Aborted: {} window(s) have unsent text in the input area:", windows_with_pending.len());
            for (id, text) in &windows_with_pending {
                if let Some(t) = text {
                    println!("  Window {}: \"{}\"", id, truncate(t, 40));
                } else {
                    println!("  Window {}: (pending input detected)", id);
                }
            }
            println!("\nUse --force to type anyway (will append to pending input)");
            return Ok(());
        }
    }

    for window_id in window_ids {
        core.type_text(window_id, text).await
            .with_context(|| format!("Failed to type text to window {}", window_id))?;
        println!("Typed to window {}", window_id);
    }
    Ok(())
}

/// Broadcast text to all Claude windows with Enter
///
/// This is a convenience wrapper around send with target=*.
/// If any window has pending input, the broadcast is aborted unless force=true.
///
/// TODO: Future enhancement with scrollparse integration:
/// - Capture pending input from all windows before broadcast
/// - Send the broadcast text
/// - Restore pending input to each window
/// - This enables safe broadcast without losing work-in-progress
#[instrument(level = "debug", skip(core, text))]
pub async fn cmd_broadcast(core: &BabelCore, text: &str, force: bool) -> Result<()> {
    let windows = core.windows().await?;

    if windows.is_empty() {
        println!("No Claude windows found");
        return Ok(());
    }

    let window_ids: Vec<u64> = windows.iter().map(|w| w.id()).collect();
    println!("Broadcasting to {} window(s)...", window_ids.len());

    // Check for pending input unless --force
    if !force {
        let windows_with_pending = check_pending_inputs(core, &window_ids).await?;
        if !windows_with_pending.is_empty() {
            println!("⚠ Aborted: {} window(s) have unsent text in the input area:", windows_with_pending.len());
            for (id, pending_text) in &windows_with_pending {
                let title = windows.iter()
                    .find(|w| w.id() == *id)
                    .map(|w| w.title.as_str())
                    .unwrap_or("Unknown");
                if let Some(t) = pending_text {
                    println!("  {} ({}): \"{}\"", id, truncate(title, 20), truncate(t, 30));
                } else {
                    println!("  {} ({}): (pending input detected)", id, truncate(title, 20));
                }
            }
            println!("\nUse --force to broadcast anyway (will overwrite pending input)");
            return Ok(());
        }
    }

    let mut success_count = 0;
    let mut fail_count = 0;

    for window_id in window_ids {
        match core.send(window_id, text).await {
            Ok(()) => {
                success_count += 1;
            }
            Err(e) => {
                eprintln!("  Failed window {}: {}", window_id, e);
                fail_count += 1;
            }
        }
    }

    println!("Broadcast complete: {} succeeded, {} failed", success_count, fail_count);
    Ok(())
}

/// Check which windows have pending input
///
/// Returns a list of (window_id, pending_text) for windows with unsent input.
#[instrument(level = "debug", skip(core))]
async fn check_pending_inputs(core: &BabelCore, window_ids: &[u64]) -> Result<Vec<(u64, Option<String>)>> {
    let mut pending = Vec::new();

    for &window_id in window_ids {
        match core.has_pending_input(window_id).await {
            Ok((true, text)) => pending.push((window_id, text)),
            Ok((false, _)) => {}
            Err(e) => {
                // Log but don't fail - we'll allow the operation if we can't check
                tracing::debug!(window_id, error = %e, "Failed to check pending input");
            }
        }
    }

    Ok(pending)
}

/// Truncate a string for display
fn truncate(s: &str, max_len: usize) -> String {
    if s.len() <= max_len {
        s.to_string()
    } else {
        format!("{}…", &s[..max_len.saturating_sub(1)])
    }
}

// ═══════════════════════════════════════════════════════════════════════════════
// Metadata Management
// ═══════════════════════════════════════════════════════════════════════════════

/// Set icon (emoji tag) for window(s) (via BabelCore)
#[instrument(level = "debug", skip(core))]
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
#[instrument(level = "debug", skip(core))]
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
#[instrument(level = "debug", skip(core, prompt))]
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
#[instrument(level = "debug")]
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
#[instrument(level = "debug")]
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
#[instrument(level = "debug", skip(filter))]
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

// ═══════════════════════════════════════════════════════════════════════════════
// Interactive Window Selection
// ═══════════════════════════════════════════════════════════════════════════════

/// Point-and-click window selection via slop
///
/// Uses `slop` to let the user click any X11 window, then maps the X11 window ID
/// to kitty pane IDs. Works locally (no daemon needed) since slop requires X11 display.
///
/// Output:
/// - Plain: space-separated pane IDs (for shell command substitution)
/// - JSON: array of {id, title, socket} objects
#[instrument(level = "debug")]
pub async fn cmd_target(json: bool) -> Result<()> {
    use std::process::Command;
    use claude_babel::kitty::get_panes_by_platform_id;

    // Run slop to get X11 window ID from user click
    // -t 999999: force window selection (not region), disable drag tolerance
    // -f '%i': output only the window ID
    eprintln!("Click a kitty window to select...");

    let output = Command::new("slop")
        .args(["-t", "999999", "-f", "%i"])
        .output()
        .context("Failed to run slop. Is it installed? (pacman -S slop)")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        if stderr.contains("cancelled") || output.status.code() == Some(1) {
            eprintln!("Selection cancelled");
            return Ok(());
        }
        anyhow::bail!("slop failed: {}", stderr);
    }

    let x11_window_id: u64 = String::from_utf8_lossy(&output.stdout)
        .trim()
        .parse()
        .context("Failed to parse X11 window ID from slop")?;

    // Map X11 window ID to kitty panes
    let panes = get_panes_by_platform_id(x11_window_id)?;

    if panes.is_empty() {
        eprintln!("No kitty panes found in window {}", x11_window_id);
        eprintln!("(The clicked window may not be a kitty terminal)");
        return Ok(());
    }

    if json {
        // JSON output: array of pane info
        let output: Vec<_> = panes.iter().map(|p| serde_json::json!({
            "id": p.id,
            "title": p.title,
            "socket": p.socket,
            "cwd": p.cwd,
        })).collect();
        println!("{}", serde_json::to_string_pretty(&output)?);
    } else {
        // Plain output: space-separated IDs for command substitution
        // Example: `babel send $(babel target) "hello"`
        let ids: Vec<String> = panes.iter().map(|p| p.id.to_string()).collect();
        println!("{}", ids.join(" "));

        // Info on stderr so it doesn't interfere with command substitution
        if panes.len() == 1 {
            eprintln!("Selected pane {} ({})", panes[0].id, truncate(&panes[0].title, 40));
        } else {
            eprintln!("Selected {} panes:", panes.len());
            for p in &panes {
                eprintln!("  {} - {}", p.id, truncate(&p.title, 40));
            }
        }
    }

    Ok(())
}
