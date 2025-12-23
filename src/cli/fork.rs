//! Fork and Tail commands - meta-cognitive session spawning
//!
//! - `babel tail [target]` - Output recent transcript from a session
//! - `babel fork [target]` - Launch Claude primed with another session's context
//!
//! The fork command enables "2nd degree mode" - a Claude session that's aware of
//! and can reflect on another session's work. This is particularly useful for:
//! - Continuing interrupted work with fresh context
//! - Getting a second opinion on an approach
//! - Debugging by observing what another Claude did
//! - Meta-analysis of patterns and decisions

use std::path::{Path, PathBuf};

use anyhow::{anyhow, Context, Result};
use tracing::instrument;

use claude_babel::core::BabelCore;
use claude_babel::pager::parse_transcript;
use claude_babel::utility::claude_storage::{
    find_session_transcript, get_recent_sessions, get_session_info,
};
use scrollparse::MessageKind;

// ═══════════════════════════════════════════════════════════════════════════════
// Target Resolution
// ═══════════════════════════════════════════════════════════════════════════════

/// Resolve a fork target to a session ID
///
/// Target can be:
/// - "." → most recent session in current directory
/// - Window ID (number) → session from that window
/// - Session ID (uuid-like) → direct session reference
/// - Path → most recent session in that directory
async fn resolve_fork_target(core: &BabelCore, target: &str) -> Result<String> {
    // Case 1: Current directory "."
    if target == "." {
        return find_session_for_cwd(&std::env::current_dir()?).await;
    }

    // Case 2: Window ID (pure number)
    if let Ok(window_id) = target.parse::<u64>() {
        return find_session_for_window(core, window_id).await;
    }

    // Case 3: Looks like a session ID (uuid-ish: 8+ hex chars with dashes)
    if target.len() >= 8 && target.chars().all(|c| c.is_ascii_hexdigit() || c == '-') {
        // Verify it exists
        if find_session_transcript(target)?.is_some() {
            return Ok(target.to_string());
        }
        return Err(anyhow!("Session not found: {}", target));
    }

    // Case 4: Path-like → treat as directory
    let path = PathBuf::from(target);
    if path.exists() && path.is_dir() {
        return find_session_for_cwd(&path).await;
    }

    Err(anyhow!(
        "Could not resolve target '{}': not a window ID, session ID, or directory",
        target
    ))
}

/// Find most recent session for a directory
async fn find_session_for_cwd(cwd: &Path) -> Result<String> {
    let sessions = get_recent_sessions(50)?;

    for session in sessions {
        // Match if:
        // - cwd is within session's project (e.g., cwd=/foo/bar/baz, project=/foo/bar)
        // - session's project is within cwd (e.g., project=/foo/bar/baz, cwd=/foo/bar)
        // - session's cwd matches either direction
        let project_match = cwd.starts_with(&session.project) || session.project.starts_with(cwd);
        let cwd_match = session
            .cwd
            .as_ref()
            .map(|c| cwd.starts_with(c) || c.starts_with(cwd))
            .unwrap_or(false);

        if project_match || cwd_match {
            return Ok(session.session_id);
        }
    }

    Err(anyhow!(
        "No sessions found for directory: {}",
        cwd.display()
    ))
}

/// Find session ID from a window
async fn find_session_for_window(core: &BabelCore, window_id: u64) -> Result<String> {
    let windows = core.windows().await?;

    for window in windows {
        if window.id() == window_id {
            if let Some(session_id) = &window.session_id {
                return Ok(session_id.clone());
            }
            return Err(anyhow!("Window {} has no session ID", window_id));
        }
    }

    Err(anyhow!("Window {} not found", window_id))
}

// ═══════════════════════════════════════════════════════════════════════════════
// Tail Command
// ═══════════════════════════════════════════════════════════════════════════════

