//! Action commands - state-changing operations (focus, send, set-*, etc.)
//!
//! These commands modify window state, send input, or update metadata.
//! All commands support both daemon-first (fast) and direct fallback modes.

use anyhow::{Context, Result};

use claude_babel::discovery::{discover_claude_windows, enrich_window};
use claude_babel::ipc::{send_request, Request, Response};
use claude_babel::kitty::{focus_window, get_scrollback, send_text, set_window_title};
use claude_babel::overlay::{init_db, mark_read, set_icon};

use super::{resolve_target, Target};

// ═══════════════════════════════════════════════════════════════════════════════
// Title Management
// ═══════════════════════════════════════════════════════════════════════════════

/// Set window title(s) - custom or auto-determined from session
///
/// With a title argument, sets that exact title on the target window(s).
/// Without a title argument, auto-determines the title from the session summary
/// (the "✳ Summary" format that Claude Code uses).
pub async fn cmd_set_title(target: &Target, title: Option<&str>) -> Result<()> {
    let window_ids = resolve_target(target).await?;

    if window_ids.is_empty() {
        println!("No Claude windows found");
        return Ok(());
    }

    let mut windows = discover_claude_windows()?;

    for window_id in window_ids {
        let window = windows.iter_mut().find(|w| w.kitty_id == window_id);

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

        // Set the title via kitty remote control
        set_window_title(window_id, &new_title)
            .with_context(|| format!("Failed to set title for window {}", window_id))?;

        println!("Set title for window {}: {}", window_id, new_title);
    }

    Ok(())
}

// ═══════════════════════════════════════════════════════════════════════════════
// Window Focus
// ═══════════════════════════════════════════════════════════════════════════════

/// Focus a Claude window - by ID or interactive rofi picker
pub async fn cmd_focus(window_id: Option<u64>) -> Result<()> {
    // Direct focus if ID provided
    if let Some(id) = window_id {
        return focus_by_id(id).await;
    }

    // Interactive picker via rofi
    let windows = super::query::get_windows().await?;
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
            .then(a.kitty_id.cmp(&b.kitty_id))
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
        (win.kitty_id, label)
    }).collect();

    // Launch rofi
    let labels: Vec<&str> = entries.iter().map(|(_, l)| l.as_str()).collect();

    match rofi::Rofi::new(&labels).prompt("Claude").run() {
        Ok(choice) => {
            if let Some((id, _)) = entries.iter().find(|(_, l)| l == &choice) {
                focus_by_id(*id).await?;
            }
        }
        Err(rofi::Error::Interrupted) => {} // User cancelled (Esc)
        Err(e) => anyhow::bail!("Rofi error: {}", e),
    }

    Ok(())
}

/// Focus a window by its kitty ID (via daemon or direct)
async fn focus_by_id(window_id: u64) -> Result<()> {
    // Try daemon first
    if let Ok(Response::Ok { message }) = send_request(&Request::Focus { window_id }).await {
        println!("{}", message);
        return Ok(());
    }

    // Direct fallback
    focus_window(window_id).context("Failed to focus window")?;
    println!("Focused window {}", window_id);
    Ok(())
}

// ═══════════════════════════════════════════════════════════════════════════════
// Scrollback Retrieval
// ═══════════════════════════════════════════════════════════════════════════════

/// Get scrollback from a window (via daemon or direct)
pub async fn cmd_get_scrollback(window_id: u64, lines: Option<usize>) -> Result<()> {
    // Try daemon first
    if let Ok(Response::Scrollback { text }) = send_request(&Request::Scroll { window_id }).await {
        let output = if let Some(n) = lines {
            text.lines().take(n).collect::<Vec<_>>().join("\n")
        } else {
            text
        };
        print!("{}", output);
        return Ok(());
    }

    // Direct fallback
    let scrollback = get_scrollback(window_id).context("Failed to get scrollback")?;
    let output = if let Some(n) = lines {
        scrollback.lines().take(n).collect::<Vec<_>>().join("\n")
    } else {
        scrollback
    };
    print!("{}", output);
    Ok(())
}

// ═══════════════════════════════════════════════════════════════════════════════
// Send Text
// ═══════════════════════════════════════════════════════════════════════════════

/// Send text to window(s) (via daemon or direct)
pub async fn cmd_send(target: &Target, text: &str) -> Result<()> {
    let window_ids = resolve_target(target).await?;

    if window_ids.is_empty() {
        println!("No Claude windows found");
        return Ok(());
    }

    for window_id in window_ids {
        // Try daemon first
        if let Ok(Response::Ok { message }) = send_request(&Request::Send {
            window_id,
            text: text.to_string(),
        }).await {
            println!("{}", message);
            continue;
        }

        // Direct fallback
        send_text(window_id, text).context("Failed to send text")?;
        println!("Sent text to window {}", window_id);
    }
    Ok(())
}

// ═══════════════════════════════════════════════════════════════════════════════
// Metadata Management
// ═══════════════════════════════════════════════════════════════════════════════

/// Set icon (emoji tag) for window(s) (via daemon or direct)
pub async fn cmd_set_icon(target: &Target, icon: &str) -> Result<()> {
    let window_ids = resolve_target(target).await?;

    if window_ids.is_empty() {
        println!("No Claude windows found");
        return Ok(());
    }

    let windows = discover_claude_windows()?;
    let conn = init_db()?;

    for window_id in window_ids {
        // Try daemon first
        if let Ok(Response::Ok { message }) = send_request(&Request::Tag {
            window_id,
            icon: icon.to_string(),
        }).await {
            println!("{}", message);
            continue;
        }

        // Direct fallback - need to find session first
        let window = windows
            .iter()
            .find(|w| w.kitty_id == window_id)
            .context("Window not found")?;

        let session_id = window.session_id.as_ref().context("Window has no session")?;
        set_icon(&conn, session_id, icon)?;
        println!("Set icon for window {}: {}", window_id, icon);
    }
    Ok(())
}

/// Mark window(s) as read (via daemon or direct)
pub async fn cmd_set_read(target: &Target) -> Result<()> {
    let window_ids = resolve_target(target).await?;

    if window_ids.is_empty() {
        println!("No Claude windows found");
        return Ok(());
    }

    let windows = discover_claude_windows()?;
    let conn = init_db()?;

    for window_id in window_ids {
        // Try daemon first
        if let Ok(Response::Ok { message }) = send_request(&Request::MarkRead { window_id }).await {
            println!("{}", message);
            continue;
        }

        // Direct fallback
        let window = windows
            .iter()
            .find(|w| w.kitty_id == window_id)
            .context("Window not found")?;

        let session_id = window.session_id.as_ref().context("Window has no session")?;
        mark_read(&conn, session_id)?;
        println!("Marked window {} as read", window_id);
    }
    Ok(())
}
