//! Resume and Continue commands - conversation history browsing
//!
//! - `babel resume` / `babel r` - Interactive TUI pager for session browsing
//! - `babel resume 3 6 9` - Resume sessions by ls-sessions index
//! - `babel continue` / `babel c` - Resume most recent non-running session

use std::collections::HashSet;
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::{Command, Stdio};

use anyhow::{anyhow, Context, Result};
use console::style;
use tracing::instrument;
use vtr::{boundary, checkpoint, trace_error};

use babel::core::BabelCore;
use babel::utility::claude_storage::claude_base;

/// Interactive pager for browsing and resuming sessions
///
/// Opens TUI with session list (left) and transcript preview (right).
/// Tab toggles between cwd-only and all projects.
/// Enter resumes selected session.
#[instrument(level = "debug", skip(core))]
pub async fn cmd_resume(core: &BabelCore, all: bool, _json: bool) -> Result<()> {
    // Run the pager (to be implemented in phase 2)
    // For now, just show a placeholder message
    let selected = babel::pager::run_resume_pager(core, all).await?;

    // If a session was selected, launch claude --resume
    if let Some(session_id) = selected {
        launch_claude_resume(&session_id)?;
    }

    Ok(())
}

/// Resume sessions by their ls-sessions index.
///
/// Resolves indices against the same scan pipeline as `ls-sessions`, then
/// spawns a kitty terminal per session with the harness-native resume command.
/// Restores to last-known workspace when available in the overlay DB.
#[instrument(level = "debug")]
pub async fn cmd_resume_by_index(indices: &[usize]) -> Result<()> {
    use babel::babel_storage::{get_metadata, init_db};
    use babel::AgentKind;

    let sessions = super::query::scan_all_sessions(None, &Default::default());

    if sessions.is_empty() {
        return Err(anyhow!("No sessions found"));
    }

    let conn = init_db().ok();
    let main_socket = babel::kitty::main_socket();

    for &idx in indices {
        if idx == 0 || idx > sessions.len() {
            eprintln!(
                "{} index {} out of range (1-{})",
                style("✗").red(),
                idx,
                sessions.len()
            );
            continue;
        }

        let session = &sessions[idx - 1];
        let spec = session.agent_kind.spec();

        let Some(resume_tpl) = spec.resume_cmd else {
            eprintln!(
                "{} {} has no resume command",
                style("✗").red(),
                session.agent_kind.display_name()
            );
            continue;
        };

        let resume_full = resume_tpl.replace("{}", &session.native_id);
        let parts: Vec<&str> = resume_full.split_whitespace().collect();
        if parts.is_empty() {
            continue;
        }

        let cwd = session
            .project_path
            .as_deref()
            .map(PathBuf::from)
            .filter(|p| p.exists())
            .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));

        // Check overlay DB for workspace placement
        let session_key = session.agent_kind.session_key(&session.native_id);
        let target_workspace = conn
            .as_ref()
            .and_then(|c| get_metadata(c, &session_key).ok().flatten())
            .and_then(|m| m.last_workspace);

        // Spawn in kitty via kitten @ launch
        let mut cmd = Command::new("kitten");
        cmd.arg("@").arg("launch");
        cmd.args(["--type", "os-window"]);
        cmd.args(["--cwd", &cwd.to_string_lossy()]);

        if let Some(ref socket) = main_socket {
            cmd.args(["--to", socket]);
        }

        cmd.arg("--");
        cmd.args(&parts);

        cmd.stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        let title = session
            .display_name
            .as_deref()
            .map(|t| super::query::sanitize_display(t, 40))
            .unwrap_or_default();

        match cmd.output() {
            Ok(output) if output.status.success() => {
                // Move to workspace if we have placement data
                if let Some(ws) = target_workspace {
                    // Parse new pane id from launch output
                    let pane_id_str = String::from_utf8_lossy(&output.stdout);
                    if let Ok(pane_id) = pane_id_str.trim().parse::<u64>() {
                        if let Ok(Some(pane)) = babel::kitty::get_window(pane_id).await {
                            if let Err(e) = pane.move_to_workspace(ws) {
                                tracing::debug!(error = %e, ws, "Failed to move to workspace");
                            }
                        }
                    }
                }
                eprintln!(
                    " {} {} {}  {}  {}",
                    style(format!("{:>2}", idx)).dim(),
                    style(session.agent_kind.slug()).bold(),
                    style(&cwd.display().to_string()).dim(),
                    title,
                    style(&resume_full).dim(),
                );
            }
            Ok(output) => {
                let stderr = String::from_utf8_lossy(&output.stderr);
                eprintln!(
                    "{} failed to launch {}: {}",
                    style("✗").red(),
                    session.agent_kind.display_name(),
                    stderr.trim()
                );
            }
            Err(e) => {
                eprintln!(
                    "{} failed to spawn kitten: {}",
                    style("✗").red(),
                    e
                );
            }
        }
    }

    Ok(())
}