/// Output recent transcript from a session
///
/// The output format is designed to be both human-readable and machine-parseable,
/// with clear role prefixes (> for user, ● for assistant, etc.)
#[instrument(level = "debug", skip(core))]
pub async fn cmd_tail(
    core: &BabelCore,
    target: &str,
    lines: usize,
    json: bool,
) -> Result<()> {
    let session_id = resolve_fork_target(core, target).await?;
    let transcript_path = find_session_transcript(&session_id)?
        .ok_or_else(|| anyhow!("Transcript not found for session: {}", session_id))?;

    let messages = parse_transcript(&transcript_path)?;

    if json {
        // JSON output
        let output: Vec<_> = messages
            .iter()
            .rev()
            .take(lines)
            .rev()
            .map(|m| {
                serde_json::json!({
                    "kind": format!("{:?}", m.kind),
                    "content": m.content,
                })
            })
            .collect();
        println!("{}", serde_json::to_string_pretty(&output)?);
    } else {
        // Human-readable output with role prefixes
        let tail_messages: Vec<_> = messages.iter().rev().take(lines).rev().collect();

        // Print header with session info
        eprintln!("─── Session {} ───", &session_id[..8.min(session_id.len())]);

        for msg in tail_messages {
            let (prefix, content) = match &msg.kind {
                MessageKind::User => ("> ", &msg.content),
                MessageKind::Assistant => ("● ", &msg.content),
                MessageKind::ToolCall { name, args } => {
                    println!("▸ {}({})", name, truncate(args.as_str(), 50));
                    continue;
                }
                MessageKind::ToolOutput => ("  ⎿ ", &msg.content),
                MessageKind::Status => ("  ", &msg.content),
            };

            // Print with role prefix, handling multiline content
            for (i, line) in content.lines().enumerate() {
                if i == 0 {
                    println!("{}{}", prefix, line);
                } else {
                    println!("  {}", line);
                }
            }
        }
    }

    Ok(())
}

// ═══════════════════════════════════════════════════════════════════════════════
// Fork Command
// ═══════════════════════════════════════════════════════════════════════════════

/// The reflection prompt for 2nd degree mode
///
/// This primes Claude to introspect on another session's work, recognizing patterns
/// and being ready to continue, critique, or fork the approach.
///
/// The forked Claude knows:
/// - It was forked from another session (bootstrap circuit)
/// - The exact command used (`babel fork`)
/// - How to explore further (`babel tail`, `babel ls`)
/// - It has fresh eyes and should leverage that perspective
const FORK_PROMPT: &str = r#"**🔀 FORKED SESSION**

You were spawned via `babel fork` to continue or reflect on another Claude's work.
This is 2nd-degree mode: you have fresh context and can see patterns the working Claude may have missed.

<source-session>
{TRANSCRIPT}
</source-session>

**Bootstrap Protocol:**

1. **Grok** - Parse the context. What was being built? What's the current state? Any blockers?
2. **Reflect** - With fresh eyes, what patterns or issues do you notice? Any better approaches?
3. **Act** - Either continue the work seamlessly, or propose a fork in direction

**Your Toolbelt:**
```
babel tail .              # More context from current directory
babel tail <session-id>   # Tail a specific session
babel tail <window-id>    # Tail from a running pane
babel ls                  # List active Claude sessions
babel history             # Browse conversation history
```

You are not starting from scratch—you're picking up the thread. The source Claude may still be running; you're a parallel perspective. If the source asked you to fork for a specific task, focus on that. Otherwise, reflect on what you see and propose next steps.

What's the situation?"#;

