//! Fork and Tail commands - meta-cognitive session spawning
//!
//! - `babel tail [target]` - Output recent transcript from a session
//! - `babel fork [target]` - Launch Claude primed with another session's context
//!
//! The fork command enables "2nd degree mode" - an agent session that's aware of
//! and can reflect on another session's work. This is particularly useful for:
//! - Continuing interrupted work with fresh context
//! - Getting a second opinion on an approach
//! - Debugging by observing what another agent did
//! - Meta-analysis of patterns and decisions

use std::path::{Path, PathBuf};

use anyhow::{anyhow, Context, Result};
use tracing::instrument;
use vtr::{boundary, checkpoint};

use babel::core::BabelCore;
use babel::pager::parse_transcript;
use babel::utility::claude_storage::{
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
    if let Ok(pane_id) = target.parse::<u64>() {
        return find_session_for_pane(core, pane_id).await;
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
async fn find_session_for_pane(core: &BabelCore, pane_id: u64) -> Result<String> {
    let windows = core.panes().await?;

    for window in windows {
        if window.id() == pane_id {
            if let Some(session_id) = &window.session_id {
                return Ok(session_id.clone());
            }
            return Err(anyhow!("Window {} has no session ID", pane_id));
        }
    }

    Err(anyhow!("Window {} not found", pane_id))
}

// ═══════════════════════════════════════════════════════════════════════════════
// Tail Command
// ═══════════════════════════════════════════════════════════════════════════════

/// Output recent transcript from a session
///
/// The output format is designed to be both human-readable and machine-parseable,
/// with clear role prefixes (> for user, ● for assistant, etc.)
#[instrument(level = "debug", skip(core))]
pub async fn cmd_tail(core: &BabelCore, target: &str, lines: usize, json: bool) -> Result<()> {
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

// ═══════════════════════════════════════════════════════════════════════════════
// Prompt Library - Configurable fork prompts for different cognitive modes
// ═══════════════════════════════════════════════════════════════════════════════

/// Get a fork prompt by name
///
/// Available prompts:
/// - "reflect" (default) - Metacognitive self-optimization mode
/// - "continue" - Seamless handoff, minimal meta
/// - "review" - Code review perspective
pub fn get_fork_prompt(name: &str) -> &'static str {
    match name {
        "continue" => PROMPT_CONTINUE,
        "review" => PROMPT_REVIEW,
        _ => PROMPT_REFLECT, // default
    }
}

/// Default prompt - 2nd-degree metacognitive observation mode
///
/// Optimized for:
/// - Observing the WORKER, not the work
/// - Watching cognitive patterns, friction, flow
/// - Identifying operations that could be hotwired
/// - Maintenance on metacognition itself
/// - Noticing new patterns that could become standard
const PROMPT_REFLECT: &str = r#"**🔀 2ND DEGREE MODE** (reflect)

You are NOT continuing this work. You are OBSERVING a agent worker.

The transcript below is EVIDENCE of cognitive patterns—watch the worker, not the work.
Your task: optimize Claude's ability to work, not the project itself.

<worker-observation>
{TRANSCRIPT}
</worker-observation>

**You are watching for:**

1. **Flow friction** - Where did the worker struggle, loop, or lose momentum?
2. **Pattern emergence** - What recurring operations could be hotwired into tooling?
3. **Missing scaffolding** - Should this workflow be documented in `Docs/`?
4. **CLAUDE.md gaps** - What should the worker have known but didn't?
5. **Cognitive waste** - Repeated reasoning that could become instant field transmission

**You are NOT here to:**
- Continue the project work
- Fix bugs in the code
- Answer the user's original question
- Pick up where the worker left off

**You ARE here to:**
- Observe how the worker navigated
- Identify friction points in the process
- Propose hotwiring: new commands, abstractions, patterns
- Upgrade the system so future workers flow better

**Your Toolbelt:**
```
babel fork .              # Spawn another observer
babel tail .              # More context from cwd
babel tail <window-id>    # Observe another worker
babel ls                  # List active workers
```

Now: What patterns do you observe in this worker's session?"#;

/// Seamless continuation prompt - minimal meta, just handoff
const PROMPT_CONTINUE: &str = r#"**🔀 FORKED SESSION** (continue mode)

Picking up from another agent session. Seamless handoff.

<source-session>
{TRANSCRIPT}
</source-session>

Continue the work. The source agent may still be running in parallel.
If blocked or need more context: `babel tail .` or `babel ls`

What's next?"#;

/// Code review prompt - critical eye perspective
const PROMPT_REVIEW: &str = r#"**🔀 FORKED SESSION** (review mode)

You were spawned to review another agent's work with a critical eye.

<source-session>
{TRANSCRIPT}
</source-session>

**Review Checklist:**
1. **Correctness** - Does the implementation match the intent?
2. **Edge cases** - What could break? What's not handled?
3. **Style** - Does it follow codebase conventions?
4. **Simplicity** - Is there a simpler approach?
5. **Security** - Any OWASP concerns? Input validation?

Don't just approve—find something to improve. Fresh eyes catch what working eyes miss.

What issues do you see?"#;

/// Fork from another session with full context injection
///
/// Launches a new agent session primed with:
/// - The transcript from the source session
/// - The reflection prompt for 2nd degree mode
/// - Clear annotation of babel commands for further exploration
///
/// Mode can be: "reflect" (default), "continue", or "review"
/// Location can be: "hsplit", "vsplit", "tab", or "os-window"
#[instrument(level = "debug", skip(core))]
pub async fn cmd_fork(
    core: &BabelCore,
    target: &str,
    lines: usize,
    mode: &str,
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
    let prompt_template = get_fork_prompt(mode);
    let full_prompt = prompt_template.replace("{TRANSCRIPT}", &transcript_text);

    checkpoint!(
        "fork_start",
        source_session = session_id,
        mode = mode,
        cwd = format!("{:?}", cwd),
        transcript_lines = format!("{}", tail_messages.len())
    );

    let location_label = match location {
        "hsplit" => "horizontal split",
        "vsplit" => "vertical split",
        "tab" => "new tab",
        _ => "new window",
    };

    eprintln!(
        "🔀 Forking from session {}",
        &session_id[..8.min(session_id.len())]
    );
    eprintln!("   Mode: {}", mode);
    eprintln!("   Working directory: {}", cwd.display());
    eprintln!("   Context: {} messages", tail_messages.len());
    eprintln!("   Location: {}", location_label);

    // Write prompt to temp file and launcher script
    // Script reads prompt safely without shell expansion issues
    let base = std::env::temp_dir().join(format!("babel-fork-{}", std::process::id()));
    let prompt_file = base.with_extension("txt");
    let script_file = base.with_extension("sh");

    std::fs::write(&prompt_file, &full_prompt)?;
    std::fs::write(
        &script_file,
        format!(
            r#"#!/bin/sh
PROMPT=$(cat '{}')
rm -f '{}' '{}'
exec claude "$PROMPT"
"#,
            prompt_file.display(),
            prompt_file.display(),
            script_file.display()
        ),
    )?;

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
        .arg(&script_file)
        .stdin(Stdio::null())
        .stderr(Stdio::null());

    // Target main socket if available
    if let Some(socket) = babel::kitty::main_socket() {
        cmd.args(["--to", &socket]);
        boundary!("kitty", "spawn_fork", socket = socket, location = location);
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