/// Resume the most recent session not currently running
///
/// Non-interactive: finds the most recent session from history that isn't
/// currently open in any pane, then launches `claude --resume`.
#[instrument(level = "debug", skip(core))]
pub async fn cmd_continue(core: &BabelCore) -> Result<()> {
    // Get running sessions from active windows
    let windows = core.panes().await.unwrap_or_default();
    let running_ids: HashSet<String> = windows
        .iter()
        .filter_map(|w| w.session_id.clone())
        .collect();

    // Get recent sessions from history (check last 50)
    let sessions = core.history(50).await?;

    if sessions.is_empty() {
        return Err(anyhow!("No conversation history found"));
    }

    // Find first non-running session
    let target = sessions
        .iter()
        .find(|s| !running_ids.contains(&s.session_id));

    // Fall back to most recent if all are running (unusual but handle it)
    let target = target
        .or(sessions.first())
        .ok_or_else(|| anyhow!("No sessions available"))?;

    checkpoint!(
        "resume_start",
        session_id = target.session_id.as_str(),
        project = format!("{:?}", target.project)
    );

    // Launch claude --resume, replacing current process
    launch_claude_resume(&target.session_id)
}

/// Launch `claude --resume <session_id>` replacing current process
///
/// Uses exec() to replace the current process with Claude, so this function
/// only returns on error (successful exec never returns).
fn launch_claude_resume(session_id: &str) -> Result<()> {
    // Determine working directory from session
    let cwd = get_session_cwd(session_id)?;

    boundary!(
        "claude",
        "resume",
        session_id = session_id,
        cwd = format!("{:?}", cwd)
    );

    // Use exec to replace current process - never returns on success
    let err = std::process::Command::new("claude")
        .arg("--resume")
        .arg(session_id)
        .current_dir(&cwd)
        .exec();

    // If we get here, exec failed
    Err(anyhow!("Failed to exec claude: {}", err))
}

/// Get working directory for a session by searching ~/.claude/projects/
///
/// Search strategy:
/// 1. Look for session JSONL file in project directories
/// 2. Extract cwd from session metadata if available
/// 3. Fall back to decoding project directory name
/// 4. Last resort: current directory
fn get_session_cwd(session_id: &str) -> Result<PathBuf> {
    let base = claude_base();
    let projects_dir = base.join("projects");

    if !projects_dir.exists() {
        return Err(anyhow!("No projects directory found at {:?}", projects_dir));
    }

    // Search for the session in all project directories
    for entry in std::fs::read_dir(&projects_dir)
        .with_context(|| format!("Failed to read projects directory: {:?}", projects_dir))?
    {
        let entry = entry?;
        let project_dir = entry.path();

        if !project_dir.is_dir() {
            continue;
        }

        // Look for session JSONL file: {session_id}.jsonl
        let session_file = project_dir.join(format!("{}.jsonl", session_id));
        if !session_file.exists() {
            continue;
        }

        // Found the session file - try to extract cwd from session info
        if let Ok(info) = babel::utility::claude_storage::get_session_info(&session_file) {
            if let Some(cwd) = info.cwd {
                if cwd.exists() {
                    return Ok(cwd);
                }
                trace_error!("session_cwd_missing", cwd = format!("{:?}", cwd));
            }
        }

        // Fall back to decoding project directory name
        // Claude encodes paths as: /home/user/project → -home-user-project
        if let Some(project_name) = project_dir.file_name().and_then(|n| n.to_str()) {
            if project_name.starts_with('-') {
                // Decode: "-home-user-project" -> "/home/user/project"
                let decoded = project_name.replacen('-', "/", 1).replace('-', "/");
                let path = PathBuf::from(&decoded);
                if path.exists() {
                    checkpoint!("decoded_cwd", path = format!("{:?}", path));
                    return Ok(path);
                }
            }
        }

        // If we found the session but can't determine cwd, that's still an error
        return Err(anyhow!(
            "Found session {} but could not determine working directory",
            session_id
        ));
    }

    // Session not found in any project
    Err(anyhow!(
        "Session {} not found in ~/.claude/projects/",
        session_id
    ))
}