/// Fork from another session with full context injection
///
/// Launches a new Claude session primed with:
/// - The transcript from the source session
/// - The reflection prompt for 2nd degree mode
/// - Clear annotation of babel commands for further exploration
///
/// Location can be: "hsplit", "vsplit", "tab", or "os-window"
#[instrument(level = "debug", skip(core))]
pub async fn cmd_fork(
    core: &BabelCore,
    target: &str,
    lines: usize,
    location: &str,
) -> Result<()> {
    let session_id = resolve_fork_target(core, target).await?;
    let transcript_path = find_session_transcript(&session_id)?
        .ok_or_else(|| anyhow!("Transcript not found for session: {}", session_id))?;

    // Get session info for working directory
    let session_info = get_session_info(&transcript_path)?;
    let cwd = session_info
        .cwd
        .unwrap_or_else(|| session_info.project.clone());

    // Parse and format transcript
    let messages = parse_transcript(&transcript_path)?;
    let tail_messages: Vec<_> = messages.iter().rev().take(lines).rev().collect();

    let mut transcript_text = String::new();
    transcript_text.push_str(&format!(
        "# babel tail {} (last {} messages)\n\n",
        &session_id[..8.min(session_id.len())],
        tail_messages.len()
    ));

    for msg in &tail_messages {
        let line = match &msg.kind {
            MessageKind::User => format!("> {}\n", msg.content),
            MessageKind::Assistant => format!("● {}\n", msg.content),
            MessageKind::ToolCall { name, args } => {
                format!("▸ {}({})\n", name, truncate(args.as_str(), 80))
            }
            MessageKind::ToolOutput => format!("  ⎿ {}\n", truncate(msg.content.as_str(), 200)),
            MessageKind::Status => format!("  {}\n", msg.content),
        };
        transcript_text.push_str(&line);
    }

    // Build the full prompt with transcript injected
    let full_prompt = FORK_PROMPT.replace("{TRANSCRIPT}", &transcript_text);

    tracing::info!(
        source_session = %session_id,
        ?cwd,
        transcript_lines = tail_messages.len(),
        "Forking session"
    );

    let location_label = match location {
        "hsplit" => "horizontal split",
        "vsplit" => "vertical split",
        "tab" => "new tab",
        _ => "new window",
    };

    eprintln!("🔀 Forking from session {}", &session_id[..8.min(session_id.len())]);
    eprintln!("   Working directory: {}", cwd.display());
    eprintln!("   Context: {} messages", tail_messages.len());
    eprintln!("   Location: {}", location_label);

    // Write prompt to temp file - too long for command line args
    let prompt_file = std::env::temp_dir().join(format!("babel-fork-{}.txt", std::process::id()));
    std::fs::write(&prompt_file, &full_prompt)?;

    // Use kitty @ launch for full control over location
    // SHELL=bash because Claude Code doesn't support zsh
    use std::process::{Command, Stdio};

    let mut cmd = Command::new("kitty");
    cmd.args(["@", "launch"]);

    // --type for window/tab, --location for splits
    match location {
        "hsplit" | "vsplit" => {
            cmd.args(["--location", location]);
        }
        "tab" => {
            cmd.args(["--type", "tab"]);
        }
        _ => {
            cmd.args(["--type", "os-window"]);
        }
    }

    cmd.args(["--cwd", &cwd.to_string_lossy()])
        .args(["--env", "SHELL=/usr/bin/bash"])
        .arg("--")
        .arg("sh")
        .arg("-c")
        .arg(format!("cat '{}' | claude -p - ; rm -f '{}'", prompt_file.display(), prompt_file.display()))
        .stdin(Stdio::null())
        .stderr(Stdio::null());

    // Target main socket if available
    if let Some(socket) = claude_babel::kitty::main_socket() {
        cmd.args(["--to", &socket]);
        tracing::debug!(socket, location, "Spawning fork via kitty @");
    }

    let output = cmd.output().context("Failed to run kitty @ launch")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        // Clean up prompt file on error
        let _ = std::fs::remove_file(&prompt_file);
        return Err(anyhow!("kitty @ launch failed: {}", stderr));
    }

    eprintln!("   ✓ Spawned");

    Ok(())
}

// ═══════════════════════════════════════════════════════════════════════════════
// Helpers
// ═══════════════════════════════════════════════════════════════════════════════

fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        s.to_string()
    } else {
        format!("{}…", &s[..max.saturating_sub(1)])
    }
}
