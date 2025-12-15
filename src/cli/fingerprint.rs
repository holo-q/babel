//! Fingerprint debugging command
//!
//! Traces the connection between a terminal window, its scrollback fingerprint,
//! and session files to understand linkage and matching confidence.

use anyhow::{bail, Context, Result};
use std::path::PathBuf;

use claude_babel::discovery::discover_claude_windows;
use claude_babel::kitty::get_scrollback;
use claude_babel::discovery::ClaudeWindow;
use claude_babel::ipc::{Request, Response};
use claude_babel::ipc::send_request;

use claude_babel::fingerprint::{extract_from_scrollback, extract_from_jsonl, match_fingerprints, MatchConfidence};
use claude_babel::claude_storage::{list_sessions, path_to_encoded};

/// Debug fingerprint linkage between terminals, sessions, and directories
///
/// Input is auto-detected or forced via flags. Shows the full trace of fingerprint data
/// and cross-matching between terminals and session files.
pub async fn cmd_fingerprint(
    input: Option<String>,
    force_window: bool,
    force_dir: bool,
    force_session: bool,
    json: bool,
) -> Result<()> {
    use console::style;

    // Require input
    let input = match input {
        Some(i) => i,
        None => {
            bail!("Input required: window ID, directory path, or session ID.\n\
                   Examples:\n\
                     babel fingerprint 42        # Window ID\n\
                     babel fingerprint .         # Current directory\n\
                     babel fingerprint abc123    # Session ID");
        }
    };

    // Determine input type via flags or auto-detection
    enum InputType {
        Window(u64),
        Directory(PathBuf),
        Session(String),
    }

    let input_type = if force_window {
        let id = input.parse::<u64>()
            .context("--window flag set but input is not a valid window ID")?;
        InputType::Window(id)
    } else if force_dir {
        InputType::Directory(PathBuf::from(&input))
    } else if force_session {
        InputType::Session(input.clone())
    } else {
        // Auto-detect based on content
        if input.chars().all(|c| c.is_ascii_digit()) {
            // Pure digits = window ID
            InputType::Window(input.parse().unwrap())
        } else if input.starts_with('/') || input.starts_with('.') || input.contains('/') {
            // Path-like = directory
            InputType::Directory(PathBuf::from(&input))
        } else {
            // Otherwise = session ID
            InputType::Session(input.clone())
        }
    };

    // Convert to the old parameter style for compatibility with existing logic
    let (window, dir, session): (Option<u64>, Option<PathBuf>, Option<String>) = match input_type {
        InputType::Window(id) => (Some(id), None, None),
        InputType::Directory(path) => (None, Some(path), None),
        InputType::Session(id) => (None, None, Some(id)),
    };

    // Resolve directory to absolute path
    let scope_dir = dir.map(|d| {
        d.canonicalize().unwrap_or_else(|_| std::env::current_dir().unwrap().join(&d))
    });

    println!();
    println!("{}", style("═══════════════════════════════════════════════════════════════════════════════").cyan());
    println!("{}", style("FINGERPRINT TRACE").cyan().bold());
    println!("{}", style("═══════════════════════════════════════════════════════════════════════════════").cyan());
    println!();

    // Show input
    if let Some(ref d) = scope_dir {
        println!("Input: directory {}", d.display());
    }
    if let Some(w) = window {
        println!("Input: window id:{}", w);
    }
    if let Some(ref s) = session {
        println!("Input: session {}", s);
    }
    println!();

    // ─── Terminals ─────────────────────────────────────────────────────────────
    let terminals = discover_claude_windows()?;
    let filtered_terminals: Vec<_> = terminals.iter().filter(|w| {
        // Filter by window ID if specified
        if let Some(wid) = window {
            if w.kitty_id != wid {
                return false;
            }
        }
        // Filter by scope directory if specified
        if let Some(ref d) = scope_dir {
            if !w.cwd.starts_with(d) {
                return false;
            }
        }
        true
    }).collect();

    println!("{}", style("─── Terminals ────────────────────────────────────────────────────────────────").dim());

    if filtered_terminals.is_empty() {
        println!("  (no matching terminals found)");
    }

    // Collect terminal fingerprints for cross-matching
    let mut terminal_fps: Vec<(u64, claude_babel::fingerprint::SessionFingerprint)> = Vec::new();

    for win in &filtered_terminals {
        let in_scope = scope_dir.as_ref().map(|d| win.cwd.starts_with(d)).unwrap_or(true);
        let scope_marker = if in_scope { style("✓ IN SCOPE").green() } else { style("✗ not in scope").dim() };

        println!("  id:{:<4}  cwd: {}  {}", win.kitty_id, win.cwd.display(), scope_marker);

        let title = win.title.strip_prefix("✳ ").unwrap_or(&win.title);
        println!("         title: \"{}\"", title);

        if let Some(ref sid) = win.session_id {
            println!("         tagged session: {}", style(sid).cyan());
        } else {
            println!("         tagged session: {}", style("(none)").dim());
        }

        // Extract fingerprint from scrollback
        match get_scrollback(win.kitty_id) {
            Ok(scrollback) => {
                let fp = extract_from_scrollback(&scrollback);
                terminal_fps.push((win.kitty_id, fp.clone()));

                println!();
                println!("         {}", style("Scrollback Fingerprint:").yellow());
                if let Some(ref first) = fp.first_prompt {
                    let display = if first.len() > 50 { format!("{}...", &first[..50]) } else { first.clone() };
                    println!("           first_prompt: \"{}\"", display);
                } else {
                    println!("           first_prompt: {}", style("(none)").dim());
                }
                println!("           recent_prompts: {:?}", fp.recent_prompts);
                println!("           tool_sequence: {:?}", fp.tool_sequence);
            }
            Err(e) => {
                println!("         {}: {}", style("scrollback error").red(), e);
            }
        }
        println!();
    }

    // ─── Sessions ──────────────────────────────────────────────────────────────
    println!("{}", style("─── Sessions ─────────────────────────────────────────────────────────────────").dim());

    // Determine which project directories to scan
    let claude_base = dirs::home_dir().unwrap().join(".claude").join("projects");
    let project_dirs: Vec<PathBuf> = if let Some(ref d) = scope_dir {
        // Specific directory - find its project folder
        let encoded = path_to_encoded(d);
        let project_dir = claude_base.join(&encoded);
        if project_dir.exists() {
            vec![project_dir]
        } else {
            println!("  Project: {}", encoded);
            println!("    {}", style("(no project folder found)").dim());
            vec![]
        }
    } else if let Some(ref s) = session {
        // Session ID specified - search all projects
        let mut found = Vec::new();
        if let Ok(entries) = std::fs::read_dir(&claude_base) {
            for entry in entries.filter_map(|e| e.ok()) {
                let sessions_path = entry.path();
                if sessions_path.is_dir() {
                    let session_file = sessions_path.join(format!("{}.jsonl", s));
                    if session_file.exists() {
                        found.push(sessions_path);
                        break;
                    }
                }
            }
        }
        found
    } else {
        vec![]
    };

    // Collect session fingerprints for cross-matching
    let mut session_fps: Vec<(String, PathBuf, claude_babel::fingerprint::SessionFingerprint)> = Vec::new();

    for project_dir in &project_dirs {
        let project_name = project_dir.file_name().unwrap_or_default().to_string_lossy();
        println!("  Project: {}", project_name);
        println!();

        let sessions = list_sessions(project_dir)?;

        // Filter sessions if specific one requested
        let sessions: Vec<_> = sessions.into_iter().filter(|path| {
            if let Some(ref s) = session {
                path.file_stem().map(|fs| fs.to_string_lossy().contains(s)).unwrap_or(false)
            } else {
                true
            }
        }).collect();

        if sessions.is_empty() {
            println!("    {}", style("(no matching sessions)").dim());
            continue;
        }

        for session_path in sessions {
            let session_name = session_path.file_name().unwrap_or_default().to_string_lossy();
            let session_id = session_path.file_stem().unwrap_or_default().to_string_lossy().to_string();

            // Get file metadata
            let metadata = std::fs::metadata(&session_path).ok();
            let modified = metadata.and_then(|m| m.modified().ok())
                .map(|t| {
                    let elapsed = t.elapsed().unwrap_or_default();
                    if elapsed.as_secs() < 60 { format!("{}s ago", elapsed.as_secs()) }
                    else if elapsed.as_secs() < 3600 { format!("{}m ago", elapsed.as_secs() / 60) }
                    else if elapsed.as_secs() < 86400 { format!("{}h ago", elapsed.as_secs() / 3600) }
                    else { format!("{}d ago", elapsed.as_secs() / 86400) }
                })
                .unwrap_or_else(|| "?".to_string());

            println!("  {}  ({})", style(&session_name).cyan(), modified);

            // Extract fingerprint from JSONL
            match extract_from_jsonl(&session_path) {
                Ok(fp) => {
                    session_fps.push((session_id.clone(), session_path.clone(), fp.clone()));

                    if let Some(ref first) = fp.first_prompt {
                        let display = if first.len() > 50 { format!("{}...", &first[..50]) } else { first.clone() };
                        println!("    first_prompt: \"{}\"", display);
                    }
                    println!("    tool_sequence: {:?}", fp.tool_sequence);

                    // Cross-match with terminals
                    for (term_id, term_fp) in &terminal_fps {
                        let confidence = match_fingerprints(term_fp, &fp);
                        if confidence != MatchConfidence::None {
                            println!();
                            println!("    {} id:{}: {} (confidence: {:?})",
                                style("Match vs").yellow(), term_id, format_confidence(confidence), confidence);

                            // Show match details
                            if term_fp.first_prompt == fp.first_prompt && term_fp.first_prompt.is_some() {
                                println!("      {} first_prompt match (+2)", style("✓").green());
                            }
                            let overlap: Vec<_> = term_fp.recent_prompts.iter()
                                .filter(|p| fp.recent_prompts.contains(p))
                                .collect();
                            if !overlap.is_empty() {
                                println!("      {} recent_prompts overlap (+1)", style("✓").green());
                            }
                            // Tool overlap
                            let term_tools: std::collections::HashSet<_> = term_fp.tool_sequence.iter().collect();
                            let session_tools: std::collections::HashSet<_> = fp.tool_sequence.iter().collect();
                            let intersection = term_tools.intersection(&session_tools).count();
                            let union = term_tools.union(&session_tools).count();
                            if union > 0 && (intersection as f64 / union as f64) > 0.5 {
                                println!("      {} tool_sequence similarity (+1)", style("✓").green());
                            }
                        }
                    }
                }
                Err(e) => {
                    println!("    {}: {}", style("parse error").red(), e);
                }
            }
            println!();
        }
    }

    // ─── Linkage Summary ───────────────────────────────────────────────────────
    println!("{}", style("─── Linkage ──────────────────────────────────────────────────────────────────").dim());

    let mut found_links = false;
    for (term_id, term_fp) in &terminal_fps {
        let mut best_match: Option<(&str, MatchConfidence)> = None;

        for (session_id, _, session_fp) in &session_fps {
            let confidence = match_fingerprints(term_fp, session_fp);
            // Collapsed: Only update best_match if confidence is Medium+ AND better than current best
            if confidence >= MatchConfidence::Medium && best_match.map(|(_, c)| confidence > c).unwrap_or(true) {
                best_match = Some((session_id.as_str(), confidence));
            }
        }

        if let Some((sid, conf)) = best_match {
            found_links = true;
            println!("  Terminal id:{} ←─[{:?}]─→ Session {}", term_id, conf, sid);
        }
    }

    if !found_links && !terminal_fps.is_empty() && !session_fps.is_empty() {
        println!("  {}", style("No strong matches found").yellow());
        println!();
        println!("  Recommendation: Check that the terminal has exchanged at least one message");
        println!("                  with Claude (fingerprinting needs prompt content).");
    } else if terminal_fps.is_empty() && session_fps.is_empty() {
        println!("  {}", style("No data to link").dim());
    }

    println!();
    println!("{}", style("═══════════════════════════════════════════════════════════════════════════════").cyan());
    println!();

    // JSON output
    if json {
        let output = serde_json::json!({
            "terminals": terminal_fps.iter().map(|(id, fp)| {
                serde_json::json!({
                    "kitty_id": id,
                    "first_prompt": fp.first_prompt,
                    "recent_prompts": fp.recent_prompts,
                    "tool_sequence": fp.tool_sequence,
                })
            }).collect::<Vec<_>>(),
            "sessions": session_fps.iter().map(|(id, path, fp)| {
                serde_json::json!({
                    "session_id": id,
                    "path": path,
                    "first_prompt": fp.first_prompt,
                    "tool_sequence": fp.tool_sequence,
                })
            }).collect::<Vec<_>>(),
        });
        println!("{}", serde_json::to_string_pretty(&output)?);
    }

    Ok(())
}

/// Format confidence level for display
pub fn format_confidence(conf: MatchConfidence) -> &'static str {
    match conf {
        MatchConfidence::None => "NONE",
        MatchConfidence::Low => "LOW",
        MatchConfidence::Medium => "MEDIUM",
        MatchConfidence::High => "HIGH",
        MatchConfidence::Exact => "EXACT",
    }
}

/// Get windows with fingerprints, trying daemon first then falling back to direct extraction
pub async fn get_windows_with_fingerprints() -> Result<Vec<ClaudeWindow>> {
    // Try daemon first
    if let Ok(Response::Windows { windows }) = send_request(&Request::ListWithFingerprints).await {
        return Ok(windows);
    }

    // Direct fallback - extract fingerprints manually
    let mut windows = discover_claude_windows()?;
    for win in &mut windows {
        if let Ok(scrollback) = get_scrollback(win.kitty_id) {
            let fp = extract_from_scrollback(&scrollback);
            win.fingerprint = Some(fp);
        }
    }
    Ok(windows)
}
